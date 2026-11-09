//! 矩形列表形式的区域（region）运算。
//!
//! SHAPE 与 XFIXES 都需要"一组矩形描述的任意形状"，但用途（裁剪、命中测试、
//! 并/交/差）完全一致，所以抽出来放在这里共用。实现选择**逐行扫描**：
//! 区域内部按行存放已合并的横向区间，任何布尔运算都退化为"同一行上两列
//! 区间的并/交/差"。行数上限是矩形的高度（屏幕级别也就一两千行），对本
//! 服务器的请求频率来说开销可以忽略，换来的是实现直观、不容易写错。
//!
//! 约定：区间一律右开 `[x0, x1)`；行按 `y` 升序排列，行内区间按 `x0` 升序
//! 且互不重叠（规范形）。SHAPE 规范里的坐标是"相对窗口外角"（含边框），
//! 这里不做坐标解释，只存调用方给的值。

use x11rb_protocol::protocol::xproto::Rectangle;

/// 区域内的一行：`y` 上 `[x0, x1)` 的若干互不重叠区间。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionRow {
    pub y: i32,
    /// 已排序、已合并的 `(x0, x1)` 右开区间。
    pub spans: Vec<(i32, i32)>,
}

/// 一个区域。空区域的 `rows` 为空。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rects {
    rows: Vec<RegionRow>,
}

impl Rects {
    /// NyaTerm（ShapeMask 用）：从位图逐行扫描构造区域。
    ///
    /// 输入的行区间应已排序、合并；全空的行可以省略。
    pub fn from_row_spans(rows: Vec<RegionRow>) -> Self {
        Self { rows }
    }

    /// 从矩形列表构造规范形区域。
    pub fn from_rectangles(rectangles: &[Rectangle]) -> Self {
        if rectangles.is_empty() {
            return Self { rows: Vec::new() };
        }
        let min_y = rectangles.iter().map(|r| i32::from(r.y)).min().unwrap();
        let max_y = rectangles
            .iter()
            .map(|r| i32::from(r.y) + i32::from(r.height))
            .max()
            .unwrap();
        let mut rows = Vec::new();
        for y in min_y..max_y {
            let mut spans = Vec::new();
            for rect in rectangles {
                let top = i32::from(rect.y);
                let bottom = top + i32::from(rect.height);
                if y < top || y >= bottom || rect.width == 0 {
                    continue;
                }
                let x0 = i32::from(rect.x);
                let x1 = x0 + i32::from(rect.width);
                if x1 > x0 {
                    spans.push((x0, x1));
                }
            }
            if !spans.is_empty() {
                spans.sort_unstable();
                let spans = merge_spans(spans);
                rows.push(RegionRow { y, spans });
            }
        }
        Self { rows }
    }

    /// 单个矩形构成的区域。
    pub fn from_rectangle(rect: Rectangle) -> Self {
        Self::from_rectangles(&[rect])
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn rows(&self) -> &[RegionRow] {
        &self.rows
    }

    /// 是否包含一个（本坐标系下的）点。
    pub fn contains(&self, x: i32, y: i32) -> bool {
        let Ok(index) = self.rows.binary_search_by_key(&y, |row| row.y) else {
            return false;
        };
        self.rows[index]
            .spans
            .binary_search_by(|&(x0, x1)| {
                // 命中区间时 binary_search 要求返回 Equal，这里手工比较：
                // x < x0 → Less，x >= x1 → Greater，区间内 → Equal
                if x < x0 {
                    std::cmp::Ordering::Greater
                } else if x >= x1 {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }

    /// 包围盒（覆盖所有像素的最小矩形）；空区域返回 `None`。
    pub fn extents(&self) -> Option<Rectangle> {
        let first = self.rows.first()?;
        let mut x0 = i32::MAX;
        let mut x1 = i32::MIN;
        for row in &self.rows {
            for &(sx0, sx1) in &row.spans {
                x0 = x0.min(sx0);
                x1 = x1.max(sx1);
            }
        }
        let y0 = first.y;
        let y1 = self.rows.last().unwrap().y + 1;
        Some(Rectangle {
            x: x0 as i16,
            y: y0 as i16,
            width: (x1 - x0) as u16,
            height: (y1 - y0) as u16,
        })
    }

    /// 平移（原地）。
    pub fn translate(&mut self, dx: i16, dy: i16) {
        for row in &mut self.rows {
            row.y += i32::from(dy);
            for span in &mut row.spans {
                span.0 += i32::from(dx);
                span.1 += i32::from(dx);
            }
        }
    }

    /// 并集。
    pub fn union(&self, other: &Rects) -> Rects {
        Self::binary_op(self, other, |a, b| merge_spans(sorted_concat(a, b)))
    }

    /// 交集。
    pub fn intersect(&self, other: &Rects) -> Rects {
        Self::binary_op(self, other, intersect_spans)
    }

    /// 差集：`self - other`。
    pub fn subtract(&self, other: &Rects) -> Rects {
        Self::binary_op(self, other, subtract_spans)
    }

    /// 取反：`bounds - self`。SHAPE 的 Invert / XFIXES 的 InvertRegion 都是这个语义。
    pub fn invert(&self, bounds: &Rects) -> Rects {
        bounds.subtract(self)
    }

    /// 逐行二元运算：`op(a_row_spans, b_row_spans) -> 结果行的区间`。
    fn binary_op(&self, other: &Rects, op: fn(&[(i32, i32)], &[(i32, i32)]) -> Vec<(i32, i32)>) -> Rects {
        let mut rows = Vec::new();
        let (mut i, mut j) = (0usize, 0usize);
        while i < self.rows.len() || j < other.rows.len() {
            // 两边的行都按 y 升序，双指针前进；只处理同一 y 上"双方都可能有像素"的行
            let y = match (self.rows.get(i), other.rows.get(j)) {
                (Some(a), Some(b)) => a.y.min(b.y),
                (Some(a), None) => a.y,
                (None, Some(b)) => b.y,
                (None, None) => break,
            };
            let a_spans: &[(i32, i32)] = if self.rows.get(i).map(|row| row.y) == Some(y) {
                i += 1;
                &self.rows[i - 1].spans
            } else {
                &[]
            };
            let b_spans: &[(i32, i32)] = if other.rows.get(j).map(|row| row.y) == Some(y) {
                j += 1;
                &other.rows[j - 1].spans
            } else {
                &[]
            };
            let spans = op(a_spans, b_spans);
            if !spans.is_empty() {
                rows.push(RegionRow { y, spans });
            }
        }
        Self { rows }
    }
}

/// 合并一行内相互接触/重叠的区间（入参须已排序）。
fn merge_spans(mut spans: Vec<(i32, i32)>) -> Vec<(i32, i32)> {
    let mut merged: Vec<(i32, i32)> = Vec::with_capacity(spans.len());
    for (x0, x1) in spans.drain(..) {
        match merged.last_mut() {
            // 接触（x0 == 上个 x1）也算连续：区域语义里相邻即相连
            Some(last) if x0 <= last.1 => last.1 = last.1.max(x1),
            _ => merged.push((x0, x1)),
        }
    }
    merged
}

/// 两个已排序、已合并的区间列表求交。
fn intersect_spans(a: &[(i32, i32)], b: &[(i32, i32)]) -> Vec<(i32, i32)> {
    let mut result = Vec::new();
    for &(ax0, ax1) in a {
        for &(bx0, bx1) in b {
            let (x0, x1) = (ax0.max(bx0), ax1.min(bx1));
            if x1 > x0 {
                result.push((x0, x1));
            }
        }
    }
    result
}

/// 两个已排序、已合并的区间列表求差：`a - b`。
fn subtract_spans(a: &[(i32, i32)], b: &[(i32, i32)]) -> Vec<(i32, i32)> {
    let mut result = Vec::new();
    for &(ax0, ax1) in a {
        let mut cursor = ax0;
        // b 不重叠时，区间按 x0 升序，扫描到越过当前 a 区间就可以停
        for &(bx0, bx1) in b {
            if bx1 <= cursor {
                continue;
            }
            if bx0 >= ax1 {
                break;
            }
            if bx0 > cursor {
                result.push((cursor, bx0));
            }
            cursor = cursor.max(bx1);
            if cursor >= ax1 {
                break;
            }
        }
        if cursor < ax1 {
            result.push((cursor, ax1));
        }
    }
    result
}

/// 拼接两个区间列表（交运算的输入需要的是"两个列表求并"这一步之前的形式）。
fn sorted_concat(a: &[(i32, i32)], b: &[(i32, i32)]) -> Vec<(i32, i32)> {
    let mut all = a.to_vec();
    all.extend_from_slice(b);
    all.sort_unstable();
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i16, y: i16, width: u16, height: u16) -> Rectangle {
        Rectangle { x, y, width, height }
    }

    /// 区域覆盖的像素数（用来做与手工计算无关的数量校验）。
    fn area(region: &Rects) -> usize {
        region
            .rows()
            .iter()
            .map(|row| row.spans.iter().map(|&(x0, x1)| (x1 - x0) as usize).sum::<usize>())
            .sum()
    }

    #[test]
    fn union_intersect_subtract_and_invert_are_consistent() {
        // (0,0)-(10,10) 与 (5,5)-(15,15) 两个正方形
        let a = Rects::from_rectangle(rect(0, 0, 10, 10));
        let b = Rects::from_rectangle(rect(5, 5, 10, 10));

        // 并：两个 10x10 方块重叠 5x5 → 175
        let u = a.union(&b);
        assert_eq!(area(&u), 10 * 10 + 10 * 10 - 5 * 5);
        // 交：5x5
        let i = a.intersect(&b);
        assert_eq!(area(&i), 5 * 5);
        // 差：a - b = 75
        let s = a.subtract(&b);
        assert_eq!(area(&s), 10 * 10 - 5 * 5);
        // 反演（以并集为界）：u = a + (b-a)，所以 u - a = b - a = 75
        let inv = a.invert(&u);
        assert_eq!(area(&inv), 75);
        // 运算的自洽性：(a-b) ∪ (a∩b) = a
        assert_eq!(area(&s.union(&i)), 100);
    }

    #[test]
    fn contains_and_extents_follow_the_canonical_form() {
        let region = Rects::from_rectangles(&[rect(0, 0, 4, 2), rect(4, 2, 4, 2)]);
        assert!(region.contains(0, 0));
        assert!(region.contains(3, 1));
        assert!(!region.contains(4, 0), "第一行只到 x=4 之前");
        assert!(region.contains(4, 2), "第二行从 x=4 开始");
        assert!(!region.contains(8, 2), "右开区间");
        let extents = region.extents().expect("非空区域必有包围盒");
        assert_eq!(
            (extents.x, extents.y, extents.width, extents.height),
            (0, 0, 8, 4),
            "包围盒覆盖全部像素"
        );
    }

    #[test]
    fn translate_moves_every_row() {
        let mut region = Rects::from_rectangle(rect(0, 0, 4, 4));
        region.translate(10, 20);
        assert!(region.contains(10, 20));
        assert!(region.contains(13, 23));
        assert!(!region.contains(0, 0));
        let extents = region.extents().expect("平移后非空");
        assert_eq!((extents.x, extents.y, extents.width, extents.height), (10, 20, 4, 4));
    }

    #[test]
    fn empty_and_degenerate_rectangles_are_handled() {
        let empty = Rects::from_rectangles(&[]);
        assert!(empty.is_empty());
        assert!(empty.extents().is_none());
        assert!(!empty.contains(0, 0));        // 宽 0 的矩形不占像素
        let degenerate = Rects::from_rectangle(rect(0, 0, 0, 10));
        assert!(degenerate.is_empty());
        // 与空区域做运算不崩、结果为空
        let some = Rects::from_rectangle(rect(0, 0, 5, 5));
        assert!(some.intersect(&empty).is_empty());
        assert_eq!(area(&some.subtract(&empty)), 25);
    }
}
