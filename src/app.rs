//! 应用外壳（GPUI 版）：工具栏 + 快速连接栏 + 左侧会话栏 + 中央工作区（标签页）
//! + 右侧 SFTP / AI 助手面板 + 底部状态栏 + 各类对话框（覆盖层）。
//!
//! 标签页统一承载四类会话：本地 / SSH 终端、内置浏览器、远程桌面、SFTP。
//!
//! 与 egui 版的对应关系：
//! - `eframe::App::ui` → `Render::render`（声明式元素树，每帧重建）
//! - egui Window 对话框 → 绝对定位覆盖层（同帧渲染，无独立视口）
//! - egui 全局快捷键 → GPUI action（`cx.bind_keys` + 根节点 `on_action`）

use crate::ai_assistant::AiAssistantPanel;
use crate::ai_config_panel::AiConfigPanel;
use crate::browser::{BrowserService, BrowserTab};
use crate::config::AppConfig;
use crate::config_panel::ConfigPanel;
use crate::i18n::{tr, I18n};
use crate::macros::MacroPanel;
use crate::rdp::{RdTabProtocol, RdpConnectionProfile, RdpProfiles, RemoteDesktopTab};
use crate::sessions::{SessionKind, SessionStore};
use crate::sftp::SftpPanel;
use crate::split::SplitNode;
use crate::tabs::{PaneId, PaneState, TabContent, TerminalTab};
use crate::term::{self, SpawnCfg, TermPane, TermSnapshot};
use crate::ui_icons::ic;
use crate::widgets::password_field;
use gpui_kit::base::component_traits::{Disableable as _, Selectable as _};
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::component::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use tokio::runtime::Runtime;

/// 内置 Lucide 图标全集（`gpui-kit-assets` 提供完整目录）。
use gpui_kit::assets::IconName;

// ---------------------------------------------------------------------------
// 全局快捷键 action
// ---------------------------------------------------------------------------

gpui_kit::actions!(rustterm, [
    NextTab, PrevTab, NewLocalTab, NewBrowserTab, ToggleSftp, ToggleLocalFiles, ToggleRdp,
    ToggleBrowser, ToggleAi, CloseTab, ToggleFullscreen, FocusAddress, FindInTerminal,
    JumpTab1, JumpTab2, JumpTab3, JumpTab4, JumpTab5, JumpTab6, JumpTab7, JumpTab8, JumpTab9,
    MoveTabLeft, MoveTabRight,
    // 分屏（轻量版：只做「分」与「关」，不做拖拽换位 / 布局持久化）
    SplitRight, SplitDown, ClosePane, NextPane,
]);

/// 浏览器内容区 bounds（canvas prepaint 写入，下一帧同步 WebView 子窗口）。
///
/// 带标签索引：`BrowserArea` 是全局量，多个浏览器标签页共用一个槽位。
/// 切换标签的那一帧，新标签的 canvas 还没 prepaint，槽位里仍是**上一个**
/// 浏览器标签的矩形；若不校验索引，就会把旧矩形同步给新标签，WebView
/// 会先出现在错误位置（盖住工具栏/地址栏，导致这些控件点不到）。
struct BrowserArea(Bounds<Pixels>, usize);
impl Global for BrowserArea {}

// ---------------------------------------------------------------------------
// 表单输入组
// ---------------------------------------------------------------------------

/// SSH 连接表单的输入框实体。
#[derive(Clone)]
pub struct SshFormInputs {
    pub host: Entity<InputState>,
    pub port: Entity<InputState>,
    pub user: Entity<InputState>,
    pub password: Entity<InputState>,
    pub key_path: Entity<InputState>,
    pub key_passphrase: Entity<InputState>,
    pub auth_key: bool,
}

/// 新建/编辑会话表单的输入框实体。
#[derive(Clone)]
pub struct AddFormInputs {
    pub name: Entity<InputState>,
    pub host: Entity<InputState>,
    pub port: Entity<InputState>,
    pub user: Entity<InputState>,
    pub password: Entity<InputState>,
    pub key_path: Entity<InputState>,
    pub domain: Entity<InputState>,
    pub kind: SessionKind,
    pub auth_key: bool,
    /// Some(id) = 编辑已有会话
    pub editing: Option<String>,
}

// ---------------------------------------------------------------------------
// RustTermApp
// ---------------------------------------------------------------------------

pub struct RustTermApp {
    pub runtime: Arc<Runtime>,
    /// 统一标签页集合（终端 / 浏览器 / 远程桌面 / SFTP）
    pub tabs: Vec<TabContent>,
    pub active: usize,
    /// 终端键盘焦点
    pub term_focus: Option<FocusHandle>,
    /// 根容器焦点：没有任何标签页时也要有个焦点节点，
    /// 否则按键事件没有分发路径，Ctrl+N / Ctrl+T 等全局快捷键全部失灵。
    pub root_focus: Option<FocusHandle>,
    /// 本帧各窗格的绘制快照（pane id → 快照，render 阶段生成，canvas 消费）。
    ///
    /// 分屏后一屏可能有多个终端，原来「本帧一个 term_snap」的单例假设必须拆掉。
    pub pane_snaps: HashMap<PaneId, Arc<TermSnapshot>>,
    /// 各窗格「终端绘制区」上一帧的 bounds（canvas prepaint 写入）。
    ///
    /// 两个用途：
    /// 1. 下一帧按其尺寸 `fit_to`（分屏后各格尺寸不同，不能再用视口估算）；
    /// 2. 鼠标命中判定 + 列/行换算（取代原来的全局 `TermArea`）。
    pub pane_areas: HashMap<PaneId, Rc<Cell<Bounds<Pixels>>>>,
    /// 鼠标按下时锁定的窗格：拖动选区跨出本格后仍然投递给它，直到松开。
    pub mouse_pane: Option<PaneId>,

    // 功能面板
    pub config: AppConfig,
    pub i18n: I18n,
    pub config_panel: ConfigPanel,
    pub ai_config_panel: AiConfigPanel,
    pub ai_assistant: AiAssistantPanel,
    pub sftp: SftpPanel,
    pub macros: MacroPanel,
    pub rdp_profiles: RdpProfiles,
    pub browser_service: BrowserService,

    next_browser_id: usize,
    pub show_browser_history: bool,
    pub sessions: SessionStore,
    pub sidebar_tab: SidebarTab,
    pub selected_session: Option<String>,
    pub search: String,
    /// 侧栏搜索框（InputState 实体，首帧创建）
    pub search_input: Option<Entity<InputState>>,
    /// 快速连接输入（InputState 实体，首帧创建）
    pub quick_input: Option<Entity<InputState>>,
    pub jump_input: Option<Entity<InputState>>,
    pub quick_secret: Option<Entity<InputState>>,
    pub quick_auth: String,
    /// 新建/编辑会话窗口
    pub add_form: Option<AddFormInputs>,
    /// SSH 连接窗口
    pub ssh_form: Option<SshFormInputs>,
    pub ssh_form_open: bool,
    pub ssh_form_error: Option<String>,
    /// SSH 表单预填内容（主机, 端口）——端口扫描结果双击后带过来
    pub ssh_form_prefill: Option<(String, u16)>,
    /// 关于窗口
    pub about_open: bool,
    /// 临时提示（状态栏显示）
    pub toast: Option<String>,
    /// 终端查找栏
    pub find_open: bool,
    pub find_input: Option<Entity<InputState>>,
    /// 请求把焦点交给查找输入框（Ctrl+F / 工具栏「查找」时置位）
    pub find_focus: bool,
    /// 请求把焦点交回终端（关闭查找栏后置位）
    pub focus_term: bool,
    /// 终端右键菜单的弹出位置（窗口坐标），None = 未打开
    pub term_menu: Option<Point<Pixels>>,
    /// 上一帧的活动标签
    pub last_active: usize,
    /// Telnet 表单
    pub telnet_open: bool,
    pub telnet_host: Option<Entity<InputState>>,
    pub telnet_port: Option<Entity<InputState>>,
    /// Ping 表单
    pub ping_open: bool,
    pub ping_host: Option<Entity<InputState>>,
    /// 端口扫描对话框
    pub port_scan: crate::portscan::PortScanPanel,
    /// 批量执行命令
    pub batch_open: bool,
    pub batch_cmd: Option<Entity<InputState>>,
    /// 同步输入
    pub sync_input: bool,
    pub sync_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    pub sync_tx: Option<std::sync::mpsc::Sender<Vec<u8>>>,
    /// 上一活动标签的 SSH 会话身份
    pub last_ssh_key: Option<usize>,
    /// 端口隧道
    pub tunnel_open: bool,
    pub tunnels: Vec<crate::tunnel::TunnelHandle>,
    pub tunnel_next_id: u64,
    pub tunnel_local: Option<Entity<InputState>>,
    pub tunnel_host: Option<Entity<InputState>>,
    pub tunnel_port: Option<Entity<InputState>>,
    /// 请求把焦点给当前对话框首个输入框
    pub focus_dialog: bool,
    /// 请求把焦点给浏览器地址栏
    pub focus_address: bool,
    /// 菜单栏点击了「新建会话」：真正的表单构建需要 `&mut Window`，
    /// 而菜单回调只有 `&mut App`，因此置位后在下一帧 render 里执行。
    pub pending_new_session: bool,
    /// 一次性初始化完成标记
    initialized: bool,
}

#[derive(Clone, Copy, PartialEq)]
pub enum SidebarTab {
    Sessions,
    Tools,
    Macros,
}

/// 工具栏功能入口 → 打开对应标签页 / 对话框。
#[derive(Clone, Copy, PartialEq)]
pub enum ToolAction {
    LocalTerminal,
    SshClient,
    SftpClient,
    TelnetClient,
    RdpClient,
    VncClient,
    SpiceClient,
    LocalFiles,
    Browser,
    Ping,
    Find,
    Sync,
    Batch,
    Tunnel,
    Macro,
    PortScan,
}

/// 构造一个下拉菜单项。
///
/// - `action`：仅用于在菜单右侧渲染快捷键提示。点击行为一律由 `op` 决定
///   ——菜单渲染在 popover 层内，不是根容器的 DOM 后代，GPUI 的 action
///   冒泡到不了根节点上的 `on_action`）。
/// - `checked`：渲染勾选标记（当前生效状态）。
/// - `op`：真正的业务操作，直接改 `RustTermApp` 状态。
fn menu_item(
    app: &WeakEntity<RustTermApp>,
    label: impl Into<SharedString>,
    action: Option<Box<dyn Action>>,
    checked: bool,
    op: impl Fn(&mut RustTermApp, &mut Context<RustTermApp>) + 'static,
) -> PopupMenuItem {
    let app = app.clone();
    let mut item = PopupMenuItem::new(label);
    if let Some(a) = action {
        item = item.action(a);
    }
    if checked {
        item = item.checked(true);
    }
    item.on_click(move |_evt, _window, cx| {
        let _ = app.update(cx, |this, cx| {
            op(this, cx);
            cx.notify();
        });
    })
}

impl RustTermApp {
    pub fn new() -> Self {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime"),
        );
        let config = AppConfig::load();
        let mut i18n = I18n::default();
        i18n.set_lang(&config.language);
        Self {
            runtime,
            tabs: Vec::new(),
            active: 0,
            term_focus: None,
            root_focus: None,
            pane_snaps: HashMap::new(),
            pane_areas: HashMap::new(),
            mouse_pane: None,
            config,
            i18n,
            config_panel: ConfigPanel::default(),
            ai_config_panel: AiConfigPanel::default(),
            ai_assistant: AiAssistantPanel::default(),
            sftp: SftpPanel::default(),
            macros: MacroPanel::default(),
            rdp_profiles: RdpProfiles::default(),
            browser_service: BrowserService::default(),
            next_browser_id: 1,
            show_browser_history: false,
            sessions: SessionStore::load(),
            sidebar_tab: SidebarTab::Sessions,
            selected_session: None,
            search: String::new(),
            search_input: None,
            quick_input: None,
            jump_input: None,
            quick_secret: None,
            quick_auth: "password".to_string(),
            add_form: None,
            ssh_form: None,
            ssh_form_open: false,
            ssh_form_error: None,
            ssh_form_prefill: None,
            about_open: false,
            toast: None,
            find_open: false,
            find_input: None,
            find_focus: false,
            focus_term: false,
            term_menu: None,
            last_active: 0,
            telnet_open: false,
            telnet_host: None,
            telnet_port: None,
            ping_open: false,
            ping_host: None,
            port_scan: Default::default(),
            batch_open: false,
            batch_cmd: None,
            sync_input: false,
            sync_rx: None,
            sync_tx: None,
            last_ssh_key: None,
            tunnel_open: false,
            tunnels: Vec::new(),
            tunnel_next_id: 1,
            tunnel_local: None,
            tunnel_host: None,
            tunnel_port: None,
            focus_dialog: false,
            focus_address: false,
            pending_new_session: false,
            initialized: false,
        }
    }

    pub fn set_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some(msg.into());
    }

    /// 当前活动标签的浏览器（渲染/订阅回调用）。
    pub fn active_browser(&self) -> Option<&crate::browser::BrowserTab> {
        match self.tabs.get(self.active) {
            Some(TabContent::Browser(b)) => Some(b),
            _ => None,
        }
    }

    pub fn active_browser_mut(&mut self) -> Option<&mut crate::browser::BrowserTab> {
        match self.tabs.get_mut(self.active) {
            Some(TabContent::Browser(b)) => Some(b),
            _ => None,
        }
    }

    pub fn runtime_handle(&self) -> tokio::runtime::Handle {
        self.runtime.handle().clone()
    }

    /// 快速连接输入框当前文本。
    pub fn input_value(state: &Option<Entity<InputState>>, cx: &App) -> String {
        state
            .as_ref()
            .map(|s| s.read(cx).value().to_string())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------------
    // 标签页创建
    // ------------------------------------------------------------------

    pub fn open_local(&mut self) {
        let title = format!("本地 {}", self.tabs.len() + 1);
        match TermPane::new_local(title, 80, 24) {
            Ok(pane) => self.push_tab(TabContent::Terminal(TerminalTab::new(pane))),
            Err(e) => self.set_toast(format!("本地终端启动失败: {e:#}")),
        }
    }

    pub fn submit_ssh(
        &mut self,
        host: &str,
        port: u16,
        user: &str,
        auth: crate::backends::ssh::SshAuth,
        jump: Option<crate::backends::ssh::JumpHost>,
    ) {
        let title = match &jump {
            Some(j) => format!("{user}@{host}（经 {}）", j.host),
            None => format!("{user}@{host}"),
        };
        let host = host.trim().to_string();
        let user = user.trim().to_string();
        let runtime = self.runtime.clone();
        let tab_title = format!("{user}@{host}");

        let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<TermPane>>();
        std::thread::spawn(move || {
            let result = runtime
                .block_on(TermPane::new_ssh(tab_title, host, port, user, auth, jump, 80, 24));
            let _ = tx.send(result);
        });

        self.push_tab(TabContent::Connecting { title, rx });
    }

    pub fn open_browser(&mut self, url: &str) {
        let tab = BrowserTab::new(self.next_browser_id, url.to_string());
        self.next_browser_id += 1;
        self.push_tab(TabContent::Browser(tab));
    }

    pub fn open_remote_desktop(&mut self, protocol: RdTabProtocol) {
        let profile = RdpConnectionProfile {
            name: format!("新建 {}", protocol.label()),
            protocol,
            host: String::new(),
            port: protocol.default_port(),
            ..Default::default()
        };
        self.push_tab(TabContent::RemoteDesktop(RemoteDesktopTab::new(profile)));
    }

    pub fn open_sftp(&mut self) {
        self.sftp.toggle();
    }

    pub fn open_local_files(&mut self) {
        if let Some(idx) = self
            .tabs
            .iter()
            .position(|t| matches!(t, TabContent::LocalFiles(_)))
        {
            self.active = idx;
            return;
        }
        self.push_tab(TabContent::LocalFiles(crate::filebrowser::FileBrowser::default()));
    }

    /// 打开键宏管理器标签页（已存在则切过去）。
    pub fn open_macro_tab(&mut self) {
        if let Some(idx) = self.tabs.iter().position(|t| matches!(t, TabContent::Macro)) {
            self.active = idx;
            return;
        }
        self.push_tab(TabContent::Macro);
    }

    /// 打开端口扫描标签页（已存在则切过去）。
    pub fn open_port_scan_tab(&mut self) {
        if let Some(idx) = self.tabs.iter().position(|t| matches!(t, TabContent::PortScan)) {
            self.active = idx;
            return;
        }
        self.push_tab(TabContent::PortScan);
    }

    /// 停止端口扫描（供 UI 调用）。
    pub fn port_scan_stop(&mut self) {
        self.port_scan.stop();
    }

    /// 用给定主机 / 端口打开 SSH 连接表单（端口扫描结果双击 → 直接连）。
    pub fn open_ssh_for(&mut self, host: &str, port: u16) {
        self.ssh_form_prefill = Some((host.to_string(), port));
        // 丢掉旧表单，下一帧重建时把预填值写进去
        self.ssh_form = None;
        self.ssh_form_open = true;
        self.ssh_form_error = None;
        self.focus_dialog = true;
    }

    pub fn push_tab(&mut self, content: TabContent) {
        let is_term = content.terminal().is_some();
        self.tabs.push(content);
        self.active = self.tabs.len() - 1;
        // 新终端直接吃掉键盘焦点：否则还要用户先点一下终端才能打字
        if is_term {
            self.focus_term = true;
        }
    }

    /// 解析快速连接输入 "user@host:port" / "host" / "host:port"。
    fn parse_quick_connect(s: &str) -> Option<(Option<String>, String, u16)> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        let (user, rest) = match s.split_once('@') {
            Some((u, r)) => (Some(u.trim().to_string()), r.trim().to_string()),
            None => (None, s.to_string()),
        };
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) => (h.trim().to_string(), p.parse().unwrap_or(22)),
            None => (rest, 22),
        };
        if host.is_empty() {
            None
        } else {
            Some((user, host, port))
        }
    }

    /// 快速连接（listener 中取好输入值后调用）。
    pub fn quick_connect_submit(&mut self, addr: String, jump_host: String, secret: String) {
        let parsed = Self::parse_quick_connect(&addr);
        let Some((user, host, port)) = parsed else {
            self.set_toast("快速连接格式: user@host:22");
            return;
        };
        let user = user.unwrap_or_else(|| "root".to_string());

        let is_key = self.quick_auth.eq_ignore_ascii_case("key");
        let auth = if is_key {
            crate::backends::ssh::SshAuth::Key {
                path: secret.trim().to_string(),
                passphrase: String::new(),
            }
        } else {
            crate::backends::ssh::SshAuth::Password(secret.clone())
        };

        if auth.is_empty() {
            self.set_toast(if is_key {
                "请先选择或填写私钥路径"
            } else {
                "未填写密码，将尝试空密码登录"
            });
            if is_key {
                return;
            }
        }

        let jump = crate::backends::ssh::JumpHost::parse(&jump_host, &user, auth.clone());
        if jump.is_some() {
            self.set_toast(format!(
                "经跳板机 {} 连接 {host}:{port}",
                jump.as_ref().map(|j| j.host.clone()).unwrap_or_default()
            ));
        }

        self.submit_ssh(&host, port, &user, auth, jump);
    }

    /// 连接已保存的会话（listener 中传 jump 文本）。
    pub fn connect_session(&mut self, id: &str, jump_host: &str) {
        let Some(s) = self.sessions.by_id(id).cloned() else {
            return;
        };
        self.sessions.touch_recent(&s.id);
        if let Err(e) = self.sessions.save() {
            self.set_toast(e);
        }
        match s.kind {
            SessionKind::Ssh => {
                let auth = if s.uses_key() {
                    crate::backends::ssh::SshAuth::Key {
                        path: s.key_path.clone(),
                        passphrase: String::new(),
                    }
                } else {
                    crate::backends::ssh::SshAuth::Password(s.password.clone())
                };
                let jump = crate::backends::ssh::JumpHost::parse(jump_host, &s.user, auth.clone());
                self.submit_ssh(&s.host, s.port, &s.user, auth, jump);
            }
            kind => {
                let proto = match kind {
                    SessionKind::Vnc => RdTabProtocol::Vnc,
                    SessionKind::Rdp => RdTabProtocol::Rdp,
                    SessionKind::Spice => RdTabProtocol::Spice,
                    SessionKind::Ssh => unreachable!(),
                };
                let name = s.display();
                let profile = RdpConnectionProfile {
                    name: name.clone(),
                    protocol: proto,
                    host: s.host.clone(),
                    port: s.port,
                    username: s.user.clone(),
                    password: s.password.clone(),
                    domain: s.domain.clone(),
                    ..Default::default()
                };
                self.rdp_profiles.ensure(profile.clone());
                let mut tab = RemoteDesktopTab::new(profile);
                tab.connect();
                self.push_tab(TabContent::RemoteDesktop(tab));
            }
        }
    }

    /// 执行工具栏 / 侧栏的动作。
    pub fn run_tool_action(&mut self, action: ToolAction) {
        match action {
            ToolAction::LocalTerminal => self.open_local(),
            ToolAction::SshClient => {
                self.ssh_form_open = true;
                self.ssh_form_error = None;
            }
            ToolAction::SftpClient => {
                if self.last_ssh_key.is_some() {
                    self.sftp.toggle();
                } else {
                    self.set_toast("SFTP 需要活动的 SSH 会话：先打开一个 SSH 终端标签");
                }
            }
            ToolAction::TelnetClient => {
                self.telnet_open = true;
                self.focus_dialog = true;
            }
            ToolAction::RdpClient => self.open_remote_desktop(RdTabProtocol::Rdp),
            ToolAction::VncClient => self.open_remote_desktop(RdTabProtocol::Vnc),
            ToolAction::SpiceClient => self.open_remote_desktop(RdTabProtocol::Spice),
            ToolAction::LocalFiles => self.open_local_files(),
            ToolAction::Browser => self.open_browser("about:blank"),
            ToolAction::Ping => {
                self.ping_open = true;
                self.focus_dialog = true;
            }
            ToolAction::Find => self.toggle_find(),
            ToolAction::Sync => self.toggle_sync_input(),
            ToolAction::Batch => {
                self.batch_open = true;
                self.focus_dialog = true;
            }
            ToolAction::Tunnel => self.tunnel_open = true,
            ToolAction::Macro => self.open_macro_tab(),
            ToolAction::PortScan => self.open_port_scan_tab(),
        }
    }

    /// 打开/关闭终端查找栏。打开时把焦点交给查找输入框。
    pub fn toggle_find(&mut self) {
        if self.tabs.get(self.active).and_then(|t| t.terminal()).is_none() {
            self.set_toast("查找仅对终端标签页可用");
            return;
        }
        self.find_open = !self.find_open;
        if self.find_open {
            self.find_focus = true;
        } else {
            let idx = self.active.min(self.tabs.len().saturating_sub(1));
            if let Some(pane) = self.tabs.get_mut(idx).and_then(|t| t.terminal_mut()) {
                pane.clear_search();
            }
            self.focus_term = true;
        }
    }

    pub fn toggle_sync_input(&mut self) {        self.sync_input = !self.sync_input;
        if self.sync_input {
            let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
            self.sync_tx = Some(tx);
            self.sync_rx = Some(rx);
            self.set_toast("同步输入已开启：活动终端的按键会复制到其它终端");
        } else {
            self.sync_tx = None;
            self.sync_rx = None;
            // 分屏后一屏可能有多个窗格，全部解除镜像
            for tab in self.tabs.iter_mut() {
                let Some(t) = tab.terminal_tab_mut() else {
                    continue;
                };
                for slot in t.panes.iter_mut() {
                    if let PaneState::Live(pane) = &mut slot.state {
                        pane.mirror = None;
                    }
                }
            }
            self.set_toast("同步输入已关闭");
        }
    }

    pub fn pump_sync_input(&mut self) {
        if !self.sync_input {
            return;
        }
        let Some(tx) = self.sync_tx.clone() else {
            return;
        };
        let active_tab = self.active.min(self.tabs.len().saturating_sub(1));
        let active_pane = self.active_pane_id();

        // 镜像源只有一个：活动标签的**活动窗格**；其余窗格（含同一标签的其它分屏）
        // 都要清掉，否则上个来源的按键会被继续复制出去
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            let Some(t) = tab.terminal_tab_mut() else {
                continue;
            };
            for slot in t.panes.iter_mut() {
                let PaneState::Live(pane) = &mut slot.state else {
                    continue;
                };
                let is_source = i == active_tab && Some(slot.id) == active_pane;
                if is_source {
                    if pane.mirror.is_none() {
                        pane.mirror = Some(tx.clone());
                    }
                } else if pane.mirror.is_some() {
                    pane.mirror = None;
                }
            }
        }

        let mut batch: Vec<Vec<u8>> = Vec::new();
        if let Some(rx) = &self.sync_rx {
            while let Ok(bytes) = rx.try_recv() {
                batch.push(bytes);
            }
        }
        if batch.is_empty() {
            return;
        }
        // 分发到除镜像源之外的每一个窗格
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            let Some(t) = tab.terminal_tab_mut() else {
                continue;
            };
            for slot in t.panes.iter_mut() {
                let PaneState::Live(pane) = &mut slot.state else {
                    continue;
                };
                if i == active_tab && Some(slot.id) == active_pane {
                    continue;
                }
                if pane.dead {
                    continue;
                }
                for b in &batch {
                    pane.write_input_local(b.clone());
                }
            }
        }
    }

    pub fn open_telnet(&mut self, host: &str, port: u16) {
        let host = host.trim().to_string();
        if host.is_empty() {
            self.set_toast("Telnet 主机不能为空");
            return;
        }
        let title = format!("Telnet {host}:{port}");
        match TermPane::new_tcp(title, host.clone(), port, 80, 24) {
            Ok(pane) => {
                self.push_tab(TabContent::Terminal(TerminalTab::new(pane)));
                self.set_toast(format!("已连接 {host}:{port}"));
            }
            Err(e) => self.set_toast(format!("Telnet 连接失败: {e:#}")),
        }
    }

    pub fn open_ping(&mut self, host: &str) {
        let host = host.trim().to_string();
        if host.is_empty() {
            self.set_toast("Ping 目标不能为空");
            return;
        }
        #[cfg(windows)]
        let (program, args) = ("ping.exe", vec!["-t".to_string(), host.clone()]);
        #[cfg(not(windows))]
        let (program, args) = ("ping", vec![host.clone()]);

        match TermPane::new_command(format!("Ping {host}"), program, &args, 100, 24) {
            Ok(pane) => self.push_tab(TabContent::Terminal(TerminalTab::new(pane))),
            Err(e) => self.set_toast(format!("Ping 启动失败: {e:#}")),
        }
    }

    // ------------------------------------------------------------------
    // 分屏（轻量版）：只做「分」与「关」，不做拖拽换位 / 布局持久化
    // ------------------------------------------------------------------

    /// 当前活动标签对应的终端标签页（仅终端返回）。
    pub fn active_terminal_tab_mut(&mut self) -> Option<&mut TerminalTab> {
        let idx = self.active.min(self.tabs.len().saturating_sub(1));
        self.tabs.get_mut(idx).and_then(|t| t.terminal_tab_mut())
    }

    /// 在活动窗格旁边分屏。
    ///
    /// - `axis`：横向 = 左右并排，纵向 = 上下叠放；`before` = 新窗格放左/上。
    /// - 新窗格**复制当前窗格的启动配置**（`SpawnCfg`），所以「分屏」得到的是
    ///   同一个 SSH 主机 / 同一条命令的另一个会话，而不是一个空白本地终端。
    /// - SSH 要建链，不能卡住 UI：走后台线程 + `PaneState::Connecting` 占位，
    ///   连上后由 `poll_connecting` 换入。
    pub fn split_active_pane(&mut self, axis: Axis, before: bool, cx: &mut Context<Self>) {
        let idx = self.active.min(self.tabs.len().saturating_sub(1));

        // 先只读地把「分谁、按什么配置分」取出来，避免后面 &mut self 冲突
        let Some((target, spawn, cols, rows, title)) = self
            .tabs
            .get(idx)
            .and_then(|t| t.terminal_tab())
            .and_then(|tab| {
                let pane = tab.active_pane()?;
                Some((
                    tab.active_id(),
                    pane.spawn.clone(),
                    pane.dims.cols,
                    pane.dims.rows,
                    pane.title.clone(),
                ))
            })
        else {
            self.set_toast("分屏仅对已连接的终端窗格有效");
            return;
        };

        let runtime = self.runtime.clone();
        let made: Result<PaneState, String> = match spawn {
            SpawnCfg::Local => TermPane::new_local(title, cols, rows)
                .map(PaneState::Live)
                .map_err(|e| format!("分屏失败: {e:#}")),
            SpawnCfg::Command { program, args } => {
                TermPane::new_command(title, &program, &args, cols, rows)
                    .map(PaneState::Live)
                    .map_err(|e| format!("分屏失败: {e:#}"))
            }
            SpawnCfg::Tcp { host, port } => TermPane::new_tcp(title, host, port, cols, rows)
                .map(PaneState::Live)
                .map_err(|e| format!("分屏失败: {e:#}")),
            SpawnCfg::Ssh {
                host,
                port,
                user,
                auth,
                jump,
            } => {
                let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<TermPane>>();
                let tab_title = title.clone();
                std::thread::spawn(move || {
                    let result = runtime.block_on(TermPane::new_ssh(
                        tab_title, host, port, user, auth, jump, cols, rows,
                    ));
                    let _ = tx.send(result);
                });
                Ok(PaneState::Connecting { title, rx })
            }
        };

        match made {
            Ok(state) => {
                let mut split = false;
                {
                    if let Some(tab) = self.tabs.get_mut(idx).and_then(|t| t.terminal_tab_mut()) {
                        split = tab.split_with(target, axis, state, before).is_some();
                    }
                }
                if split {
                    self.focus_term = true;
                    cx.notify();
                } else {
                    self.set_toast("分屏失败：目标窗格已不存在");
                }
            }
            Err(e) => self.set_toast(e),
        }
    }

    /// 关闭当前窗格。只剩一个窗格时不做任何事（提示用 Ctrl+W 关标签）。
    pub fn close_active_pane(&mut self, cx: &mut Context<Self>) {
        let idx = self.active.min(self.tabs.len().saturating_sub(1));
        let Some((target, panes)) = self
            .tabs
            .get(idx)
            .and_then(|t| t.terminal_tab())
            .map(|tab| (tab.active_id(), tab.layout.len()))
        else {
            return;
        };
        if panes <= 1 {
            self.set_toast("只有一个窗格：用 Ctrl+W 关闭整个标签");
            return;
        }

        // 关窗格等同于这段会话结束：先跑「断开时」宏（字节已进发送队列，随后销毁无影响）
        let mut bytes: Vec<Vec<u8>> = Vec::new();
        for i in self.macros.get_on_disconnect_macros() {
            if let Some(b) = self.macros.send_macro_bytes(i) {
                bytes.push(b);
            }
        }
        if !bytes.is_empty() {
            if let Some(tab) = self.tabs.get_mut(idx).and_then(|t| t.terminal_tab_mut()) {
                if let Some(pane) = tab.pane_mut(target) {
                    for b in bytes {
                        pane.write_input(b);
                    }
                }
            }
        }

        let mut removed = false;
        {
            if let Some(tab) = self.tabs.get_mut(idx).and_then(|t| t.terminal_tab_mut()) {
                removed = tab.remove(target);
            }
        }
        if removed {
            self.focus_term = true;
            cx.notify();
        }
    }

    /// 把焦点切到下一个窗格（只有一个窗格时无操作）。
    pub fn focus_next_pane(&mut self, cx: &mut Context<Self>) {
        let idx = self.active.min(self.tabs.len().saturating_sub(1));
        let mut moved = false;
        {
            if let Some(tab) = self.tabs.get_mut(idx).and_then(|t| t.terminal_tab_mut()) {
                let ids = tab.leaf_ids();
                if ids.len() > 1 {
                    let cur = ids.iter().position(|id| *id == tab.active).unwrap_or(0);
                    tab.active = ids[(cur + 1) % ids.len()];
                    moved = true;
                }
            }
        }
        if moved {
            self.focus_term = true;
            cx.notify();
        }
    }

    /// 活动标签是否可分屏（菜单项置灰用）。
    pub fn can_split(&self) -> bool {
        self.tabs
            .get(self.active.min(self.tabs.len().saturating_sub(1)))
            .and_then(|t| t.terminal_tab())
            .map(|tab| tab.active_pane().is_some())
            .unwrap_or(false)
    }

    /// 活动标签是否有多个窗格（「关闭当前窗格」菜单项置灰用）。
    pub fn has_multiple_panes(&self) -> bool {
        self.tabs
            .get(self.active.min(self.tabs.len().saturating_sub(1)))
            .and_then(|t| t.terminal_tab())
            .map(|tab| tab.layout.len() > 1)
            .unwrap_or(false)
    }

    pub fn send_batch_command(&mut self, cmd: String) {
        let cmd = cmd.trim().to_string();
        if cmd.is_empty() {
            return;
        }
        let mut n = 0usize;
        for tab in self.tabs.iter_mut() {
            let Some(t) = tab.terminal_tab_mut() else {
                continue;
            };
            // 批量执行覆盖**每一个**终端窗格（分屏也算各自的会话）
            for slot in t.panes.iter_mut() {
                let PaneState::Live(pane) = &mut slot.state else {
                    continue;
                };
                if pane.dead {
                    continue;
                }
                pane.write_input_local(format!("{cmd}\r").into_bytes());
                n += 1;
            }
        }
        self.batch_open = false;
        self.set_toast(format!("已把命令发送到 {n} 个终端"));
    }

    pub fn send_macro_to_active(&mut self, idx: usize) {
        if let Some(bytes) = self.macros.send_macro_bytes(idx) {
            if let Some(pane) = self.tabs.get_mut(self.active).and_then(|t| t.terminal_mut()) {
                if !pane.dead {
                    pane.write_input(bytes);
                    self.set_toast("宏已发送到当前终端");
                }
            }
        }
    }

    pub fn close_tab(&mut self, index: usize) {
        if index < self.tabs.len() {
            // 关闭前先执行「断开时」宏（如 exit / 清理命令）——
            // 字节已经进了 SSH 通道的发送队列，随后标签被销毁不影响投递。
            if matches!(self.tabs[index], TabContent::Terminal(_)) {
                self.run_trigger_macros(crate::macros::TriggerKind::OnDisconnect, index);
            }
            self.tabs[index].shutdown();
            self.tabs.remove(index);
            if !self.tabs.is_empty() {
                self.active = if index <= self.active {
                    self.active.saturating_sub(1).min(self.tabs.len() - 1)
                } else {
                    self.active.min(self.tabs.len() - 1)
                };
            } else {
                self.active = 0;
            }
        }
    }

    pub fn move_tab(&mut self, from: usize, ins: usize) {
        if from >= self.tabs.len() || ins >= self.tabs.len() {
            return;
        }
        let mut a = self.active;
        if a == from {
            a = ins;
        } else {
            if a > from {
                a -= 1;
            }
            if a >= ins {
                a += 1;
            }
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(ins, tab);
        self.active = a;
    }

    pub fn poll_connecting(&mut self) {
        // 分屏窗格：SSH 连接结果同样经通道回传。连上后补执行「连接时」宏，
        // 并把键盘焦点交给新窗格（用户刚点的分屏，期待立刻能打字）。
        for i in 0..self.tabs.len() {
            let changed = self
                .tabs
                .get_mut(i)
                .and_then(|t| t.terminal_tab_mut())
                .map(|tab| tab.poll())
                .unwrap_or(false);
            if changed {
                self.run_trigger_macros(crate::macros::TriggerKind::OnConnect, i);
                self.focus_term = true;
            }
        }

        for i in 0..self.tabs.len() {
            let mut done: Option<std::result::Result<TermPane, String>> = None;
            if let TabContent::Connecting { rx, .. } = &self.tabs[i] {
                match rx.try_recv() {
                    Ok(Ok(pane)) => done = Some(Ok(pane)),
                    Ok(Err(e)) => done = Some(Err(format!("{e:#}"))),
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        done = Some(Err("连接线程异常退出".into()))
                    }
                }
            }
            if let Some(result) = done {
                match result {
                    Ok(pane) => {
                        self.tabs[i] = TabContent::Terminal(TerminalTab::new(pane));
                        // SSH 连上后执行「连接时」宏
                        self.run_trigger_macros(crate::macros::TriggerKind::OnConnect, i);
                    }
                    Err(msg) => self.tabs[i] = TabContent::Failed(msg),
                }
            }
        }
    }

    /// 执行指定触发方式的宏，发送到第 `tab` 个标签的终端。
    fn run_trigger_macros(&mut self, kind: crate::macros::TriggerKind, tab: usize) {
        let idxs = match kind {
            crate::macros::TriggerKind::OnConnect => self.macros.get_on_connect_macros(),
            crate::macros::TriggerKind::OnDisconnect => self.macros.get_on_disconnect_macros(),
            _ => Vec::new(),
        };
        if idxs.is_empty() {
            return;
        }
        let mut sent = 0;
        for i in idxs {
            let Some(bytes) = self.macros.send_macro_bytes(i) else {
                continue;
            };
            if let Some(pane) = self.tabs.get_mut(tab).and_then(|t| t.terminal_mut()) {
                pane.write_input(bytes);
                sent += 1;
            }
        }
        if sent > 0 {
            let what = match kind {
                crate::macros::TriggerKind::OnConnect => "连接",
                _ => "断开",
            };
            self.set_toast(format!("已执行 {sent} 个「{what}时」宏"));
        }
    }

    /// 切换语言。
    pub fn toggle_language(&mut self) {
        self.i18n.toggle();
        self.config.language = self.i18n.id().to_string();
        if let Err(e) = self.config.save() {
            self.set_toast(e);
        }
        self.set_toast(match self.i18n.lang {
            crate::i18n::Lang::Zh => "界面语言：中文",
            crate::i18n::Lang::En => "UI language: English",
        });
    }

    /// 当前活动终端的概要（AI 面板展示用）。
    pub fn active_terminal_hint(&self) -> Option<String> {
        let tab = self.tabs.get(self.active)?;
        let pane = tab.terminal()?;
        Some(match &pane.spawn {
            SpawnCfg::Ssh { host, port, user, .. } => format!("SSH {user}@{host}:{port}"),
            SpawnCfg::Tcp { host, port } => format!("Telnet {host}:{port}"),
            SpawnCfg::Command { program, .. } => format!("命令 {program}"),
            SpawnCfg::Local => "本地终端".to_string(),
        })
    }

    /// 取当前活动终端的最近输出，作为 AI 对话的上下文。
    pub fn current_terminal_context(&self) -> Option<String> {
        let tab = self.tabs.get(self.active)?;
        let pane = tab.terminal()?;
        let text =
            pane.context_text(self.config.ai.context_lines, self.config.ai.context_max_line_len);
        if text.trim().is_empty() {
            return None;
        }
        let header = match &pane.spawn {
            SpawnCfg::Ssh { host, port, user, .. } => {
                format!("【终端信息】SSH 连接 {}@{}:{}（远端）\n", user, host, port)
            }
            SpawnCfg::Tcp { host, port } => {
                format!("【终端信息】Telnet 连接 {}:{}（远端）\n", host, port)
            }
            SpawnCfg::Command { program, .. } => {
                format!("【终端信息】本地运行命令 {program} 的输出\n")
            }
            SpawnCfg::Local => "【终端信息】本地终端\n".to_string(),
        };
        Some(format!("{header}{}", text))
    }

    /// 是否有浮层正在显示（WebView 需要整体隐藏）。
    pub fn overlay_open(&self) -> bool {
        self.config_panel.open
            || self.ai_config_panel.open
            || self.ssh_form_open
            || self.add_form.is_some()
            || self.about_open
            || self.telnet_open
            || self.ping_open
            || self.batch_open
            || self.tunnel_open
    }

    /// 只有活动标签页的 WebView 可见。
    pub fn sync_browser_visibility(&mut self) {
        let active = self.active;
        let allow_visible = !self.overlay_open();
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if let TabContent::Browser(b) = tab {
                b.set_visible(allow_visible && i == active);
            }
        }
        if self.last_active != active {
            if !matches!(self.tabs.get(active), Some(TabContent::Browser(_))) {
                crate::win32::focus_main_window();
            }
            self.last_active = active;
        }
    }

    // ------------------------------------------------------------------
    // 全局 action
    // ------------------------------------------------------------------

    fn bind_keys(cx: &mut App) {
        use gpui_kit::{KeyBinding, Keystroke};
        let _ = Keystroke::parse("ctrl-n");
        cx.bind_keys([
            KeyBinding::new("ctrl-tab", NextTab, None),
            KeyBinding::new("ctrl-shift-tab", PrevTab, None),
            KeyBinding::new("ctrl-n", NewLocalTab, None),
            KeyBinding::new("ctrl-t", NewBrowserTab, None),
            KeyBinding::new("ctrl-shift-s", ToggleSftp, None),
            KeyBinding::new("ctrl-shift-e", ToggleLocalFiles, None),
            KeyBinding::new("ctrl-shift-r", ToggleRdp, None),
            KeyBinding::new("ctrl-shift-b", ToggleBrowser, None),
            KeyBinding::new("ctrl-shift-a", ToggleAi, None),
            KeyBinding::new("ctrl-w", CloseTab, None),
            KeyBinding::new("f11", ToggleFullscreen, None),
            KeyBinding::new("ctrl-l", FocusAddress, None),
            KeyBinding::new("ctrl-f", FindInTerminal, None),
            KeyBinding::new("ctrl-1", JumpTab1, None),
            KeyBinding::new("ctrl-2", JumpTab2, None),
            KeyBinding::new("ctrl-3", JumpTab3, None),
            KeyBinding::new("ctrl-4", JumpTab4, None),
            KeyBinding::new("ctrl-5", JumpTab5, None),
            KeyBinding::new("ctrl-6", JumpTab6, None),
            KeyBinding::new("ctrl-7", JumpTab7, None),
            KeyBinding::new("ctrl-8", JumpTab8, None),
            KeyBinding::new("ctrl-9", JumpTab9, None),
            KeyBinding::new("ctrl-shift-left", MoveTabLeft, None),
            KeyBinding::new("ctrl-shift-right", MoveTabRight, None),
            // 分屏：分的是「当前活动窗格」，不是新开标签
            KeyBinding::new("ctrl-shift-d", SplitRight, None),
            KeyBinding::new("ctrl-shift-u", SplitDown, None),
            KeyBinding::new("ctrl-shift-w", ClosePane, None),
            KeyBinding::new("ctrl-shift-p", NextPane, None),
        ]);
    }

    /// 输入框实体惰性创建辅助。
    fn mk_input(placeholder: &str, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        cx.new(|cx| {
            gpui_kit::base::input::InputState::new(window, cx).placeholder(placeholder.to_string())
        })
    }

    fn mk_password(
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        cx.new(|cx| {
            gpui_kit::base::input::InputState::new(window, cx)
                .placeholder(placeholder.to_string())
                .masked(true)
        })
    }

    // ==================================================================
    // UI 构建（gpui 版）
    // ==================================================================

    /// 切换亮/暗主题。
    pub fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mode = if self.config.theme == "light" {
            self.config.theme = "dark".to_string();
            ThemeMode::Dark
        } else {
            self.config.theme = "light".to_string();
            ThemeMode::Light
        };
        Theme::change(mode, Some(window), cx);
        if let Err(e) = self.config.save() {
            self.set_toast(e);
        }
        cx.notify();
    }

    /// 切换到指定主题。
    ///
    /// 与 [`Self::toggle_theme`] 的区别是不需要 `&mut Window`——菜单栏的点击
    /// 回调只拿得到 `&mut App`，而 `Theme::change` 本身也不读窗口。
    pub fn set_theme(&mut self, mode: ThemeMode, cx: &mut Context<Self>) {
        self.config.theme = mode.name().to_string();
        Theme::change(mode, None, cx);
        if let Err(e) = self.config.save() {
            self.set_toast(e);
        }
        cx.notify();
    }

    /// 向输入框实体写入文本。
    fn set_input(state: &Entity<InputState>, text: &str, window: &mut Window, cx: &mut App) {
        state.update(cx, |s, cx| s.set_value(text.to_string(), window, cx));
    }

    /// 顶部菜单栏（对应旧 egui 版的 6 个下拉菜单：终端 / 会话 / 视图 /
    /// 工具 / 设置 / 帮助）。
    ///
    /// 实现要点：
    /// - 菜单项的点击**不**走 GPUI 的 action 派发路径（弹出菜单渲染在
    ///   popover 层里，不是根容器的 DOM 后代，冒泡到不了根节点的
    ///   `on_action`），而是直接拿到 `WeakEntity<RustTermApp>` 改状态。
    /// - `action` 字段只用于在菜单右侧渲染快捷键提示，点击仍以闭包为准。
    fn menu_bar_ui(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let lang = self.i18n.lang;
        let theme = Theme::global(cx).clone();
        let app = cx.weak_entity();

        // ---- 终端 ----
        let terminal = {
            let app = app.clone();
            move |menu: PopupMenu, window: &mut Window, cx: &mut Context<PopupMenu>| {
                let mut m = menu;
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.local_connect"),
                    Some(Box::new(NewLocalTab)),
                    false,
                    |this, _cx| this.open_local(),
                ));
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.ssh_connect"),
                    None,
                    false,
                    |this, _cx| {
                        this.ssh_form_open = true;
                        this.ssh_form_error = None;
                    },
                ));
                m = m.separator();
                m = m.item(menu_item(
                    &app,
                    tr(lang, "tb.sync"),
                    None,
                    false,
                    |this, _cx| this.toggle_sync_input(),
                ));
                m = m.item(menu_item(
                    &app,
                    tr(lang, "tb.batch"),
                    None,
                    false,
                    |this, _cx| {
                        this.batch_open = true;
                        this.focus_dialog = true;
                    },
                ));
                m = m.item(menu_item(
                    &app,
                    tr(lang, "tb.tunnel"),
                    None,
                    false,
                    |this, _cx| this.tunnel_open = true,
                ));
                m = m.separator();
                m = m.item(menu_item(
                    &app,
                    "查找（Ctrl+F）",
                    Some(Box::new(FindInTerminal)),
                    false,
                    |this, _cx| this.toggle_find(),
                ));
                m = m.item(menu_item(
                    &app,
                    "复制选区（Ctrl+Shift+C）",
                    None,
                    false,
                    |this, cx| {
                        let idx = this.active.min(this.tabs.len().saturating_sub(1));
                        if let Some(pane) = this.tabs.get_mut(idx).and_then(|t| t.terminal_mut()) {
                            if pane.copy_selection(cx) {
                                this.set_toast("已复制到剪贴板");
                            } else {
                                this.set_toast("没有选中内容");
                            }
                        }
                    },
                ));
                m = m.item(menu_item(&app, "粘贴（Ctrl+V）", None, false, |this, cx| {
                    let idx = this.active.min(this.tabs.len().saturating_sub(1));
                    if let Some(pane) = this.tabs.get_mut(idx).and_then(|t| t.terminal_mut()) {
                        if pane.paste_clipboard(cx) {
                            this.set_toast("已粘贴");
                        } else {
                            this.set_toast("剪贴板没有文本");
                        }
                    }
                }));
                m = m.item(menu_item(&app, "全选", None, false, |this, _cx| {
                    let idx = this.active.min(this.tabs.len().saturating_sub(1));
                    if let Some(pane) = this.tabs.get_mut(idx).and_then(|t| t.terminal_mut()) {
                        pane.select_all();
                    }
                }));
                m = m.separator();
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.split_right"),
                    Some(Box::new(SplitRight)),
                    false,
                    |this, cx| this.split_active_pane(Axis::Horizontal, false, cx),
                ));
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.split_down"),
                    Some(Box::new(SplitDown)),
                    false,
                    |this, cx| this.split_active_pane(Axis::Vertical, false, cx),
                ));
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.next_pane"),
                    Some(Box::new(NextPane)),
                    false,
                    |this, cx| this.focus_next_pane(cx),
                ));
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.close_pane"),
                    Some(Box::new(ClosePane)),
                    false,
                    |this, cx| this.close_active_pane(cx),
                ));
                m = m.separator();
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.close_tab"),
                    Some(Box::new(CloseTab)),
                    false,
                    |this, _cx| {
                        if !this.tabs.is_empty() {
                            this.close_tab(this.active);
                        }
                    },
                ));
                let _ = (window, cx);
                m
            }
        };

        // ---- 会话 ----
        let session = {
            let app = app.clone();
            move |menu: PopupMenu, window: &mut Window, cx: &mut Context<PopupMenu>| {
                let mut m = menu;
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.new_session"),
                    None,
                    false,
                    |this, _cx| this.pending_new_session = true,
                ));
                m = m.separator();
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.session_list"),
                    None,
                    false,
                    |this, _cx| this.sidebar_tab = SidebarTab::Sessions,
                ));
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.refresh_sessions"),
                    None,
                    false,
                    |this, _cx| this.sessions = SessionStore::load(),
                ));
                m = m.separator();
                m = m.item(menu_item(
                    &app,
                    "工具箱",
                    None,
                    false,
                    |this, _cx| this.sidebar_tab = SidebarTab::Tools,
                ));
                m = m.item(menu_item(
                    &app,
                    "键宏",
                    None,
                    false,
                    |this, _cx| this.sidebar_tab = SidebarTab::Macros,
                ));
                let _ = (window, cx);
                m
            }
        };

        // ---- 视图 ----
        let view = {
            let app = app.clone();
            move |menu: PopupMenu, window: &mut Window, cx: &mut Context<PopupMenu>| {
                let mut m = menu;
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.browser"),
                    Some(Box::new(ToggleBrowser)),
                    false,
                    |this, _cx| this.open_browser("about:blank"),
                ));
                // 远程桌面子菜单：RDP / VNC / SPICE
                let sub_app = app.clone();
                m = m.submenu(
                    tr(lang, "item.remote_desktop"),
                    window,
                    cx,
                    move |menu: PopupMenu, window: &mut Window, cx: &mut Context<PopupMenu>| {
                        let mut m = menu;
                        m = m.item(menu_item(
                            &sub_app,
                            "RDP",
                            Some(Box::new(ToggleRdp)),
                            false,
                            |this, _cx| this.open_remote_desktop(RdTabProtocol::Rdp),
                        ));
                        m = m.item(menu_item(
                            &sub_app,
                            "VNC",
                            None,
                            false,
                            |this, _cx| this.open_remote_desktop(RdTabProtocol::Vnc),
                        ));
                        m = m.item(menu_item(
                            &sub_app,
                            "SPICE",
                            None,
                            false,
                            |this, _cx| this.open_remote_desktop(RdTabProtocol::Spice),
                        ));
                        let _ = (window, cx);
                        m
                    },
                );
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.local_files"),
                    Some(Box::new(ToggleLocalFiles)),
                    false,
                    |this, _cx| this.open_local_files(),
                ));
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.sftp_panel"),
                    Some(Box::new(ToggleSftp)),
                    false,
                    |this, _cx| this.open_sftp(),
                ));
                m = m.separator();
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.ai_assistant"),
                    Some(Box::new(ToggleAi)),
                    false,
                    |this, _cx| this.ai_assistant.open = !this.ai_assistant.open,
                ));
                m = m.separator();
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.status_bar"),
                    None,
                    false,
                    |this, _cx| {
                        this.config.show_status_bar = !this.config.show_status_bar;
                        if let Err(e) = this.config.save() {
                            this.set_toast(e);
                        }
                    },
                ));
                m
            }
        };

        // ---- 工具 ----
        let tools = {
            let app = app.clone();
            move |menu: PopupMenu, window: &mut Window, cx: &mut Context<PopupMenu>| {
                let mut m = menu;
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.ping"),
                    None,
                    false,
                    |this, _cx| this.run_tool_action(ToolAction::Ping),
                ));
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.telnet"),
                    None,
                    false,
                    |this, _cx| this.run_tool_action(ToolAction::TelnetClient),
                ));
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.portscan"),
                    None,
                    false,
                    |this, _cx| this.run_tool_action(ToolAction::PortScan),
                ));
                m = m.separator();
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.macro_manager"),
                    None,
                    false,
                    |this, _cx| this.run_tool_action(ToolAction::Macro),
                ));
                let _ = (window, cx);
                m
            }
        };

        // ---- 设置 ----
        let settings = {
            let app = app.clone();
            let is_dark = self.config.theme != "light";
            move |menu: PopupMenu, window: &mut Window, cx: &mut Context<PopupMenu>| {
                let mut m = menu;
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.config_panel"),
                    None,
                    false,
                    |this, _cx| this.config_panel.open = true,
                ));
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.ai_config"),
                    None,
                    false,
                    |this, _cx| this.ai_config_panel.open = true,
                ));
                m = m.separator();
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.dark_theme"),
                    None,
                    is_dark,
                    |this, cx| this.set_theme(ThemeMode::Dark, cx),
                ));
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.light_theme"),
                    None,
                    !is_dark,
                    |this, cx| this.set_theme(ThemeMode::Light, cx),
                ));
                m = m.separator();
                m = m.item(menu_item(
                    &app,
                    tr(lang, "common.language_switch"),
                    None,
                    false,
                    |this, _cx| this.toggle_language(),
                ));
                let _ = (window, cx);
                m
            }
        };

        // ---- 帮助 ----
        let help = {
            let app = app.clone();
            move |menu: PopupMenu, window: &mut Window, cx: &mut Context<PopupMenu>| {
                let mut m = menu;
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.about"),
                    None,
                    false,
                    |this, _cx| this.about_open = true,
                ));
                let _ = (window, cx);
                m
            }
        };

        // 六个菜单的构建函数类型各不相同，统一擦除成 trait object 后再排布。
        type MenuBuilder = Box<
            dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
        >;
        let buttons: [(&'static str, String, MenuBuilder); 6] = [
            ("menu-terminal", tr(lang, "menu.terminal"), Box::new(terminal)),
            ("menu-session", tr(lang, "menu.session"), Box::new(session)),
            ("menu-view", tr(lang, "menu.view"), Box::new(view)),
            ("menu-tools", tr(lang, "menu.tools"), Box::new(tools)),
            ("menu-settings", tr(lang, "menu.settings"), Box::new(settings)),
            ("menu-help", tr(lang, "menu.help"), Box::new(help)),
        ];

        let mut row = div().h_flex().items_center().gap_0().px_1().min_w_0();
        for (id, label, build) in buttons {
            row = row.child(
                Button::new(id)
                    .label(label)
                    .compact()
                    .ghost()
                    .dropdown_menu(build),
            );
        }

        div()
            .h_flex()
            .items_center()
            .flex_none()
            .h(px(MENUBAR_H))
            .bg(theme.colors.title_bar)
            .border_b_1()
            .border_color(theme.colors.title_bar_border)
            .overflow_hidden()
            .child(row)
            .into_any_element()
    }

    /// 工具栏：竖排「图标 + 文字」按钮，按语义分组。
    ///
    /// 旧实现是一排 15 个同权重的文字按钮，占满整行且无法扫读；这里改成
    /// 小尺寸图标按钮并分组（连接 / 批量 / 工具 / 资源），分组之间用细分
    /// 隔线区分，右侧留白后放 AI / 设置 / 语言 / 主题 / 帮助。
    fn toolbar_ui(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let lang = self.i18n.lang;
        let theme = Theme::global(cx).clone();
        let app = cx.weak_entity();

        /// 竖排图标按钮：图标在上、文字在下，hover / 选中带底色。
        fn tool_button(
            app: &WeakEntity<RustTermApp>,
            theme: &Theme,
            id: impl Into<ElementId>,
            icon: IconName,
            label: String,
            active: bool,
            op: impl Fn(&mut RustTermApp, &mut Window, &mut Context<RustTermApp>) + 'static,
        ) -> AnyElement {
            let app = app.clone();
            let fg = if active {
                theme.colors.foreground
            } else {
                theme.colors.muted_foreground
            };
            div()
                .id(id)
                .v_flex()
                .items_center()
                .justify_center()
                .gap_1()
                .px_2()
                .py_1()
                .rounded(px(6.0))
                .cursor_pointer()
                .when(active, |d| d.bg(theme.colors.accent))
                .hover(|d| d.bg(theme.colors.accent))
                .on_click(move |_evt, win, cx| {
                    let _ = app.update(cx, |this, cx| {
                        op(this, win, cx);
                        cx.notify();
                    });
                })
                .child(ic(icon).small().text_color(fg))
                .child(div().text_size(px(10.0)).text_color(fg).child(label))
                .into_any_element()
        }

        // 分组之间的竖向细分隔线。
        let separator = || {
            div()
                .w(px(1.0))
                .h(px(22.0))
                .mx_2()
                .bg(theme.colors.border)
                .into_any_element()
        };

        // (图标, 文案, 动作)
        let groups: Vec<Vec<(IconName, String, ToolAction)>> = vec![
            vec![
                (
                    IconName::SquareTerminal,
                    tr(lang, "tb.local"),
                    ToolAction::LocalTerminal,
                ),
                (IconName::Terminal, tr(lang, "tb.ssh"), ToolAction::SshClient),
                (IconName::Plug, "Telnet".into(), ToolAction::TelnetClient),
            ],
            vec![
                (IconName::RefreshCw, tr(lang, "tb.sync"), ToolAction::Sync),
                (IconName::ListTree, tr(lang, "tb.batch"), ToolAction::Batch),
                (IconName::Cable, tr(lang, "tb.tunnel"), ToolAction::Tunnel),
            ],
            vec![
                (IconName::Activity, "Ping".into(), ToolAction::Ping),
                (IconName::Search, tr(lang, "tb.find"), ToolAction::Find),
            ],
            vec![
                (IconName::Folder, tr(lang, "tb.files"), ToolAction::LocalFiles),
                (IconName::Globe, tr(lang, "tb.browser"), ToolAction::Browser),
            ],
        ];

        let mut row = div().h_flex().items_center().gap_0().px_1().min_w_0();
        for (gi, group) in groups.iter().enumerate() {
            if gi > 0 {
                row = row.child(separator());
            }
            for (i, (icon, label, action)) in group.iter().enumerate() {
                let action = *action;
                row = row.child(tool_button(
                    &app,
                    &theme,
                    format!("tb-g{gi}-{i}"),
                    *icon,
                    label.clone(),
                    false,
                    move |this, _win, _cx| this.run_tool_action(action),
                ));
            }
        }

        // ---- 右侧：面板 / 设置类入口 ----
        let ai_open = self.ai_assistant.open;
        let is_dark = self.config.theme != "light";
        let right = div().h_flex().items_center().gap_0().ml_auto().px_1();
        let right = right
            .child(tool_button(
                &app,
                &theme,
                "tb-ai",
                IconName::Bot,
                tr(lang, "tb.ai"),
                ai_open,
                |this, _win, _cx| this.ai_assistant.open = !this.ai_assistant.open,
            ))
            .child(tool_button(
                &app,
                &theme,
                "tb-settings",
                IconName::Settings,
                tr(lang, "tb.settings"),
                false,
                |this, _win, _cx| this.config_panel.open = true,
            ))
            .child(tool_button(
                &app,
                &theme,
                "tb-language",
                IconName::Languages,
                tr(lang, "tb.language"),
                false,
                |this, _win, _cx| this.toggle_language(),
            ))
            .child(tool_button(
                &app,
                &theme,
                "tb-theme",
                if is_dark { IconName::Moon } else { IconName::Sun },
                tr(lang, "tb.theme"),
                false,
                |this, win, cx| this.toggle_theme(win, cx),
            ))
            .child(tool_button(
                &app,
                &theme,
                "tb-help",
                IconName::Info,
                tr(lang, "tb.help"),
                false,
                |this, _win, _cx| this.about_open = true,
            ));

        div()
            .h_flex()
            .items_center()
            .flex_none()
            .h(px(TOOLBAR_H))
            .bg(theme.colors.background)
            .border_b_1()
            .border_color(theme.colors.border)
            .overflow_hidden()
            .child(row)
            .child(right)
            .into_any_element()
    }

    fn quick_connect_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let lang = self.i18n.lang;
        if self.quick_input.is_none() {
            let input = Self::mk_input(&tr(lang, "qc.hint"), window, cx);
            cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let addr = Self::input_value(&this.quick_input, cx);
                    let jump = Self::input_value(&this.jump_input, cx);
                    let secret = Self::input_value(&this.quick_secret, cx);
                    this.quick_connect_submit(addr, jump, secret);
                }
            })
            .detach();
            self.quick_input = Some(input);
        }
        if self.jump_input.is_none() {
            self.jump_input = Some(Self::mk_input("跳板机 user@host:port", window, cx));
        }
        let is_key = self.quick_auth.eq_ignore_ascii_case("key");
        if self.quick_secret.is_none() {
            self.quick_secret = Some(if is_key {
                Self::mk_input("私钥路径 ~/.ssh/id_rsa", window, cx)
            } else {
                Self::mk_password("密码（可空，连接后提示）", window, cx)
            });
        }
        let addr = self.quick_input.clone().unwrap();
        let jump = self.jump_input.clone().unwrap();
        let secret = self.quick_secret.clone().unwrap();

        div()
            .h_flex()
            .items_center()
            .flex_none()
            .h(px(QUICK_H))
            .gap_2()
            .px_2()
            .overflow_hidden()
            .border_b_1()
            .border_color(Theme::global(cx).colors.border)
            .child(Input::new(&addr).flex_1().min_w_0())
            .child(Input::new(&jump).w(px(160.0)))
            .child(
                Button::new("qc-auth")
                    .label(if is_key {
                        tr(lang, "qc.key")
                    } else {
                        tr(lang, "qc.password")
                    })
                    .compact()
                    .on_click(cx.listener(|this, _, _, _| {
                        this.quick_auth = if this.quick_auth.eq_ignore_ascii_case("key") {
                            "password".into()
                        } else {
                            "key".into()
                        };
                        // 认证方式切换后重建凭据输入框（密码框 ↔ 文本框）
                        this.quick_secret = None;
                    })),
            )
            // 凭据框：密码模式带「查看密码」眼睛按钮，私钥路径是明文路径不需要
            .child(
                if is_key {
                    Input::new(&secret)
                } else {
                    password_field(&secret)
                }
                .w(px(200.0)),
            )
            .child(
                Button::new("qc-connect")
                    .label(tr(lang, "qc.connect"))
                    .compact()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let addr = Self::input_value(&this.quick_input, cx);
                        let jump = Self::input_value(&this.jump_input, cx);
                        let secret = Self::input_value(&this.quick_secret, cx);
                        this.quick_connect_submit(addr, jump, secret);
                    })),
            )
            .into_any_element()
    }

    fn sidebar_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let lang = self.i18n.lang;
        let theme = Theme::global(cx).clone();
        let border = theme.colors.border;
        let mut panel = div()
            .v_flex()
            .w(px(SIDEBAR_W))
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(border)
            .overflow_hidden();

        // 页签改成分段控件：外层圆角容器 + 选中项白底，避免三个纯文字按钮
        // 与下方内容糊在一起。
        let mut tabs_row = div()
            .h_flex()
            .gap_0p5()
            .p_1()
            .m_1()
            .rounded(px(6.0))
            .bg(theme.colors.muted);
        for (tab, label, icon) in [
            (SidebarTab::Sessions, tr(lang, "side.sessions"), IconName::Server),
            (SidebarTab::Tools, tr(lang, "side.tools"), IconName::Wrench),
            (SidebarTab::Macros, tr(lang, "side.macros"), IconName::Keyboard),
        ] {
            let selected = self.sidebar_tab == tab;
            let fg = if selected {
                theme.colors.foreground
            } else {
                theme.colors.muted_foreground
            };
            tabs_row = tabs_row.child(
                div()
                    .id(SharedString::from(format!(
                        "side-{}",
                        label.replace(' ', "_")
                    )))
                    .h_flex()
                    .items_center()
                    .justify_center()
                    .gap_1()
                    .flex_1()
                    .h(px(24.0))
                    .rounded(px(4.0))
                    .text_size(px(11.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(if selected {
                        theme.colors.border
                    } else {
                        transparent_black()
                    })
                    .when(selected, |d| d.bg(theme.colors.background))
                    .hover(|d| d.bg(theme.colors.background))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.sidebar_tab = tab;
                        cx.notify();
                    }))
                    .child(ic(icon).xsmall().text_color(fg))
                    .child(div().text_color(fg).child(label)),
            );
        }
        panel = panel.child(tabs_row);

        match self.sidebar_tab {
            SidebarTab::Sessions => panel.child(self.sidebar_sessions_ui(window, cx)),
            SidebarTab::Tools => panel.child(self.sidebar_tools_ui(cx)),
            SidebarTab::Macros => panel.child(self.sidebar_macros_ui(cx)),
        }
        .into_any_element()
    }

    /// 会话标签页：搜索框 + 会话列表（单击选中，双击连接，行内编辑/删除）。
    fn sidebar_sessions_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let lang = self.i18n.lang;
        let theme = Theme::global(cx).clone();
        let border = theme.colors.border;
        let dim = theme.colors.muted_foreground;

        if self.search_input.is_none() {
            let input = Self::mk_input(&tr(lang, "side.search"), window, cx);
            cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.search = Self::input_value(&this.search_input, cx);
                    cx.notify();
                }
            })
            .detach();
            self.search_input = Some(input);
        }
        let search = self.search_input.clone().unwrap();

        struct Row {
            id: String,
            text: String,
            icon: IconName,
            selected: bool,
        }
        let mut ids: Vec<String> = self
            .sessions
            .sessions
            .iter()
            .filter(|s| s.matches_search(&self.search))
            .map(|s| s.id.clone())
            .collect();
        ids.sort_by(|a, b| {
            let da = self.sessions.by_id(a).map(|s| s.display()).unwrap_or_default();
            let db = self.sessions.by_id(b).map(|s| s.display()).unwrap_or_default();
            da.cmp(&db)
        });
        let rows: Vec<Row> = ids
            .iter()
            .filter_map(|id| {
                let s = self.sessions.by_id(id)?;
                Some(Row {
                    id: id.clone(),
                    text: s.display(),
                    icon: match s.kind {
                        SessionKind::Ssh => IconName::Terminal,
                        _ => IconName::Monitor,
                    },
                    selected: self.selected_session.as_deref() == Some(id.as_str()),
                })
            })
            .collect();

        let mut list = div().v_flex().flex_1().min_h_0().overflow_y_scrollbar();
        for (i, r) in rows.iter().enumerate() {
            let sel_id = r.id.clone();
            let conn_id = r.id.clone();
            let edit_id = r.id.clone();
            let del_id = r.id.clone();
            let row_id = r.id.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("sess-{i}")))
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded(px(4.0))
                    .text_size(px(12.0))
                    .when(r.selected, |d| d.bg(theme.colors.secondary))
                    .hover(|d| d.bg(theme.colors.secondary_hover))
                    .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                        this.selected_session = Some(sel_id.clone());
                        if let ClickEvent::Mouse(e) = ev {
                            if e.up.click_count >= 2 {
                                this.connect_session(&row_id, "");
                            }
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme.colors.muted_foreground)
                                    .child(ic(r.icon).small()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_ellipsis()
                                    .child(r.text.clone()),
                            ),
                    )
                    .child(
                        Button::new(SharedString::from(format!("sess-c-{i}")))
                            .icon(ic(IconName::Plug))
                            .tooltip("连接")
                            .compact()
                            .on_click(cx.listener(move |this, _, _, _| {
                                this.connect_session(&conn_id, "");
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("sess-e-{i}")))
                            .icon(ic(IconName::Pencil))
                            .tooltip("编辑")
                            .compact()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit_session(&edit_id, window, cx);
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("sess-d-{i}")))
                            .icon(ic(IconName::Trash))
                            .tooltip("删除")
                            .compact()
                            .on_click(cx.listener(move |this, _, _, _| {
                                this.sessions.remove(&del_id);
                                if let Err(e) = this.sessions.save() {
                                    this.set_toast(e);
                                }
                                if this.selected_session.as_deref() == Some(del_id.as_str()) {
                                    this.selected_session = None;
                                }
                            })),
                    ),
            );
        }
        if rows.is_empty() {
            list = list.child(
                div()
                    .p_2()
                    .text_size(px(11.0))
                    .text_color(dim)
                    .child("暂无会话"),
            );
        }

        div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .border_t_1()
            .border_color(border)
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .child(
                        Button::new("sess-new")
                            .label(tr(lang, "side.new"))
                            .compact()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start_add_session(window, cx);
                            })),
                    )
                    .child(
                        Button::new("sess-reload")
                            .icon(ic(IconName::RefreshCw))
                            .tooltip("刷新")
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| {
                                this.sessions = SessionStore::load();
                                this.set_toast("会话列表已刷新");
                            })),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .pb_1()
                    .child(Input::new(&search).text_size(px(12.0))),
            )
            .child(list)
            .into_any_element()
    }

    /// 打开"新建会话"表单。
    fn start_add_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = Self::mk_input("名称", window, cx);
        let host = Self::mk_input("主机", window, cx);
        let port = Self::mk_input("端口（22/5900/…）", window, cx);
        let user = Self::mk_input("用户名", window, cx);
        let password = Self::mk_password("密码", window, cx);
        let key_path = Self::mk_input("私钥路径", window, cx);
        let domain = Self::mk_input("域（RDP）", window, cx);
        self.add_form = Some(AddFormInputs {
            name,
            host,
            port,
            user,
            password,
            key_path,
            domain,
            kind: SessionKind::Ssh,
            auth_key: false,
            editing: None,
        });
        self.focus_dialog = true;
    }

    /// 编辑已有会话：把字段填进表单。
    fn edit_session(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.sessions.by_id(id).cloned() else {
            return;
        };
        let name = Self::mk_input("名称", window, cx);
        let host = Self::mk_input("主机", window, cx);
        let port = Self::mk_input("端口", window, cx);
        let user = Self::mk_input("用户名", window, cx);
        let password = Self::mk_password("密码", window, cx);
        let key_path = Self::mk_input("私钥路径", window, cx);
        let domain = Self::mk_input("域（RDP）", window, cx);
        Self::set_input(&name, &s.name, window, cx);
        Self::set_input(&host, &s.host, window, cx);
        Self::set_input(&port, &s.port.to_string(), window, cx);
        Self::set_input(&user, &s.user, window, cx);
        Self::set_input(&password, &s.password, window, cx);
        Self::set_input(&key_path, &s.key_path, window, cx);
        Self::set_input(&domain, &s.domain, window, cx);
        self.add_form = Some(AddFormInputs {
            name,
            host,
            port,
            user,
            password,
            key_path,
            domain,
            kind: s.kind,
            auth_key: s.uses_key(),
            editing: Some(s.id),
        });
        self.focus_dialog = true;
    }

    /// 工具标签页：各客户端入口。
    fn sidebar_tools_ui(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let actions = [
            (ToolAction::SshClient, "SSH 终端"),
            (ToolAction::SftpClient, "SFTP 文件传输"),
            (ToolAction::TelnetClient, "Telnet"),
            (ToolAction::RdpClient, "RDP 远程桌面"),
            (ToolAction::VncClient, "VNC 远程桌面"),
            (ToolAction::SpiceClient, "SPICE 远程桌面"),
            (ToolAction::LocalFiles, "本地文件浏览"),
            (ToolAction::Browser, "内置浏览器"),
        ];
        let mut list = div().v_flex().flex_1().min_h_0().overflow_y_scrollbar().p_1();
        for (i, (action, label)) in actions.into_iter().enumerate() {
            list = list.child(
                Button::new(SharedString::from(format!("tool-{i}")))
                    .label(label)
                    .compact()
                    .on_click(cx.listener(move |this, _, _, _| this.run_tool_action(action))),
            );
        }
        list.into_any_element()
    }

    /// 宏标签页：宏列表 + 执行按钮。
    fn sidebar_macros_ui(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::global(cx).clone();
        let dim = theme.colors.muted_foreground;
        let entries: Vec<(usize, String, String, bool)> = self
            .macros
            .macros
            .iter()
            .enumerate()
            .map(|(i, m)| (i, m.name.clone(), m.description.clone(), m.enabled))
            .collect();

        let mut list = div().v_flex().flex_1().min_h_0().overflow_y_scrollbar().p_1();
        for (i, name, desc, enabled) in entries {
            list = list
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_ellipsis()
                                .text_size(px(12.0))
                                .text_color(if enabled {
                                    theme.colors.foreground
                                } else {
                                    dim
                                })
                                .child(name),
                        )
                        .child(
                            Button::new(SharedString::from(format!("macro-run-{i}")))
                                .icon(ic(IconName::Play))
                                .compact()
                                .disabled(!enabled)
                                .on_click(cx.listener(move |this, _, _, _| {
                                    this.send_macro_to_active(i);
                                })),
                        ),
                )
                .when(!desc.is_empty(), |d| {
                    d.child(
                        div()
                            .px_1()
                            .text_size(px(10.0))
                            .text_color(dim)
                            .text_ellipsis()
                            .child(desc),
                    )
                });
        }
        let mut panel = div().v_flex().flex_1().min_h_0().border_t_1().border_color(theme.colors.border);
        panel = panel.child(
            div()
                .h_flex()
                .items_center()
                .px_2()
                .py_1()
                .child(div().flex_1().text_size(px(12.0)).child("键宏"))
                .child(
                    Button::new("macro-manage")
                        .label("管理")
                        .compact()
                        .on_click(cx.listener(|this, _, _, _| {
                            this.open_macro_tab();
                        })),
                ),
        );
        panel.child(list).into_any_element()
    }

    // ------------------------------------------------------------------
    // 标签条
    // ------------------------------------------------------------------

    fn tab_strip_ui(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::global(cx).clone();
        let border = theme.colors.border;
        let dim = theme.colors.muted_foreground;

        struct Row {
            label: String,
            icon: Option<IconName>,
            active: bool,
            failed: bool,
            closable: bool,
        }
        let rows: Vec<Row> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, tab)| {
                let (label, icon) = match tab {
                    TabContent::Connecting { title, .. } => (title.clone(), Some(IconName::Loader)),
                    TabContent::Terminal(t) => {
                        if t.active_dead() {
                            (format!("{} 已断开", t.title()), Some(IconName::X))
                        } else {
                            (t.title(), Some(IconName::SquareTerminal))
                        }
                    }
                    TabContent::Failed(_) => ("连接失败".to_string(), Some(IconName::TriangleAlert)),
                    TabContent::Browser(_) => (tab.title(), Some(IconName::Globe)),
                    TabContent::RemoteDesktop(_) => (tab.title(), Some(IconName::Monitor)),
                    TabContent::LocalFiles(_) => (tab.title(), Some(IconName::Folder)),
                    TabContent::Macro => (tab.title(), Some(IconName::Keyboard)),
                    TabContent::PortScan => (tab.title(), Some(IconName::Radar)),
                };
                Row {
                    label,
                    icon,
                    active: i == self.active,
                    failed: matches!(tab, TabContent::Failed(_)),
                    closable: true,
                }
            })
            .collect();

        let mut strip = div().h_flex().items_center().gap_1().px_2().overflow_x_scrollbar();
        for (i, r) in rows.iter().enumerate() {
            let color = if r.failed { theme.colors.danger } else { dim };
            let idx = i;
            strip = strip.child(
                div()
                    .id(SharedString::from(format!("tab-{i}")))
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .h(px(TABS_H - 6.0))
                    .relative()
                    .rounded(px(6.0))
                    .text_size(px(12.0))
                    .when(r.active, |d| d.bg(theme.colors.secondary))
                    .hover(|d| d.bg(theme.colors.secondary_hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.active = idx;
                        cx.notify();
                    }))
                    .when_some(r.icon, |d, icon| {
                        d.child(ic(icon).xsmall().text_color(if r.active {
                            theme.colors.foreground
                        } else {
                            color
                        }))
                    })
                    .child(
                        div()
                            .text_color(if r.active {
                                theme.colors.foreground
                            } else {
                                color
                            })
                            .child(r.label.clone()),
                    )
                    .when(r.closable, |d| {
                        d.child(
                            Button::new(SharedString::from(format!("tab-close-{i}")))
                                .icon(ic(IconName::X))
                                .tooltip("关闭标签")
                                .compact()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.close_tab(idx);
                                    window.blur(cx);
                                })),
                        )
                    })
                    .when(r.active, |d| {
                        // 活动标签底部的高亮条
                        d.child(
                            div()
                                .absolute()
                                .left(px(8.0))
                                .right(px(8.0))
                                .bottom(px(0.0))
                                .h(px(2.0))
                                .rounded(px(1.0))
                                .bg(theme.colors.primary),
                        )
                    }),
            );
        }

        div()
            .h_flex()
            .items_center()
            .flex_none()
            .h(px(TABS_H))
            .bg(theme.colors.background)
            .border_b_1()
            .border_color(border)
            .child(strip)
            .into_any_element()
    }

    // ------------------------------------------------------------------
    // 中央工作区
    // ------------------------------------------------------------------

    fn center_content_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::global(cx).clone();
        let dim = theme.colors.muted_foreground;
        let active = self.active.min(self.tabs.len().saturating_sub(1));

        if self.tabs.is_empty() {
            let lang = self.i18n.lang;
            // 空状态卡片：图标 + 标题 + 快捷键提示，避免中间孤零零两行字。
            let shortcuts = [
                ("Ctrl+N", tr(lang, "tb.local")),
                ("Ctrl+T", tr(lang, "tb.browser")),
                ("Ctrl+Shift+R", tr(lang, "item.remote_desktop")),
            ];
            let mut chips = div().h_flex().items_center().gap_2().mt_2();
            for (key, desc) in shortcuts {
                chips = chips.child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1()
                        .rounded(px(6.0))
                        .bg(theme.colors.muted)
                        .border_1()
                        .border_color(theme.colors.border)
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.colors.foreground)
                                .child(key),
                        )
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(dim)
                                .child(desc),
                        ),
                );
            }
            return div()
                .flex_1()
                .v_flex()
                .items_center()
                .justify_center()
                .gap_3()
                .child(
                    div()
                        .v_flex()
                        .items_center()
                        .justify_center()
                        .gap_3()
                        .px_10()
                        .py_8()
                        .rounded(px(12.0))
                        .bg(theme.colors.background)
                        .border_1()
                        .border_color(theme.colors.border)
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(px(56.0))
                                .rounded(px(14.0))
                                .bg(theme.colors.muted)
                                .child(ic(IconName::SquareTerminal).large().text_color(dim)),
                        )
                        .child(
                            div()
                                .text_size(px(16.0))
                                .text_color(theme.colors.foreground)
                                .child(tr(lang, "welcome.subtitle")),
                        )
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(dim)
                                .child(tr(lang, "welcome.desc")),
                        )
                        .child(chips),
                )
                .into_any_element();
        }

        // 键宏 / 端口扫描是「应用级面板」（面板本体挂在 app 上，不在标签里），
        // 因此这里要在借用 tabs 之前先渲染，否则会和 &mut self.tabs[active] 冲突。
        match self.tabs.get(active) {
            Some(TabContent::Macro) => return self.macros.render_panel(window, cx),
            Some(TabContent::PortScan) => return self.port_scan.render_panel(window, cx),
            _ => {}
        }

        // 书签栏 / 历史记录面板同理：它们读的是应用级的 browser_service，
        // 必须在借用 tabs 之前构建好。
        let is_browser = matches!(self.tabs.get(active), Some(TabContent::Browser(_)));
        let bookmark_bar = if is_browser && self.browser_service.show_bookmarks {
            self.bookmark_bar_ui(cx)
        } else {
            div().into_any_element()
        };
        let history_panel = if is_browser && self.show_browser_history {
            self.browser_history_ui(cx)
        } else {
            div().into_any_element()
        };

        match &mut self.tabs[active] {
            TabContent::Terminal(_) => self.terminal_area_ui(window, cx),
            TabContent::Connecting { title, .. } => div()
                .flex_1()
                .v_flex()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(14.0))
                                .text_color(dim)
                                .child(ic(IconName::Loader).small()),
                        )
                        .child(div().text_size(px(14.0)).child(title.clone())),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(dim)
                        .child("正在连接…"),
                )
                .into_any_element(),
            TabContent::Failed(msg) => div()
                .flex_1()
                .v_flex()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .text_size(px(14.0))
                        .text_color(theme.colors.danger)
                        .child("连接失败"),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(dim)
                        .max_w(px(480.0))
                        .child(msg.clone()),
                )
                .into_any_element(),
            TabContent::Browser(b) => {
                b.ensure_webview();
                b.poll_events();

                // 地址栏输入实体（惰性创建 + Enter 订阅）
                if b.address_input.is_none() {
                    let input = cx.new(|cx| {
                        InputState::new(window, cx).placeholder("搜索或输入网址".to_string())
                    });
                    input.update(cx, |s, cx| {
                        s.set_value(b.address_bar.clone(), window, cx)
                    });
                    cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                        if matches!(event, InputEvent::PressEnter { .. }) {
                            let text = this
                                .active_browser()
                                .and_then(|b| {
                                    b.address_input
                                        .as_ref()
                                        .map(|s| s.read(cx).value().to_string())
                                })
                                .unwrap_or_default();
                            let url = this.browser_service.resolve_address(&text);
                            if let Some(b) = this.active_browser_mut() {
                                b.navigate(url.clone());
                                b.load_url(&url);
                                this.browser_service.add_to_history(url);
                            }
                            cx.notify();
                        }
                    })
                    .detach();
                    b.address_input = Some(input);
                }
                // 非聚焦时地址栏跟随当前 URL
                let focused = b
                    .address_input
                    .as_ref()
                    .map(|s| s.read(cx).focus_handle(cx).is_focused(window))
                    .unwrap_or(false);
                b.address_focused = focused;
                if !focused {
                    let input = b.address_input.clone().unwrap();
                    let cur = input.read(cx).value().to_string();
                    if cur != b.url && cur != b.address_bar {
                        input.update(cx, |s, cx| s.set_value(b.url.clone(), window, cx));
                    } else if cur != b.address_bar {
                        input.update(cx, |s, cx| {
                            s.set_value(b.address_bar.clone(), window, cx)
                        });
                    }
                }

                // Ctrl+L：请求聚焦地址栏
                if self.focus_address {
                    self.focus_address = false;
                    if let Some(input) = &b.address_input {
                        let fh = input.read(cx).focus_handle(cx).clone();
                        window.focus(&fh, cx);
                    }
                }

                let addr = b.address_input.clone().unwrap();
                let theme2 = Theme::global(cx).clone();
                div()
                    .v_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_1()
                            .px_2()
                            .py_1()
                            .border_b_1()
                            .border_color(theme2.colors.border)
                            .child(
                                Button::new("web-back")
                                    .icon(ic(IconName::ArrowLeft))
                                    .tooltip("后退")
                                    .compact()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(b) = this.active_browser_mut() {
                                            b.go_back_web();
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("web-fwd")
                                    .icon(ic(IconName::ArrowRight))
                                    .tooltip("前进")
                                    .compact()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(b) = this.active_browser_mut() {
                                            b.go_forward_web();
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("web-reload")
                                    .icon(ic(IconName::RotateCw))
                                    .tooltip("刷新")
                                    .compact()
                                    .on_click(cx.listener(|this, _, _, _| {
                                        this.active_browser_mut().map(|b| b.reload());
                                    })),
                            )
                            .child(Input::new(&addr).flex_1().min_w_0().text_size(px(12.0)))
                            .child(
                                Button::new("web-bookmarks")
                                    .icon(ic(IconName::Bookmark))
                                    .tooltip("书签栏")
                                    .compact()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.browser_service.show_bookmarks =
                                            !this.browser_service.show_bookmarks;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("web-history")
                                    .icon(ic(IconName::Clock))
                                    .tooltip("历史记录")
                                    .compact()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_browser_history = !this.show_browser_history;
                                        cx.notify();
                                    })),
                            ),
                    )
                    // 书签栏 / 历史记录面板（在借用 tabs 之前已构建好）
                    .child(bookmark_bar)
                    .child(history_panel)
                    // 占位区：canvas 记录内容区 bounds 供 WebView 对齐
                    .child(
                        if let Some(err) = &b.webview_error {
                            // WebView2 初始化失败（未装 Runtime / panic 兜底）：
                            // 显示可读错误，而不是一片黑 / 直接崩溃。
                            div()
                                .flex_1()
                                .min_h_0()
                                .flex()
                                .flex_col()
                                .items_center()
                                .justify_center()
                                .gap_2()
                                .p_6()
                                .text_color(theme2.colors.danger)
                                .text_size(px(13.0))
                                .children(err.lines().map(|l| div().child(l.to_string())))
                                .into_any_element()
                        } else if !b.has_webview() {
                            // WebView 在专属线程上创建（不能占用渲染线程），
                            // 这里显示等待态，避免出现「点了没反应」
                            div()
                                .flex_1()
                                .min_h_0()
                                .flex()
                                .flex_col()
                                .items_center()
                                .justify_center()
                                .gap_2()
                                .h_flex()
                                .items_center()
                                .gap_2()
                                .text_size(px(13.0))
                                .text_color(theme2.colors.muted_foreground)
                                .child(ic(IconName::Loader).small())
                                .child("正在启动 WebView2 引擎…")
                                .into_any_element()
                        } else {
                            let web_idx = self.active;
                            div()
                                .flex_1()
                                .min_h_0()
                                .v_flex()
                                .child(
                                    canvas(
                                        move |bounds, _window, cx| {
                                            cx.set_global(BrowserArea(bounds, web_idx));
                                        },
                                        |_bounds, (), _window, _cx| {},
                                    )
                                    .flex_1()
                                    .min_h_0(),
                                )
                                .into_any_element()
                        },
                    )
                    .into_any_element()
            }
            TabContent::RemoteDesktop(tab) => tab.render(window, cx),
            TabContent::LocalFiles(fb) => fb.render(false, window, cx),
            TabContent::Macro | TabContent::PortScan => div().into_any_element(),
        }
    }

    /// 浏览器书签栏：横向排列书签，点击直接访问。
    fn bookmark_bar_ui(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::global(cx).clone();
        let dim = theme.colors.muted_foreground;
        // 按分组（folder）聚在一起，组与组之间加分隔线
        let mut groups: Vec<(String, Vec<(String, String)>)> = Vec::new();
        for bm in &self.browser_service.bookmarks {
            match groups.iter_mut().find(|(f, _)| *f == bm.folder) {
                Some((_, list)) => list.push((bm.name.clone(), bm.url.clone())),
                None => groups.push((
                    bm.folder.clone(),
                    vec![(bm.name.clone(), bm.url.clone())],
                )),
            }
        }

        let mut bar = div()
            .h_flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.colors.border)
            .overflow_x_scrollbar();
        for (gi, (folder, items)) in groups.iter().enumerate() {
            if gi > 0 {
                bar = bar.child(
                    div()
                        .w(px(1.0))
                        .h(px(12.0))
                        .bg(theme.colors.border),
                );
            }
            bar = bar.child(
                div()
                    .text_size(px(10.0))
                    .text_color(dim)
                    .flex_none()
                    .child(folder.clone()),
            );
            for (name, url) in items {
                let u = url.clone();
                bar = bar.child(
                    div()
                        .id(SharedString::from(format!("bm-{}", u)))
                        .text_size(px(11.0))
                        .text_color(theme.colors.foreground)
                        .px_1()
                        .rounded(px(3.0))
                        .cursor_pointer()
                        .hover(|d| d.bg(theme.colors.secondary_hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.navigate_browser(&u);
                            cx.notify();
                        }))
                        .child(name.clone()),
                );
            }
        }
        bar.into_any_element()
    }

    /// 浏览器历史记录面板：最近访问的地址，点击直接访问。
    fn browser_history_ui(&self, cx: &mut Context<Self>) -> AnyElement {
        use gpui_kit::component::scroll::ScrollableElement as _;
        let theme = Theme::global(cx).clone();
        let dim = theme.colors.muted_foreground;
        let items: Vec<String> = self.browser_service.history.iter().take(50).cloned().collect();

        let mut list = div()
            .v_flex()
            .max_h(px(220.0))
            .overflow_y_scrollbar()
            .border_b_1()
            .border_color(theme.colors.border);
        if items.is_empty() {
            list = list.child(
                div()
                    .px_2()
                    .py_1()
                    .text_size(px(11.0))
                    .text_color(dim)
                    .child("暂无历史记录"),
            );
        }
        for (i, url) in items.into_iter().enumerate() {
            let u = url.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("hist-{i}")))
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_0p5()
                    .text_size(px(11.0))
                    .cursor_pointer()
                    .hover(|d| d.bg(theme.colors.secondary_hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.navigate_browser(&u);
                        this.show_browser_history = false;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_ellipsis()
                            .child(url.clone()),
                    ),
            );
        }
        list.into_any_element()
    }

    /// 在当前浏览器标签页访问指定地址。
    fn navigate_browser(&mut self, url: &str) {
        let resolved = self.browser_service.resolve_address(url);
        self.browser_service.add_to_history(resolved.clone());
        if let Some(b) = self.active_browser_mut() {
            b.navigate(resolved.clone());
            b.load_url(&resolved);
        }
    }

    /// 终端标签页：canvas 绘制 + 键盘/鼠标事件 + 查找栏。
    /// 终端区：可分屏的若干终端格。
    ///
    /// 结构：`term-area`（键盘 + 鼠标命中路由）→ 分屏树（`h_resizable` /
    /// `v_resizable` 递归）→ 每个叶子一个 `pane-*` div（canvas 画该格快照）。
    ///
    /// 关键变化：每个窗格的尺寸**不再用视口估算**。canvas 的 prepaint 把本格
    /// bounds 写进 `pane_areas`，下一帧据此 `fit_to`，所以分屏后每格各自算列数/
    /// 行数。首帧还没有测量值时才退回「视口减去各固定栏」的估算。
    fn terminal_area_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let _theme = Theme::global(cx).clone();
        let focus = self.term_focus.clone();

        // 键盘/鼠标事件处理用的快照句柄（App 内已持有，直接从 self 取）
        let mut area = div()
            .id("term-area")
            .flex_1()
            .min_h_0()
            .v_flex()
            .relative()
            .overflow_hidden()
            .bg(crate::term::bg_color())
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                if let Some(f) = this.term_focus.clone() {
                    window.focus(&f, cx);
                }
            }))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                // 只有键盘焦点真在终端上时才往 pty 送字节。
                // 这个 div 是查找输入框的祖先，按键会冒泡上来；不判断的话
                // 在查找框里打字会同时打进 shell（也就是「输入不进去/乱码」）。
                let term_focused = this
                    .term_focus
                    .as_ref()
                    .map(|f| f.is_focused(window))
                    .unwrap_or(false);
                if !term_focused {
                    return;
                }
                // 查找栏开着时 Esc 直接关掉（焦点又回到终端的情况）
                if this.find_open && ev.keystroke.key == "escape" {
                    this.close_find(cx);
                    cx.stop_propagation();
                    return;
                }
                // 宏快捷键优先：命中就把这次按键吃掉，改为执行宏。
                // （宏面板里「触发方式 = 快捷键」的宏在这里生效）
                let hotkey = crate::macros::hotkey_of(&ev.keystroke);
                if let Some(idx) = this.macros.get_macro_by_hotkey(&hotkey) {
                    this.send_macro_to_active(idx);
                    cx.notify();
                    return;
                }
                // 输入一律进**活动窗格**：分屏后每格一个 pty，不能发错人
                let active = this.active.min(this.tabs.len().saturating_sub(1));
                if let Some(pane) = this.tabs.get_mut(active).and_then(|t| t.terminal_mut()) {
                    pane.handle_keystroke(&ev.keystroke, cx);
                    cx.notify();
                }
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                if let Some(f) = this.term_focus.clone() {
                    window.focus(&f, cx);
                }
                this.term_mouse(ev.button, ev.position, true, ev.modifiers, cx);
            }))
            .on_mouse_down(MouseButton::Middle, cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                this.term_mouse(ev.button, ev.position, true, ev.modifiers, cx);
            }))
            .on_mouse_down(MouseButton::Right, cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                this.term_mouse(ev.button, ev.position, true, ev.modifiers, cx);
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, ev: &MouseUpEvent, _, cx| {
                this.term_mouse_up(ev.button, ev.position, ev.modifiers, cx);
            }))
            .on_mouse_up(MouseButton::Middle, cx.listener(|this, ev: &MouseUpEvent, _, cx| {
                this.term_mouse_up(ev.button, ev.position, ev.modifiers, cx);
            }))
            .on_mouse_up(MouseButton::Right, cx.listener(|this, ev: &MouseUpEvent, _, cx| {
                this.term_mouse_up(ev.button, ev.position, ev.modifiers, cx);
            }))
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                this.term_mouse_move(ev.position, ev.pressed_button, cx);
            }))
            .on_scroll(cx.listener(|this, ev: &ScrollEvent, _, cx| {
                this.term_scroll(ev.delta, ev.position, cx);
            }));

        if let Some(f) = focus.clone() {
            area = area.track_focus(&f).key_context("Terminal");
        }

        // 分屏树（叶子 = 一个终端格；canvas 的 prepaint 记录本格 bounds）
        area = area.child(
            div()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .child(self.pane_tree_element(cx)),
        );

        // 查找栏：停靠在终端上方（不再压在终端底部遮挡命令行），
        // 独立于 term-area 之外——点击/键盘都不会被终端抢走焦点。
        let mut column = div().v_flex().flex_1().min_h_0();
        if self.find_open {
            column = column.child(self.find_bar_ui(window, cx));
        }
        column.child(area).into_any_element()
    }

    /// 递归构建分屏树元素。
    ///
    /// 同一个分割节点每帧都拿到**同一个 ElementId**（`("pane-split", node_id)`），
    /// 于是 `ResizablePanelGroup` 内部按 id 缓存的 `ResizableState` 不会被重建，
    /// 用户拖出来的比例在重绘 / 切标签后仍然保留。
    fn pane_tree_element(&self, cx: &App) -> AnyElement {
        let Some(tab) = self
            .tabs
            .get(self.active.min(self.tabs.len().saturating_sub(1)))
            .and_then(|t| t.terminal_tab())
        else {
            return div().flex_1().min_h_0().into_any_element();
        };
        self.split_node_element(tab, tab.layout.root(), cx)
    }

    fn split_node_element(&self, tab: &TerminalTab, node: &SplitNode, cx: &App) -> AnyElement {
        match node {
            SplitNode::Leaf { pane, .. } => self.pane_element(tab, *pane, cx),
            SplitNode::Split {
                node: id,
                axis,
                children,
            } => {
                let panels = children
                    .iter()
                    .map(|child| resizable_panel().child(self.split_node_element(tab, child, cx)));
                let group = match axis {
                    Axis::Horizontal => h_resizable(("pane-split", *id)),
                    Axis::Vertical => v_resizable(("pane-split", *id)),
                };
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(group.children(panels))
                    .into_any_element()
            }
        }
    }

    /// 单个终端格：已连上画快照；连接中 / 失败画占位。
    fn pane_element(&self, tab: &TerminalTab, id: PaneId, cx: &App) -> AnyElement {
        let theme = Theme::global(cx).clone();
        let dim = theme.colors.muted_foreground;
        let is_active = tab.active == id;
        let multi = tab.is_split();

        let base = div()
            .id(("term-pane", id))
            .flex_1()
            .min_w_0()
            .min_h_0()
            .relative()
            .overflow_hidden()
            // 分屏时才描边：单格保持原来的观感；非活动格用全透明（几何不变，
            // 只有颜色变，避免切换焦点时终端网格跳 1px）
            .when(multi, |d| {
                d.border_1().border_color(if is_active {
                    theme.colors.primary
                } else {
                    gpui_kit::hsla(0.0, 0.0, 0.0, 0.0)
                })
            });

        match tab.slot(id).map(|s| &s.state) {
            Some(PaneState::Live(_)) => {
                let area_rc = self
                    .pane_areas
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| Rc::new(Cell::new(Bounds::default())));
                let snap = self.pane_snaps.get(&id).cloned();
                let prepaint_area = area_rc;
                let paint_snap = snap;
                base.child(
                    canvas(
                        move |bounds, _window, _cx| {
                            // 记录本格「终端绘制区」的原始 bounds（含内边距）。
                            // 文本网格原点 = 该 bounds 左上 + TERM_PAD，
                            // 下一帧 fit_to 与鼠标列/行换算都用这一份数据。
                            prepaint_area.set(bounds);
                        },
                        move |bounds, (), window, cx| {
                            if let Some(snap) = &paint_snap {
                                let origin = Point::new(
                                    bounds.origin.x + px(TERM_PAD),
                                    bounds.origin.y + px(TERM_PAD),
                                );
                                crate::term::paint_snapshot(snap, origin, window, cx);
                            }
                        },
                    )
                    .size_full(),
                )
                .into_any_element()
            }
            Some(PaneState::Connecting { title, .. }) => base
                .v_flex()
                .items_center()
                .justify_center()
                .gap_2()
                .child(ic(IconName::Loader).small().text_color(dim))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.colors.foreground)
                        .child(format!("{title} 正在连接…")),
                )
                .into_any_element(),
            Some(PaneState::Failed { msg, .. }) => base
                .v_flex()
                .items_center()
                .justify_center()
                .gap_2()
                .px_4()
                .child(ic(IconName::TriangleAlert).small().text_color(theme.colors.danger))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.colors.danger)
                        .child("连接失败"),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(dim)
                        .max_w(px(420.0))
                        .child(msg.clone()),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(dim)
                        .child("用 Ctrl+Shift+W 关闭该窗格"),
                )
                .into_any_element(),
            None => base.into_any_element(),
        }
    }

    /// 命中判定：鼠标位置落在哪个窗格里。
    ///
    /// 与分割条抢事件的处理：分割条约 4px 宽、压在相邻两格的公共边界上，而
    /// 事件会从分割条冒泡到容器（本函数所在层的监听器）。因此对**内部边界**
    /// （与兄弟格共享的那条边）向内缩 [`SPLIT_GRIP`] 像素：落在这一带就判定为
    /// 「哪格都没命中」，拖分割条不会顺手在终端里起一个选区。外边界不缩，
    /// 免得贴着侧栏/状态栏的边缘出现点不动的死区。
    fn pane_at(&self, pos: Point<Pixels>) -> Option<PaneId> {
        let tab = self
            .tabs
            .get(self.active.min(self.tabs.len().saturating_sub(1)))?
            .terminal_tab()?;
        let rects: Vec<(PaneId, Bounds<Pixels>)> = tab
            .leaf_ids()
            .into_iter()
            .filter_map(|id| self.pane_areas.get(&id).map(|rc| (id, rc.get())))
            .filter(|(_, b)| b.size.width.as_f32() > 1.0 && b.size.height.as_f32() > 1.0)
            .collect();

        let grip = px(SPLIT_GRIP);
        let near = |a: Pixels, b: Pixels| (a - b).as_f32().abs() <= 1.0;

        for (id, r) in rects.iter() {
            let left = if rects
                .iter()
                .any(|(o, s)| o != id && near(s.right(), r.left()))
            {
                r.left() + grip
            } else {
                r.left()
            };
            let right = if rects
                .iter()
                .any(|(o, s)| o != id && near(s.left(), r.right()))
            {
                r.right() - grip
            } else {
                r.right()
            };
            let top = if rects
                .iter()
                .any(|(o, s)| o != id && near(s.bottom(), r.top()))
            {
                r.top() + grip
            } else {
                r.top()
            };
            let bottom = if rects
                .iter()
                .any(|(o, s)| o != id && near(s.top(), r.bottom()))
            {
                r.bottom() - grip
            } else {
                r.bottom()
            };
            if right <= left || bottom <= top {
                continue;
            }
            let hit = Bounds::new(
                Point::new(left, top),
                size(right - left, bottom - top),
            );
            if hit.contains(&pos) {
                return Some(*id);
            }
        }
        None
    }

    /// 某个窗格的（单元格尺寸, 文本网格原点）。两者都来自上一帧快照/测量。
    fn pane_cell_origin(&self, id: PaneId) -> Option<(Size<Pixels>, Point<Pixels>)> {
        let area = self.pane_areas.get(&id)?.get();
        let snap = self.pane_snaps.get(&id)?;
        Some((
            snap.cell,
            Point::new(
                area.origin.x + px(TERM_PAD),
                area.origin.y + px(TERM_PAD),
            ),
        ))
    }

    /// 本次鼠标事件的投递目标。
    ///
    /// `during_drag = true` 时优先用按下时锁定的窗格 —— 拖动选区拖出本格
    /// （甚至拖到分割条上）也仍然投给起始格，不会中途换手。
    fn mouse_target(&self, pos: Point<Pixels>, during_drag: bool) -> Option<PaneId> {
        if during_drag {
            if let Some(id) = self.mouse_pane {
                return Some(id);
            }
        }
        self.pane_at(pos)
    }

    /// 活动窗格 id（滚动等没有明确落点时用）。
    fn active_pane_id(&self) -> Option<PaneId> {
        self.tabs
            .get(self.active.min(self.tabs.len().saturating_sub(1)))
            .and_then(|t| t.terminal_tab())
            .map(|tab| tab.active_id())
    }

    /// 终端查找栏：输入即搜，Enter 下一个 / Shift+Enter 上一个 / Esc 关闭。
    fn find_bar_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::global(cx).clone();

        if self.find_input.is_none() {
            let input = Self::mk_input("查找文本（Enter 下一个，Esc 关闭）", window, cx);
            cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                match event {
                    InputEvent::Change => {
                        let needle = Self::input_value(&this.find_input, cx);
                        let idx = this.active.min(this.tabs.len().saturating_sub(1));
                        if let Some(pane) =
                            this.tabs.get_mut(idx).and_then(|t| t.terminal_mut())
                        {
                            pane.set_search(&needle);
                        }
                        cx.notify();
                    }
                    InputEvent::PressEnter { shift, .. } => {
                        this.search_step(!*shift, cx);
                    }
                    _ => {}
                }
            })
            .detach();
            self.find_input = Some(input);
        }
        let input = self.find_input.clone().unwrap();

        // Ctrl+F 打开时把焦点交给输入框（原来只置了 focus_dialog，没人消费，
        // 结果焦点还在终端上，字全打进了 shell）。
        if std::mem::take(&mut self.find_focus) {
            let handle = input.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }

        let (idx, total) = {
            let i = self.active.min(self.tabs.len().saturating_sub(1));
            self.tabs
                .get(i)
                .and_then(|t| t.terminal())
                .map(|p| {
                    let n = p.search_matches.len();
                    (if n == 0 { 0 } else { p.search_index + 1 }, n)
                })
                .unwrap_or((0, 0))
        };
        let counter = if total == 0 {
            "无匹配".to_string()
        } else {
            format!("{idx}/{total}")
        };

        div()
            .id("find-bar")
            .h_flex()
            .flex_none()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .bg(theme.colors.background)
            .border_b_1()
            .border_color(theme.colors.border)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                // Input 的 Escape 默认会往上冒泡（clean_on_escape=false）
                if ev.keystroke.key == "escape" {
                    this.close_find(cx);
                    cx.stop_propagation();
                }
            }))
            .child(ic(IconName::Search).small().text_color(theme.colors.muted_foreground))
            .child(Input::new(&input).flex_1().text_size(px(12.0)))
            .child(
                div()
                    .flex_none()
                    .min_w(px(56.0))
                    .text_size(px(11.0))
                    .text_color(theme.colors.muted_foreground)
                    .child(counter),
            )
            .child(
                Button::new("find-prev")
                    .icon(ic(IconName::ArrowUp))
                    .tooltip("上一个（Shift+Enter）")
                    .compact()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.search_step(false, cx);
                        // 点完按钮把焦点还给输入框，否则继续打字会失去焦点
                        this.find_focus = true;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("find-next")
                    .icon(ic(IconName::ArrowDown))
                    .tooltip("下一个（Enter）")
                    .compact()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.search_step(true, cx);
                        this.find_focus = true;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("find-close")
                    .icon(ic(IconName::X))
                    .tooltip("关闭（Esc）")
                    .compact()
                    .on_click(cx.listener(|this, _, _, cx| this.close_find(cx))),
            )
            .into_any_element()
    }

    /// 跳到下一个 / 上一个匹配项（Enter / Shift+Enter / 上下箭头共用）。
    ///
    /// 关键点：只有在关键字真的发生变化时才调用 `set_search`。它会把当前索引
    /// 重置到「最后一处匹配」，如果每步跳转前都调一次，索引永远回到同一处，
    /// 表现就是「点下一个没反应 / 只跳一次」。
    fn search_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let needle = Self::input_value(&self.find_input, cx);
        let idx = self.active.min(self.tabs.len().saturating_sub(1));
        if let Some(pane) = self.tabs.get_mut(idx).and_then(|t| t.terminal_mut()) {
            if pane.search != needle {
                pane.set_search(&needle);
            } else if forward {
                pane.search_next();
            } else {
                pane.search_prev();
            }
        }
        cx.notify();
    }

    /// 关闭查找栏：清空高亮，焦点交回终端。
    fn close_find(&mut self, cx: &mut Context<Self>) {
        self.find_open = false;
        let idx = self.active.min(self.tabs.len().saturating_sub(1));
        if let Some(pane) = self.tabs.get_mut(idx).and_then(|t| t.terminal_mut()) {
            pane.clear_search();
        }
        self.focus_term = true;
        cx.notify();
    }

    /// 终端右键菜单：复制 / 粘贴 / 全选。
    ///
    /// 键盘快捷键之外再给一条纯鼠标路径——不同输入法/键盘布局下
    /// Ctrl+Shift+C 未必到得了终端，菜单永远可用。
    fn term_context_menu_ui(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pos = self.term_menu?;
        let theme = Theme::global(cx).clone();
        let idx = self.active.min(self.tabs.len().saturating_sub(1));
        let (has_sel, has_text) = self
            .tabs
            .get(idx)
            .and_then(|t| t.terminal())
            .map(|p| (p.has_selection(), true))
            .unwrap_or((false, false));

        let dim = theme.colors.muted_foreground;
        let fg = theme.colors.foreground;

        let item = |id: &'static str, label: &'static str, enabled: bool| {
            let color = if enabled { fg } else { dim };
            div()
                .id(id)
                .h_flex()
                .items_center()
                .px_2()
                .py_1()
                .text_size(px(12.0))
                .text_color(color)
                .when(enabled, |d| d.cursor_pointer().hover(|d| d.bg(theme.colors.secondary_hover)))
                .child(label)
        };

        let mut menu = div()
            .id("term-menu")
            .absolute()
            .left(pos.x)
            .top(pos.y)
            .min_w(px(160.0))
            .v_flex()
            .py_1()
            .rounded(px(6.0))
            .bg(theme.colors.background)
            .border_1()
            .border_color(theme.colors.border)
            // 菜单内部的点击不能冒泡到遮罩层，否则先被关掉
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation());

        menu = menu.child(item("term-menu-copy", "复制", has_sel).on_click(cx.listener(
            |this, _, _, cx| {
                let idx = this.active.min(this.tabs.len().saturating_sub(1));
                if let Some(pane) = this.tabs.get_mut(idx).and_then(|t| t.terminal_mut()) {
                    if pane.copy_selection(cx) {
                        this.set_toast("已复制到剪贴板");
                    } else {
                        this.set_toast("没有选中内容");
                    }
                }
                this.term_menu = None;
                this.focus_term = true;
                cx.notify();
            },
        )));
        menu = menu.child(item("term-menu-paste", "粘贴", has_text).on_click(cx.listener(
            |this, _, _, cx| {
                let idx = this.active.min(this.tabs.len().saturating_sub(1));
                if let Some(pane) = this.tabs.get_mut(idx).and_then(|t| t.terminal_mut()) {
                    if pane.paste_clipboard(cx) {
                        this.set_toast("已粘贴");
                    } else {
                        this.set_toast("剪贴板没有文本");
                    }
                }
                this.term_menu = None;
                this.focus_term = true;
                cx.notify();
            },
        )));
        menu = menu.child(
            div()
                .h(px(1.0))
                .my_1()
                .mx_2()
                .bg(theme.colors.border)
                .into_any_element(),
        );
        menu = menu.child(
            item("term-menu-all", "全选", has_text).on_click(cx.listener(|this, _, _, cx| {
                let idx = this.active.min(this.tabs.len().saturating_sub(1));
                if let Some(pane) = this.tabs.get_mut(idx).and_then(|t| t.terminal_mut()) {
                    pane.select_all();
                }
                this.term_menu = None;
                this.focus_term = true;
                cx.notify();
            })),
        );

        // 分屏：右键菜单是分屏最容易被发现的地方（菜单栏也有同样四项）。
        // 文案沿用这个弹出菜单的既有风格（中文硬编码，与「复制/粘贴/全选」一致）。
        menu = menu.child(
            div()
                .h(px(1.0))
                .my_1()
                .mx_2()
                .bg(theme.colors.border)
                .into_any_element(),
        );
        let can_split = self.can_split();
        let multi = self.has_multiple_panes();
        menu = menu.child(
            item("term-menu-split-r", "向右分屏（Ctrl+Shift+D）", can_split).on_click(cx.listener(
                |this, _, _, cx| {
                    this.term_menu = None;
                    this.split_active_pane(Axis::Horizontal, false, cx);
                },
            )),
        );
        menu = menu.child(
            item("term-menu-split-d", "向下分屏（Ctrl+Shift+U）", can_split).on_click(cx.listener(
                |this, _, _, cx| {
                    this.term_menu = None;
                    this.split_active_pane(Axis::Vertical, false, cx);
                },
            )),
        );
        menu = menu.child(
            item("term-menu-pane-next", "下一个窗格（Ctrl+Shift+P）", multi).on_click(cx.listener(
                |this, _, _, cx| {
                    this.term_menu = None;
                    this.focus_next_pane(cx);
                },
            )),
        );
        menu = menu.child(
            item("term-menu-pane-close", "关闭当前窗格（Ctrl+Shift+W）", multi).on_click(
                cx.listener(|this, _, _, cx| {
                    this.term_menu = None;
                    this.close_active_pane(cx);
                }),
            ),
        );

        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                    this.term_menu = None;
                    cx.notify();
                }))
                .on_mouse_down(MouseButton::Right, cx.listener(|this, _, _, cx| {
                    this.term_menu = None;
                    cx.notify();
                }))
                .child(menu)
                .into_any_element(),
        )
    }

    /// 把窗口坐标换算成某格的（列, 行）。
    fn pane_col_row(&self, id: PaneId, pos: Point<Pixels>) -> Option<(usize, i32)> {
        let (cell, origin) = self.pane_cell_origin(id)?;
        let col = ((pos.x - origin.x).as_f32() / cell.width.as_f32())
            .floor()
            .max(0.0) as usize;
        let row = ((pos.y - origin.y).as_f32() / cell.height.as_f32())
            .floor()
            .max(0.0) as i32;
        Some((col, row))
    }

    /// 终端区鼠标按下（列/行换算 + 鼠标报告 + 右键菜单）。
    ///
    /// 目标窗格由命中判定决定（落在分割条上会被判为「无命中」而整体忽略），
    /// 按下时把该格设为**活动窗格**：键盘输入与随后弹出的右键菜单都作用于它。
    fn term_mouse(
        &mut self,
        button: MouseButton,
        pos: Point<Pixels>,
        pressed: bool,
        mods: Modifiers,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.mouse_target(pos, false) else {
            return;
        };
        let Some((col, row)) = self.pane_col_row(id, pos) else {
            return;
        };

        let mut consumed = false;
        {
            let idx = self.active.min(self.tabs.len().saturating_sub(1));
            if let Some(tab) = self.tabs.get_mut(idx).and_then(|t| t.terminal_tab_mut()) {
                tab.set_active(id);
                if let Some(pane) = tab.pane_mut(id) {
                    consumed = match button {
                        MouseButton::Left => pane.mouse_down(col, row, &mods),
                        MouseButton::Middle => pane.mouse_middle(col, row, &mods, pressed),
                        MouseButton::Right => pane.mouse_right(col, row, &mods, pressed),
                        _ => false,
                    };
                }
            }
        }
        // 锁定本次拖拽的窗格：拖出本格（甚至拖到分割条上）也继续投给它
        self.mouse_pane = Some(id);
        // 非鼠标报告模式：右键弹出「复制/粘贴」菜单（终端里右键菜单比
        // 直接粘贴更安全，也不会把剪贴板内容意外打进远端 shell）。
        if !consumed && button == MouseButton::Right && pressed {
            self.term_menu = Some(pos);
            self.focus_term = true;
        }
        cx.notify();
    }

    fn term_mouse_up(
        &mut self,
        button: MouseButton,
        pos: Point<Pixels>,
        mods: Modifiers,
        cx: &mut Context<Self>,
    ) {
        let target = self.mouse_pane.or_else(|| self.pane_at(pos));
        let Some(id) = target else {
            self.mouse_pane = None;
            return;
        };
        let Some((col, row)) = self.pane_col_row(id, pos) else {
            self.mouse_pane = None;
            return;
        };
        {
            let idx = self.active.min(self.tabs.len().saturating_sub(1));
            if let Some(pane) = self
                .tabs
                .get_mut(idx)
                .and_then(|t| t.terminal_tab_mut())
                .and_then(|tab| tab.pane_mut(id))
            {
                match button {
                    MouseButton::Left => {
                        pane.mouse_up(col, row, &mods);
                    }
                    MouseButton::Middle => {
                        pane.mouse_middle(col, row, &mods, false);
                    }
                    MouseButton::Right => {
                        pane.mouse_right(col, row, &mods, false);
                    }
                    _ => {}
                }
            }
        }
        self.mouse_pane = None;
        cx.notify();
    }

    fn term_mouse_move(
        &mut self,
        pos: Point<Pixels>,
        pressed: Option<MouseButton>,
        cx: &mut Context<Self>,
    ) {
        // 拖动中（有按键按下）沿用按下时锁定的窗格；纯悬停按落点判定
        let Some(id) = self.mouse_target(pos, pressed.is_some()) else {
            return;
        };
        let Some((col, row)) = self.pane_col_row(id, pos) else {
            return;
        };
        let mut consumed = false;
        {
            let idx = self.active.min(self.tabs.len().saturating_sub(1));
            if let Some(pane) = self
                .tabs
                .get_mut(idx)
                .and_then(|t| t.terminal_tab_mut())
                .and_then(|tab| tab.pane_mut(id))
            {
                consumed = match pressed {
                    Some(_) => pane.mouse_drag(col, row, &Modifiers::default()),
                    None => pane.mouse_motion(col, row),
                };
            }
        }
        if consumed {
            cx.notify();
        }
    }

    /// 滚轮回看：落在哪一格就滚哪一格（落点不确定时退回活动窗格）。
    fn term_scroll(&mut self, delta: ScrollDelta, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(id) = self.pane_at(pos).or_else(|| self.active_pane_id()) else {
            return;
        };
        let idx = self.active.min(self.tabs.len().saturating_sub(1));
        let mut scrolled = false;
        {
            if let Some(pane) = self
                .tabs
                .get_mut(idx)
                .and_then(|t| t.terminal_tab_mut())
                .and_then(|tab| tab.pane_mut(id))
            {
                pane.scroll(delta);
                scrolled = true;
            }
        }
        if scrolled {
            cx.notify();
        }
    }

    // ------------------------------------------------------------------
    // 状态栏 / AI 面板
    // ------------------------------------------------------------------

    fn status_bar_ui(&self, cx: &App) -> AnyElement {
        let lang = self.i18n.lang;
        let theme = Theme::global(cx).clone();
        let dim = theme.colors.muted_foreground;
        let fg = theme.colors.foreground;

        let current = if self.tabs.is_empty() {
            tr(lang, "status.none")
        } else {
            let t = &self.tabs[self.active.min(self.tabs.len() - 1)];
            format!(
                "{} [{}] {}",
                t.tab_type().icon(),
                t.tab_type().label(),
                t.title()
            )
        };

        let (status_text, status_color) = if self.tabs.is_empty() {
            (format!("● {}", tr(lang, "status.none")), dim)
        } else {
            match &self.tabs[self.active.min(self.tabs.len() - 1)] {
                TabContent::Terminal(t) => {
                    if t.active_dead() {
                        (format!("● {}", tr(lang, "status.disconnected")), theme.colors.danger)
                    } else {
                        (format!("● {}", tr(lang, "status.connected")), theme.colors.success)
                    }
                }
                TabContent::Connecting { .. } => (
                    format!("◐ {}", tr(lang, "status.connecting")),
                    theme.colors.warning,
                ),
                TabContent::Failed(_) => (format!("● {}", tr(lang, "status.failed")), theme.colors.danger),
                TabContent::Browser(_) => (format!("● {}", tr(lang, "status.browser")), theme.colors.info),
                TabContent::RemoteDesktop(t) => {
                    if t.is_active() {
                        (format!("● {}", tr(lang, "status.remote_active")), theme.colors.success)
                    } else if t.is_connecting() {
                        (format!("◐ {}", tr(lang, "status.remote_connecting")), theme.colors.warning)
                    } else if t.error().is_some() {
                        (format!("● {}", tr(lang, "status.failed")), theme.colors.danger)
                    } else {
                        (format!("○ {}", tr(lang, "status.remote_idle")), dim)
                    }
                }
                TabContent::LocalFiles(_) => (format!("● {}", tr(lang, "status.files")), theme.colors.success),
                TabContent::Macro => (format!("● {}", tr(lang, "status.macros")), theme.colors.info),
                TabContent::PortScan => (
                    if self.port_scan.is_running() {
                        format!("◐ {}", tr(lang, "status.portscan_running"))
                    } else {
                        format!("● {}", tr(lang, "status.portscan"))
                    },
                    if self.port_scan.is_running() {
                        theme.colors.warning
                    } else {
                        theme.colors.info
                    },
                ),
            }
        };

        // ---- 左区：当前标签 + 连接状态 ----
        let mut left = div().h_flex().items_center().gap_2().min_w_0().flex_1();
        left = left
            .child(
                div()
                    .text_color(dim)
                    .min_w_0()
                    .text_ellipsis()
                    .child(format!("{}: {current}", tr(lang, "status.opened"))),
            )
            .child(
                div()
                    .text_color(status_color)
                    .flex_none()
                    .child(status_text),
            );

        // ---- 右区：尺寸 / 光标 / 标签计数 / 会话数 ----
        let mut right = div()
            .h_flex()
            .items_center()
            .gap_3()
            .flex_none()
            .text_size(px(10.0));

        if let Some(tab) = self
            .tabs
            .get(self.active.min(self.tabs.len().saturating_sub(1)))
            .and_then(|t| t.terminal_tab())
        {
            // 尺寸 / 光标行列为**活动窗格**的数据；分屏时额外标一个格数
            let active_id = tab.active_id();
            if let Some(pane) = tab.active_pane() {
                right = right.child(
                    div()
                        .text_color(dim)
                        .text_size(px(10.0))
                        .child(format!("{}×{}", pane.dims.cols, pane.dims.rows)),
                );
            }
            if tab.is_split() {
                right = right.child(
                    div()
                        .text_color(dim)
                        .text_size(px(10.0))
                        .child(format!("{} 格", tab.layout.len())),
                );
            }
            if let Some(snap) = self.pane_snaps.get(&active_id) {
                if let Some(cur) = &snap.cursor {
                    let r = (cur.bounds.origin.y.as_f32() / snap.cell.height.as_f32()).floor()
                        as i32
                        + 1;
                    let c = (cur.bounds.origin.x.as_f32() / snap.cell.width.as_f32()).floor()
                        as i32
                        + 1;
                    right = right.child(
                        div()
                            .text_color(dim)
                            .text_size(px(10.0))
                            .child(format!("行:{r}, 列:{c}")),
                    );
                }
                // 终端模式提示：备用屏（vim/top 之类）与鼠标跟踪
                if snap.alt_screen {
                    right = right.child(
                        div()
                            .text_color(theme.colors.warning)
                            .text_size(px(10.0))
                            .child("备用屏"),
                    );
                }
                if snap.mouse_mode {
                    right = right.child(
                        div()
                            .text_color(theme.colors.info)
                            .text_size(px(10.0))
                            .child("鼠标跟踪"),
                    );
                }
            }
        } else if !self.tabs.is_empty() {
            right = right.child(
                div()
                    .text_color(dim)
                    .text_size(px(10.0))
                    .child(format!("标签: {}/{}", self.active + 1, self.tabs.len())),
            );
        }

        right = right.child(
            div()
                .text_color(dim)
                .text_size(px(10.0))
                .child(format!("会话 {}", self.sessions.sessions.len())),
        );

        // 同步输入是「进行中」的状态，放在右区并高亮
        if self.sync_input {
            right = right.child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .rounded(px(4.0))
                    .bg(theme.colors.warning)
                    .text_color(theme.colors.background)
                    .text_size(px(10.0))
                    .child("同步输入"),
            );
        }

        // ---- 中区：临时提示 ----
        let middle = match &self.toast {
            Some(t) => div()
                .h_flex()
                .items_center()
                .gap_1()
                .px_2()
                .max_w(px(420.0))
                .min_w_0()
                .text_color(theme.colors.warning)
                .text_size(px(11.0))
                .child(ic(IconName::Info).xsmall().text_color(theme.colors.warning))
                // 文本单独包一层并加 min_w_0 + text_ellipsis，否则长提示会
                // 溢出中区把右区（尺寸 / 光标 / 会话数）挤出状态栏。
                .child(
                    div()
                        .min_w_0()
                        .text_ellipsis()
                        .child(t.clone()),
                ),
            None => div(),
        };

        div()
            .h_flex()
            .items_center()
            .flex_none()
            .h(px(STATUS_H))
            .border_t_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.muted)
            .text_color(fg)
            .text_size(px(11.0))
            .px_2()
            .gap_2()
            .overflow_hidden()
            .child(left)
            .child(middle)
            .child(right)
            .into_any_element()
    }

    fn ai_panel_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.ai_assistant.open {
            return None;
        }
        let ai = self.config.ai.clone();
        self.ai_assistant.render(&ai, window, cx)
    }

    // ------------------------------------------------------------------
    // 对话框（覆盖层）
    // ------------------------------------------------------------------

    /// 绝对定位覆盖层 + 居中卡片。
    fn dialog_overlay(cx: &App, title: &str, width: f32, content: impl IntoElement) -> AnyElement {
        let theme = Theme::global(cx).clone();
        div()
            .id(SharedString::from(format!("dialog-backdrop-{title}")))
            // 模态遮罩必须拦住鼠标：否则点击会漏到下层的终端，
            // 焦点被终端抢走 → 对话框里的输入框点不进去、也打不了字。
            .occlude()
            .absolute()
            .inset_0()
            .bg(hsla(0.0, 0.0, 0.0, 0.35))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id(SharedString::from(format!("dialog-card-{title}")))
                    .occlude()
                    .v_flex()
                    .w(px(width))
                    .max_h_full()
                    .bg(theme.colors.background)
                    .border_1()
                    .border_color(theme.colors.border)
                    .rounded(px(8.0))
                    .p_4()
                    .gap_3()
                    .child(
                        div()
                            .text_size(px(15.0))
                            .text_color(theme.colors.foreground)
                            .child(title.to_string()),
                    )
                    .child(content),
            )
            .into_any_element()
    }

    fn ssh_dialog_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.ssh_form_open {
            return None;
        }
        if self.ssh_form.is_none() {
            let host = Self::mk_input("主机", window, cx);
            let port = Self::mk_input("端口（默认 22）", window, cx);
            let user = Self::mk_input("用户名", window, cx);
            let password = Self::mk_password("密码", window, cx);
            let key_path = Self::mk_input("私钥路径 ~/.ssh/id_rsa", window, cx);
            let key_passphrase = Self::mk_password("私钥口令（可空）", window, cx);
            // 预填（端口扫描结果双击带过来的主机 / 端口）
            if let Some((h, p)) = self.ssh_form_prefill.take() {
                host.update(cx, |s, cx| s.set_value(h, window, cx));
                port.update(cx, |s, cx| s.set_value(p.to_string(), window, cx));
                user.update(cx, |s, cx| s.set_value("root".to_string(), window, cx));
            }
            self.ssh_form = Some(SshFormInputs {
                host,
                port,
                user,
                password,
                key_path,
                key_passphrase,
                auth_key: false,
            });
            self.focus_dialog = true;
        }
        if self.focus_dialog {
            self.focus_dialog = false;
            if let Some(form) = &self.ssh_form {
                let fh = form.host.read(cx).focus_handle(cx).clone();
                window.focus(&fh, cx);
            }
        }
        let form = self.ssh_form.clone()?; // 临时克隆实体（InputState 是实体，克隆共享状态）

        let error_line = self.ssh_form_error.clone().map(|e| {
            div()
                .text_size(px(12.0))
                .text_color(Theme::global(cx).colors.danger)
                .child(e)
        });

        let is_key = form.auth_key;
        let content = div()
            .v_flex()
            .gap_2()
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(52.0)).text_size(px(12.0)).child("主机"))
                    .child(Input::new(&form.host).flex_1()),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(52.0)).text_size(px(12.0)).child("端口"))
                    .child(Input::new(&form.port).w(px(120.0))),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(52.0)).text_size(px(12.0)).child("用户"))
                    .child(Input::new(&form.user).flex_1()),
            )
            .child(
                Checkbox::new("ssh-auth-key")
                    .label("密钥认证")
                    .checked(form.auth_key)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        if let Some(f) = this.ssh_form.as_mut() {
                            f.auth_key = *checked;
                        }
                        cx.notify();
                    })),
            )
            .when(is_key, |d| {
                d.child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_2()
                        .child(div().w(px(52.0)).text_size(px(12.0)).child("私钥"))
                        .child(Input::new(&form.key_path).flex_1()),
                )
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_2()
                        .child(div().w(px(52.0)).text_size(px(12.0)).child("口令"))
                        .child(password_field(&form.key_passphrase).flex_1()),
                )
            })
            .when(!is_key, |d| {
                d.child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_2()
                        .child(div().w(px(52.0)).text_size(px(12.0)).child("密码"))
                        .child(password_field(&form.password).flex_1()),
                )
            })
            .children(error_line)
            .child(
                div()
                    .h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("ssh-cancel")
                            .label("取消")
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| {
                                this.ssh_form_open = false;
                                this.ssh_form = None;
                            })),
                    )
                    .child(
                        Button::new("ssh-ok")
                            .label("连接")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.ssh_dialog_submit(cx);
                            })),
                    ),
            );
        Some(Self::dialog_overlay(cx, "SSH 连接", 420.0, content))
    }

    fn ssh_dialog_submit(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.ssh_form.clone() else {
            return;
        };
        let host = Self::input_value(&Some(form.host.clone()), cx);
        let port: u16 = Self::input_value(&Some(form.port.clone()), cx)
            .trim()
            .parse()
            .unwrap_or(22);
        let user = Self::input_value(&Some(form.user.clone()), cx);
        let auth = if form.auth_key {
            crate::backends::ssh::SshAuth::Key {
                path: Self::input_value(&Some(form.key_path.clone()), cx),
                passphrase: Self::input_value(&Some(form.key_passphrase.clone()), cx),
            }
        } else {
            crate::backends::ssh::SshAuth::Password(Self::input_value(
                &Some(form.password.clone()),
                cx,
            ))
        };
        if host.trim().is_empty() || user.trim().is_empty() {
            self.ssh_form_error = Some("主机与用户名不能为空".to_string());
            cx.notify();
            return;
        }
        self.ssh_form_open = false;
        self.ssh_form = None;
        self.submit_ssh(&host, port, &user, auth, None);
    }

    fn add_session_dialog_ui(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let form = self.add_form.clone()?;

        // 打开时把键盘焦点交给第一个输入框。
        // 原来只置了 `focus_dialog` 却没人消费，焦点还停在侧栏按钮/终端上，
        // 表现就是「对话框能看见、但打不进任何字」。
        if std::mem::take(&mut self.focus_dialog) {
            let fh = form.name.read(cx).focus_handle(cx).clone();
            window.focus(&fh, cx);
        }
        let theme = Theme::global(cx).clone();

        // 类型选择按钮行
        let kinds = [
            (SessionKind::Ssh, "SSH"),
            (SessionKind::Vnc, "VNC"),
            (SessionKind::Rdp, "RDP"),
            (SessionKind::Spice, "SPICE"),
        ];
        let mut kind_row = div().h_flex().items_center().gap_2().child(
            div().w(px(52.0)).text_size(px(12.0)).child("类型"),
        );
        for (kind, label) in kinds {
            let editing_kind = form.kind == kind;
            kind_row = kind_row.child(
                Button::new(SharedString::from(format!("kind-{}", kind.label())))
                    .label(label)
                    .compact()
                    .selected(editing_kind)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(f) = this.add_form.as_mut() {
                            f.kind = kind;
                        }
                        cx.notify();
                    })),
            );
        }

        let field = |label: &str, input: &Entity<InputState>| {
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .child(div().w(px(52.0)).text_size(px(12.0)).child(label.to_string()))
                .child(Input::new(input).flex_1())
        };
        // 密码字段：带「查看密码」眼睛按钮
        let pw_field = |label: &str, input: &Entity<InputState>| {
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .child(div().w(px(52.0)).text_size(px(12.0)).child(label.to_string()))
                .child(password_field(input).flex_1())
        };

        let content = div()
            .v_flex()
            .gap_2()
            .max_h(px(520.0))
            .overflow_y_scrollbar()
            .child(kind_row)
            .child(field("名称", &form.name))
            .child(field("主机", &form.host))
            .child(field("端口", &form.port))
            .child(field("用户", &form.user))
            .child(pw_field("密码", &form.password))
            .child(field("私钥", &form.key_path))
            .child(field("域", &form.domain))
            .child(
                Checkbox::new("add-auth-key")
                    .label("密钥认证")
                    .checked(form.auth_key)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        if let Some(f) = this.add_form.as_mut() {
                            f.auth_key = *checked;
                        }
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("add-cancel")
                            .label("取消")
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| this.add_form = None)),
                    )
                    .child(
                        Button::new("add-save")
                            .label("保存")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.add_session_submit(cx);
                            })),
                    ),
            );
        let _ = theme;
        // Esc 关闭表单（焦点在表单内时按键才会冒泡到这里）
        let content = content.on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
            if ev.keystroke.key == "escape" {
                this.add_form = None;
                this.focus_term = true;
                cx.stop_propagation();
                cx.notify();
            }
        }));
        let title = if form.editing.is_some() {
            "编辑会话"
        } else {
            "新建会话"
        };
        Some(Self::dialog_overlay(cx, title, 420.0, content))
    }

    fn add_session_submit(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.add_form.clone() else {
            return;
        };
        let name = Self::input_value(&Some(form.name.clone()), cx);
        let host = Self::input_value(&Some(form.host.clone()), cx);
        let port: u16 = Self::input_value(&Some(form.port.clone()), cx)
            .trim()
            .parse()
            .unwrap_or(form.kind.default_port());
        let user = Self::input_value(&Some(form.user.clone()), cx);
        let password = Self::input_value(&Some(form.password.clone()), cx);
        let key_path = Self::input_value(&Some(form.key_path.clone()), cx);
        let domain = Self::input_value(&Some(form.domain.clone()), cx);

        if host.trim().is_empty() {
            self.set_toast("主机不能为空");
            return;
        }
        let entry = crate::sessions::SessionEntry {
            id: form.editing.unwrap_or_else(crate::sessions::SessionEntry::new_id),
            name,
            kind: form.kind,
            host,
            port,
            user,
            password,
            auth_method: if form.auth_key { "key" } else { "password" }.to_string(),
            key_path,
            domain,
            group: String::new(),
        };
        self.sessions.update(entry);
        if let Err(e) = self.sessions.save() {
            self.set_toast(e);
        }
        self.add_form = None;
        self.set_toast("会话已保存");
    }

    fn telnet_dialog_ui(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.telnet_open {
            return None;
        }
        if self.telnet_host.is_none() {
            self.telnet_host = Some(Self::mk_input("主机", window, cx));
            self.telnet_port = Some(Self::mk_input("23", window, cx));
        }
        if self.focus_dialog {
            self.focus_dialog = false;
            if let Some(h) = &self.telnet_host {
                let fh = h.read(cx).focus_handle(cx).clone();
                window.focus(&fh, cx);
            }
        }
        let host = self.telnet_host.clone().unwrap();
        let port = self.telnet_port.clone().unwrap();
        let content = div()
            .v_flex()
            .gap_2()
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(40.0)).text_size(px(12.0)).child("主机"))
                    .child(Input::new(&host).flex_1()),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(40.0)).text_size(px(12.0)).child("端口"))
                    .child(Input::new(&port).w(px(100.0))),
            )
            .child(
                div()
                    .h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("telnet-cancel")
                            .label("取消")
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| this.telnet_open = false)),
                    )
                    .child(
                        Button::new("telnet-ok")
                            .label("连接")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                let host = Self::input_value(&this.telnet_host, cx);
                                let port: u16 = Self::input_value(&this.telnet_port, cx)
                                    .trim()
                                    .parse()
                                    .unwrap_or(23);
                                this.telnet_open = false;
                                this.open_telnet(&host, port);
                            })),
                    ),
            );
        Some(Self::dialog_overlay(cx, "Telnet 连接", 380.0, content))
    }

    fn ping_dialog_ui(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.ping_open {
            return None;
        }
        if self.ping_host.is_none() {
            self.ping_host = Some(Self::mk_input("目标主机 / IP", window, cx));
        }
        if self.focus_dialog {
            self.focus_dialog = false;
            if let Some(h) = &self.ping_host {
                let fh = h.read(cx).focus_handle(cx).clone();
                window.focus(&fh, cx);
            }
        }
        let host = self.ping_host.clone().unwrap();
        let content = div()
            .v_flex()
            .gap_2()
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(40.0)).text_size(px(12.0)).child("目标"))
                    .child(Input::new(&host).flex_1()),
            )
            .child(
                div()
                    .h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("ping-cancel")
                            .label("取消")
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| this.ping_open = false)),
                    )
                    .child(
                        Button::new("ping-ok")
                            .label("开始")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                let host = Self::input_value(&this.ping_host, cx);
                                this.ping_open = false;
                                this.open_ping(&host);
                            })),
                    ),
            );
        Some(Self::dialog_overlay(cx, "Ping", 360.0, content))
    }

    fn batch_dialog_ui(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.batch_open {
            return None;
        }
        if self.batch_cmd.is_none() {
            self.batch_cmd = Some(Self::mk_input("要批量执行的命令", window, cx));
        }
        if self.focus_dialog {
            self.focus_dialog = false;
            if let Some(h) = &self.batch_cmd {
                let fh = h.read(cx).focus_handle(cx).clone();
                window.focus(&fh, cx);
            }
        }
        let input = self.batch_cmd.clone().unwrap();
        let n_alive = self
            .tabs
            .iter()
            .filter(|t| {
                t.terminal()
                    .map(|p| !p.dead)
                    .unwrap_or(false)
            })
            .count();
        let content = div()
            .v_flex()
            .gap_2()
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(Theme::global(cx).colors.muted_foreground)
                    .child(format!("将发送到 {n_alive} 个存活终端")),
            )
            .child(Input::new(&input).flex_1())
            .child(
                div()
                    .h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("batch-cancel")
                            .label("取消")
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| this.batch_open = false)),
                    )
                    .child(
                        Button::new("batch-ok")
                            .label("发送")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                let cmd = Self::input_value(&this.batch_cmd, cx);
                                this.send_batch_command(cmd);
                            })),
                    ),
            );
        Some(Self::dialog_overlay(cx, "批量执行命令", 420.0, content))
    }

    fn tunnel_dialog_ui(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.tunnel_open {
            return None;
        }
        if self.tunnel_local.is_none() {
            self.tunnel_local = Some(Self::mk_input("本地端口", window, cx));
            self.tunnel_host = Some(Self::mk_input("目标主机", window, cx));
            self.tunnel_port = Some(Self::mk_input("目标端口", window, cx));
        }
        // 与其它对话框一致：打开时把焦点交给第一个输入框
        if std::mem::take(&mut self.focus_dialog) {
            if let Some(local) = &self.tunnel_local {
                let fh = local.read(cx).focus_handle(cx).clone();
                window.focus(&fh, cx);
            }
        }
        let local = self.tunnel_local.clone().unwrap();
        let host = self.tunnel_host.clone().unwrap();
        let port = self.tunnel_port.clone().unwrap();
        let theme = Theme::global(cx).clone();

        let mut rows = div().v_flex().gap_1().max_h(px(220.0)).overflow_y_scrollbar();
        for (i, t) in self.tunnels.iter().enumerate() {
            let idx = i;
            let (status, color) = if let Some(e) = t.error() {
                (format!("错误: {e}"), theme.colors.danger)
            } else if t.running() {
                ("运行中".to_string(), theme.colors.success)
            } else {
                ("已停止".to_string(), theme.colors.muted_foreground)
            };
            rows = rows.child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .text_size(px(11.0))
                    .child(
                        div()
                            .w(px(110.0))
                            .child(format!("127.0.0.1:{}", t.local_port)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(format!("{}:{}", t.target_host, t.target_port)),
                    )
                    .child(div().child(format!("连接 {}", t.connections()))),
            );
            rows = rows.child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .text_size(px(11.0))
                    .child(
                        div()
                            .flex_1()
                            .text_color(color)
                            .min_w_0()
                            .text_ellipsis()
                            .child(status),
                    )
                    .child(
                        Button::new(SharedString::from(format!("tunnel-stop-{i}")))
                            .label("停止")
                            .compact()
                            .on_click(cx.listener(move |this, _, _, _| {
                                let id = this.tunnels.get(idx).map(|t| t.id);
                                if let Some(id) = id {
                                    if let Some(t) = this.tunnels.iter().find(|t| t.id == id) {
                                        t.stop();
                                    }
                                    this.tunnels.retain(|t| t.id != id);
                                    this.set_toast("隧道已停止");
                                }
                            })),
                    ),
            );
        }

        let content = div()
            .v_flex()
            .gap_2()
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.colors.muted_foreground)
                    .child("本地端口 → 目标主机端口（直连 TCP 转发）"),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(Input::new(&local).w(px(90.0)))
                    .child(div().child("→"))
                    .child(Input::new(&host).flex_1())
                    .child(div().child(":"))
                    .child(Input::new(&port).w(px(80.0)))
                    .child(
                        Button::new("tunnel-start")
                            .label("启动")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                let lp: u16 =
                                    Self::input_value(&this.tunnel_local, cx).trim().parse().unwrap_or(0);
                                let tp: u16 =
                                    Self::input_value(&this.tunnel_port, cx).trim().parse().unwrap_or(0);
                                let host = Self::input_value(&this.tunnel_host, cx)
                                    .trim()
                                    .to_string();
                                if host.is_empty() || tp == 0 {
                                    this.set_toast("请填写目标主机与端口");
                                    return;
                                }
                                let id = this.tunnel_next_id;
                                match crate::tunnel::start(id, lp, &host, tp) {
                                    Ok(h) => {
                                        this.tunnel_next_id += 1;
                                        let lp_actual = h.local_port;
                                        this.tunnels.push(h);
                                        this.set_toast(format!(
                                            "隧道已启动: 127.0.0.1:{lp_actual} → {host}:{tp}"
                                        ));
                                    }
                                    Err(e) => this.set_toast(format!("隧道启动失败: {e}")),
                                }
                            })),
                    ),
            )
            .child(
                div()
                    .border_t_1()
                    .border_color(theme.colors.border)
                    .pt_2(),
            )
            .when(self.tunnels.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.colors.muted_foreground)
                        .child("暂无隧道"),
                )
            })
            .child(rows)
            .child(
                div()
                    .h_flex()
                    .justify_end()
                    .child(
                        Button::new("tunnel-close")
                            .label("关闭")
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| this.tunnel_open = false)),
                    ),
            );
        Some(Self::dialog_overlay(cx, "端口隧道", 520.0, content))
    }

    fn about_dialog_ui(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let content = div()
            .v_flex()
            .gap_2()
            .child(div().text_size(px(12.0)).child("RustTerm GPUI — 终端 / SSH / 远程桌面一体化客户端"))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(Theme::global(cx).colors.muted_foreground)
                    .child("Ctrl+N 本地终端 · Ctrl+T 浏览器 · Ctrl+W 关闭标签 · F11 全屏 · Ctrl+1..9 切换标签"),
            )
            .child(
                div()
                    .h_flex()
                    .justify_end()
                    .child(
                        Button::new("about-ok")
                            .label("关闭")
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| this.about_open = false)),
                    ),
            );
        Self::dialog_overlay(cx, "关于 RustTerm", 420.0, content)
    }
}


// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

// 终端 canvas 四周留出的内边距（绘制与鼠标映射共用）
const TERM_PAD: f32 = 6.0;

/// 分屏命中判定时，公共边界向两侧各让出的像素。
///
/// 分割条的拖拽带是 `HANDLE_SIZE(1px) + HANDLE_PADDING(4px) × 2`，压在相邻两格
/// 的公共边界上；事件会从它冒泡到终端容器。留出这么宽的一圈不看，拖分割条就
/// 不会误在终端里起选区。
const SPLIT_GRIP: f32 = 5.0;

const MENUBAR_H: f32 = 30.0;
const TOOLBAR_H: f32 = 50.0;
const QUICK_H: f32 = 36.0;
const TABS_H: f32 = 34.0;
const FIND_BAR_H: f32 = 34.0;
const STATUS_H: f32 = 26.0;
const SIDEBAR_W: f32 = 220.0;
const SFTP_W: f32 = 300.0;
const AI_W: f32 = 360.0;

impl Render for RustTermApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // ---- 一次性初始化 ----
        if !self.initialized {
            self.initialized = true;
            self.term_focus = Some(cx.focus_handle());
            // 根焦点：空状态（没有任何标签页）时窗口没有焦点节点，
            // GPUI 的按键分发路径为空 → 全局快捷键全部失效。
            let root_focus = cx.focus_handle();
            self.root_focus = Some(root_focus.clone());
            window.focus(&root_focus, cx);
            Self::bind_keys(cx);
            let mode = if self.config.theme == "light" {
                ThemeMode::Light
            } else {
                ThemeMode::Dark
            };
            Theme::change(mode, Some(window), cx);
            #[cfg(windows)]
            if let Some(hwnd) = crate::win32::raw_hwnd_from_window(window) {
                crate::win32::register_main_hwnd(hwnd);
            }
        }

        self.poll_connecting();
        self.pump_sync_input();

        // 关闭查找栏后把焦点交回终端（否则按键没人接，终端像「死了」一样）
        if std::mem::take(&mut self.focus_term) {
            // 只要活动标签是终端就交还焦点：分屏窗格可能还在连接中
            // （那时 terminal() 返回 None，但不该因此丢掉键盘焦点）
            let has_term = self
                .tabs
                .get(self.active.min(self.tabs.len().saturating_sub(1)))
                .and_then(|t| t.terminal_tab())
                .is_some();
            if has_term {
                if let Some(f) = self.term_focus.clone() {
                    window.focus(&f, cx);
                }
            }
        }

        // 切到 SSH 标签自动弹出 SFTP，切走自动收起
        {
            let ssh_key = self
                .tabs
                .get(self.active.min(self.tabs.len().saturating_sub(1)))
                .and_then(|t| t.terminal())
                .and_then(|p| p.ssh_handle())
                .map(|h| Arc::as_ptr(&h) as *const u8 as usize);
            if ssh_key != self.last_ssh_key {
                self.last_ssh_key = ssh_key;
                if ssh_key.is_some() {
                    self.sftp.open = true;
                    self.sftp.bound_initialized = false;
                } else {
                    self.sftp.open = false;
                }
            }
        }

        // SFTP 绑定当前活动 SSH 会话
        {
            let (label, ssh) = match self
                .tabs
                .get(self.active.min(self.tabs.len().saturating_sub(1)))
            {
                Some(TabContent::Terminal(tab)) => {
                    // SFTP 跟随**活动窗格**：分屏时哪一格聚焦就绑哪一格的会话
                    let pane = tab.active_pane();
                    (
                        pane.map(|p| p.title.clone()).unwrap_or_default(),
                        pane.and_then(|p| p.ssh_handle()),
                    )
                }
                _ => (String::new(), None),
            };
            self.sftp
                .bind(&self.runtime_handle(), &label, ssh);
        }

        // 菜单栏里点「新建会话」：真正的表单需要 &mut Window，菜单回调拿不到，
        // 因此延后到这一帧（render 里有 window）再执行。
        if std::mem::take(&mut self.pending_new_session) {
            self.start_add_session(window, cx);
        }

        // 端口扫描：消费 worker 回传的结果。
        // 扫描在后台线程跑，结果要靠这一行拉回 UI 侧 —— 没有它，
        // 面板里的进度和命中列表会永远停在 0。
        {
            let was_running = self.port_scan.is_running();
            let changed = self.port_scan.poll();
            if was_running && !self.port_scan.is_running() {
                // 扫描刚结束：提示一次
                let n = self.port_scan.hits.len();
                self.set_toast(format!("扫描完成，发现 {n} 个开放端口"));
            }
            if changed {
                cx.notify();
            }
        }

        // 终端数据泵 + 快照（活动标签的**每一个**窗格都要走一遍）
        {
            let focused = self
                .term_focus
                .as_ref()
                .map(|f| f.is_focused(window))
                .unwrap_or(false);
            let mut dirty = false;
            let idx = self.active.min(self.tabs.len().saturating_sub(1));

            // 首帧还没有测量值时的兜底：视口减去各固定栏（分屏后只当临时值，
            // 下一帧就会被各格自己的 bounds 取代）
            let vp = window.viewport_size();
            let mut est_w = vp.width.as_f32() - SIDEBAR_W;
            if self.sftp.open {
                est_w -= SFTP_W;
            }
            if self.ai_assistant.open {
                est_w -= AI_W;
            }
            let est_h = vp.height.as_f32()
                - MENUBAR_H
                - TOOLBAR_H
                - QUICK_H
                - TABS_H
                - if self.find_open { FIND_BAR_H } else { 0.0 }
                - if self.config.show_status_bar { STATUS_H } else { 0.0 };
            let estimate = size(
                px((est_w - TERM_PAD * 2.0).max(80.0)),
                px((est_h - TERM_PAD * 2.0).max(80.0)),
            );

            let font_size = self.config.font_size;
            // 查找栏开着：切换窗格后要把高亮补到新格上（查找状态是每格一份的）
            let needle = if self.find_open {
                Self::input_value(&self.find_input, cx)
            } else {
                String::new()
            };
            if let Some(tab) = self.tabs.get_mut(idx).and_then(|t| t.terminal_tab_mut()) {
                let active_id = tab.active_id();
                for slot in tab.panes.iter_mut() {
                    let PaneState::Live(pane) = &mut slot.state else {
                        continue;
                    };
                    let id = slot.id;
                    // 分屏的每个窗格各自一条连接：标题 / OSC52 / 数据都要各自吸
                    let (_title_changed, clip) = pane.drain_pane_events();
                    if let Some(text) = clip {
                        term::copy_to_clipboard(cx, text);
                    }
                    dirty |= pane.pump();

                    // 本格可用空间：优先用上一帧量到的 bounds（分屏后各格不同），
                    // 首帧才退回视口估算
                    let avail = self
                        .pane_areas
                        .get(&id)
                        .map(|rc| rc.get())
                        .filter(|b| b.size.width.as_f32() > 1.0 && b.size.height.as_f32() > 1.0)
                        .map(|b| {
                            size(
                                px((b.size.width.as_f32() - TERM_PAD * 2.0).max(80.0)),
                                px((b.size.height.as_f32() - TERM_PAD * 2.0).max(80.0)),
                            )
                        })
                        .unwrap_or(estimate);
                    dirty |= pane.fit_to(window, font_size, avail);

                    // 焦点状态写回窗格：只有「终端区有焦点 + 本格是活动格」才画实心光标
                    pane.has_focus = focused && id == active_id;
                    if !needle.is_empty() && pane.search != needle {
                        pane.set_search(&needle);
                    }
                    self.pane_snaps
                        .insert(id, Arc::new(pane.snapshot()));
                    // 保证本格有测量槽位（canvas prepaint 会把 bounds 写进来）
                    self.pane_areas
                        .entry(id)
                        .or_insert_with(|| Rc::new(Cell::new(Bounds::default())));
                }
            }

            // 清掉已经消失的窗格（关分屏 / 关标签）留下的测量与快照
            let live: Vec<PaneId> = self
                .tabs
                .iter()
                .filter_map(|t| t.terminal_tab())
                .flat_map(|tab| tab.leaf_ids())
                .collect();
            self.pane_snaps.retain(|id, _| live.contains(id));
            self.pane_areas.retain(|id, _| live.contains(id));
            if self.mouse_pane.map(|id| !live.contains(&id)).unwrap_or(false) {
                self.mouse_pane = None;
            }

            if dirty {
                cx.notify();
            }
        }

        // AI 面板上下文提示 + 待发送
        self.ai_assistant.attach_hint = self.active_terminal_hint();
        if self.ai_assistant.take_pending_send() {
            let context = self.current_terminal_context();
            let ai = self.config.ai.clone();
            self.ai_assistant.send(&ai, context, window, cx);
        }

        // AI 面板的「发送到终端」请求
        if let Some((cmd, dangerous)) = self.ai_assistant.command_request.take() {
            if self.config.ai.read_only {
                self.set_toast("只读模式：写命令已被禁用");
            } else if dangerous {
                self.set_toast(format!("⚠ 危险命令已拦截: {cmd}"));
            } else if let Some(pane) = self.tabs.get_mut(self.active).and_then(|t| t.terminal_mut()) {
                if !pane.dead {
                    pane.write_input(format!("{cmd}\r").into_bytes());
                    self.set_toast("命令已发送到终端");
                }
            }
        }

        self.sync_browser_visibility();

        // WebView 子窗口边界同步（用上一帧记录的内容区矩形）。
        // 只有矩形所属的标签页正好是当前活动标签时才同步 —— 切换标签的那
        // 一帧槽位里可能还留着上一个浏览器标签的矩形。
        if let Some((area, area_idx)) = cx.try_global::<BrowserArea>().map(|b| (b.0, b.1)) {
            let idx = self.active.min(self.tabs.len().saturating_sub(1));
            if area_idx == idx {
                if let Some(TabContent::Browser(b)) = self.tabs.get_mut(idx) {
                    b.sync_bounds(area);
                }
            }
        }

        let lang = self.i18n.lang;

        // ---- 根容器 ----
        let mut root = div()
            .id("root")
            .relative()
            .size_full()
            .v_flex()
            .font_family(".SystemUIFont")
            .when_some(self.root_focus.clone(), |d, f| d.track_focus(&f))
            .on_action(cx.listener(|this, _: &NextTab, _, _| {
                if !this.tabs.is_empty() {
                    this.active = (this.active + 1) % this.tabs.len();
                }
            }))
            .on_action(cx.listener(|this, _: &PrevTab, _, _| {
                if !this.tabs.is_empty() {
                    this.active = (this.active + this.tabs.len() - 1) % this.tabs.len();
                }
            }))
            .on_action(cx.listener(|this, _: &NewLocalTab, _, _| this.open_local()))
            .on_action(cx.listener(|this, _: &NewBrowserTab, _, _| this.open_browser("about:blank")))
            .on_action(cx.listener(|this, _: &ToggleSftp, _, _| this.open_sftp()))
            .on_action(cx.listener(|this, _: &ToggleLocalFiles, _, _| this.open_local_files()))
            .on_action(cx.listener(|this, _: &ToggleRdp, _, _| {
                this.open_remote_desktop(RdTabProtocol::Rdp)
            }))
            .on_action(cx.listener(|this, _: &ToggleBrowser, _, _| this.open_browser("about:blank")))
            .on_action(cx.listener(|this, _: &ToggleAi, _, _| {
                this.ai_assistant.open = !this.ai_assistant.open
            }))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                if !this.tabs.is_empty() {
                    this.close_tab(this.active);
                    window.blur(cx);
                }
            }))
            .on_action(cx.listener(|_this, _: &ToggleFullscreen, window, _| {
                window.toggle_fullscreen();
            }))
            .on_action(cx.listener(|this, _: &FocusAddress, _, _| {
                if matches!(this.tabs.get(this.active), Some(TabContent::Browser(_))) {
                    this.focus_address = true;
                } else {
                    this.set_toast("Ctrl+L 需要先打开浏览器标签页");
                }
            }))
            .on_action(cx.listener(|this, _: &FindInTerminal, window, cx| {
                if matches!(this.tabs.get(this.active), Some(TabContent::Terminal(_))) {
                    if !this.find_open {
                        this.find_open = true;
                    }
                    this.find_focus = true;
                    // 焦点先落到输入框：否则按键会冒泡到终端，字打进 shell
                    if let Some(input) = this.find_input.clone() {
                        let handle = input.read(cx).focus_handle(cx);
                        window.focus(&handle, cx);
                    }
                    cx.notify();
                } else {
                    this.set_toast("Ctrl+F 仅在终端标签页可用");
                }
            }))
            .on_action(cx.listener(|this, _: &JumpTab1, _, _| {
                if !this.tabs.is_empty() {
                    this.active = 0
                }
            }))
            .on_action(cx.listener(|this, _: &JumpTab2, _, _| {
                if 1 < this.tabs.len() {
                    this.active = 1
                }
            }))
            .on_action(cx.listener(|this, _: &JumpTab3, _, _| {
                if 2 < this.tabs.len() {
                    this.active = 2
                }
            }))
            .on_action(cx.listener(|this, _: &JumpTab4, _, _| {
                if 3 < this.tabs.len() {
                    this.active = 3
                }
            }))
            .on_action(cx.listener(|this, _: &JumpTab5, _, _| {
                if 4 < this.tabs.len() {
                    this.active = 4
                }
            }))
            .on_action(cx.listener(|this, _: &JumpTab6, _, _| {
                if 5 < this.tabs.len() {
                    this.active = 5
                }
            }))
            .on_action(cx.listener(|this, _: &JumpTab7, _, _| {
                if 6 < this.tabs.len() {
                    this.active = 6
                }
            }))
            .on_action(cx.listener(|this, _: &JumpTab8, _, _| {
                if 7 < this.tabs.len() {
                    this.active = 7
                }
            }))
            .on_action(cx.listener(|this, _: &JumpTab9, _, _| {
                if 8 < this.tabs.len() {
                    this.active = 8
                }
            }))
            // 标签左右移动（Ctrl+Shift+← / →）
            .on_action(cx.listener(|this, _: &MoveTabLeft, _, _| {
                let cur = this.active;
                if cur > 0 {
                    this.move_tab(cur, cur - 1);
                }
            }))
            .on_action(cx.listener(|this, _: &MoveTabRight, _, _| {
                let cur = this.active;
                if cur + 1 < this.tabs.len() {
                    this.move_tab(cur, cur + 1);
                }
            }))
            // 分屏（light：只分当前窗格，不新开标签）
            .on_action(cx.listener(|this, _: &SplitRight, _, cx| {
                this.split_active_pane(Axis::Horizontal, false, cx)
            }))
            .on_action(cx.listener(|this, _: &SplitDown, _, cx| {
                this.split_active_pane(Axis::Vertical, false, cx)
            }))
            .on_action(cx.listener(|this, _: &ClosePane, _, cx| this.close_active_pane(cx)))
            .on_action(cx.listener(|this, _: &NextPane, _, cx| this.focus_next_pane(cx)));

        root = root
            .child(self.menu_bar_ui(window, cx))
            .child(self.toolbar_ui(window, cx))
            .child(self.quick_connect_ui(window, cx))
            .child(
                // 注意：gpui-kit 的 h_flex() 自带 items_center（交叉轴居中），
                // 会让侧栏/终端列按内容高度收缩并被垂直居中，导致终端 canvas
                // 高度坍缩为 0（屏幕上看不到任何输出）。这里必须 items_stretch。
                div()
                    .flex_1()
                    .flex()
                    .flex_row()
                    .items_stretch()
                    .min_w_0()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.sidebar_ui(window, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .v_flex()
                            .overflow_hidden()
                            .child(self.tab_strip_ui(window, cx))
                            .child(self.center_content_ui(window, cx)),
                    )
                    .children(self.sftp.render(window, cx))
                    .children(self.ai_panel_ui(window, cx)),
            )
            .children(if self.config.show_status_bar {
                Some(self.status_bar_ui(cx))
            } else {
                None
            });

        // ---- 对话框（覆盖层） ----
        let mut dialogs: Vec<AnyElement> = Vec::new();
        if let Some(el) = self.ssh_dialog_ui(window, cx) {
            dialogs.push(el);
        }
        if let Some(el) = self.add_session_dialog_ui(window, cx) {
            dialogs.push(el);
        }
        if let Some(el) = self.telnet_dialog_ui(window, cx) {
            dialogs.push(el);
        }
        if let Some(el) = self.ping_dialog_ui(window, cx) {
            dialogs.push(el);
        }
        if let Some(el) = self.batch_dialog_ui(window, cx) {
            dialogs.push(el);
        }
        if let Some(el) = self.tunnel_dialog_ui(window, cx) {
            dialogs.push(el);
        }
        if let Some(el) = self.config_panel.render(&mut self.config, &mut self.i18n, window, cx) {
            dialogs.push(el);
        }
        if let Some(el) = self.ai_config_panel.render(&mut self.config, window, cx) {
            dialogs.push(el);
        }
        if self.about_open {
            dialogs.push(self.about_dialog_ui(cx));
        }
        // 终端右键菜单（最上层，带点击遮罩关闭）
        if let Some(el) = self.term_context_menu_ui(cx) {
            dialogs.push(el);
        }
        for d in dialogs {
            root = root.child(d);
        }

        // 终端/浏览器/远程桌面需要持续重绘：驱动下一帧，保证 PTY 输出、光标、
        // WebView 对齐、远程桌面帧实时刷新。没有这类活动标签时停止循环，避免空转。
        let live = self.tabs.iter().any(|t| {
            matches!(
                t,
                TabContent::Terminal(_)
                    | TabContent::Browser(_)
                    | TabContent::RemoteDesktop(_)
                    | TabContent::Connecting { .. }
            )
        }) || self.port_scan.is_running();
        if live {
            window.request_animation_frame();
        }

        let _ = lang;
        root
    }
}
