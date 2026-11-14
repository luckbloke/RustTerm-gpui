//! 终端核心：alacritty_terminal（VT 状态机）+ GPUI 绘制 + 键鼠编码。
//!
//! 数据流：
//!   pty/ssh ─ mpsc ─► processor.advance(term) ─► renderable_content() ─► GPUI canvas 绘制
//!   键鼠事件 ─ VT 序列编码 ─► mpsc ─► pty/ssh（Term 产生的查询响应经 PtyProxy 回写）
//!
//! 渲染方式：每帧在 `prepare_frame` 中生成一份 `TermSnapshot`（可见行的
//! 文本 + 颜色 run + 选区/查找/光标覆盖层），由 app 层的 `canvas` 元素
//! 持快照绘制。输入经 GPUI 事件监听器（cx.listener）回写本模块。
//!
//! 选区是我们自己的状态（Point 起点/终点），复制文本用
//! `term.bounds_to_string()`，避免依赖 alacritty 的 Selection 类型。

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Term, TermMode};
use anyhow::Result;
use gpui_kit::{
    App, Bounds, Font, Hsla, Keystroke, Modifiers, Pixels, Point as GPoint,
    ScrollDelta, SharedString, Size, TextRun, Window, fill, hsla, px, quad, size,
};
use tokio::sync::mpsc;
use vte::ansi::Processor;

use crate::backends::{self, TermIo};

/// ZMODEM（rz/sz）文件收发：触发检测 + 状态机驱动 + 落盘。
///
/// 为什么挂在 term 模块下：本功能的唯一集成点就是 `TermPane::pump()`——
/// 远端字节流进 vte 的唯一漏斗；放在一起看最省事。也正因为集成点这么窄，
/// 三种后端（本地 pty / SSH / telnet）共用 `io.output_rx` / `io.input_tx`，
/// 功能对它们自动全部可用。
///
/// 写入范围约定不允许改 main.rs，所以用 `#[path]` 把 `src/zmodem.rs`
/// 挂成 term 的子模块（`crate::term::zmodem`），而不是在 crate 根声明。
#[path = "zmodem.rs"]
pub mod zmodem;

const SCROLLBACK: usize = 10_000;

/// ZMODEM 会话多久收不到远端字节就判僵死。
///
/// 正常传输（sz 连续发送 / rz 及时应答）不会静默这么久；没有这个上限，
/// 对端崩溃或网络断开后，窗格会永远停在"传输中"（Ctrl+C 才能出来）。
/// 用户正在选文件（`zm_need_files`）时不适用——那时是我们在等用户。
const ZMODEM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// 会话结束后开多久的"协议尾巴过滤窗口"。
///
/// 远端 `sz`/`rz` 退出前还会发最后一个应答头和 over-and-out 的 `OO`：这些是
/// 协议字节，直接进 vte 会显示成乱码。窗口内先过 `zmodem::TailFilter`；窗口
/// 一到（或认出了正常输出）就把还扣着的字节还回终端。窗口短，用户无感。
const ZMODEM_COOLDOWN: std::time::Duration = std::time::Duration::from_millis(300);

/// 终端 canvas 四周留出的内边距（绘制与鼠标映射共用）。
///
/// 放在这里是因为两个使用方都要它：面板视图（`terminal_view.rs`）测尺寸、
/// 换算列/行；绘制时把文本网格原点同样右移/下移这么多。
pub const TERM_PAD: f32 = 6.0;

const BG: Hsla = Hsla {
    h: 0.619,
    s: 0.25,
    l: 0.104,
    a: 1.0,
};
const FG_DEFAULT: Hsla = Hsla {
    h: 0.60,
    s: 0.36,
    l: 0.898,
    a: 1.0,
};

/// Term → UI 的事件（经 PtyProxy 转发）。
pub enum PaneEvent {
    /// OSC 0/2：窗口标题变更
    Title(String),
    /// OSC 52：终端请求把内容写入系统剪贴板
    ClipboardStore(String),
}

/// Term 尺寸。total_lines = 屏幕行数 + 回滚深度。
#[derive(Clone, Copy, Debug)]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
}

impl Dimensions for TermSize {
    fn columns(&self) -> usize {
        self.cols as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn total_lines(&self) -> usize {
        self.rows as usize + SCROLLBACK
    }
}

/// Term 的事件代理：
/// - `PtyWrite`（设备查询的应答）回写给后端输入通道；
/// - `Title` / `ClipboardStore` 转给 UI（app_tx）；
/// - `ColorRequest` 用内置色板应答；`ClipboardLoad`（OSC52 读）回空串。
pub struct PtyProxy {
    tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
    app_tx: Option<mpsc::UnboundedSender<PaneEvent>>,
}

fn send_pty(tx: &Option<mpsc::UnboundedSender<Vec<u8>>>, text: &str) {
    if let Some(tx) = tx {
        let _ = tx.send(text.as_bytes().to_vec());
    }
}

fn color_request_rgb(index: usize) -> vte::ansi::Rgb {
    // 0..16 命名色 / 16..232 色块 / 232..256 灰阶 / 256+ 特殊色彩
    let c = match index {
        256 => hsla_to_rgb(FG_DEFAULT),      // foreground
        257 => hsla_to_rgb(BG),              // background
        258 => (255, 255, 255),              // cursor
        259..=266 => rgb_to_tuple(index - 259),
        267 => rgb_to_tuple(14),             // bright foreground（近似）
        268 => hsla_to_rgb(BG),              // dim background
        i if i < 16 => rgb_to_tuple(i),
        i => hsla_to_rgb(palette_color((i as u8).min(255))),
    };
    vte::ansi::Rgb { r: c.0, g: c.1, b: c.2 }
}

fn rgb_to_tuple(i: usize) -> (u8, u8, u8) {
    const RGB16: [[u8; 3]; 16] = [
        [30, 30, 34],
        [234, 78, 102],
        [69, 197, 112],
        [238, 190, 70],
        [86, 143, 238],
        [197, 108, 234],
        [72, 202, 224],
        [200, 208, 220],
        [90, 96, 110],
        [255, 120, 140],
        [120, 240, 160],
        [255, 220, 120],
        [130, 180, 255],
        [230, 160, 255],
        [130, 240, 255],
        [240, 244, 252],
    ];
    let c = RGB16[i.min(15)];
    (c[0], c[1], c[2])
}

fn hsla_to_rgb(c: Hsla) -> (u8, u8, u8) {
    let rgba = c.to_rgb();
    (
        (rgba.r * 255.0).round() as u8,
        (rgba.g * 255.0).round() as u8,
        (rgba.b * 255.0).round() as u8,
    )
}

impl EventListener for PtyProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => send_pty(&self.tx, &text),
            Event::ColorRequest(index, formatter) => {
                send_pty(&self.tx, &formatter(color_request_rgb(index)));
            }
            Event::ClipboardLoad(_typ, formatter) => {
                // OSC52 读剪贴板：不支持读取，回空串避免程序挂起
                send_pty(&self.tx, &formatter(""));
            }
            Event::Title(title) => {
                if let Some(app_tx) = &self.app_tx {
                    let _ = app_tx.send(PaneEvent::Title(title));
                }
            }
            Event::ResetTitle => {
                if let Some(app_tx) = &self.app_tx {
                    let _ = app_tx.send(PaneEvent::Title("终端".to_string()));
                }
            }
            Event::ClipboardStore(_typ, text) => {
                if let Some(app_tx) = &self.app_tx {
                    let _ = app_tx.send(PaneEvent::ClipboardStore(text));
                }
            }
            _ => {}
        }
    }
}

/// 一个终端标签页。
pub struct TermPane {
    pub title: String,
    pub term: Term<PtyProxy>,
    processor: Processor,
    pub dims: TermSize,
    io: TermIo,
    /// 本窗格的启动配置（分屏时按此复制一个新会话）
    pub spawn: SpawnCfg,
    /// 对端断开
    pub dead: bool,
    /// 选区：(起点, 终点)，绝对 Line 坐标
    pub selection: Option<(Point<Line>, Point<Line>)>,
    /// 是否为分屏中的聚焦窗格
    pub has_focus: bool,
    /// PaneEvent 接收端（标题/OSC52）
    events_rx: mpsc::UnboundedReceiver<PaneEvent>,
    /// 查找关键字（空 = 未搜索）
    pub search: String,
    /// 匹配项：(绝对行, 起始列, 结束列)
    pub search_matches: Vec<(i32, usize, usize)>,
    /// 当前定位到的匹配序号
    pub search_index: usize,
    /// 输入镜像通道：开启「同步输入」时，把本窗格输入复制一份出去
    pub mirror: Option<std::sync::mpsc::Sender<Vec<u8>>>,
    /// 每帧单元格尺寸（逻辑像素），由 prepare_frame 按字体测量
    pub cell: Size<Pixels>,
    /// 绘制快照使用的字号（与 fit_to 测量保持一致，避免列对齐错位）
    pub font_size: Pixels,
    /// 防抖：拖分屏时尺寸连续变化，每帧发 SIGWINCH 会让 bash 反复重绘提示符
    /// （碎片化输出）。先记目标，限频 200ms 发一次，落定后补最终尺寸。
    pending_resize: Option<(u16, u16)>,
    /// 挂起目标第一次出现的时刻：连续变化期间不断刷新，稳定 200ms 后才发
    pending_since: std::time::Instant,
    /// ZMODEM 会话（None = 正常终端模式，远端字节照常进 vte）
    zmodem: Option<zmodem::Session>,
    /// 正常模式下扫描远端流里有没有 ZMODEM 触发头（跨 chunk 边界）
    zm_detector: zmodem::Detector,
    /// 远端 `rz` 已就绪、等本地选文件：由 app 层取走后在自己线程里弹 rfd
    zm_need_files: bool,
    /// 选择器已经弹出、在等用户操作（此时不该算"对端无响应超时"）
    zm_picking: bool,
    /// 一次性提示（完成 / 失败 / 取消），app 取走弹 toast
    zm_notice: Option<String>,
    /// 会话刚结束的冷却截止时刻（见 `ZMODEM_COOLDOWN`）
    zm_cooldown_until: Option<std::time::Instant>,
    /// 冷却期内的协议尾巴过滤器（把最后一个应答头 / `OO` 挡在 vte 外面）
    zm_tail: zmodem::TailFilter,
}

/// 新窗格的启动方式（预留：分屏复制会话时按此重建）。
#[derive(Clone)]
#[allow(dead_code)] // 字段用于记录来源，分屏复制会话时会读取
pub enum SpawnCfg {
    Local,
    Ssh {
        host: String,
        port: u16,
        user: String,
        auth: backends::ssh::SshAuth,
        /// 跳板机（ProxyJump），None = 直连
        jump: Option<backends::ssh::JumpHost>,
        /// SSH X11 转发（内置 X server）
        x11: bool,
    },
    /// 在 PTY 里运行任意命令（Ping / traceroute 等）
    Command {
        program: String,
        args: Vec<String>,
    },
    /// 原始 TCP（Telnet）
    Tcp {
        host: String,
        port: u16,
    },
}

impl TermPane {
    pub fn new_local(title: String, cols: u16, rows: u16) -> Result<Self> {
        let mut pane = Self::new(title, backends::local::spawn(cols, rows)?, cols, rows);
        pane.spawn = SpawnCfg::Local;
        Ok(pane)
    }

    /// 在 PTY 中运行任意命令（Ping 等），复用终端渲染与交互。
    pub fn new_command(
        title: String,
        program: &str,
        args: &[String],
        cols: u16,
        rows: u16,
    ) -> Result<Self> {
        let io = backends::local::spawn_command(program, args, cols, rows)?;
        let mut pane = Self::new(title, io, cols, rows);
        pane.spawn = SpawnCfg::Command {
            program: program.to_string(),
            args: args.to_vec(),
        };
        Ok(pane)
    }

    /// 原始 TCP（Telnet）会话。
    pub fn new_tcp(title: String, host: String, port: u16, cols: u16, rows: u16) -> Result<Self> {
        let io = backends::tcp::spawn(&host, port)?;
        let mut pane = Self::new(title, io, cols, rows);
        pane.spawn = SpawnCfg::Tcp { host, port };
        Ok(pane)
    }

    pub async fn new_ssh(
        title: String,
        host: String,
        port: u16,
        user: String,
        auth: backends::ssh::SshAuth,
        jump: Option<backends::ssh::JumpHost>,
        cols: u16,
        rows: u16,
        x11: bool,
    ) -> Result<Self> {
        let io = backends::ssh::spawn(
            host.clone(),
            port,
            user.clone(),
            auth.clone(),
            jump.clone(),
            cols,
            rows,
            x11,
        )
        .await?;
        let mut pane = Self::new(title, io, cols, rows);
        pane.spawn = SpawnCfg::Ssh { host, port, user, auth, jump, x11 };
        Ok(pane)
    }

    /// task-22 多显示：本终端会话的 X 屏幕 id（启用 X11 转发时才有）。
    pub fn x11_screen_id(&self) -> Option<String> {
        self.io
            .ssh
            .as_ref()
            .and_then(|h| h.x11_screen_id.clone())
    }

    fn new(title: String, io: TermIo, cols: u16, rows: u16) -> Self {
        let dims = TermSize { cols, rows };
        let (events_tx, events_rx) = mpsc::unbounded_channel::<PaneEvent>();
        let proxy = PtyProxy {
            tx: Some(io.input_tx.clone()),
            app_tx: Some(events_tx),
        };
        let term = Term::new(term::Config::default(), &dims, proxy);
        Self {
            title,
            term,
            processor: Processor::new(),
            dims,
            io,
            spawn: SpawnCfg::Local,
            dead: false,
            selection: None,
            has_focus: false,
            events_rx,
            search: String::new(),
            search_matches: Vec::new(),
            search_index: 0,
            mirror: None,
            cell: size(px(9.0), px(17.0)),
            font_size: px(14.0),
            pending_resize: None,
            pending_since: std::time::Instant::now(),
            zmodem: None,
            zm_detector: zmodem::Detector::new(),
            zm_need_files: false,
            zm_picking: false,
            zm_notice: None,
            zm_cooldown_until: None,
            zm_tail: zmodem::TailFilter::default(),
        }
    }

    /// 吸收后端字节流，返回 true 表示**终端画面**有新内容。
    ///
    /// ZMODEM 只在这里加一个岔路口：正常模式下扫描触发头，命中后本窗格转入
    /// 传输模式（字节改喂协议状态机，不再进 vte），会话结束后自动回正常模式。
    /// 三种后端的回写通道都是 `io.input_tx`，所以状态机写出的字节不需要
    /// 任何后端相关的代码。
    pub fn pump(&mut self) -> bool {
        let mut dirty = false;
        let mut budget = 16; // 每帧最多 16 批，避免大吞吐打满 UI
        loop {
            if budget == 0 {
                break;
            }
            match self.io.output_rx.try_recv() {
                Ok(data) => {
                    budget -= 1;
                    if self.absorb(&data) {
                        dirty = true;
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.dead = true;
                    return true;
                }
            }
        }
        // 检测器扣住的半截头超时还没等到后续字节 → 还回终端：
        // 普通输出末尾恰好是 `*` 时不能一直扣着不显示。
        if let Some(bytes) = self.zm_detector.flush_stale() {
            self.processor.advance(&mut self.term, &bytes);
            dirty = true;
        }
        // 协议尾巴过滤窗口结束：把还扣着的字节还回去（宁可显示，也不静默丢输出）
        if let Some(until) = self.zm_cooldown_until {
            if std::time::Instant::now() >= until {
                self.zm_cooldown_until = None;
                let left = self.zm_tail.finish();
                if !left.is_empty() {
                    self.processor.advance(&mut self.term, &left);
                    dirty = true;
                }
            }
        }
        // 传输推进（每帧一次，单帧工作量由 zmodem 模块的解析预算限制）
        if self.zmodem.is_some() {
            self.drive_zmodem();
            let done = self.zmodem.as_ref().map(|s| s.is_done()).unwrap_or(false);
            if done {
                self.finish_zmodem();
            } else {
                let idle = self
                    .zmodem
                    .as_ref()
                    .map(|s| s.idle_for())
                    .unwrap_or_default();
                if !self.zm_need_files && !self.zm_picking && idle > ZMODEM_IDLE_TIMEOUT {
                    let msg =
                        format!("等待远端响应超时（{} 秒无数据）", ZMODEM_IDLE_TIMEOUT.as_secs());
                    if let Some(session) = self.zmodem.as_mut() {
                        session.fail(msg);
                    }
                }
            }
        }
        dirty
    }

    /// 一批远端字节的去向：传输中 → 状态机；否则 → 检测器 →（vte 或新会话）。
    ///
    /// 返回是否真的喂给了 vte（决定要不要重建绘制快照）。
    fn absorb(&mut self, data: &[u8]) -> bool {
        if let Some(session) = self.zmodem.as_mut() {
            session.feed(data);
            // 传输期间画面内容不变：不重建整屏快照（大文件传输时这是每帧都在省的固定开销）
            return false;
        }
        if !crate::config::zmodem_enabled() {
            self.processor.advance(&mut self.term, data);
            return true;
        }
        // 触发检测永远最先跑：紧跟着上一次会话的第二次 sz/rz 不能被冷却期漏掉。
        // 残留的协议字节都是 ZFIN/ZRINIT 这类"非开场帧"，检测器不会误命中，
        // 它们会落到下面的 Miss 分支交给尾巴过滤器。
        match self.zm_detector.scan(data) {
            zmodem::Scan::Miss { bytes } => {
                if bytes.is_empty() {
                    return false;
                }
                // 会话刚结束的冷却期：远端 sz/rz 退出前还会发最后一个应答头和
                // `OO`，先剥掉这些协议字节，剩下的（shell 提示符）才进 vte
                if self.zm_cooldown_until.is_some() {
                    let out = self.zm_tail.push(&bytes);
                    if out.is_empty() {
                        return false;
                    }
                    self.processor.advance(&mut self.term, &out);
                    return true;
                }
                self.processor.advance(&mut self.term, &bytes);
                true
            }
            zmodem::Scan::Hit {
                trigger,
                before,
                head,
            } => {
                // 头之前的字节照常进终端：远端 `sz` 之前打印的东西不能丢
                if !before.is_empty() {
                    self.processor.advance(&mut self.term, &before);
                }
                // 新会话开始：冷却期与上一个会话的尾巴过滤全部作废
                self.zm_tail = zmodem::TailFilter::default();
                self.zm_cooldown_until = None;
                self.start_zmodem(trigger, head);
                true
            }
        }
    }

    /// 命中 ZMODEM 头：按帧类型选角色建会话，并把已扣下的头交给状态机。
    fn start_zmodem(&mut self, trigger: zmodem::Trigger, head: Vec<u8>) {
        let session = match trigger {
            zmodem::Trigger::Receive => zmodem::Session::receive(zmodem::download_dir()),
            zmodem::Trigger::Send => zmodem::Session::send(),
        };
        let Ok(mut session) = session else {
            // 建会话失败（理论上只有内存不足）：字节还给终端，绝不吞掉输出
            self.processor.advance(&mut self.term, &head);
            return;
        };
        match session.role() {
            zmodem::Role::Receive => log::info!("ZMODEM: 检测到 ZRQINIT，开始接收文件"),
            zmodem::Role::Send => log::info!("ZMODEM: 检测到 ZRINIT，准备发送文件"),
        }
        session.feed(&head);
        if session.role() == zmodem::Role::Send {
            // 远端 `rz` 已经就绪：请 app 层弹文件选择器（rfd 不能在本线程调）
            self.zm_need_files = true;
        }
        self.zmodem = Some(session);
    }

    /// 会话结束：把状态机没消费的尾巴还给终端（远端 rz/sz 退出后打印的
    /// shell 提示符就在里面），记下提示，回正常模式。
    fn finish_zmodem(&mut self) {
        let Some(mut session) = self.zmodem.take() else {
            return;
        };
        let left = session.take_leftover();
        // 尾巴里混着协议字节（最后一个应答头 / `OO`）：过一把过滤器，
        // 只把正常输出（shell 提示符）交给 vte
        let show = self.zm_tail.start(&left);
        if !show.is_empty() {
            self.processor.advance(&mut self.term, &show);
        }
        self.zm_notice = Some(session.notice_text());
        self.zm_need_files = false;
        self.zm_picking = false;
        self.zm_cooldown_until = Some(std::time::Instant::now() + ZMODEM_COOLDOWN);
    }

    /// 推进 ZMODEM 会话：状态机要求写出的字节直接进 `input_tx`。
    fn drive_zmodem(&mut self) {
        {
            // input_tx 与 zmodem 是 self 的不同字段：分开借用，闭包只拿发送端
            let tx = &self.io.input_tx;
            let mut wire = |bytes: &[u8]| {
                let _ = tx.send(bytes.to_vec());
            };
            if let Some(session) = self.zmodem.as_mut() {
                session.drive(&mut wire);
            }
        }
    }

    /// 状态栏用：ZMODEM 传输状态文字（没有活动会话时 None）。
    pub fn zmodem_status(&self) -> Option<String> {
        if self.zm_need_files || self.zm_picking {
            return Some("⬆ ZMODEM：等待选择发送文件…".to_string());
        }
        self.zmodem.as_ref().and_then(|s| s.status_text())
    }

    /// 当前窗格是否正处在 ZMODEM 传输模式（字节不进 vte）。
    pub fn zmodem_active(&self) -> bool {
        self.zmodem.is_some()
    }

    /// 取走一次性提示（完成 / 失败 / 取消），app 拿去弹 toast。
    pub fn take_zmodem_notice(&mut self) -> Option<String> {
        self.zm_notice.take()
    }

    /// 远端 `rz` 是否在等我们选文件。
    pub fn zmodem_wants_files(&self) -> bool {
        self.zm_need_files
    }

    /// 取走"要选文件"的请求（真正弹选择器之前调用，避免重复弹）。
    pub fn zmodem_take_file_request(&mut self) -> bool {
        let wanted = std::mem::take(&mut self.zm_need_files);
        if wanted {
            self.zm_picking = true;
        }
        wanted
    }

    /// app 层选完文件后回调；`files` 为空 = 用户取消 → 按协议中止会话，
    /// 不能让远端 `rz` 一直等着。
    pub fn zmodem_files_picked(&mut self, files: Vec<std::path::PathBuf>) {
        self.zm_picking = false;
        let Some(session) = self.zmodem.as_mut() else {
            return;
        };
        if files.is_empty() || session.add_files(files) == 0 {
            session.abort();
            self.zm_notice = Some("已取消 ZMODEM 发送".to_string());
        }
        self.drive_zmodem();
    }

    /// 吸收 PaneEvent（标题 / OSC52），返回 (标题变化, 待写剪贴板文本)。
    pub fn drain_pane_events(&mut self) -> (bool, Option<String>) {
        let mut changed = false;
        let mut clip = None;
        while let Ok(ev) = self.events_rx.try_recv() {
            match ev {
                PaneEvent::Title(t) => {
                    self.title = t;
                    changed = true;
                }
                PaneEvent::ClipboardStore(text) => {
                    clip = Some(text);
                }
            }
        }
        (changed, clip)
    }

    /// 终端 resize（含通知后端），返回是否发生变化。
    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        if cols < 2 || rows < 2 || (self.dims.cols, self.dims.rows) == (cols, rows) {
            self.pending_resize = None;
            return false;
        }
        // **完全防抖**：拖分屏时尺寸连续变化，哪怕限频发 SIGWINCH，bash 在
        // 每个新宽度下重绘提示符都会留下换行碎片（实测依旧）。所以连续变化
        // 期间一个都不发：挂起目标不断被刷新（pending_since 也刷新），只有
        // 尺寸稳定 200ms 后才由 `flush_pending_resize` 发最终尺寸。
        let key = (cols, rows);
        if self.pending_resize.as_ref() != Some(&key) {
            self.pending_resize = Some(key);
            self.pending_since = std::time::Instant::now();
        }
        false
    }

    /// 立即应用 pty 尺寸（绕过防抖，落定补发用）。
    fn apply_resize(&mut self, cols: u16, rows: u16) -> bool {
        self.pending_resize = None;
        self.dims = TermSize { cols, rows };
        self.term.resize(self.dims);
        // 尺寸变化使选区坐标失效：新网格可能容不下旧选区（分屏拖动把格子
        // 变矮后，copy_selection 沿旧坐标索引网格越界 panic——crash.log
        // 实录）。真实终端在 resize 后也会作废/收敛选区。
        self.selection = None;
        let _ = self.io.resize_tx.send((cols, rows));
        // 不清屏：清屏后 shell 不知道，提示符状态丢失，用户每次按回车才
        // 出新提示符——体感等同「敲了回车」（实测反馈）。正确机制是完全
        // 防抖 + 落定后**只发一次 SIGWINCH**：bash 收到后原位重绘提示符，
        // 一次、干净、不追加行。
        true
    }

    /// 每帧调用：有挂起的尺寸变化且已到限频间隔就补发（拖动结束的收尾）。
    pub fn flush_pending_resize(&mut self) -> bool {
        let Some((cols, rows)) = self.pending_resize else {
            return false;
        };
        // 尺寸稳定（200ms 内没再变化）才发，避免拖动中碎片化重绘
        if self.pending_since.elapsed().as_millis() < 200 {
            return false;
        }
        self.apply_resize(cols, rows)
    }

    /// SSH 会话句柄（本地 / Telnet 窗格返回 None），供 SFTP 面板复用连接。
    pub fn ssh_handle(&self) -> Option<std::sync::Arc<backends::ssh::SshHandle>> {
        self.io.ssh.clone()
    }

    pub fn write_input(&self, bytes: Vec<u8>) {
        // 同步输入：先复制给其它窗格，再写本窗格
        if let Some(mx) = &self.mirror {
            let _ = mx.send(bytes.clone());
        }
        let _ = self.io.input_tx.send(bytes);
    }

    /// 只写本窗格（广播回放时用，避免二次镜像造成回环）。
    pub fn write_input_local(&self, bytes: Vec<u8>) {
        let _ = self.io.input_tx.send(bytes);
    }

    // ------------------------------------------------------------------
    // 查找
    // ------------------------------------------------------------------

    /// 设置查找关键字并重新计算匹配项（大小写不敏感）。
    pub fn set_search(&mut self, needle: &str) {
        self.search = needle.to_string();
        self.search_matches.clear();
        self.search_index = 0;
        let needle = needle.trim().to_lowercase();
        if needle.is_empty() {
            return;
        }
        let (top, bottom) = {
            let grid = self.term.grid();
            (grid.topmost_line().0, grid.bottommost_line().0)
        };
        for line in top..=bottom {
            if let Some((start, end)) = self.find_in_line(Line(line), &needle) {
                self.search_matches.push((line, start, end));
            }
        }
        // 默认定位到最靠近屏幕底部（最新）的一处
        if !self.search_matches.is_empty() {
            self.search_index = self.search_matches.len() - 1;
            self.scroll_to_search_match();
        }
    }

    /// 在当前行内查找，返回 (起始列, 结束列)——按列而非字符索引。
    fn find_in_line(&self, line: Line, needle: &str) -> Option<(usize, usize)> {
        let grid = self.term.grid();
        let mut text = String::new();
        // char 序号 → 列号映射（跳过双宽字符的占位格）
        let mut col_of_char: Vec<usize> = Vec::new();
        for col in 0..self.dims.cols as usize {
            let cd = &grid[line][Column(col)];
            if cd.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            text.push(cd.c);
            col_of_char.push(col);
        }
        let lower = text.to_lowercase();
        let pos = lower.find(needle)?;
        let char_start = text[..pos].chars().count();
        let char_len = needle.chars().count();
        let char_end = (char_start + char_len).min(col_of_char.len());
        if char_start >= col_of_char.len() {
            return None;
        }
        let start_col = col_of_char[char_start];
        let end_col = col_of_char
            .get(char_end.saturating_sub(1))
            .copied()
            .unwrap_or(start_col);
        Some((start_col, end_col))
    }

    /// 把视口滚动到当前匹配项。
    pub fn scroll_to_search_match(&mut self) {
        let Some(&(line, _, _)) = self.search_matches.get(self.search_index) else {
            return;
        };
        let content = self.term.renderable_content();
        let offset = content.display_offset as i32;
        drop(content);
        let bottom_line = self.dims.rows as i32 - 1 - offset;
        let mid = self.dims.rows as i32 / 2;
        let delta = line - (bottom_line - mid);
        if delta != 0 {
            self.term.scroll_display(Scroll::Delta(-delta));
        }
    }

    /// 下一个 / 上一个匹配项。
    pub fn search_next(&mut self) {
        if self.search_matches.is_empty() {
            return;
        }
        self.search_index = (self.search_index + 1) % self.search_matches.len();
        self.scroll_to_search_match();
    }

    pub fn search_prev(&mut self) {
        if self.search_matches.is_empty() {
            return;
        }
        self.search_index = if self.search_index == 0 {
            self.search_matches.len() - 1
        } else {
            self.search_index - 1
        };
        self.scroll_to_search_match();
    }

    pub fn clear_search(&mut self) {
        self.search.clear();
        self.search_matches.clear();
        self.search_index = 0;
    }

    pub fn selection_text(&self) -> Option<String> {
        let (a, b) = normalized_selection(&self.selection)?;
        // resize（分屏拖动）后旧选区可能落在网格外：bounds_to_string 按选区
        // 行索引网格，越界直接 panic（crash.log 实录：len=50，
        // index=18446744073709551564——负行号 usize 回绕）。越界即作废选区。
        let grid = self.term.grid();
        let cols = self.dims.cols as usize;
        let in_grid =
            |p: &Point<Line>| -> bool {
                p.line >= grid.topmost_line()
                    && p.line <= grid.bottommost_line()
                    && (p.column.0 as usize) < cols
            };
        if !in_grid(&a) || !in_grid(&b) {
            return None;
        }
        Some(self.term.bounds_to_string(a, b))
    }

    /// 是否存在非空选区（起点与终点不同）。
    pub fn has_selection(&self) -> bool {
        match normalized_selection(&self.selection) {
            Some((a, b)) => a != b,
            None => false,
        }
    }

    /// 把选区文本写入系统剪贴板，返回是否真的复制到了内容。
    pub fn copy_selection(&self, cx: &mut App) -> bool {
        let Some(raw) = self.selection_text() else {
            return false;
        };
        // 行尾空白不进入剪贴板，避免粘到 shell 里带一堆空格
        let text = raw
            .split('\n')
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            return false;
        }
        copy_to_clipboard(cx, text);
        true
    }

    /// 系统剪贴板 → 终端输入。返回是否发送了内容。
    pub fn paste_clipboard(&mut self, cx: &App) -> bool {
        let Some(text) = read_clipboard(cx) else {
            return false;
        };
        self.scroll_to_bottom(0);
        let text = text.replace("\r\n", "\n");
        // 开了 bracketed paste 的程序（bash/zsh/vim）要求粘贴内容被
        // `ESC[200~ … ESC[201~` 包住，否则多行粘贴会被当成逐条命令执行。
        let bracketed = {
            let content = self.term.renderable_content();
            content.mode.contains(TermMode::BRACKETED_PASTE)
        };
        if bracketed {
            let mut out = String::with_capacity(text.len() + 12);
            out.push_str("\x1b[200~");
            out.push_str(&text);
            out.push_str("\x1b[201~");
            self.write_input(out.into_bytes());
        } else {
            // 终端里回车只有一个字节 \r
            let text = text.replace('\n', "\r");
            self.write_input(text.into_bytes());
        }
        true
    }

    /// 全选（含回滚历史）。
    pub fn select_all(&mut self) {
        let (top, bottom) = {
            let grid = self.term.grid();
            (grid.topmost_line(), grid.bottommost_line())
        };
        let last_col = Column(self.dims.cols.saturating_sub(1) as usize);
        self.selection = Some((Point::new(top, Column(0)), Point::new(bottom, last_col)));
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// 取终端最近若干行可见文本（供 AI 上下文使用）。
    pub fn context_text(&self, max_lines: usize, max_line_len: usize) -> String {
        if max_lines == 0 {
            return String::new();
        }
        let content = self.term.renderable_content();
        let display_offset = content.display_offset;
        drop(content);

        let grid = self.term.grid();
        let bottom = self.dims.rows as i32 - 1 - display_offset as i32;
        let topmost = grid.topmost_line().0;
        let mut lines: Vec<String> = Vec::new();
        for i in 0..max_lines as i32 {
            let row = bottom - i;
            if row < topmost {
                break;
            }
            let text = self.line_text(Line(row), max_line_len);
            if !text.trim().is_empty() {
                lines.push(text);
            }
        }
        lines.reverse();
        lines.join("\n")
    }

    /// 取某一绝对行的文本（按 max_len 截断）。
    fn line_text(&self, line: Line, max_len: usize) -> String {
        let grid = self.term.grid();
        let mut s = String::new();
        for col in 0..self.dims.cols as usize {
            let cd = &grid[line][Column(col)];
            if cd.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            s.push(cd.c);
        }
        let trimmed = s.trim_end().to_string();
        if trimmed.chars().count() > max_len {
            let head: String = trimmed.chars().take(max_len).collect();
            format!("{head}…")
        } else {
            trimmed
        }
    }

    /// 根据可用空间与字体测量单元格，并 resize 终端。
    /// 在渲染帧开头调用（render 拿得到 window）。
    pub fn fit_to(
        &mut self,
        window: &mut Window,
        font_size: f32,
        available: Size<Pixels>,
    ) -> bool {
        self.font_size = px(font_size);
        let font = terminal_font();
        let run = [TextRun {
            len: 1,
            font: font.clone(),
            color: FG_DEFAULT,
            background_color: None,
            underline: None,
            strikethrough: None,
        }];
        let shaped = window.text_system().shape_line(
            SharedString::from("W"),
            px(font_size),
            &run,
            None,
        );
        self.cell = size(shaped.width, shaped.ascent + shaped.descent + px(1.0));
        let cols = ((available.width / self.cell.width).floor() as u16).max(4);
        let rows = ((available.height / self.cell.height).floor() as u16).max(3);
        self.resize(cols, rows)
    }

    /// 生成一帧绘制快照（app 层的 canvas 元素用它绘制网格）。
    pub fn snapshot(&self) -> TermSnapshot {
        let content = self.term.renderable_content();
        let cursor = content.cursor;
        let display_offset = content.display_offset;
        let mode: TermMode = content.mode;
        let sel = normalized_selection(&self.selection);
        let cols_i = self.dims.cols;
        let rows_i = self.dims.rows;
        let colors: Colors = *content.colors;
        drop(content);

                let cell = self.cell;
        let mut snap = TermSnapshot {
            cell,
            font_size: self.font_size,
            rows: Vec::with_capacity(rows_i as usize),
            sel_quads: Vec::new(),
            search_quads: Vec::new(),
            search_current_quads: Vec::new(),
            cursor: None,
            focused: self.has_focus,
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            mouse_mode: mode.intersects(
                TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION,
            ),
        };

        let q = |row: i32, c0: usize, c1: usize| {
            let x = px(c0 as f32 * cell.width.as_f32());
            let y = px(row as f32 * cell.height.as_f32());
            let w = px(((c1 + 1).max(c0) - c0) as f32 * cell.width.as_f32());
            Bounds::new(GPoint::new(x, y), size(w, cell.height))
        };

        // ---- 选区高亮 ----
        if let Some((a, b)) = sel {
            for line in a.line.0..=b.line.0 {
                let row = line + display_offset as i32;
                if row < 0 || row >= rows_i as i32 {
                    continue;
                }
                let col_start = if line == a.line.0 { a.column.0 } else { 0 };
                let col_end = if line == b.line.0 {
                    (b.column.0 + 1).min(cols_i as usize)
                } else {
                    cols_i as usize
                };
                if col_start >= col_end {
                    continue;
                }
                snap.sel_quads.push(q(row, col_start, col_end - 1));
            }
        }

        // ---- 查找高亮 ----
        if !self.search_matches.is_empty() {
            let current = self.search_matches.get(self.search_index).copied();
            for (idx, &(line, c0, c1)) in self.search_matches.iter().enumerate() {
                let row = line + display_offset as i32;
                if row < 0 || row >= rows_i as i32 {
                    continue;
                }
                let is_current = current.map(|c| c.0 == line).unwrap_or(false)
                    && idx == self.search_index;
                if is_current {
                    snap.search_current_quads.push(q(row, c0, c1));
                } else {
                    snap.search_quads.push(q(row, c0, c1));
                }
            }
        }

        // ---- 网格文本 ----
        let grid = self.term.grid();
        for row in 0..rows_i as i32 {
            let line = Line(row - display_offset as i32);
            let mut runs: Vec<TermRun> = Vec::new();
            // 已被跳过的「空白 + 无底色」格数：稍后补回占位空格，
            // 保证等宽字形按列推进（否则后面文本会因缺格而向左挤）
            let mut pending_spaces = 0usize;
            for col in 0..cols_i as usize {
                let cd = &grid[line][Column(col)];
                let flags = cd.flags;
                if flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue; // 双宽字符的后半格
                }
                let mut fg = resolve_color(cd.fg, &colors, true, flags);
                let mut bg = resolve_color(cd.bg, &colors, false, flags);
                if flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let bg = if bg == BG { None } else { Some(bg) };

                let mut s: String = cd.c.into();
                if let Some(zw) = cd.zerowidth() {
                    for z in zw.iter() {
                        s.push(*z);
                    }
                }
                if s == " " && bg.is_none() {
                    // 空白且无底色：先记下列数，继续推进（行尾的会自然丢弃）
                    pending_spaces += 1;
                    continue;
                }
                if pending_spaces > 0 {
                    // 把跳过的空白补成占位空格。空格本身不可见、
                    // TextRun bg=None 不画底色，颜色跟谁都可以。
                    let pad = " ".repeat(std::mem::take(&mut pending_spaces));
                    if let Some(last) = runs.last_mut() {
                        last.text.push_str(&pad);
                    } else {
                        runs.push(TermRun {
                            text: pad,
                            fg,
                            bg,
                            underline: false,
                        });
                    }
                }
                // 合并相同样式的相邻 run
                if let Some(last) = runs.last_mut() {
                    if last.fg == fg && last.bg == bg && last.underline == flags.contains(Flags::UNDERLINE) {
                        last.text.push_str(&s);
                        continue;
                    }
                }
                runs.push(TermRun {
                    text: s,
                    fg,
                    bg,
                    underline: flags.contains(Flags::UNDERLINE),
                });
            }
            if !runs.is_empty() {
                snap.rows.push(TermRow { row, runs });
            }
        }

        // ---- 光标 ----
        let cursor_row = cursor.point.line.0 + display_offset as i32;
        if cursor_row >= 0 && cursor_row < rows_i as i32 && cursor.shape != vte::ansi::CursorShape::Hidden {
            let bounds = Bounds::new(
                GPoint::new(
                    px(cursor.point.column.0 as f32 * cell.width.as_f32()),
                    px(cursor_row as f32 * cell.height.as_f32()),
                ),
                cell,
            );
            let color = if self.has_focus {
                hsla(0.6, 0.35, 0.9, 0.9)
            } else {
                hsla(0.0, 0.0, 0.66, 0.35)
            };
            snap.cursor = Some(TermCursor {
                bounds,
                shape: match cursor.shape {
                    vte::ansi::CursorShape::Block => CursorShape::Block,
                    vte::ansi::CursorShape::HollowBlock => CursorShape::Hollow,
                    vte::ansi::CursorShape::Beam => CursorShape::Beam,
                    _ => CursorShape::Underline,
                },
                color,
            });
        }

        snap
    }

    // ------------------------------------------------------------------
    // 输入处理（由 app 层的 GPUI 监听器转发进来）
    // ------------------------------------------------------------------

    /// 处理一次按键（GPUI Keystroke）。
    ///
    /// 剪贴板快捷键必须在这里拦下来：pty 只认字节流，
    /// Ctrl+C 在终端里是中断信号（^C），所以复制统一走 Ctrl+Shift+C。
    pub fn handle_keystroke(&mut self, ks: &Keystroke, cx: &mut App) {
        let mods = &ks.modifiers;
        let ctrl = mods.control && !mods.alt;

        // ---- ZMODEM 传输中的 Ctrl+C：按协议中止 ----
        // 传输期间本窗格的字节通路被协议状态机接管，把 ^C 发出去只会被远端的
        // sz/rz 当成数据字节；用户按 Ctrl+C 的意图是"取消这次传输"，所以这里
        // 优先按协议发 CAN 中止序列。复制仍可用 Ctrl+Shift+C（条件带 shift）。
        if ctrl && !mods.shift && ks.key == "c" && self.zmodem.is_some() {
            if let Some(session) = self.zmodem.as_mut() {
                session.abort();
            }
            self.zm_need_files = false;
            self.zm_picking = false;
            self.zm_notice = Some("正在中止 ZMODEM 传输…".to_string());
            // 立刻把 CAN 序列送出去，不等下一帧
            self.drive_zmodem();
            return;
        }

        // ---- 复制 ----
        // Ctrl+Shift+C / Ctrl+Insert 是终端通用复制键；
        // 另加「Ctrl+C 且有选区时复制」——Windows Terminal 的智能复制习惯，
        // 没选区时仍然往下走，保持 ^C 中断语义。
        let smart_copy = ctrl && ks.key == "c" && !mods.shift && self.has_selection();
        let explicit_copy = (ctrl && mods.shift && ks.key == "c") || (ctrl && ks.key == "insert");
        if explicit_copy || smart_copy {
            // 没有真正选区时不做任何事：单击只留下一个「零宽选区」，
            // 直接复制会把光标下那一个字塞进剪贴板。
            if self.has_selection() {
                self.copy_selection(cx);
                if smart_copy {
                    // 复制完清掉选区，否则第二次 Ctrl+C 还是复制，永远发不出 ^C
                    self.clear_selection();
                }
            }
            return;
        }

        // ---- 粘贴 ----
        if (ctrl && (ks.key == "v" || ks.key == "insert"))
            || (mods.shift && ks.key == "insert")
        {
            // 传输期间粘贴同样不能写进远端字节流（见下面的统一拦截）
            if self.zmodem.is_none() {
                self.paste_clipboard(cx);
            }
            return;
        }

        // ---- 传输期间锁住键盘输入 ----
        // Ctrl+C（中止，上面已处理）和复制（Ctrl+Shift+C）之外，任何按键都不能
        // 写进远端字节流：那会插进 ZMODEM 数据里把传输搞坏。真实终端在 ZMODEM
        // 传输时也会锁住键盘输入。
        if self.zmodem.is_some() {
            return;
        }

        // 可打印文本（含 CJK 输入法提交）：直接送字节
        if !mods.control && !mods.alt {
            if let Some(text) = &ks.key_char {
                if !text.is_empty() {
                    self.scroll_to_bottom(0);
                    self.write_input(text.clone().into_bytes());
                    return;
                }
            }
        }

        // 命名键 → VT 序列
        if let Some(seq) = encode_key(&ks.key, mods) {
            self.scroll_to_bottom(0);
            self.write_input(seq);
        }
    }

    /// 鼠标点击 → 选区起点 / 焦点请求。
    pub fn mouse_down(&mut self, col: usize, row: i32, mods: &Modifiers) -> bool {
        let content = self.term.renderable_content();
        let mode = content.mode;
        drop(content);
        if mode.intersects(
            TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION,
        ) {
            let b = match () {
                _ => 0u8, // 调用方只传左键信息，其余按钮由 app 层直接报告
            };
            let _ = b;
            self.mouse_report(mods, col, row, 0, true);
            // 鼠标交给远端程序了，本地选区不再有意义
            self.clear_selection();
            return true;
        }
        let point = self.point_at(col, row);
        self.selection = Some((point, point));
        true
    }

    /// 鼠标拖拽 → 选区扩展。返回是否处于鼠标报告模式。
    pub fn mouse_drag(&mut self, col: usize, row: i32, mods: &Modifiers) -> bool {
        let content = self.term.renderable_content();
        let mode = content.mode;
        drop(content);
        if mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) {
            self.mouse_report(mods, col, row, 32, true);
            return true;
        }
        let point = self.point_at(col, row);
        self.selection = self.selection.map(|(a, _)| (a, point));
        false
    }

    /// 鼠标抬起：非鼠标报告模式下结束选区。
    pub fn mouse_up(&mut self, col: usize, row: i32, mods: &Modifiers) -> bool {
        let content = self.term.renderable_content();
        let mode = content.mode;
        drop(content);
        if mode.intersects(
            TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION,
        ) {
            self.mouse_report(mods, col, row, 0, false);
            return true;
        }
        let point = self.point_at(col, row);
        self.selection = self.selection.map(|(a, _)| (a, point));
        false
    }

    /// 中键（鼠标报告模式时）。
    pub fn mouse_middle(&mut self, col: usize, row: i32, mods: &Modifiers, pressed: bool) -> bool {
        let content = self.term.renderable_content();
        let mode = content.mode;
        drop(content);
        if mode.intersects(
            TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION,
        ) {
            self.mouse_report(mods, col, row, 1, pressed);
            true
        } else {
            false
        }
    }

    /// 右键（鼠标报告模式时）。
    pub fn mouse_right(&mut self, col: usize, row: i32, mods: &Modifiers, pressed: bool) -> bool {
        let content = self.term.renderable_content();
        let mode = content.mode;
        drop(content);
        if mode.intersects(
            TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION,
        ) {
            self.mouse_report(mods, col, row, 2, pressed);
            true
        } else {
            false
        }
    }

    /// 鼠标移动（悬停，鼠标报告模式时）。
    pub fn mouse_motion(&mut self, col: usize, row: i32) -> bool {
        let content = self.term.renderable_content();
        let mode = content.mode;
        drop(content);
        if mode.contains(TermMode::MOUSE_MOTION) {
            self.mouse_report(&Modifiers::default(), col, row, 32, true);
            true
        } else {
            false
        }
    }

    /// 滚轮：返回 true 表示事件被终端消费（鼠标报告/备屏）。
    pub fn scroll(&mut self, delta: ScrollDelta) -> bool {
        let content = self.term.renderable_content();
        let mode = content.mode;
        drop(content);
        let up = match delta {
            ScrollDelta::Pixels(p) => p.y > px(0.0),
            ScrollDelta::Lines(l) => l.y > 0.0,
        };
        if mode.intersects(
            TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION,
        ) {
            let (row, col) = self.viewport_bottom_cell();
            let button = if up { 64 } else { 65 };
            self.mouse_report(&Modifiers::default(), col, row, button, true);
            return true;
        } else if mode.contains(TermMode::ALT_SCREEN) {
            // alternate scroll：换 ↑/↓
            let seq: &[u8] = if up { b"\x1b[A" } else { b"\x1b[B" };
            for _ in 0..3 {
                self.write_input(seq.to_vec());
            }
            return true;
        } else {
            let d: i32 = if up { 3 } else { -3 };
            self.term.scroll_display(Scroll::Delta(d));
            return false;
        }
    }

    fn viewport_bottom_cell(&self) -> (i32, usize) {
        (self.dims.rows as i32 - 1, 0)
    }

    /// 列/行 → 绝对坐标点。
    fn point_at(&self, col: usize, row: i32) -> Point<Line> {
        let content = self.term.renderable_content();
        let offset = content.display_offset as i32;
        drop(content);
        Point::new(Line(row - offset), Column(col.min(self.dims.cols as usize - 1)))
    }

    /// SGR(1006) 鼠标报告：positions 为视口坐标（1-based）。
    fn mouse_report(&mut self, mods: &Modifiers, col: usize, row: i32, button: u8, pressed: bool) {
        let mod_extra =
            (mods.shift as u8) | ((mods.alt as u8) << 1) | ((mods.control as u8) << 2);
        let code = button + mod_extra * 4;
        let action = if pressed { 'M' } else { 'm' };
        let seq = format!("\x1b[<{code};{};{}{}", col + 1, row + 1, action);
        self.write_input(seq.into_bytes());
    }

    /// 键入时若停在回滚历史之上，自动跳回底部（xterm 习惯）。
    fn scroll_to_bottom(&mut self, _display_offset: usize) {
        let content = self.term.renderable_content();
        let offset = content.display_offset;
        drop(content);
        if offset > 0 {
            self.term.scroll_display(Scroll::Bottom);
        }
    }
}

/* ----------------------------------------------------------------------
 * 绘制快照
 * ------------------------------------------------------------------- */

/// 单行文本样式段（相邻同样式已合并）。
#[derive(Clone, Debug)]
pub struct TermRun {
    pub text: String,
    pub fg: Hsla,
    pub bg: Option<Hsla>,
    pub underline: bool,
}

/// 单行快照。row 为视口行号。
#[derive(Clone)]
pub struct TermRow {
    pub row: i32,
    pub runs: Vec<TermRun>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CursorShape {
    Block,
    Hollow,
    Beam,
    Underline,
}

#[derive(Clone)]
pub struct TermCursor {
    pub bounds: Bounds<Pixels>,
    pub shape: CursorShape,
    pub color: Hsla,
}

/// 一帧终端绘制快照（坐标相对终端原点）。
#[derive(Clone)]
pub struct TermSnapshot {
    pub cell: Size<Pixels>,
    /// 与 fit_to 测量时相同的字号（保证 shape_line 推进与列宽一致）
    pub font_size: Pixels,
    pub rows: Vec<TermRow>,
    pub sel_quads: Vec<Bounds<Pixels>>,
    pub search_quads: Vec<Bounds<Pixels>>,
    pub search_current_quads: Vec<Bounds<Pixels>>,
    pub cursor: Option<TermCursor>,
    pub focused: bool,
    pub alt_screen: bool,
    pub mouse_mode: bool,
}

/// 行级 shaping 缓存。
///
/// 终端的绘制瓶颈不在「画」而在「排版」：每帧把每一行重新 `shape_line`
/// （字体 shaping + 字形定位）比上传一次纹理贵得多，而绝大多数行在两帧
/// 之间**一个字都没变**（真正在变的只有光标行和程序刚输出的那几行）。
/// 这里按「行文本 + 每段颜色/下划线 + 字号 + 字体族」算一个 64 位 key，
/// 命中就直接复用上一帧的 `ShapedLine`。
///
/// key 只含**内容**、不含行号：滚动时同一行换个位置，依然是命中。
pub struct LineShapeCache {
    map: std::collections::HashMap<u64, gpui_kit::ShapedLine>,
    /// 本帧引用到的 key：超限淘汰时保留它们，避免「刚排好又丢掉」
    used: std::collections::HashSet<u64>,
    /// 累计命中 / 未命中（可在状态栏观察缓存是否真的生效）
    pub hits: u64,
    pub misses: u64,
}

impl LineShapeCache {
    pub fn new() -> Self {
        Self {
            map: std::collections::HashMap::new(),
            used: std::collections::HashSet::with_capacity(128),
            hits: 0,
            misses: 0,
        }
    }

    /// 一帧开始：清掉「本帧用到」的标记。
    pub fn begin_frame(&mut self) {
        self.used.clear();
    }

    /// 一帧结束：超过上限就丢掉本帧没再用到的行。
    ///
    /// 不做 LRU：终端缓存的主体就是「当前可见的几十行」，
    /// 每帧清一次非活跃项已经够用，实现也简单。
    pub fn end_frame(&mut self) {
        const LIMIT: usize = 2048;
        if self.map.len() > LIMIT {
            let used = &self.used;
            self.map.retain(|k, _| used.contains(k));
        }
    }
}

impl Default for LineShapeCache {
    fn default() -> Self {
        Self::new()
    }
}

/// 把 Hsla 的四个分量打进 hash（`Hsla` 本身没实现 `Hash`）。
fn hash_color(c: &Hsla, h: &mut std::collections::hash_map::DefaultHasher) {
    use std::hash::Hash as _;
    c.h.to_bits().hash(h);
    c.s.to_bits().hash(h);
    c.l.to_bits().hash(h);
    c.a.to_bits().hash(h);
}

/// 行 key：内容 + 样式 + （由 `base` 携带的）字号与字体族。
fn line_key(base: u64, row: &TermRow) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash as _, Hasher as _};
    let mut h = DefaultHasher::new();
    base.hash(&mut h);
    for r in &row.runs {
        r.text.hash(&mut h);
        hash_color(&r.fg, &mut h);
        match &r.bg {
            Some(bg) => {
                1u8.hash(&mut h);
                hash_color(bg, &mut h);
            }
            None => 0u8.hash(&mut h),
        }
        r.underline.hash(&mut h);
    }
    h.finish()
}

/// 把快照绘制到 window（origin 为终端区域原点）。
///
/// `cache` 由面板持有（一个终端一份）：这样同一格终端跨帧复用同一批
/// `ShapedLine`，分屏时各格互不干扰。
pub fn paint_snapshot(
    snap: &TermSnapshot,
    origin: GPoint<Pixels>,
    cache: &mut LineShapeCache,
    window: &mut Window,
    cx: &mut App,
) {
    // 选区（画在文字下面）
    for b in &snap.sel_quads {
        window.paint_quad(fill(*b + origin, hsla(0.606, 0.55, 0.487, 0.39)));
    }
    // 查找高亮
    for b in &snap.search_quads {
        window.paint_quad(fill(*b + origin, hsla(0.16, 0.5, 0.42, 0.27)));
    }
    for b in &snap.search_current_quads {
        window.paint_quad(fill(*b + origin, hsla(0.11, 0.9, 0.62, 0.47)));
    }

    // 文本
    let font = terminal_font();

    // 与行内容无关的那部分 key（字号 + 字体族），每帧只算一次
    let base = {
        use std::hash::{Hash as _, Hasher as _};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        snap.font_size.as_f32().to_bits().hash(&mut h);
        crate::fonts::terminal_font_family().hash(&mut h);
        h.finish()
    };

    cache.begin_frame();
    for row in &snap.rows {
        let key = line_key(base, row);
        cache.used.insert(key);

        if !cache.map.contains_key(&key) {
            // 只有未命中才拼字符串 + shaping
            let mut text = String::new();
            let mut runs: Vec<TextRun> = Vec::new();
            for r in &row.runs {
                let byte_len = r.text.len();
                text.push_str(&r.text);
                runs.push(TextRun {
                    len: byte_len,
                    font: font.clone(),
                    color: r.fg,
                    background_color: r.bg,
                    underline: None,
                    strikethrough: None,
                });
            }
            if text.is_empty() {
                continue;
            }
            let shaped = window.text_system().shape_line(
                SharedString::from(text),
                snap.font_size,
                &runs,
                None,
            );
            cache.misses += 1;
            cache.map.insert(key, shaped);
        } else {
            cache.hits += 1;
        }

        let Some(shaped) = cache.map.get(&key) else {
            continue;
        };
        let text_len = shaped.text.len();
        if text_len == 0 {
            continue;
        }
        let line_origin =
            GPoint::new(origin.x, origin.y + px(row.row as f32 * snap.cell.height.as_f32()));
        let _ = shaped.paint(
            line_origin,
            snap.cell.height,
            gpui_kit::TextAlign::Left,
            None,
            window,
            cx,
        );
        // 下划线（按字节占比估算每段的水平位置，与等宽字体一致）
        let mut x = 0.0f32;
        for r in &row.runs {
            let run_w = shaped.width.as_f32() * (r.text.len() as f32 / text_len as f32);
            if r.underline {
                let ub = Bounds::new(
                    GPoint::new(origin.x + px(x), line_origin.y + snap.cell.height - px(1.5)),
                    size(px(run_w), px(1.0)),
                );
                window.paint_quad(fill(ub, r.fg));
            }
            x += run_w;
        }
    }
    cache.end_frame();

    // 光标
    if let Some(cur) = &snap.cursor {
        let b = cur.bounds + origin;
        // 失焦时光标画成空心（终端没获得键盘焦点，用户一眼能看出来）
        let shape = if snap.focused {
            cur.shape
        } else {
            match cur.shape {
                CursorShape::Block => CursorShape::Hollow,
                other => other,
            }
        };
        match shape {
            CursorShape::Block => {
                window.paint_quad(fill(b, hsla(0.6, 0.3, 0.9, 0.27)));
                window.paint_quad(quad(
                    b,
                    gpui_kit::Corners::all(px(0.0)),
                    gpui_kit::transparent_black(),
                    gpui_kit::Edges::all(px(1.0)),
                    cur.color,
                    gpui_kit::BorderStyle::Solid,
                ));
            }
            CursorShape::Hollow => {
                window.paint_quad(quad(
                    b,
                    gpui_kit::Corners::all(px(0.0)),
                    gpui_kit::transparent_black(),
                    gpui_kit::Edges::all(px(1.5)),
                    cur.color,
                    gpui_kit::BorderStyle::Solid,
                ));
            }
            CursorShape::Beam => {
                let bb =
                    Bounds::new(b.origin, size(px(b.size.width.as_f32() * 0.18), b.size.height));
                window.paint_quad(fill(bb, cur.color));
            }
            CursorShape::Underline => {
                let bb = Bounds::new(
                    GPoint::new(b.origin.x, b.origin.y + px(b.size.height.as_f32() * 0.78)),
                    size(b.size.width, px(b.size.height.as_f32() * 0.22)),
                );
                window.paint_quad(fill(bb, cur.color));
            }
        }
    }
}

/// 终端等宽字体。
pub fn terminal_font() -> Font {
    Font {
        family: crate::fonts::terminal_font_family().into(),
        weight: gpui_kit::FontWeight::NORMAL,
        style: gpui_kit::FontStyle::Normal,
        features: Default::default(),
        fallbacks: None,
    }
}

/// 写系统剪贴板。
pub fn copy_to_clipboard(cx: &mut App, text: String) {
    use gpui_kit::ClipboardItem;
    cx.write_to_clipboard(ClipboardItem::new_string(text));
}

/// 读系统剪贴板文本。
pub fn read_clipboard(cx: &App) -> Option<String> {
    cx.read_from_clipboard().and_then(|item| {
        if let Some(text) = item.text() {
            if !text.is_empty() {
                return Some(text);
            }
        }
        None
    })
}

/// 归一化选区（保证 a 在 b 之前）。
pub fn normalized_selection(
    sel: &Option<(Point<Line>, Point<Line>)>,
) -> Option<(Point<Line>, Point<Line>)> {
    let (a, b) = sel.clone()?;
    let (la, ca) = (a.line.0, a.column.0);
    let (lb, cb) = (b.line.0, b.column.0);
    if (la, ca) <= (lb, cb) {
        Some((a, b))
    } else {
        Some((b, a))
    }
}

/// GPUI 键名 → VT 字节。返回 None 表示该事件被 UI 功能吃掉。
/// 可打印字符（含 AltGr/中文输入）走 Keystroke::key_char，
/// 这里只处理命名键与 Ctrl 控制码。
fn encode_key(key: &str, mods: &Modifiers) -> Option<Vec<u8>> {
    // ---- Ctrl 控制码 ----
    if mods.control && !mods.alt {
        if let Some(code) = ctrl_code(key) {
            return Some(vec![code]);
        }
    }

    // ---- 命名键 → CSI 序列 ----
    let mod_param = 1
        + (mods.shift as i32)
        + ((mods.alt as i32) << 1)
        + ((mods.control as i32) << 2);
    let mod_suffix = if mod_param > 1 {
        format!(";{mod_param}")
    } else {
        String::new()
    };

    // 带字母的 CSI 键（A-D/H/F）
    if let Some(csi_char) = match key {
        "up" => Some(if mod_param > 1 { 'a' } else { 'A' }),
        "down" => Some(if mod_param > 1 { 'b' } else { 'B' }),
        "right" => Some(if mod_param > 1 { 'c' } else { 'C' }),
        "left" => Some(if mod_param > 1 { 'd' } else { 'D' }),
        "home" => Some(if mod_param > 1 { 'h' } else { 'H' }),
        "end" => Some(if mod_param > 1 { 'f' } else { 'F' }),
        _ => None,
    } {
        use std::fmt::Write as _;
        let mut seq = String::from("\x1b[");
        if mod_param > 1 {
            let _ = write!(seq, "1{mod_suffix}");
        }
        seq.push(csi_char);
        return Some(seq.into_bytes());
    }

    // ~ 结尾的 CSI 键
    let csi_tilde: Option<u32> = match key {
        "insert" => Some(2),
        "delete" => Some(3),
        "pageup" => Some(5),
        "pagedown" => Some(6),
        "f5" => Some(15),
        "f6" => Some(17),
        "f7" => Some(18),
        "f8" => Some(19),
        "f9" => Some(20),
        "f10" => Some(21),
        "f11" => Some(23),
        "f12" => Some(24),
        _ => None,
    };
    if let Some(n) = csi_tilde {
        use std::fmt::Write as _;
        let mut seq = String::from("\x1b[");
        let _ = write!(seq, "{n}{mod_suffix}~");
        return Some(seq.into_bytes());
    }
    // F1-F4（SS 前缀）
    if mod_param == 1 {
        match key {
            "f1" => return Some(b"\x1bOP".to_vec()),
            "f2" => return Some(b"\x1bOQ".to_vec()),
            "f3" => return Some(b"\x1bOR".to_vec()),
            "f4" => return Some(b"\x1bOS".to_vec()),
            _ => {}
        }
    }

    match key {
        "enter" => Some(b"\r".to_vec()),
        "tab" => Some(b"\t".to_vec()),
        "backspace" => Some(vec![0x7f]),
        "escape" => Some(b"\x1b".to_vec()),
        _ => None,
    }
}

/// Ctrl+字母 → 控制码（^A=1 … ^Z=26）。
fn ctrl_code(key: &str) -> Option<u8> {
    let mut chars = key.chars();
    let (c, rest) = (chars.next()?, chars.as_str());
    if !rest.is_empty() || !c.is_ascii_alphabetic() {
        return None;
    }
    Some((c.to_ascii_uppercase() as u8) & 0x1f)
}

/* ---- 颜色 ---- */

fn rgb(r: u8, g: u8, b: u8) -> Hsla {
    // 与 egui Color32::from_rgb 等价的 sRGB → Hsla（GPUI 的 hsla 为线性色空间，
    // 但终端配色使用 hsla 近似即可；显示效果与旧版差异可接受）。
    let (h, s, l) = rgb_to_hsl(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    hsla(h, s, l, 1.0)
}

fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h / 6.0, s, l)
}

fn named_color(n: vte::ansi::NamedColor) -> Hsla {
    use vte::ansi::NamedColor;
    let i = match n {
        NamedColor::Black | NamedColor::DimBlack => 0,
        NamedColor::Red | NamedColor::DimRed => 1,
        NamedColor::Green | NamedColor::DimGreen => 2,
        NamedColor::Yellow | NamedColor::DimYellow => 3,
        NamedColor::Blue | NamedColor::DimBlue => 4,
        NamedColor::Magenta | NamedColor::DimMagenta => 5,
        NamedColor::Cyan | NamedColor::DimCyan => 6,
        NamedColor::White | NamedColor::DimWhite => 7,
        NamedColor::BrightBlack => 8,
        NamedColor::BrightRed => 9,
        NamedColor::BrightGreen => 10,
        NamedColor::BrightYellow => 11,
        NamedColor::BrightBlue => 12,
        NamedColor::BrightMagenta => 13,
        NamedColor::BrightCyan => 14,
        NamedColor::BrightWhite | NamedColor::BrightForeground => 15,
        NamedColor::Foreground => return FG_DEFAULT,
        NamedColor::Background => return BG,
        NamedColor::Cursor => return rgb(240, 244, 252),
        NamedColor::DimForeground => return FG_DEFAULT,
    };
    rgb_by_index(i)
}

fn rgb_by_index(i: usize) -> Hsla {
    const RGB16: [[u8; 3]; 16] = [
        [30, 30, 34],
        [234, 78, 102],
        [69, 197, 112],
        [238, 190, 70],
        [86, 143, 238],
        [197, 108, 234],
        [72, 202, 224],
        [200, 208, 220],
        [90, 96, 110],
        [255, 120, 140],
        [120, 240, 160],
        [255, 220, 120],
        [130, 180, 255],
        [230, 160, 255],
        [130, 240, 255],
        [240, 244, 252],
    ];
    let c = RGB16[i.min(15)];
    rgb(c[0], c[1], c[2])
}

/// xterm 256 色表（内置静态色板）。
fn palette_color(i: u8) -> Hsla {
    if i < 16 {
        return rgb_by_index(i as usize);
    }
    if i < 232 {
        let i = i - 16;
        let [r, g, b] = [((i / 36) % 6) as u8, ((i / 6) % 6) as u8, (i % 6) as u8];
        let ch = |v: u8| -> u8 {
            if v == 0 {
                0
            } else {
                v * 40 + 55
            }
        };
        return rgb(ch(r), ch(g), ch(b));
    }
    let v = 8 + (i - 232) * 10;
    rgb(v, v, v)
}

fn resolve_color(c: vte::ansi::Color, colors: &Colors, is_fg: bool, flags: Flags) -> Hsla {
    // OSC10/11/256 改色后的动态覆盖优先，未覆盖时才落回内置色板
    let base = match c {
        vte::ansi::Color::Spec(sc) => rgb(sc.r, sc.g, sc.b),
        vte::ansi::Color::Indexed(i) => match colors[i as usize] {
            Some(c) => rgb(c.r, c.g, c.b),
            None => palette_color(i),
        },
        vte::ansi::Color::Named(named) => match colors[named] {
            Some(c) => rgb(c.r, c.g, c.b),
            None => match named {
                vte::ansi::NamedColor::Foreground => FG_DEFAULT,
                vte::ansi::NamedColor::Background => BG,
                other => named_color(other),
            },
        },
    };
    let _ = is_fg;
    // BOLD 前景提亮 +8；DIM 减半
    if is_fg && flags.contains(Flags::BOLD) {
        if let vte::ansi::Color::Indexed(i) = c {
            if i < 8 {
                return palette_color(i + 8);
            }
        }
    }
    if flags.contains(Flags::DIM) {
        hsla(base.h, base.s, base.l * 0.6, base.a)
    } else {
        base
    }
}

/* ---- 背景/默认色常量（供状态栏等复用） ---- */

pub fn bg_color() -> Hsla {
    BG
}

/* ----------------------------------------------------------------------
 * 测试
 * ------------------------------------------------------------------- */

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::TermIo;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::sync::mpsc;

    fn test_dir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("target")
            .join("zmodem-test-tmp")
            .join(format!("pane-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 造一个不连真后端的窗格：`output_rx` 是我们喂"远端字节"的入口，
    /// `input_tx` 是协议状态机回写远端的出口。真实会话里这两个通道分别由
    /// pty / SSH / telnet 的后端线程持有，行为完全一样。
    fn test_pane(
        download: &std::path::Path,
    ) -> (
        TermPane,
        mpsc::UnboundedReceiver<Vec<u8>>,
        mpsc::UnboundedSender<Vec<u8>>,
    ) {
        let (input_tx, input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, _resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();
        let (output_tx, output_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let io = TermIo {
            input_tx,
            resize_tx,
            output_rx,
            ssh: None,
        };
        let pane = TermPane::new("zmodem-test".to_string(), io, 80, 24);
        // 接收目录走配置全局量（真实运行也是这么取的）
        crate::config::set_zmodem(true, Some(download.display().to_string()));
        (pane, input_rx, output_tx)
    }

    /// 集成测试：远端 `sz`（用本项目自己的发送端扮演）经过 `TermPane::pump()`
    /// 的字节漏斗把文件送进窗格。一次覆盖四个验收点：
    /// ① 触发头被识别、之后的字节不再进 vte；
    /// ② 文件按消毒名落盘且字节完全一致；
    /// ③ 会话结束自动回正常模式（状态栏/终端都恢复）；
    /// ④ 会话前后的正常输出不受影响，协议字节不会漏进终端画面。
    #[test]
    fn pump_hijacks_zmodem_runs_session_and_restores_normal_mode() {
        let dir = test_dir("recv");
        let (mut pane, mut remote_in, remote_out) = test_pane(&dir);

        // ① 会话前的正常输出原样显示，且不产生任何回写
        remote_out.send(b"before: hello\r\n".to_vec()).unwrap();
        assert!(pane.pump());
        assert!(pane.context_text(30, 200).contains("before: hello"));
        assert!(
            remote_in.try_recv().is_err(),
            "普通输出不该触发任何回写"
        );
        assert!(!pane.zmodem_active());

        // ② 用本项目的发送端扮演远端 `sz`：它发出的第一段就是 ZRQINIT（`**\x18B00`）
        let src_dir = test_dir("recv-src");
        let payload: Vec<u8> = (0..4096u32)
            .flat_map(|i| {
                [0x18u8, 0x11, 0x0d]
                    .into_iter()
                    .chain(std::iter::once((i % 251) as u8))
            })
            .collect();
        let src = src_dir.join("remote.bin");
        std::fs::write(&src, &payload).unwrap();
        let mut remote = zmodem::Session::send().unwrap();
        assert_eq!(remote.add_files(vec![src]), 1);

        let mut steps = 0usize;
        while !(remote.is_done() && !pane.zmodem_active()) {
            steps += 1;
            assert!(steps < 200_000, "窗格集成传输没有结束（协议死锁？）");
            {
                let tx = remote_out.clone();
                let mut wire = |b: &[u8]| {
                    let _ = tx.send(b.to_vec());
                };
                remote.drive(&mut wire);
            }
            pane.pump();
            while let Ok(bytes) = remote_in.try_recv() {
                remote.feed(&bytes);
            }
        }

        // ③ 文件落盘、字节一致、名字正确
        let received = dir.join("remote.bin");
        assert!(received.exists(), "没有落到下载目录: {}", received.display());
        assert_eq!(
            std::fs::read(&received).unwrap(),
            payload,
            "收到的字节与发端不一致"
        );

        // ④ 回正常模式：状态栏清空，新输出照常进终端，且没有协议字节漏进来
        assert!(pane.zmodem_status().is_none());
        remote_out.send(b"after: prompt$ ".to_vec()).unwrap();
        assert!(pane.pump());
        let text = pane.context_text(30, 400);
        assert!(
            text.contains("after: prompt$"),
            "会话结束后正常输出没有恢复: {text:?}"
        );
        assert!(
            !text.contains("**"),
            "ZMODEM 协议字节漏进了终端画面: {text:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&src_dir);
    }
}
