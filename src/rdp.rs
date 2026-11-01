//! 远程桌面标签页：VNC / RDP / SPICE 的真实连接与渲染（GPUI 版）。
//!
//! - 后端：`crate::remote`（每会话一个线程 + 事件通道）
//! - 渲染：整帧 BGRA 缓冲 + `RenderImage` 贴图（canvas 元素）
//! - 输入：键盘（扫描码 + keysym，gpui 键名映射）、鼠标、滚轮转发到远端

use crate::remote::{self, RdCommand, RdEvent, RdParams, RdProtocol, RdSession, RdState};
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
}

impl RdTabProtocol {
    pub fn label(self) -> &'static str {
        match self {
            RdTabProtocol::Vnc => "VNC (RFB)",
            RdTabProtocol::Rdp => "RDP (Microsoft)",
            RdTabProtocol::Spice => "SPICE (QEMU)",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            RdTabProtocol::Vnc => 5900,
            RdTabProtocol::Rdp => 3389,
            RdTabProtocol::Spice => 5930,
        }
    }

    fn to_backend(self) -> RdProtocol {
        match self {
            RdTabProtocol::Vnc => RdProtocol::Vnc,
            RdTabProtocol::Rdp => RdProtocol::Rdp,
            RdTabProtocol::Spice => RdProtocol::Spice,
        }
    }

    pub fn all() -> [RdTabProtocol; 3] {
        [RdTabProtocol::Vnc, RdTabProtocol::Rdp, RdTabProtocol::Spice]
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
}

pub struct RemoteDesktopTab {
    pub profile: RdpConnectionProfile,
    pub state: RdState,
    /// 最近一次失败原因（后端会先发 Error 再发 Disconnected，
    /// 后者不能把错误信息冲掉，否则用户只看到「连不上」没有原因）
    pub last_error: Option<String>,
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
    /// 当前按下的鼠标键位掩码
    buttons: u8,
    last_pointer: Option<(u16, u16)>,
    frames: u32,
    fps_t: std::time::Instant,
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
            fb: crate::framebuf::FrameBuffer::new(),
            image: None,
            fit: true,
            zoom: 1.0,
            fps: 0.0,
            clipboard_sync: true,
            capture_keys: true,
            session: None,
            prev_mods: (false, false, false),
            buttons: 0,
            last_pointer: None,
            frames: 0,
            fps_t: std::time::Instant::now(),
            focus: None,
            form: None,
            cursor: None,
            cursor_pos: (0, 0),
            cursor_hidden: false,
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
        if self.profile.host.trim().is_empty() {
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
        };
        self.state = RdState::Connecting;
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
    }

    pub fn disconnect(&mut self) {
        if let Some(mut s) = self.session.take() {
            s.close();
        }
        self.last_error = None;
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
    pub fn poll(&mut self) -> bool {
        let mut got_frame = false;

        let mut events: Vec<RdEvent> = Vec::new();
        if let Some(s) = &self.session {
            while let Ok(ev) = s.events.try_recv() {
                events.push(ev);
            }
        }

        for ev in events {
            got_frame |= self.apply_event(ev);
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

        got_frame
    }

    /// 处理单个后端事件。返回是否带来了新画面。
    ///
    /// 抽成独立函数是为了能脱离网络直接做单元测试（多帧写入、
    /// 分辨率变化、越界脏区这些历史崩溃点都在这里）。
    pub fn apply_event(&mut self, ev: RdEvent) -> bool {
        let mut got_frame = false;
        match ev {
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

    fn send_key(&mut self, ks: &Keystroke, down: bool) {
        self.apply_mods((
            ks.modifiers.shift,
            ks.modifiers.control,
            ks.modifiers.alt,
        ));
        match remote::map_key(&ks.key) {
            Some((scancode, keysym)) => {
                let keysym = remote::printable_keysym(&ks.key, ks.modifiers.shift)
                    .unwrap_or(keysym);
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
        // 隐藏本地系统光标 —— 远端画面里已经有远端系统绘制的光标，
        // 两个都显示就是「两个鼠标」。
        crate::win32::hide_cursor();
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
        self.poll();

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
                toolbar = toolbar
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(dim)
                            .child(if connecting { "正在连接…" } else { "正在认证…" }),
                    )
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
                        this.active_rd(|t| t.clipboard_sync = *checked);
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

        if let Some(err) = self.error().map(|e| e.to_string()) {
            toolbar = toolbar
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.colors.danger)
                        .min_w_0()
                        .text_ellipsis()
                        .child(format!("⚠ {err}")),
                )
                .child(
                    Button::new("rd-retry")
                        .label("重试")
                        .compact()
                        .on_click(cx.listener(|this, _, _, _| {
                            this.active_rd(|t| t.connect());
                        })),
                );
        }

        // ---- 画面 / 表单 ----
        let body = if !self.is_active() && self.image.is_none() {
            self.connect_form(window, cx)
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
            content = content
                .child(field("用户", &form.username))
                .child(pw_field("密码", &form.password))
                .child(field("域", &form.domain));
        } else {
            content = content.child(pw_field("密码", &form.password));
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
                            // 表单值回填
                            if let Some(f) = &t.form {
                                // 输入值在 app 层读取后回填（见 active_rd_connect）
                                let _ = f;
                            }
                            t.connect_from_form(cx);
                        });
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
