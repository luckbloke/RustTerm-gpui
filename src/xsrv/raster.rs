//! 圆弧与多边形的扫描转换。
//!
//! 这些函数只做纯几何计算，不碰画布：输入是 X11 的 `Arc` / 顶点列表，
//! 输出是按行排列的水平跨度（`Span`）。把它们和 `draw::Canvas` 分开，是为了能
//! 单独对几何行为写单元测试——圆弧的角度方向、多边形的填充规则最容易写错，
//! 而且一旦写错，画出来的东西"看着像但就是不对"，靠肉眼很难定位。
//!
//! 角度约定（X11 协议）：`angle1` / `angle2` 以 1/64 度为单位，**逆时针**为正，
//! 而屏幕坐标 y 向下。因此角度换算到屏幕时要对 y 取反。

use x11rb_protocol::protocol::xproto::{Arc, PolyShape};

/// 一行上的水平填充区间，`x0..=x1`（闭区间，单位是像素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub y: i32,
    pub x0: i32,
    pub x1: i32,
}

/// 圆弧的圆心与半径（像素坐标，浮点以便采样）。
pub fn arc_geometry(arc: &Arc) -> (f64, f64, f64, f64) {
    let width = f64::from(arc.width);
    let height = f64::from(arc.height);
    (
        f64::from(arc.x) + width / 2.0,
        f64::from(arc.y) + height / 2.0,
        width / 2.0,
        height / 2.0,
    )
}

/// 点相对圆心的角度（度，逆时针为正，范围 -180..=180）。
fn point_angle(cx: f64, cy: f64, px: f64, py: f64) -> f64 {
    // 屏幕 y 向下，取反后才是数学意义上的角度
    (-(py - cy)).atan2(px - cx).to_degrees()
}

/// `angle` 是否落在从 `start` 起、跨度为 `sweep` 的扇形内（`sweep` 可正可负）。
fn angle_within(angle: f64, start: f64, sweep: f64) -> bool {
    // 统一到 [0, 360) 再比较，避免 -180/180 边界与跨 0 度的情况
    let normalize = |value: f64| {
        let mut value = value % 360.0;
        if value < 0.0 {
            value += 360.0;
        }
        value
    };
    let sweep_abs = sweep.abs().min(360.0);
    if sweep_abs >= 360.0 {
        return true;
    }
    let delta = if sweep >= 0.0 {
        normalize(angle - start)
    } else {
        normalize(start - angle)
    };
    delta <= sweep_abs
}

/// 描边用：把圆弧采样成一串点（首尾相接画折线即可）。
///
/// 采样步长按弧长换算，保证 1 像素左右的精度：半径越大、跨度越长，点越多。
pub fn arc_outline(arc: &Arc, max_step_px: f64) -> Vec<(i32, i32)> {
    let (cx, cy, rx, ry) = arc_geometry(arc);
    let sweep = f64::from(arc.angle2) / 64.0;
    if rx <= 0.0 || ry <= 0.0 || sweep == 0.0 {
        return Vec::new();
    }
    let sweep = sweep.clamp(-360.0, 360.0);
    let start = f64::from(arc.angle1) / 64.0;

    // 椭圆周长近似用 Ramanujan 的一阶公式，够用来决定采样密度
    let perimeter = std::f64::consts::PI * (3.0 * (rx + ry) - ((3.0 * rx + ry) * (rx + 3.0 * ry)).sqrt());
    let arc_len = perimeter * (sweep.abs() / 360.0);
    let steps = (arc_len / max_step_px).ceil().max(2.0) as usize;
    let steps = steps.min(8192); // 防御：异常的巨型弧不要采样出百万个点

    let mut points = Vec::with_capacity(steps + 1);
    for index in 0..=steps {
        let t = start + sweep * (index as f64 / steps as f64);
        let radians = t.to_radians();
        let x = cx + rx * radians.cos();
        let y = cy - ry * radians.sin();
        points.push((x.floor() as i32, y.floor() as i32));
    }
    points
}

/// 填充用的跨度：扇形（PIE_SLICE）与弓形（CHORD）。
///
/// 用逐像素的角度测试而不是解析求交：圆弧的填充区域在 |sweep| > 180 时不是凸的，
/// 解析法要处理"一行两个区间"的分裂，容易出错；逐像素慢一点但语义直白，
/// 而且圆弧区域通常不大（X 应用里的按钮圆角、仪表盘、眼睛）。
pub fn fill_arc_spans(arc: &Arc, pie_slice: bool) -> Vec<Span> {
    let (cx, cy, rx, ry) = arc_geometry(arc);
    let sweep = f64::from(arc.angle2) / 64.0;
    if rx <= 0.0 || ry <= 0.0 || sweep == 0.0 {
        return Vec::new();
    }
    let sweep = sweep.clamp(-360.0, 360.0);
    let start = f64::from(arc.angle1) / 64.0;
    let full = sweep.abs() >= 360.0;

    // 弓形的弦：两端点连成的直线，填充区域取"弧所在一侧"
    let (chord_a, chord_b) = if pie_slice || full {
        ((0.0, 0.0), (0.0, 0.0))
    } else {
        let to_point = |t: f64| {
            let radians = t.to_radians();
            (cx + rx * radians.cos(), cy - ry * radians.sin())
        };
        (to_point(start), to_point(start + sweep))
    };
    // 弧中点用来判断弦的哪一侧属于填充区
    let arc_mid = {
        let radians = (start + sweep / 2.0).to_radians();
        (cx + rx * radians.cos(), cy - ry * radians.sin())
    };

    let top = arc.y as i32;
    let bottom = arc.y as i32 + arc.height as i32 - 1;
    let mut spans = Vec::new();

    for row in top..=bottom {
        let py = f64::from(row) + 0.5;
        let dy = (py - cy) / ry;
        if dy.abs() > 1.0 {
            continue;
        }
        let half = (1.0 - dy * dy).sqrt() * rx;
        let left = (cx - half).floor() as i32;
        let right = (cx + half).ceil() as i32;

        let mut run: Option<i32> = None;
        for column in left..=right {
            let px = f64::from(column) + 0.5;
            let nx = (px - cx) / rx;
            let inside_ellipse = nx * nx + dy * dy <= 1.0;
            let inside = inside_ellipse
                && (full
                    || (angle_within(point_angle(cx, cy, px, py), start, sweep)
                        && (pie_slice || chord_side(chord_a, chord_b, arc_mid, px, py))));

            match (inside, run) {
                (true, None) => run = Some(column),
                (false, Some(x0)) => {
                    spans.push(Span { y: row, x0, x1: column - 1 });
                    run = None;
                }
                _ => {}
            }
        }
        if let Some(x0) = run {
            spans.push(Span { y: row, x0, x1: right });
        }
    }
    spans
}

/// 点与弧中点是否在弦的同一侧（叉积同号）。
fn chord_side(a: (f64, f64), b: (f64, f64), reference: (f64, f64), px: f64, py: f64) -> bool {
    let cross = |p: (f64, f64)| (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0);
    let side_reference = cross(reference);
    let side_point = cross((px, py));
    if side_reference == 0.0 || side_point == 0.0 {
        return true;
    }
    (side_reference > 0.0) == (side_point > 0.0)
}

/// 多边形填充的扫描转换。
///
/// `Convex` / `Nonconvex` 用奇偶规则，`Complex` 用非零环绕数规则——这是 X11 的规定，
/// 也是自交多边形能画出"中空"效果的原因。
pub fn polygon_spans(points: &[(i32, i32)], shape: PolyShape) -> Vec<Span> {
    if points.len() < 3 {
        return Vec::new();
    }
    let top = points.iter().map(|point| point.1).min().unwrap();
    let bottom = points.iter().map(|point| point.1).max().unwrap();
    let winding_rule = shape == PolyShape::COMPLEX;

    let mut spans = Vec::new();
    let mut crossings: Vec<(f64, i32)> = Vec::new();

    for row in top..=bottom {
        let scan = f64::from(row) + 0.5;
        crossings.clear();

        for index in 0..points.len() {
            let (x1, y1) = points[index];
            let (x2, y2) = points[(index + 1) % points.len()];
            let (y1f, y2f) = (f64::from(y1), f64::from(y2));
            // 半开区间 [y1, y2) 避免顶点被数两次
            if (y1f <= scan && y2f > scan) || (y2f <= scan && y1f > scan) {
                let t = (scan - y1f) / (y2f - y1f);
                let x = f64::from(x1) + t * f64::from(x2 - x1);
                crossings.push((x, if y2 > y1 { 1 } else { -1 }));
            }
        }
        if crossings.is_empty() {
            continue;
        }
        crossings.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        if winding_rule {
            let mut winding = 0;
            let mut start: Option<f64> = None;
            for (x, direction) in &crossings {
                let before = winding;
                winding += direction;
                if before == 0 && winding != 0 {
                    start = Some(*x);
                } else if before != 0 && winding == 0 {
                    if let Some(x0) = start.take() {
                        push_span(&mut spans, row, x0, *x);
                    }
                }
            }
            if let Some(x0) = start {
                push_span(&mut spans, row, x0, crossings.last().map(|c| c.0).unwrap_or(x0));
            }
        } else {
            for pair in crossings.chunks_exact(2) {
                push_span(&mut spans, row, pair[0].0, pair[1].0);
            }
        }
    }
    spans
}

/// 把浮点区间转成像素区间：像素 x 的中心落在 [x0, x1) 内就算被覆盖。
fn push_span(spans: &mut Vec<Span>, y: i32, x0: f64, x1: f64) {
    let start = (x0 - 0.5).ceil() as i32;
    let end = (x1 - 0.5).ceil() as i32 - 1;
    if end < start {
        return;
    }
    spans.push(Span { y, x0: start, x1: end });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arc(x: i16, y: i16, width: u16, height: u16, angle1: i16, angle2: i16) -> Arc {
        Arc { x, y, width, height, angle1, angle2 }
    }

    #[test]
    fn full_circle_fills_its_bounding_box_center() {
        let spans = fill_arc_spans(&arc(0, 0, 20, 20, 0, 360 * 64), true);
        assert!(!spans.is_empty());
        let middle = spans.iter().find(|span| span.y == 10).expect("中间行必须有跨度");
        assert!(middle.x0 <= 9 && middle.x1 >= 10, "圆心附近必须被覆盖");
        assert!(spans.iter().all(|span| span.y >= 0 && span.y <= 19));
    }

    #[test]
    fn half_circle_only_covers_one_side() {
        // 上半圆（X11 里 0..180 度是屏幕上方的半圆）
        let spans = fill_arc_spans(&arc(0, 0, 20, 20, 0, 180 * 64), true);
        assert!(spans.iter().all(|span| span.y <= 10), "上半圆不该画到下半部分");
        assert!(spans.iter().any(|span| span.y == 2), "靠近顶部的行应当有填充");
    }

    #[test]
    fn zero_sweep_draws_nothing() {
        assert!(fill_arc_spans(&arc(0, 0, 10, 10, 0, 0), true).is_empty());
        assert!(arc_outline(&arc(0, 0, 10, 10, 0, 0), 1.0).is_empty());
    }

    #[test]
    fn arc_outline_starts_at_the_first_angle() {
        // 从 0 度（右侧）开始，圆心 (10,10)、半径 10 → 起点约 (20,10)
        let points = arc_outline(&arc(0, 0, 20, 20, 0, 90 * 64), 1.0);
        assert!(points.len() > 2);
        assert_eq!(points[0], (20, 10));
        // 90 度在屏幕坐标里是上方：终点应在圆心正上方
        let (last_x, last_y) = *points.last().unwrap();
        assert!((last_x - 10).abs() <= 1, "终点 x 应当回到圆心上方：{last_x}");
        assert!((last_y - 0).abs() <= 1, "终点 y 应当在圆的顶部：{last_y}");
    }

    #[test]
    fn square_polygon_fills_interior() {
        let square = [(0, 0), (10, 0), (10, 10), (0, 10)];
        let spans = polygon_spans(&square, PolyShape::CONVEX);
        // 每行覆盖 0..=9
        let row = spans.iter().find(|span| span.y == 5).unwrap();
        assert_eq!((row.x0, row.x1), (0, 9));
        assert_eq!(spans.len(), 10, "10 行各一条跨度");
    }

    #[test]
    fn self_intersecting_polygon_differs_between_rules() {
        // 五角星：中心点被轮廓包了两圈，所以环绕数规则（X11 的 Complex）填满它，
        // 奇偶规则（Convex/Nonconvex）认为穿越次数是偶数 → 中心是空的。
        //
        // 注意判据是"中心点在不在填充里"，不是跨度条数：奇偶规则会把中心行
        // **切成两条**跨度（中间空出来），条数反而更多。
        let star = star_points();
        let even_odd = polygon_spans(&star, PolyShape::NONCONVEX);
        let winding = polygon_spans(&star, PolyShape::COMPLEX);
        let covered = |spans: &[Span], x: i32, y: i32| {
            spans
                .iter()
                .any(|span| span.y == y && span.x0 <= x && x <= span.x1)
        };
        assert!(covered(&winding, 10, 10), "环绕数规则下中心应当被填满");
        assert!(!covered(&even_odd, 10, 10), "奇偶规则下中心应当是空的");
    }

    fn star_points() -> Vec<(i32, i32)> {
        let mut points = Vec::new();
        for index in 0..5 {
            let angle = (90.0 + index as f64 * 144.0).to_radians();
            points.push((
                10 + (10.0 * angle.cos()).round() as i32,
                10 - (10.0 * angle.sin()).round() as i32,
            ));
        }
        points
    }

    #[test]
    fn angle_wrapping_handles_the_zero_crossing() {
        assert!(angle_within(10.0, 350.0, 30.0), "跨 0 度的扇形要能包含 10 度");
        assert!(angle_within(355.0, 350.0, 30.0));
        assert!(!angle_within(300.0, 350.0, 30.0));
        // 负跨度（顺时针）
        assert!(angle_within(350.0, 10.0, -30.0));
        assert!(!angle_within(20.0, 10.0, -30.0));
    }
}
