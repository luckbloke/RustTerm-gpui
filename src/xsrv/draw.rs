//! 核心绘制原语。
//!
//! 所有绘制都发生在"一块像素缓冲 + 一份 GC 状态"之上，不关心这块缓冲属于窗口还是
//! pixmap。这里负责的是**绘制语义**：
//!
//! - 裁剪：`SetClipRectangles` 的矩形列表与 clip-mask 位图；
//! - raster op（GX）与 plane mask；
//! - 填充样式（实心 / tile / stipple / opaque-stipple）；
//! - 线宽、线型（实线 / 虚线）、虚线相位；
//! - 圆弧与多边形（几何计算在 `raster.rs`）；
//! - 核心字体的文字绘制（字形在 `font.rs`）。
//!
//! 窗口在屏幕上的位置由 `state::compose` 负责，这里只画"窗口自身坐标"。

use x11rb_protocol::protocol::xproto::{Arc, ArcMode, LineStyle, PolyShape, Rectangle};

use super::font;
use super::raster;
use super::state::GcData;

/// 一块 1 位深位图（stipple / tile / clip mask）的视图。
#[derive(Clone, Copy)]
pub struct Bitmap<'a> {
    pub pixels: &'a [u32],
    pub width: usize,
    pub height: usize,
}

impl Bitmap<'_> {
    /// 取位图在 (x, y) 处的像素；坐标按位图尺寸取模（X11 的 tile/stipple 是平铺的）。
    #[inline]
    fn sample(&self, x: i32, y: i32) -> u32 {
        if self.width == 0 || self.height == 0 {
            return 0;
        }
        let tx = x.rem_euclid(self.width as i32) as usize;
        let ty = y.rem_euclid(self.height as i32) as usize;
        // 用 get 而不是裸索引：辅助位图可能因为上游的借还漏配（显式归还，
        // 没有 Drop 兜底）拿到空/短缓冲——裸索引会 panic，而这条路径上的
        // panic 会杀死唯一的光栅化 worker（整个界面永久停更）。缺像素按 0 处理。
        self.pixels.get(ty * self.width + tx).copied().unwrap_or(0)
    }
}

/// 裁剪信息：矩形列表与位图掩码（两者互斥，X11 里后设置的那个生效）。
#[derive(Clone, Copy)]
pub struct Clip<'a> {
    pub x_origin: i32,
    pub y_origin: i32,
    pub rectangles: &'a [Rectangle],
    pub mask: Option<Bitmap<'a>>,
}

impl Clip<'_> {
    fn is_empty(&self) -> bool {
        self.rectangles.is_empty() && self.mask.is_none()
    }

    /// 画布坐标 (x, y) 是否在裁剪区内。
    #[inline]
    fn allows(&self, x: i32, y: i32) -> bool {
        let cx = x - self.x_origin;
        let cy = y - self.y_origin;
        if !self.rectangles.is_empty() {
            return self.rectangles.iter().any(|rect| {
                let left = i32::from(rect.x);
                let top = i32::from(rect.y);
                cx >= left
                    && cx < left + i32::from(rect.width)
                    && cy >= top
                    && cy < top + i32::from(rect.height)
            });
        }
        if let Some(mask) = self.mask {
            if cx < 0 || cy < 0 || cx >= mask.width as i32 || cy >= mask.height as i32 {
                return false;
            }
            // 同 `Bitmap::sample`：短缓冲不得 panic（缺像素视为未设置）
            return mask
                .pixels
                .get(cy as usize * mask.width + cx as usize)
                .copied()
                .unwrap_or(0)
                & 1
                != 0;
        }
        true
    }
}

/// 一块可绘制区域。
pub struct Canvas<'a> {
    pub pixels: &'a mut [u32],
    pub width: usize,
    pub height: usize,
    gc: &'a GcData,
    clip: Option<Clip<'a>>,
    tile: Option<Bitmap<'a>>,
    stipple: Option<Bitmap<'a>>,
}

impl<'a> Canvas<'a> {
    /// 不带裁剪的画布。
    pub fn new(pixels: &'a mut [u32], width: usize, height: usize, gc: &'a GcData) -> Self {
        debug_assert!(pixels.len() >= width * height, "像素缓冲小于画布尺寸");
        Self { pixels, width, height, gc, clip: None, tile: None, stipple: None }
    }

    /// 带 GC 裁剪、tile、stipple 的画布。
    pub fn with_gc(
        pixels: &'a mut [u32],
        width: usize,
        height: usize,
        gc: &'a GcData,
        clip: Option<Clip<'a>>,
        tile: Option<Bitmap<'a>>,
        stipple: Option<Bitmap<'a>>,
    ) -> Self {
        debug_assert!(pixels.len() >= width * height, "像素缓冲小于画布尺寸");
        Self { pixels, width, height, gc, clip, tile, stipple }
    }

    #[inline]
    fn index(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return None;
        }
        if let Some(clip) = &self.clip {
            if !clip.allows(x, y) {
                return None;
            }
        }
        Some(y as usize * self.width + x as usize)
    }

    #[inline]
    pub fn get(&self, x: i32, y: i32) -> Option<u32> {
        self.index(x, y).map(|index| self.pixels[index])
    }

    /// 用 GC 的 raster op 与 plane mask 写一个像素。
    #[inline]
    pub fn paint(&mut self, x: i32, y: i32, source: u32) {
        if let Some(index) = self.index(x, y) {
            let destination = self.pixels[index];
            let result = gx(self.gc.function, source, destination);
            self.pixels[index] = (result & self.gc.plane_mask) | (destination & !self.gc.plane_mask);
        }
    }

    /// 绕过 GC 直接写入（ClearArea 用窗口背景色）。
    #[inline]
    pub fn store(&mut self, x: i32, y: i32, value: u32) {
        if let Some(index) = self.index(x, y) {
            self.pixels[index] = value;
        }
    }

    /// 当前填充样式下 (x, y) 该用的源颜色；`None` 表示这个像素保持原样。
    fn fill_source(&self, x: i32, y: i32) -> Option<u32> {
        let foreground = self.gc.foreground;
        match self.gc.fill_style & 0x3 {
            // Solid
            0 => Some(foreground),
            // Tiled：用 tile 平铺（tile 以可绘制对象原点对齐）
            1 => self.tile.map(|tile| tile.sample(x, y)),
            // Stippled：stipple 为 1 的像素画前景，为 0 的保持原样
            2 => self
                .stipple
                .filter(|stipple| stipple.sample(x, y) & 1 != 0)
                .map(|_| foreground),
            // OpaqueStippled：为 0 的像素画背景
            _ => self.stipple.map(|stipple| {
                if stipple.sample(x, y) & 1 != 0 {
                    foreground
                } else {
                    self.gc.background.unwrap_or(foreground)
                }
            }),
        }
    }

    /// 填充一行上的闭区间 `x0..=x1`。
    pub fn span(&mut self, y: i32, x0: i32, x1: i32) {
        if y < 0 || y >= self.height as i32 || x1 < x0 {
            return;
        }
        let start = x0.max(0);
        let end = x1.min(self.width as i32 - 1);
        if end < start {
            return;
        }
        // 实心填充是最常见路径：省掉逐像素的样式判断
        if self.gc.fill_style & 0x3 == 0 {
            let foreground = self.gc.foreground;
            for x in start..=end {
                self.paint(x, y, foreground);
            }
            return;
        }
        for x in start..=end {
            if let Some(source) = self.fill_source(x, y) {
                self.paint(x, y, source);
            }
        }
    }

    /// 实心矩形填充（按 fill style）。
    pub fn fill_rect(&mut self, x: i32, y: i32, width: i32, height: i32) {
        if width <= 0 || height <= 0 {
            return;
        }
        let top = y.max(0);
        let bottom = (y + height - 1).min(self.height as i32 - 1);
        for row in top..=bottom {
            self.span(row, x, x + width - 1);
        }
    }

    /// 实心填充，绕过 GC（ClearArea 用）。
    pub fn fill_solid(&mut self, x: i32, y: i32, width: i32, height: i32, value: u32) {
        if width <= 0 || height <= 0 {
            return;
        }
        let left = x.max(0);
        let top = y.max(0);
        let right = (x + width).min(self.width as i32);
        let bottom = (y + height).min(self.height as i32);
        if right <= left || bottom <= top {
            return;
        }
        for row in top..bottom {
            let start = row as usize * self.width + left as usize;
            let end = row as usize * self.width + right as usize;
            self.pixels[start..end].fill(value);
        }
    }

    /// 虚线图案：返回 `Some(pattern)` 表示按图案取舍，`None` 表示实线。
    ///
    /// `SetDashes` 未设置时按线型给默认图案（X11 规定 OnOffDash/DoubleDash 的默认
    /// 是 `4 4`）。`dash_offset` 是图案的起始相位。
    fn dash_pattern(&self) -> Option<Vec<bool>> {
        if self.gc.line_style == u32::from(LineStyle::SOLID) {
            return None;
        }
        // dashes 为空时用默认图案；只有一个值时按"亮/灭交替同一长度"处理
        let dashes: Vec<u8> = if self.gc.dashes.is_empty() {
            vec![4, 4]
        } else {
            self.gc.dashes.clone()
        };
        let total: usize = dashes.iter().map(|len| usize::from(*len)).sum();
        if total == 0 {
            return None;
        }
        let mut pattern = Vec::with_capacity(total);
        let mut on = true;
        for len in &dashes {
            for _ in 0..*len {
                pattern.push(on);
            }
            on = !on;
        }
        let offset = usize::try_from(self.gc.dash_offset).unwrap_or(0) % total;
        if offset != 0 {
            pattern.rotate_left(offset);
        }
        Some(pattern)
    }

    /// 按线宽画一个"笔尖"：以 (x, y) 为中心、边长 `thickness` 的方块。
    ///
    /// X11 的粗线是以路径为中心加宽，端点默认是 butt 样式。用方块笔尖近似，
    /// 对 1..8 像素的字宽足够；斜接/圆角/butt 的精确差异留给后续里程碑。
    fn stamp(&mut self, x: i32, y: i32, thickness: i32) {
        if thickness <= 1 {
            if let Some(source) = self.fill_source(x, y) {
                self.paint(x, y, source);
            }
            return;
        }
        let offset = (thickness - 1) / 2;
        for row in (y - offset)..=(y - offset + thickness - 1) {
            for column in (x - offset)..=(x - offset + thickness - 1) {
                if let Some(source) = self.fill_source(column, row) {
                    self.paint(column, row, source);
                }
            }
        }
    }

    /// 一条直线（受线宽与线型影响）。
    pub fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        self.polyline(&[(x0, y0), (x1, y1)]);
    }

    /// 折线：段与段之间共享虚线相位，顶点处补一个笔尖当连接。
    pub fn polyline(&mut self, points: &[(i32, i32)]) {
        if points.len() < 2 {
            return;
        }
        let pattern = self.dash_pattern();
        let thickness = self.gc.line_width.max(1) as i32;
        let mut phase = 0usize;

        for pair in points.windows(2) {
            let (x0, y0) = pair[0];
            let (x1, y1) = pair[1];
            let dx = (x1 - x0).abs();
            let dy = -(y1 - y0).abs();
            let sx = if x0 < x1 { 1 } else { -1 };
            let sy = if y0 < y1 { 1 } else { -1 };
            let mut error = dx + dy;
            let (mut x, mut y) = (x0, y0);
            loop {
                let draw = match &pattern {
                    Some(pattern) => pattern[phase % pattern.len()],
                    None => true,
                };
                if draw {
                    self.stamp(x, y, thickness);
                }
                phase += 1;
                if x == x1 && y == y1 {
                    break;
                }
                let doubled = error * 2;
                if doubled >= dy {
                    error += dy;
                    x += sx;
                }
                if doubled <= dx {
                    error += dx;
                    y += sy;
                }
            }
        }
    }

    /// 矩形边框。
    pub fn stroke_rect(&mut self, x: i32, y: i32, width: i32, height: i32) {
        if width <= 0 || height <= 0 {
            return;
        }
        let right = x + width - 1;
        let bottom = y + height - 1;
        self.polyline(&[(x, y), (right, y), (right, bottom), (x, bottom), (x, y)]);
    }

    /// 圆弧描边。
    pub fn arc(&mut self, arc: &Arc) {
        let points = raster::arc_outline(arc, 0.7);
        match points.len() {
            0 => {}
            1 => {
                let thickness = self.gc.line_width.max(1) as i32;
                self.stamp(points[0].0, points[0].1, thickness);
            }
            _ => self.polyline(&points),
        }
    }

    /// 圆弧填充（扇形或弓形，由 GC 的 arc_mode 决定）。
    pub fn fill_arc(&mut self, arc: &Arc) {
        let pie_slice = self.gc.arc_mode != u32::from(ArcMode::CHORD);
        for span in raster::fill_arc_spans(arc, pie_slice) {
            self.span(span.y, span.x0, span.x1);
        }
    }

    /// 多边形填充（`shape` 决定奇偶规则还是环绕数规则）。
    pub fn fill_polygon(&mut self, points: &[(i32, i32)], shape: PolyShape) {
        for span in raster::polygon_spans(points, shape) {
            self.span(span.y, span.x0, span.x1);
        }
    }

    /// 画一个字符：先按需铺背景，再把字形中为 1 的像素画成前景色。
    fn draw_char(&mut self, pen_x: i32, baseline: i32, ch: u16) {
        let top = baseline - i32::from(font::ASCENT);
        let cell_width = i32::from(font::CHAR_WIDTH);
        let cell_height = i32::from(font::CELL_HEIGHT);

        // 背景只在 GC 明确设置了 background 时填充：默认（未设置）时 X11 语义是
        // "不填背景"，只画前景位——否则文字周围会出现一片色块。
        if let Some(background) = self.gc.background {
            let bottom = top + cell_height - 1;
            for row in top..=bottom {
                for x in pen_x..(pen_x + cell_width) {
                    self.paint(x, row, background);
                }
            }
        }

        let rows = font::glyph(ch);
        let foreground = self.gc.foreground;
        for (row_index, bits) in rows.iter().enumerate() {
            if *bits == 0 {
                continue;
            }
            let y = top + row_index as i32;
            for column in 0..cell_width {
                if bits & (1 << column) != 0 {
                    self.paint(pen_x + column, y, foreground);
                }
            }
        }
    }

    /// 画一段 8 位字符（`ImageText8` / `PolyText8` 的基本单元）。
    ///
    /// 返回画完后的笔位置（像素），供 PolyText 的 `delta` 使用。
    pub fn text8(&mut self, x: i32, baseline: i32, bytes: &[u8]) -> i32 {
        let mut pen = x;
        for &byte in bytes {
            self.draw_char(pen, baseline, u16::from(byte));
            pen += i32::from(font::CHAR_WIDTH);
        }
        pen
    }

    /// 画一段 16 位字符（`ImageText16` / `PolyText16`）。
    ///
    /// 内置字体只有 Latin-1，超出范围的字符走缺字字形。
    pub fn text16(&mut self, x: i32, baseline: i32, chars: &[u16]) -> i32 {
        let mut pen = x;
        for &ch in chars {
            self.draw_char(pen, baseline, ch);
            pen += i32::from(font::CHAR_WIDTH);
        }
        pen
    }
}

/// GC raster op（GX）。`source` 是前景/图案，`destination` 是画布原值。
pub fn gx(function: u32, source: u32, destination: u32) -> u32 {
    match function & 0xf {
        0x0 => 0,
        0x1 => source & destination,
        0x2 => source & !destination,
        0x3 => source,
        0x4 => !source & destination,
        0x5 => destination,
        0x6 => source ^ destination,
        0x7 => source | destination,
        0x8 => !(source | destination),
        0x9 => !(source ^ destination),
        0xa => !destination,
        0xb => source | !destination,
        0xc => !source,
        0xd => !source | destination,
        0xe => !(source & destination),
        0xf => 0xffff_ffff,
        _ => source,
    }
}

/// 把 PutImage 的 ZPixmap 数据（每像素 4 字节）写进画布。
///
/// 返回 false 表示数据长度不够（调用方回 BadLength）。超出画布的部分静默裁剪。
pub fn put_zpixmap(
    canvas: &mut Canvas<'_>,
    dst_x: i32,
    dst_y: i32,
    width: u16,
    height: u16,
    data: &[u8],
) -> bool {
    let width = width as usize;
    let height = height as usize;
    if data.len() < width * height * 4 {
        return false;
    }
    for row in 0..height {
        for column in 0..width {
            let offset = (row * width + column) * 4;
            let pixel = u32::from_ne_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]);
            canvas.store(dst_x + column as i32, dst_y + row as i32, pixel);
        }
    }
    true
}

/// ZPixmap 按**目标深度**解包写进画布。
///
/// 行宽按 X11 规则对齐：每行字节数补齐到 32 位边界
/// （`bitmap_format_scanline_pad = 32`）。深度 → 每像素字节数：
/// 24/32 → 4 字节（0x00RRGGBB / 0xAARRGGBB，取低 24 位）；
/// 16 → 2 字节（RGB565，扩展到 8 位通道）；8 → 1 字节（灰度展开）；
/// 4 → 半字节打包（灰度展开）。
pub fn put_zpixmap_depth(
    canvas: &mut Canvas<'_>,
    dst_x: i32,
    dst_y: i32,
    width: u16,
    height: u16,
    depth: u8,
    data: &[u8],
) -> bool {
    let width = width as usize;
    let height = height as usize;
    if width == 0 || height == 0 {
        return false;
    }

    /// 一行的字节数（含对齐补齐）。
    fn row_stride(width: usize, depth: u8) -> Option<usize> {
        let raw = match depth {
            24 | 32 => width.checked_mul(4)?,
            16 => width.checked_mul(2)?,
            8 => width,
            4 => width.div_ceil(2),
            _ => return None,
        };
        Some(raw.div_ceil(4) * 4)
    }
    let Some(stride) = row_stride(width, depth) else {
        return false;
    };

    /// 8 位通道的低多位展开到全 8 位（i<<3)|(i>>2) 这类复制高位，避免暗化。
    let expand5 = |v: u8| -> u32 { (u32::from(v) << 3) | u32::from(v >> 2) };
    let expand6 = |v: u8| -> u32 { (u32::from(v) << 2) | u32::from(v >> 4) };

    for row in 0..height {
        let Some(row_start) = row.checked_mul(stride) else { return false; };
        let Some(line) = data.get(row_start..row_start + stride) else { return false; };
        for column in 0..width {
            let pixel: u32 = match depth {
                24 | 32 => {
                    let offset = column * 4;
                    u32::from_ne_bytes([line[offset], line[offset + 1], line[offset + 2], line[offset + 3]])
                }
                16 => {
                    let offset = column * 2;
                    let raw = u16::from_ne_bytes([line[offset], line[offset + 1]]);
                    let r = expand5(((raw >> 11) & 0x1f) as u8);
                    let g = expand6(((raw >> 5) & 0x3f) as u8);
                    let b = expand5((raw & 0x1f) as u8);
                    (r << 16) | (g << 8) | b
                }
                8 => {
                    let v = line[column];
                    let g = (u32::from(v) << 4) | u32::from(v);
                    (g << 16) | (g << 8) | g
                }
                4 => {
                    let byte = line[column / 2];
                    let v = if column % 2 == 0 { byte >> 4 } else { byte & 0x0f };
                    let g = (u32::from(v) << 4) | u32::from(v);
                    (g << 16) | (g << 8) | g
                }
                _ => return false,
            };
            // depth 32 (ARGB32) 保留完整 u32（含 alpha 字节）——sample_picture
            // 按 alpha 通道采样；depth 24 按服务端 0x00RRGGBB 约定抹掉高位。
            let stored = if depth == 32 { pixel } else { pixel & 0x00ff_ffff };
            canvas.store(dst_x + column as i32, dst_y + row as i32, stored);
        }
    }
    true
}

/// 把 1 位深的位图数据写进画布（stipple / tile / bitmap 的典型用法）。
///
/// `stride_bits` 是一行的位宽（含补齐），X11 的 `bitmap_format_scanline_pad` 是 32，
/// 所以通常是 `ceil(width / 32) * 32`。
pub fn put_bitmap(
    canvas: &mut Canvas<'_>,
    dst_x: i32,
    dst_y: i32,
    width: u16,
    height: u16,
    stride_bits: usize,
    data: &[u8],
    foreground: u32,
    background: Option<u32>,
) -> bool {
    let width_usize = width as usize;
    let height_usize = height as usize;
    let stride_bytes = stride_bits.div_ceil(8);
    if data.len() < stride_bytes * height_usize {
        return false;
    }
    for row in 0..height_usize {
        for column in 0..width_usize {
            let bit_index = column;
            let byte = data[row * stride_bytes + bit_index / 8];
            let set = byte & (0x80 >> (bit_index % 8)) != 0;
            let x = dst_x + column as i32;
            let y = dst_y + row as i32;
            match (set, background) {
                (true, _) => canvas.store(x, y, foreground),
                (false, Some(background)) => canvas.store(x, y, background),
                (false, None) => {}
            }
        }
    }
    true
}

/// GetImage 的 ZPixmap 路径：把矩形读成小端 4 字节像素。
pub fn get_zpixmap(
    pixels: &[u32],
    canvas_width: usize,
    canvas_height: usize,
    x: i32,
    y: i32,
    width: u16,
    height: u16,
) -> Option<Vec<u8>> {
    let width = width as usize;
    let height = height as usize;
    if x < 0
        || y < 0
        || x + width as i32 > canvas_width as i32
        || y + height as i32 > canvas_height as i32
    {
        return None;
    }
    let mut data = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        for column in 0..width {
            // 裸索引换成 get：缓冲区短于声明尺寸时（借还漏配）不得 panic
            let pixel = pixels
                .get((y as usize + row) * canvas_width + x as usize + column)
                .copied()
                .unwrap_or(0);
            data.extend_from_slice(&pixel.to_ne_bytes());
        }
    }
    Some(data)
}

/// GetImage 的位图路径：1 位深的可绘制对象按位打包，一行补齐到 32 位。
pub fn get_bitmap(
    pixels: &[u32],
    canvas_width: usize,
    canvas_height: usize,
    x: i32,
    y: i32,
    width: u16,
    height: u16,
) -> Option<Vec<u8>> {
    let width_usize = width as usize;
    let height_usize = height as usize;
    if x < 0
        || y < 0
        || x + width as i32 > canvas_width as i32
        || y + height as i32 > canvas_height as i32
    {
        return None;
    }
    let stride_bytes = width_usize.div_ceil(32) * 4;
    let mut data = vec![0u8; stride_bytes * height_usize];
    for row in 0..height_usize {
        for column in 0..width_usize {
            // 裸索引换成 get：缓冲区短于声明尺寸时（借还漏配）不得 panic
            let pixel = pixels
                .get((y as usize + row) * canvas_width + x as usize + column)
                .copied()
                .unwrap_or(0);
            if pixel & 1 != 0 {
                data[row * stride_bytes + column / 8] |= 0x80 >> (column % 8);
            }
        }
    }
    Some(data)
}

/// 在画布内做矩形拷贝（源像素已经快照到 `source`）。
#[allow(clippy::too_many_arguments)]
pub fn copy_area(
    canvas: &mut Canvas<'_>,
    source: &[u32],
    source_width: usize,
    src_x: i32,
    src_y: i32,
    dst_x: i32,
    dst_y: i32,
    width: u16,
    height: u16,
) {
    for row in 0..height as i32 {
        for column in 0..width as i32 {
            let sx = src_x + column;
            let sy = src_y + row;
            if sx < 0 || sy < 0 {
                continue;
            }
            let source_index = sy as usize * source_width + sx as usize;
            let Some(&pixel) = source.get(source_index) else {
                continue;
            };
            canvas.paint(dst_x + column, dst_y + row, pixel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb_protocol::protocol::xproto::{FillStyle, GX};

    fn gc() -> GcData {
        GcData::new(1, 1, 24)
    }

    #[test]
    fn raster_ops_follow_the_protocol_truth_table() {
        assert_eq!(gx(u32::from(GX::CLEAR), 0xff, 0x0f), 0);
        assert_eq!(gx(u32::from(GX::COPY), 0xff, 0x0f), 0xff);
        assert_eq!(gx(u32::from(GX::XOR), 0xff, 0x0f), 0xf0);
        assert_eq!(gx(u32::from(GX::INVERT), 0xff, 0x0f), !0x0fu32);
        assert_eq!(gx(u32::from(GX::SET), 0, 0), 0xffff_ffff);
    }

    #[test]
    fn fill_rect_is_clipped_and_applies_plane_mask() {
        let mut pixels = vec![0x00ff_ffffu32; 16 * 16];
        let mut gc = gc();
        gc.foreground = 0x00ff_0000;
        gc.plane_mask = 0x0000_ff00;
        {
            let mut canvas = Canvas::new(&mut pixels, 16, 16, &gc);
            canvas.fill_rect(-4, -4, 10, 10);
        }
        // plane mask 只放行绿色通道：G 取前景的 0x00，R/B 保留目标原来的 0xff
        assert_eq!(pixels[0], 0x00ff_00ff);
        // 裁剪区域外不受影响
        assert_eq!(pixels[10 * 16 + 10], 0x00ff_ffff);
    }

    #[test]
    fn put_and_get_image_round_trip() {
        let mut pixels = vec![0u32; 4 * 4];
        let gc = gc();
        {
            let mut canvas = Canvas::new(&mut pixels, 4, 4, &gc);
            let data = [0x11u8, 0x22, 0x33, 0x00].repeat(4);
            assert!(put_zpixmap(&mut canvas, 1, 1, 2, 2, &data));
        }
        let data = [0x11u8, 0x22, 0x33, 0x00].repeat(4);
        let read = get_zpixmap(&pixels, 4, 4, 1, 1, 2, 2).unwrap();
        assert_eq!(read, data);
        assert!(get_zpixmap(&pixels, 4, 4, 3, 3, 2, 2).is_none(), "越界必须拒绝");
    }

    #[test]
    fn clip_rectangles_limit_drawing_area() {
        let mut pixels = vec![0u32; 16 * 16];
        let mut gc = gc();
        gc.foreground = 0x0000_00ff;
        let rectangles = [Rectangle { x: 4, y: 4, width: 4, height: 4 }];
        let clip = Clip { x_origin: 0, y_origin: 0, rectangles: &rectangles, mask: None };
        {
            let mut canvas = Canvas::with_gc(&mut pixels, 16, 16, &gc, Some(clip), None, None);
            canvas.fill_rect(0, 0, 16, 16);
        }
        assert_eq!(pixels[5 * 16 + 5], 0x0000_00ff, "裁剪区内要画上");
        assert_eq!(pixels[0], 0, "裁剪区外必须保持原样");
        assert_eq!(pixels[8 * 16 + 8], 0); // 刚好越界
    }

    #[test]
    fn stippled_fill_paints_only_set_bits() {
        let mut pixels = vec![0u32; 8 * 8];
        // 1x2 的 stipple：第一行亮、第二行灭
        let stipple_pixels = [1u32, 0u32];
        let stipple = Bitmap { pixels: &stipple_pixels, width: 1, height: 2 };
        let mut gc = gc();
        gc.foreground = 0x00ff_0000;
        gc.fill_style = u32::from(FillStyle::STIPPLED);
        {
            let mut canvas = Canvas::with_gc(&mut pixels, 8, 8, &gc, None, None, Some(stipple));
            canvas.fill_rect(0, 0, 8, 8);
        }
        assert_eq!(pixels[0], 0x00ff_0000, "偶数行点亮");
        assert_eq!(pixels[8], 0, "奇数行保持原样");
    }

    #[test]
    fn dashed_line_leaves_gaps() {
        let mut pixels = vec![0u32; 32];
        let mut gc = gc();
        gc.foreground = 0x00ff_ffff;
        gc.line_style = u32::from(LineStyle::ON_OFF_DASH);
        gc.dashes = vec![2, 2];
        {
            let mut canvas = Canvas::new(&mut pixels, 32, 1, &gc);
            canvas.line(0, 0, 31, 0);
        }
        // 图案 2 亮 2 灭：第 0、1 像素亮，第 2、3 像素灭
        assert_eq!(pixels[0], 0x00ff_ffff);
        assert_eq!(pixels[1], 0x00ff_ffff);
        assert_eq!(pixels[2], 0);
        assert_eq!(pixels[3], 0);
        assert_eq!(pixels[4], 0x00ff_ffff);
    }

    #[test]
    fn text_paints_foreground_and_optional_background() {
        let mut pixels = vec![0u32; 8 * 16];
        let mut gc = gc();
        gc.foreground = 0x00ff_ffff;
        gc.background = Some(0x0000_1100);
        {
            let mut canvas = Canvas::new(&mut pixels, 8, 16, &gc);
            canvas.text8(0, 10, b"I");
        }
        // 背景铺满整个字格
        assert_eq!(pixels[0], 0x0000_1100);
        // 'I' 的字形里应当有前景像素
        assert!(pixels.iter().any(|pixel| *pixel == 0x00ff_ffff), "文字必须画出前景");
    }
}
