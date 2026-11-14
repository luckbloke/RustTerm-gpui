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
use crate::rdp::{RdActivated, RdTabProtocol, RdpConnectionProfile, RdpProfiles, RemoteDesktopTab};
use crate::sessions::{SessionKind, SessionStore};
use crate::sftp::SftpPanel;
use crate::tabs::{dock_items, dock_panels, DockPanel, PendingPane, TabContent, TerminalWorkspace};
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
// UI 帧调度（脏驱动重绘）
// ---------------------------------------------------------------------------

/// 帧预算（毫秒）：单帧 CPU 超过它就说明主线程已经吃紧——继续"画完立刻再画"
/// 会把输入事件饿死（实测症状：过一会儿界面就操作不动了）。
pub const FRAME_BUDGET_MS: u32 = 24;

/// 活跃期的唤醒间隔（毫秒）。
///
/// 它不是帧率：唤醒只做 try_recv 级别的排空，真出帧还得有新内容。
/// 8ms 决定的是"PTY 输出 / 远端帧多久被发现"，比原来的 60fps 轮询更跟手。
pub const UI_WAKE_ACTIVE_MS: u64 = 8;

/// 上一帧超预算时的唤醒间隔：退一步，给输入处理与合成留出空隙
/// （避免"越忙越卡"的正反馈）。
pub const UI_WAKE_BACKOFF_MS: u64 = 16;

/// 没有任何活动内容时的兜底间隔（毫秒）。
///
/// 空闲时仍然要定期做几件必须轮询的事：隐藏面板事件排空、连接状态回传、
/// ZMODEM 提示、布局标脏。200ms 对它们足够，而 CPU 占用可以忽略。
pub const UI_WAKE_IDLE_MS: u64 = 200;

/// `[hb-ui]` 汇报间隔（毫秒）。
const UI_REPORT_MS: u128 = 5_000;

/// task-22 按需显示：底部状态栏的常驻提示（如「远端 X 程序已启动」）。
/// `None` = 无提示。x11_auto_popup 关闭时由 ui_pump_once 写入。
pub static STATUS_NOTICE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 帧调度输入（都是"这一帧观察到的事实"）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameSignals {
    /// 本帧处理过会改变画面的内容（PTY 输出 / 远端帧 / X11 变化 / 布局变化）
    pub dirty: bool,
    /// 本帧耗时（毫秒）
    pub frame_ms: u32,
}

/// 这一帧画完后要不要**立刻**再续一帧（纯函数，可单测）。
///
/// 有脏内容才续帧；上一帧已经超预算就交给 UI 泵按退让节奏唤醒，不再自己
/// 加码——"超预算还立刻续帧"就是越忙越卡的正反馈。
pub fn wants_extra_frame(sig: FrameSignals) -> bool {
    sig.dirty && sig.frame_ms < FRAME_BUDGET_MS
}

/// UI 泵的唤醒间隔（纯函数，可单测）。
///
/// - 没有活动内容 → 慢节奏兜底（空闲 CPU 目标就是靠这一条：不再 60fps 空转）；
/// - 有活动内容 + 上帧超预算 → 退让节奏；
/// - 有活动内容 → 快节奏（保证回显与远端画面跟手）。
pub fn wake_interval_ms(live: bool, over_budget: bool) -> u64 {
    if !live {
        UI_WAKE_IDLE_MS
    } else if over_budget {
        UI_WAKE_BACKOFF_MS
    } else {
        UI_WAKE_ACTIVE_MS
    }
}

/// 帧耗时窗口的百分位（纯函数，可单测）：返回 (p50, p95, max)。
/// 空窗口返回 (0, 0, 0)。
pub fn ui_frame_percentiles_of(samples: &[u32]) -> (u32, u32, u32) {
    if samples.is_empty() {
        return (0, 0, 0);
    }
    let mut v = samples.to_vec();
    v.sort_unstable();
    let at = |q: f32| v[((v.len() - 1) as f32 * q).round() as usize];
    (at(0.5), at(0.95), v[v.len() - 1])
}

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
    /// 当前活动的**远程桌面面板**（dock 里有远程格时，`set_active` 会发
    /// `RdActivated`，这里记下来）。「关闭当前窗格」要按它决定关哪一格。
    pub focused_rd: Option<WeakEntity<RemoteDesktopTab>>,
    /// ---- UI 帧调度（脏驱动重绘，见 `spawn_ui_pump`）----
    /// 上一帧开始绘制的时刻
    ui_frame_at: std::time::Instant,
    /// 最近若干帧的耗时样本（环形缓冲，算 p50/p95 用）
    ui_frame_ms: [u32; 64],
    ui_frame_ms_len: usize,
    ui_frame_ms_pos: usize,
    /// 上一帧是否超预算（泵据此退让）
    ui_over_budget: bool,
    /// 上次唤醒渲染的时刻（泵据此限频）
    ui_wake_at: std::time::Instant,
    /// 有内容等待重绘但还没唤醒（限频时攒着，不能丢）
    ui_paint_pending: bool,
    /// 泵统计（`[hb-ui]` 证据：优化前后对比就看这几项）
    ui_frames: u64,
    ui_ticks: u64,
    ui_wakes: u64,
    ui_last_report: std::time::Instant,
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
    /// 真 Agent 执行循环（Agent 开关开启且用户下达任务时才有值）。
    /// 驱动见 `agent_poll` / `agent_apply`；纯状态机在 `agent.rs`。
    pub agent_loop: Option<crate::agent::AgentLoop>,
    /// 本轮 Agent 锁定的终端窗格（Start 时锁定，中途死亡 → 终止本轮）。
    pub agent_target: Option<Entity<TerminalView>>,
    /// 当前命令写入终端的时刻。
    pub agent_wait_since: Option<std::time::Instant>,
    /// 上一次输出快照与时刻（静默检测：快照不变 ≥1.2s 即结算；
    /// 快照变化会重置静默计时——慢命令的持续输出不会被半途截断）。
    pub agent_last_snapshot: Option<(String, std::time::Instant)>,
    /// Agent 模型请求的结果通道（后台线程 → UI）。
    pub agent_rx: Option<std::sync::mpsc::Receiver<Result<String, crate::ai::AiError>>>,
    /// Thinking 节流拍的上一帧时刻（150ms 一拍，避免等模型时 60fps 空转）。
    pub agent_last_poll_notify: std::time::Instant,
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
    /// AI 面板右键菜单：窗口坐标 + 右键时所在消息的全文（复制整条用）
    /// + **右键瞬间抓取的划选文字**（复制选中用——在右键处理里当场读
    /// TextSelection，避免菜单弹出期间的任何交互把选区清掉）。
    pub ai_menu: Option<(Point<Pixels>, String, String)>,
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
    /// ZMODEM 发送：正在等用户选文件。
    ///
    /// 选择器必须在后台线程跑（见 `pick_folder_async` 的说明：Windows 上 rfd
    /// 会在当前线程跑嵌套消息循环，撞上 GPUI 已持有的借用 → release 下直接
    /// abort）；结果回前台再投递。记 EntityId 是为了把结果投回**原来那个**
    /// 窗格——分屏/多标签时不能投错。
    pub zmodem_pick: Option<(
        EntityId,
        std::sync::mpsc::Receiver<Vec<std::path::PathBuf>>,
    )>,
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
    /// 把当前标签放进另一个终端工作区的分屏（远程桌面 / 终端均可）
    MoveTabIntoSplit,
}

/// 被拖拽的标签。
///
/// 松手落在中间内容区 = 把这个标签放进分屏（见
/// [`RustTermApp::move_tab_into_split_at`]）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TabKey {
    /// 终端工作区：用 dock 实体 id 作稳定标识
    Dock(EntityId),
    /// 远程桌面：用面板实体 id
    Remote(EntityId),
    /// 其它类型（浏览器 / 文件 / 宏 / 端口扫描）——不支持放进分屏
    Other,
}

#[derive(Clone)]
pub struct TabDrag {
    /// 拖拽**开始时**的标签下标（仅用于预览提示）
    pub index: usize,
    /// 稳定标识：拖拽期间标签可能被重排（下标失效），落点按它重新解析
    pub key: TabKey,
    /// 标签文字（拖拽预览用）
    pub label: SharedString,
}

/// 拖拽时跟随光标的预览块。
struct TabDragPreview {
    label: SharedString,
}

impl Render for TabDragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx).clone();
        div()
            .px_2()
            .py_1()
            .rounded(px(4.0))
            .border_1()
            .border_color(theme.colors.primary)
            .bg(theme.colors.background)
            .text_size(px(12.0))
            .text_color(theme.colors.foreground)
            .child(self.label.clone())
    }
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
            focused_rd: None,
            ui_frame_at: std::time::Instant::now(),
            ui_frame_ms: [0; 64],
            ui_frame_ms_len: 0,
            ui_frame_ms_pos: 0,
            ui_over_budget: false,
            ui_wake_at: std::time::Instant::now(),
            ui_paint_pending: false,
            ui_frames: 0,
            ui_ticks: 0,
            ui_wakes: 0,
            ui_last_report: std::time::Instant::now(),
            next_workspace_id: 1,
            pushed_font_size: 0.0,
            pending_pane_action: None,
            config,
            i18n,
            config_panel: ConfigPanel::default(),
            ai_config_panel: AiConfigPanel::default(),
            ai_assistant: AiAssistantPanel::default(),
            agent_loop: None,
            agent_target: None,
            agent_wait_since: None,
        agent_last_snapshot: None,
            agent_rx: None,
            agent_last_poll_notify: std::time::Instant::now(),
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
        ai_menu: None,
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
            zmodem_pick: None,
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

    pub fn open_remote_desktop(&mut self, protocol: RdTabProtocol, cx: &mut Context<Self>) {
        let profile = RdpConnectionProfile {
            name: format!("新建 {}", protocol.label()),
            protocol,
            host: String::new(),
            port: protocol.default_port(),
            ..Default::default()
        };
        let tab = self.new_rd_tab(RemoteDesktopTab::new(profile), cx);
        self.push_tab(tab);
    }

    /// 打开内置 X11 服务的画面标签（已存在则切过去）。
    ///
    /// X server 是进程内全局单例：SSH 会话启用 X11 转发后，远端 X 程序的
    /// 连接都灌进同一个屏幕；这个标签只是它的"取景器"。
    pub fn open_x11_screen(&mut self, cx: &mut Context<Self>) {
        // task-22 多显示：优先打开**当前活动终端会话**的独享屏幕。
        let bound = self
            .tabs
            .get(self.active.min(self.tabs.len().saturating_sub(1)))
            .and_then(|t| t.workspace())
            .and_then(|_| self.tab_first_pane_id(self.active, cx));
        self.open_x11_screen_for(bound, cx);
    }

    /// 活动终端标签的第一个面板的 X 屏幕 id（未启用 X11 转发 = None）。
    fn tab_first_pane_id(&self, tab: usize, cx: &App) -> Option<String> {
        let view = self.tab_first_view(tab, cx)?;
        let pane = view.read(cx).pane_ref()?;
        pane.x11_screen_id()
    }

    /// 打开（或切换到）绑定 `screen_id` 的 X 屏幕标签。
    ///
    /// - 已有绑定同一屏幕的标签 → 直接切过去（不重复开）；
    /// - 没有 → 新建一个，标签名带序号；`screen_id` 写进 profile，
    ///   后端按它挂到对应会话独享的 server 上（多显示互不重叠）。
    /// - `screen_id = None`：独立屏幕（"default"）。
    pub fn open_x11_screen_for(&mut self, screen_id: Option<String>, cx: &mut Context<Self>) {
        let screen_id = screen_id.clone();
        // 打开（或切到）屏幕即满足提示诉求：清掉状态栏提示与待弹出队列
        *STATUS_NOTICE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if let Some(id) = &screen_id {
            crate::remote::x11::clear_popup_request(id);
        }
        if let Some(idx) = self.tabs.iter().position(
            |t| matches!(t, TabContent::RemoteDesktop(tab) if tab.read(cx).profile.protocol == RdTabProtocol::X11 && tab.read(cx).profile.x11_screen_id == screen_id),
        ) {
            self.active = idx;
            return;
        }
        let lang = self.i18n.lang;
        let n = self
            .tabs
            .iter()
            .filter(|t| matches!(t, TabContent::RemoteDesktop(tab) if tab.read(cx).profile.protocol == RdTabProtocol::X11))
            .count();
        let label = if n == 0 {
            crate::i18n::tr(lang, "x11.screen_name").to_string()
        } else {
            format!("{} #{}", crate::i18n::tr(lang, "x11.screen_name"), n + 1)
        };
        let profile = RdpConnectionProfile {
            name: label,
            protocol: RdTabProtocol::X11,
            x11_screen_id: screen_id.clone(),
            ..Default::default()
        };
        let mut tab = RemoteDesktopTab::new(profile);
        tab.connect();
        let tab = self.new_rd_tab(tab, cx);
        self.push_tab(tab);
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
    /// 创建一个远程桌面标签。
    ///
    /// 面板持有的是**实体**：这样它既能当普通标签渲染，也能被直接装进终端
    /// 工作区的 dock 成为一个分屏格——「把远程桌面放进分屏」就靠这个。
    fn new_rd_tab(&mut self, tab: RemoteDesktopTab, cx: &mut Context<Self>) -> TabContent {
        let entity = cx.new(|_| tab);
        // 记住「哪个远程面板在前台」：dock 不暴露当前选中面板的查询接口，
        // 只能由面板在 set_active(true) 时发事件（见 rdp.rs BasePanel::set_active）。
        cx.subscribe(&entity, |app, panel, _: &RdActivated, cx| {
            app.focused_rd = Some(panel.downgrade());
            cx.notify();
        })
        .detach();
        TabContent::RemoteDesktop(entity)
    }

    pub fn connect_session(&mut self, id: &str, jump_host: &str, cx: &mut Context<Self>) {
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
                let tab = self.new_rd_tab(tab, cx);
                self.push_tab(tab);
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
    /// 把某个远程桌面面板的设置（共享目录 / 旧版兼容）写回它来源的会话。
    ///
    /// 由面板的 `settings_dirty` 标志驱动：面板自己改 `profile`，app 负责落盘。
    /// **不依赖 `self.active`**——面板可能被放进终端工作区的 dock 分屏格，
    /// 那时当前标签是终端而不是它。
    pub fn persist_rd_settings_for(
        &mut self,
        session_id: Option<String>,
        share_dir: String,
        legacy_compat: bool,
    ) {
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
                TabContent::RemoteDesktop(t) => {
                    let t = t.read(cx);
                    out.push(LastTab::Rd {
                        protocol: t.profile.protocol.key().to_string(),
                        host: t.profile.host.clone(),
                        port: t.profile.port,
                        user: t.profile.username.clone(),
                        domain: t.profile.domain.clone(),
                        share_dir: t.profile.share_dir.clone(),
                        legacy_compat: t.profile.legacy_compat,
                        session_id: t.profile.session_id.clone(),
                        x11_screen_id: t.profile.x11_screen_id.clone(),
                    })
                }
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
                    x11_screen_id,
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
                        x11_screen_id,
                        cx,
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
        x11_screen_id: Option<String>,
        cx: &mut Context<Self>,
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
            // task-22 多显示：屏幕绑定必须在 connect() **之前**进 profile——
            // 后端建连时按它挂到会话独享的 server（连接后再改就晚了）
            x11_screen_id,
            ..Default::default()
        };
        let mut tab = RemoteDesktopTab::new(profile);
        // 有密码（说明来自会话）才自动连；否则停在表单上让用户确认
        if !password.is_empty() {
            tab.connect();
        }
        let tab = self.new_rd_tab(tab, cx);
        self.push_tab(tab);
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
            ToolAction::RdpClient => self.open_remote_desktop(RdTabProtocol::Rdp, cx),
            ToolAction::VncClient => self.open_remote_desktop(RdTabProtocol::Vnc, cx),
            ToolAction::SpiceClient => self.open_remote_desktop(RdTabProtocol::Spice, cx),
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
            ToolAction::X11Server => self.open_x11_screen(cx),
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

    /// 关闭当前聚焦的窗格；只剩一个窗格时等价于关闭整个工作区标签。
    ///
    /// 「当前聚焦」的判定顺序：
    /// 1. **有终端面板在前台** → 关它（窗口焦点是最新鲜的信号）；
    /// 2. 否则关**最后被 dock 激活的远程桌面面板**（面板 `set_active(true)`
    ///    时发的 `RdActivated` 事件记在 `focused_rd`）。
    ///
    /// 注意「还剩几个窗格」必须用 `dock_items`（终端 + 远程桌面）：早先只看
    /// 终端，dock 里放着「1 终端 + 1 远程」时会误判成「最后一格」而把整个
    /// 标签关掉——两个会话一起没了。
    pub fn close_focused_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dock) = self.active_workspace_dock() else {
            // 远程桌面等非终端标签没有 dock：给个提示，别让快捷键像坏了
            self.set_toast("当前标签没有可关闭的窗格");
            cx.notify();
            return;
        };
        let pane_count = dock_items(&dock, cx).len();

        // 1) 终端在前台：关这个终端窗格
        if let Some(view) = self.focused_view(cx) {
            if pane_count <= 1 {
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

            // 焦点交给剩下的第一个终端面板
            if let Some(next) = dock_panels(&dock, cx).first() {
                let handle = next.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
                self.focused_term = Some(next.downgrade());
            }
            cx.notify();
            return;
        }

        // 2) 没有终端在前台：关最后激活的远程桌面窗格
        let Some(rd) = self.focused_rd.as_ref().and_then(|w| w.upgrade()) else {
            self.set_toast("没有可关闭的窗格");
            cx.notify();
            return;
        };
        let in_this_dock = dock_items(&dock, cx)
            .iter()
            .any(|p| matches!(p, DockPanel::Remote(e) if *e == rd));
        if !in_this_dock {
            self.set_toast("没有可关闭的窗格");
            cx.notify();
            return;
        }
        if pane_count <= 1 {
            let idx = self.active.min(self.tabs.len().saturating_sub(1));
            self.close_tab(idx, cx);
            window.blur(cx);
            return;
        }

        dock.update(cx, |area, acx| {
            // 面板的 on_removed 会断开后端会话、释放线程与 socket
            area.remove_panel(rd.clone(), window, acx);
        });
        self.focused_rd = None;
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
                // 焦点在终端上：清掉远程标记，避免「关闭当前窗格」关错
                self.focused_rd = None;
                return;
            }
        }
        // 远程桌面面板同样按**窗口焦点**更新：`RdActivated` 只是「被 dock
        // 选中」的边沿信号，同屏两个远程格时不会再触发，只靠它会把窗格关错。
        for rd in self.all_rd_views(cx) {
            if rd.read(cx).focus_handle(cx) == handle {
                self.focused_rd = Some(rd.downgrade());
                // 对称清理：焦点已从终端跳到远程格，「关闭当前窗格」必须关
                // 远程而不是残留的 focused_term（否则分屏里会关错会话）。
                self.focused_term = None;
                return;
            }
        }
    }

    /// 所有标签里可见的远程桌面面板（标签形态 + 已放进分屏的），焦点跟踪用。
    pub fn all_rd_views(&self, cx: &App) -> Vec<Entity<RemoteDesktopTab>> {
        let mut out = Vec::new();
        for tab in &self.tabs {
            match tab {
                TabContent::RemoteDesktop(e) => out.push(e.clone()),
                TabContent::Terminal(ws) => {
                    if let Some(dock) = ws.dock.as_ref() {
                        for panel in dock_items(dock, cx) {
                            if let DockPanel::Remote(e) = panel {
                                out.push(e);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        out
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

    /// 按稳定标识找回标签当前的下标（拖拽期间标签可能已重排）。
    fn resolve_tab_key(&self, key: TabKey) -> Option<usize> {
        if key == TabKey::Other {
            return None;
        }
        self.tabs.iter().position(|tab| {
            let cur = match tab {
                TabContent::Terminal(ws) => ws
                    .dock
                    .as_ref()
                    .map(|d| TabKey::Dock(d.entity_id()))
                    .unwrap_or(TabKey::Other),
                TabContent::RemoteDesktop(e) => TabKey::Remote(e.entity_id()),
                _ => TabKey::Other,
            };
            cur == key
        })
    }

    /// 把**当前标签**放进另一个终端工作区的分屏。
    pub fn move_tab_into_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.move_tab_into_split_at(self.active, window, cx);
    }

    /// 把第 `index` 个标签放进另一个终端工作区的分屏。
    ///
    /// - 远程桌面标签（RDP/VNC/SPICE/X11）：面板实体直接装进目标 dock，
    ///   于是它从「标签」变成终端旁边的一个分屏格（「拖标签进分屏」的实现）；
    /// - 终端标签：它的所有终端面板并入目标 dock（两个终端工作区合并分屏）。
    pub fn move_tab_into_split_at(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tabs.len() < 2 {
            self.set_toast("只有一个标签：先再开一个终端，才能把它放进分屏");
            cx.notify();
            return;
        }
        let index = index.min(self.tabs.len() - 1);

        // 目标：另一个已经建好 dock 的终端工作区
        let target = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != index)
            .find_map(|(i, t)| {
                t.workspace()
                    .and_then(|ws| ws.dock.clone())
                    .map(|dock| (i, dock))
            });
        let Some((target_idx, dock)) = target else {
            self.set_toast("需要一个已打开的终端标签作为分屏目标");
            cx.notify();
            return;
        };

        // 先取出要迁移的面板实体（Clone 后移除源标签也不会销毁它们）。
        //
        // 必须用 dock_items（终端 **和** 远程桌面）：早先只取终端，结果把
        // 「已经放进分屏的远程面板」连同它的后端会话一起静默丢掉——源标签
        // 一 drop，源 dock 释放，远程实体没人引用 → 会话断开（对抗审查发现）。
        let remote = match self.tabs.get(index) {
            Some(TabContent::RemoteDesktop(e)) => Some(e.clone()),
            _ => None,
        };
        let mut term_views: Vec<Entity<TerminalView>> = Vec::new();
        let mut dock_remotes: Vec<Entity<RemoteDesktopTab>> = Vec::new();
        if remote.is_none() {
            if let Some(d) = self
                .tabs
                .get(index)
                .and_then(|t| t.workspace())
                .and_then(|ws| ws.dock.clone())
            {
                for panel in dock_items(&d, cx) {
                    match panel {
                        DockPanel::Terminal(v) => term_views.push(v),
                        DockPanel::Remote(v) => dock_remotes.push(v),
                    }
                }
            }
        }
        if remote.is_none() && term_views.is_empty() && dock_remotes.is_empty() {
            self.set_toast("这个标签没有可以放进分屏的面板");
            cx.notify();
            return;
        }

        // 移除源标签；目标索引在移除后可能左移一位
        self.tabs.remove(index);
        // `tab_titles` 是与 tabs 平行的数组（标签条按下标取它）：不同步删除
        // 会让所有自定义标签名整体错位一格。重命名/拖拽态同样作废（下标不可信）。
        if index < self.tab_titles.len() {
            self.tab_titles.remove(index);
        }
        self.tab_rename = None;
        self.tab_rename_input = None;
        self.tab_drag = None;
        let target_idx = if target_idx > index { target_idx - 1 } else { target_idx };
        self.active = target_idx.min(self.tabs.len().saturating_sub(1));

        // 落位：add_panel_view 会并进目标**标签组**（新面板成为组内活动标签，
        // 终端就被盖住了）。真正的「分屏」要再 move_panel 拆到右侧——锚点取
        // 目标 dock 里的第一个面板。
        let anchor = dock_items(&dock, cx)
            .first()
            .map(|p| match p {
                DockPanel::Terminal(v) => PanelId::from(v.entity_id()),
                DockPanel::Remote(v) => PanelId::from(v.entity_id()),
            });
        let node = anchor.and_then(|id| {
            dock.read(cx)
                .layout(DockPlacement::Center)
                .and_then(|tree| tree.find_panel_node(id))
        });

        let mut moved: Vec<PanelId> = Vec::new();
        dock.update(cx, |area, acx| {
            for entity in remote.into_iter().chain(dock_remotes) {
                moved.push(PanelId::from(entity.entity_id()));
                area.add_panel_view(panel_handle(entity), DockPlacement::Center, None, window, acx);
            }
            for view in term_views {
                moved.push(PanelId::from(view.entity_id()));
                area.add_panel_view(panel_handle(view), DockPlacement::Center, None, window, acx);
            }
            if let Some(node) = node {
                // 反序：每次插到锚点后一格会把先插的往后顶，正序迁移最终
                // 视觉顺序与迁移顺序相反；反序迁让先迁的贴近锚点。
                for id in moved.iter().rev() {
                    let id = *id;
                    area.move_panel(
                        id,
                        gpui_kit::base::dock::InsertTarget::Split {
                            node,
                            placement: Placement::Right,
                            size: None,
                        },
                        window,
                        acx,
                    );
                }
            }
        });
        cx.notify();
    }

    /// 当前工作区是否多于一个终端面板（「关闭当前窗格」菜单项置灰用）。
    pub fn has_multiple_panes(&self, cx: &App) -> bool {
        self.active_workspace_dock()
            // 远程桌面面板也算一个窗格：终端 + 远程同屏时「关闭当前窗格」应可用
            .map(|dock| dock_items(&dock, cx).len() > 1)
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

    /// 收「整标签级」的连接结果。返回是否有标签状态变化。
    ///
    /// 返回值的意义：脏驱动重绘之后，只有"有变化"才唤醒渲染；调用方
    /// （UI 泵 / render）据此决定要不要 notify。
    pub fn poll_connecting(&mut self) -> bool {
        // 分屏 / 新开标签的 SSH 结果都在面板自己的 `tick` 里收（见
        // `terminal_view.rs`），这里只处理「整标签级」的 Connecting：
        // 连上后开一个终端工作区，面板在首帧渲染时装进 dock。
        let mut changed = false;
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
                changed = true;
            }
        }
        changed
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

    /// ZMODEM：每帧一次的小调度。
    ///
    /// 两件事：
    /// 1. 后台文件选择器的结果回投给原来那个窗格（见 `zmodem_pick` 字段说明）；
    /// 2. 有窗格报「远端 `rz` 在等文件」就起后台线程弹 rfd 多选。
    ///
    /// 真正收/发文件、协议状态机都在 `term.rs` / `zmodem.rs` 里，UI 层只负责
    /// "弹窗、取结果、显示提示"。
    fn poll_zmodem(&mut self, cx: &mut Context<Self>) {
        // ---- 1) 选择器结果 ----
        let picked = match self.zmodem_pick.as_ref() {
            Some((id, rx)) => match rx.try_recv() {
                Ok(files) => Some((*id, files)),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                // 线程异常退出：当作用户取消，别让窗格一直等下去
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some((*id, Vec::new())),
            },
            None => None,
        };
        if let Some((id, files)) = picked {
            self.zmodem_pick = None;
            if let Some(view) = self
                .all_term_views(cx)
                .into_iter()
                .find(|v| v.entity_id() == id)
            {
                view.update(cx, |v, _| {
                    if let Some(pane) = v.pane_mut_ref() {
                        pane.zmodem_files_picked(files);
                    }
                });
            }
        }

        // ---- 2) 提示 + 新的选择请求 ----
        for view in self.all_term_views(cx) {
            let (notice, wants) = view.update(cx, |v, _| match v.pane_mut_ref() {
                Some(pane) => (pane.take_zmodem_notice(), pane.zmodem_wants_files()),
                None => (None, false),
            });
            if let Some(msg) = notice {
                self.set_toast(msg);
            }
            // 一次只弹一个：已有对话框在等结果时，别的窗格的请求先留着
            if wants && self.zmodem_pick.is_none() {
                let took = view.update(cx, |v, _| {
                    v.pane_mut_ref()
                        .map(|p| p.zmodem_take_file_request())
                        .unwrap_or(false)
                });
                if took {
                    let (tx, rx) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        let files = rfd::FileDialog::new()
                            .set_title("选择要通过 ZMODEM 发送的文件")
                            .pick_files()
                            .unwrap_or_default();
                        let _ = tx.send(files);
                    });
                    self.zmodem_pick = Some((view.entity_id(), rx));
                }
            }
        }
    }

    /// 状态栏：正在进行的 ZMODEM 传输（分屏时只显示一个，避免状态栏被撑爆）。
    fn zmodem_status_text(&self, cx: &App) -> Option<String> {
        self.all_term_views(cx)
            .into_iter()
            .find_map(|v| v.read(cx).pane_ref().and_then(|p| p.zmodem_status()))
    }

    // ------------------------------------------------------------------
    // UI 帧调度（脏驱动重绘）
    // ------------------------------------------------------------------

    /// 有"活"内容（终端 / 浏览器 / 远程桌面 / 连接中 / 端口扫描）时用快节奏泵。
    fn ui_has_live_content(&self) -> bool {
        self.tabs.iter().any(|t| {
            matches!(
                t,
                TabContent::Terminal(_)
                    | TabContent::Browser(_)
                    | TabContent::RemoteDesktop(_)
                    | TabContent::Connecting { .. }
            )
        }) || self.port_scan.is_running()
    }

    /// 启动 UI 泵：脏驱动重绘里**唯一**的调度者。
    ///
    /// 为什么必须有它：GPUI 只在被 notify 时才重绘，而 PTY 输出、远端桌面帧、
    /// 连接结果、端口扫描结果都没有后台任务会 notify。原实现因此只能无条件
    /// 每帧 `request_animation_frame()` —— 60fps 全 UI 重绘，实测把主线程占满
    /// （空闲 `[hb] cpu=+4s/5s`，X11 活跃时输入被饿死到 5~7fps）。
    ///
    /// 现在：泵按 8ms（活跃）/ 200ms（空闲）的节奏做**廉价排空**（只 try_recv
    /// 与状态更新，不布局不绘制），只有真的排到新内容才唤醒一帧。空闲时几乎
    /// 不画，活跃时按内容量画。
    fn spawn_ui_pump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // task-24：挂死取证看门狗（幂等，只有第一次调用真正起线程）
        crate::xsrv::spawn_watchdog();
        cx.spawn_in(window, async move |this, cx| loop {
            let interval = match this.update(cx, |this, _| this.ui_pump_interval()) {
                Ok(iv) => iv,
                // 应用已退出
                Err(_) => break,
            };
            cx.background_executor().timer(interval).await;
            if this.update(cx, |this, cx| this.ui_pump_once(cx)).is_err() {
                break;
            }
        })
        .detach();
    }

    fn ui_pump_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(wake_interval_ms(
            self.ui_has_live_content(),
            self.ui_over_budget,
        ))
    }

    /// 泵一拍：把所有"不需要绘制"的数据推进模型；有新内容才唤醒渲染。
    fn ui_pump_once(&mut self, cx: &mut Context<Self>) {
        self.ui_ticks += 1;
        // task-24 信标：UI 泵活着/卡在哪个阶段（看门狗写 stall.txt）
        crate::xsrv::counters::UI_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        crate::xsrv::counters::UI_PHASE.store(1, std::sync::atomic::Ordering::Relaxed);
        let mut dirty = crate::xsrv::counters::take_ui_dirty();

        // 0) task-22 按需显示：远端程序映射了窗口且屏幕标签未开。
        //    x11_auto_popup 开 → 自动弹出对应屏幕（**消费即完成，不再回队**——
        //    回队会让泵每拍强制把 active 拽回 X 屏幕标签，用户永远切不走）；
        //    关 → 回队保留提示，直到用户打开对应屏幕才清除。
        if let Some(screen_id) = crate::remote::x11::take_popup_request() {
            let auto = crate::config::x11_auto_popup();
            crate::xsrv::log_line(format_args!(
                "[popup] UI 消费 screen={screen_id} 自动弹出={auto}"
            ));
            if auto {
                self.open_x11_screen_for(Some(screen_id), cx);
                dirty = true;
            } else {
                crate::remote::x11::queue_popup_request(screen_id.clone());
                *STATUS_NOTICE.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(crate::i18n::tr(self.i18n.lang, "x11.program_started").to_string());
                dirty = true;
            }
        }

        // 1) 整标签级连接结果（连上 → 换成终端工作区）
        if self.poll_connecting() {
            dirty = true;
        }

        // 2) 终端面板：吸 PTY 数据 / 排 PaneEvent / 收面板级连接结果。
        //    隐藏面板同样要排——不排的话 unbounded 通道会堆积（内存 + 重新
        //    显示时雪崩式补帧），这是历史上踩过的坑。
        for view in self.all_term_views(cx) {
            if view.update(cx, |v, vcx| v.pump_cheap(vcx)) {
                // 数据已经离开通道：必须显式通知这个视图——GPUI 只重绘被
                // notify 过的视图，dock 里的终端不会因为父容器重绘而更新
                view.update(cx, |_, vcx| vcx.notify());
                dirty = true;
            }
        }

        // 3) 远程桌面（可见 + 隐藏）：排空 RdEvent。
        //    原实现放在 render 里并节流 300ms —— 那是在渲染借用窗口里 poll 的
        //    妥协（会放大与后台任务抢 App 借用的概率）。挪到泵里反而更安全，
        //    也不需要节流：可见面板的帧延迟从"下一次重绘"降到 ≤8ms。
        for rd in self.all_rd_views(cx) {
            if rd.update(cx, |t, tcx| t.poll(tcx)) {
                rd.update(cx, |_, tcx| tcx.notify());
                dirty = true;
            }
            // dock 里的远程面板不在标签渲染路径上：会话设置落盘在这里补一次，
            // 否则「共享目录 / 旧版兼容」改完重启就丢
            if rd.update(cx, |t, _| t.take_settings_dirty()) {
                let (sid, share, legacy) = {
                    let t = rd.read(cx);
                    (
                        t.profile.session_id.clone(),
                        t.profile.share_dir.clone(),
                        t.profile.legacy_compat,
                    )
                };
                self.persist_rd_settings_for(sid, share, legacy);
            }
        }

        // 4) 端口扫描进度 / ZMODEM 提示（app 级状态）
        if self.port_scan.poll() {
            dirty = true;
        }
        self.poll_zmodem(cx);
        // AI 面板的后台结果（聊天回复 / 文件选择）也在这里收：改成脏驱动重绘
        // 之后面板 render 不再每帧跑，没人收的话结果要等下一次偶发重绘才出现。
        if self.ai_assistant.poll_async() {
            dirty = true;
        }

        crate::xsrv::counters::UI_PHASE.store(3, std::sync::atomic::Ordering::Relaxed);
        // 5) 限频唤醒：没到间隔就先攒着（ui_paint_pending 跨拍保留，内容不会丢）
        if dirty {
            self.ui_paint_pending = true;
        }
        let interval = self.ui_pump_interval();
        if self.ui_paint_pending && self.ui_wake_at.elapsed() >= interval {
            self.ui_paint_pending = false;
            self.ui_wake_at = std::time::Instant::now();
            self.ui_wakes += 1;
            cx.notify();
        }
        crate::xsrv::counters::UI_PHASE.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// 记录一帧耗时 + 5s 一次的 `[hb-ui]` 汇报（优化前后对照的证据）。
    fn ui_note_frame(&mut self, frame_ms: u32) {
        self.ui_frames += 1;
        self.ui_frame_ms[self.ui_frame_ms_pos] = frame_ms;
        self.ui_frame_ms_pos = (self.ui_frame_ms_pos + 1) % self.ui_frame_ms.len();
        self.ui_frame_ms_len = (self.ui_frame_ms_len + 1).min(self.ui_frame_ms.len());
        self.ui_over_budget = frame_ms >= FRAME_BUDGET_MS;

        if self.ui_last_report.elapsed().as_millis() < UI_REPORT_MS {
            return;
        }
        let (p50, p95, max) = self.ui_frame_percentiles();
        let frames = std::mem::take(&mut self.ui_frames);
        let ticks = std::mem::take(&mut self.ui_ticks);
        let wakes = std::mem::take(&mut self.ui_wakes);
        crate::xsrv::log_line(format_args!(
            "[hb-ui] frames=+{frames} ticks=+{ticks} wakes=+{wakes} build_p50={p50}ms build_p95={p95}ms build_max={max}ms live={} over_budget={}",
            self.ui_has_live_content(),
            self.ui_over_budget,
        ));
        self.ui_last_report = std::time::Instant::now();
    }

    fn ui_frame_percentiles(&self) -> (u32, u32, u32) {
        ui_frame_percentiles_of(&self.ui_frame_ms[..self.ui_frame_ms_len])
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
                    tr(lang, "item.move_into_split"),
                    None,
                    false,
                    |this, _cx| {
                        this.pending_pane_action = Some(PaneAction::MoveTabIntoSplit)
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
                            |this, cx| this.open_remote_desktop(RdTabProtocol::Rdp, cx),
                        ));
                        m = m.item(menu_item(
                            &sub_app,
                            "VNC",
                            None,
                            false,
                            |this, cx| this.open_remote_desktop(RdTabProtocol::Vnc, cx),
                        ));
                        m = m.item(menu_item(
                            &sub_app,
                            "SPICE",
                            None,
                            false,
                            |this, cx| this.open_remote_desktop(RdTabProtocol::Spice, cx),
                        ));
                        m = m.item(menu_item(
                            &sub_app,
                            tr(lang, "menu.x11"),
                            None,
                            false,
                            |this, cx| this.open_x11_screen(cx),
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
                    |this, _cx| this.ai_assistant.toggle(),
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
                |this, _win, _cx| this.ai_assistant.toggle(),
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
                                this.connect_session(&row_id, "", cx);
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
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.connect_session(&conn_id, "", cx);
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
            /// 稳定标识：拖拽落点按它重新解析下标（拖拽期间标签可能被重排）
            key: TabKey,
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
                let key = match tab {
                    TabContent::Terminal(ws) => ws
                        .dock
                        .as_ref()
                        .map(|d| TabKey::Dock(d.entity_id()))
                        .unwrap_or(TabKey::Other),
                    TabContent::RemoteDesktop(e) => TabKey::Remote(e.entity_id()),
                    _ => TabKey::Other,
                };
                Row {
                    label,
                    icon,
                    active: i == self.active,
                    failed: matches!(tab, TabContent::Failed(_)),
                    closable: true,
                    key,
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
                                    // 这只是一次「重排」，不是「放进分屏」：
                                    // 吃掉事件，别让内容区的 on_drop 再跑一遍。
                                    cx.stop_propagation();
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
                    // 拖这个标签（松手落在中间内容区 = 放进分屏）
                    .on_drag(
                        TabDrag {
                            index: idx,
                            key: r.key,
                            label: SharedString::from(r.label.clone()),
                        },
                        |drag: &TabDrag, _offset, _window, cx| {
                            let label = drag.label.clone();
                            cx.new(|_| TabDragPreview { label })
                        },
                    )
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
            TabContent::RemoteDesktop(tab) => {
                // 先把实体克隆出来（Entity 是 Arc，克隆很便宜）：后面的
                // persist_rd_settings_for 需要 &mut self，不能与 match 的
                // &self.tabs 借用共存。
                let panel = tab.clone();
                // 面板自己改了会话设置 → 由 app 落盘（不依赖 self.active）
                if panel.update(cx, |t, _| t.take_settings_dirty()) {
                    let (sid, share, legacy) = {
                        let t = panel.read(cx);
                        (
                            t.profile.session_id.clone(),
                            t.profile.share_dir.clone(),
                            t.profile.legacy_compat,
                        )
                    };
                    self.persist_rd_settings_for(sid, share, legacy);
                }
                panel.update(cx, |t, tcx| t.render_panel(window, tcx))
            }
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

    /// AI 面板右键菜单：复制选中（划选的文字）/ 复制整条消息 / 复制全部对话。
    ///
    /// 触发：消息气泡的右键（ai_assistant.rs 里置 `ai_menu`）。关闭：点菜单外的
    /// 任意位置（整屏遮罩）或菜单项执行后自关。选中文本来自 GPUI 的
    /// `TextSelection::selected_text`（SelectableText 划选的结果）。
    fn ai_context_menu_ui(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (pos, whole, sel_at_open) = self.ai_menu.as_ref()?;
        let pos = *pos;
        let whole = whole.clone();
        // 右键瞬间抓到的划选文字（可能为空 = 没划选）
        let sel_at_open = sel_at_open.trim().to_string();
        let lang = self.i18n.lang;
        let theme = Theme::global(cx).clone();
        let fg = theme.colors.foreground;
        let dim = theme.colors.muted_foreground;

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
            .id("ai-menu")
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
            .shadow_md()
            // 关键：菜单内的点击**绝不穿透**到整屏遮罩——遮罩的 on_mouse_down
            // 会把 ai_menu 清掉，菜单在鼠标抬起前就被移除，on_click 永远不触发
            //（症状正是「菜单弹出但点了没反应」）。左右键都拦。
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_mouse_down(gpui_kit::MouseButton::Right, |_, _, cx| {
                cx.stop_propagation();
            });

        // 复制选中：划选的文字（SelectableText 的结果）。没选中就复制整条，
        // toast 如实说明——不弹空的「无选择」挫败感。
        let whole_sel = whole.clone();
        menu = menu.child(
            item(
                "ai-menu-copy-sel",
                tr(lang, "ai.menu_copy_sel"),
                true,
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                let sel = sel_at_open.clone();
                if sel.is_empty() {
                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(whole_sel.clone()));
                    this.set_toast(tr(lang, "ai.toast_no_sel_copy"));
                } else {
                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(sel));
                    this.set_toast(tr(lang, "ai.toast_sel_copied"));
                }
                this.ai_menu = None;
                cx.notify();
            })),
        );
        // 复制整条消息
        let whole_for_btn = whole.clone();
        menu = menu.child(
            item(
                "ai-menu-copy-whole",
                tr(lang, "ai.menu_copy_whole"),
                true,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(whole_for_btn.clone()));
                this.set_toast(tr(lang, "ai.toast_whole_copied"));
                this.ai_menu = None;
                cx.notify();
            })),
        );
        // 分隔线
        menu = menu.child(
            div()
                .h(px(1.0))
                .my_1()
                .mx_2()
                .bg(theme.colors.border)
                .into_any_element(),
        );
        // 复制全部对话（排查问题时把上下文整个贴出去用）
        menu = menu.child(
            item(
                "ai-menu-copy-all",
                tr(lang, "ai.menu_copy_all"),
                true,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                let all = this
                    .ai_assistant
                    .messages
                    .iter()
                    .map(|m| format!("[{}] {}", m.time, m.content))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(all));
                this.set_toast(tr(lang, "ai.toast_all_copied"));
                this.ai_menu = None;
                cx.notify();
            })),
        );

        // 整屏遮罩：点任意处关闭（左右键都关）。
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .on_mouse_down(gpui_kit::MouseButton::Left, cx.listener(|this, _, _, cx| {
                    this.ai_menu = None;
                    cx.notify();
                }))
                .on_mouse_down(gpui_kit::MouseButton::Right, cx.listener(|this, _, _, cx| {
                    this.ai_menu = None;
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
                    let t = t.read(cx);
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

        // task-22 按需显示：常驻提示（如「远端 X 程序已启动」）。
        // 用户打开对应屏幕后自动清除。
        if let Some(notice) = STATUS_NOTICE.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            left = left.child(
                div()
                    .text_color(theme.colors.warning)
                    .flex_none()
                    .child(format!("⚠ {notice}")),
            );
        }

        // ---- 右区：尺寸 / 光标 / 标签计数 / 会话数 ----
        let mut right = div()
            .h_flex()
            .items_center()
            .gap_3()
            .flex_none()
            .text_size(px(10.0));

        // ZMODEM（rz/sz）传输进行中：状态栏右侧显示进度
        // （会话状态挂在窗格上，这里只取一个在传的；具体实现见 term.rs/zmodem.rs）
        if let Some(text) = self.zmodem_status_text(cx) {
            right = right.child(
                div()
                    .text_color(theme.colors.info)
                    .text_size(px(10.0))
                    .child(text),
            );
        }

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
                .map(|d| dock_items(&d, cx).len())
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

    // ------------------------------------------------------------------
    // 真 Agent：终端执行循环的驱动层（纯状态机在 agent.rs，这里只做 IO）
    // ------------------------------------------------------------------

    /// Agent 发送入口（Agent 开 + 只读闸通过才进入循环）。
    /// 返回 true = 已作为 Agent 任务受理（调用方不再走单轮发送）。
    fn agent_try_start(&mut self, task: String, cx: &mut Context<Self>) -> bool {
        if !self.ai_assistant.agent_mode {
            return false;
        }
        if self.config.ai.read_only {
            // 只读模式：Agent 整个禁用（安全边界 2）
            self.set_toast("只读模式下 Agent 已禁用——请先关闭只读再使用 Agent");
            return true; // 已受理但拒绝执行：不再走单轮发送，避免绕过只读闸
        }
        if self.agent_loop.is_some() {
            self.set_toast("Agent 任务进行中，请等它结束或点「停止」");
            return true;
        }
        let snapshot = self.current_terminal_context(cx).unwrap_or_default();
        let target = self
            .focused_view(cx)
            .or_else(|| self.all_term_views(cx).first().cloned());
        let Some(target) = target else {
            self.set_toast("没有可用的终端窗格，Agent 无法启动");
            return true;
        };
        let (loop_, actions) = crate::agent::AgentLoop::start(task, snapshot);
        self.agent_loop = Some(loop_);
        self.agent_target = Some(target);
        self.agent_wait_since = None;
        self.ai_assistant.agent_busy = true;
        self.agent_apply(actions, cx);
        true
    }

    /// 执行状态机给出的动作序列。
    fn agent_apply(
        &mut self,
        actions: Vec<crate::agent::AgentAction>,
        cx: &mut Context<Self>,
    ) {
        use crate::agent::AgentAction as A;
        for action in actions {
            match action {
                A::Say(text) => {
                    self.ai_assistant.push_agent_message(text);
                    cx.notify();
                }
                A::RunCommand(cmd) => {
                    let sent = self
                        .agent_target
                        .as_ref()
                        .map(|view| {
                            view.update(cx, |v, _| {
                                if v.is_dead() {
                                    return false;
                                }
                                v.write_input(format!("{cmd}\r").into_bytes());
                                true
                            })
                        })
                        .unwrap_or(false);
                    if sent {
                        self.agent_wait_since = Some(std::time::Instant::now());
                    } else {
                        // 锁定的窗格死亡 → 终止本轮（安全边界 6）
                        let actions = self
                            .agent_loop
                            .as_mut()
                            .map(|l| l.next(crate::agent::AgentEvent::TargetDied));
                        if let Some(actions) = actions {
                            self.agent_apply(actions, cx);
                        }
                        return;
                    }
                }
                A::AskConfirm(cmd) => {
                    // 危险命令：整轮暂停，弹三选一确认卡（安全边界 1）
                    self.ai_assistant.agent_confirm_cmd = Some(cmd);
                    cx.notify();
                }
                A::RequestModelTurn(_prompt) => {
                    // 组模型历史并发起后台请求（结果由 agent_poll 消费）
                    let Some(loop_) = self.agent_loop.as_ref() else {
                        continue;
                    };
                    let history: Vec<crate::ai::AiMessage> = loop_
                        .chat_history()
                        .into_iter()
                        .map(|h| match h.role.as_str() {
                            "assistant" => crate::ai::AiMessage::assistant(h.content),
                            _ => crate::ai::AiMessage::user(h.content),
                        })
                        .collect();
                    let cfg = self.config.ai.clone();
                    let agent_mode = true;
                    let (tx, rx) = std::sync::mpsc::channel();
                    self.agent_rx = Some(rx);
                    std::thread::spawn(move || {
                        let result = crate::ai::chat(&cfg, agent_mode, &history);
                        let _ = tx.send(result);
                    });
                }
                A::Finish(summary) => {
                    self.ai_assistant.push_agent_message(summary);
                    self.agent_cleanup();
                    cx.notify();
                }
                A::Abort(reason) => {
                    self.ai_assistant.push_error(reason);
                    self.agent_cleanup();
                    cx.notify();
                }
            }
        }
    }

    /// Agent 收尾：清循环与面板标志（summary/错误消息已由调用方写入）。
    fn agent_cleanup(&mut self) {
        self.agent_loop = None;
        self.agent_target = None;
        self.agent_wait_since = None;
        self.agent_rx = None;
        self.ai_assistant.agent_busy = false;
        self.ai_assistant.agent_status = None;
        self.ai_assistant.agent_confirm_cmd = None;
        self.ai_assistant.agent_confirm = None;
        self.ai_assistant.agent_stop = false;
    }

    /// 每帧推进 Agent 循环（确认选择 / 停止 / 模型结果 / 输出静默）。
    /// 先把循环 `take` 出来——`agent_apply` 需要 `&mut self`，不能让
    /// `loop_` 的可变借用跨过它；每个分支在调用 `agent_apply` 前放回。
    fn agent_poll(&mut self, cx: &mut Context<Self>) {
        let Some(mut loop_) = self.agent_loop.take() else {
            return;
        };

        // 1) 危险命令确认（三选一）
        if let Some(choice) = self.ai_assistant.agent_confirm.take() {
            self.ai_assistant.agent_confirm_cmd = None;
            let actions = loop_.next(crate::agent::AgentEvent::Confirm {
                run: choice == crate::ai_assistant::AgentChoice::Run,
            });
            self.agent_loop = Some(loop_);
            self.agent_apply(actions, cx);
            return;
        }
        // 2) 用户停止
        if self.ai_assistant.agent_stop {
            self.ai_assistant.agent_stop = false;
            let actions = loop_.next(crate::agent::AgentEvent::Stop);
            self.agent_loop = Some(loop_);
            self.agent_apply(actions, cx);
            return;
        }
        // 3) 模型回复
        if self.agent_rx.is_some() {
            let got = self.agent_rx.as_ref().unwrap().try_recv().ok();
            if let Some(result) = got {
                self.agent_rx = None;
                match result {
                    Ok(reply_text) => {
                        // 双格式解析（JSON 优先，bash 块/[DONE] 降级）
                        let plan = crate::ai::parse_agent_reply(&reply_text);
                        let actions =
                            loop_.next(crate::agent::AgentEvent::ModelReply { plan });
                        self.agent_loop = Some(loop_);
                        self.agent_apply(actions, cx);
                    }
                    Err(e) => {
                        // 模型请求失败 → 终止本轮（错误消息进面板）
                        self.ai_assistant
                            .push_error(format!("Agent 模型请求失败：{e}"));
                        self.agent_cleanup();
                        cx.notify();
                    }
                }
                return;
            }
        }
        // 4) 输出静默结算：固定 800ms 会把慢命令（find / 大文件操作 / yum）
        //    的输出半途截断喂给模型。改为跟随输出活动：快照不变 ≥1.2s 且写入
        //    ≥0.4s 才结算；20s 硬上限兜底（持续输出型命令如 tail -f 截取
        //    部分输出，由系统提示词告知模型可能不完整）。
        if let Some(since) = self.agent_wait_since {
            let snapshot = self.agent_capture_output(cx);
            let now = std::time::Instant::now();
            let changed = self
                .agent_last_snapshot
                .as_ref()
                .map_or(true, |(s, _)| *s != snapshot);
            if changed {
                self.agent_last_snapshot = Some((snapshot, now));
            }
            let since_change = self
                .agent_last_snapshot
                .as_ref()
                .map(|(_, t)| now.saturating_duration_since(*t))
                .unwrap_or(std::time::Duration::ZERO);
            let since_write = now.saturating_duration_since(since);
            if crate::agent::agent_should_settle(since_write, since_change) {
                self.agent_wait_since = None;
                self.agent_last_snapshot = None;
                let output = self.agent_capture_output(cx);
                let actions = loop_.next(crate::agent::AgentEvent::CmdOutput { output });
                self.agent_loop = Some(loop_);
                self.agent_apply(actions, cx);
                return;
            }
        }
        // 5) 状态行同步（有变化才 notify——避免无谓空转）
        let new_status = loop_.status_line();
        if self.ai_assistant.agent_status.as_deref() != Some(new_status.as_str()) {
            self.ai_assistant.agent_status = Some(new_status);
            cx.notify();
        }
        // Running（等终端输出，≤800ms）→ 全速结算；Thinking（等模型，可达
        // 数十秒）→ 150ms 节流拍（只做 try_recv 级 poll，等效 ui_pump 的
        // 慢节奏，不会复现 60fps 空转）。节流拍经 agent_poll_if_active 自续，
        // 循环结束自动停止。
        let now = std::time::Instant::now();
        let due = match loop_.state {
            // Running（等输出静默）用 100ms 节拍：静默阈值 1.2s 下，100ms
            // 粒度的结算延迟无感，且避免等待期间 60fps 空转。
            crate::agent::AgentState::Running { .. } => {
                now.saturating_duration_since(self.agent_last_poll_notify)
                    >= std::time::Duration::from_millis(100)
            }
            crate::agent::AgentState::Thinking { .. } => {
                now.saturating_duration_since(self.agent_last_poll_notify)
                    >= std::time::Duration::from_millis(150)
            }
            _ => false,
        };
        if due {
            self.agent_last_poll_notify = now;
            cx.notify();
        }
        self.agent_loop = Some(loop_);
    }

    /// 从**锁定的**终端窗格捕获最近输出（不用 focused——目标已锁定）。
    fn agent_capture_output(&self, cx: &App) -> String {
        let Some(target) = self.agent_target.as_ref() else {
            return String::new();
        };
        target.read(cx).context_text(
            self.config.ai.context_lines,
            self.config.ai.context_max_line_len,
        )
    }

    /// Agent 每帧驱动入口。
    fn agent_poll_if_active(&mut self, cx: &mut Context<Self>) {
        if self.agent_loop.is_some() {
            self.agent_poll(cx);
        }
    }

    fn ai_panel_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.ai_assistant.open {
            return None;
        }
        let ai = self.config.ai.clone();
        self.ai_assistant.render(&ai, self.i18n.lang, window, cx)
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
        // 帧耗时统计的起点（[hb-ui] 报的是"状态更新 + 元素树构建"这段，
        // 布局与绘制在 render 返回之后，由 [hb] 的 CPU 口径覆盖）
        self.ui_frame_at = std::time::Instant::now();

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
            // 脏驱动重绘的唯一调度者（终端/远端/连接/扫描的廉价排空 + 唤醒）
            self.spawn_ui_pump(window, cx);
        }

        let _ = self.poll_connecting();
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
                PaneAction::MoveTabIntoSplit => self.move_tab_into_split(window, cx),
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
            if self.agent_loop.is_some() {
                // Agent 循环进行中：输入走「追加任务/追问」语义——作为普通消息
                // 追加进对话（不中断循环）。v1 简化：提示等当前任务完成。
                self.set_toast("Agent 任务进行中：请等它结束或点「停止」");
            } else if self.ai_assistant.agent_mode
                && self.agent_try_start(self.ai_assistant.input.trim().to_string(), cx)
            {
                // Agent 已受理（含只读拒绝 / 无窗格拒绝的提示）。
                // 清空输入框与内部缓冲（与单轮 send 的清空行为一致）。
                if let Some(s) = &self.ai_assistant.input_state {
                    s.update(cx, |s, cx| s.set_value(String::new(), window, cx));
                }
                self.ai_assistant.input.clear();
                cx.notify();
            } else if self.ai_assistant.waiting {
                // 上一轮还没回来：不能静默吞掉这次点击，否则用户以为"发送没反应"
                self.set_toast("AI 正在回复中：请稍候，或点面板里的「停止」取消");
            } else {
                let context = self.current_terminal_context(cx);
                let ai = self.config.ai.clone();
                self.ai_assistant.send(&ai, context, window, cx);
            }
        }

        // AI 面板的「发送到终端」请求
        if let Some((cmd, dangerous)) = self.ai_assistant.command_request.take() {
            // 诊断：发送到终端的每一次决策都落日志（用户报「第一次正常、第二次
            // 静默失败」——静态推不出唯一分支，让日志说话）。
            crate::xsrv::log_line(format_args!(
                "[ai-send] taken cmd={cmd:?} dangerous={dangerous}"
            ));
            if self.config.ai.read_only {
                self.set_toast("只读模式：写命令已被禁用");
            } else if dangerous {
                self.set_toast(format!("⚠ 危险命令已拦截: {cmd}"));
            } else {
                // 目标选择：优先焦点所在的终端窗格；拿不到（用户正盯着远程
                // 桌面/X11 格——此时 focused_term 已被 track_terminal_focus 清空，
                // 或应用启动后从没点过终端）就退到**当前活动标签的第一个终端**。
                // 之前两级都没有时是静默失败，用户视角就是"按钮没反应"。
                let target = self.focused_view(cx).or_else(|| {
                    // 兜底升级为**全仓任意开着的终端窗格**：之前只找当前活动
                    // 标签——用户在 X11/远程桌面标签上看屏幕时，开着终端的
                    // SSH 标签不在 active 里，照样找不到（用户实测：终端明明
                    // 开着，发送还是"静默失败"）。any 终端都比失败强。
                    let any = self.all_term_views(cx).first().cloned();
                    crate::xsrv::log_line(format_args!(
                        "[ai-send] focused 无 → 兜底 any 终端 = {}",
                        any.is_some()
                    ));
                    any
                });
                if let Some(view) = target {
                    let sent = view.update(cx, |v, _| {
                        if v.is_dead() {
                            return false;
                        }
                        v.write_input(format!("{cmd}\r").into_bytes());
                        true
                    });
                    crate::xsrv::log_line(format_args!("[ai-send] sent={sent}"));
                    if sent {
                        self.set_toast("命令已发送到终端");
                    } else {
                        self.set_toast("终端已断开，命令未发送");
                    }
                } else {
                    crate::xsrv::log_line(format_args!("[ai-send] 无任何终端窗格"));
                    self.set_toast("没有可用的终端窗格，命令未发送");
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
            .on_action(cx.listener(|this, _: &ToggleRdp, _, cx| {
                this.open_remote_desktop(RdTabProtocol::Rdp, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleBrowser, _, _| this.open_browser("about:blank")))
            .on_action(cx.listener(|this, _: &ToggleAi, _, _| {
                this.ai_assistant.toggle()
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
                            // **只有内容区**是拖放目标：标签条不算。否则「拖标签
                            // 重排」松手时会被劫持成「放进分屏」（而且用的是重排
                            // 前的旧下标，会搬错标签）。
                            .child(
                                // 必须是 flex 容器：center_content 的根（终端的
                                // terminal_area / dock）用 `.flex_1()` 拿高度，
                                // 块级父容器里 flex 属性无效 → 高度塌缩 → 整个
                                // dock 白屏（D2 修复时引入的回归，诊断日志
                                // canvas=425x0 实锤）。
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .min_h_0()
                                    .v_flex()
                                    .on_drop::<TabDrag>(cx.listener(
                                        |this, drag: &TabDrag, window, cx| {
                                            // 用**稳定标识**重新解析下标：拖拽期间
                                            // 标签可能已被重排，index 早已过期。
                                            let Some(ix) = this.resolve_tab_key(drag.key) else {
                                                return;
                                            };
                                            this.move_tab_into_split_at(ix, window, cx);
                                        },
                                    ))
                                    .child(self.center_content_ui(window, cx)),
                            ),
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
        // AI 面板右键菜单：复制选中 / 复制整条消息 / 复制全部对话
        if let Some(el) = self.ai_context_menu_ui(cx) {
            dialogs.push(el);
        }
        for d in dialogs {
            root = root.child(d);
        }

        // 真 Agent 循环驱动（每帧推进：确认 / 停止 / 模型结果 / 输出静默结算）。
        // 只在循环进行中做事；调度本身（续帧策略）在下方 task-10 的段落，未动。
        self.agent_poll_if_active(cx);

        // ---- 帧调度：脏驱动重绘 ----
        //
        // 旧实现：无条件 `request_animation_frame()` —— 60fps 全 UI 重绘，
        // 空闲也把主线程占满（`[hb] cpu=+4s/5s`），X11 一忙输入就被饿死
        // （实测 5~7fps）。现在：
        //   · 数据由 ui_pump 任务排空（终端/远端/连接/扫描，见 spawn_ui_pump），
        //     排到新内容才 notify —— 没有内容就没有帧；
        //   · 本帧处理了脏内容（刚到的远端帧、布局变化…）就再补一帧把它画出来；
        //     上一帧超预算时不补，交给泵按退让节奏唤醒，避免"越忙越卡"的正反馈。
        //
        // 保留 `live` 判定只是为了让没有活动标签时连这一下都省掉。
        let live = self.ui_has_live_content();
        let frame_ms = self.ui_frame_at.elapsed().as_millis() as u32;
        let dirty = crate::xsrv::counters::take_ui_dirty();
        self.ui_note_frame(frame_ms);
        if live && wants_extra_frame(FrameSignals { dirty, frame_ms }) {
            window.request_animation_frame();
        }

        let _ = lang;
        root
    }
}


/* ----------------------------------------------------------------------
 * 测试
 * ------------------------------------------------------------------- */

#[cfg(test)]
mod tests {
    // 显式导入，**不要** `use super::*`：本文件引入了 `gpui_kit::*` 通配，
    // 通配里的名字会盖掉内置的 `#[test]` 属性宏，展开时递归爆掉
    //（"recursion limit reached while expanding #[test]"）。
    use super::{
        ui_frame_percentiles_of, wake_interval_ms, wants_extra_frame, FrameSignals,
        FRAME_BUDGET_MS, UI_WAKE_ACTIVE_MS, UI_WAKE_BACKOFF_MS, UI_WAKE_IDLE_MS,
    };

    /// 帧预算（编译期约束）：退让节奏必须比活跃节奏慢、但比空闲兜底快。
    #[test]
    fn wake_intervals_are_ordered() {
        assert!(UI_WAKE_ACTIVE_MS < UI_WAKE_BACKOFF_MS);
        assert!(UI_WAKE_BACKOFF_MS < UI_WAKE_IDLE_MS);
    }

    /// 空闲（没有脏内容）时不得再请求下一帧 —— 这是 CPU 从 80% 掉下来的关键。
    #[test]
    fn idle_never_requests_another_frame() {
        assert!(!wants_extra_frame(FrameSignals {
            dirty: false,
            frame_ms: 3,
        }));
        // 没有活动内容 → 慢节奏兜底（只维持隐藏面板排空/连接状态）
        assert_eq!(wake_interval_ms(false, false), UI_WAKE_IDLE_MS);
        // 即使上一帧超预算，没有活动内容也走同样的兜底间隔
        assert_eq!(wake_interval_ms(false, true), UI_WAKE_IDLE_MS);
    }

    /// 有脏内容 → 补一帧把内容画出来；活跃期用 8ms 节奏（回显/远端画面跟手）。
    #[test]
    fn dirty_content_requests_exactly_one_more_frame() {
        assert!(wants_extra_frame(FrameSignals {
            dirty: true,
            frame_ms: 3,
        }));
        assert_eq!(wake_interval_ms(true, false), UI_WAKE_ACTIVE_MS);
    }

    /// 超预算时不再"画完立刻再画"，泵也退让 —— 避免"越忙越卡"的正反馈。
    #[test]
    fn over_budget_frames_back_off_instead_of_queueing() {
        assert!(!wants_extra_frame(FrameSignals {
            dirty: true,
            frame_ms: FRAME_BUDGET_MS,
        }));
        assert!(!wants_extra_frame(FrameSignals {
            dirty: true,
            frame_ms: 120,
        }));
        assert_eq!(wake_interval_ms(true, true), UI_WAKE_BACKOFF_MS);
        // 边界：刚好在预算内仍然续帧
        assert!(wants_extra_frame(FrameSignals {
            dirty: true,
            frame_ms: FRAME_BUDGET_MS - 1,
        }));
    }

    /// `[hb-ui]` 的百分位口径：空窗口归零、单样本三值相同、排序后取分位
    /// （对照证据的读数函数——p95 应显著高于 p50 才说明有掉帧/超预算）。
    #[test]
    fn frame_percentiles_are_ordered_and_empty_is_zero() {
        assert_eq!(ui_frame_percentiles_of(&[]), (0, 0, 0));
        assert_eq!(ui_frame_percentiles_of(&[7]), (7, 7, 7));
        // 乱序输入（环形缓冲里的真实顺序）也应正确排序
        let (p50, p95, max) = ui_frame_percentiles_of(&[30, 2, 8, 16, 4, 24]);
        // 排序后 [2,4,8,16,24,30]：p50 取下标 round(5*0.5)=3 → 16，p95 → 30
        assert_eq!((p50, p95, max), (16, 30, 30));
        // 典型空闲画面：全部 0ms → 三值都是 0（CPU 归零的证据形态）
        assert_eq!(ui_frame_percentiles_of(&[0; 64]), (0, 0, 0));
    }
}