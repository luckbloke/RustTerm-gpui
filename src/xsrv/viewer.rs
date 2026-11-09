//! 帧导出：把服务端的根窗口像素变成 UI 能直接上纹理的 RGBA8 脏矩形。
//!
//! 与工程里远程桌面（RDP/VNC）的帧契约完全一致（`remote::RdEvent::Frame`）：
//! 脏矩形 + RGBA8 行优先紧密排列。gpui 端没有 webview 中转，直接把 RGBA
//! 写进 `RenderImage`，所以不再走 JPEG（省一次编码 + 解码）。
//!
//! 两个关键约束（与主工程一致）：
//!
//! - **按需编码**：服务端每次 `compose()` 都会自增 `frame_seq`，序号没变就不做
//!   任何事。X 应用空闲时不会白白烧 CPU 转换像素。
//! - **脏矩形而不是整屏**：整屏 1280x800 的 RGBA 每帧约 4MB。这里拿上一帧做
//!   逐行比较求包围盒，只有变化面积超过阈值（大幅重绘、窗口开关）才退化成整屏。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::server::ServerShared;

/// 相邻两帧之间的最小间隔。16ms ≈ 60fps——UI 交互响应优先于带宽。
/// （旧的 33ms=30fps 让滚动/悬停的视觉延迟翻倍，表现为"只有动鼠标才慢慢展开"。）
const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);
/// 变化面积超过屏幕的这个比例时直接整屏发送——脏矩形本身也要一次全量比较，
/// 面积这么大时拆成矩形反而更亏。
const FULL_FRAME_RATIO: f32 = 0.6;

/// 一帧里要更新的脏矩形。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl FrameRect {
    fn full(width: usize, height: usize) -> Self {
        Self {
            x: 0,
            y: 0,
            width: width as u16,
            height: height as u16,
        }
    }

    fn area(self) -> usize {
        usize::from(self.width) * usize::from(self.height)
    }
}

/// 一帧要交给 UI 的像素：脏矩形 + RGBA8（行优先，`w * h * 4` 字节）。
#[derive(Debug, Clone)]
pub struct XFrame {
    pub rect: FrameRect,
    pub rgba: Vec<u8>,
}

/// UI 侧注册的帧接收端。
pub type FrameSink = tokio::sync::mpsc::UnboundedSender<XFrame>;

/// 帧泵：持有 UI 的帧接收端，按需把屏幕导出成帧。
pub struct FramePump {
    shared: Arc<ServerShared>,
    sink: Mutex<Option<FrameSink>>,
    /// 停止标志：会话关闭时置位，循环自然退出。
    stopped: AtomicBool,
}

impl FramePump {
    pub fn new(shared: Arc<ServerShared>) -> Arc<Self> {
        Arc::new(Self {
            shared,
            sink: Mutex::new(None),
            stopped: AtomicBool::new(false),
        })
    }

    /// 挂上（或替换）帧接收端。UI 切标签回来时会重新 attach。
    pub fn attach(&self, sink: FrameSink) {
        *self.sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
        self.stopped.store(false, Ordering::Release);
    }

    pub fn detach(&self) {
        *self.sink.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.detach();
    }

    fn has_viewer(&self) -> bool {
        self.sink.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    fn send(&self, frame: XFrame) {
        crate::xsrv::counters::FRAMES.fetch_add(1, Ordering::Relaxed);
        crate::xsrv::counters::FRAME_BYTES
            .fetch_add(frame.rgba.len() as u64, Ordering::Relaxed);
        let guard = self.sink.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sink) = guard.as_ref() {
            if sink.send(frame).is_err() {
                // UI 侧已关：丢掉接收端，避免后面每帧都失败一遍
                drop(guard);
                self.detach();
            }
        }
    }

    /// 启动帧循环。返回的 `JoinHandle` 在 `stop()` 后自行结束。
    ///
    /// NyaTerm: 帧的对比与抠取全部在 `ServerState::take_frame_if_changed`
    /// 内完成——空闲时零拷贝，画面变化时只处理脏区提示范围内的差异。
    /// （旧实现每 33ms 做一次整屏快照 + 整屏比较 + 基准帧拷贝，1080p 下
    /// 约 750MB/s 内存带宽，把一个核吃满，终端打字都被拖成慢镜头。）
    pub fn spawn_loop(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let pump = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(FRAME_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                ticker.tick().await;
                if pump.stopped.load(Ordering::Acquire) {
                    break;
                }
                if !pump.has_viewer() {
                    // 没有观众时作废基准帧，下次 attach 一定走整屏
                    pump.shared.state().invalidate_sent_frame();
                    continue;
                }

                let Some((seq, x, y, width, height, pixels)) =
                    pump.shared.state().take_frame_if_changed()
                else {
                    continue;
                };
                let frame = encode_frame(&pixels, usize::from(width), usize::from(height), seq, x, y);
                if let Some(frame) = frame {
                    pump.send(frame);
                }
            }
        })
    }
}

/// 求两帧之间的脏矩形（包围盒）。完全相同返回 `None`。
pub fn dirty_rect(
    previous: &[u32],
    current: &[u32],
    width: usize,
    height: usize,
) -> Option<FrameRect> {
    if previous.len() != current.len() || width == 0 || height == 0 {
        return None;
    }
    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0;
    let mut max_y = 0;
    let mut changed = false;

    for y in 0..height {
        let start = y * width;
        let row = &current[start..start + width];
        let old = &previous[start..start + width];
        if row == old {
            continue;
        }
        changed = true;
        min_y = min_y.min(y);
        max_y = max_y.max(y);
        // 整行 memcmp 相等就跳过，只有不同的行才逐列找边界
        let first = row.iter().zip(old).position(|(a, b)| a != b)?;
        let last = row.iter().zip(old).rposition(|(a, b)| a != b)?;
        min_x = min_x.min(first);
        max_x = max_x.max(last);
    }

    if !changed {
        return None;
    }
    Some(FrameRect {
        x: min_x as u16,
        y: min_y as u16,
        width: (max_x - min_x + 1) as u16,
        height: (max_y - min_y + 1) as u16,
    })
}

/// 矩形本地像素 → RGBA8（`0x00RRGGBB` 的服务端像素 → R/G/B/A 字节）。
///
/// NyaTerm: 逐字节 `push` 在 1280x800 帧上是 4M 次调用，实测占帧编码的
/// 大头。改为按 8 像素一块批量展开（`extend_from_slice` 走 memcpy 路径），
/// 再补齐尾部——同样的语义，快一个数量级。
fn encode_frame(
    pixels: &[u32],
    width: usize,
    height: usize,
    seq: u64,
    x: u16,
    y: u16,
) -> Option<XFrame> {
    let _ = seq;
    let w = width;
    let h = height;
    if w == 0 || h == 0 || pixels.len() < w * h {
        return None;
    }

    let mut rgba = vec![0u8; w * h * 4];
    for row in 0..h {
        let start = row * w;
        let dst = row * w * 4;
        for (i, &pixel) in pixels[start..start + w].iter().enumerate() {
            rgba[dst + i * 4] = (pixel >> 16) as u8; // R
            rgba[dst + i * 4 + 1] = (pixel >> 8) as u8; // G
            rgba[dst + i * 4 + 2] = pixel as u8; // B
            rgba[dst + i * 4 + 3] = 0xff; // A
        }
    }

    Some(XFrame {
        rect: FrameRect {
            x,
            y,
            width: w as u16,
            height: h as u16,
        },
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_frames_have_no_dirty_rect() {
        let frame = vec![0u32; 16 * 16];
        assert_eq!(dirty_rect(&frame, &frame, 16, 16), None);
    }

    #[test]
    fn dirty_rect_is_bounding_box_of_changes() {
        let old = vec![0u32; 16 * 16];
        let mut new = old.clone();
        new[5 * 16 + 3] = 0xff0000;
        new[9 * 16 + 7] = 0x00ff00;
        let rect = dirty_rect(&old, &new, 16, 16).expect("change should be detected");
        assert_eq!(rect, FrameRect { x: 3, y: 5, width: 5, height: 5 });
    }

    #[test]
    fn mismatched_sizes_yield_no_dirty_rect() {
        let old = vec![0u32; 4];
        let new = vec![0u32; 8];
        assert_eq!(dirty_rect(&old, &new, 2, 2), None);
    }

    /// 脏矩形帧必须是 RGBA8，且与请求的矩形自洽。
    #[test]
    fn frame_is_rgba_and_matches_rect() {
        let width = 8usize;
        let height = 4usize;
        // 0x00RRGGBB：R=0x11 G=0x22 B=0x33
        let rect = (2u16, 1u16, 4usize, 2usize); // (x, y, w, h)
        // take_frame_if_changed 返回的是矩形本地的像素
        let mut pixels = vec![0u32; width * height];
        for row in 0..rect.3 {
            for col in 0..rect.2 {
                pixels[(row + rect.1 as usize) * width + rect.0 as usize + col] = 0x0011_2233;
            }
        }
        let local: Vec<u32> = (0..rect.3)
            .flat_map(|row| {
                let start = (row + rect.1 as usize) * width + rect.0 as usize;
                pixels[start..start + rect.2].to_vec()
            })
            .collect();

        let frame = encode_frame(&local, rect.2, rect.3, 1, rect.0, rect.1).expect("应该编码出一帧");

        assert_eq!(frame.rect, FrameRect { x: rect.0, y: rect.1, width: rect.2 as u16, height: rect.3 as u16 });
        assert_eq!(frame.rgba.len(), 4 * 2 * 4, "w*h*4 字节");
        // 第一像素的 RGBA
        assert_eq!(&frame.rgba[0..4], &[0x11, 0x22, 0x33, 0xff]);
    }
}
