//! 统一标签页模型。
//!
//! 主界面的标签栏同时承载多种会话与工具面板，与终端工作区平级：
//! - `Terminal`：**终端工作区**（内部是一个 dock 区域，可任意嵌套分屏 / 标签组）
//! - `Browser`：内置浏览器（独立 WebView2 子窗口）
//! - `RemoteDesktop`：VNC / RDP / SPICE 远程桌面
//! - `LocalFiles`：本地文件浏览器（宽栏）
//! - `Macro`：键宏管理器
//! - `PortScan`：端口扫描器
//!
//! SFTP 不在标签页里：它是右侧停靠面板（见 `sftp.rs`）。
//!
//! ## 终端工作区与 dock
//!
//! 终端不再由本工程自己写分屏树：每个工作区持有一个 `gpui-base` 的
//! [`DockArea`]（外观由 `gpui-component` 的 [`DockSkin`] 提供），于是分屏、
//! 标签组、拖到边缘分裂、单格缩放、整树 dump/load 都由 dock 负责。
//!
//! 面板是 [`TerminalView`]（见 `terminal_view.rs`）。dock 需要 `&mut Window`
//! 才能创建，而「新建终端」常从菜单/快捷键发起（那时没有 window），因此新面板
//! 先排进 `pending`，在首帧渲染时统一装进 dock。

use crate::browser::BrowserTab;
use crate::filebrowser::FileBrowser;
use crate::rdp::RemoteDesktopTab;
use crate::terminal_view::TerminalView;
use gpui_kit::base::dock::{DockArea, DockPlacement};
use gpui_kit::component::dock::DockSkin;
use gpui_kit::{App, Entity};
use std::rc::Rc;

/// 标签页类别。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TabType {
    Terminal,
    Browser,
    RemoteDesktop,
    LocalFiles,
    Macro,
    PortScan,
}

impl TabType {
    /// 标签栏左侧图标。
    pub fn icon(&self) -> &'static str {
        match self {
            TabType::Terminal => "🖥",
            TabType::Browser => "🌐",
            TabType::RemoteDesktop => "🖥",
            TabType::LocalFiles => "📁",
            TabType::Macro => "⌨",
            TabType::PortScan => "🔎",
        }
    }

    /// 状态栏 / 提示用的中文名。
    pub fn label(&self) -> &'static str {
        match self {
            TabType::Terminal => "终端",
            TabType::Browser => "浏览器",
            TabType::RemoteDesktop => "远程桌面",
            TabType::LocalFiles => "本地文件",
            TabType::Macro => "键宏",
            TabType::PortScan => "端口扫描",
        }
    }
}

/// dock 里的一个面板。
///
/// 分屏不再只收终端：远程桌面（RDP/VNC/SPICE/X11）标签也能被拖进分屏格，
/// 与终端同屏并排。以枚举区分类型，调用方按需 match。
#[derive(Clone)]
pub enum DockPanel {
    /// 终端面板
    Terminal(Entity<TerminalView>),
    /// 远程桌面面板（RDP/VNC/SPICE/X11）
    Remote(Entity<RemoteDesktopTab>),
}

impl DockPanel {
    /// 面板标题（标签栏 / 状态栏用）。
    pub fn title(&self, cx: &App) -> String {
        match self {
            DockPanel::Terminal(t) => t.read(cx).title(),
            DockPanel::Remote(t) => t.read(cx).title(),
        }
    }
}

/// 取出一个 dock 区域里的**所有**面板（终端 + 远程桌面，按布局顺序）。
///
/// 这是 `dock_panels` 的泛化版本：需要一个能同时容纳多种面板类型的调用点
/// （关闭/分屏/通知/FPS 统计）用它。
pub fn dock_items(dock: &Entity<DockArea>, cx: &App) -> Vec<DockPanel> {
    let area = dock.read(cx);
    let Some(tree) = area.layout(DockPlacement::Center) else {
        return Vec::new();
    };
    tree.panels()
        .filter_map(|id| area.panel(id))
        .filter_map(|handle| {
            if let Ok(view) = handle.view().downcast::<TerminalView>() {
                return Some(DockPanel::Terminal(view));
            }
            if let Ok(view) = handle.view().downcast::<RemoteDesktopTab>() {
                return Some(DockPanel::Remote(view));
            }
            None
        })
        .collect()
}

/// 取出一个 dock 区域里的所有终端面板（按布局顺序）。
///
/// dock 是「有哪些面板」的唯一真相：面板实体以 `Arc<dyn PanelView>` 形式存在
/// dock 里，这里通过 `PanelView::view() -> AnyView` 反查回具体类型。
pub fn dock_panels(dock: &Entity<DockArea>, cx: &App) -> Vec<Entity<TerminalView>> {
    dock_items(dock, cx)
        .into_iter()
        .filter_map(|p| match p {
            DockPanel::Terminal(t) => Some(t),
            DockPanel::Remote(_) => None,
        })
        .collect()
}

/// 待装入 dock 的面板。
///
/// 「新建终端」常常从菜单/快捷键发起，那时既没有 `&mut Window`（dock 创建需要），
/// 也不方便要一个 `&mut App`（面板实体创建需要）。所以先把**意图**排进工作区，
/// 首帧渲染时统一变成 [`TerminalView`] 实体再装进 dock。
pub enum PendingPane {
    /// 已经拿到终端状态机（本地 / Telnet / 命令 / 已连上的 SSH）
    Ready(crate::term::TermPane),
    /// SSH 后台连接中（结果经通道送回）
    Connecting {
        title: String,
        rx: std::sync::mpsc::Receiver<anyhow::Result<crate::term::TermPane>>,
    },
}

/// 一个终端工作区。
pub struct TerminalWorkspace {
    /// dock 区域；由首帧渲染惰性创建（创建需要 `&mut Window`）
    pub dock: Option<Entity<DockArea>>,
    /// 外观句柄（改面板样式用，例如把标签栏改成常显）
    pub skin: Option<Rc<DockSkin>>,
    /// 待装入 dock 的终端（首帧消费掉）
    pub pending: Vec<PendingPane>,
}

impl TerminalWorkspace {
    /// 空工作区（还没有终端）。
    pub fn new() -> Self {
        Self {
            dock: None,
            skin: None,
            pending: Vec::new(),
        }
    }

    /// 带一个终端的工作区。
    pub fn with_pane(pane: crate::term::TermPane) -> Self {
        Self {
            dock: None,
            skin: None,
            pending: vec![PendingPane::Ready(pane)],
        }
    }

    /// 带一个「连接中」终端的工作区（SSH）。
    pub fn with_connecting(
        title: String,
        rx: std::sync::mpsc::Receiver<anyhow::Result<crate::term::TermPane>>,
    ) -> Self {
        Self {
            dock: None,
            skin: None,
            pending: vec![PendingPane::Connecting { title, rx }],
        }
    }

    pub fn push_pane(&mut self, pane: crate::term::TermPane) {
        self.pending.push(PendingPane::Ready(pane));
    }

    /// 把「要开一个终端」记在工作区上（还没装进 dock）。
    pub fn queue(&mut self, pending: PendingPane) {
        self.pending.push(pending);
    }

    /// 该工作区是否还没有任何终端（dock 建好后用它决定关标签）。
    pub fn is_empty(&self, cx: &App) -> bool {
        match &self.dock {
            Some(dock) => dock_panels(dock, cx).is_empty(),
            None => self.pending.is_empty(),
        }
    }

    /// 标签栏显示的标题：取第一个终端面板的标题（分屏时补一个格数）。
    pub fn title(&self, cx: &App) -> String {
        let Some(dock) = &self.dock else {
            return "终端".to_string();
        };
        let panels = dock_panels(dock, cx);
        let base = panels
            .first()
            .map(|v| v.read(cx).title())
            .unwrap_or_else(|| "终端".to_string());
        // 格数要把**远程桌面**面板也算上（与状态栏一致）：1 终端 + 1 远程时
        // 标签写「1 格」、状态栏写「2 格」会让人以为分屏没生效。
        let cells = dock_items(dock, cx).len();
        if cells > 1 {
            format!("{base} ⟨{cells} 格⟩")
        } else {
            base
        }
    }

    /// 该工作区里是否已经有「死了」的终端（标签栏图标用）。
    pub fn any_dead(&self, cx: &App) -> Option<bool> {
        let dock = self.dock.as_ref()?;
        let panels = dock_panels(dock, cx);
        if panels.is_empty() {
            return None;
        }
        Some(panels.iter().any(|v| v.read(cx).is_dead()))
    }
}

impl Default for TerminalWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

/// 标签页内容。
pub enum TabContent {
    /// SSH 连接中（结果经线程送回）
    Connecting {
        title: String,
        rx: std::sync::mpsc::Receiver<anyhow::Result<crate::term::TermPane>>,
    },
    /// 终端工作区（内部是 dock：分屏 / 标签组 / 缩放 / 持久化）
    Terminal(TerminalWorkspace),
    /// 连接 / 认证失败（保留 tab 供查看错误）
    Failed(String),
    /// 内置浏览器
    Browser(BrowserTab),
    /// 远程桌面。
    ///
    /// 持有**实体**（而不是值）：这样它既能在标签里渲染，也能被直接装进
    /// 终端工作区的 dock 成为分屏格——「把远程桌面标签拖进分屏」正是靠
    /// 把这里的实体交给 `DockArea::add_panel_view` 实现的。
    RemoteDesktop(Entity<RemoteDesktopTab>),
    /// 本地文件浏览器（宽栏）
    LocalFiles(FileBrowser),
    /// 键宏管理器（面板本体在 `RustTermApp::macros`，这里只是「视图入口」）
    Macro,
    /// 端口扫描器（面板本体在 `RustTermApp::port_scan`）
    PortScan,
}

impl TabContent {
    pub fn tab_type(&self) -> TabType {
        match self {
            TabContent::Connecting { .. } | TabContent::Terminal(_) | TabContent::Failed(_) => {
                TabType::Terminal
            }
            TabContent::Browser(_) => TabType::Browser,
            TabContent::RemoteDesktop(_) => TabType::RemoteDesktop,
            TabContent::LocalFiles(_) => TabType::LocalFiles,
            TabContent::Macro => TabType::Macro,
            TabContent::PortScan => TabType::PortScan,
        }
    }

    /// 标签栏显示的标题。
    pub fn title(&self, cx: &App) -> String {
        match self {
            TabContent::Connecting { title, .. } => title.clone(),
            TabContent::Terminal(ws) => ws.title(cx),
            TabContent::Failed(_) => "连接失败".to_string(),
            TabContent::Browser(tab) => tab.display_title(),
            TabContent::RemoteDesktop(tab) => tab.read(cx).title(),
            TabContent::LocalFiles(_) => "本地文件".to_string(),
            TabContent::Macro => "键宏".to_string(),
            TabContent::PortScan => "端口扫描".to_string(),
        }
    }

    /// 终端工作区（仅 Terminal 返回）。
    pub fn workspace(&self) -> Option<&TerminalWorkspace> {
        match self {
            TabContent::Terminal(ws) => Some(ws),
            _ => None,
        }
    }

    pub fn workspace_mut(&mut self) -> Option<&mut TerminalWorkspace> {
        match self {
            TabContent::Terminal(ws) => Some(ws),
            _ => None,
        }
    }

    /// 释放底层资源（关闭标签时调用，确保 WebView 子窗口被销毁）。
    pub fn shutdown(&mut self) {
        if let TabContent::Browser(tab) = self {
            tab.shutdown();
        }
    }
}
