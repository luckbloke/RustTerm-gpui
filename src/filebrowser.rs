//! 本地文件浏览器（单栏，真实文件系统，GPUI 版）。
//!
//! 服务「本地文件」主标签页（宽栏）。只做本地文件系统浏览。

use std::path::{Path, PathBuf};

use crate::ui_icons::ic;
use gpui_kit::assets::IconName;
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::theme::Theme;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<String>,
}

impl FileEntry {
    pub fn size_label(&self) -> String {
        if self.is_dir {
            String::new()
        } else {
            format_size(self.size)
        }
    }
}

pub struct FileBrowser {
    pub path: PathBuf,
    pub path_input: Option<Entity<InputState>>,
    pub entries: Vec<FileEntry>,
    pub selected: Option<usize>,
    pub show_hidden: bool,
    /// 是否已加载过一次（首帧自动填目录）
    loaded: bool,
    /// 状态行（错误提示）
    pub status: Option<String>,
}

impl Default for FileBrowser {
    fn default() -> Self {
        Self::new(dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
    }
}

impl FileBrowser {
    pub fn new(path: PathBuf) -> Self {
        let mut me = Self {
            path,
            path_input: None,
            entries: Vec::new(),
            selected: None,
            show_hidden: false,
            loaded: false,
            status: None,
        };
        me.refresh();
        me
    }

    /// 重新读取当前目录。
    pub fn refresh(&mut self) {
        self.entries.clear();
        self.selected = None;
        self.status = None;

        let rd = match std::fs::read_dir(&self.path) {
            Ok(rd) => rd,
            Err(e) => {
                self.status = Some(format!("无法读取 {}: {e}", self.path.display()));
                return;
            }
        };

        let mut list: Vec<FileEntry> = Vec::new();
        for entry in rd.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            let meta = entry.metadata().ok();
            let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let modified = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .map(|t| {
                    let dt: chrono::DateTime<chrono::Local> = t.into();
                    dt.format("%Y-%m-%d %H:%M").to_string()
                });
            let hidden = is_hidden(&name, entry.path().as_path());
            if hidden && !self.show_hidden {
                continue;
            }
            list.push(FileEntry {
                name,
                path: entry.path(),
                is_dir,
                size,
                modified,
            });
        }

        // 目录优先 + 名称自然序（忽略大小写）
        list.sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });

        self.entries = list;
        self.loaded = true;
    }

    fn enter(&mut self, path: PathBuf) {
        if path.is_dir() {
            self.path = path;
            self.refresh();
        }
    }

    fn go_up(&mut self) {
        if let Some(parent) = self.path.parent() {
            let parent = parent.to_path_buf();
            self.enter(parent);
        }
    }

    fn go_home(&mut self) {
        if let Some(home) = dirs::home_dir() {
            self.enter(home);
        }
    }

    /// 渲染浏览器。`compact` = 窄栏模式（隐藏大小/时间列）。
    pub fn render(
        &mut self,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<crate::app::RustTermApp>,
    ) -> AnyElement {
        if !self.loaded {
            self.refresh();
        }

        let theme = Theme::global(cx).clone();
        let border = theme.colors.border;
        let dim = theme.colors.muted_foreground;
        let accent = theme.colors.link;

        // ---- 路径输入框（惰性创建；路径变化时同步显示值）----
        if self.path_input.is_none() {
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder("输入路径后回车".to_string())
            });
            input.update(cx, |s, cx| {
                s.set_value(self.path.to_string_lossy().to_string(), window, cx)
            });
            cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let p = this
                        .active_file_browser()
                        .and_then(|fb| fb.path_input.as_ref())
                        .map(|s| s.read(cx).value().to_string())
                        .unwrap_or_default();
                    let p = PathBuf::from(p.trim());
                    if p.is_dir() {
                        if let Some(fb) = this.active_file_browser_mut() {
                            fb.enter(p);
                        }
                    } else if let Some(fb) = this.active_file_browser_mut() {
                        fb.status = Some(format!("不是有效目录: {}", p.display()));
                    }
                }
            })
            .detach();
            self.path_input = Some(input);
        }
        let path_input = self.path_input.clone().unwrap();

        // ---- 工具栏 ----
        let mut bar = div()
            .h_flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .child(
                Button::new("fb-up")
                    .icon(ic(IconName::ArrowUp))
                    .tooltip("上一级")
                    .compact()
                    .on_click(cx.listener(|this, _, _, _| {
                        this.active_file_browser_mut().map(|fb| fb.go_up());
                    })),
            )
            .child(
                Button::new("fb-home")
                    .icon(ic(IconName::House))
                    .tooltip("主目录")
                    .compact()
                    .on_click(cx.listener(|this, _, _, _| {
                        this.active_file_browser_mut().map(|fb| fb.go_home());
                    })),
            )
            .child(
                Button::new("fb-refresh")
                    .icon(ic(IconName::RotateCw))
                    .tooltip("刷新")
                    .compact()
                    .on_click(cx.listener(|this, _, _, _| {
                        this.active_file_browser_mut().map(|fb| fb.refresh());
                    })),
            )
            .child(Input::new(&path_input).flex_1().min_w_0().text_size(px(11.0)));

        if !compact {
            bar = bar.child(
                Checkbox::new("fb-hidden")
                    .label("隐藏")
                    .checked(self.show_hidden)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        if let Some(fb) = this.active_file_browser_mut() {
                            fb.show_hidden = *checked;
                            fb.refresh();
                        }
                        cx.notify();
                    })),
            );
        }

        // ---- 文件列表 ----
        let mut list = div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .border_t_1()
            .border_color(border)
            .text_size(px(12.0));
        for (idx, file) in self.entries.iter().enumerate() {
            let selected = self.selected == Some(idx);
            let icon = if file.is_dir { "📁" } else { "📄" };
            let path = file.path.clone();
            let is_dir = file.is_dir;
            let sel_idx = idx;
            // 大小 / 修改时间（宽栏模式才显示，窄栏省地方）
            let size_text = file.size_label();
            let modified = file.modified.clone().unwrap_or_default();
            let mut row = div()
                .id(SharedString::from(format!("fb-{idx}")))
                .h_flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_0p5()
                .rounded(px(3.0))
                .when(selected, |d| d.bg(theme.colors.secondary))
                .hover(|d| d.bg(theme.colors.secondary_hover))
                .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                    if let Some(fb) = this.active_file_browser_mut() {
                        fb.selected = Some(sel_idx);
                    }
                    if let ClickEvent::Mouse(e) = ev {
                        if e.up.click_count >= 2 {
                            if is_dir {
                                this.active_file_browser_mut().map(|fb| fb.enter(path.clone()));
                            } else {
                                open_path(&path);
                            }
                        }
                    }
                    cx.notify();
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_ellipsis()
                        .text_color(if is_dir { accent } else { theme.colors.foreground })
                        .child(format!("{icon} {}", file.name)),
                );
            if !compact {
                row = row
                    .child(
                        div()
                            .w(px(70.0))
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(dim)
                            .text_ellipsis()
                            .child(size_text),
                    )
                    .child(
                        div()
                            .w(px(96.0))
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(dim)
                            .text_ellipsis()
                            .child(modified),
                    );
            }
            list = list.child(row);
        }

        // ---- 状态行 ----
        let mut status_row = div()
            .h_flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(border)
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(dim)
                    .child(format!("{} 项", self.entries.len())),
            );
        if let Some(s) = &self.status {
            status_row = status_row.child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme.colors.warning)
                    .min_w_0()
                    .text_ellipsis()
                    .child(s.clone()),
            );
        }

        div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .child(bar)
            .child(list)
            .child(status_row)
            .into_any_element()
    }
}

/// 取当前活动标签的本地文件浏览器（订阅回调用）。
trait AppFileBrowserExt {
    fn active_file_browser(&self) -> Option<&FileBrowser>;
    fn active_file_browser_mut(&mut self) -> Option<&mut FileBrowser>;
}

impl AppFileBrowserExt for crate::app::RustTermApp {
    fn active_file_browser(&self) -> Option<&FileBrowser> {
        match self.tabs.get(self.active) {
            Some(crate::tabs::TabContent::LocalFiles(fb)) => Some(fb),
            _ => None,
        }
    }
    fn active_file_browser_mut(&mut self) -> Option<&mut FileBrowser> {
        match self.tabs.get_mut(self.active) {
            Some(crate::tabs::TabContent::LocalFiles(fb)) => Some(fb),
            _ => None,
        }
    }
}

/// 隐藏判定：Unix 点前缀；Windows 额外看 FILE_ATTRIBUTE_HIDDEN。
fn is_hidden(name: &str, path: &Path) -> bool {
    if name.starts_with('.') {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if let Ok(meta) = std::fs::metadata(path) {
            const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
            const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
            let attr = meta.file_attributes();
            if attr & FILE_ATTRIBUTE_HIDDEN != 0 || attr & FILE_ATTRIBUTE_SYSTEM != 0 {
                return true;
            }
        }
    }
    let _ = path;
    false
}

/// 用系统默认程序打开文件。
fn open_path(path: &Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(path)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

pub fn format_size(size: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = size as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{} {}", size as u64, UNITS[unit_idx])
    } else {
        format!("{:.1} {}", size, UNITS[unit_idx])
    }
}
