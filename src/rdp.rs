//! 远程桌面标签页：VNC / RDP / SPICE 的真实连接与渲染（GPUI 版）。
//!
//! - 后端：`crate::remote`（每会话一个线程 + 事件通道）
//! - 渲染：整帧 BGRA 缓冲 + `RenderImage` 贴图（canvas 元素）
//! - 输入：键盘（扫描码 + keysym，gpui 键名映射）、鼠标、滚轮转发到远端

use crate::remote::{self, RdCommand, RdEvent, RdParams, RdProtocol, RdSession, RdState, TransferUpdate};
use crate::ui_icons::ic;
use crate::widgets::password_field;

use gpui_kit::assets::IconName;
use gpui_kit::base::component_traits::Selectable as _;
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::theme::Theme;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RdTabProtocol {
    Vnc,
    Rdp,
    Spice,
    /// 内置 X11 服务：X server 就跑在本进程里，没有"远端主机"。
    /// 不出现在「新建远程桌面」的协议选择里，由「X 服务」菜单直接打开。
    X11,
}

impl RdTabProtocol {
    pub fn label(self) -> &'static str {
        match self {
            RdTabProtocol::Vnc => "VNC (RFB)",
            RdTabProtocol::Rdp => "RDP (Microsoft)",
            RdTabProtocol::Spice => "SPICE (QEMU)",
            RdTabProtocol::X11 => "X11 (内置服务)",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            RdTabProtocol::Vnc => 5900,
            RdTabProtocol::Rdp => 3389,
            RdTabProtocol::Spice => 5930,
            RdTabProtocol::X11 => 0,
        }
    }

    fn to_backend(self) -> RdProtocol {
        match self {
            RdTabProtocol::Vnc => RdProtocol::Vnc,
            RdTabProtocol::Rdp => RdProtocol::Rdp,
            RdTabProtocol::Spice => RdProtocol::Spice,
            RdTabProtocol::X11 => RdProtocol::X11,
        }
    }

    pub fn all() -> [RdTabProtocol; 3] {
        [RdTabProtocol::Vnc, RdTabProtocol::Rdp, RdTabProtocol::Spice]
    }

    /// 存盘用的稳定键名（`label()` 是给人看的，会随文案变，不能拿来做标识）。
    pub fn key(self) -> &'static str {
        match self {
            RdTabProtocol::Vnc => "vnc",
            RdTabProtocol::Rdp => "rdp",
            RdTabProtocol::Spice => "spice",
            RdTabProtocol::X11 => "x11",
        }
    }

    /// 从存盘键名还原；老版本/未知值返回 `None`（快照里那一项直接跳过）。
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "vnc" => Some(RdTabProtocol::Vnc),
            "rdp" => Some(RdTabProtocol::Rdp),
            "spice" => Some(RdTabProtocol::Spice),
            "x11" => Some(RdTabProtocol::X11),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RdpConnectionProfile {
    pub name: String,
    pub protocol: RdTabProtocol,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub domain: String,
    pub color_depth: u8,
    /// 共享给远端的本机目录（仅 RDP；空 = 不共享）。
    ///
    /// RDP 通过 RDPDR 通道把它挂成远端「我的电脑」里的一个盘符，
    /// 往远端拷贝文件就是普通的资源管理器拖拽。
    pub share_dir: String,
    /// 旧版服务器兼容模式（Windows Server 2008 / 2008 R2 等）。
    ///
    /// 勾选后连接时跳过 NLA（CredSSP），只用 TLS 安全层。
    /// 即使不勾选，NLA 握手失败时后端也会自动回退一次。
    pub legacy_compat: bool,
    /// 该档案来自哪条保存的会话（`None` = 「新建远程桌面」这种临时连接）。
    ///
    /// 用来把标签页里改的共享目录写回 `sessions.json`：
    /// 表单是本次运行的临时状态，会话才是跨重启的真相。
    pub session_id: Option<String>,
}

impl Default for RdpConnectionProfile {
    fn default() -> Self {
        Self {
            name: String::new(),
            protocol: RdTabProtocol::Vnc,
            host: String::new(),
            port: 5900,
            username: String::new(),
            password: String::new(),
            domain: String::new(),
            color_depth: 32,
            share_dir: String::new(),
            legacy_compat: false,
            session_id: None,
        }
    }
}

/// 全局共享的连接档案表（远程桌面标签之间共用）。
#[derive(Clone)]
pub struct RdpProfiles {
    pub profiles: Vec<RdpConnectionProfile>,
}

impl Default for RdpProfiles {
    fn default() -> Self {
        Self {
            profiles: Vec::new(),
        }
    }
}

impl RdpProfiles {
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.profiles.iter().position(|p| p.name == name)
    }

    pub fn ensure(&mut self, profile: RdpConnectionProfile) -> usize {
        if let Some(i) = self.index_of(&profile.name) {
            self.profiles[i] = profile;
            i
        } else {
            self.profiles.push(profile);
            self.profiles.len() - 1
        }
    }
}

/// 远程桌面画面区域（canvas prepaint 时写入，鼠标/键盘输入换算用）。
pub struct RdArea(pub Bounds<Pixels>);
impl Global for RdArea {}

/// 画面在窗口里的实际绘制矩形：等比缩放（fit）或固定倍率（zoom）后居中。
///
/// **绘制和指针换算必须调用同一个函数**。之前绘制按比例缩放居中，
/// 而指针换算却用整个 area 归一化 —— 两者在宽高比不一致时偏差很大，
/// 表现为「点左边实际落在画面中间」。
fn view_rect_of(
    area: Bounds<Pixels>,
    fb_w: u32,
    fb_h: u32,
    fit: bool,
    zoom: f32,
) -> Bounds<Pixels> {
    let iw = fb_w.max(1) as f32;
    let ih = fb_h.max(1) as f32;
    let aw = area.size.width.as_f32();
    let ah = area.size.height.as_f32();
    if aw <= 0.0 || ah <= 0.0 {
        return area;
    }
    // fit：等比缩放到「完整放入」窗口（不放大）；否则按 zoom 固定倍率
    let scale = if fit {
        (aw / iw).min(ah / ih).min(1.0)
    } else {
        zoom.max(0.1)
    };
    let dw = iw * scale;
    let dh = ih * scale;
    Bounds::new(
        point(
            px(area.origin.x.as_f32() + (aw - dw) / 2.0),
            px(area.origin.y.as_f32() + (ah - dh) / 2.0),
        ),
        size(px(dw), px(dh)),
    )
}

/// 字节数 → 人看得懂的大小。
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

/// 连接表单输入实体。
#[derive(Clone)]
pub struct RdFormInputs {
    pub host: Entity<InputState>,
    pub port: Entity<InputState>,
    pub username: Entity<InputState>,
    pub password: Entity<InputState>,
    pub domain: Entity<InputState>,
    pub width: Entity<InputState>,
    pub height: Entity<InputState>,
    /// 共享目录（仅 RDP 显示）
    pub share: Entity<InputState>,
}

pub struct RemoteDesktopTab {
    pub profile: RdpConnectionProfile,
    pub state: RdState,
    /// 最近一次失败原因（后端会先发 Error 再发 Disconnected，
    /// 后者不能把错误信息冲掉，否则用户只看到「连不上」没有原因）
    pub last_error: Option<String>,
    /// 连接过程中的非致命提示（例如 NLA 失败后自动改用 TLS 兼容模式）。
    pub notice: Option<String>,
    /// 帧缓冲（RGBA 主缓冲 + BGRA 纹理缓冲）——逻辑见 framebuf 模块
    pub fb: crate::framebuf::FrameBuffer,
    /// 当前可绘制的整帧图像（BGRA，GPUI 渲染器格式）
    pub image: Option<Arc<RenderImage>>,
    pub fit: bool,
    /// 非自适应时的缩放
    pub zoom: f32,
    pub fps: f32,
    pub clipboard_sync: bool,
    /// 捕获键盘（关闭后键盘事件不再发往远端）
    pub capture_keys: bool,
    session: Option<RdSession>,
    /// 上一帧的修饰键状态（shift, ctrl, alt）
    prev_mods: (bool, bool, bool),
    /// 最近一次通过 Sync 事件告知远端的锁定键状态（scroll, num, caps）。
    ///
    /// `None` = 未同步过（新会话）。只在锁定键状态变化、且没有按住修饰键时
    /// 才发送 Sync——按住 Shift 等修饰键期间发 Sync 会让部分服务器重置修饰
    /// 键状态（表现为"长按 Shift 输入失效"），必须避开。
    last_sync_locks: Option<(bool, bool, bool)>,
    /// 当前按下的鼠标键位掩码
    buttons: u8,
    last_pointer: Option<(u16, u16)>,
    frames: u32,
    fps_t: std::time::Instant,
    /// VNC：最近一次对远端生效的本地 CapsLock 状态（keysym 切换检测用）。
    ///
    /// gpui 不投递 CapsLock 按键，只能靠每次普通按键时对比本地状态：发现
    /// 变化就发一次 Caps_Lock keysym 按下+抬起，让远端跟着切换。
    caps_sent: bool,
    /// 后端最初报上来的尺寸（重连时沿用）
    desired: (u16, u16),
    /// 键盘焦点（首次渲染时创建）
    pub focus: Option<FocusHandle>,
    /// 连接表单输入
    pub form: Option<RdFormInputs>,
    /// 远端光标位图（RGBA8）+ 热点。
    ///
    /// 服务器下发的光标形状。客户端自己绘制它，**并隐藏本地系统光标**，
    /// 否则本地箭头和远端光标同时可见（俗称「两个鼠标」）。
    cursor: Option<RemoteCursor>,
    /// 远端光标位置（远端桌面像素坐标）
    cursor_pos: (u16, u16),
    /// 远端是否要求隐藏光标
    cursor_hidden: bool,
    /// 最近一次同步到远端的本机剪贴板内容（去重：不变就不重发）。
    last_local_clip: Option<String>,
    /// 最近一次由远端写进本机剪贴板的内容（防止「写回 → 读到自己 → 再发回远端」的回环）。
    last_clip_write: Option<String>,
    /// 本机剪贴板轮询节流（VNC 没有剪贴板变化通知，只能定时查）
    clip_check_t: std::time::Instant,
    /// 剪贴板文件传输的最近一条状态（进度 / 结果），显示在工具条右侧。
    transfer: Option<TransferUi>,
}

/// 剪贴板传文件的状态条内容。
struct TransferUi {
    text: String,
    /// 传输已结束（成功或失败）：过一会儿自动消失，别一直占着工具条
    finished: bool,
    /// 失败时用红色显示
    failed: bool,
    at: std::time::Instant,
}

/// 远端光标：位图 + 热点偏移。
struct RemoteCursor {
    /// 宽度/高度（远端像素）
    width: u16,
    height: u16,
    /// 热点：位图内哪个像素对应远端指针位置
    hotspot_x: u16,
    hotspot_y: u16,
    /// RGBA8 位图（行优先紧密排列）
    rgba: Vec<u8>,
    /// GPUI 贴图（上传后缓存，避免每帧重建）
    image: Option<Arc<RenderImage>>,
}

impl RemoteDesktopTab {
    pub fn new(profile: RdpConnectionProfile) -> Self {
        Self {
            desired: (1280, 800),
            profile,
            state: RdState::Disconnected,
            last_error: None,
            notice: None,
            fb: crate::framebuf::FrameBuffer::new(),
            image: None,
            fit: true,
            zoom: 1.0,
            fps: 0.0,
            clipboard_sync: true,
            capture_keys: true,
            session: None,
            prev_mods: (false, false, false),
            caps_sent: crate::win32::caps_lock_on(),
            last_sync_locks: None,
            buttons: 0,
            last_pointer: None,
            frames: 0,
            fps_t: std::time::Instant::now(),
            focus: None,
            form: None,
            cursor: None,
            cursor_pos: (0, 0),
            cursor_hidden: false,
            last_local_clip: None,
            last_clip_write: None,
            clip_check_t: std::time::Instant::now(),
            transfer: None,
        }
    }

    pub fn title(&self) -> String {
        if self.profile.name.is_empty() {
            format!("{}:{}", self.profile.host, self.profile.port)
        } else {
            self.profile.name.clone()
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self.state, RdState::Active)
    }

    pub fn is_connecting(&self) -> bool {
        matches!(self.state, RdState::Connecting | RdState::Authenticating)
    }

    pub fn error(&self) -> Option<&str> {
        if let Some(e) = &self.last_error {
            return Some(e.as_str());
        }
        match &self.state {
            RdState::Failed(e) => Some(e.as_str()),
            _ => None,
        }
    }

    /// 建立连接（已有会话先断开）。
    pub fn connect(&mut self) {
        if self.is_connecting() || self.is_active() {
            return;
        }
        // 内置 X11 服务没有"远端主机"：X server 就在本进程里
        if self.profile.protocol != RdTabProtocol::X11 && self.profile.host.trim().is_empty() {
            self.state = RdState::Failed("请先填写主机地址".into());
            return;
        }
        self.disconnect();
        self.image = None;
        self.fb.clear();

        let params = RdParams {
            protocol: self.profile.protocol.to_backend(),
            host: self.profile.host.trim().to_string(),
            port: self.profile.port,
            username: self.profile.username.clone(),
            password: self.profile.password.clone(),
            domain: self.profile.domain.clone(),
            width: self.desired.0,
            height: self.desired.1,
            color_depth: self.profile.color_depth,
            // 剪贴板通道在建连时协商：中途切换开关要重连才生效
            clipboard: self.clipboard_sync,
            share_dir: if self.profile.protocol == RdTabProtocol::Rdp {
                let d = self.profile.share_dir.trim();
                if d.is_empty() {
                    None
                } else {
                    Some(d.to_string())
                }
            } else {
                None
            },
            legacy_compat: self.profile.legacy_compat,
        };
        self.state = RdState::Connecting;
        self.notice = None;
        self.last_error = None;
        self.session = Some(remote::spawn(params));
        self.prev_mods = (false, false, false);
        self.buttons = 0;
        self.last_pointer = None;
        self.fps_t = std::time::Instant::now();
        self.frames = 0;
        // 重连时清掉上一条会话残留的光标状态
        self.cursor = None;
        self.cursor_pos = (0, 0);
        self.cursor_hidden = false;
        // 剪贴板去重状态也随会话重置
        self.last_local_clip = None;
        self.last_clip_write = None;
        // 锁定键同步基准也随会话重置：新会话首个无修饰按键会重新对齐
        self.last_sync_locks = None;
    }

    pub fn disconnect(&mut self) {
        if let Some(mut s) = self.session.take() {
            s.close();
        }
        self.last_error = None;
        self.notice = None;
        self.state = RdState::Disconnected;
        self.image = None;
        self.fb.clear();
        self.prev_mods = (false, false, false);
        self.buttons = 0;
        self.cursor = None;
        self.cursor_pos = (0, 0);
        self.cursor_hidden = false;
    }

    /// 消费后端事件并更新整帧缓冲。返回是否有新画面。
    ///
    /// 剪贴板事件在这里单独处理（写本机剪贴板需要 `cx`），
    /// 其余都转给 [`Self::apply_event`]。
    pub fn poll(&mut self, cx: &mut App) -> bool {
        let mut got_frame = false;

        let mut events: Vec<RdEvent> = Vec::new();
        if let Some(s) = &self.session {
            while let Ok(ev) = s.events.try_recv() {
                events.push(ev);
            }
        }

        for ev in events {
            match ev {
                RdEvent::Clipboard(text) => {
                    // 远端 → 本机：写系统剪贴板。记住内容防止回环
                    //（本机剪贴板轮询会读到这次写入，不能当成「用户复制了」再发回去）。
                    if self.clipboard_sync && self.last_clip_write.as_deref() != Some(text.as_str())
                    {
                        crate::term::copy_to_clipboard(cx, text.clone());
                        self.last_clip_write = Some(text);
                        self.last_local_clip = None;
                    }
                }
                other => {
                    got_frame |= self.apply_event(other);
                }
            }
        }

        // ---- 本机 → 远端 ----
        // RDP 的 CLIPRDR + WinClipboard 在后端自行双向同步（见 remote/rdp.rs）；
        // VNC 没有变化通知，只能节流轮询本机剪贴板。
        if self.clipboard_sync
            && self.is_active()
            && self.profile.protocol == RdTabProtocol::Vnc
            && self.clip_check_t.elapsed().as_millis() >= 500
        {
            self.clip_check_t = std::time::Instant::now();
            if let Some(text) = crate::term::read_clipboard(cx) {
                if !text.is_empty()
                    && self.last_clip_write.as_deref() != Some(text.as_str())
                    && self.last_local_clip.as_deref() != Some(text.as_str())
                {
                    self.send(RdCommand::Clipboard(text.clone()));
                    self.last_local_clip = Some(text);
                }
            }
        }

        // ---- 上传纹理：RGBA 主缓冲 → BGRA 副本 ----
        // 主缓冲始终保留（还要接后续脏区），只有纹理副本被取走。
        if let Some(bgra) = self.fb.take_texture() {
            let (w, h) = (self.fb.width, self.fb.height);
            if let Some(img) = image::RgbaImage::from_raw(w, h, bgra) {
                self.image = Some(Arc::new(RenderImage::new(smallvec::smallvec![
                    image::Frame::new(img),
                ])));
            }
        }

        if got_frame && self.fps_t.elapsed().as_secs_f32() >= 0.5 {
            self.fps = self.frames as f32 / self.fps_t.elapsed().as_secs_f32();
            self.frames = 0;
            self.fps_t = std::time::Instant::now();
        }

        // NyaTerm: 有新画面 → 让 app 的续帧循环立刻再画一帧。空闲时那边会降到
        // ~10fps 兜底，不再无条件 60fps 重绘（那会把主线程占满）。
        if got_frame {
            crate::xsrv::counters::mark_ui_dirty();
        }

        got_frame
    }

    /// 处理单个后端事件。返回是否带来了新画面。
    ///
    /// 抽成独立函数是为了能脱离网络直接做单元测试（多帧写入、
    /// 分辨率变化、越界脏区这些历史崩溃点都在这里）。
    pub fn apply_event(&mut self, ev: RdEvent) -> bool {
        let mut got_frame = false;
        match ev {
            // 服务器告知远端锁定键状态（`TS_SET_KEYBOARD_INDICATORS_PDU`）。
            // 大小写对齐不依赖它：RDP 每次按键都发绝对设置的 Sync 事件
            // （见 send_key），这里仅为将来显示远端 LED 状态而保留管线。
            RdEvent::KeyboardIndicators(_) => {}
            RdEvent::State(st) => match st {
                RdState::Disconnected => {
                    // 会话线程在报错后还会再发一次 Disconnected：
                    // 有错误信息时保留错误，别把提示冲掉
                    if self.last_error.is_none() {
                        self.state = RdState::Disconnected;
                    }
                    self.session = None;
                }
                other => {
                    // 连上以后不再显示「正在回退」之类的过程提示
                    if other == RdState::Active {
                        self.notice = None;
                    }
                    self.state = other;
                }
            },
            RdEvent::Resized { width, height } => {
                if !self.fb.resize(width, height) {
                    return false;
                }
                // 旧尺寸的贴图必须作废，否则会被拉伸绘制
                self.image = None;
            }
            RdEvent::Frame { x, y, w, h, rgba } => {
                if !self.fb.put(x, y, w, h, &rgba) {
                    return false;
                }
                self.frames += 1;
                got_frame = true;
            }
            RdEvent::Cursor {
                width,
                height,
                hotspot_x,
                hotspot_y,
                rgba,
            } => {
                // 远端光标形状更新：重建位图并重新上传贴图。
                // image 置 None 强制下一次绘制时重建。
                self.cursor = Some(RemoteCursor {
                    width,
                    height,
                    hotspot_x,
                    hotspot_y,
                    rgba,
                    image: None,
                });
                self.cursor_hidden = false;
            }
            RdEvent::CursorPos { x, y } => {
                self.cursor_pos = (x, y);
                self.cursor_hidden = false;
            }
            RdEvent::CursorHidden => {
                self.cursor_hidden = true;
            }
            RdEvent::Error(msg) => {
                self.last_error = Some(msg.clone());
                self.state = RdState::Failed(msg);
            }
            // 非致命提示：只更新连接中的状态行，不进入失败状态
            RdEvent::Notice(msg) => {
                self.notice = Some(msg);
            }
            // 剪贴板传文件：只更新工具条右侧那一行状态
            RdEvent::Transfer(update) => {
                let (text, finished, failed) = match update {
                    TransferUpdate::Progress { sent, total } => {
                        let text = if total == 0 {
                            format!("剪贴板传文件 {}", human_size(sent))
                        } else {
                            format!(
                                "剪贴板传文件 {:.0}%（{} / {}）",
                                sent as f64 * 100.0 / total as f64,
                                human_size(sent),
                                human_size(total)
                            )
                        };
                        (text, false, false)
                    }
                    TransferUpdate::Done(hint) => (hint, true, false),
                    TransferUpdate::Failed(err) => {
                        (format!("剪贴板传文件失败：{err}"), true, true)
                    }
                };
                self.transfer = Some(TransferUi {
                    text,
                    finished,
                    failed,
                    at: std::time::Instant::now(),
                });
            }
            // 剪贴板事件在 poll() 里已经消费（写系统剪贴板需要 cx），
            // 走到这里说明是测试路径，直接忽略。
            RdEvent::Clipboard(_) => {}
        }
        got_frame
    }

    fn send(&self, cmd: RdCommand) {
        if let Some(s) = &self.session {
            s.send(cmd);
        }
    }

    /// 远端光标贴图（RGBA → GPUI 的 BGRA RenderImage），带缓存。
    ///
    /// 服务器不一定每次都下发新形状，位图没变时复用已有贴图，
    /// 避免每帧重建纹理。
    fn cursor_image(&mut self) -> Option<Arc<RenderImage>> {
        let cur = self.cursor.as_mut()?;
        if let Some(img) = &cur.image {
            return Some(img.clone());
        }
        let (w, h) = (cur.width as u32, cur.height as u32);
        if w == 0 || h == 0 {
            return None;
        }
        // 远端给的是 RGBA8，GPUI 渲染器要 BGRA
        let mut bgra = Vec::with_capacity((w * h * 4) as usize);
        for px in cur.rgba.chunks_exact(4) {
            bgra.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
        let img = image::RgbaImage::from_raw(w, h, bgra)?;
        let rendered = Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(img)]));
        cur.image = Some(rendered.clone());
        Some(rendered)
    }

    /// 按目标状态同步修饰键（Shift/Ctrl/Alt）。
    fn apply_mods(&mut self, target: (bool, bool, bool)) {
        const MODS: [remote::KeyInput; 3] =
            [remote::MOD_SHIFT, remote::MOD_CTRL, remote::MOD_ALT];
        for (idx, proto) in MODS.iter().enumerate() {
            let want = match idx {
                0 => target.0,
                1 => target.1,
                _ => target.2,
            };
            let held = match idx {
                0 => self.prev_mods.0,
                1 => self.prev_mods.1,
                _ => self.prev_mods.2,
            };
            if want == held {
                continue;
            }
            self.send(RdCommand::Key(remote::KeyInput {
                down: want,
                ..*proto
            }));
            match idx {
                0 => self.prev_mods.0 = want,
                1 => self.prev_mods.1 = want,
                _ => self.prev_mods.2 = want,
            }
        }
    }

    // ------------------------------------------------------------------
    // 输入转发（由 app 层事件监听器调用）
    // ------------------------------------------------------------------

    /// 把远端锁定键**绝对设置**为本地状态——但只在必要时发。
    ///
    /// 规则（实测约束，都别动）：
    /// 1. 只在锁定键状态与上次同步值不同时发送（幂等降频）；
    /// 2. **按住任何修饰键时不发送**——实测部分服务器收到修饰键按住期间的
    ///    Sync 事件会重置修饰键状态，表现为"长按 Shift 输入失效"；推迟到
    ///    修饰键释放后的第一个无修饰按键再对齐；
    /// 3. 新会话（`last_sync_locks = None`）的第一个无修饰按键必然触发一次
    ///    对齐——这同时解决了 Win7/2008 进入会话时锁定键状态未知的(entry
    ///    状态)问题。
    ///
    /// CapsLock 键本身 gpui 不投递按键事件（本地 LED 由系统切换），所以本地
    /// 状态只能靠每次按键时读 `GetKeyState` 获得。
    fn sync_lock_keys(&mut self, shift: bool, ctrl: bool, alt: bool) {
        // VNC / SPICE：keysym 自带大小写 / 扫描码由远端 guest 状态解释，
        // CapsLock 经透传切换（见 send_key），无需绝对同步。
        if self.profile.protocol != RdTabProtocol::Rdp {
            return;
        }
        if shift || ctrl || alt {
            // 修饰键按住期间不动锁定键，等释放后再对齐。
            return;
        }
        let local = (
            crate::win32::scroll_lock_on(),
            crate::win32::num_lock_on(),
            crate::win32::caps_lock_on(),
        );
        if self.last_sync_locks == Some(local) {
            return;
        }
        self.send(RdCommand::SyncLockKeys {
            scroll: local.0,
            num: local.1,
            caps: local.2,
        });
        self.last_sync_locks = Some(local);
        crate::xsrv::log_line(format_args!(
            "[key] sync scroll={} num={} caps={}",
            local.0, local.1, local.2
        ));
    }

    fn send_key(&mut self, ks: &Keystroke, down: bool) {
        // NyaTerm diagnostic: 按键轨迹（长按 Shift 失效问题的测量点）。
        crate::xsrv::log_line(format_args!(
            "[key] {} {:?} shift={} ctrl={} alt={} char={:?} prev_shift={}",
            if down { "down" } else { "up" },
            ks.key,
            ks.modifiers.shift,
            ks.modifiers.control,
            ks.modifiers.alt,
            ks.key_char,
            self.prev_mods.0
        ));
        // CapsLock 键本身 gpui 不投递按键事件（本地 LED 由系统切换）；
        // RDP 用 Sync 事件对齐（见下）。VNC / SPICE 是扫描码或 keysym 驱动、
        // 远端 guest 自己维护锁定键状态——本地 CapsLock 状态变化时透传一次
        // Caps_Lock（按下+抬起）让远端跟上；透传的触发点在下面 down 分支里
        // （对比 caps_sent 检测本地状态变化）。
        if ks.key == "capslock" {
            return;
        }
        // 按下事件前按需对齐远端锁定键（只在状态变化且无修饰键时发送）。
        // VNC / SPICE 无需对齐：keysym 自带大小写 / 扫描码由远端 guest 状态解释。
        // VNC 在这里顺带检测本地 CapsLock 状态变化（gpui 不投递 CapsLock 键），
        // 变化时透传一次 Caps_Lock（按下+抬起）让远端跟上——**检测后继续走
        // 普通按键路径**（此前在这里误加了 return，导致 VNC 全部按键丢失）。
        if down
            && matches!(
                self.profile.protocol,
                RdTabProtocol::Vnc | RdTabProtocol::Spice
            )
        {
            // VNC / SPICE：gpui 不投递 CapsLock 键。这里对比本地 LED 状态，
            // 变化时透传一次 Caps_Lock（按下+抬起）让远端跟上——**检测后继续
            // 走普通按键路径**（此前在这里误加了 return，导致 VNC 全部按键丢失）。
            // 同一个 KeyInput 对两个协议都正确：VNC 取 keysym 0xffe5（XK_Caps_Lock），
            // SPICE 取 scancode 0x3a（Linux KEY_CAPSLOCK = 58）。
            let local = crate::win32::caps_lock_on();
            if local != self.caps_sent {
                self.caps_sent = local;
                for d in [true, false] {
                    self.send(RdCommand::Key(remote::KeyInput {
                        scancode: 0x3a,
                        keysym: 0xffe5,
                        ch: None,
                        down: d,
                    }));
                }
            }
        }
        if down && self.profile.protocol == RdTabProtocol::Rdp {
            self.sync_lock_keys(ks.modifiers.shift, ks.modifiers.control, ks.modifiers.alt);
        }

        // NyaTerm: 远端 CapsLock 与本地不一致期间（对齐 Sync 被修饰键按住抑制，
        // 只能等第一个无修饰按键），用 **Shift 补偿**保证字符仍然正确：
        // 期望大写且远端 caps ON → 不发 Shift（caps 自给大写）；
        // 期望小写且远端 caps ON → 反而发 Shift（caps+shift = 小写）。
        // 只对字母生效——数字/符号的大小写不受 CapsLock 影响，照搬本地 Shift。
        let want_upper = ks
            .key_char
            .as_ref()
            .and_then(|s| s.chars().next())
            .map(|ch| ch.is_uppercase());
        let is_letter = ks.key.chars().next().is_some_and(|ch| ch.is_alphabetic());
        let remote_caps = self.last_sync_locks.map(|(_, _, caps)| caps);
        let shift_for_scancode = match (want_upper, remote_caps, is_letter) {
            (Some(want), Some(remote), true) => want != remote,
            _ => ks.modifiers.shift,
        };
        self.apply_mods((
            shift_for_scancode,
            ks.modifiers.control,
            ks.modifiers.alt,
        ));
        match remote::map_key(&ks.key) {
            Some((scancode, keysym)) => {
                // NyaTerm: VNC 的 keysym 自带大小写——直接按本地输出的字符构造
                //（key_char 已反映 caps+shift）。ASCII 字符的 keysym 就是码点：
                //'A'=0x41、'a'=0x61、'!'=0x21。此前用 printable_keysym 只看 shift
                //不改 caps，本地大写锁开启时发出去的全是小写 keysym。
                let keysym = if self.profile.protocol == RdTabProtocol::Vnc {
                    ks.key_char
                        .as_ref()
                        .and_then(|s| s.chars().next())
                        .filter(|c| c.is_ascii())
                        .map(|c| c as u32)
                        .unwrap_or_else(|| remote::printable_keysym(&ks.key, shift_for_scancode).unwrap_or(keysym))
                } else {
                    remote::printable_keysym(&ks.key, shift_for_scancode).unwrap_or(keysym)
                };
                crate::xsrv::log_line(format_args!(
                    "[key]   -> scancode={scancode:#04x} keysym={keysym:#x} shift={shift_for_scancode}"
                ));
                self.send(RdCommand::Key(remote::KeyInput {
                    scancode,
                    keysym,
                    ch: None,
                    down,
                }));
            }
            None => {
                // 没有物理键映射：可打印字符走 Unicode 通道
                if down {
                    if let Some(ch) = ks.key_char.as_ref().and_then(|s| s.chars().next()) {
                        let key = remote::KeyInput {
                            scancode: 0,
                            // X11 用 0x01000000 + 码点 表示 Unicode 键
                            keysym: 0x0100_0000 + ch as u32,
                            ch: Some(ch),
                            down: true,
                        };
                        self.send(RdCommand::Key(key));
                        // 补一个抬起，否则远端会认为这个键一直按着
                        self.send(RdCommand::Key(remote::KeyInput {
                            down: false,
                            ..key
                        }));
                    }
                }
            }
        }
    }

    pub fn on_key_down(&mut self, ks: &Keystroke) {
        if self.is_active() && self.capture_keys {
            self.send_key(ks, true);
        }
    }

    pub fn on_key_up(&mut self, ks: &Keystroke) {
        if self.is_active() && self.capture_keys {
            self.send_key(ks, false);
        }
    }

    /// 指针事件（坐标为窗口像素，内部换算到远端像素）。
    fn pointer_at(&mut self, pos: Point<Pixels>, area: Bounds<Pixels>, buttons: u8) {
        // 按「实际绘制矩形」归一化，而不是整个 area ——
        // 绘制是等比缩放后居中的，用整个 area 换算会让点击位置整体偏移。
        let view = view_rect_of(
            area,
            self.fb.width.max(1),
            self.fb.height.max(1),
            self.fit,
            self.zoom,
        );
        if view.size.width.as_f32() <= 0.0 || view.size.height.as_f32() <= 0.0 {
            return;
        }
        let rel_x =
            ((pos.x - view.origin.x).as_f32() / view.size.width.as_f32()).clamp(0.0, 0.999);
        let rel_y =
            ((pos.y - view.origin.y).as_f32() / view.size.height.as_f32()).clamp(0.0, 0.999);
        let rx = (rel_x * self.fb.width as f32) as u16;
        let ry = (rel_y * self.fb.height as f32) as u16;
        self.last_pointer = Some((rx, ry));
        // 远端光标跟着我们的指针走：远端画面里的光标位置 = 我们发过去的位置
        self.cursor_pos = (rx, ry);
        self.send(RdCommand::Pointer {
            x: rx,
            y: ry,
            buttons,
        });
        // 只有在**确实要画远端光标**时才隐藏本地系统光标。
        //
        // 后端没有下发光标形状时（内置 X server 的 X11 会话就不发 Cursor 事件），
        // 本地光标藏掉后画面上没有任何光标；而 Windows 每次鼠标移动都先发
        // WM_SETCURSOR、gpui 会把它恢复成默认箭头——两者来回拉锯，表现就是
        // 用户看到的「鼠标一直闪烁、飘忽不定」。此时保持本地光标可见才是对的。
        if self.cursor.is_some() {
            crate::win32::hide_cursor();
        }
    }

    pub fn on_mouse_move(&mut self, pos: Point<Pixels>, area: Bounds<Pixels>) {
        if !self.is_active() {
            return;
        }
        // 悬停 / 拖拽都持续上报位置（buttons 为当前按下掩码）
        self.pointer_at(pos, area, self.buttons);
    }

    pub fn on_mouse_down(&mut self, button: MouseButton, pos: Point<Pixels>, area: Bounds<Pixels>) {
        if !self.is_active() {
            return;
        }
        let mask = match button {
            MouseButton::Left => 1,
            MouseButton::Middle => 2,
            MouseButton::Right => 4,
            _ => 0,
        };
        self.buttons |= mask;
        self.pointer_at(pos, area, self.buttons);
    }

    pub fn on_mouse_up(&mut self, button: MouseButton, pos: Point<Pixels>, area: Bounds<Pixels>) {
        if !self.is_active() {
            return;
        }
        let mask = match button {
            MouseButton::Left => 1,
            MouseButton::Middle => 2,
            MouseButton::Right => 4,
            _ => 0,
        };
        self.buttons &= !mask;
        self.pointer_at(pos, area, self.buttons);
    }

    pub fn on_wheel(&mut self, delta: ScrollDelta) {
        if !self.is_active() {
            return;
        }
        let dy = match delta {
            ScrollDelta::Pixels(p) => p.y.as_f32(),
            ScrollDelta::Lines(l) => l.y * 20.0,
        };
        if dy.abs() > 0.5 {
            self.send(RdCommand::Wheel {
                dy: if dy > 0.0 { 1 } else { -1 },
            });
        }
    }

    // ------------------------------------------------------------------
    // 渲染
    // ------------------------------------------------------------------

    /// 渲染整个标签页内容（工具条 + 画面/连接表单）。
    pub fn render(
        &mut self,
        window: &mut Window,
        cx: &mut Context<crate::app::RustTermApp>,
    ) -> AnyElement {
        self.poll(cx);

        // 传文件的结果提示留一会儿就撤，避免工具条一直挂着一行旧状态
        if self
            .transfer
            .as_ref()
            .is_some_and(|t| t.finished && t.at.elapsed().as_secs() >= 8)
        {
            self.transfer = None;
        }

        if self.focus.is_none() {
            self.focus = Some(cx.focus_handle());
        }

        let theme = Theme::global(cx).clone();
        let dim = theme.colors.muted_foreground;

        // ---- 顶部工具条 ----
        let mut toolbar = div().h_flex().items_center().gap_2().px_2().py_1();
        match &self.state {
            RdState::Disconnected | RdState::Failed(_) => {
                toolbar = toolbar.child(
                    Button::new("rd-connect")
                        .icon(ic(IconName::Plug))
                        .label("连接")
                        .compact()
                        .on_click(cx.listener(|this, _, _, _| {
                            this.active_rd(|t| t.connect());
                        })),
                );
            }
            RdState::Connecting | RdState::Authenticating => {
                let connecting = self.state == RdState::Connecting;
                let mut status = if connecting { "正在连接…" } else { "正在认证…" }.to_string();
                if let Some(note) = &self.notice {
                    status.push(' ');
                    status.push_str(note);
                }
                toolbar = toolbar
                    .child(div().text_size(px(12.0)).text_color(dim).child(status))
                    .child(
                        Button::new("rd-cancel")
                            .label("取消")
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| {
                                this.active_rd(|t| t.disconnect());
                            })),
                    );
            }
            RdState::Active => {
                toolbar = toolbar.child(
                    Button::new("rd-disconnect")
                        .icon(ic(IconName::PlugZap))
                        .label("断开")
                        .compact()
                        .on_click(cx.listener(|this, _, _, _| {
                            this.active_rd(|t| t.disconnect());
                        })),
                );
            }
        }
        toolbar = toolbar
            .child(
                Checkbox::new("rd-fit")
                    .label("自适应")
                    .checked(self.fit)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.active_rd(|t| t.fit = *checked);
                        cx.notify();
                    })),
            )
            .child(
                Checkbox::new("rd-capture")
                    .label("捕获键盘")
                    .checked(self.capture_keys)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.active_rd(|t| t.capture_keys = *checked);
                        cx.notify();
                    })),
            )
            .child(
                Checkbox::new("rd-clip")
                    .label("剪贴板")
                    .checked(self.clipboard_sync)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        // 会话中切换：通道是建连时协商的，提示重连生效
                        let was_active =
                            this.active_rd_ref().map(|t| t.is_active()).unwrap_or(false);
                        this.active_rd(|t| t.clipboard_sync = *checked);
                        if was_active {
                            this.set_toast(if *checked {
                                "剪贴板同步将在下次连接时生效"
                            } else {
                                "已关闭剪贴板同步（重新连接后完全停止）"
                            });
                        }
                        cx.notify();
                    })),
            )
            .when(self.is_active(), |d| {
                d.child(
                    div()
                        .text_size(px(10.0))
                        .text_color(dim)
                        .child(format!(
                            "{}×{}  {:.0} fps",
                            self.fb.width, self.fb.height, self.fps
                        )),
                )
            });

        // 剪贴板传文件的状态：进度常驻，结果 8 秒后自动消失
        if let Some(t) = &self.transfer {
            toolbar = toolbar.child(
                div()
                    .text_size(px(10.0))
                    .text_color(if t.failed { theme.colors.danger } else { dim })
                    .min_w_0()
                    .text_ellipsis()
                    .child(t.text.clone()),
            );
        }

        // 报错信息只在下方面板里展示（表单自带「连接」按钮可重试），
        // 不再在工具栏重复一条，避免同一错误出现两遍。

        // ---- 画面 / 表单 ----
        let body = if !self.is_active() && self.image.is_none() {
            if self.profile.protocol == RdTabProtocol::X11 {
                // 内置 X11 服务：没有连接表单——X server 随应用运行，
                // SSH 会话启用 X11 转发后，远端 X 程序会自动出现在这里。
                div()
                    .v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme.colors.muted_foreground)
                            .child("X server 已随应用运行。SSH 会话启用 X11 转发后，远端 X 程序会显示在这里。"),
                    )
                    .child(
                        Button::new("x11-connect")
                            .label("连接")
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| {
                                this.active_rd(|t| t.connect());
                            })),
                    )
                    .into_any_element()
            } else {
                self.connect_form(window, cx)
            }
        } else {
            self.screen_area(cx)
        };

        div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .border_b_1()
                    .border_color(theme.colors.border)
                    .child(toolbar),
            )
            .child(body)
            .into_any_element()
    }

    /// 画面区域（canvas 贴图 + 输入监听）。
    fn screen_area(&mut self, cx: &mut Context<crate::app::RustTermApp>) -> AnyElement {
        let image = self.image.clone();
        let focus = self.focus.clone();

        let mut area = div()
            .id("rd-area")
            .flex_1()
            .min_h_0()
            // canvas 自身没有尺寸（Canvas<T> 的默认 Style 为空），必须靠父容器
            // 是 flex 容器 + canvas 带 flex_1 才能拿到真实的绘制区域，
            // 否则画面区域是 0×0 —— 表现为「已连接但一片空白」。
            .v_flex()
            .relative()
            .overflow_hidden()
            .bg(hsla(0.0, 0.0, 0.08, 1.0))
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                let area = cx.global::<RdArea>().0;
                this.active_rd(|t| t.on_mouse_move(ev.position, area));
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                let area = cx.global::<RdArea>().0;
                this.active_rd(|t| t.on_mouse_down(ev.button, ev.position, area));
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Middle, cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                let area = cx.global::<RdArea>().0;
                this.active_rd(|t| t.on_mouse_down(ev.button, ev.position, area));
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Right, cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                let area = cx.global::<RdArea>().0;
                this.active_rd(|t| t.on_mouse_down(ev.button, ev.position, area));
                cx.notify();
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, ev: &MouseUpEvent, _, cx| {
                let area = cx.global::<RdArea>().0;
                this.active_rd(|t| t.on_mouse_up(ev.button, ev.position, area));
                cx.notify();
            }))
            .on_mouse_up(MouseButton::Middle, cx.listener(|this, ev: &MouseUpEvent, _, cx| {
                let area = cx.global::<RdArea>().0;
                this.active_rd(|t| t.on_mouse_up(ev.button, ev.position, area));
                cx.notify();
            }))
            .on_mouse_up(MouseButton::Right, cx.listener(|this, ev: &MouseUpEvent, _, cx| {
                let area = cx.global::<RdArea>().0;
                this.active_rd(|t| t.on_mouse_up(ev.button, ev.position, area));
                cx.notify();
            }))
            .on_scroll(cx.listener(|this, ev: &ScrollEvent, _, cx| {
                this.active_rd(|t| t.on_wheel(ev.delta));
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                this.active_rd(|t| t.on_key_down(&ev.keystroke));
                cx.notify();
            }))
            .on_key_up(cx.listener(|this, ev: &KeyUpEvent, _, cx| {
                this.active_rd(|t| t.on_key_up(&ev.keystroke));
                cx.notify();
            }));

        if let Some(f) = focus {
            area = area
                .track_focus(&f)
                .key_context("RemoteDesktop")
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    if let Some(f) = this.active_rd_ref().and_then(|t| t.focus.clone()) {
                        window.focus(&f, cx);
                    }
                }));
        }

        // 绘制用的矩形和指针换算用的矩形必须完全一致，
        // 否则鼠标位置和看到的内容对不上（点左边实际落在中间）。
        // 两边都调用同一个纯函数 view_rect_of，参数相同 → 结果相同。
        let fb_w = self.fb.width.max(1);
        let fb_h = self.fb.height.max(1);
        let fit = self.fit;
        let zoom = self.zoom;
        let cursor_img = self.cursor_image();
        let cursor_pos = self.cursor_pos;
        let cursor_hotspot = self
            .cursor
            .as_ref()
            .map(|c| (c.hotspot_x, c.hotspot_y))
            .unwrap_or((0, 0));
        let cursor_size = self.cursor.as_ref().map(|c| (c.width, c.height)).unwrap_or((0, 0));
        let cursor_visible = !self.cursor_hidden && cursor_img.is_some();

        area = area.child(canvas(
            move |bounds, _window, cx| {
                cx.set_global(RdArea(bounds));
            },
            move |bounds, (), window, _cx| {
                if let Some(img) = &image {
                    let area = bounds;
                    let sz = img.size(0);
                let iw = sz.width.0.max(1) as f32;
                // 与 pointer_at 用的是同一个函数 → 绘制矩形与命中区域一致
                    let draw_rect = view_rect_of(area, fb_w, fb_h, fit, zoom);
                    let scale = draw_rect.size.width.as_f32() / iw;
                    let _ = window.paint_image(
                        area,
                        draw_rect,
                        Corners::default(),
                        img.clone(),
                        0,
                        false,
                    );

                    // ---- 远端光标叠加 ----
                    // 服务器下发的光标形状由客户端绘制，本地系统光标则隐藏，
                    // 避免同时出现两个鼠标。
                    if cursor_visible {
                        if let Some(cimg) = &cursor_img {
                            let (cw, ch) = cursor_size;
                            if cw > 0 && ch > 0 {
                                let cs = scale;
                                let cx0 = draw_rect.origin.x.as_f32()
                                    + (cursor_pos.0 as f32 - cursor_hotspot.0 as f32) * cs;
                                let cy0 = draw_rect.origin.y.as_f32()
                                    + (cursor_pos.1 as f32 - cursor_hotspot.1 as f32) * cs;
                                let cur_rect = Bounds::new(
                                    point(px(cx0), px(cy0)),
                                    size(px(cw as f32 * cs), px(ch as f32 * cs)),
                                );
                                let _ = window.paint_image(
                                    area,
                                    cur_rect,
                                    Corners::default(),
                                    cimg.clone(),
                                    0,
                                    false,
                                );
                            }
                        }
                    }
                }
            },
        )
        .flex_1()
        .min_h_0());

        if !self.is_active() {
            if let Some(err) = self.error().map(|e| e.to_string()) {
                area = area.child(
                    div()
                        .absolute()
                        .bottom_2()
                        .left_0()
                        .right_0()
                        .text_center()
                        .text_size(px(13.0))
                        .text_color(hsla(0.0, 0.7, 0.7, 1.0))
                        .child(format!("已断开：{err}")),
                );
            }
        }

        area.into_any_element()
    }

    /// 连接表单。
    fn connect_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<crate::app::RustTermApp>,
    ) -> AnyElement {
        let theme = Theme::global(cx).clone();
        let dim = theme.colors.muted_foreground;

        if self.form.is_none() {
            // 先拷出 profile 值，避免与 self.form 赋值、mk 闭包借用冲突
            let host = self.profile.host.clone();
            let port = self.profile.port.to_string();
            let username = self.profile.username.clone();
            let password = self.profile.password.clone();
            let domain = self.profile.domain.clone();
            let share = self.profile.share_dir.clone();
            let (dw, dh) = self.desired;
            // RDP 必须有用户名，占位符直接点明，避免等到握手超时才发现
            let user_ph = if self.profile.protocol == RdTabProtocol::Rdp {
                "用户名（RDP 必填）"
            } else {
                ""
            };
            let mut mk = |text: String, placeholder: &str, masked: bool| {
                let e = cx.new(|cx| {
                    let s = InputState::new(window, cx).placeholder(placeholder.to_string());
                    if masked {
                        s.masked(true)
                    } else {
                        s
                    }
                });
                e.update(cx, |s, cx| s.set_value(text, window, cx));
                e
            };
            self.form = Some(RdFormInputs {
                host: mk(host, "192.168.1.16", false),
                port: mk(port, "", false),
                username: mk(username, user_ph, false),
                // 密码默认密文，配合「查看密码」眼睛按钮
                password: mk(password, "VNC/SPICE 密码，可空", true),
                domain: mk(domain, "可空", false),
                width: mk(dw.to_string(), "", false),
                height: mk(dh.to_string(), "", false),
                share: mk(
                    share,
                    r"D:\share 或 D:\share\子目录（可选，RDP 共享目录）",
                    false,
                ),
            });
        }
        let form = self.form.clone().unwrap();

        let field = |label: &str, input: &Entity<InputState>| {
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .child(div().w(px(64.0)).text_size(px(12.0)).child(label.to_string()))
                .child(Input::new(input).flex_1().text_size(px(12.0)))
        };
        // 密码字段：带「查看密码」眼睛按钮
        let pw_field = |label: &str, input: &Entity<InputState>| {
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .child(div().w(px(64.0)).text_size(px(12.0)).child(label.to_string()))
                .child(password_field(input).flex_1().text_size(px(12.0)))
        };

        let mut content = div().v_flex().gap_2();
        // 协议选择
        let mut proto_row = div()
            .h_flex()
            .items_center()
            .gap_1()
            .child(div().w(px(64.0)).text_size(px(12.0)).child("协议"));
        for p in RdTabProtocol::all() {
            proto_row = proto_row.child(
                Button::new(SharedString::from(format!("rd-p-{:?}", p)))
                    .label(p.label())
                    .compact()
                    .selected(self.profile.protocol == p)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.active_rd(|t| {
                            t.profile.protocol = p;
                            const OLD: [u16; 3] = [5900, 3389, 5930];
                            if OLD.contains(&t.profile.port) {
                                t.profile.port = p.default_port();
                            }
                            t.form = None; // 重建表单以更新端口占位
                        });
                        cx.notify();
                    })),
            );
        }
        content = content.child(proto_row).child(field("主机", &form.host)).child(field("端口", &form.port));
        if self.profile.protocol == RdTabProtocol::Rdp {
            // 共享目录：RDP 独有的「传文件」手段 —— 把本机目录挂成远端盘符。
            // VNC / SPICE 没有标准文件通道（RFB 只有剪贴板），所以只在 RDP 下显示。
            let share_row = div()
                .h_flex()
                .items_center()
                .gap_2()
                .child(div().w(px(64.0)).text_size(px(12.0)).child("共享目录"))
                .child(Input::new(&form.share).flex_1().text_size(px(12.0)))
                .child(
                    Button::new("rd-share-pick")
                        .label("浏览…")
                        .compact()
                        .on_click(cx.listener(|_, _, window, cx| {
                            crate::app::RustTermApp::pick_folder_async(
                                cx,
                                window,
                                |this, window, cx, path| {
                                    this.active_rd(|t| {
                                        t.profile.share_dir = path.clone();
                                        if let Some(f) = &t.form {
                                            let input = f.share.clone();
                                            input.update(cx, |s, cx| {
                                                s.set_value(path.clone(), window, cx)
                                            });
                                        }
                                    });
                                    cx.notify();
                                },
                            );
                        })),
                );
            content = content
                .child(field("用户", &form.username))
                .child(pw_field("密码", &form.password))
                .child(field("域", &form.domain))
                .child(share_row)
                .child(
                    Checkbox::new("rd-legacy")
                        .label("旧版服务器兼容（Server 2008：禁用 NLA，仅用 TLS）")
                        .checked(self.profile.legacy_compat)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.active_rd(|t| t.profile.legacy_compat = *checked);
                            // 勾选状态也属于会话设置：下次从侧栏连同一个会话要保留
                            this.persist_rd_settings();
                            cx.notify();
                        })),
                );
        } else {
            content = content.child(pw_field("密码", &form.password)).child(
                div()
                    .text_size(px(11.0))
                    .text_color(dim)
                    .child("该协议没有文件传输通道：需要传文件请用右侧 SFTP 面板，或改用 RDP 共享目录"),
            );
        }        content = content.child(
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .child(div().w(px(64.0)).text_size(px(12.0)).child("分辨率"))
                .child(Input::new(&form.width).w(px(64.0)).text_size(px(12.0)))
                .child(div().child("×"))
                .child(Input::new(&form.height).w(px(64.0)).text_size(px(12.0))),
        );

        if let Some(err) = self.error() {
            content = content.child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.colors.danger)
                    .child(err.to_string()),
            );
        }
        if self.is_connecting() {
            content = content.child(
                div().text_size(px(12.0)).text_color(dim).child("正在连接…"),
            );
        } else {
            content = content.child(
                Button::new("rd-go")
                    .icon(ic(IconName::Plug))
                    .label("连接")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.active_rd(|t| {
                            t.connect_from_form(cx);
                        });
                        // 表单里可能刚改过共享目录/兼容开关：写回它来源的会话，
                        // 否则下次从侧栏连同一个会话，盘符或兼容设置又没了。
                        this.persist_rd_settings();
                    })),
            );
        }

        div()
            .flex_1()
            .v_flex()
            .items_center()
            .pt_6()
            .child(
                div()
                    .v_flex()
                    .gap_2()
                    .w(px(440.0))
                    .child(
                        div()
                            .text_size(px(16.0))
                            .child(format!("🖥 {}", self.title())),
                    )
                    .child(
                        div()
                            .v_flex()
                            .gap_2()
                            .p_3()
                            .border_1()
                            .border_color(theme.colors.border)
                            .rounded(px(8.0))
                            .child(content),
                    ),
            )
            .into_any_element()
    }
}

/// 读取 app 的活动远程桌面标签。
trait ActiveRd {
    fn active_rd(&mut self, f: impl FnOnce(&mut RemoteDesktopTab));
    fn active_rd_ref(&self) -> Option<&RemoteDesktopTab>;
}

impl ActiveRd for crate::app::RustTermApp {
    fn active_rd(&mut self, f: impl FnOnce(&mut RemoteDesktopTab)) {
        let idx = self.active.min(self.tabs.len().saturating_sub(1));
        if let Some(TabContent::RemoteDesktop(tab)) = self.tabs.get_mut(idx) {
            f(tab);
        }
    }
    fn active_rd_ref(&self) -> Option<&RemoteDesktopTab> {
        match self.tabs.get(self.active) {
            Some(TabContent::RemoteDesktop(tab)) => Some(tab),
            _ => None,
        }
    }
}

use crate::tabs::TabContent;

impl RemoteDesktopTab {
    /// 从连接表单回填参数并连接。需要读输入实体值，由 app 层提供 cx。
    pub fn connect_from_form(&mut self, cx: &App) {
        if let Some(f) = &self.form {
            let get = |e: &Entity<InputState>| e.read(cx).value().to_string();
            let host = get(&f.host);
            self.profile.host = host;
            if let Ok(p) = get(&f.port).trim().parse::<u16>() {
                self.profile.port = p;
            }
            self.profile.username = get(&f.username);
            self.profile.password = get(&f.password);
            self.profile.domain = get(&f.domain);
            // RDP 走 CredSSP/NLA，用户名是硬性要求：缺了会等到
            // CredSSP 握手超时才报 "Got empty identity"，很难看懂。
            // 这里提前拦下，直接给出可操作的提示。
            if self.profile.protocol == RdTabProtocol::Rdp
                && self.profile.username.trim().is_empty()
            {
                self.state = RdState::Failed("RDP 需要用户名：请在「用户」栏填写用户名".into());
                return;
            }
            if let Ok(w) = get(&f.width).trim().parse::<u16>() {
                self.desired.0 = w.clamp(640, 3840);
            }
            if let Ok(h) = get(&f.height).trim().parse::<u16>() {
                self.desired.1 = h.clamp(480, 2160);
            }
            // 共享目录只对 RDP 有意义（VNC/SPICE 没有文件通道）
            self.profile.share_dir = if self.profile.protocol == RdTabProtocol::Rdp {
                get(&f.share).trim().to_string()
            } else {
                String::new()
            };
        }
        self.connect();
    }
}

// ---------------------------------------------------------------------------
// 回归测试
//
// 这些用例锁住远程桌面曾经反复崩溃的几个点：
// 1. 主帧缓冲被 mem::take 掏空 → 第二帧写入越界 panic（三协议通杀）
// 2. 脏区越界 / 长度不足的帧导致切片 panic
// 3. 分辨率变化后旧尺寸贴图没作废，画面拉伸错位
// ---------------------------------------------------------------------------
