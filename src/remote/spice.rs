//! SPICE 客户端 —— 基于 vendor 的 capsaicin。
//!
//! 画面：主 surface（id 0）作为桌面输出；子 surface 单独维护一份缓冲，
//! **不合成**到主画面 —— SPICE 把子 surface 贴到主面用的是 draw 通道的
//! `DRAW_COPY_BITS`，capsaicin 目前把它们归到 `UnhandledDraw`，
//! 客户端拿不到「贴到哪、贴多大」，硬合成只会把像素画到错误位置。
//! 子面缓冲留着，等 capsaicin 解析 draw 通道后可以直接接上。
//!
//! 光标：走 cursor 通道（`CursorEvent`），转成与 RDP / VNC 相同的
//! `RdEvent::Cursor` / `CursorPos` / `CursorHidden`，由 UI 侧统一绘制并
//! 隐藏本地系统光标 —— 否则会出现「两个鼠标」。
//!
//! 帧输出仍是 RGBA8 脏矩形。

use anyhow::{anyhow, Result};
use capsaicin_client::{
    ClientEvent, CursorEvent, DisplayEvent, InputEvent, MouseMode, RegionPixels, SpiceClient,
    SurfaceFormat,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use super::{RdCommand, RdEvent, RdParams, RdState};

const FRAME_INTERVAL: Duration = Duration::from_millis(33);
const POLL_INTERVAL: Duration = Duration::from_millis(8);

type Dirty = Option<(u32, u32, u32, u32)>;

fn union(dirty: Dirty, r: (u32, u32, u32, u32)) -> Dirty {
    if r.2 == 0 || r.3 == 0 {
        return dirty;
    }
    Some(match dirty {
        None => r,
        Some(d) => {
            // saturating_add：debug 构建下 u32 溢出会 panic
            let x1 = d.0.min(r.0);
            let y1 = d.1.min(r.1);
            let x2 = d.0.saturating_add(d.2).max(r.0.saturating_add(r.2));
            let y2 = d.1.saturating_add(d.3).max(r.1.saturating_add(r.3));
            (x1, y1, x2 - x1, y2 - y1)
        }
    })
}

struct Fb {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl Fb {
    fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 || width > 8192 || height > 8192 {
            return Err(anyhow!("SPICE 分辨率异常: {width}x{height}"));
        }
        Ok(Self {
            width,
            height,
            rgba: vec![0u8; width as usize * height as usize * 4],
        })
    }

    fn blit_raw(
        &mut self,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        data: &[u8],
        stride: u32,
        format: SurfaceFormat,
    ) {
        if w == 0 || h == 0 {
            return;
        }
        let dst_stride = self.width as usize * 4;
        for row in 0..h as usize {
            let dy = y as i64 + row as i64;
            if dy < 0 || dy >= self.height as i64 {
                continue;
            }
            let src_off = row * stride as usize;
            for col in 0..w as usize {
                let dx = x as i64 + col as i64;
                if dx < 0 || dx >= self.width as i64 {
                    continue;
                }
                let so = src_off + col * 4;
                if so + 4 > data.len() {
                    continue;
                }
                let (r, g, b) = convert_px(&data[so..so + 4], format);
                let doff = dy as usize * dst_stride + dx as usize * 4;
                self.rgba[doff] = r;
                self.rgba[doff + 1] = g;
                self.rgba[doff + 2] = b;
                self.rgba[doff + 3] = 255;
            }
        }
    }

    fn fill_solid(&mut self, x: i32, y: i32, w: u32, h: u32, color: u32) {
        let r = ((color >> 16) & 0xff) as u8;
        let g = ((color >> 8) & 0xff) as u8;
        let b = (color & 0xff) as u8;
        let stride = self.width as usize * 4;
        for row in 0..h as usize {
            let dy = y as i64 + row as i64;
            if dy < 0 || dy >= self.height as i64 {
                continue;
            }
            for col in 0..w as usize {
                let dx = x as i64 + col as i64;
                if dx < 0 || dx >= self.width as i64 {
                    continue;
                }
                let off = dy as usize * stride + dx as usize * 4;
                self.rgba[off] = r;
                self.rgba[off + 1] = g;
                self.rgba[off + 2] = b;
                self.rgba[off + 3] = 255;
            }
        }
    }

    fn copy_rect(&mut self, src_x: i32, src_y: i32, dst_x: i32, dst_y: i32, w: u32, h: u32) {
        let stride = self.width as usize * 4;
        let bytes = w as usize * 4;
        let mut tmp = vec![0u8; bytes];
        for row in 0..h as usize {
            let sy = src_y as i64 + row as i64;
            let dy = dst_y as i64 + row as i64;
            if sy < 0 || dy < 0 || sy >= self.height as i64 || dy >= self.height as i64 {
                continue;
            }
            if src_x < 0 || dst_x < 0 {
                continue;
            }
            let so = sy as usize * stride + src_x as usize * 4;
            let doff = dy as usize * stride + dst_x as usize * 4;
            if so + bytes > self.rgba.len() || doff + bytes > self.rgba.len() {
                continue;
            }
            tmp.copy_from_slice(&self.rgba[so..so + bytes]);
            self.rgba[doff..doff + bytes].copy_from_slice(&tmp);
        }
    }

    fn sub_rgba(&self, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
        // 硬性裁剪到帧缓冲范围内（dirty 矩形可能越界 → 切片 panic）
        if self.width == 0 || self.height == 0 {
            return Vec::new();
        }
        let x = x.min(self.width - 1);
        let y = y.min(self.height - 1);
        let w = w.min(self.width - x);
        let h = h.min(self.height - y);
        if w == 0 || h == 0 {
            return Vec::new();
        }
        let stride = self.width as usize * 4;
        let mut out = Vec::with_capacity(w as usize * h as usize * 4);
        for row in 0..h as usize {
            let off = (y as usize + row) * stride + x as usize * 4;
            if off + w as usize * 4 > self.rgba.len() {
                break;
            }
            out.extend_from_slice(&self.rgba[off..off + w as usize * 4]);
        }
        out
    }
}

/// SPICE 坐标是 i32（可能为负，光标移出桌面时会给出负值），RdEvent 用 u16。
fn clamp_pos(v: i32) -> u16 {
    v.clamp(0, u16::MAX as i32) as u16
}

/// SPICE 光标位图（Argb8888，内存序 B,G,R,A）→ RGBA8。
///
/// 与帧缓冲不同，光标位图的 alpha 是真实透明度（非预乘），必须保留，
/// 否则光标会画成一块实心方块。
fn argb_to_rgba(pixels: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len());
    for px in pixels.chunks_exact(4) {
        out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }
    out
}

fn convert_px(px: &[u8], format: SurfaceFormat) -> (u8, u8, u8) {
    match format {
        // 内存序 B,G,R,A
        SurfaceFormat::Xrgb8888 | SurfaceFormat::Argb8888 => (px[2], px[1], px[0]),
        SurfaceFormat::Rgb565 => {
            let v = u16::from_le_bytes([px[0], px[1]]) as u32;
            let r = ((v >> 11) & 0x1f) as u8;
            let g = ((v >> 5) & 0x3f) as u8;
            let b = (v & 0x1f) as u8;
            (r << 3 | r >> 2, g << 2 | g >> 4, b << 3 | b >> 2)
        }
        SurfaceFormat::Rgb555 => {
            let v = u16::from_le_bytes([px[0], px[1]]) as u32;
            let r = ((v >> 10) & 0x1f) as u8;
            let g = ((v >> 5) & 0x1f) as u8;
            let b = (v & 0x1f) as u8;
            (r << 3 | r >> 2, g << 3 | g >> 2, b << 3 | b >> 2)
        }
        _ => (0, 0, 0),
    }
}

pub async fn run(
    params: &RdParams,
    ev: &Sender<RdEvent>,
    cmds: &Receiver<RdCommand>,
    cancel: &AtomicBool,
) -> Result<()> {
    let _ = ev.send(RdEvent::State(RdState::Connecting));
    let addr = format!("{}:{}", params.host, params.port);
    let mut client = tokio::time::timeout(
        Duration::from_secs(20),
        SpiceClient::connect(&addr, &params.password),
    )
    .await
    .map_err(|_| anyhow!("SPICE 连接 {addr} 超时"))?
    .map_err(|e| anyhow!("SPICE 连接 {addr} 失败: {e}"))?;

    let _ = ev.send(RdEvent::State(RdState::Active));

    let mut fb: Option<Fb> = None;
    let mut fb_format = SurfaceFormat::Xrgb8888;
    // 子 surface 缓冲（id → 帧）。不参与主画面输出，见文件头说明。
    let mut subs: HashMap<u32, Fb> = HashMap::new();
    // 可缓存的光标位图（unique → 宽高/热点/像素）
    let mut cursor_cache: HashMap<u64, (u16, u16, u16, u16, Vec<u8>)> = HashMap::new();
    let mut dirty: Dirty = None;
    let mut last_frame = Instant::now() - FRAME_INTERVAL;
    let mut buttons: u8 = 0;
    let mut last_pos = (0u16, 0u16);
    let mut absolute_mouse = true;

    loop {
        if cancel.load(Ordering::Relaxed) {
            client.close().await;
            return Ok(());
        }

        // ---- 上行输入 ----
        while let Ok(cmd) = cmds.try_recv() {
            match cmd {
                RdCommand::Close => {
                    client.close().await;
                    return Ok(());
                }
                RdCommand::Key(k) => {
                    let code = if k.scancode & 0xFF00 == 0xE000 {
                        0xE000u32 | (k.scancode & 0xFF) as u32
                    } else {
                        k.scancode as u32
                    };
                    if code != 0 {
                        let input = if k.down {
                            InputEvent::KeyDown(code)
                        } else {
                            InputEvent::KeyUp(code)
                        };
                        let _ = client.send_input(input).await;
                    }
                }
                RdCommand::Pointer { x, y, buttons: nb } => {
                    if absolute_mouse {
                        let _ = client
                            .send_input(InputEvent::MousePosition {
                                x: x as u32,
                                y: y as u32,
                                buttons: nb as u32,
                                display: 0,
                            })
                            .await;
                    } else {
                        let dx = x as i32 - last_pos.0 as i32;
                        let dy = y as i32 - last_pos.1 as i32;
                        if dx != 0 || dy != 0 {
                            let _ = client
                                .send_input(InputEvent::MouseMotion {
                                    dx,
                                    dy,
                                    buttons: nb as u32,
                                })
                                .await;
                        }
                    }
                    last_pos = (x, y);
                    // 按键状态变化
                    if nb != buttons {
                        for (bit, spice_btn) in [(1u8, 1u8), (2u8, 2u8), (4u8, 3u8)] {
                            let was = buttons & bit != 0;
                            let now = nb & bit != 0;
                            if was != now {
                                let input = if now {
                                    InputEvent::MousePress {
                                        button: spice_btn,
                                        buttons: nb as u32,
                                    }
                                } else {
                                    InputEvent::MouseRelease {
                                        button: spice_btn,
                                        buttons: nb as u32,
                                    }
                                };
                                let _ = client.send_input(input).await;
                            }
                        }
                        buttons = nb;
                    }
                }
                RdCommand::Wheel { dy } => {
                    // SPICE 滚轮用 4/5 号键
                    let button = if dy > 0 { 4u8 } else { 5u8 };
                    let _ = client
                        .send_input(InputEvent::MousePress {
                            button,
                            buttons: buttons as u32,
                        })
                        .await;
                    let _ = client
                        .send_input(InputEvent::MouseRelease {
                            button,
                            buttons: buttons as u32,
                        })
                        .await;
                }
                // SPICE 后端尚未实现剪贴板通道（capsaicin-client 没有暴露
                // 剪贴板接口），开关对本协议暂无效果。
                RdCommand::Clipboard(_) => {}
            }
        }

        // ---- 下行事件 ----
        match tokio::time::timeout(POLL_INTERVAL, client.next_event()).await {
            Ok(Some(event)) => match event {
                ClientEvent::Display(DisplayEvent::SurfaceCreated {
                    id,
                    width,
                    height,
                    format,
                    primary,
                }) => {
                    if width == 0 || height == 0 {
                        continue;
                    }
                    if primary || id == 0 || fb.is_none() {
                        // 尺寸没变就不重建，避免把已有画面清零
                        let changed = fb
                            .as_ref()
                            .map(|f| f.width != width || f.height != height)
                            .unwrap_or(true);
                        if changed {
                            fb = Some(Fb::new(width, height)?);
                            fb_format = format;
                            let _ = ev.send(RdEvent::Resized { width, height });
                            dirty = Some((0, 0, width, height));
                        }
                    } else {
                        // 子 surface（视频叠加层 / 子窗口）：单独留一份缓冲。
                        // 合成需要 draw 通道的 destination，capsaicin 还没解析，
                        // 这里只保证「不把子面的像素当主面画」。
                        if subs.len() < 16 {
                            let _ = subs.insert(id, Fb::new(width, height)?);
                        }
                    }
                }
                ClientEvent::Display(DisplayEvent::SurfaceDestroyed { id }) => {
                    subs.remove(&id);
                }
                ClientEvent::Display(DisplayEvent::Mode { width, height, .. }) => {
                    // 分辨率可能在连接后变化（用户改分辨率 / VM 重配）：
                    // 只要尺寸变了就要重建帧缓冲并发 Resized，否则后面的
                    // region 坐标按新分辨率来、画面会错位。
                    if width == 0 || height == 0 {
                        continue;
                    }
                    let changed = fb
                        .as_ref()
                        .map(|f| f.width != width || f.height != height)
                        .unwrap_or(true);
                    if changed {
                        fb = Some(Fb::new(width, height)?);
                        let _ = ev.send(RdEvent::Resized { width, height });
                        dirty = Some((0, 0, width, height));
                    }
                }
                ClientEvent::Display(DisplayEvent::Region {
                    surface_id,
                    rect,
                    pixels,
                    surface_format,
                }) => {
                    if surface_id != 0 {
                        // 子 surface 的绘制落到它自己的缓冲里（不输出、不标脏）
                        if let Some(f) = subs.get_mut(&surface_id) {
                            let w = rect.width().max(0) as u32;
                            let h = rect.height().max(0) as u32;
                            if w > 0 && h > 0 {
                                match pixels {
                                    RegionPixels::Raw { data, stride } => f.blit_raw(
                                        rect.left,
                                        rect.top,
                                        w,
                                        h,
                                        &data,
                                        stride,
                                        surface_format,
                                    ),
                                    RegionPixels::SolidColor(color) => {
                                        f.fill_solid(rect.left, rect.top, w, h, color)
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    let Some(f) = fb.as_mut() else { continue };
                    let x = rect.left;
                    let y = rect.top;
                    // rect.width()/height() 是 i32，畸形包可能为负 →
                    // 直接 as u32 会变成 ~40 亿，污染 dirty 矩形导致越界 panic
                    let w = rect.width().max(0) as u32;
                    let h = rect.height().max(0) as u32;
                    if w == 0 || h == 0 {
                        continue;
                    }
                    match pixels {
                        RegionPixels::Raw { data, stride } => {
                            f.blit_raw(x, y, w, h, &data, stride, surface_format)
                        }
                        RegionPixels::SolidColor(color) => f.fill_solid(x, y, w, h, color),
                    }
                    dirty = union(
                        dirty,
                        (x.max(0) as u32, y.max(0) as u32, w, h),
                    );
                }
                ClientEvent::Display(DisplayEvent::CopyRect {
                    surface_id,
                    src_x,
                    src_y,
                    dest_rect,
                }) => {
                    let cw = dest_rect.width().max(0) as u32;
                    let ch = dest_rect.height().max(0) as u32;
                    if cw == 0 || ch == 0 {
                        continue;
                    }
                    if surface_id != 0 {
                        if let Some(f) = subs.get_mut(&surface_id) {
                            f.copy_rect(
                                src_x,
                                src_y,
                                dest_rect.left,
                                dest_rect.top,
                                cw,
                                ch,
                            );
                        }
                        continue;
                    }
                    if let Some(f) = fb.as_mut() {
                        f.copy_rect(
                            src_x,
                            src_y,
                            dest_rect.left,
                            dest_rect.top,
                            cw,
                            ch,
                        );
                        dirty = union(
                            dirty,
                            (
                                dest_rect.left.max(0) as u32,
                                dest_rect.top.max(0) as u32,
                                cw,
                                ch,
                            ),
                        );
                    }
                }
                ClientEvent::Display(DisplayEvent::StreamFrame { dest_rect, pixels, .. }) => {
                    if let Some(f) = fb.as_mut() {
                        let w = dest_rect.width().max(0) as u32;
                        let h = dest_rect.height().max(0) as u32;
                        if w == 0 || h == 0 {
                            continue;
                        }
                        match pixels {
                            RegionPixels::Raw { data, stride } => f.blit_raw(
                                dest_rect.left,
                                dest_rect.top,
                                w,
                                h,
                                &data,
                                stride,
                                SurfaceFormat::Xrgb8888,
                            ),
                            RegionPixels::SolidColor(color) => f.fill_solid(
                                dest_rect.left,
                                dest_rect.top,
                                w,
                                h,
                                color,
                            ),
                        }
                        dirty = union(
                            dirty,
                            (
                                dest_rect.left.max(0) as u32,
                                dest_rect.top.max(0) as u32,
                                w,
                                h,
                            ),
                        );
                    }
                }
                ClientEvent::Display(DisplayEvent::Reset) => {
                    if let Some(f) = fb.as_mut() {
                        f.rgba.iter_mut().for_each(|b| *b = 0);
                        dirty = Some((0, 0, f.width, f.height));
                    }
                }
                // ---- 远端光标 ----
                //
                // SPICE 的光标走独立通道（位图 + 位置 + 可见性）。不处理的话
                // 画面上就没有远端指针，只剩本地箭头 —— 在远端桌面里
                // 「看到箭头但点不准」基本都是这个原因。
                ClientEvent::Cursor(cursor_evt) => match cursor_evt {
                    CursorEvent::Set {
                        x,
                        y,
                        hot_x,
                        hot_y,
                        width,
                        height,
                        pixels,
                        unique,
                        cacheable,
                        visible,
                    } => {
                        if cacheable {
                            cursor_cache.insert(
                                unique,
                                (width, height, hot_x, hot_y, pixels.clone()),
                            );
                        }
                        if !visible {
                            let _ = ev.send(RdEvent::CursorHidden);
                            continue;
                        }
                        if width == 0 || height == 0 || pixels.is_empty() {
                            // 未解码的形状（旧式单色光标）：干脆退回本地系统光标，
                            // 总比画面里完全没有指针可用
                            let _ = ev.send(RdEvent::CursorHidden);
                            continue;
                        }
                        let _ = ev.send(RdEvent::Cursor {
                            width,
                            height,
                            hotspot_x: hot_x,
                            hotspot_y: hot_y,
                            rgba: argb_to_rgba(&pixels),
                        });
                        let _ = ev.send(RdEvent::CursorPos {
                            x: clamp_pos(x),
                            y: clamp_pos(y),
                        });
                    }
                    CursorEvent::SetFromCache {
                        x,
                        y,
                        unique,
                        visible,
                    } => {
                        if !visible {
                            let _ = ev.send(RdEvent::CursorHidden);
                            continue;
                        }
                        if let Some((w, h, hx, hy, px)) = cursor_cache.get(&unique) {
                            let _ = ev.send(RdEvent::Cursor {
                                width: *w,
                                height: *h,
                                hotspot_x: *hx,
                                hotspot_y: *hy,
                                rgba: argb_to_rgba(px),
                            });
                            let _ = ev.send(RdEvent::CursorPos {
                                x: clamp_pos(x),
                                y: clamp_pos(y),
                            });
                        }
                    }
                    CursorEvent::Move { x, y } => {
                        let _ = ev.send(RdEvent::CursorPos {
                            x: clamp_pos(x),
                            y: clamp_pos(y),
                        });
                    }
                    CursorEvent::Hide => {
                        let _ = ev.send(RdEvent::CursorHidden);
                    }
                    CursorEvent::InvalidateOne { unique } => {
                        cursor_cache.remove(&unique);
                    }
                    CursorEvent::InvalidateAll => cursor_cache.clear(),
                },
                ClientEvent::MouseMode(mode) => {
                    absolute_mouse = matches!(mode, MouseMode::Client);
                }
                ClientEvent::Closed(err) => {
                    return Err(anyhow!(
                        "SPICE 连接关闭{}",
                        err.map(|e| format!(": {e}")).unwrap_or_default()
                    ));
                }
                _ => {}
            },
            Ok(None) => return Err(anyhow!("SPICE 事件流结束")),
            Err(_) => {}
        }

        // ---- 推送脏区 ----
        if let (Some(rect), Some(f)) = (dirty, fb.as_ref()) {
            if last_frame.elapsed() >= FRAME_INTERVAL {
                last_frame = Instant::now();
                dirty = None;
                let (x, y, w, h) = rect;
                let w = w.min(f.width.saturating_sub(x));
                let h = h.min(f.height.saturating_sub(y));
                if w > 0 && h > 0 {
                    let rgba = f.sub_rgba(x, y, w, h);
                    let _ = ev.send(RdEvent::Frame { x, y, w, h, rgba });
                }
            }
        }
        let _ = fb_format;
    }
}
