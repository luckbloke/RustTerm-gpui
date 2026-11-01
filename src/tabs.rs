//! 统一标签页模型。
//!
//! 主界面的标签栏同时承载多种会话与工具面板，与本地 / SSH 终端平级：
//! - `Terminal`：终端（**可以是分屏**，见 [`TerminalTab`]）
//! - `Browser`：内置浏览器（独立 WebView2 子窗口）
//! - `RemoteDesktop`：VNC / RDP / SPICE 远程桌面
//! - `LocalFiles`：本地文件浏览器（宽栏）
//! - `Macro`：键宏管理器
//! - `PortScan`：端口扫描器
//!
//! 键宏与端口扫描原先是「模态对话框」，只能盖在终端上、关掉就没了；
//! 现在两者都提升为**标签页**，可以和其它会话并排存在、随时切回。
//!
//! SFTP 不在标签页里：它是右侧停靠面板（见 `sftp.rs`）。
//!
//! 连接中的 SSH 标签用 `Connecting` 表示（结果经独立线程送回），
//! 失败用 `Failed` 保留标签供查看错误；**分屏**新增的 SSH 窗格走
//! [`PaneState::Connecting`] / [`PaneState::Failed`]，语义一致但不占整张标签。

use crate::browser::BrowserTab;
use crate::filebrowser::FileBrowser;
use crate::rdp::RemoteDesktopTab;
use crate::split::{next_pane_id, PaneId, SplitTree};
use crate::term::TermPane;

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

// ---------------------------------------------------------------------------
// 终端标签页：多窗格 + 分屏布局
// ---------------------------------------------------------------------------

/// 一个分屏窗格的状态。
pub enum PaneState {
    /// 已连上（本地 / SSH / Telnet / 命令）
    Live(TermPane),
    /// SSH 分屏连接中：结果由后台线程经通道送回，每帧 [`TerminalTab::poll`]
    Connecting {
        title: String,
        rx: std::sync::mpsc::Receiver<anyhow::Result<TermPane>>,
    },
    /// 分屏连接失败（保留占位与错误信息，可关闭）
    Failed { title: String, msg: String },
}

/// 窗格槽位：稳定 id + 状态。
pub struct PaneSlot {
    pub id: PaneId,
    pub state: PaneState,
}

/// 一个终端标签页：若干窗格 + 一棵分屏树。
///
/// 窗格顺序（`panes`）只是创建顺序；屏幕上的位置一律以 `layout` 为准。
/// `active` 是**窗格 id**，不是下标 —— 关掉中间一个窗格不会让活动窗格串位。
pub struct TerminalTab {
    pub panes: Vec<PaneSlot>,
    pub active: PaneId,
    pub layout: SplitTree,
}

impl TerminalTab {
    /// 用第一个窗格开一个不分屏的终端标签。
    pub fn new(pane: TermPane) -> Self {
        Self::with_slot(PaneState::Live(pane))
    }

    /// 用任意初始状态开标签（SSH 分屏失败后重建、或直接以「连接中」开标签）。
    pub fn with_slot(state: PaneState) -> Self {
        let id = next_pane_id();
        Self {
            panes: vec![PaneSlot { id, state }],
            active: id,
            layout: SplitTree::new(id),
        }
    }

    pub fn active_id(&self) -> PaneId {
        self.active
    }

    /// 活动窗格（连接中/失败时返回 None）。
    pub fn active_pane(&self) -> Option<&TermPane> {
        self.pane(self.active)
    }

    pub fn active_pane_mut(&mut self) -> Option<&mut TermPane> {
        self.pane_mut(self.active)
    }

    pub fn pane(&self, id: PaneId) -> Option<&TermPane> {
        match self.slot(id)?.state {
            PaneState::Live(p) => Some(p),
            _ => None,
        }
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut TermPane> {
        match &mut self.slot_mut(id)?.state {
            PaneState::Live(p) => Some(p),
            _ => None,
        }
    }

    pub fn slot(&self, id: PaneId) -> Option<&PaneSlot> {
        self.panes.iter().find(|s| s.id == id)
    }

    pub fn slot_mut(&mut self, id: PaneId) -> Option<&mut PaneSlot> {
        self.panes.iter_mut().find(|s| s.id == id)
    }

    /// 布局顺序的窗格 id。
    pub fn leaf_ids(&self) -> Vec<PaneId> {
        self.layout.leaves()
    }

    /// 是否分屏（多于一个窗格）。
    pub fn is_split(&self) -> bool {
        self.layout.len() > 1
    }

    pub fn set_active(&mut self, id: PaneId) {
        if self.layout.contains(id) {
            self.active = id;
        }
    }

    /// 追加一个窗格并挂到 `target` 旁边，返回新窗格 id。
    ///
    /// `axis` 横向 = 左右并排（`before` = 放左边），纵向 = 上下叠放（`before` = 放上边）。
    pub fn split_with(
        &mut self,
        target: PaneId,
        axis: gpui_kit::Axis,
        state: PaneState,
        before: bool,
    ) -> Option<PaneId> {
        let id = next_pane_id();
        self.panes.push(PaneSlot { id, state });
        if self.layout.split(target, axis, id, before) {
            self.active = id;
            Some(id)
        } else {
            self.panes.retain(|s| s.id != id);
            None
        }
    }

    /// 关掉一个窗格。返回 false 表示这是最后一个窗格（调用方应关掉整张标签）。
    pub fn remove(&mut self, id: PaneId) -> bool {
        if self.layout.len() <= 1 {
            return false;
        }
        if !self.layout.remove(id) {
            return false;
        }
        self.panes.retain(|s| s.id != id);
        if self.active == id {
            // 优先接管「布局里的下一个」，没有就退回第一个
            self.active = self.leaf_ids().first().copied().unwrap_or(self.active);
        }
        true
    }

    /// 轮询连接中的窗格，返回是否有变化。
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        for slot in self.panes.iter_mut() {
            let done = match &slot.state {
                PaneState::Connecting { rx, .. } => Some(match rx.try_recv() {
                    Ok(Ok(pane)) => Ok(pane),
                    Ok(Err(e)) => Err(format!("{e:#}")),
                    Err(std::sync::mpsc::TryRecvError::Empty) => continue,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        Err("连接线程异常退出".to_string())
                    }
                }),
                _ => None,
            };
            if let Some(result) = done {
                let title = match &slot.state {
                    PaneState::Connecting { title, .. } => title.clone(),
                    _ => "终端".to_string(),
                };
                slot.state = match result {
                    Ok(pane) => PaneState::Live(pane),
                    Err(msg) => PaneState::Failed { title, msg },
                };
                changed = true;
            }
        }
        changed
    }

    /// 标签栏显示的名字：活动窗格优先，分屏时补一个格数提示。
    pub fn title(&self) -> String {
        let base = match self.slot(self.active).map(|s| &s.state) {
            Some(PaneState::Live(p)) => p.title.clone(),
            Some(PaneState::Connecting { title, .. }) => format!("{title}（连接中）"),
            Some(PaneState::Failed { title, .. }) => format!("{title}（连接失败）"),
            None => "终端".to_string(),
        };
        if self.is_split() {
            format!("{base} ⟨{} 格⟩", self.layout.len())
        } else {
            base
        }
    }

    /// 活动窗格是否已断开（标签栏图标用）。
    pub fn active_dead(&self) -> bool {
        self.active_pane().map(|p| p.dead).unwrap_or(false)
    }
}

/// 标签页内容。
pub enum TabContent {
    /// SSH 连接中（结果经线程送回）
    Connecting {
        title: String,
        rx: std::sync::mpsc::Receiver<anyhow::Result<TermPane>>,
    },
    /// 终端（本地 / SSH / Telnet，可含多个分屏窗格）
    Terminal(TerminalTab),
    /// 连接 / 认证失败（保留 tab 供查看错误）
    Failed(String),
    /// 内置浏览器
    Browser(BrowserTab),
    /// 远程桌面
    RemoteDesktop(RemoteDesktopTab),
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
    pub fn title(&self) -> String {
        match self {
            TabContent::Connecting { title, .. } => title.clone(),
            TabContent::Terminal(tab) => tab.title(),
            TabContent::Failed(_) => "连接失败".to_string(),
            TabContent::Browser(tab) => tab.display_title(),
            TabContent::RemoteDesktop(tab) => tab.title(),
            TabContent::LocalFiles(_) => "本地文件".to_string(),
            TabContent::Macro => "键宏".to_string(),
            TabContent::PortScan => "端口扫描".to_string(),
        }
    }

    /// 终端标签页本体（仅 Terminal 返回）。
    pub fn terminal_tab(&self) -> Option<&TerminalTab> {
        match self {
            TabContent::Terminal(tab) => Some(tab),
            _ => None,
        }
    }

    pub fn terminal_tab_mut(&mut self) -> Option<&mut TerminalTab> {
        match self {
            TabContent::Terminal(tab) => Some(tab),
            _ => None,
        }
    }

    /// **活动**窗格（仅 Terminal 且活动窗格已连上时返回）。
    ///
    /// 保持这个名字是为了让既有调用点（键盘/复制/粘贴/查找/AI 上下文……）
    /// 不用改动：它们本来就只关心「当前聚焦的那一格」。
    pub fn terminal(&self) -> Option<&TermPane> {
        self.terminal_tab()?.active_pane()
    }

    pub fn terminal_mut(&mut self) -> Option<&mut TermPane> {
        self.terminal_tab_mut()?.active_pane_mut()
    }

    /// 释放底层资源（关闭标签时调用，确保 WebView 子窗口被销毁）。
    pub fn shutdown(&mut self) {
        if let TabContent::Browser(tab) = self {
            tab.shutdown();
        }
    }
}
