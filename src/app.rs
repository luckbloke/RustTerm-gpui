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
use crate::tabs::{dock_panels, PendingPane, TabContent, TerminalWorkspace};
use crate::term::{SpawnCfg, TermPane};
use crate::terminal_view::TerminalView;
use crate::ui_icons::ic;
use crate::widgets::password_field;
use gpui_kit::base::component_traits::{Disableable as _, Selectable as _};
use gpui_kit::base::dock::{DockArea, DockPlacement, PanelId};
use gpui_kit::base::Placement;
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::dock::{panel_handle, DockSkin};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
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
    /// SSH X11 转发（内置 X server）
    pub x11: bool,
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
    /// 共享目录（仅 RDP 显示；RDPDR 驱动器重定向）
    pub share: Entity<InputState>,
    /// 旧版服务器兼容（仅 RDP：禁用 NLA，只用 TLS）
    pub legacy_compat: bool,
    /// SSH X11 转发（内置 X server）
    pub x11_forward: bool,
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
    /// 用户自定义的标签标题，与 `tabs` 平行（`None` = 用默认标题）。
    ///
    /// 不塞进 `TabContent`：标题是纯展示层信息，不该污染会话状态，
    /// 也不该跟着终端工作区的 dump/load 一起持久化。
    pub tab_titles: Vec<Option<String>>,
    /// 正在被拖动的标签索引（按下未松开；用于拖拽排序）
    pub tab_drag: Option<usize>,
    /// 正在重命名的标签索引（双击标签进入）
    pub tab_rename: Option<usize>,
    /// 重命名输入框（进入重命名时创建，退出时销毁）
    pub tab_rename_input: Option<Entity<InputState>>,
    /// 终端键盘焦点
    pub term_focus: Option<FocusHandle>,
    /// 根容器焦点：没有任何标签页时也要有个焦点节点，
    /// 否则按键事件没有分发路径，Ctrl+N / Ctrl+T 等全局快捷键全部失灵。
    pub root_focus: Option<FocusHandle>,
    /// 当前聚焦的终端面板（每帧按窗口焦点刷新；焦点跑到菜单/输入框时保留上一次，
    /// 所以菜单项、右键菜单、AI 面板都还能作用于「刚才那个终端」）。
    pub focused_term: Option<WeakEntity<TerminalView>>,
    /// 工作区 dock 编号（生成 dock id 用）
    next_workspace_id: usize,
    /// 上次同步给面板的字号（配置里改了字号时推给所有面板）
    pushed_font_size: f32,
    /// 菜单栏发起的窗格操作（下一帧 render 执行：需要 window）
    pub pending_pane_action: Option<PaneAction>,

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
    /// 上次写盘的标签快照（json 文本）。与当前标签对比，变了才落盘。
    last_snap_json: String,
    /// 快照节流：render 每帧都跑，落盘两秒一次就够
    last_snap_at: std::time::Instant,
    /// 本次运行是否真的有过多标签 —— 免得「一个都没开就退出」把上次的快照冲掉
    had_tabs: bool,
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
    X11Server,
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

/// 菜单栏里发起的窗格操作。
///
/// 菜单项的点击回调只有 `&mut App`（拿不到 `&mut Window`），而 dock 的分屏 /
/// 关闭 / 切标签都需要 window——所以先记下意图，在下一帧 render 里执行。
/// 与既有的 `pending_new_session` 同一套路。
#[derive(Clone, Copy, PartialEq)]
pub enum PaneAction {
    /// 在聚焦面板旁边分屏
    Split(Placement),
    /// 关闭聚焦面板
    Close,
    /// 聚焦下一个面板
    Next,
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
            tab_titles: Vec::new(),
            tab_drag: None,
            tab_rename: None,
            tab_rename_input: None,
            term_focus: None,
            root_focus: None,
            focused_term: None,
            next_workspace_id: 1,
            pushed_font_size: 0.0,
            pending_pane_action: None,
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
            // 先把磁盘上的旧快照当基线：首帧标签为空，不这么做会立刻用空快照覆盖它
            last_snap_json: serde_json::to_string(&crate::sessions::LastSession::load().tabs)
                .unwrap_or_default(),
            last_snap_at: std::time::Instant::now(),
            had_tabs: false,
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
            Ok(pane) => self.push_tab(TabContent::Terminal(TerminalWorkspace::with_pane(pane))),
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
        x11: bool,
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
                .block_on(TermPane::new_ssh(tab_title, host, port, user, auth, jump, 80, 24, x11));
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

    /// 打开内置 X11 服务的画面标签（已存在则切过去）。
    ///
    /// X server 是进程内全局单例：SSH 会话启用 X11 转发后，远端 X 程序的
    /// 连接都灌进同一个屏幕；这个标签只是它的"取景器"。
    pub fn open_x11_screen(&mut self) {
        if let Some(idx) = self.tabs.iter().position(
            |t| matches!(t, TabContent::RemoteDesktop(tab) if tab.profile.protocol == RdTabProtocol::X11),
        ) {
            self.active = idx;
            return;
        }
        let lang = self.i18n.lang;
        let profile = RdpConnectionProfile {
            name: crate::i18n::tr(lang, "x11.screen_name"),
            protocol: RdTabProtocol::X11,
            ..Default::default()
        };
        let mut tab = RemoteDesktopTab::new(profile);
        tab.connect();
        self.push_tab(TabContent::RemoteDesktop(tab));
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
        let is_term = content.workspace().is_some();
        self.tabs.push(content);
        // 与 tabs 平行的自定义标题数组必须同步长高，否则下标会错位
        self.tab_titles.push(None);
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

        // 快速连接没有 X11 开关：X11 跟随保存的会话记录（编辑会话勾选）
        self.submit_ssh(&host, port, &user, auth, jump, false);
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
                self.submit_ssh(&s.host, s.port, &s.user, auth, jump, s.x11);
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
                    // 共享目录随会话走：连上之后远端「我的电脑」里就有这个盘符
                    share_dir: if proto == RdTabProtocol::Rdp {
                        s.share_dir.clone()
                    } else {
                        String::new()
                    },
                    // 旧版服务器兼容也只对 RDP 有意义
                    legacy_compat: proto == RdTabProtocol::Rdp && s.legacy_compat,
                    // 记住来源，标签页里改目录能写回这条会话
                    session_id: Some(s.id.clone()),
                    ..Default::default()
                };
                self.rdp_profiles.ensure(profile.clone());
                let mut tab = RemoteDesktopTab::new(profile);
                tab.connect();
                self.push_tab(TabContent::RemoteDesktop(tab));
            }
        }
    }

    /// 把当前远程桌面标签页里的连接设置写回它来源的那条会话。
    ///
    /// 共享目录和「旧版服务器兼容」都有两个入口：会话对话框（本来就存盘）
    /// 和标签页连接表单。后者原来只是本次运行的临时状态，重启后回落到
    /// 会话里的旧值 —— 明明是「连同一个会话」，盘符或兼容设置却没了。
    /// 这里在点「连接」/切换勾选时对齐一次，两处入口以**标签页**为准
    ///（那是用户刚改的）。
    ///
    /// 值没变就不写盘：每次连接都刷一遍 sessions.json 没必要。
    pub fn persist_rd_settings(&mut self) {
        let (session_id, share_dir, legacy_compat) = match self.tabs.get(self.active) {
            Some(TabContent::RemoteDesktop(tab)) => (
                tab.profile.session_id.clone(),
                tab.profile.share_dir.clone(),
                tab.profile.legacy_compat,
            ),
            _ => return,
        };
        let Some(id) = session_id else { return };
        let Some(slot) = self.sessions.sessions.iter_mut().find(|s| s.id == id) else {
            return;
        };
        let mut changed = false;
        if slot.share_dir != share_dir {
            slot.share_dir = share_dir;
            changed = true;
        }
        if slot.legacy_compat != legacy_compat {
            slot.legacy_compat = legacy_compat;
            changed = true;
        }
        if !changed {
            return;
        }
        if let Err(e) = self.sessions.save() {
            self.set_toast(e);
        }
    }

    // ------------------------------------------------------------------
    // 恢复上次会话（last_tabs.json）
    // ------------------------------------------------------------------

    /// 把当前打开的标签缩成可恢复描述。
    ///
    /// 终端工作区只取**第一个**面板的启动方式：分屏的内部结构归 dock 管，
    /// 这里只负责「这个标签原本是个什么连接」。
    fn snapshot_tabs(&self, cx: &App) -> Vec<crate::sessions::LastTab> {
        use crate::sessions::LastTab;
        let mut out = Vec::new();
        for tab in &self.tabs {
            match tab {
                TabContent::Terminal(ws) => {
                    let Some(dock) = &ws.dock else { continue };
                    let panels = crate::tabs::dock_panels(dock, cx);
                    let Some(view) = panels.first() else { continue };
                    let view = view.read(cx);
                    let Some(pane) = view.pane_ref() else { continue };
                    out.push(match &pane.spawn {
                        crate::term::SpawnCfg::Local => LastTab::Local,
                        crate::term::SpawnCfg::Command { program, args } => LastTab::Command {
                            program: program.clone(),
                            args: args.clone(),
                        },
                        crate::term::SpawnCfg::Tcp { host, port } => LastTab::Tcp {
                            host: host.clone(),
                            port: *port,
                        },
                        crate::term::SpawnCfg::Ssh { host, port, user, auth, .. } => {
                            let key_path = match auth {
                                crate::backends::ssh::SshAuth::Key { path, .. } => path.clone(),
                                _ => String::new(),
                            };
                            // 口令不进快照：恢复时按 host/port/user 回会话表找
                            LastTab::Ssh {
                                host: host.clone(),
                                port: *port,
                                user: user.clone(),
                                key_path,
                                session_id: None,
                            }
                        }
                    });
                }
                TabContent::Browser(t) => out.push(LastTab::Browser {
                    url: t.url.clone(),
                }),
                TabContent::RemoteDesktop(t) => out.push(LastTab::Rd {
                    protocol: t.profile.protocol.key().to_string(),
                    host: t.profile.host.clone(),
                    port: t.profile.port,
                    user: t.profile.username.clone(),
                    domain: t.profile.domain.clone(),
                    share_dir: t.profile.share_dir.clone(),
                    legacy_compat: t.profile.legacy_compat,
                    session_id: t.profile.session_id.clone(),
                }),
                TabContent::LocalFiles(_) => out.push(LastTab::Files),
                // 连接中 / 失败 / 键宏 / 端口扫描没有恢复价值
                TabContent::Connecting { .. }
                | TabContent::Failed(_)
                | TabContent::Macro
                | TabContent::PortScan => {}
            }
        }
        out
    }

    /// 每次 render 调一次：标签集合变了就落盘（两秒节流）。
    fn maybe_save_last_session(&mut self, cx: &App) {
        if self.tabs.is_empty() && !self.had_tabs {
            return;
        }
        if self.last_snap_at.elapsed() < std::time::Duration::from_secs(2) {
            return;
        }
        let tabs = self.snapshot_tabs(cx);
        let json = serde_json::to_string(&tabs).unwrap_or_default();
        if json == self.last_snap_json {
            // 内容没变也要推进节流时间，否则下一帧又白算一次
            self.last_snap_at = std::time::Instant::now();
            return;
        }
        self.had_tabs |= !tabs.is_empty();
        self.last_snap_json = json;
        self.last_snap_at = std::time::Instant::now();
        crate::sessions::LastSession { tabs }.save();
    }

    /// 恢复上次退出时开着的标签（欢迎页按钮 / 将来也能接到启动参数）。
    ///
    /// 凭据一律回 `sessions.json` 取：SSH 用 `session_id` 或主机+账户匹配，
    /// 远程桌面同理。匹配不到凭据的 SSH 标签**不会**拿空密码去撞，
    /// 而是跳过并在提示里报个数 —— 静默用错凭据比少恢复一个更难查。
    pub fn restore_last_session(&mut self, cx: &mut Context<Self>) {
        use crate::sessions::LastTab;
        let snap = crate::sessions::LastSession::load();
        if snap.tabs.is_empty() {
            self.set_toast("没有可恢复的会话记录");
            return;
        }
        let (mut restored, mut need_login) = (0usize, 0usize);
        for tab in snap.tabs {
            match tab {
                LastTab::Local => {
                    self.open_local();
                    restored += 1;
                }
                LastTab::Command { program, args } => {
                    if let Ok(pane) =
                        TermPane::new_command(program.clone(), &program, &args, 100, 24)
                    {
                        self.push_tab(TabContent::Terminal(TerminalWorkspace::with_pane(pane)));
                        restored += 1;
                    }
                }
                LastTab::Tcp { host, port } => {
                    self.open_telnet(&host, port);
                    restored += 1;
                }
                LastTab::Ssh { host, port, user, key_path, session_id } => {
                    let auth = self.resolve_ssh_auth(&host, port, &user, session_id.as_deref(), &key_path);
                    match auth {
                        Some(auth) => {
                            // X11 转发跟随保存的会话记录（最后标签快照里没有这个字段）
                            let x11 = session_id
                                .as_deref()
                                .and_then(|id| self.sessions.by_id(id))
                                .map(|s| s.x11)
                                .unwrap_or(false);
                            self.submit_ssh(&host, port, &user, auth, None, x11);
                            restored += 1;
                        }
                        None => need_login += 1,
                    }
                }
                LastTab::Browser { url } => {
                    self.open_browser(&url);
                    restored += 1;
                }
                LastTab::Rd {
                    protocol,
                    host,
                    port,
                    user,
                    domain,
                    share_dir,
                    legacy_compat,
                    session_id,
                } => {
                    if self.restore_remote_desktop(
                        &protocol,
                        host,
                        port,
                        user,
                        domain,
                        share_dir,
                        legacy_compat,
                        session_id,
                    ) {
                        restored += 1;
                    }
                }
                LastTab::Files => {
                    self.open_local_files();
                    restored += 1;
                }
            }
        }
        if need_login > 0 {
            self.set_toast(format!(
                "已恢复 {restored} 个标签；{need_login} 个 SSH 会话没存密码，请从侧栏重新连接"
            ));
        } else {
            self.set_toast(format!("已恢复 {restored} 个标签"));
        }
        cx.notify();
    }

    /// 恢复 SSH 用的认证信息：会话表里找得到就用它的（可能存了密码），
    /// 否则退回快照里的密钥路径。都没有 → `None`（调用方跳过这个标签）。
    fn resolve_ssh_auth(
        &self,
        host: &str,
        port: u16,
        user: &str,
        session_id: Option<&str>,
        key_path: &str,
    ) -> Option<crate::backends::ssh::SshAuth> {
        use crate::backends::ssh::SshAuth;
        let hit = session_id
            .and_then(|id| self.sessions.by_id(id))
            .or_else(|| {
                self.sessions
                    .sessions
                    .iter()
                    .find(|s| s.host == host && s.port == port && s.user == user)
            });
        if let Some(s) = hit {
            if s.uses_key() && !s.key_path.is_empty() {
                return Some(SshAuth::Key {
                    path: s.key_path.clone(),
                    passphrase: String::new(),
                });
            }
            if !s.password.is_empty() {
                return Some(SshAuth::Password(s.password.clone()));
            }
        }
        // 会话里没有（或没存密码）：临时连的会话只剩密钥路径可用
        if !key_path.is_empty() {
            return Some(SshAuth::Key {
                path: key_path.to_string(),
                passphrase: String::new(),
            });
        }
        None
    }

    /// 远程桌面标签的恢复：有来源会话就复用它的密码，然后直接重连；
    /// 没有就只把表单填好，等用户点连接（不猜密码）。
    fn restore_remote_desktop(
        &mut self,
        protocol: &str,
        host: String,
        port: u16,
        user: String,
        domain: String,
        share_dir: String,
        legacy_compat: bool,
        session_id: Option<String>,
    ) -> bool {
        let Some(proto) = RdTabProtocol::from_key(protocol) else {
            return false;
        };
        let password = session_id
            .as_deref()
            .and_then(|id| self.sessions.by_id(id))
            .map(|s| s.password.clone())
            .or_else(|| {
                self.sessions
                    .sessions
                    .iter()
                    .find(|s| s.host == host && s.port == port)
                    .map(|s| s.password.clone())
            })
            .unwrap_or_default();
        let name = if host.is_empty() {
            format!("恢复的 {}", proto.label())
        } else {
            format!("{} {}:{}", proto.label(), host, port)
        };
        let profile = RdpConnectionProfile {
            name,
            protocol: proto,
            host,
            port,
            username: user,
            password: password.clone(),
            domain,
            share_dir,
            legacy_compat,
            session_id,
            ..Default::default()
        };
        let mut tab = RemoteDesktopTab::new(profile);
        // 有密码（说明来自会话）才自动连；否则停在表单上让用户确认
        if !password.is_empty() {
            tab.connect();
        }
        self.push_tab(TabContent::RemoteDesktop(tab));
        true
    }

    /// 执行工具栏 / 侧栏的动作。
    pub fn run_tool_action(&mut self, action: ToolAction, cx: &mut Context<Self>) {
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
            ToolAction::Find => self.toggle_find(cx),
            ToolAction::Sync => self.toggle_sync_input(cx),
            ToolAction::Batch => {
                self.batch_open = true;
                self.focus_dialog = true;
            }
            ToolAction::Tunnel => self.tunnel_open = true,
            ToolAction::Macro => self.open_macro_tab(),
            ToolAction::PortScan => self.open_port_scan_tab(),
            ToolAction::X11Server => self.open_x11_screen(),
        }
    }

    /// 打开/关闭终端查找栏。打开时把焦点交给查找输入框。
    pub fn toggle_find(&mut self, cx: &mut App) {
        if self.focused_view(cx).is_none() {
            self.set_toast("查找仅对终端可用（先点一下某个终端窗格）");
            return;
        }
        self.find_open = !self.find_open;
        if self.find_open {
            self.find_focus = true;
        } else {
            if let Some(view) = self.focused_view(cx) {
                view.update(cx, |v, _| v.clear_search());
            }
            self.focus_term = true;
        }
    }

    pub fn toggle_sync_input(&mut self, cx: &mut App) {        self.sync_input = !self.sync_input;
        if self.sync_input {
            let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
            self.sync_tx = Some(tx);
            self.sync_rx = Some(rx);
            self.set_toast("同步输入已开启：聚焦窗格的按键会复制到其它终端");
        } else {
            self.sync_tx = None;
            self.sync_rx = None;
            // 分屏后一屏可能有多个终端，全部解除镜像
            for view in self.all_term_views(cx) {
                view.update(cx, |v, _| v.set_mirror(None));
            }
            self.set_toast("同步输入已关闭");
        }
    }

    pub fn pump_sync_input(&mut self, cx: &mut App) {
        if !self.sync_input {
            return;
        }
        let Some(tx) = self.sync_tx.clone() else {
            return;
        };
        let source = self.focused_view(cx);
        let views = self.all_term_views(cx);

        // 镜像源只有一个：当前聚焦的终端；其余终端都要清掉，
        // 否则上个来源的按键会被继续复制出去
        for view in &views {
            let is_source = source.as_ref().map(|s| s == view).unwrap_or(false);
            let tx = tx.clone();
            view.update(cx, |v, _| {
                if is_source {
                    v.set_mirror(Some(tx));
                } else {
                    v.set_mirror(None);
                }
            });
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
        // 分发到除镜像源之外的每一个终端
        for view in &views {
            if source.as_ref().map(|s| s == view).unwrap_or(false) {
                continue;
            }
            view.update(cx, |v, _| {
                for b in &batch {
                    v.write_input_local(b.clone());
                }
            });
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
                self.push_tab(TabContent::Terminal(TerminalWorkspace::with_pane(pane)));
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
            Ok(pane) => self.push_tab(TabContent::Terminal(TerminalWorkspace::with_pane(pane))),
            Err(e) => self.set_toast(format!("Ping 启动失败: {e:#}")),
        }
    }

    // ------------------------------------------------------------------
    // 终端面板：创建 / 分屏 / 关闭（布局全部交给 dock）
    // ------------------------------------------------------------------

    /// 当前聚焦的终端面板。
    ///
    /// `focused_term` 由每帧的 [`Self::track_terminal_focus`] 维护：焦点落在某个
    /// 终端上就记住它；焦点跑去菜单 / 输入框时**保留上一次**，这样右键菜单、
    /// 菜单栏项、AI 面板仍作用于「刚才那个终端」。
    pub fn focused_view(&self, cx: &App) -> Option<Entity<TerminalView>> {
        let view = self.focused_term.as_ref()?.upgrade()?;
        // 实体可能刚被 dock 移除，用「还在某个工作区的面板里」再确认一次
        if self.all_term_views(cx).iter().any(|v| v == &view) {
            Some(view)
        } else {
            None
        }
    }

    /// 所有工作区里的全部终端面板（批量执行 / 同步输入 / 焦点跟踪用）。
    pub fn all_term_views(&self, cx: &App) -> Vec<Entity<TerminalView>> {
        let mut out = Vec::new();
        for tab in &self.tabs {
            if let Some(dock) = tab.workspace().and_then(|ws| ws.dock.as_ref()) {
                out.extend(dock_panels(dock, cx));
            }
        }
        out
    }

    /// 某个标签（工作区）里的第一个终端面板。
    fn tab_first_view(&self, tab: usize, cx: &App) -> Option<Entity<TerminalView>> {
        let ws = self.tabs.get(tab)?.workspace()?;
        dock_panels(ws.dock.as_ref()?, cx).into_iter().next()
    }

    /// 当前活动标签对应的 dock 区域（仅终端标签返回）。
    pub fn active_workspace_dock(&self) -> Option<Entity<DockArea>> {
        self.tabs
            .get(self.active.min(self.tabs.len().saturating_sub(1)))
            .and_then(|t| t.workspace())
            .and_then(|ws| ws.dock.clone())
    }

    /// 首帧把工作区的 dock 建起来，并把排队的面板装进去。
    ///
    /// dock 需要 `&mut Window`、面板实体需要 `&mut App`，而「新建终端」是从菜单 /
    /// 快捷键发起的（两者都没有），所以面板先排队，在这里统一落地。
    fn ensure_workspace_dock(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let need_build = self
            .tabs
            .get(idx)
            .and_then(|t| t.workspace())
            .map(|ws| ws.dock.is_none())
            .unwrap_or(false);
        if !need_build {
            return;
        }

        let id = format!("rustterm-term-{}", self.next_workspace_id);
        self.next_workspace_id += 1;
        let (area, skin) = DockSkin::dock_area(id, Some(1), window, cx);

        let pendings = {
            let Some(ws) = self.tabs.get_mut(idx).and_then(|t| t.workspace_mut()) else {
                return;
            };
            ws.dock = Some(area.clone());
            ws.skin = Some(skin);
            std::mem::take(&mut ws.pending)
        };

        let font_size = self.config.font_size;
        let mut created: Vec<(Entity<TerminalView>, bool)> = Vec::new();
        for pending in pendings {
            let (view, is_ssh) = match pending {
                PendingPane::Ready(pane) => {
                    let is_ssh = matches!(pane.spawn, SpawnCfg::Ssh { .. });
                    (TerminalView::entity(pane, font_size, cx), is_ssh)
                }
                PendingPane::Connecting { title, rx } => (
                    TerminalView::connecting(title, rx, font_size, cx),
                    true,
                ),
            };
            created.push((view.clone(), is_ssh));
            area.update(cx, |area, acx| {
                area.add_panel_view(
                    panel_handle(view.clone()),
                    DockPlacement::Center,
                    None,
                    window,
                    acx,
                );
            });
        }

        // 新终端直接吃掉键盘焦点（否则还要先点一下才能打字）
        if let Some((view, _)) = created.first() {
            let handle = view.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
            self.focused_term = Some(view.downgrade());
        }

        // SSH 会话就绪：执行「连接时」宏（与旧的整标签连接行为一致）
        let macro_idxs = self.macros.get_on_connect_macros();
        if !macro_idxs.is_empty() {
            for (view, is_ssh) in &created {
                if !*is_ssh {
                    continue;
                }
                let mut bytes: Vec<Vec<u8>> = Vec::new();
                for i in &macro_idxs {
                    if let Some(b) = self.macros.send_macro_bytes(*i) {
                        bytes.push(b);
                    }
                }
                if bytes.is_empty() {
                    continue;
                }
                view.update(cx, |v, _| {
                    for b in &bytes {
                        v.write_input(b.clone());
                    }
                });
            }
        }
    }

    /// 按启动配置造一个终端面板（SSH 走后台线程，先显示「连接中」占位）。
    fn build_pane(
        &self,
        spawn: SpawnCfg,
        cols: u16,
        rows: u16,
        title: String,
        cx: &mut App,
    ) -> Result<Entity<TerminalView>, String> {
        let font_size = self.config.font_size;
        match spawn {
            SpawnCfg::Local => TermPane::new_local(title, cols, rows)
                .map(|p| TerminalView::entity(p, font_size, cx))
                .map_err(|e| format!("分屏失败: {e:#}")),
            SpawnCfg::Command { program, args } => {
                TermPane::new_command(title, &program, &args, cols, rows)
                    .map(|p| TerminalView::entity(p, font_size, cx))
                    .map_err(|e| format!("分屏失败: {e:#}"))
            }
            SpawnCfg::Tcp { host, port } => TermPane::new_tcp(title, host, port, cols, rows)
                .map(|p| TerminalView::entity(p, font_size, cx))
                .map_err(|e| format!("分屏失败: {e:#}")),
            SpawnCfg::Ssh {
                host,
                port,
                user,
                auth,
                jump,
                x11,
            } => {
                let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<TermPane>>();
                let runtime = self.runtime.clone();
                let tab_title = title.clone();
                std::thread::spawn(move || {
                    let result = runtime.block_on(TermPane::new_ssh(
                        tab_title, host, port, user, auth, jump, cols, rows, x11,
                    ));
                    let _ = tx.send(result);
                });
                Ok(TerminalView::connecting(title, rx, font_size, cx))
            }
        }
    }

    /// 在当前聚焦面板旁边分屏（`placement` 决定方向）。
    ///
    /// 新面板**复制当前会话的启动配置**：SSH 会另起一条连接（不共享通道），
    /// 本地 / Telnet / 命令同样照搬。
    pub fn split_focused(
        &mut self,
        placement: Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.focused_view(cx) else {
            self.set_toast("分屏需要先有一个终端窗格");
            return;
        };
        let Some(dock) = self.active_workspace_dock() else {
            return;
        };
        let picked = {
            let v = view.read(cx);
            v.spawn().cloned().map(|spawn| {
                let (cols, rows) = v.dims();
                (spawn, cols, rows, v.title())
            })
        };
        let Some((spawn, cols, rows, title)) = picked else {
            self.set_toast("该窗格还没连上，稍后再分屏");
            return;
        };

        let new_view = match self.build_pane(spawn, cols, rows, title, cx) {
            Ok(v) => v,
            Err(e) => {
                self.set_toast(e);
                return;
            }
        };

        let target = PanelId::from(view.entity_id());
        let new_id = PanelId::from(new_view.entity_id());
        let node = dock
            .read(cx)
            .layout(DockPlacement::Center)
            .and_then(|tree| tree.find_panel_node(target));
        let handle = new_view.read(cx).focus_handle(cx);
        dock.update(cx, |area, acx| {
            // 先在 dock 里登记面板实体（panel() 查找需要它），再用 move_panel
            // 挪到分割位置——move_panel 会先从 add_panel_view 落位的地方摘除。
            // 此前用 split_at（只插不摘），同一个面板实体被两个节点引用，
            // 渲染两遍：主窗格 + 上下各一格，且上下"同步输入"（其实是同一个
            // pty 被画了两遍）。
            area.add_panel_view(
                panel_handle(new_view.clone()),
                DockPlacement::Center,
                None,
                window,
                acx,
            );
            if let Some(node) = node {
                area.move_panel(
                    new_id,
                    gpui_kit::base::dock::InsertTarget::Split {
                        node,
                        placement,
                        size: None,
                    },
                    window,
                    acx,
                );
            }
        });
        window.focus(&handle, cx);
        self.focused_term = Some(new_view.downgrade());
        cx.notify();
    }

    /// 关闭当前聚焦的终端面板；只剩一个面板时等价于关闭整个工作区标签。
    pub fn close_focused_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.focused_view(cx) else {
            self.set_toast("没有可关闭的终端窗格");
            return;
        };
        let Some(dock) = self.active_workspace_dock() else {
            return;
        };
        if dock_panels(&dock, cx).len() <= 1 {
            let idx = self.active.min(self.tabs.len().saturating_sub(1));
            self.close_tab(idx, cx);
            window.blur(cx);
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
            view.update(cx, |v, _| {
                for b in &bytes {
                    v.write_input(b.clone());
                }
            });
        }

        dock.update(cx, |area, acx| {
            area.remove_panel(view.clone(), window, acx);
        });

        // 焦点交给剩下的第一个面板
        if let Some(next) = dock_panels(&dock, cx).first() {
            let handle = next.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
            self.focused_term = Some(next.downgrade());
        }
        cx.notify();
    }

    /// 每帧记录键盘焦点落在哪个终端面板上。
    ///
    /// 焦点不在任何终端上（菜单 / 输入框 / 浏览器）时**保留上一次**：菜单项与
    /// 右键菜单在点击瞬间已经不再持有终端焦点，但仍要作用于刚才那个终端。
    fn track_terminal_focus(&mut self, window: &Window, cx: &App) {
        let Some(handle) = window.focused(cx) else {
            return;
        };
        for view in self.all_term_views(cx) {
            if view.read(cx).focus_handle(cx) == handle {
                self.focused_term = Some(view.downgrade());
                return;
            }
        }
    }

    /// 在同一个工作区内把焦点切到下一个终端面板（dock 切标签 + 键盘焦点）。
    pub fn focus_next_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dock) = self.active_workspace_dock() else {
            return;
        };
        let panels = dock_panels(&dock, cx);
        if panels.len() < 2 {
            return;
        }
        let cur = self
            .focused_view(cx)
            .and_then(|v| panels.iter().position(|p| p == &v))
            .unwrap_or(0);
        let next = panels[(cur + 1) % panels.len()].clone();
        let id = PanelId::from(next.entity_id());
        let handle = next.read(cx).focus_handle(cx);
        dock.update(cx, |area, acx| area.select_panel(id, window, acx));
        window.focus(&handle, cx);
        self.focused_term = Some(next.downgrade());
        cx.notify();
    }

    /// 是否有可操作的终端面板（菜单项置灰用）。
    pub fn can_split(&self, cx: &App) -> bool {
        self.focused_view(cx).is_some()
    }

    /// 当前工作区是否多于一个终端面板（「关闭当前窗格」菜单项置灰用）。
    pub fn has_multiple_panes(&self, cx: &App) -> bool {
        self.active_workspace_dock()
            .map(|dock| dock_panels(&dock, cx).len() > 1)
            .unwrap_or(false)
    }

    pub fn send_batch_command(&mut self, cmd: String, cx: &mut App) {
        let cmd = cmd.trim().to_string();
        if cmd.is_empty() {
            return;
        }
        // 批量执行覆盖**每一个**终端面板（分屏、各工作区都算）
        let views = self.all_term_views(cx);
        let mut n = 0usize;
        for view in &views {
            view.update(cx, |v, _| {
                if !v.is_dead() {
                    v.write_input_local(format!("{cmd}\r").into_bytes());
                    n += 1;
                }
            });
        }
        self.batch_open = false;
        self.set_toast(format!("已把命令发送到 {n} 个终端"));
    }

    pub fn send_macro_to_active(&mut self, idx: usize, cx: &mut App) {
        let Some(bytes) = self.macros.send_macro_bytes(idx) else {
            return;
        };
        let target = self.focused_view(cx).or_else(|| self.tab_first_view(self.active, cx));
        let Some(view) = target else {
            return;
        };
        let sent = view.update(cx, |v, _| {
            if v.is_dead() {
                return false;
            }
            v.write_input(bytes);
            true
        });
        if sent {
            self.set_toast("宏已发送到当前终端");
        }
    }

    pub fn close_tab(&mut self, index: usize, cx: &mut App) {
        if index < self.tabs.len() {
            // 关闭前先执行「断开时」宏（如 exit / 清理命令）——
            // 字节已经进了 SSH 通道的发送队列，随后标签被销毁不影响投递。
            if self.tabs[index].workspace().is_some() {
                let mut bytes: Vec<Vec<u8>> = Vec::new();
                for i in self.macros.get_on_disconnect_macros() {
                    if let Some(b) = self.macros.send_macro_bytes(i) {
                        bytes.push(b);
                    }
                }
                if !bytes.is_empty() {
                    if let Some(view) = self.tab_first_view(index, cx) {
                        view.update(cx, |v, _| {
                            for b in &bytes {
                                v.write_input(b.clone());
                            }
                        });
                    }
                }
            }
            self.tabs[index].shutdown();
            self.tabs.remove(index);
            if index < self.tab_titles.len() {
                self.tab_titles.remove(index);
            }
            // 关标签后重命名态一律作废：索引已经不可信
            self.tab_rename = None;
            self.tab_rename_input = None;
            self.tab_drag = None;
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
        // 自定义标题跟着走，否则重命名会「粘」在原来的位置上
        if from < self.tab_titles.len() && ins <= self.tab_titles.len() {
            let title = self.tab_titles.remove(from);
            self.tab_titles.insert(ins, title);
        }
        self.active = a;
        // 拖拽排序后重命名态作废（索引变了）
        self.tab_rename = None;
        self.tab_rename_input = None;
        self.tab_drag = None;
    }

    pub fn poll_connecting(&mut self) {
        // 分屏 / 新开标签的 SSH 结果都在面板自己的 `tick` 里收（见
        // `terminal_view.rs`），这里只处理「整标签级」的 Connecting：
        // 连上后开一个终端工作区，面板在首帧渲染时装进 dock。
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
                        self.tabs[i] = TabContent::Terminal(TerminalWorkspace::with_pane(pane));
                        // 「连接时」宏在面板装进 dock 之后执行
                        // （`ensure_workspace_dock` 里统一处理）
                    }
                    Err(msg) => self.tabs[i] = TabContent::Failed(msg),
                }
            }
        }
    }

    // 「连接时 / 断开时」宏的执行已内联到实际发生连接/断开的地方：
    // - 连接：`ensure_workspace_dock`（面板装进 dock 后统一执行）
    // - 断开：`close_focused_pane` / `close_tab`（销毁面板前把字节排进发送队列）

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

    /// 当前聚焦终端的概要（AI 面板展示用）。
    pub fn active_terminal_hint(&self, cx: &App) -> Option<String> {
        let view = self.focused_view(cx)?;
        let v = view.read(cx);
        let spawn = v.spawn()?;
        Some(match spawn {
            SpawnCfg::Ssh { host, port, user, .. } => format!("SSH {user}@{host}:{port}"),
            SpawnCfg::Tcp { host, port } => format!("Telnet {host}:{port}"),
            SpawnCfg::Command { program, .. } => format!("命令 {program}"),
            SpawnCfg::Local => "本地终端".to_string(),
        })
    }

    /// 取当前聚焦终端的最近输出，作为 AI 对话的上下文。
    pub fn current_terminal_context(&self, cx: &App) -> Option<String> {
        let view = self.focused_view(cx)?;
        let v = view.read(cx);
        let spawn = v.spawn()?;
        let text = v.context_text(self.config.ai.context_lines, self.config.ai.context_max_line_len);
        if text.trim().is_empty() {
            return None;
        }
        let header = match spawn {
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

    /// 异步弹出「选择目录」对话框，选中后回调写回界面。
    ///
    /// 不能在 UI 线程直接调 `rfd::FileDialog::pick_folder()`：Windows 上 rfd 会
    /// 在当前线程里跑一个嵌套消息循环，GPUI 会借机把事件派发进 App，撞上外层
    /// 已经持有的 RefCell 借用 → `RefCell already borrowed` panic（release 下
    /// panic = abort，表现就是整个程序闪退）。
    /// 这里把对话框放到后台线程，结果再回前台线程应用。
    pub(crate) fn pick_folder_async(
        cx: &mut Context<Self>,
        window: &Window,
        on_pick: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>, String) + 'static,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let picked = cx
                .background_executor()
                .spawn(async { rfd::FileDialog::new().pick_folder() })
                .await;
            let Some(dir) = picked else { return };
            let path = dir.display().to_string();
            this.update_in(cx, |this, window, cx| on_pick(this, window, cx, path))
                .ok();
        })
        .detach();
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
                    |this, cx| this.toggle_sync_input(cx),
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
                    tr(lang, "item.find"),
                    Some(Box::new(FindInTerminal)),
                    false,
                    |this, cx| this.toggle_find(cx),
                ));
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.copy_sel"),
                    None,
                    false,
                    |this, cx| {
                        let copied = this
                            .focused_view(cx)
                            .map(|v| v.update(cx, |v, vcx| v.copy_selection(vcx)))
                            .unwrap_or(false);
                        let lang = this.i18n.lang;
                        this.set_toast(tr(
                            lang,
                            if copied {
                                "toast.copied"
                            } else {
                                "toast.no_selection"
                            },
                        ));
                    },
                ));
                m = m.item(menu_item(&app, tr(lang, "item.paste_clip"), None, false, |this, cx| {
                    let pasted = match this.focused_view(cx) {
                        Some(v) => v.update(cx, |v, vcx| v.paste_clipboard(vcx)),
                        None => false,
                    };
                    let lang = this.i18n.lang;
                    this.set_toast(tr(
                        lang,
                        if pasted {
                            "toast.pasted"
                        } else {
                            "toast.clipboard_empty"
                        },
                    ));
                }));
                m = m.item(menu_item(&app, tr(lang, "item.select_all"), None, false, |this, cx| {
                    if let Some(v) = this.focused_view(cx) {
                        v.update(cx, |v, _| v.select_all());
                    }
                }));
                m = m.separator();
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.split_right"),
                    Some(Box::new(SplitRight)),
                    false,
                    |this, _cx| this.pending_pane_action = Some(PaneAction::Split(Placement::Right)),
                ));
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.split_down"),
                    Some(Box::new(SplitDown)),
                    false,
                    |this, _cx| {
                        this.pending_pane_action = Some(PaneAction::Split(Placement::Bottom))
                    },
                ));
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.next_pane"),
                    Some(Box::new(NextPane)),
                    false,
                    |this, _cx| this.pending_pane_action = Some(PaneAction::Next),
                ));
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.close_pane"),
                    Some(Box::new(ClosePane)),
                    false,
                    |this, _cx| this.pending_pane_action = Some(PaneAction::Close),
                ));
                m = m.separator();
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.close_tab"),
                    Some(Box::new(CloseTab)),
                    false,
                    |this, cx| {
                        if !this.tabs.is_empty() {
                            this.close_tab(this.active, cx);
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
                        m = m.item(menu_item(
                            &sub_app,
                            tr(lang, "menu.x11"),
                            None,
                            false,
                            |this, _cx| this.open_x11_screen(),
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
                    |this, cx| this.run_tool_action(ToolAction::Ping, cx),
                ));
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.telnet"),
                    None,
                    false,
                    |this, cx| this.run_tool_action(ToolAction::TelnetClient, cx),
                ));
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.portscan"),
                    None,
                    false,
                    |this, cx| this.run_tool_action(ToolAction::PortScan, cx),
                ));
                m = m.item(menu_item(
                    &app,
                    tr(lang, "menu.x11"),
                    None,
                    false,
                    |this, cx| this.run_tool_action(ToolAction::X11Server, cx),
                ));
                m = m.separator();
                m = m.item(menu_item(
                    &app,
                    tr(lang, "item.macro_manager"),
                    None,
                    false,
                    |this, cx| this.run_tool_action(ToolAction::Macro, cx),
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
                    move |this, _win, cx| this.run_tool_action(action, cx),
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
        let share = Self::mk_input("共享目录（RDP，可空）", window, cx);
        self.add_form = Some(AddFormInputs {
            name,
            host,
            port,
            user,
            password,
            key_path,
            domain,
            share,
            legacy_compat: false,
            kind: SessionKind::Ssh,
            auth_key: false,
            x11_forward: false,
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
        let share = Self::mk_input("共享目录（RDP，可空）", window, cx);
        Self::set_input(&name, &s.name, window, cx);
        Self::set_input(&host, &s.host, window, cx);
        Self::set_input(&port, &s.port.to_string(), window, cx);
        Self::set_input(&user, &s.user, window, cx);
        Self::set_input(&password, &s.password, window, cx);
        Self::set_input(&key_path, &s.key_path, window, cx);
        Self::set_input(&domain, &s.domain, window, cx);
        Self::set_input(&share, &s.share_dir, window, cx);
        self.add_form = Some(AddFormInputs {
            name,
            host,
            port,
            user,
            password,
            key_path,
            domain,
            share,
            legacy_compat: s.legacy_compat,
            kind: s.kind,
            auth_key: s.uses_key(),
            x11_forward: s.x11,
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
            (ToolAction::X11Server, "X 服务（内置 X11）"),
            (ToolAction::LocalFiles, "本地文件浏览"),
            (ToolAction::Browser, "内置浏览器"),
        ];
        let mut list = div().v_flex().flex_1().min_h_0().overflow_y_scrollbar().p_1();
        for (i, (action, label)) in actions.into_iter().enumerate() {
            list = list.child(
                Button::new(SharedString::from(format!("tool-{i}")))
                    .label(label)
                    .compact()
                    .on_click(cx.listener(move |this, _, _, cx| this.run_tool_action(action, cx))),
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
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.send_macro_to_active(i, cx);
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

        // 重命名 / 拖拽状态里的索引可能因为关标签而失效，先自查
        if self.tab_rename.map(|i| i >= self.tabs.len()).unwrap_or(false) {
            self.tab_rename = None;
            self.tab_rename_input = None;
        }
        if self.tab_drag.map(|i| i >= self.tabs.len()).unwrap_or(false) {
            self.tab_drag = None;
        }
        let renaming = self.tab_rename;
        let drag_from = self.tab_drag;

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
                let (default_label, icon) = match tab {
                    TabContent::Connecting { title, .. } => (title.clone(), Some(IconName::Loader)),
                    TabContent::Terminal(ws) => {
                        if ws.any_dead(cx).unwrap_or(false) {
                            (format!("{} 已断开", ws.title(cx)), Some(IconName::X))
                        } else {
                            (ws.title(cx), Some(IconName::SquareTerminal))
                        }
                    }
                    TabContent::Failed(_) => ("连接失败".to_string(), Some(IconName::TriangleAlert)),
                    TabContent::Browser(_) => (tab.title(cx), Some(IconName::Globe)),
                    TabContent::RemoteDesktop(_) => (tab.title(cx), Some(IconName::Monitor)),
                    TabContent::LocalFiles(_) => (tab.title(cx), Some(IconName::Folder)),
                    TabContent::Macro => (tab.title(cx), Some(IconName::Keyboard)),
                    TabContent::PortScan => (tab.title(cx), Some(IconName::Radar)),
                };
                // 用户重命名过的优先（平行的标题数组，None 表示用默认）
                let label = self
                    .tab_titles
                    .get(i)
                    .and_then(|t| t.clone())
                    .unwrap_or(default_label);
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
            let idx = i;

            // 正在重命名的那一格：换成输入框（回车提交，点击别处也提交）
            if renaming == Some(idx) {
                if let Some(input) = self.tab_rename_input.clone() {
                    strip = strip.child(
                        div()
                            .h_flex()
                            .items_center()
                            .px_2()
                            .h(px(TABS_H - 6.0))
                            .rounded(px(6.0))
                            .bg(theme.colors.secondary)
                            .child(Input::new(&input).w(px(150.0)).text_size(px(12.0))),
                    );
                }
                continue;
            }

            let color = if r.failed { theme.colors.danger } else { dim };
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
                    // 拖动中的源标签淡一点，让用户知道「正在搬这一格」
                    .when(drag_from == Some(idx), |d| d.opacity(0.45))
                    .hover(|d| d.bg(theme.colors.secondary_hover))
                    // 拖拽排序：按下记源、松开落在哪一格就插到哪一格
                    // （不用拖影/阈值：标签条本来就是等宽小块，直接换位最直观）
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.tab_drag = Some(idx);
                            cx.notify();
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            let from = this.tab_drag.take();
                            if let Some(from) = from {
                                if from != idx {
                                    this.move_tab(from, idx);
                                }
                            }
                            cx.notify();
                        }),
                    )
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                        // 双击标签 → 内联重命名
                        if ev.click_count() >= 2 {
                            this.begin_tab_rename(idx, window, cx);
                            return;
                        }
                        // 单击别处时，先落定上一次重命名
                        if this.tab_rename.is_some() {
                            this.commit_tab_rename(cx);
                        }
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
                                    this.close_tab(idx, cx);
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

    /// 进入标签重命名：创建输入框、填入当前标题并聚焦。
    fn begin_tab_rename(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        let current = self
            .tab_titles
            .get(idx)
            .and_then(|t| t.clone())
            .unwrap_or_else(|| self.tabs[idx].title(cx));
        let input = Self::mk_input("标签名", window, cx);
        Self::set_input(&input, &current, window, cx);
        // 全选：双击重命名的心理预期是「直接打字覆盖」
        let handle = input.read(cx).focus_handle(cx).clone();
        input.update(cx, |state, cx| state.select_all(window, cx));
        cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
            match event {
                InputEvent::PressEnter { .. } => this.commit_tab_rename(cx),
                // 点到别处也算改完（否则输入框一直挂着）
                InputEvent::Blur => this.commit_tab_rename(cx),
                _ => {}
            }
        })
        .detach();
        self.tab_rename = Some(idx);
        self.tab_rename_input = Some(input);
        window.focus(&handle, cx);
        cx.notify();
    }

    /// 提交重命名（空字符串 = 恢复默认标题）。
    fn commit_tab_rename(&mut self, cx: &mut Context<Self>) {
        let Some(idx) = self.tab_rename.take() else {
            return;
        };
        let text = Self::input_value(&self.tab_rename_input, cx);
        if idx < self.tab_titles.len() {
            let t = text.trim();
            self.tab_titles[idx] = if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            };
        }
        self.tab_rename_input = None;
        cx.notify();
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
                        .child(chips)
                        .child(
                            // 「恢复上次会话」：上次退出时开着哪些标签就重开哪些。
                            // 没记录时不隐藏按钮，点了给个明确提示 —— 灰按钮更容易被当成坏了
                            Button::new("welcome-restore")
                                .icon(ic(IconName::RotateCcw))
                                .label(tr(lang, "welcome.restore"))
                                .compact()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.restore_last_session(cx);
                                })),
                        ),
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

    /// 终端标签页：dock 区域 + 查找栏 + 键盘事件。
    ///
    /// 终端的绘制、吸数据、鼠标都由各面板自己负责（见 `terminal_view.rs`）；
    /// 这里只保留**键盘**：宏快捷键与查找栏 Esc 要在这一层处理，而按键会从
    /// 聚焦的面板冒泡到这个容器。
    fn terminal_area_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let idx = self.active.min(self.tabs.len().saturating_sub(1));
        // dock 需要 window 才能创建：首帧在这里落地
        self.ensure_workspace_dock(idx, window, cx);

        let focus = self.term_focus.clone();
        let dock = self
            .tabs
            .get(idx)
            .and_then(|t| t.workspace())
            .and_then(|ws| ws.dock.clone());

        let mut area = div()
            .id("term-area")
            .flex_1()
            .min_h_0()
            .v_flex()
            .relative()
            .overflow_hidden()
            .bg(crate::term::bg_color())
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                // 焦点不在终端面板上时不要往 pty 送字节（查找框、菜单里的按键
                // 也会冒泡到这里）
                let Some(view) = this.focused_view(cx) else {
                    return;
                };
                if !view.read(cx).focused(window) {
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
                if let Some(macro_ix) = this.macros.get_macro_by_hotkey(&hotkey) {
                    this.send_macro_to_active(macro_ix, cx);
                    cx.notify();
                    return;
                }
                // 其余按键交给**聚焦的那个面板**（分屏后每格一个 pty，不能发错人）
                view.update(cx, |v, vcx| v.handle_key(&ev.keystroke, vcx));
                cx.notify();
            }))
            // 右键菜单的**唯一入口**：终端面板自己不消费右键（鼠标上报模式除外，
            // 那种情况面板会 stop_propagation），事件冒泡到这里才开菜单。
            // 位置直接用窗口坐标 —— 菜单挂在覆盖层，left/top 就是窗口坐标。
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    this.term_menu = Some(ev.position);
                    cx.notify();
                }),
            );

        if let Some(f) = focus.clone() {
            area = area.track_focus(&f).key_context("Terminal");
        }

        if let Some(dock) = dock {
            // 包一层显式 flex 列：dock 自身是 size_full，父层要有确定高度
            area = area.child(
                div()
                    .v_flex()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(dock.into_any_element()),
            );
        }

        // 查找栏：停靠在终端上方（不再压在终端底部遮挡命令行），
        // 独立于 term-area 之外——点击/键盘都不会被终端抢走焦点。
        let mut column = div().v_flex().flex_1().min_h_0();
        if self.find_open {
            column = column.child(self.find_bar_ui(window, cx));
        }
        column.child(area).into_any_element()
    }

    /// 鼠标命中判定 / 逐格画布这些都随「面板自己画自己」搬到 `terminal_view.rs`，
    /// 这里不再需要分屏树、pane 元素构建与命中路由。

    /// 终端查找栏：输入即搜，Enter 下一个 / Shift+Enter 上一个 / Esc 关闭。
    fn find_bar_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::global(cx).clone();

        if self.find_input.is_none() {
            let input = Self::mk_input("查找文本（Enter 下一个，Esc 关闭）", window, cx);
            cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                match event {
                    InputEvent::Change => {
                        let needle = Self::input_value(&this.find_input, cx);
                        if let Some(view) = this.focused_view(cx) {
                            view.update(cx, |v, _| v.set_search(&needle));
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

        let (idx, total) = self
            .focused_view(cx)
            .map(|v| v.read(cx).search_progress())
            .unwrap_or((0, 0));
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
        if let Some(view) = self.focused_view(cx) {
            view.update(cx, |v, _| {
                if v.search_needle() != needle {
                    v.set_search(&needle);
                } else if forward {
                    v.search_next();
                } else {
                    v.search_prev();
                }
            });
        }
        cx.notify();
    }

    /// 关闭查找栏：清空高亮，焦点交回终端。
    fn close_find(&mut self, cx: &mut Context<Self>) {
        self.find_open = false;
        if let Some(view) = self.focused_view(cx) {
            view.update(cx, |v, _| v.clear_search());
        }
        self.focus_term = true;
        cx.notify();
    }

    /// 终端右键菜单：复制 / 粘贴 / 全选 / 分屏四项。
    ///
    /// 键盘快捷键之外再给一条纯鼠标路径——不同输入法/键盘布局下
    /// Ctrl+Shift+C 未必到得了终端，菜单永远可用。
    fn term_context_menu_ui(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pos = self.term_menu?;
        let lang = self.i18n.lang;
        let theme = Theme::global(cx).clone();
        let target = self.focused_view(cx);
        let has_sel = target
            .as_ref()
            .map(|v| v.read(cx).has_selection())
            .unwrap_or(false);
        let has_text = target.is_some();

        let dim = theme.colors.muted_foreground;
        let fg = theme.colors.foreground;

        let item = |id: &'static str, label: String, enabled: bool| {
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

        menu = menu.child(item("term-menu-copy", tr(lang, "item.copy"), has_sel).on_click(
            cx.listener(|this, _, _, cx| {
                let copied = this
                    .focused_view(cx)
                    .map(|v| v.update(cx, |v, vcx| v.copy_selection(vcx)))
                    .unwrap_or(false);
                let lang = this.i18n.lang;
                this.set_toast(tr(
                    lang,
                    if copied {
                        "toast.copied"
                    } else {
                        "toast.no_selection"
                    },
                ));
                this.term_menu = None;
                this.focus_term = true;
                cx.notify();
            },
        )));
        menu = menu.child(item("term-menu-paste", tr(lang, "item.paste"), has_text).on_click(
            cx.listener(|this, _, _, cx| {
                let pasted = match this.focused_view(cx) {
                    Some(v) => v.update(cx, |v, vcx| v.paste_clipboard(vcx)),
                    None => false,
                };
                let lang = this.i18n.lang;
                this.set_toast(tr(
                    lang,
                    if pasted {
                        "toast.pasted"
                    } else {
                        "toast.clipboard_empty"
                    },
                ));
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
            item("term-menu-all", tr(lang, "item.select_all"), has_text).on_click(
                cx.listener(|this, _, _, cx| {
                    if let Some(v) = this.focused_view(cx) {
                        v.update(cx, |v, _| v.select_all());
                    }
                    this.term_menu = None;
                    this.focus_term = true;
                    cx.notify();
                }),
            ),
        );

        // 分屏：右键菜单是分屏最容易被发现的地方（菜单栏也有同样四项）。
        menu = menu.child(
            div()
                .h(px(1.0))
                .my_1()
                .mx_2()
                .bg(theme.colors.border)
                .into_any_element(),
        );
        let can_split = self.can_split(cx);
        let multi = self.has_multiple_panes(cx);
        menu = menu.child(
            item("term-menu-split-r", tr(lang, "ctx.split_right"), can_split).on_click(
                cx.listener(|this, _, window, cx| {
                    this.term_menu = None;
                    this.split_focused(Placement::Right, window, cx);
                }),
            ),
        );
        menu = menu.child(
            item("term-menu-split-d", tr(lang, "ctx.split_down"), can_split).on_click(
                cx.listener(|this, _, window, cx| {
                    this.term_menu = None;
                    this.split_focused(Placement::Bottom, window, cx);
                }),
            ),
        );
        menu = menu.child(
            item("term-menu-pane-next", tr(lang, "ctx.next_pane"), multi).on_click(
                cx.listener(|this, _, window, cx| {
                    this.term_menu = None;
                    this.focus_next_pane(window, cx);
                }),
            ),
        );
        menu = menu.child(
            item("term-menu-pane-close", tr(lang, "ctx.close_pane"), multi).on_click(
                cx.listener(|this, _, window, cx| {
                    this.term_menu = None;
                    this.close_focused_pane(window, cx);
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

    // 终端的列/行换算与全部鼠标处理都在 `terminal_view.rs`：
    // 每个面板自己收自己的鼠标事件，不再需要「命中判定 → 投递给某一格」这一层。

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
                t.title(cx)
            )
        };

        let (status_text, status_color) = if self.tabs.is_empty() {
            (format!("● {}", tr(lang, "status.none")), dim)
        } else {
            match &self.tabs[self.active.min(self.tabs.len() - 1)] {
                TabContent::Terminal(t) => {
                    if t.any_dead(cx).unwrap_or(false) {
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

        // 尺寸 / 光标行列为**聚焦终端**的数据；分屏时额外标一个格数
        if let Some(view) = self.focused_view(cx) {
            let v = view.read(cx);
            let (cols, rows) = v.dims();
            right = right.child(
                div()
                    .text_color(dim)
                    .text_size(px(10.0))
                    .child(format!("{cols}×{rows}")),
            );
            let panes = self
                .active_workspace_dock()
                .map(|d| dock_panels(&d, cx).len())
                .unwrap_or(1);
            if panes > 1 {
                right = right.child(
                    div()
                        .text_color(dim)
                        .text_size(px(10.0))
                        .child(format!("{panes} 格")),
                );
            }
            if let Some((r, c)) = v.cursor_row_col() {
                right = right.child(
                    div()
                        .text_color(dim)
                        .text_size(px(10.0))
                        .child(format!("行:{r}, 列:{c}")),
                );
            }
            // 终端模式提示：备用屏（vim/top 之类）与鼠标跟踪
            if v.alt_screen() {
                right = right.child(
                    div()
                        .text_color(theme.colors.warning)
                        .text_size(px(10.0))
                        .child("备用屏"),
                );
            }
            if v.mouse_mode() {
                right = right.child(
                    div()
                        .text_color(theme.colors.info)
                        .text_size(px(10.0))
                        .child("鼠标跟踪"),
                );
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
                x11: false,
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
            .child(
                Checkbox::new("ssh-x11")
                    .label(crate::i18n::tr(self.i18n.lang, "item.x11_forward"))
                    .checked(form.x11)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        if let Some(f) = this.ssh_form.as_mut() {
                            f.x11 = *checked;
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
        self.submit_ssh(&host, port, &user, auth, None, form.x11);
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
            .child(field("域", &form.domain));
        // 共享目录只对 RDP 有意义：VNC / SPICE 协议里没有文件通道
        let content = if form.kind == SessionKind::Rdp {
            content
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_2()
                        .child(div().w(px(52.0)).text_size(px(12.0)).child("共享目录"))
                        .child(Input::new(&form.share).flex_1().text_size(px(12.0)))
                        .child(
                            Button::new("add-share-pick")
                                .label("浏览…")
                                .compact()
                                .on_click(cx.listener(|_, _, window, cx| {
                                    Self::pick_folder_async(cx, window, |this, window, cx, path| {
                                        if let Some(f) = this.add_form.as_mut() {
                                            let input = f.share.clone();
                                            input.update(cx, |s, cx| s.set_value(path.clone(), window, cx));
                                        }
                                        cx.notify();
                                    });
                                })),
                        ),
                )
                .child(
                    Checkbox::new("add-rd-legacy")
                        .label("旧版服务器兼容（Server 2008：禁用 NLA，仅用 TLS）")
                        .checked(form.legacy_compat)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            if let Some(f) = this.add_form.as_mut() {
                                f.legacy_compat = *checked;
                            }
                            cx.notify();
                        })),
                )
        } else {
            content
        };
        let content = content
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
            .when(form.kind == SessionKind::Ssh, |d| {
                d.child(
                    Checkbox::new("add-x11-forward")
                        .label(crate::i18n::tr(self.i18n.lang, "item.x11_forward"))
                        .checked(form.x11_forward)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            if let Some(f) = this.add_form.as_mut() {
                                f.x11_forward = *checked;
                            }
                            cx.notify();
                        })),
                )
            })
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
        let share_dir = if form.kind == SessionKind::Rdp {
            Self::input_value(&Some(form.share.clone()), cx)
                .trim()
                .to_string()
        } else {
            // 切到 VNC/SPICE 时把旧目录丢掉，避免 json 里留下无意义的路径
            String::new()
        };
        let legacy_compat = form.kind == SessionKind::Rdp && form.legacy_compat;

        if host.trim().is_empty() {
            self.set_toast("主机不能为空");
            return;
        }
        let id = form.editing.clone().unwrap_or_else(crate::sessions::SessionEntry::new_id);
        // 分组在表单里没有输入框：编辑时必须沿用原值，否则一保存就掉进默认分组
        let group = self
            .sessions
            .by_id(&id)
            .map(|s| s.group.clone())
            .unwrap_or_else(crate::sessions::default_group);
        let entry = crate::sessions::SessionEntry {
            id,
            name,
            kind: form.kind,
            host,
            port,
            user,
            password,
            auth_method: if form.auth_key { "key" } else { "password" }.to_string(),
            key_path,
            domain,
            share_dir,
            legacy_compat,
            x11: form.kind == SessionKind::Ssh && form.x11_forward,
            group,
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
            .all_term_views(cx)
            .iter()
            .filter(|v| !v.read(cx).is_dead())
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
                                this.send_batch_command(cmd, cx);
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

// 终端 canvas 的内边距常量已挪到 `term.rs`（`term::TERM_PAD`），
// 面板视图与绘制共用一份。

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
        self.pump_sync_input(cx);
        // 标签集合变化就更新「上次会话」快照（两秒节流，写在 render 里是因为
        // 开/关标签的入口太多，逐个接线容易漏；这里天然覆盖所有路径）
        self.maybe_save_last_session(cx);
        // 记录键盘焦点落在哪个终端面板（菜单项 / AI / SFTP 都按它取目标）
        self.track_terminal_focus(window, cx);

        // 菜单栏发起的窗格操作（分屏 / 关闭 / 切换）：需要 window，延后到这一帧
        if let Some(action) = self.pending_pane_action.take() {
            match action {
                PaneAction::Split(placement) => self.split_focused(placement, window, cx),
                PaneAction::Close => self.close_focused_pane(window, cx),
                PaneAction::Next => self.focus_next_pane(window, cx),
            }
        }

        // 字号改了：推给所有终端面板（面板自己重测格子）
        if (self.pushed_font_size - self.config.font_size).abs() > f32::EPSILON {
            self.pushed_font_size = self.config.font_size;
            let size = self.config.font_size;
            for view in self.all_term_views(cx) {
                view.update(cx, |v, vcx| v.set_font_size(size, vcx));
            }
        }

        // 关闭查找栏后把焦点交回终端（否则按键没人接，终端像「死了」一样）
        if std::mem::take(&mut self.focus_term) {
            // 聚焦面板优先；没有就交给当前工作区的第一个面板
            let target = self
                .focused_view(cx)
                .or_else(|| self.tab_first_view(self.active, cx));
            if let Some(view) = target {
                let handle = view.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
        }

        // 切到 SSH 标签自动弹出 SFTP，切走自动收起
        {
            let ssh_key = self
                .focused_view(cx)
                .and_then(|v| v.read(cx).ssh_handle())
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

        // SFTP 绑定当前聚焦的 SSH 会话
        {
            let (label, ssh) = match self.focused_view(cx) {
                Some(view) => {
                    let v = view.read(cx);
                    (v.title(), v.ssh_handle())
                }
                None => (String::new(), None),
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
                crate::xsrv::counters::mark_ui_dirty();
                cx.notify();
            }
        }

        // 终端数据泵 / 尺寸 fit / 快照都在各面板自己的 render 里完成
        // （见 `terminal_view.rs`）：一屏多个终端时各自管各自的数据与画面。
        // 帧的持续由文件末尾的 `request_animation_frame` 保证。

        // AI 面板上下文提示 + 待发送
        self.ai_assistant.attach_hint = self.active_terminal_hint(cx);
        if self.ai_assistant.take_pending_send() {
            let context = self.current_terminal_context(cx);
            let ai = self.config.ai.clone();
            self.ai_assistant.send(&ai, context, window, cx);
        }

        // AI 面板的「发送到终端」请求
        if let Some((cmd, dangerous)) = self.ai_assistant.command_request.take() {
            if self.config.ai.read_only {
                self.set_toast("只读模式：写命令已被禁用");
            } else if dangerous {
                self.set_toast(format!("⚠ 危险命令已拦截: {cmd}"));
            } else if let Some(view) = self.focused_view(cx) {
                let sent = view.update(cx, |v, _| {
                    if v.is_dead() {
                        return false;
                    }
                    v.write_input(format!("{cmd}\r").into_bytes());
                    true
                });
                if sent {
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
                    this.close_tab(this.active, cx);
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
            // 分屏（dock：任意嵌套 / 拖到边缘 / 标签组 / 单格缩放）
            .on_action(cx.listener(|this, _: &SplitRight, window, cx| {
                this.split_focused(Placement::Right, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SplitDown, window, cx| {
                this.split_focused(Placement::Bottom, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ClosePane, window, cx| {
                this.close_focused_pane(window, cx)
            }))
            .on_action(cx.listener(|this, _: &NextPane, window, cx| {
                this.focus_next_pane(window, cx)
            }));

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
            // NyaTerm: dock 化之后 gpui 只重绘「被 notify 过」的视图——终端视图
            // 藏在 dock 里，不显式通知的话 PTY 输出要等偶发事件（鼠标移动、
            // 切标签）触发重绘才显示，打字回显就成了数秒一批的慢镜头。
            // 这里逐帧通知当前工作区的所有终端面板，恢复"有终端标签就一直
            // 重绘"的设计意图（帧成本有行级 shaping 缓存兜底）。
            let idx = self.active.min(self.tabs.len().saturating_sub(1));
            let dock = self
                .tabs
                .get(idx)
                .and_then(|t| t.workspace())
                .and_then(|ws| ws.dock.clone());
            if let Some(dock) = dock {
                for view in crate::tabs::dock_panels(&dock, cx) {
                    view.update(cx, |_, vcx| vcx.notify());
                }
            }
            // NyaTerm: 这里必须**每帧**续帧，不能改成"按需/降频"。
            //
            // 教训（实测踩过）：GPUI 只在被 notify 时才重绘，而远程画面的
            // RdEvent 只在 render() 里被 poll() 消费——没有任何后台任务会在
            // 新帧到达时 notify。所以一旦停掉续帧，新帧就再也唤不醒 UI，画面
            // 卡到下一次偶发 notify（鼠标划过）才动，终端也跟着卡。
            // CPU 优化只能做在"每帧少做无用功"上（见 terminal_view 的快照按需
            // 重建），不能省掉续帧本身。
            window.request_animation_frame();
        }

        let _ = lang;
        root
    }
}

