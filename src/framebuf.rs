//! 远程桌面帧缓冲：把后端推来的「脏矩形」累积成整帧，供 GPU 上传贴图。
//!
//! 单独抽成模块有两个原因：
//! 1. **纯逻辑，不依赖 gpui**，可以在探针二进制里独立回归测试；
//! 2. 历史崩溃点集中在这里 —— 主缓冲曾被 `std::mem::take` 掏空，
//!    导致第二帧写入越界 panic（RDP / VNC / SPICE 三个协议通杀)。
//!
//! 数据流向：
//! ```text
//! 后端 RdEvent::Frame{x,y,w,h,rgba}
//!        │  rgba 是 RGBA8、行跨度 = w*4 的紧密排列
//!        ▼
//!   FrameBuffer.rgba   ← 主缓冲（RGBA），一直保留，接收后续脏区
//!        │  转换成 BGRA（GPUI 渲染器字节序）
//!        ▼
//!   FrameBuffer.bgra   ← 上传纹理用
//! ```

/// 单帧缓冲：RGBA 主缓冲 + BGRA 纹理缓冲。
#[derive(Default)]
pub struct FrameBuffer {
    /// 帧宽（像素）
    pub width: u32,
    /// 帧高（像素）
    pub height: u32,
    /// 主缓冲（RGBA8），后端脏区直接写在这里
    pub rgba: Vec<u8>,
    /// 纹理缓冲（BGRA8），只在需要上传时填充
    pub bgra: Vec<u8>,
    /// 主缓冲自上次取纹理后是否被改动
    dirty: bool,
}

/// 尺寸上限：超过这个值直接拒绝，避免畸形包导致巨额分配。
const MAX_DIM: u32 = 8192;

impl FrameBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 调整帧尺寸。返回 true 表示尺寸发生了变化。
    ///
    /// 尺寸变化时主缓冲会被清零（旧画面不再有效），纹理缓冲同步重建。
    pub fn resize(&mut self, width: u32, height: u32) -> bool {
        if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
            return false;
        }
        if self.width == width && self.height == height && !self.rgba.is_empty() {
            return false;
        }
        self.width = width;
        self.height = height;
        let need = width as usize * height as usize * 4;
        self.rgba = vec![0u8; need];
        self.bgra = vec![0u8; need];
        self.dirty = true;
        true
    }

    /// 写入一个脏矩形。返回 true 表示确实写进去了。
    ///
    /// 防御要点（都是历史崩溃源）：
    /// - 脏区完全越界 → 直接丢弃，不做任何切片；
    /// - 数据长度不足 → 丢弃（不能按 UI 尺寸去猜后端行跨度）；
    /// - 行跨度按**后端自己的 w** 计算，跟 UI 侧裁剪后的宽度无关。
    pub fn put(&mut self, x: u32, y: u32, w: u32, h: u32, data: &[u8]) -> bool {
        if self.width == 0 || self.height == 0 || self.rgba.is_empty() {
            return false;
        }
        if x >= self.width || y >= self.height {
            return false;
        }
        let bw = w as usize;
        let bh = h as usize;
        let need = match bw.checked_mul(4).and_then(|v| v.checked_mul(bh)) {
            Some(n) => n,
            None => return false,
        };
        if data.len() < need {
            return false;
        }
        let cw = bw.min(self.width as usize - x as usize);
        let ch = bh.min(self.height as usize - y as usize);
        if cw == 0 || ch == 0 {
            return false;
        }
        let stride = self.width as usize * 4;
        for row in 0..ch {
            let s = row * bw * 4;
            let d = (y as usize + row) * stride + x as usize * 4;
            if d + cw * 4 > self.rgba.len() {
                break;
            }
            self.rgba[d..d + cw * 4].copy_from_slice(&data[s..s + cw * 4]);
        }
        self.dirty = true;
        true
    }

    /// 取出可用于上传纹理的 BGRA 数据（主缓冲保持不变）。
    ///
    /// 没有新内容时返回 None。
    pub fn take_texture(&mut self) -> Option<Vec<u8>> {
        if !self.dirty || self.width == 0 || self.height == 0 {
            return None;
        }
        self.dirty = false;
        let need = self.width as usize * self.height as usize * 4;
        if self.rgba.len() != need {
            // 尺寸与缓冲对不上：重置，等下一帧重新填满
            self.rgba = vec![0u8; need];
            self.bgra = vec![0u8; need];
            return None;
        }
        if self.bgra.len() != need {
            self.bgra = vec![0u8; need];
        }
        for (dst, src) in self.bgra.chunks_exact_mut(4).zip(self.rgba.chunks_exact(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = src[3];
        }
        Some(std::mem::take(&mut self.bgra))
    }

    /// 清空所有内容（断开连接时用）。
    pub fn clear(&mut self) {
        self.width = 0;
        self.height = 0;
        self.rgba.clear();
        self.bgra.clear();
        self.dirty = false;
    }
}
