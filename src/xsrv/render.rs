//! RENDER 扩展（版本 0.11 核心子集）。
//!
//! 线格式编解码全部复用 `x11rb-protocol` 的 `render` 模块，本文件只实现
//! "解析之后做什么"。像素模型沿用 `state.rs` 的约定：drawable 的像素缓冲是
//! `0x00RRGGBB`，**没有 alpha 通道**——目标永远按不透明处理（alpha = 1）；
//! 源端（solid fill / glyph / picture）按预乘 alpha 语义混合。
//!
//! 支持（对 toolkit 的重要性排序）：
//! - QueryVersion / QueryPictFormats（ARGB32、RGB24、A8 三种直接格式）；
//! - Picture 生命周期（Create/Change/Free/SetPictureClipRectangles/
//!   SetPictureFilter/SetPictureTransform）；
//! - CreateSolidFill、FillRectangles（Over / Source）；
//! - GlyphSet / AddGlyphs / FreeGlyphs / Composite(Glyphs)——文字渲染主通道；
//! - Composite（Over / Source / Atop / In，单位变换）；
//! - Trapezoids / Triangles / TriStrip / TriFan（span 扫描栅格化 + 按 op 混合）；
//! - QueryPictIndexValues（只报 direct 格式，按规范回 BadMatch）。
//!
//! 不做（见 docs/x11-extensions.md）：Gradients 回 BadImplementation 让
//! toolkit 走回退；picture 的 transform（缩放/旋转）、convolution filter、
//! repeat 的平滑回卷、A1 字形格式、索引色格式。客户端超出子集的属性
//! （alphamap / clipmask pixmap / polyedge / polymode / dither /
//! componentalpha）按忽略处理——注释里逐条说明。

use std::collections::HashMap;

use x11rb_protocol::protocol::render;
use x11rb_protocol::protocol::xproto;
use x11rb_protocol::x11_utils::Serialize;

use super::error::{BAD_VALUE, XError};
use super::handshake::{ROOT_VISUAL, SCREEN_DEPTH};
use super::raster::{self, Span};
use super::region::Rects;
use super::state::{Glyph, GlyphSetData, PictureData, PictureSource, ServerState};

// ---- 注册表常量（extensions.rs 的表引用这些值） ----

/// RENDER 的 major opcode（Composite 的 139 之后第一个空闲值）。
pub const OPCODE_RENDER: u8 = 140;
/// RENDER 没有事件。
pub const FIRST_EVENT_RENDER: u8 = 0;
/// 错误基址：避开核心错误（1..=17）与 XFIXES/DAMAGE 的 128..=129。
pub const FIRST_ERROR_RENDER: u8 = 150;
pub const BAD_PICT_FORMAT: u8 = FIRST_ERROR_RENDER + render::PICT_FORMAT_ERROR;
pub const BAD_PICTURE: u8 = FIRST_ERROR_RENDER + render::PICTURE_ERROR;
pub const BAD_PICT_OP: u8 = FIRST_ERROR_RENDER + render::PICT_OP_ERROR;
pub const BAD_GLYPH_SET: u8 = FIRST_ERROR_RENDER + render::GLYPH_SET_ERROR;
pub const BAD_GLYPH: u8 = FIRST_ERROR_RENDER + render::GLYPH_ERROR;

/// 服务端支持的版本。
pub const RENDER_MAJOR_VERSION: u32 = 0;
pub const RENDER_MINOR_VERSION: u32 = 11;

// ---- Pictformat ID 表（QueryPictFormats 与所有请求共用同一组数字） ----

pub const FORMAT_ARGB32: u32 = 1;
pub const FORMAT_RGB24: u32 = 2;
pub const FORMAT_A8: u32 = 3;

/// 格式深度；不认识的 ID 返回 `None`（调用方回 BadPictFormat）。
pub fn pict_format_depth(id: u32) -> Option<u8> {
    match id {
        FORMAT_ARGB32 => Some(32),
        FORMAT_RGB24 => Some(24),
        FORMAT_A8 => Some(8),
        _ => None,
    }
}

/// 组装一个 Pictforminfo（直接格式；本服务器没有索引色）。
///
/// NyaTerm: `shift/mask` 字段的线上语义是"通道位置 / **通道位掩码**"——cairo
/// 用 `a_mask = alphaMask << alphaShift` 组合出位掩码再做 pixman 映射，
/// 掩码必须形如 0xff（此前误发位数 8，组合出 0x8 这种非 0xff 掩码，触发
/// cairo `_pixman_format_from_masks` 的 assert 崩溃——firefox 启动即崩）。
fn pict_form_info(id: u32) -> render::Pictforminfo {
    let (depth, red, green, blue, alpha) = match id {
        FORMAT_ARGB32 => (32, (16u16, 0xffu16), (8, 0xff), (0, 0xff), (24, 0xff)),
        FORMAT_RGB24 => (24, (16, 0xff), (8, 0xff), (0, 0xff), (0, 0)),
        _ => (8, (0, 0), (0, 0), (0, 0), (0, 0xff)),
    };
    render::Pictforminfo {
        id: render::Pictformat::from(id),
        type_: render::PictType::DIRECT,
        depth,
        direct: render::Directformat {
            red_shift: red.0,
            red_mask: red.1,
            green_shift: green.0,
            green_mask: green.1,
            blue_shift: blue.0,
            blue_mask: blue.1,
            alpha_shift: alpha.0,
            alpha_mask: alpha.1,
        },
        colormap: 0,
    }
}

// ---- 像素混合原语 ----

/// 8 位通道按 alpha 混合：`src*a + dst*(1-a)`，`a` 与通道都是 0..=255。
fn mix8(dst: u32, src: u32, a: u32) -> u32 {
    (src * a + dst * (255 - a) + 127) / 255
}

/// Over 公式的一步：`src` 已经是预乘色，不再乘 alpha——
/// `out = src_premul + dst*(1-a)`（结果保持在 0..=255 量级）。
fn mix_over(dst: u32, src_premul: u32, a: u32) -> u32 {
    (src_premul * 255 + dst * (255 - a) + 127) / 255
}

/// 从 `0x00RRGGBB` 像素里取 8 位通道（`shift` = 16 红 / 8 绿 / 0 蓝）。
fn channel(pixel: u32, shift: u32) -> u32 {
    (pixel >> shift) & 0xff
}

/// 把三个 8 位通道合回 `0x00RRGGBB`。
fn pixel_of(r: u32, g: u32, b: u32) -> u32 {
    (r << 16) | (g << 8) | b
}

/// 把 16 位通道颜色转成预乘的 8 位 `(r, g, b, a)`。
///
/// 预乘语义：混合公式 `out = src*a + dst*(1-a)` 里的 src 是"颜色×alpha"，
/// 所以这里先把 RGB 乘上 alpha。
fn premultiply(color: render::Color) -> (u32, u32, u32, u32) {
    let a = (color.alpha >> 8) as u32;
    (
        ((color.red >> 8) as u32 * a + 127) / 255,
        ((color.green >> 8) as u32 * a + 127) / 255,
        ((color.blue >> 8) as u32 * a + 127) / 255,
        a,
    )
}

/// 在一个不透明目标像素上按 op 应用一个预乘源色。
///
/// 目标 alpha 恒为 1（`0x00RRGGBB` 无 alpha 通道），因此 Atop 与 Over 等价、
/// In 退化为 `src*src_alpha`。返回 `None` 表示 op 不支持（规范要求未定义的
/// op 报 BadPictOp，不能静默乱画）。
fn blend_pixel(op: u8, dst: u32, src: (u32, u32, u32, u32)) -> Option<u32> {
    let (dr, dg, db) = (channel(dst, 16), channel(dst, 8), channel(dst, 0));
    let (sr, sg, sb, sa) = src;

    // ---- Porter-Duff 族（目标不透明，da = 1）----
    match op {
        o if o == u8::from(render::PictOp::CLEAR) => return Some(0x0000_0000),
        o if o == u8::from(render::PictOp::SRC)
            || o == u8::from(render::PictOp::OVER_REVERSE) =>
        {
            return Some(pixel_of(sr, sg, sb));
        }
        o if o == u8::from(render::PictOp::DST)
            || o == u8::from(render::PictOp::ATOP_REVERSE) =>
        {
            return Some(pixel_of(dr, dg, db));
        }
        o if o == u8::from(render::PictOp::OVER)
            || o == u8::from(render::PictOp::ATOP) =>
        {
            return Some(pixel_of(
                mix_over(dr, sr, sa),
                mix_over(dg, sg, sa),
                mix_over(db, sb, sa),
            ));
        }
        // IN：目标不透明时就是预乘源色本身
        o if o == u8::from(render::PictOp::IN) => {
            return Some(pixel_of(sr, sg, sb));
        }
        // IN_REVERSE / OUT_REVERSE / XOR：dst·(1−sa)
        o if o == u8::from(render::PictOp::IN_REVERSE)
            || o == u8::from(render::PictOp::OUT_REVERSE)
            || o == u8::from(render::PictOp::XOR) =>
        {
            return Some(pixel_of(
                dr * (255 - sa) / 255,
                dg * (255 - sa) / 255,
                db * (255 - sa) / 255,
            ));
        }
        // OUT：目标不透明 → (1−da)·src = 0
        o if o == u8::from(render::PictOp::OUT) => return Some(0x0000_0000),
        // ADD / SATURATE：预乘相加（饱和截断）
        o if o == u8::from(render::PictOp::ADD)
            || o == u8::from(render::PictOp::SATURATE) =>
        {
            return Some(pixel_of(
                (dr + sr).min(255),
                (dg + sg).min(255),
                (db + sb).min(255),
            ));
        }
        _ => {}
    }

    // ---- 混合模式（separable + HSL）：源反预乘后按 CSS/PDF 公式混色，
    // 结果 = (1−αs)·Cd + αs·B（目标不透明，输出按预乘处理 alpha=1）----
    if sa == 0 {
        return Some(pixel_of(dr, dg, db));
    }
    let cs = (sr as f64 * 255.0 / sa as f64, sg as f64 * 255.0 / sa as f64, sb as f64 * 255.0 / sa as f64);
    let cd = (dr as f64, dg as f64, db as f64);
    let blended = match op {
        o if o == u8::from(render::PictOp::MULTIPLY) => sep(cd, cs, |b, s| b * s / 255.0),
        o if o == u8::from(render::PictOp::SCREEN) => sep(cd, cs, |b, s| b + s - b * s / 255.0),
        // Overlay(backdrop, source) = HardLight(source, backdrop)
        o if o == u8::from(render::PictOp::OVERLAY) => sep(cd, cs, |b, s| hard_light(b, s)),
        o if o == u8::from(render::PictOp::DARKEN) => sep(cd, cs, |b, s| b.min(s)),
        o if o == u8::from(render::PictOp::LIGHTEN) => sep(cd, cs, |b, s| b.max(s)),
        o if o == u8::from(render::PictOp::COLOR_DODGE) => sep(cd, cs, color_dodge),
        o if o == u8::from(render::PictOp::COLOR_BURN) => sep(cd, cs, color_burn),
        o if o == u8::from(render::PictOp::HARD_LIGHT) => sep(cd, cs, |b, s| hard_light(b, s)),
        o if o == u8::from(render::PictOp::SOFT_LIGHT) => sep(cd, cs, soft_light),
        o if o == u8::from(render::PictOp::DIFFERENCE) => sep(cd, cs, |b, s| (b - s).abs()),
        o if o == u8::from(render::PictOp::EXCLUSION) => {
            sep(cd, cs, |b, s| b + s - b * s / 255.0)
        }
        o if o == u8::from(render::PictOp::HSL_HUE) => hsl_hue(cd, cs),
        o if o == u8::from(render::PictOp::HSL_SATURATION) => hsl_saturation(cd, cs),
        o if o == u8::from(render::PictOp::HSL_COLOR) => hsl_color(cd, cs),
        o if o == u8::from(render::PictOp::HSL_LUMINOSITY) => hsl_luminosity(cd, cs),
        _ => return None,
    };
    Some(pixel_of(
        ((1.0 - sa as f64 / 255.0) * dr as f64 + sa as f64 / 255.0 * blended.0) as u32,
        ((1.0 - sa as f64 / 255.0) * dg as f64 + sa as f64 / 255.0 * blended.1) as u32,
        ((1.0 - sa as f64 / 255.0) * db as f64 + sa as f64 / 255.0 * blended.2) as u32,
    ))
}

/// 逐通道混合公式应用。
fn sep(
    cd: (f64, f64, f64),
    cs: (f64, f64, f64),
    f: impl Fn(f64, f64) -> f64,
) -> (f64, f64, f64) {
    (f(cd.0, cs.0), f(cd.1, cs.1), f(cd.2, cs.2))
}

/// Overlay/HardLight 核心公式（CSS）：低半区乘法、高半区屏幕。
fn hard_light(backdrop: f64, source: f64) -> f64 {
    if source <= 127.5 {
        2.0 * source * backdrop / 255.0
    } else {
        255.0 - 2.0 * (255.0 - source) * (255.0 - backdrop) / 255.0
    }
}

/// ColorDodge（CSS）。
fn color_dodge(backdrop: f64, source: f64) -> f64 {
    if backdrop == 0.0 {
        0.0
    } else if source >= 255.0 {
        255.0
    } else {
        255.0 * backdrop / (255.0 - source)
    }
}

/// ColorBurn（CSS）。
fn color_burn(backdrop: f64, source: f64) -> f64 {
    if backdrop >= 255.0 {
        255.0
    } else if source == 0.0 {
        0.0
    } else {
        255.0 - 255.0 * (255.0 - backdrop) / source
    }
}

/// SoftLight（CSS/PDF 公式）。
fn soft_light(backdrop: f64, source: f64) -> f64 {
    let g = if backdrop <= 64.0 {
        ((16.0 * backdrop - 12.0) * backdrop + 4.0) * backdrop
    } else {
        backdrop.sqrt()
    };
    if source <= 127.5 {
        backdrop - (255.0 - 2.0 * source) * backdrop * (255.0 - backdrop) / 255.0
    } else {
        backdrop + (2.0 * source - 255.0) * (g - backdrop)
    }
}

/// 亮度（CSS Rec.601 系数）。
fn hsl_lum(c: (f64, f64, f64)) -> f64 {
    0.3 * c.0 + 0.59 * c.1 + 0.11 * c.2
}

/// SetSat（CSS）：把通道缩放到饱和度 s。
fn hsl_set_sat(c: (f64, f64, f64), s: f64) -> (f64, f64, f64) {
    let mut order = [c.0, c.1, c.2];
    order.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if order[2] > order[0] {
        let map = |v: f64| (v - order[0]) * s / (order[2] - order[0]);
        let r = if c.0 == order[2] {
            s
        } else if c.0 == order[0] {
            0.0
        } else {
            map(c.0)
        };
        let g = if c.1 == order[2] {
            s
        } else if c.1 == order[0] {
            0.0
        } else {
            map(c.1)
        };
        let b = if c.2 == order[2] { s } else if c.2 == order[0] { 0.0 } else { map(c.2) };
        (r, g, b)
    } else {
        (0.0, 0.0, 0.0)
    }
}

/// SetLum（CSS）：把颜色调整到亮度 l，必要时裁剪到色域。
fn hsl_set_lum(c: (f64, f64, f64), l: f64) -> (f64, f64, f64) {
    let d = l - hsl_lum(c);
    let clip = |v: f64| -> f64 { v.max(0.0).min(255.0) };
    let mut r = clip(c.0 + d);
    let mut g = clip(c.1 + d);
    let mut b = clip(c.2 + d);
    // 裁剪后亮度偏差：按 1:1 通道近似回补（标准的 LuMt 差值分配）
    let cur = hsl_lum((r, g, b));
    let delta = l - cur;
    r = clip(r + delta);
    g = clip(g + delta);
    b = clip(b + delta);
    (r, g, b)
}

/// Hue / Saturation / Color：SetSat/SetLum 组合（公式各不相同，见 CSS）。
fn hsl_hue(cd: (f64, f64, f64), cs: (f64, f64, f64)) -> (f64, f64, f64) {
    hsl_set_lum(hsl_set_sat(cs, hsl_lum(cd)), hsl_lum(cd))
}
fn hsl_saturation(cd: (f64, f64, f64), cs: (f64, f64, f64)) -> (f64, f64, f64) {
    hsl_set_lum(hsl_set_sat(cd, hsl_lum(cs)), hsl_lum(cd))
}
fn hsl_color(cd: (f64, f64, f64), cs: (f64, f64, f64)) -> (f64, f64, f64) {
    hsl_set_lum(hsl_set_sat(cs, hsl_lum(cd)), hsl_lum(cd))
}
fn hsl_luminosity(cd: (f64, f64, f64), cs: (f64, f64, f64)) -> (f64, f64, f64) {
    hsl_set_lum(cd, hsl_lum(cs))
}

// ---- 源像素采样与目标写入的公共设施 ----

/// 绘制期间从资源表里借走的一块 drawable 像素（见 `state.rs::BorrowedBitmap`
/// 同一套手法：`mem::take` 移动 Vec 头指针，用完放回）。
struct BorrowedPixels {
    id: Option<u32>,
    pixels: Vec<u32>,
    width: usize,
    height: usize,
    /// 源 drawable 的深度：32 位 pixmap 的像素是 0xAARRGGBB（采样要取 alpha），
    /// 其余深度按服务端 0x00RRGGBB 不透明约定。
    depth: u8,
}

impl BorrowedPixels {
    /// 借走 `id` 的像素；`exclude` 是绘制目标自身（"自合成"时跳过，避免被取空）。
    fn take(state: &mut ServerState, id: u32, exclude: u32) -> Self {
        if id == exclude {
            return Self::empty();
        }
        let data = if let Some(window) = state.windows.get_mut(&id) {
            let depth = window.depth;
            (std::mem::take(&mut window.pixels), window.width, window.height, depth)
        } else if let Some(pixmap) = state.pixmaps.get_mut(&id) {
            let depth = pixmap.depth;
            (std::mem::take(&mut pixmap.pixels), pixmap.width, pixmap.height, depth)
        } else {
            return Self::empty();
        };
        Self {
            id: Some(id),
            pixels: data.0,
            width: data.1 as usize,
            height: data.2 as usize,
            depth: data.3,
        }
    }

    fn empty() -> Self {
        Self { id: None, pixels: Vec::new(), width: 0, height: 0, depth: 24 }
    }

    fn view(&self) -> Option<(&[u32], usize, usize)> {
        self.id.map(|_| (self.pixels.as_slice(), self.width, self.height))
    }

    fn restore(self, state: &mut ServerState) {
        if let Some(id) = self.id {
            if let Some(window) = state.windows.get_mut(&id) {
                window.pixels = self.pixels;
            } else if let Some(pixmap) = state.pixmaps.get_mut(&id) {
                pixmap.pixels = self.pixels;
            }
        }
    }
}

/// 从 picture 采样一个预乘 RGBA（坐标是源 drawable 的像素坐标）。
///
/// drawable 包装：像素无 alpha → alpha=255；Solid：整张图一个颜色。
/// `repeat=false` 时越界返回全透明；`repeat=true` 时坐标按尺寸回卷。
/// 源是 picture 自身 + 借来的像素缓冲，不再碰 `ServerState`，可以在持有
/// 目标可变借用时调用。
fn sample_picture(
    picture: &PictureData,
    x: i32,
    y: i32,
    borrowed: &BorrowedPixels,
) -> (u32, u32, u32, u32) {
    match &picture.source {
        PictureSource::Solid { red, green, blue, alpha } => premultiply(render::Color {
            red: *red,
            green: *green,
            blue: *blue,
            alpha: *alpha,
        }),
        PictureSource::LinearGradient { x1, y1, x2, y2, stops } => {
            let (x1, y1, x2, y2) = (*x1, *y1, *x2, *y2);
            let dx = x2 - x1;
            let dy = y2 - y1;
            let len2 = dx * dx + dy * dy;
            let t = if len2 == 0.0 {
                0.0
            } else {
                ((x as f64 - x1) * dx + (y as f64 - y1) * dy) / len2
            };
            gradient_color(stops, t)
        }
        PictureSource::RadialGradient { x1, y1, r1, x2, y2, r2, stops } => {
            let (x1, y1, r1, x2, y2, r2) = (*x1, *y1, *r1, *x2, *y2, *r2);
            let stops = stops.as_slice();
            // RENDER 规范 7.6：双圆渐变的 t 参数（取落在 [0,1] 的根）。
            let (px, py) = (x as f64 - x1, y as f64 - y1);
            let (gx, gy) = (x2 - x1, y2 - y1);
            let (dr, _dg) = (r2 - r1, r2 - r1);
            let a = gx * gx + gy * gy - dr * dr;
            let b = 2.0 * (px * gx + py * gy + r1 * dr);
            let cq = px * px + py * py - r1 * r1;
            let t = if a.abs() < f64::EPSILON {
                // 内圆退化为点：t = sqrt(点距²−r1²)/dr
                let d = (px * px + py * py - r1 * r1).sqrt();
                if dr != 0.0 { d / dr } else { 0.0 }
            } else {
                let disc = b * b - 4.0 * a * cq;
                if disc < 0.0 {
                    0.0
                } else {
                    let root = disc.sqrt();
                    let t1 = (-b + root) / (2.0 * a);
                    let t2 = (-b - root) / (2.0 * a);
                    // 取落在 0..=1 的根（规范约定用较大的有效根）
                    if (0.0..=1.0).contains(&t1) {
                        t1
                    } else if (0.0..=1.0).contains(&t2) {
                        t2
                    } else {
                        t1.clamp(0.0, 1.0)
                    }
                }
            };
            gradient_color(stops, t)
        }
        PictureSource::ConicalGradient { x: cx, y: cy, angle, stops } => {
            let stops = stops.as_slice();
            // 角度渐变：点相对中心的方位角 − 起始角，归一化到 0..=1 圈。
            let point_angle = ((y as f64 - cy).atan2(x as f64 - cx)).to_degrees();
            let mut t = (point_angle - angle) / 360.0;
            t -= t.floor();
            gradient_color(stops, t)
        }
        PictureSource::Drawable(_) => {
            let Some((pixels, width, height)) = borrowed.view() else {
                // 唯一借不到的情况是"源 == 目标"：按不透明黑采样，不放大面积空像素
                return (0, 0, 0, 255);
            };
            let (mut sx, mut sy) = (x, y);
            if picture.repeat && width > 0 && height > 0 {
                sx = sx.rem_euclid(width as i32);
                sy = sy.rem_euclid(height as i32);
            }
            if sx < 0 || sy < 0 || sx >= width as i32 || sy >= height as i32 {
                return (0, 0, 0, 0);
            }
            let pixel = pixels[sy as usize * width + sx as usize];
            // depth 32 (ARGB32) 的像素带 alpha 字节（0xAARRGGBB，firefox 的
            // UI 基底大量是 alpha=0 的透明层——强制不透明会渲染成全黑）；
            // 其余深度按服务端 0x00RRGGBB 不透明约定。
            if borrowed.depth == 32 {
                let alpha = channel(pixel, 24);
                (channel(pixel, 16), channel(pixel, 8), channel(pixel, 0), alpha)
            } else {
                (channel(pixel, 16), channel(pixel, 8), channel(pixel, 0), 255)
            }
        }
    }
}

/// 渐变取色：t 钳到 [0,1]，落在相邻停靠点之间时线性插值（未预乘通道），
/// 输出换算成 8 位并按 alpha 预乘（与 Solid 采样同语义）。
fn gradient_color(stops: &[crate::xsrv::state::GradientStop], t: f64) -> (u32, u32, u32, u32) {
    if stops.is_empty() {
        return (0, 0, 0, 255);
    }
    let t = t.clamp(0.0, 1.0);
    // 找到第一个 offset >= t 的停靠点及其前驱
    let upper = stops.iter().position(|s| s.offset >= t).unwrap_or(stops.len() - 1);
    let lower = upper.saturating_sub(1);
    let (a_stop, b_stop) = (&stops[lower], &stops[upper]);
    let span = b_stop.offset - a_stop.offset;
    let factor = if span <= 0.0 {
        0.0
    } else {
        ((t - a_stop.offset) / span).clamp(0.0, 1.0)
    };
    let lerp = |a: u16, b: u16| -> u32 {
        let av = a as f64 + (b as f64 - a as f64) * factor;
        (av / 257.0).round().min(255.0) as u32
    };
    let (r, g, b, a) = (
        lerp(a_stop.red, b_stop.red),
        lerp(a_stop.green, b_stop.green),
        lerp(a_stop.blue, b_stop.blue),
        lerp(a_stop.alpha, b_stop.alpha),
    );
    // 预乘（alpha 16 位 → 8 位）
    let af = a as f64 / 255.0;
    ((r as f64 / 257.0 * af).round() as u32,
     (g as f64 / 257.0 * af).round() as u32,
     (b as f64 / 257.0 * af).round() as u32,
     af.round() as u32)
}

/// picture 的裁剪判定：clip 区域存在时坐标要落进（SetPictureClipRectangles
/// 已经把 origin 平移进区域）。
fn clip_contains(picture: &PictureData, x: i32, y: i32) -> bool {
    match &picture.clip {
        Some(clip) => clip.contains(x, y),
        None => true,
    }
}

/// 写目标 drawable 后的收尾：标脏 damage + 已映射窗口重新合成
/// （与 `state.rs::draw_with_gc` 的收尾一致）。
fn finish_draw(state: &mut ServerState, drawable: u32) {
    state.mark_damage(drawable);
    if state.windows.get(&drawable).map(|w| w.mapped) == Some(true) {
        state.mark_compose_dirty();
    }
}

/// Composite / 形状类请求的公共部分：解析出 op、目标信息与源。
///
/// 目标必须是 drawable 包装的 picture（solid 不能当目标）；`src_pixels`
/// 是已经借走的源像素。
struct CompositeSetup {
    op: u8,
    drawable: u32,
    width: usize,
    height: usize,
    dst: PictureData,
    src: PictureData,
    src_pixels: BorrowedPixels,
}

fn composite_setup(
    state: &mut ServerState,
    op: u8,
    src_id: u32,
    dst_id: u32,
) -> Result<CompositeSetup, XError> {
    let dst = state.picture(dst_id)?.clone();
    let drawable = match dst.source {
        PictureSource::Drawable(d) => d,
        // 渐变/Solid 作为绘制目标没有底层 drawable（规范禁止）
        PictureSource::Solid { .. }
        | PictureSource::LinearGradient { .. }
        | PictureSource::RadialGradient { .. }
        | PictureSource::ConicalGradient { .. } => return Err(XError::bad_match()),
    };
    let (width, height, _) = state.drawable_geometry(drawable)?;
    let src = state.picture(src_id)?.clone();
    let src_pixels = match &src.source {
        PictureSource::Drawable(d) => BorrowedPixels::take(state, *d, drawable),
        // Solid/渐变源在 sample_picture 里现算，不需要像素缓冲
        PictureSource::Solid { .. }
        | PictureSource::LinearGradient { .. }
        | PictureSource::RadialGradient { .. }
        | PictureSource::ConicalGradient { .. } => BorrowedPixels::empty(),
    };
    Ok(CompositeSetup {
        op,
        drawable,
        width: width as usize,
        height: height as usize,
        dst,
        src,
        src_pixels,
    })
}

/// Composite 的逐像素混合（带可选 mask，只取 mask 的 alpha 通道）。
#[allow(clippy::too_many_arguments)]
fn composite_pixels(
    state: &mut ServerState,
    setup: &CompositeSetup,
    mask: Option<(&PictureData, &BorrowedPixels)>,
    dst_x: i16,
    dst_y: i16,
    src_x: i16,
    src_y: i16,
    mask_x: i16,
    mask_y: i16,
    width: u16,
    height: u16,
) -> Result<(), XError> {
    let pixels = match state.windows.get_mut(&setup.drawable) {
        Some(window) => &mut window.pixels,
        None => &mut state.pixmaps.get_mut(&setup.drawable).expect("composite_setup 已确认存在").pixels,
    };
    for row in 0..i32::from(height) {
        for col in 0..i32::from(width) {
            let dx = i32::from(dst_x) + col;
            let dy = i32::from(dst_y) + row;
            // 目标越界与 picture 裁剪：跳过（与 Xorg 的 clip 语义一致）
            if dx < 0 || dy < 0 || dx >= setup.width as i32 || dy >= setup.height as i32 {
                continue;
            }
            if !clip_contains(&setup.dst, dx, dy) {
                continue;
            }
            let sx = i32::from(src_x) + col;
            let sy = i32::from(src_y) + row;
            let mut source = sample_picture(&setup.src, sx, sy, &setup.src_pixels);
            if let Some((mask_picture, mask_pixels)) = mask {
                let mx = i32::from(mask_x) + col;
                let my = i32::from(mask_y) + row;
                let mask_alpha = sample_picture(mask_picture, mx, my, mask_pixels).3;
                // 预乘语义：alpha 被掩码削减时 RGB 要同步削减
                source.0 = source.0 * mask_alpha / 255;
                source.1 = source.1 * mask_alpha / 255;
                source.2 = source.2 * mask_alpha / 255;
                source.3 = source.3 * mask_alpha / 255;
            }
            let index = dy as usize * setup.width + dx as usize;
            let Some(value) = blend_pixel(setup.op, pixels[index], source) else {
                return Err(XError::new(BAD_PICT_OP, setup.op as u32));
            };
            pixels[index] = value;
        }
    }
    Ok(())
}

// ---- 请求语义 ----

impl ServerState {
    pub fn picture(&self, id: u32) -> Result<&PictureData, XError> {
        self.pictures.get(&id).ok_or(XError::new(BAD_PICTURE, id))
    }

    /// 解析别名链后的 glyph set（ReferenceGlyphSet 的别名指向规范表）。
    fn glyph_set(&self, id: u32) -> Result<&GlyphSetData, XError> {
        match self.glyph_sets.get(&id) {
            Some(set) => Ok(set.canonical(&self.glyph_sets)),
            None => Err(XError::new(BAD_GLYPH_SET, id)),
        }
    }

    /// 写入 glyph set 的字形（解析别名后落到规范表）。
    fn glyph_set_mut(&mut self, id: u32) -> Result<&mut GlyphSetData, XError> {
        let canonical = self.glyph_set(id)?.id;
        self.glyph_sets
            .get_mut(&canonical)
            .ok_or(XError::new(BAD_GLYPH_SET, canonical))
    }

    // ---- QueryVersion / QueryPictFormats / QueryPictIndexValues / QueryFilters ----

    pub fn render_query_version(&mut self, client: u32, sequence: u16) -> Result<(), XError> {
        self.send_reply(
            client,
            &render::QueryVersionReply {
                sequence,
                length: 0,
                major_version: RENDER_MAJOR_VERSION,
                minor_version: RENDER_MINOR_VERSION,
            },
        );
        Ok(())
    }

    pub fn render_query_pict_formats(&mut self, client: u32, sequence: u16) -> Result<(), XError> {
        // 三种直接格式；screen 结构按 toolkit 的遍历顺序组装：
        // depth 24 → RGB24 绑定根 visual；depth 32 → ARGB32（visualid 0，
        // Xorg 也是这么报的：32 位格式没有对应的 visual）。
        let formats = vec![
            pict_form_info(FORMAT_ARGB32),
            pict_form_info(FORMAT_RGB24),
            pict_form_info(FORMAT_A8),
        ];
        let depths = vec![
            render::Pictdepth {
                depth: SCREEN_DEPTH,
                visuals: vec![render::Pictvisual {
                    visual: ROOT_VISUAL,
                    format: render::Pictformat::from(FORMAT_RGB24),
                }],
            },
            render::Pictdepth {
                depth: 32,
                visuals: vec![render::Pictvisual {
                    visual: 0,
                    format: render::Pictformat::from(FORMAT_ARGB32),
                }],
            },
        ];
        let reply = render::QueryPictFormatsReply {
            sequence,
            length: 0,
            num_depths: depths.len() as u32,
            num_visuals: depths.iter().flat_map(|d| d.visuals.iter()).count() as u32,
            formats,
            screens: vec![render::Pictscreen {
                fallback: render::Pictformat::from(FORMAT_RGB24),
                depths,
            }],
            subpixels: Vec::new(),
        };
        // 这份回复的 length 字段 x11rb 不会自动回填：序列化后手工改写
        // （变长数据按 4 字节单位计）。
        let mut bytes = reply.serialize();
        let length = (bytes.len().saturating_sub(32) / 4) as u32;
        bytes[4..8].copy_from_slice(&length.to_ne_bytes());
        self.send(client, &bytes);
        Ok(())
    }

    /// 本服务器只有 direct 格式：按规范对非 indexed 格式回 BadMatch。
    /// （协议上这个请求只对 Indexed 格式有意义，toolkit 调它时都在兜底路径。）
    pub fn render_query_pict_index_values(
        &mut self,
        _client: u32,
        _sequence: u16,
        request: &render::QueryPictIndexValuesRequest,
    ) -> Result<(), XError> {
        if pict_format_depth(u32::from(request.format)).is_none() {
            return Err(XError::new(BAD_PICT_FORMAT, u32::from(request.format)));
        }
        Err(XError::bad_match())
    }

    /// QueryFilters：没有真滤波，回空列表（cairo 拿它枚举，空列表即走默认滤波）。
    pub fn render_query_filters(&mut self, client: u32, sequence: u16) -> Result<(), XError> {
        self.send_reply(
            client,
            &render::QueryFiltersReply {
                sequence,
                length: 0,
                aliases: Vec::new(),
                filters: Vec::new(),
            },
        );
        Ok(())
    }

    // ---- Picture 生命周期 ----

    pub fn render_create_picture(
        &mut self,
        client: u32,
        request: &render::CreatePictureRequest<'_>,
    ) -> Result<(), XError> {
        self.check_new_id(client, u32::from(request.pid))?;
        let format = u32::from(request.format);
        if pict_format_depth(format).is_none() {
            return Err(XError::new(BAD_PICT_FORMAT, format));
        }
        // drawable 必须存在（窗口或 pixmap）
        self.drawable_geometry(u32::from(request.drawable))?;

        let aux = &request.value_list;
        // repeat 用数值比较（x11rb 生成的枚举没有 PartialEq；Repeat::NORMAL=1
        // 是"重复"，None/PAD/REFLECT 都按"不重复"处理）
        let repeat = aux
            .repeat
            .map(|r| u32::from(r) == u32::from(render::Repeat::NORMAL))
            .unwrap_or(false);
        self.pictures.insert(
            u32::from(request.pid),
            PictureData {
                id: u32::from(request.pid),
                owner: client,
                format,
                source: PictureSource::Drawable(u32::from(request.drawable)),
                repeat,
                clip: None,
                clip_x_origin: aux.clipxorigin.unwrap_or(0) as i16,
                clip_y_origin: aux.clipyorigin.unwrap_or(0) as i16,
                filter: String::new(),
            },
        );
        // 忽略的属性：alphamap / alphaxorigin / alphayorigin（独立 alpha 通道）、
        // clipmask（1 位 pixmap 裁剪，走 SetPictureClipRectangles 路径）、
        // graphicsexposure / subwindowmode / polyedge / polymode / dither /
        // componentalpha（采样细节，单位变换下不影响常见渲染）。
        Ok(())
    }

    pub fn render_change_picture(
        &mut self,
        request: &render::ChangePictureRequest<'_>,
    ) -> Result<(), XError> {
        let id = u32::from(request.picture);
        if !self.pictures.contains_key(&id) {
            return Err(XError::new(BAD_PICTURE, id));
        }
        let aux = &request.value_list;
        // 先在栈上算好再落表，避免在 HashMap 借用里再查表
        let repeat = aux.repeat.map(|r| u32::from(r) == u32::from(render::Repeat::NORMAL));
        let clip_x = aux.clipxorigin.map(|v| v as i16);
        let clip_y = aux.clipyorigin.map(|v| v as i16);
        let picture = self.pictures.get_mut(&id).expect("上面刚确认存在");
        if let Some(repeat) = repeat {
            picture.repeat = repeat;
        }
        if let Some(x) = clip_x {
            picture.clip_x_origin = x;
        }
        if let Some(y) = clip_y {
            picture.clip_y_origin = y;
        }
        // alphamap / clipmask / polyedge / polymode / dither / componentalpha：
        // 接受并忽略（超出核心子集，见文件头注释）
        Ok(())
    }

    pub fn render_set_picture_clip_rectangles(
        &mut self,
        request: &render::SetPictureClipRectanglesRequest<'_>,
    ) -> Result<(), XError> {
        let id = u32::from(request.picture);
        if !self.pictures.contains_key(&id) {
            return Err(XError::new(BAD_PICTURE, id));
        }
        let mut rects = Rects::from_rectangles(&request.rectangles);
        rects.translate(request.clip_x_origin, request.clip_y_origin);
        let picture = self.pictures.get_mut(&id).expect("上面刚确认存在");
        picture.clip = Some(rects);
        Ok(())
    }

    pub fn render_free_picture(&mut self, request: &render::FreePictureRequest) -> Result<(), XError> {
        let id = u32::from(request.picture);
        if !self.pictures.contains_key(&id) {
            return Err(XError::new(BAD_PICTURE, id));
        }
        // NyaTerm: X11 引用计数语义——被光标引用的 picture 不能删
        //（firefox 建 picture → CreateCursor → 立刻 FreePicture → 再建第二个
        // cursor 引用同一 picture）。客户端引用释放，光标引用保留。
        if self.cursors.values().any(|r| r.picture == Some(id)) {
            return Ok(());
        }
        self.pictures.remove(&id);
        self.mark_compose_dirty();
        Ok(())
    }

    pub fn render_set_picture_transform(
        &mut self,
        request: &render::SetPictureTransformRequest,
    ) -> Result<(), XError> {
        // 只支持单位变换：接受并忽略（注释：非单位矩阵会画错位置，见已知取舍）。
        self.picture(u32::from(request.picture)).map(|_| ())
    }

    pub fn render_set_picture_filter(
        &mut self,
        request: &render::SetPictureFilterRequest<'_>,
    ) -> Result<(), XError> {
        let id = u32::from(request.picture);
        if !self.pictures.contains_key(&id) {
            return Err(XError::new(BAD_PICTURE, id));
        }
        // Nearest/Best/Bilinear 等点采样滤波都接受（实现按最近邻采样）；
        // Convolution/Cubic 这类带参数的真滤波不做，回 BadMatch 让客户端走回退。
        let name = String::from_utf8_lossy(&request.filter).to_lowercase();
        match name.as_str() {
            "nearest" | "fast" | "good" | "best" | "bilinear" => {
                self.pictures.get_mut(&id).expect("上面刚确认存在").filter = name;
                Ok(())
            }
            _ => Err(XError::bad_match()),
        }
    }

    pub fn render_create_solid_fill(
        &mut self,
        client: u32,
        request: &render::CreateSolidFillRequest,
    ) -> Result<(), XError> {
        self.check_new_id(client, u32::from(request.picture))?;
        let color = request.color;
        self.pictures.insert(
            u32::from(request.picture),
            PictureData {
                id: u32::from(request.picture),
                owner: client,
                format: FORMAT_ARGB32,
                source: PictureSource::Solid {
                    red: color.red,
                    green: color.green,
                    blue: color.blue,
                    alpha: color.alpha,
                },
                repeat: false,
                clip: None,
                clip_x_origin: 0,
                clip_y_origin: 0,
                filter: String::new(),
            },
        );
        Ok(())
    }

    /// CreateLinearGradient / Radial / Conical 共用的落地逻辑：解析停靠点并
    /// 插入 picture（渐变源不含像素，采样时按几何参数现算颜色）。
    fn insert_gradient_picture(
        &mut self,
        client: u32,
        picture_id: u32,
        source: PictureSource,
    ) -> Result<(), XError> {
        self.check_new_id(client, picture_id)?;
        self.pictures.insert(
            picture_id,
            PictureData {
                id: picture_id,
                owner: client,
                format: FORMAT_ARGB32,
                source,
                repeat: false,
                clip: None,
                clip_x_origin: 0,
                clip_y_origin: 0,
                filter: String::new(),
            },
        );
        Ok(())
    }

    /// 停靠点解析：Fixed(16.16) offset + 未预乘 16 位颜色。
    fn collect_gradient_stops(
        &self,
        stops: &[i32],
        colors: &[render::Color],
    ) -> Result<Vec<crate::xsrv::state::GradientStop>, XError> {
        if stops.len() != colors.len() || stops.is_empty() {
            return Err(XError::bad_value(stops.len() as u32));
        }
        Ok(stops
            .iter()
            .zip(colors.iter())
            .map(|(offset, color)| crate::xsrv::state::GradientStop {
                offset: *offset as f64 / 65536.0,
                red: color.red,
                green: color.green,
                blue: color.blue,
                alpha: color.alpha,
            })
            .collect())
    }

    pub fn render_create_linear_gradient(
        &mut self,
        client: u32,
        request: &render::CreateLinearGradientRequest,
    ) -> Result<(), XError> {
        let stops = self.collect_gradient_stops(&request.stops, &request.colors)?;
        let source = PictureSource::LinearGradient {
            x1: f64::from(request.p1.x),
            y1: f64::from(request.p1.y),
            x2: f64::from(request.p2.x),
            y2: f64::from(request.p2.y),
            stops,
        };
        self.insert_gradient_picture(client, u32::from(request.picture), source)
    }

    pub fn render_create_radial_gradient(
        &mut self,
        client: u32,
        request: &render::CreateRadialGradientRequest,
    ) -> Result<(), XError> {
        let stops = self.collect_gradient_stops(&request.stops, &request.colors)?;
        // Fixed(16.16) 半径换算成像素
        let source = PictureSource::RadialGradient {
            x1: f64::from(request.inner.x),
            y1: f64::from(request.inner.y),
            r1: f64::from(request.inner_radius) / 65536.0,
            x2: f64::from(request.outer.x),
            y2: f64::from(request.outer.y),
            r2: f64::from(request.outer_radius) / 65536.0,
            stops,
        };
        self.insert_gradient_picture(client, u32::from(request.picture), source)
    }

    pub fn render_create_conical_gradient(
        &mut self,
        client: u32,
        request: &render::CreateConicalGradientRequest,
    ) -> Result<(), XError> {
        let stops = self.collect_gradient_stops(&request.stops, &request.colors)?;
        let source = PictureSource::ConicalGradient {
            x: f64::from(request.center.x),
            y: f64::from(request.center.y),
            angle: f64::from(request.angle) / 65536.0,
            stops,
        };
        self.insert_gradient_picture(client, u32::from(request.picture), source)
    }

    // ---- FillRectangles ----

    pub fn render_fill_rectangles(
        &mut self,
        request: &render::FillRectanglesRequest<'_>,
    ) -> Result<(), XError> {
        let dst_id = u32::from(request.dst);
        let dst = self.picture(dst_id)?.clone();
        let drawable = match dst.source {
            PictureSource::LinearGradient { .. }
            | PictureSource::RadialGradient { .. }
            | PictureSource::ConicalGradient { .. } => return Err(XError::bad_match()),
            PictureSource::Drawable(d) => d,
            PictureSource::Solid { .. } => return Err(XError::bad_match()),
        };
        let (width, height, _) = self.drawable_geometry(drawable)?;
        let src = premultiply(request.color);

        // NyaTerm 探针：FillRectangles 的落点（插入符/下划线常走这条）。
        // 与 [glyph] 的 base 坐标对照即可判断错位在哪一侧。
        if crate::xsrv::counters::GLYPH_LOGS
            .load(std::sync::atomic::Ordering::Relaxed)
            < 800
        {
            crate::xsrv::counters::GLYPH_LOGS
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let head: Vec<(i16, i16, u16, u16)> = request
                .rects
                .iter()
                .take(4)
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect();
            crate::xsrv::log_line(format_args!(
                "[fill] dst={drawable:#x} color=({},{},{},{}) rects={head:?} n={}",
                request.color.red,
                request.color.green,
                request.color.blue,
                request.color.alpha,
                request.rects.len()
            ));
        }

        // NyaTerm 快路径：填充色实测绝大多数是**完全不透明**（alpha=65535）或
        // 完全透明（alpha=0）。不透明色在 Src/Over/Atop/In 下结果与 dst 无关，
        // 可整块直接写色；透明色在 Over/Atop 下结果就是 dst，整块跳过。
        // 这两条把 FillRectangles（实测请求数第一）从"逐像素 blend"降为"逐像素写"。
        let op_u = u8::from(request.op);
        let (clear, src_op, over, dst_op, in_op, atop, atop_rev) = (
            u8::from(render::PictOp::CLEAR),
            u8::from(render::PictOp::SRC),
            u8::from(render::PictOp::OVER),
            u8::from(render::PictOp::DST),
            u8::from(render::PictOp::IN),
            u8::from(render::PictOp::ATOP),
            u8::from(render::PictOp::ATOP_REVERSE),
        );
        let no_op =
            op_u == dst_op || op_u == atop_rev || (src.3 == 0 && (op_u == over || op_u == atop));
        let fill: Option<u32> = if no_op {
            None
        } else if op_u == clear {
            Some(0)
        } else if src.3 == 255 && (op_u == src_op || op_u == over || op_u == atop || op_u == in_op)
        {
            Some(pixel_of(src.0, src.1, src.2))
        } else {
            None
        };

        if no_op {
            // 结果就是原样，不必标记重绘
            return Ok(());
        }

        {
            let pixels = match self.windows.get_mut(&drawable) {
                Some(window) => &mut window.pixels,
                None => &mut self.pixmaps.get_mut(&drawable).expect("上面已确认存在").pixels,
            };
            for rect in request.rects.iter() {
                let left = i32::from(rect.x).max(0);
                let top = i32::from(rect.y).max(0);
                let right = (i32::from(rect.x) + i32::from(rect.width)).min(i32::from(width));
                let bottom = (i32::from(rect.y) + i32::from(rect.height)).min(i32::from(height));
                if let Some(color) = fill {
                    for y in top..bottom {
                        let row = y as usize * width as usize;
                        for x in left..right {
                            if !clip_contains(&dst, x, y) {
                                continue;
                            }
                            pixels[row + x as usize] = color;
                        }
                    }
                    continue;
                }
                for y in top..bottom {
                    for x in left..right {
                        if !clip_contains(&dst, x, y) {
                            continue;
                        }
                        let index = y as usize * width as usize + x as usize;
                        let Some(value) = blend_pixel(u8::from(request.op), pixels[index], src)
                        else {
                            return Err(XError::new(BAD_PICT_OP, u32::from(request.op)));
                        };
                        pixels[index] = value;
                    }
                }
            }
        }

        finish_draw(self, drawable);
        Ok(())
    }

    // ---- Composite ----

    pub fn render_composite(&mut self, request: &render::CompositeRequest) -> Result<(), XError> {
        let dst_id = u32::from(request.dst);
        let setup = composite_setup(self, u8::from(request.op), u32::from(request.src), dst_id)?;
        // mask：None（x11rb 的 Picture::NONE = 0）或一个 picture（只取 alpha 通道）
        let (mask_picture, mut mask_pixels) = match u32::from(request.mask) {
            0 => (None, None),
            id => {
                let picture = self.picture(id)?.clone();
                let pixels = match &picture.source {
                    // 渐变源的像素在采样时现算，不会走到像素缓冲分支
                    PictureSource::Drawable(d) => BorrowedPixels::take(self, *d, setup.drawable),
                    PictureSource::Solid { .. }
                    | PictureSource::LinearGradient { .. }
                    | PictureSource::RadialGradient { .. }
                    | PictureSource::ConicalGradient { .. } => BorrowedPixels::empty(),
                };
                (Some(picture), Some(pixels))
            }
        };
        let mask = match (&mask_picture, &mask_pixels) {
            (Some(p), Some(b)) => Some((p, b)),
            _ => None,
        };
        composite_pixels(
            self,
            &setup,
            mask,
            request.dst_x,
            request.dst_y,
            request.src_x,
            request.src_y,
            request.mask_x,
            request.mask_y,
            request.width,
            request.height,
        )?;
        setup.src_pixels.restore(self);
        if let Some(pixels) = mask_pixels.take() {
            pixels.restore(self);
        }
        finish_draw(self, setup.drawable);
        Ok(())
    }

    // ---- GlyphSet / Glyphs ----

    pub fn render_create_glyph_set(
        &mut self,
        client: u32,
        request: &render::CreateGlyphSetRequest,
    ) -> Result<(), XError> {
        self.check_new_id(client, u32::from(request.gsid))?;
        let format = u32::from(request.format);
        // 字形位图只支持 A8 与 ARGB32（A1 不做，见已知取舍）
        if format != FORMAT_A8 && format != FORMAT_ARGB32 {
            return Err(XError::bad_match());
        }
        self.glyph_sets.insert(
            u32::from(request.gsid),
            GlyphSetData {
                id: u32::from(request.gsid),
                format,
                glyphs: HashMap::new(),
                alias: None,
                owner: client,
            },
        );
        Ok(())
    }

    pub fn render_reference_glyph_set(
        &mut self,
        client: u32,
        request: &render::ReferenceGlyphSetRequest,
    ) -> Result<(), XError> {
        self.check_new_id(client, u32::from(request.gsid))?;
        let existing = self.glyph_set(u32::from(request.existing))?.clone();
        self.glyph_sets.insert(
            u32::from(request.gsid),
            GlyphSetData {
                id: u32::from(request.gsid),
                format: existing.format,
                glyphs: HashMap::new(),
                alias: Some(existing.id),
                owner: client,
            },
        );
        Ok(())
    }

    pub fn render_free_glyph_set(
        &mut self,
        request: &render::FreeGlyphSetRequest,
    ) -> Result<(), XError> {
        let id = u32::from(request.glyphset);
        if self.glyph_sets.remove(&id).is_none() {
            return Err(XError::new(BAD_GLYPH_SET, id));
        }
        Ok(())
    }

    pub fn render_add_glyphs(&mut self, request: &render::AddGlyphsRequest<'_>) -> Result<(), XError> {
        if request.glyphids.len() != request.glyphs.len() {
            return Err(XError::bad_length());
        }
        let format = self.glyph_set(u32::from(request.glyphset))?.format;
        let bytes_per_pixel: usize = match format {
            FORMAT_A8 => 1,
            FORMAT_ARGB32 => 4,
            _ => return Err(XError::bad_match()),
        };
        // 逐个字形切出位图：每行按 4 字节对齐（协议规定 Z-format pad）
        let mut offset = 0usize;
        for (index, info) in request.glyphs.iter().enumerate() {
            let width = info.width as usize;
            let height = info.height as usize;
            let stride = (width * bytes_per_pixel).div_ceil(4) * 4;
            let needed = stride.checked_mul(height).ok_or(XError::bad_length())?;
            if offset + needed > request.data.len() {
                return Err(XError::bad_length());
            }
            let bits = request.data[offset..offset + needed].to_vec();
            offset += needed;
            let glyph = Glyph {
                width: info.width,
                height: info.height,
                x: info.x,
                y: info.y,
                x_off: info.x_off,
                y_off: info.y_off,
                bits,
                bytes_per_pixel: bytes_per_pixel as u8,
            };
            let glyph_id = request.glyphids[index];
            self.glyph_set_mut(u32::from(request.glyphset))?
                .glyphs
                .insert(glyph_id, glyph);
        }
        Ok(())
    }

    pub fn render_free_glyphs(&mut self, request: &render::FreeGlyphsRequest<'_>) -> Result<(), XError> {
        // 规范要求"每个 glyph 都必须存在"，这里放宽为忽略缺失（cairo 的 glyph
        // 缓存释放时机客户端未必有序；重复 Free 也不该把服务端弄崩）
        for glyph in &*request.glyphs {
            if let Ok(set) = self.glyph_set_mut(u32::from(request.glyphset)) {
                set.glyphs.remove(&u32::from(*glyph));
            }
        }
        Ok(())
    }

    /// Composite(Glyphs)：把 glyphcmds 里的字形按笔画位置混合到目标上。
    ///
    /// 8/16/32 位三个请求字段同名，先拆成原语再走同一实现（`img` 是位宽）。
    pub fn render_composite_glyphs8(
        &mut self,
        request: &render::CompositeGlyphs8Request<'_>,
    ) -> Result<(), XError> {
        self.composite_glyphs_impl(
            u8::from(request.op),
            u32::from(request.src),
            u32::from(request.dst),
            u32::from(request.glyphset),
            i16::from(request.src_x),
            i16::from(request.src_y),
            &request.glyphcmds,
            8,
        )
    }

    pub fn render_composite_glyphs16(
        &mut self,
        request: &render::CompositeGlyphs16Request<'_>,
    ) -> Result<(), XError> {
        self.composite_glyphs_impl(
            u8::from(request.op),
            u32::from(request.src),
            u32::from(request.dst),
            u32::from(request.glyphset),
            i16::from(request.src_x),
            i16::from(request.src_y),
            // x11rb 把 glyphcmds 存成 [u16]，还原成原生字节序的字节流统一解析
            &request
                .glyphcmds
                .iter()
                .flat_map(|v| v.to_ne_bytes())
                .collect::<Vec<u8>>(),
            16,
        )
    }

    pub fn render_composite_glyphs32(
        &mut self,
        request: &render::CompositeGlyphs32Request<'_>,
    ) -> Result<(), XError> {
        self.composite_glyphs_impl(
            u8::from(request.op),
            u32::from(request.src),
            u32::from(request.dst),
            u32::from(request.glyphset),
            i16::from(request.src_x),
            i16::from(request.src_y),
            &request
                .glyphcmds
                .iter()
                .flat_map(|v| v.to_ne_bytes())
                .collect::<Vec<u8>>(),
            32,
        )
    }

    /// Glyphs 的实现核心：先解析命令流并取出字形拷贝（要在拿目标可变借用
    /// 之前做完所有只读访问），再逐字形逐像素混合。
    ///
    /// 协议上这个请求没有 dst_x/dst_y：初始笔画在 (0,0)，位置完全由
    /// glyphcmds 元素头的 deltax/deltay 提供（Xorg `miGlyphs` 的语义）。
    #[allow(clippy::too_many_arguments)]
    fn composite_glyphs_impl(
        &mut self,
        op: u8,
        src_id: u32,
        dst_id: u32,
        glyphset: u32,
        src_x: i16,
        src_y: i16,
        glyphcmds: &[u8],
        img: u8,
    ) -> Result<(), XError> {
        // 1. 解析命令流：取出字形拷贝并按协议语义计算落点
        //    （元素先平移笔画，字形画在 pen - (glyph.x, glyph.y)，画完按
        //    (x_off, y_off) 推进）。要在拿目标可变借用之前做完所有只读访问。
        let mut items: Vec<(Glyph, i32, i32)> = Vec::new();
        {
            let mut current_set = glyphset;
            let mut pen_x = 0i32;
            let mut pen_y = 0i32;
            for command in glyph_commands(glyphcmds, img)? {
                match command {
                    GlyphCommand::Position { dx, dy } => {
                        pen_x += dx;
                        pen_y += dy;
                    }
                    GlyphCommand::SwitchGlyphSet(id) => {
                        // 切换 glyphset：校验存在，后续字形从新表里取
                        self.glyph_set(id)?;
                        current_set = id;
                    }
                    GlyphCommand::Glyph(glyph_id) => {
                        let glyph = self
                            .glyph_set(current_set)?
                            .glyphs
                            .get(&glyph_id)
                            .ok_or(XError::new(BAD_GLYPH, glyph_id))?
                            .clone();
                        let base_x = pen_x - i32::from(glyph.x);
                        let base_y = pen_y - i32::from(glyph.y);
                        // NyaTerm 探针：字形定位归因（中文插入符错位）。
                        // 打印每个字形的度量与实际落点，和 [fill] 的光标矩形
                        // 坐标对照就能判断是我们画歪了还是光标画歪了。
                        if crate::xsrv::counters::GLYPH_LOGS
                            .load(std::sync::atomic::Ordering::Relaxed)
                            < 800
                        {
                            crate::xsrv::counters::GLYPH_LOGS
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            crate::xsrv::log_line(format_args!(
                                "[glyph] img={img} set={current_set:#x} id={glyph_id} w={} h={} gx={} gy={} xoff={} yoff={} pen=({pen_x},{pen_y}) base=({base_x},{base_y})",
                                glyph.width, glyph.height, glyph.x, glyph.y, glyph.x_off, glyph.y_off
                            ));
                        }
                        pen_x += i32::from(glyph.x_off);
                        pen_y += i32::from(glyph.y_off);
                        items.push((glyph, base_x, base_y));
                    }
                }
            }
        }

        // 2. 组装混合上下文（借走源像素）
        let setup = composite_setup(self, op, src_id, dst_id)?;

        // 3. 逐字形逐像素混合。A8 字形只提供 alpha（颜色取源 picture）；
        //    ARGB32 字形自带预乘色（与源无关，独立字形的语义）。
        {
            let pixels = match self.windows.get_mut(&setup.drawable) {
                Some(window) => &mut window.pixels,
                None => &mut self.pixmaps.get_mut(&setup.drawable).expect("composite_setup 已确认存在").pixels,
            };
            for (glyph, base_x, base_y) in &items {
                let stride = (glyph.width as usize * glyph.bytes_per_pixel as usize).div_ceil(4) * 4;
                for row in 0..glyph.height as usize {
                    let dy = base_y + row as i32;
                    if dy < 0 || dy >= setup.height as i32 {
                        continue;
                    }
                    for col in 0..glyph.width as usize {
                        let dx = base_x + col as i32;
                        if dx < 0 || dx >= setup.width as i32 || !clip_contains(&setup.dst, dx, dy)
                        {
                            continue;
                        }
                        let offset = row * stride + col * glyph.bytes_per_pixel as usize;
                        let source = if glyph.bytes_per_pixel == 1 {
                            // A8：字形 alpha × 源色（源采样按 src_x/src_y 平移）
                            let sx = dx + i32::from(src_x);
                            let sy = dy + i32::from(src_y);
                            let mut s = sample_picture(&setup.src, sx, sy, &setup.src_pixels);
                            let cell = *glyph.bits.get(offset).unwrap_or(&0) as u32;
                            // 预乘语义：字形 alpha 削减时 RGB 同步削减
                            s.0 = s.0 * cell / 255;
                            s.1 = s.1 * cell / 255;
                            s.2 = s.2 * cell / 255;
                            s.3 = s.3 * cell / 255;
                            s
                        } else {
                            // ARGB32：行内 4 字节 BGRA 字节序（预乘）
                            let Some(bgra) = glyph.bits.get(offset..offset + 4) else {
                                continue;
                            };
                            (
                                bgra[2] as u32,
                                bgra[1] as u32,
                                bgra[0] as u32,
                                bgra[3] as u32,
                            )
                        };
                        let index = dy as usize * setup.width + dx as usize;
                        let Some(value) = blend_pixel(setup.op, pixels[index], source) else {
                            return Err(XError::new(BAD_PICT_OP, setup.op as u32));
                        };
                        pixels[index] = value;
                    }
                }
            }
        }

        finish_draw(self, setup.drawable);
        Ok(())
    }

    // ---- Trapezoids / Triangles / TriStrip / TriFan ----

    pub fn render_trapezoids(&mut self, request: &render::TrapezoidsRequest<'_>) -> Result<(), XError> {
        // 每个梯形按上下边之间逐行扫描：左右边（Linefix）在扫描线 y+0.5 处
        // 线性插值出 x（Fixed 16.16 → f64）。
        let spans: Vec<Span> = request.traps.iter().flat_map(trapezoid_spans).collect();
        self.blend_spans(
            u8::from(request.op),
            u32::from(request.src),
            u32::from(request.dst),
            i16::from(request.src_x),
            i16::from(request.src_y),
            &spans,
        )
    }

    pub fn render_triangles(&mut self, request: &render::TrianglesRequest<'_>) -> Result<(), XError> {
        // 三个顶点直接走 raster.rs 的多边形扫描（坐标 Fixed 16.16 → 像素取整）
        let spans: Vec<Span> = request
            .triangles
            .iter()
            .flat_map(|tri| triangle_spans(tri, i32::from(request.src_x), i32::from(request.src_y)))
            .collect();
        self.blend_spans(
            u8::from(request.op),
            u32::from(request.src),
            u32::from(request.dst),
            i16::from(request.src_x),
            i16::from(request.src_y),
            &spans,
        )
    }

    pub fn render_tri_strip(&mut self, request: &render::TriStripRequest<'_>) -> Result<(), XError> {
        // strip：相邻三个顶点构成一个三角形（i, i+1, i+2）
        let points = strip_points(&request.points);
        let offsets = 0..points.len().saturating_sub(2);
        self.blend_triangle_list(
            request.op,
            u32::from(request.src),
            u32::from(request.dst),
            i16::from(request.src_x),
            i16::from(request.src_y),
            &points,
            offsets.map(|i| (i, i + 1, i + 2)),
        )
    }

    pub fn render_tri_fan(&mut self, request: &render::TriFanRequest<'_>) -> Result<(), XError> {
        // fan：全部三角形共享第一个顶点（0, i, i+1）
        let points = strip_points(&request.points);
        let offsets = 1..points.len().saturating_sub(1);
        self.blend_triangle_list(
            request.op,
            u32::from(request.src),
            u32::from(request.dst),
            i16::from(request.src_x),
            i16::from(request.src_y),
            &points,
            offsets.map(|i| (0, i, i + 1)),
        )
    }

    /// 把 span 栅格化结果按 op 混合到目标 picture 上（形状类的公共尾部）。
    fn blend_spans(
        &mut self,
        op: u8,
        src_id: u32,
        dst_id: u32,
        src_x: i16,
        src_y: i16,
        spans: &[Span],
    ) -> Result<(), XError> {
        let setup = composite_setup(self, op, src_id, dst_id)?;
        // NyaTerm 快路径：文字/图形填充的源通常是**纯色 picture**，采样结果与
        // 坐标无关。原来逐像素调 sample_picture + blend_pixel，在整屏文字下是
        // 最大的一笔开销（实测 Trapezoids 每秒上百次）。这里先做三件事：
        //   ① 结果为 dst 的 op（DST / AtopReverse）→ 整段跳过；
        //   ② 结果为纯色的情形（Clear、不透明源 + Src/Over/Atop/In）→ 整段填色；
        //   ③ 其余情形至少把纯色源的采样提到循环外。
        let op_u = setup.op;
        let (clear, src_op, over, dst_op, in_op, atop, atop_rev) = (
            u8::from(render::PictOp::CLEAR),
            u8::from(render::PictOp::SRC),
            u8::from(render::PictOp::OVER),
            u8::from(render::PictOp::DST),
            u8::from(render::PictOp::IN),
            u8::from(render::PictOp::ATOP),
            u8::from(render::PictOp::ATOP_REVERSE),
        );
        let solid = if matches!(setup.src.source, PictureSource::Solid { .. }) {
            Some(sample_picture(&setup.src, 0, 0, &setup.src_pixels))
        } else {
            None
        };
        let no_op = op_u == dst_op
            || op_u == atop_rev
            || (solid.is_some_and(|s| s.3 == 0) && (op_u == over || op_u == atop));
        let fill: Option<u32> = if no_op {
            None
        } else if op_u == clear {
            Some(0)
        } else if let Some(s) = solid {
            if s.3 == 255 && (op_u == src_op || op_u == over || op_u == atop || op_u == in_op) {
                Some(pixel_of(s.0, s.1, s.2))
            } else {
                None
            }
        } else {
            None
        };

        if no_op {
            // 结果就是原样，不必标记重绘；但**必须归还借走的源像素**
            setup.src_pixels.restore(self);
            return Ok(());
        }

        // 绘制体放进块里，让 `pixels` 的可变借用先结束，之后才能归还 SourcePixels。
        let drawn: Result<(), XError> = {
            let pixels = match self.windows.get_mut(&setup.drawable) {
                Some(window) => &mut window.pixels,
                None => &mut self.pixmaps.get_mut(&setup.drawable).expect("composite_setup 已确认存在").pixels,
            };
            let mut bad_op: Option<u8> = None;
            if let Some(color) = fill {
                for span in spans {
                    if span.y < 0 || span.y >= setup.height as i32 {
                        continue;
                    }
                    let row = span.y as usize * setup.width;
                    for x in span.x0..=span.x1 {
                        if x < 0 || x >= setup.width as i32 || !clip_contains(&setup.dst, x, span.y) {
                            continue;
                        }
                        pixels[row + x as usize] = color;
                    }
                }
            } else {
                for span in spans {
                    if span.y < 0 || span.y >= setup.height as i32 {
                        continue;
                    }
                    for x in span.x0..=span.x1 {
                        if x < 0 || x >= setup.width as i32 || !clip_contains(&setup.dst, x, span.y) {
                            continue;
                        }
                        // 源采样坐标 = 形状坐标 + (src_x, src_y) 平移（形状默认
                        // 画在 dst 原点，与 Composite 的偏移约定一致）
                        let source = match solid {
                            Some(s) => s,
                            None => sample_picture(
                                &setup.src,
                                x + i32::from(src_x),
                                span.y + i32::from(src_y),
                                &setup.src_pixels,
                            ),
                        };
                        let index = span.y as usize * setup.width + x as usize;
                        let Some(value) = blend_pixel(setup.op, pixels[index], source) else {
                            bad_op = Some(setup.op);
                            break;
                        };
                        pixels[index] = value;
                    }
                    if bad_op.is_some() {
                        break;
                    }
                }
            }
            match bad_op {
                Some(op) => Err(XError::new(BAD_PICT_OP, u32::from(op))),
                None => Ok(()),
            }
        };

        // NyaTerm: **无论成败都必须归还**借走的源像素。`BorrowedPixels` 是显式
        // 归还（没有 Drop 实现），此前的实现漏了这一步：任何"以某个 drawable 作
        // 为源"的 Trapezoids/Triangles 都会把那块 drawable 的像素缓冲用
        // `mem::take` 抽空且不还，之后 compose() 读到空缓冲 → 画面冻结、或
        // 线程 panic 毒化状态锁导致整个界面再也画不出来（用户实测：滚动一下
        // 就卡死）。
        setup.src_pixels.restore(self);
        drawn?;
        finish_draw(self, setup.drawable);
        Ok(())
    }

    /// 三角形列表（TriStrip/TriFan）的公共尾部：顶点 + 三元组下标 → span → 混合。
    fn blend_triangle_list(
        &mut self,
        op: render::PictOp,
        src_id: u32,
        dst_id: u32,
        src_x: i16,
        src_y: i16,
        points: &[(i32, i32)],
        triples: impl Iterator<Item = (usize, usize, usize)>,
    ) -> Result<(), XError> {
        let spans: Vec<Span> = triples
            .filter_map(|(a, b, c)| {
                let triangle = render::Triangle {
                    p1: pointfix_of(points[a]),
                    p2: pointfix_of(points[b]),
                    p3: pointfix_of(points[c]),
                };
                let spans = triangle_spans(&triangle, 0, 0);
                (!spans.is_empty()).then_some(spans)
            })
            .flatten()
            .collect();
        self.blend_spans(u8::from(op), src_id, dst_id, src_x, src_y, &spans)
    }

    // ---- Gradients：明确拒绝 ----

    /// Linear/Radial/Conical 渐变未实现：回 BadImplementation，toolkit
    /// （cairo/Qt）会退回到普通 picture + 手工路径。
    pub fn render_gradient_rejected(&mut self) -> Result<(), XError> {
        Err(XError::bad_implementation())
    }
}

// ---- 几何辅助 ----

/// (x, y) 像素坐标 → Pointfix（16.16 定点）。
fn pointfix_of(p: (i32, i32)) -> render::Pointfix {
    render::Pointfix { x: p.0 * 65536, y: p.1 * 65536 }
}

/// Fixed 16.16 → 像素坐标（四舍五入）。
fn fixed_to_px(v: i32) -> i32 {
    (v as f64 / 65536.0 + 0.5).floor() as i32
}

/// Linefix 边在扫描线 `y` 处的 x 坐标（f64 像素；水平边退化为 p1.x）。
fn edge_x_at(edge: &render::Linefix, y: f64) -> f64 {
    let (x1, y1) = (edge.p1.x as f64 / 65536.0, edge.p1.y as f64 / 65536.0);
    let (x2, y2) = (edge.p2.x as f64 / 65536.0, edge.p2.y as f64 / 65536.0);
    if (y2 - y1).abs() < f64::EPSILON {
        return x1;
    }
    x1 + (y - y1) * (x2 - x1) / (y2 - y1)
}

/// 一个梯形的逐行 span（上下边之间，左右边插值）。
fn trapezoid_spans(trap: &render::Trapezoid) -> Vec<Span> {
    let top = trap.top as f64 / 65536.0;
    let bottom = trap.bottom as f64 / 65536.0;
    if bottom <= top {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let first = top.ceil() as i32;
    let last = (bottom - f64::EPSILON).ceil() as i32;
    for y in first..last {
        let scan = y as f64 + 0.5;
        if scan < top || scan >= bottom {
            continue;
        }
        let xl = edge_x_at(&trap.left, scan);
        let xr = edge_x_at(&trap.right, scan);
        // 像素中心落在 [xl, xr) 内算覆盖（与 raster.rs 的约定一致）
        let x0 = (xl - 0.5).ceil() as i32;
        let x1 = (xr - 0.5).ceil() as i32 - 1;
        if x1 >= x0 {
            spans.push(Span { y, x0, x1 });
        }
    }
    spans
}

/// 一个三角形的逐行 span（raster.rs 的多边形扫描；偏移由调用方加进顶点）。
fn triangle_spans(triangle: &render::Triangle, offset_x: i32, offset_y: i32) -> Vec<Span> {
    let points = [
        (fixed_to_px(triangle.p1.x) + offset_x, fixed_to_px(triangle.p1.y) + offset_y),
        (fixed_to_px(triangle.p2.x) + offset_x, fixed_to_px(triangle.p2.y) + offset_y),
        (fixed_to_px(triangle.p3.x) + offset_x, fixed_to_px(triangle.p3.y) + offset_y),
    ];
    raster::polygon_spans(&points, xproto::PolyShape::CONVEX)
}

/// TriStrip/TriFan 的顶点列表：Pointfix 数组 → 像素坐标。
fn strip_points(points: &[render::Pointfix]) -> Vec<(i32, i32)> {
    points
        .iter()
        .map(|p| (fixed_to_px(p.x), fixed_to_px(p.y)))
        .collect()
}

// ---- glyphcmds 命令流解析 ----
//
// 与 Xorg（render/render.c 的 ProcRenderCompositeGlyphs）一致的线格式：
// 每个元素以 8 字节的 xGlyphElt 头开始（len: CARD32, deltax: INT16, deltay: INT16）：
// - len == 0xff：glyphset 切换，头后跟 4 字节 GlyphSet ID（共 12 字节）；
// - len == n：笔画先平移 (deltax, deltay)，然后是 n 个紧密排列的字形索引
//   （8/16/32 位请求各占 1/2/4 字节），整个字形列表补齐到 4 字节边界；
// - len == 0：只有位置增量（文字排版里用它做定位）。

/// glyphcmds 里的一个命令。
#[derive(Debug, Clone, PartialEq, Eq)]
enum GlyphCommand {
    /// 笔画平移（含 len=0 的纯定位元素）。
    Position { dx: i32, dy: i32 },
    /// glyphset 切换（len == 0xff）。
    SwitchGlyphSet(u32),
    /// 画一个字形。
    Glyph(u32),
}

/// 解析 glyphcmds。流被截断时回 BadLength。
fn glyph_commands(cmds: &[u8], img: u8) -> Result<Vec<GlyphCommand>, XError> {
    let size = match img {
        8 => 1usize,
        16 => 2,
        _ => 4,
    };
    let mut result = Vec::new();
    let mut offset = 0usize;
    while offset + 8 <= cmds.len() {
        let len = u32::from_ne_bytes(cmds[offset..offset + 4].try_into().unwrap());
        let dx = i16::from_ne_bytes(cmds[offset + 4..offset + 6].try_into().unwrap()) as i32;
        let dy = i16::from_ne_bytes(cmds[offset + 6..offset + 8].try_into().unwrap()) as i32;
        offset += 8;
        if len == 0xff {
            // glyphset 切换：4 字节 GlyphSet ID
            let Some(bytes) = cmds.get(offset..offset + 4) else {
                return Err(XError::bad_length());
            };
            let id = u32::from_ne_bytes(bytes.try_into().unwrap());
            result.push(GlyphCommand::SwitchGlyphSet(id));
            offset += 4;
            continue;
        }
        result.push(GlyphCommand::Position { dx, dy });
        let glyphs_bytes = size.checked_mul(len as usize).ok_or(XError::bad_length())?;
        let padded = glyphs_bytes.div_ceil(4) * 4;
        let Some(list) = cmds.get(offset..offset + padded) else {
            return Err(XError::bad_length());
        };
        for index in 0..len as usize {
            let start = index * size;
            let id = match size {
                1 => u32::from(list[start]),
                2 => u32::from(u16::from_ne_bytes(list[start..start + 2].try_into().unwrap())),
                _ => u32::from_ne_bytes(list[start..start + 4].try_into().unwrap()),
            };
            result.push(GlyphCommand::Glyph(id));
        }
        offset += padded;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;
    use x11rb_protocol::protocol::Request;
    use x11rb_protocol::x11_utils::TryParse;

    /// 起一个干净的状态 + 客户端通道（与 extensions.rs 的 fixture 同一套）。
    fn fixture() -> (ServerState, u32, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
        let mut state = ServerState::new(Default::default());
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, sender);
        (state, 0, receiver)
    }

    fn ok(state: &mut ServerState, client: u32, sequence: u16, request: Request<'static>) {
        state
            .handle(client, sequence, &request)
            .unwrap_or_else(|e| panic!("请求 {request:?} 失败：{e:?}"));
    }

    /// 建一个 16x16 的窗口并映射（客户区间里第一个 ID 是窗口，后续 ID 顺延）。
    fn make_window(state: &mut ServerState, client: u32, sequence: u16, id: u32) {
        ok(state, client, sequence, Request::CreateWindow(xproto::CreateWindowRequest {
            depth: 0,
            wid: id,
            parent: super::super::handshake::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 16,
            height: 16,
            border_width: 0,
            class: xproto::WindowClass::INPUT_OUTPUT,
            visual: 0,
            value_list: Cow::default(),
        }));
        ok(state, client, sequence + 1, Request::MapWindow(xproto::MapWindowRequest { window: id }));
    }

    fn color(r: u16, g: u16, b: u16, a: u16) -> render::Color {
        render::Color { red: r, green: g, blue: b, alpha: a }
    }

    #[test]
    fn query_pict_formats_lists_argb32() {
        let (mut state, client, mut rx) = fixture();
        ok(&mut state, client, 1, Request::RenderQueryVersion(render::QueryVersionRequest {
            client_major_version: 0,
            client_minor_version: 11,
        }));
        let frame = rx.try_recv().expect("版本回复");
        let (reply, _) = render::QueryVersionReply::try_parse(&frame).expect("解析回复");
        assert_eq!((reply.major_version, reply.minor_version), (0, 11));

        ok(&mut state, client, 2, Request::RenderQueryPictFormats(render::QueryPictFormatsRequest));
        let frame = rx.try_recv().expect("格式回复");
        let (reply, _) = render::QueryPictFormatsReply::try_parse(&frame).expect("解析回复");
        // 结构往返：ARGB32 必须在列表里且是 32 位直接格式（cairo 选格式的依据）
        let argb = reply
            .formats
            .iter()
            .find(|f| u32::from(f.id) == FORMAT_ARGB32)
            .expect("必须有 ARGB32");
        assert_eq!(u8::from(argb.type_), u8::from(render::PictType::DIRECT));
        assert_eq!(argb.depth, 32);
        assert_eq!(argb.direct.alpha_mask, 0xff, "通道掩码是位掩码，不是位数");
        assert!(reply.formats.iter().any(|f| u32::from(f.id) == FORMAT_A8));
        assert!(reply.formats.iter().any(|f| u32::from(f.id) == FORMAT_RGB24));
        // screens/depths/visuals 结构：一个 screen，depth 24 绑定根 visual
        assert_eq!(reply.screens.len(), 1);
        assert_eq!(reply.screens[0].depths.len(), 2);
        assert_eq!(reply.screens[0].fallback, render::Pictformat::from(FORMAT_RGB24));
        assert_eq!(reply.screens[0].depths[0].visuals[0].visual, ROOT_VISUAL);
        assert_eq!(reply.num_formats(), 3);
    }

    #[test]
    fn fill_rectangles_blends_over_with_alpha() {
        let (mut state, client, _rx) = fixture();
        let window = super::super::handshake::resource_id_base(0);
        make_window(&mut state, client, 1, window);

        // picture 包装窗口（RGB24 格式）
        let picture = window + 1;
        ok(&mut state, client, 3, Request::RenderCreatePicture(render::CreatePictureRequest {
            pid: render::Picture::from(picture),
            drawable: window,
            format: render::Pictformat::from(FORMAT_RGB24),
            value_list: Cow::default(),
        }));

        // 底色：先把左上角填成纯红（Source）
        ok(&mut state, client, 4, Request::RenderFillRectangles(render::FillRectanglesRequest {
            op: render::PictOp::SRC,
            dst: render::Picture::from(picture),
            color: color(0xffff, 0, 0, 0xffff),
            rects: Cow::Owned(vec![xproto::Rectangle { x: 0, y: 0, width: 8, height: 8 }]),
        }));
        assert_eq!(state.window(window).unwrap().pixels[0], 0x00ff_0000);

        // 半透明纯绿 Over：dst = src*a + dst*(1-a)，a = 0x8080>>8 = 128
        ok(&mut state, client, 5, Request::RenderFillRectangles(render::FillRectanglesRequest {
            op: render::PictOp::OVER,
            dst: render::Picture::from(picture),
            color: color(0, 0xffff, 0, 0x8080),
            rects: Cow::Owned(vec![xproto::Rectangle { x: 0, y: 0, width: 8, height: 8 }]),
        }));
        let a = 128u32;
        let pixel = state.window(window).unwrap().pixels[0];
        assert_eq!(channel(pixel, 16), (255 * (255 - a) + 127) / 255, "红通道被稀释");
        assert_eq!(channel(pixel, 8), 128, "绿通道按 alpha 落地");
        // alpha=0 的 Over 不改变目标
        ok(&mut state, client, 6, Request::RenderFillRectangles(render::FillRectanglesRequest {
            op: render::PictOp::OVER,
            dst: render::Picture::from(picture),
            color: color(0xffff, 0, 0, 0),
            rects: Cow::Owned(vec![xproto::Rectangle { x: 0, y: 0, width: 8, height: 8 }]),
        }));
        assert_eq!(state.window(window).unwrap().pixels[0], pixel, "全透明 Over 无副作用");
    }

    #[test]
    fn solid_fill_and_composite_blends_into_window() {
        let (mut state, client, _rx) = fixture();
        let window = super::super::handshake::resource_id_base(0);
        make_window(&mut state, client, 1, window);

        // 目标 picture + solid fill 源 picture
        let dst_picture = window + 1;
        ok(&mut state, client, 3, Request::RenderCreatePicture(render::CreatePictureRequest {
            pid: render::Picture::from(dst_picture),
            drawable: window,
            format: render::Pictformat::from(FORMAT_RGB24),
            value_list: Cow::default(),
        }));
        let solid = window + 2;
        ok(&mut state, client, 4, Request::RenderCreateSolidFill(render::CreateSolidFillRequest {
            picture: render::Picture::from(solid),
            color: color(0x0000, 0x8080, 0xffff, 0xffff),
        }));
        // Composite（Source）：整窗替换成 solid 颜色
        ok(&mut state, client, 5, Request::RenderComposite(render::CompositeRequest {
            op: render::PictOp::SRC,
            src: render::Picture::from(solid),
            mask: render::Picture::from(0u32),
            dst: render::Picture::from(dst_picture),
            src_x: 0,
            src_y: 0,
            mask_x: 0,
            mask_y: 0,
            dst_x: 0,
            dst_y: 0,
            width: 16,
            height: 16,
        }));
        assert_eq!(
            state.window(window).unwrap().pixels[5 * 16 + 5],
            0x0000_80ff,
            "solid fill 直接落地"
        );

        // Over + 半透明 solid：在蓝绿底上叠半透明红
        let solid_red = window + 3;
        ok(&mut state, client, 6, Request::RenderCreateSolidFill(render::CreateSolidFillRequest {
            picture: render::Picture::from(solid_red),
            color: color(0xffff, 0, 0, 0x4040),
        }));
        ok(&mut state, client, 7, Request::RenderComposite(render::CompositeRequest {
            op: render::PictOp::OVER,
            src: render::Picture::from(solid_red),
            mask: render::Picture::from(0u32),
            dst: render::Picture::from(dst_picture),
            src_x: 0,
            src_y: 0,
            mask_x: 0,
            mask_y: 0,
            dst_x: 4,
            dst_y: 4,
            width: 8,
            height: 8,
        }));
        let a = 0x40u32;
        let pixel = state.window(window).unwrap().pixels[5 * 16 + 5];
        assert_eq!(channel(pixel, 16), a, "红按预乘落地");
        assert_eq!(channel(pixel, 8), (0x80 * (255 - a) + 127) / 255, "绿被稀释");
        // FreePicture 之后资源消失，重复 Free 回 BadPicture
        ok(&mut state, client, 8, Request::RenderFreePicture(render::FreePictureRequest {
            picture: render::Picture::from(solid_red),
        }));
        let err = state.handle(
            client,
            9,
            &Request::RenderFreePicture(render::FreePictureRequest {
                picture: render::Picture::from(solid_red),
            }),
        );
        assert_eq!(err.unwrap_err().code, BAD_PICTURE);
    }

    #[test]
    fn glyphs_blend_text_onto_window() {
        let (mut state, client, _rx) = fixture();
        let window = super::super::handshake::resource_id_base(0);
        make_window(&mut state, client, 1, window);

        let dst_picture = window + 1;
        ok(&mut state, client, 3, Request::RenderCreatePicture(render::CreatePictureRequest {
            pid: render::Picture::from(dst_picture),
            drawable: window,
            format: render::Pictformat::from(FORMAT_RGB24),
            value_list: Cow::default(),
        }));
        // 源：白色 solid（文字颜色）
        let solid = window + 2;
        ok(&mut state, client, 4, Request::RenderCreateSolidFill(render::CreateSolidFillRequest {
            picture: render::Picture::from(solid),
            color: color(0xffff, 0xffff, 0xffff, 0xffff),
        }));
        // A8 glyph set
        let glyphset = window + 3;
        ok(&mut state, client, 5, Request::RenderCreateGlyphSet(render::CreateGlyphSetRequest {
            gsid: render::Glyphset::from(glyphset),
            format: render::Pictformat::from(FORMAT_A8),
        }));

        // 一个 2x1 的字形（A8，两像素 alpha 分别 255/128），推进量 x_off=2
        let info = render::Glyphinfo { width: 2, height: 1, x: 0, y: 0, x_off: 2, y_off: 0 };
        ok(&mut state, client, 6, Request::RenderAddGlyphs(render::AddGlyphsRequest {
            glyphset: render::Glyphset::from(glyphset),
            glyphids: Cow::Owned(vec![65]),
            glyphs: Cow::Owned(vec![info]),
            data: Cow::Owned(vec![0xff, 0x80, 0, 0]), // 行按 4 字节对齐
        }));

        // CompositeGlyphs8：命令流 = 1 个元素（8 字节头：len=1, dx=0, dy=0）
        // + 1 字节字形索引 65 + 3 字节对齐
        ok(&mut state, client, 7, Request::RenderCompositeGlyphs8(render::CompositeGlyphs8Request {
            op: render::PictOp::OVER,
            src: render::Picture::from(solid),
            dst: render::Picture::from(dst_picture),
            mask_format: render::Pictformat::from(FORMAT_A8),
            glyphset: render::Glyphset::from(glyphset),
            src_x: 0,
            src_y: 0,
            glyphcmds: Cow::Owned(vec![1, 0, 0, 0, 0, 0, 0, 0, 65, 0, 0, 0]),
        }));
        let pixels = &state.window(window).unwrap().pixels;
        // 初始笔画在 (0,0)：第一个像素全白，第二个按 128/255 混白
        assert_eq!(pixels[0], 0x00ff_ffff, "alpha=255 全覆盖");
        let g = 128u32;
        assert_eq!(channel(pixels[1], 16), (255 * g + 127) / 255, "半透明像素按 alpha 混合");
        assert_eq!(channel(pixels[1], 8), (255 * g + 127) / 255);

        // len=0 的定位元素（dx=5, dy=3）先平移笔画，字形落在 (5,3)
        ok(&mut state, client, 8, Request::RenderCompositeGlyphs8(render::CompositeGlyphs8Request {
            op: render::PictOp::OVER,
            src: render::Picture::from(solid),
            dst: render::Picture::from(dst_picture),
            mask_format: render::Pictformat::from(FORMAT_A8),
            glyphset: render::Glyphset::from(glyphset),
            src_x: 0,
            src_y: 0,
            glyphcmds: Cow::Owned(vec![0, 0, 0, 0, 5, 0, 3, 0, 1, 0, 0, 0, 0, 0, 0, 0, 65, 0, 0, 0]),
        }));
        let pixels = &state.window(window).unwrap().pixels;
        assert_eq!(pixels[3 * 16 + 5], 0x00ff_ffff, "定位元素后的字形落在 (5,3)");

        // 没有的字形 → BadGlyph
        let err = state.handle(
            client,
            9,
            &Request::RenderCompositeGlyphs8(render::CompositeGlyphs8Request {
                op: render::PictOp::OVER,
                src: render::Picture::from(solid),
                dst: render::Picture::from(dst_picture),
                mask_format: render::Pictformat::from(FORMAT_A8),
                glyphset: render::Glyphset::from(glyphset),
                src_x: 0,
                src_y: 0,
                glyphcmds: Cow::Owned(vec![1, 0, 0, 0, 0, 0, 0, 0, 99, 0, 0, 0]),
            }),
        );
        assert_eq!(err.unwrap_err().code, BAD_GLYPH);
    }

    #[test]
    fn triangles_and_trapezoids_rasterize_into_window() {
        let (mut state, client, _rx) = fixture();
        let window = super::super::handshake::resource_id_base(0);
        make_window(&mut state, client, 1, window);
        let dst_picture = window + 1;
        ok(&mut state, client, 3, Request::RenderCreatePicture(render::CreatePictureRequest {
            pid: render::Picture::from(dst_picture),
            drawable: window,
            format: render::Pictformat::from(FORMAT_RGB24),
            value_list: Cow::default(),
        }));
        let solid = window + 2;
        ok(&mut state, client, 4, Request::RenderCreateSolidFill(render::CreateSolidFillRequest {
            picture: render::Picture::from(solid),
            color: color(0xffff, 0xffff, 0xffff, 0xffff),
        }));

        // 三角形 (2,2)-(10,2)-(2,10)：斜边外的点不该被填，直角处该填
        let pointfix = |x: i32, y: i32| render::Pointfix { x: x * 65536, y: y * 65536 };
        ok(&mut state, client, 5, Request::RenderTriangles(render::TrianglesRequest {
            op: render::PictOp::SRC,
            src: render::Picture::from(solid),
            dst: render::Picture::from(dst_picture),
            mask_format: render::Pictformat::from(FORMAT_A8),
            src_x: 0,
            src_y: 0,
            triangles: Cow::Owned(vec![render::Triangle {
                p1: pointfix(2, 2),
                p2: pointfix(10, 2),
                p3: pointfix(2, 10),
            }]),
        }));
        let pixels = &state.window(window).unwrap().pixels;
        let at = |x: i32, y: i32| pixels[y as usize * 16 + x as usize];
        assert_eq!(at(3, 3), 0x00ff_ffff, "三角形内部");
        assert_eq!(at(8, 2), 0x00ff_ffff, "斜边顶点附近");
        assert_eq!(at(9, 2), 0, "斜边外（像素中心不在 [xl, xr) 内）");

        // 梯形：top=2, bottom=8, 左边 x=4 垂直，右边斜线 (6,2)-(10,8)
        let trap = render::Trapezoid {
            top: 2 * 65536,
            bottom: 8 * 65536,
            left: render::Linefix { p1: pointfix(4, 2), p2: pointfix(4, 8) },
            right: render::Linefix { p1: pointfix(6, 2), p2: pointfix(10, 8) },
        };
        ok(&mut state, client, 6, Request::RenderTrapezoids(render::TrapezoidsRequest {
            op: render::PictOp::OVER,
            src: render::Picture::from(solid),
            dst: render::Picture::from(dst_picture),
            mask_format: render::Pictformat::from(FORMAT_A8),
            src_x: 0,
            src_y: 0,
            traps: Cow::Owned(vec![trap]),
        }));
        let pixels = &state.window(window).unwrap().pixels;
        let at = |x: i32, y: i32| pixels[y as usize * 16 + x as usize];
        assert_eq!(at(4, 4), 0x00ff_ffff, "梯形内部（Over 全覆盖同色）");
        assert_eq!(at(0, 4), 0, "梯形与三角形都不覆盖");
    }

    #[test]
    fn picture_clip_and_lifecycle() {
        let (mut state, client, _rx) = fixture();
        let window = super::super::handshake::resource_id_base(0);
        make_window(&mut state, client, 1, window);
        let dst_picture = window + 1;
        ok(&mut state, client, 3, Request::RenderCreatePicture(render::CreatePictureRequest {
            pid: render::Picture::from(dst_picture),
            drawable: window,
            format: render::Pictformat::from(FORMAT_RGB24),
            value_list: Cow::Owned(render::CreatePictureAux {
                repeat: Some(render::Repeat::NORMAL),
                ..Default::default()
            }),
        }));
        assert!(state.pictures[&dst_picture].repeat, "CreatePicture 的 repeat 属性");

        // SetPictureClipRectangles：只允许 (0,0)-(4,4)
        ok(&mut state, client, 4, Request::RenderSetPictureClipRectangles(
            render::SetPictureClipRectanglesRequest {
                picture: render::Picture::from(dst_picture),
                clip_x_origin: 0,
                clip_y_origin: 0,
                rectangles: Cow::Owned(vec![xproto::Rectangle { x: 0, y: 0, width: 4, height: 4 }]),
            },
        ));
        ok(&mut state, client, 5, Request::RenderFillRectangles(render::FillRectanglesRequest {
            op: render::PictOp::SRC,
            dst: render::Picture::from(dst_picture),
            color: color(0xffff, 0, 0, 0xffff),
            rects: Cow::Owned(vec![xproto::Rectangle { x: 0, y: 0, width: 16, height: 16 }]),
        }));
        let pixels = &state.window(window).unwrap().pixels;
        assert_eq!(pixels[3 * 16 + 3], 0x00ff_0000, "裁剪区内被填充");
        assert_eq!(pixels[5 * 16 + 5], 0, "裁剪区外不受影响");

        // ChangePicture 关掉 repeat
        ok(&mut state, client, 6, Request::RenderChangePicture(render::ChangePictureRequest {
            picture: render::Picture::from(dst_picture),
            value_list: Cow::Owned(render::ChangePictureAux {
                repeat: Some(render::Repeat::PAD),
                ..Default::default()
            }),
        }));
        assert!(!state.pictures[&dst_picture].repeat);

        // FreePicture 后资源消失，重复 Free 回 BadPicture
        ok(&mut state, client, 7, Request::RenderFreePicture(render::FreePictureRequest {
            picture: render::Picture::from(dst_picture),
        }));
        let err = state.handle(
            client,
            8,
            &Request::RenderFreePicture(render::FreePictureRequest {
                picture: render::Picture::from(dst_picture),
            }),
        );
        assert_eq!(err.unwrap_err().code, BAD_PICTURE);

        // Gradients 已实现：空停靠点按参数校验回 BadValue（非零停靠点正常创建）
        let err = state.handle(
            client,
            9,
            &Request::RenderCreateLinearGradient(render::CreateLinearGradientRequest {
                picture: render::Picture::from(dst_picture),
                p1: render::Pointfix { x: 0, y: 0 },
                p2: render::Pointfix { x: 0, y: 0 },
                stops: Cow::default(),
                colors: Cow::default(),
            }),
        );
        assert_eq!(err.unwrap_err().code, BAD_VALUE, "空停靠点回 BadValue");
        ok(&mut state, client, 10, Request::RenderCreateLinearGradient(render::CreateLinearGradientRequest {
            picture: render::Picture::from(dst_picture),
            p1: render::Pointfix { x: 0, y: 0 },
            p2: render::Pointfix { x: 0x0100, y: 0 },
            stops: Cow::Owned(vec![0i32, 0x7fff_0000]),
            colors: Cow::Owned(vec![
                render::Color { red: 0xffff, green: 0xffff, blue: 0xffff, alpha: 0xffff },
                render::Color { red: 0, green: 0, blue: 0, alpha: 0 },
            ]),
        }));
        assert!(matches!(
            state.picture(dst_picture).unwrap().source,
            PictureSource::LinearGradient { .. }
        ), "线性渐变 picture 已创建");
    }

    /// 光标持有 picture 引用（X11 引用计数语义）——firefox 的流程：
    /// 建 picture → CreateCursor → 立刻 FreePicture → 再建第二个 cursor
    /// 引用同一 picture。FreePicture 不能删掉被光标引用的 picture。
    #[test]
    fn cursor_holds_picture_reference() {
        let (mut state, client, _rx) = fixture();
        let base = super::super::handshake::resource_id_base(0);
        // 建一个 pixmap（depth 32 ARGB）
        let pixmap = base + 1;
        ok(&mut state, client, 1, Request::CreatePixmap(xproto::CreatePixmapRequest {
            depth: 32,
            pid: pixmap,
            drawable: super::super::handshake::ROOT_WINDOW,
            width: 8,
            height: 8,
        }));
        let picture = base + 2;
        ok(&mut state, client, 2, Request::RenderCreatePicture(render::CreatePictureRequest {
            pid: render::Picture::from(picture),
            drawable: xproto::Drawable::from(pixmap),
            format: render::Pictformat::from(FORMAT_ARGB32),
            value_list: Cow::default(),
        }));
        // CreateCursor 引用 picture
        let cid = base + 3;
        ok(&mut state, client, 3, Request::RenderCreateCursor(render::CreateCursorRequest {
            cid: xproto::Cursor::from(cid),
            source: render::Picture::from(picture),
            x: 0,
            y: 0,
        }));
        // FreePicture：客户端引用释放，但光标引用保留 → picture 必须存活
        ok(&mut state, client, 4, Request::RenderFreePicture(render::FreePictureRequest {
            picture: render::Picture::from(picture),
        }));
        assert!(state.pictures.contains_key(&picture), "光标引用的 picture 不能被 FreePicture 删除");
        // 再建第二个 cursor 引用同一 picture（firefox 的崩溃点）
        ok(&mut state, client, 5, Request::RenderCreateCursor(render::CreateCursorRequest {
            cid: xproto::Cursor::from(cid + 1),
            source: render::Picture::from(picture),
            x: 0,
            y: 0,
        }));
        // FreeCursor 释放引用 → picture 归零销毁
        ok(&mut state, client, 6, Request::FreeCursor(xproto::FreeCursorRequest {
            cursor: xproto::Cursor::from(cid),
        }));
        ok(&mut state, client, 7, Request::FreeCursor(xproto::FreeCursorRequest {
            cursor: xproto::Cursor::from(cid + 1),
        }));
        assert!(!state.pictures.contains_key(&picture), "所有光标释放后 picture 才销毁");
    }

    /// picture 引用 pixmap 的引用计数语义——GTK 的光标流程：
    /// CreatePixmap → CreatePicture → DestroyPixmap（指望 picture 持有引用）
    /// → CreateCursor → FreePicture。DestroyPixmap 不能删掉被 picture
    /// 引用的 pixmap（X11 里 picture 独立存在，只是 drawable 引用失效）。
    #[test]
    fn picture_holds_pixmap_reference() {
        let (mut state, client, _rx) = fixture();
        let base = super::super::handshake::resource_id_base(0);
        let pixmap = base + 1;
        ok(&mut state, client, 1, Request::CreatePixmap(xproto::CreatePixmapRequest {
            depth: 32,
            pid: pixmap,
            drawable: super::super::handshake::ROOT_WINDOW,
            width: 8,
            height: 8,
        }));
        let picture = base + 2;
        ok(&mut state, client, 2, Request::RenderCreatePicture(render::CreatePictureRequest {
            pid: render::Picture::from(picture),
            drawable: xproto::Drawable::from(pixmap),
            format: render::Pictformat::from(FORMAT_ARGB32),
            value_list: Cow::default(),
        }));
        // DestroyPixmap：picture 引用 → pixmap 必须存活（picture 不能被连带删除）
        ok(&mut state, client, 3, Request::FreePixmap(xproto::FreePixmapRequest {
            pixmap: xproto::Pixmap::from(pixmap),
        }));
        assert!(state.pixmaps.contains_key(&pixmap), "被 picture 引用的 pixmap 不能被 DestroyPixmap 删除");
        assert!(state.pictures.contains_key(&picture), "picture 不能被 DestroyPixmap 连带删除");
        // picture 仍可使用（CreateCursor 引用它）
        let cid = base + 3;
        ok(&mut state, client, 4, Request::RenderCreateCursor(render::CreateCursorRequest {
            cid: xproto::Cursor::from(cid),
            source: render::Picture::from(picture),
            x: 0,
            y: 0,
        }));
        // FreePicture：无光标引用时才释放（光标引用时保留）
        ok(&mut state, client, 5, Request::RenderFreePicture(render::FreePictureRequest {
            picture: render::Picture::from(picture),
        }));
        assert!(state.pictures.contains_key(&picture), "光标引用时 FreePicture 不能删 picture");
        // FreeCursor 释放引用 → picture 销毁
        ok(&mut state, client, 6, Request::FreeCursor(xproto::FreeCursorRequest {
            cursor: xproto::Cursor::from(cid),
        }));
        assert!(!state.pictures.contains_key(&picture), "光标释放引用后 picture 销毁");
    }

    #[test]
    fn blend_pixel_ops() {
        let red = (255, 0, 0, 255);
        // Source 直接替换
        assert_eq!(blend_pixel(u8::from(render::PictOp::SRC), 0x0000_ff00, red), Some(0x00ff_0000));
        // Over：alpha 255 全覆盖
        assert_eq!(blend_pixel(u8::from(render::PictOp::OVER), 0x0000_ff00, red), Some(0x00ff_0000));
        // Over：半透明绿叠蓝底（预乘绿 128，alpha 128）
        let half_green = (0, 128, 0, 128);
        let out = blend_pixel(u8::from(render::PictOp::OVER), 0x0000_00ff, half_green).unwrap();
        assert_eq!(out, pixel_of(0, mix_over(0, 128, 128), mix_over(255, 0, 128)));
        // In：不透明目标退化为预乘源色
        assert_eq!(blend_pixel(u8::from(render::PictOp::IN), 0x00ff_ffff, half_green), Some(pixel_of(0, 128, 0)));
        // 未支持的 op 返回 None；CLEAR 是合法 op（黑色透明输出）
        assert_eq!(blend_pixel(99, 0, red), None);
        assert_eq!(blend_pixel(u8::from(render::PictOp::CLEAR), 0, red), Some(0x0000_0000));
    }

    #[test]
    fn glyph_command_parsing() {
        // 8 位：8 字节元素头（len: CARD32, dx: INT16, dy: INT16）+ 紧密字形 +
        // 补齐到 4 字节
        let mut cmds = 1u32.to_ne_bytes().to_vec(); // len=1
        cmds.extend_from_slice(&0i16.to_ne_bytes());
        cmds.extend_from_slice(&0i16.to_ne_bytes());
        cmds.push(65);
        cmds.extend_from_slice(&[0; 3]); // 对齐
        // len=0 的定位元素：dx=5, dy=-3
        cmds.extend_from_slice(&0u32.to_ne_bytes());
        cmds.extend_from_slice(&5i16.to_ne_bytes());
        cmds.extend_from_slice(&(-3i16).to_ne_bytes());
        // len=2：两个字形（2 字节，无需额外对齐）
        cmds.extend_from_slice(&2u32.to_ne_bytes());
        cmds.extend_from_slice(&0i16.to_ne_bytes());
        cmds.extend_from_slice(&0i16.to_ne_bytes());
        cmds.extend_from_slice(&[66, 67, 0, 0]); // 2 字节字形 + 对齐填充
        let parsed = glyph_commands(&cmds, 8).unwrap();
        assert_eq!(
            parsed,
            vec![
                GlyphCommand::Position { dx: 0, dy: 0 },
                GlyphCommand::Glyph(65),
                GlyphCommand::Position { dx: 5, dy: -3 },
                GlyphCommand::Position { dx: 0, dy: 0 },
                GlyphCommand::Glyph(66),
                GlyphCommand::Glyph(67),
            ]
        );

        // 16 位：字形各占 2 字节；0xff 元素切换 glyphset（头后跟 4 字节 ID）
        let mut cmds16 = 0xffu32.to_ne_bytes().to_vec();
        cmds16.extend_from_slice(&0i16.to_ne_bytes());
        cmds16.extend_from_slice(&0i16.to_ne_bytes());
        cmds16.extend_from_slice(&42u32.to_ne_bytes()); // 新 glyphset
        cmds16.extend_from_slice(&1u32.to_ne_bytes()); // len=1
        cmds16.extend_from_slice(&1i16.to_ne_bytes());
        cmds16.extend_from_slice(&2i16.to_ne_bytes());
        cmds16.extend_from_slice(&9u16.to_ne_bytes());
        cmds16.extend_from_slice(&[0; 2]);
        let parsed = glyph_commands(&cmds16, 16).unwrap();
        assert_eq!(
            parsed,
            vec![
                GlyphCommand::SwitchGlyphSet(42),
                GlyphCommand::Position { dx: 1, dy: 2 },
                GlyphCommand::Glyph(9),
            ]
        );

        // 不足一个元素头的尾部直接忽略（Xorg 的 while 条件语义）
        assert_eq!(glyph_commands(&[0, 5], 8).unwrap(), Vec::new());
        // 字形列表越界（头完整但字形数据不足）→ BadLength
        let mut truncated = 1u32.to_ne_bytes().to_vec();
        truncated.extend_from_slice(&0i16.to_ne_bytes());
        truncated.extend_from_slice(&0i16.to_ne_bytes());
        truncated.extend_from_slice(&[65, 0]);
        assert!(glyph_commands(&truncated, 8).is_err());
    }

    /// 回归：Trapezoids/Triangles 等"形状"请求会**借走**源 drawable 的像素缓冲
    /// （`BorrowedPixels` 是显式归还，没有 Drop）。曾经漏了归还：那块 drawable
    /// 的 pixels 被 `mem::take` 抽空且不还，之后 compose() 读到空缓冲 → 画面
    /// 冻结 / 线程 panic 毒化状态锁 → 整个界面再也画不出来
    /// （用户实测：滚动一下就卡死）。
    #[test]
    fn trapezoids_restores_the_borrowed_source_pixels() {
        let (mut state, client, _rx) = fixture();
        let src_pixmap = 0x0020_0100u32;
        let (w, h) = (8usize, 8usize);
        let original: Vec<u32> = (0..(w * h) as u32).map(|i| 0x0011_0000 + i).collect();
        state.pixmaps.insert(
            src_pixmap,
            super::super::state::PixmapData {
                id: src_pixmap,
                width: w as u16,
                height: h as u16,
                depth: 24,
                pixels: original.clone(),
                owner: u32::MAX,
            },
        );
        // 源 picture：drawable 包装（会被借走）
        state.pictures.insert(
            0x0020_0101,
            PictureData {
                id: 0x0020_0101,
                owner: client,
                format: FORMAT_RGB24,
                source: PictureSource::Drawable(src_pixmap),
                repeat: false,
                clip: None,
                clip_x_origin: 0,
                clip_y_origin: 0,
                filter: String::new(),
            },
        );
        // 目标 picture：包在根窗口上
        state.pictures.insert(
            0x0020_0102,
            PictureData {
                id: 0x0020_0102,
                owner: client,
                format: FORMAT_RGB24,
                source: PictureSource::Drawable(super::super::handshake::ROOT_WINDOW),
                repeat: false,
                clip: None,
                clip_x_origin: 0,
                clip_y_origin: 0,
                filter: String::new(),
            },
        );

        let request = render::TrapezoidsRequest {
            op: render::PictOp::OVER,
            src: 0x0020_0101,
            dst: 0x0020_0102,
            mask_format: 0,
            src_x: 0,
            src_y: 0,
            traps: std::borrow::Cow::Owned(vec![render::Trapezoid {
                // 退化梯形（高度 0）：不产生任何 span —— 本测试只关心源像素的
                // 借用与归还，几何形状无关紧要。
                top: 0,
                bottom: 0,
                left: render::Linefix {
                    p1: render::Pointfix { x: 0, y: 0 },
                    p2: render::Pointfix { x: 0, y: 0 },
                },
                right: render::Linefix {
                    p1: render::Pointfix { x: 0, y: 0 },
                    p2: render::Pointfix { x: 0, y: 0 },
                },
            }]),
        };
        state
            .render_trapezoids(&request)
            .expect("Trapezoids 应当成功");

        let after = &state.pixmaps.get(&src_pixmap).expect("源 pixmap 还在").pixels;
        assert_eq!(
            after.len(),
            w * h,
            "源 drawable 的像素缓冲必须被归还（不能是空 Vec）"
        );
        assert_eq!(after, &original, "归还的像素内容不应被改动");
    }
}
