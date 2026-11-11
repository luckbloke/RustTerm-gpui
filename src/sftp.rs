//! 右侧「SFTP」停靠面板（GPUI 版）：绑定当前 SSH 会话，浏览**远程**目录。
//!
//! 结构：
//! - 头部：`SFTP` + 上一级 / 刷新 / 上传 / 新建目录 / 关闭（全部用矢量图标）
//! - 路径栏（回车跳转）+ 家目录按钮
//! - 选项：显示隐藏文件 + 被隐藏条数
//! - 列表：`[D]/[F] 名称` + 体积；双击目录进入、双击文件下载；行内操作按钮
//! - 进度条（可取消）+ 传输队列
//! - 状态：N 项 + 绑定会话
//!
//! 关键点：**复用当前 SSH 会话已认证的连接**（`SshHandle::open_sftp`），
//! 不重新建链、不重复认证，跳板机场景同样成立。

use std::sync::mpsc::Receiver;
use std::sync::Arc;

use crate::backends::ssh::SshHandle;
use crate::sftp_client::{
    human_size, join_remote, parent_remote, RemoteEntry, SftpClient, TransferCancel,
};
use crate::ui_icons::ic;
use gpui_kit::assets::IconName;
use gpui_kit::base::component_traits::Disableable as _;
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::theme::Theme;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

/// 一次传输的运行态。
struct TransferTask {
    label: String,
    sent: u64,
    total: u64,
    cancel: Arc<TransferCancel>,
    progress_rx: Receiver<(u64, u64)>,
    result_rx: Receiver<Result<(), String>>,
    error: Option<String>,
    finished: bool,
    /// 结束后自动刷新列表（上传 / 删除会改变远端内容）
    refresh_on_done: bool,
    refreshed: bool,
}

/// 连接结果：客户端 + 家目录 + 初次列表
type ConnectResult = (Arc<SftpClient>, String, Vec<RemoteEntry>);
/// 列目录结果：(被列出的路径, 条目)
type ListResult = Result<(String, Vec<RemoteEntry>), String>;

/// 单次拖拽最多上传的文件数（防止误拖一个巨大的目录树把会话打满）。
const MAX_DROP_FILES: usize = 200;

pub struct SftpPanel {
    pub open: bool,

    // ---- 会话绑定 ----
    /// 绑定的会话身份（`Arc::as_ptr`），用于检测切换标签
    bound_key: Option<usize>,
    /// 是否已针对 `bound_key` 做过一次绑定动作
    pub bound_initialized: bool,
    bound_label: String,
    client: Option<Arc<SftpClient>>,
    connecting: bool,
    connect_rx: Option<Receiver<Result<ConnectResult, String>>>,
    bind_error: Option<String>,

    // ---- 目录 ----
    path: String,
    path_input: Option<Entity<InputState>>,
    entries: Vec<RemoteEntry>,
    show_hidden: bool,
    hidden_count: usize,
    loading: bool,
    list_rx: Option<Receiver<ListResult>>,
    selected: Option<usize>,
    hint: Option<String>,

    // ---- 传输 ----
    tasks: Vec<TransferTask>,

    /// 运行时的句柄快照（`request_list` 等内部动作需要 spawn）
    rt: Option<tokio::runtime::Handle>,

    // ---- 内联输入 ----
    /// (旧路径, 输入框)
    renaming: Option<(String, Entity<InputState>)>,
    mkdir_input: Option<Entity<InputState>>,
}

impl Default for SftpPanel {
    fn default() -> Self {
        Self {
            open: false,
            bound_key: None,
            bound_initialized: false,
            bound_label: String::new(),
            client: None,
            connecting: false,
            connect_rx: None,
            bind_error: None,
            path: ".".to_string(),
            path_input: None,
            entries: Vec::new(),
            show_hidden: false,
            hidden_count: 0,
            loading: false,
            list_rx: None,
            selected: None,
            hint: None,
            tasks: Vec::new(),
            rt: None,
            renaming: None,
            mkdir_input: None,
        }
    }
}

impl SftpPanel {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        // 关→开时重新绑定，让打开动作触发建链
        if self.open {
            self.bound_initialized = false;
        }
    }

    /// 绑定到当前活动 SSH 会话。每帧调用；会话变了才重新连接。
    pub fn bind(&mut self, rt: &tokio::runtime::Handle, label: &str, ssh: Option<Arc<SshHandle>>) {
        self.rt = Some(rt.clone());
        let key = ssh
            .as_ref()
            .map(|h| Arc::as_ptr(h) as *const u8 as usize);

        if key == self.bound_key && self.bound_initialized {
            return;
        }

        // 会话变了（或首次绑定）：清掉旧状态
        self.bound_key = key;
        self.bound_initialized = true;
        self.client = None;
        self.entries.clear();
        self.selected = None;
        self.list_rx = None;
        self.connect_rx = None;
        self.bind_error = None;
        self.loading = false;
        self.hint = None;
        self.renaming = None;
        self.mkdir_input = None;
        self.bound_label = ssh
            .as_ref()
            .map(|h| h.target.clone())
            .unwrap_or_else(|| label.to_string());

        let Some(ssh) = ssh else {
            self.hint = Some("SFTP 需要活动的 SSH 会话：先打开一个 SSH 终端标签".into());
            return;
        };
        if !self.open {
            return;
        }

        self.connecting = true;
        let (tx, rx) = std::sync::mpsc::channel::<Result<ConnectResult, String>>();
        self.connect_rx = Some(rx);
        rt.spawn(async move {
            let result = async {
                let session = ssh.open_sftp().await?;
                let client = Arc::new(SftpClient::new(session));
                let home = client.home_dir().await.unwrap_or_else(|_| ".".to_string());
                let entries = client.list_dir(&home).await?;
                Ok::<_, anyhow::Error>((client, home, entries))
            }
            .await
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
    }

    /// 渲染（右侧停靠）。None = 面板关闭。
    pub fn render(
        &mut self,
        window: &mut Window,
        cx: &mut Context<crate::app::RustTermApp>,
    ) -> Option<AnyElement> {
        if !self.open {
            return None;
        }
        self.poll();

        let theme = Theme::global(cx).clone();
        let border = theme.colors.border;
        let dim = theme.colors.muted_foreground;
        let danger = theme.colors.danger;
        let accent = theme.colors.link;

        let mut panel = div()
            .v_flex()
            .flex_none()
            .w(px(300.0))
            .h_full()
            .border_l_1()
            .border_color(border)
            .overflow_hidden()
            .text_size(px(12.0));

        // ---- 头部 ----
        panel = panel.child(
            div()
                .h_flex()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(theme.colors.foreground)
                        .child("SFTP"),
                )
                .child(
                    Button::new("sftp-up")
                        .icon(ic(IconName::ArrowUp))
                        .tooltip("上一级")
                        .compact()
                        .on_click(cx.listener(|this, _, _, _| {
                            if let Some(p) = parent_remote(&this.sftp.path) {
                                this.sftp.navigate(p);
                            }
                        })),
                )
                .child(
                    Button::new("sftp-refresh")
                        .icon(ic(IconName::RotateCw))
                        .tooltip("刷新")
                        .compact()
                        .on_click(cx.listener(|this, _, _, _| this.sftp.request_list())),
                )
                .child(
                    Button::new("sftp-upload")
                        .icon(ic(IconName::Upload))
                        .tooltip("上传")
                        .compact()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.sftp.upload_clicked(window, cx)
                        })),
                )
                .child(
                    Button::new("sftp-mkdir")
                        .icon(ic(IconName::FolderPlus))
                        .tooltip("新建目录")
                        .compact()
                        .on_click(cx.listener(|this, _, window, cx| {
                            let input = cx.new(|cx| {
                                InputState::new(window, cx)
                                    .placeholder("新目录名".to_string())
                            });
                            this.sftp.mkdir_input = Some(input);
                        })),
                )
                .child(div().flex_1())
                .child(
                    Button::new("sftp-close")
                        .icon(ic(IconName::X))
                        .tooltip("关闭 SFTP 面板")
                        .compact()
                        .on_click(cx.listener(|this, _, _, _| this.sftp.open = false)),
                ),
        );

        // ---- 连接中 / 错误 / 提示 ----
        if self.connecting {
            return Some(
                panel
                    .child(
                        div()
                            .p_2()
                            .text_color(dim)
                            .child("正在打开 SFTP 通道…"),
                    )
                    .into_any_element(),
            );
        }
        if let Some(err) = self.bind_error.clone() {
            return Some(
                panel
                    .child(
                        div()
                            .p_2()
                            .text_color(danger)
                            .min_w_0()
                            .child(err.clone()),
                    )
                    .child(
                        div().px_2().child(
                            Button::new("sftp-retry")
                                .label("重试")
                                .compact()
                                .on_click(cx.listener(|this, _, _, _| {
                                    this.sftp.bound_key = None;
                                    this.sftp.bound_initialized = false;
                                })),
                        ),
                    )
                    .into_any_element(),
            );
        }
        if let Some(hint) = self.hint.clone() {
            return Some(
                panel
                    .child(div().p_2().text_color(dim).child(hint))
                    .into_any_element(),
            );
        }

        // ---- 路径栏 ----
        if self.path_input.is_none() {
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder("远端路径，回车跳转".to_string())
            });
            cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let p = this
                        .sftp
                        .path_input
                        .as_ref()
                        .map(|s| s.read(cx).value().to_string())
                        .unwrap_or_default();
                    let p = p.trim().to_string();
                    if !p.is_empty() {
                        this.sftp.navigate(p);
                    }
                }
            })
            .detach();
            self.path_input = Some(input);
        }
        if let Some(input) = &self.path_input {
            // 路径变化时同步到输入框；用户正在输入框里编辑（有焦点）时不打扰，
            // 点别处失焦后下一次渲染再同步。之前条件写成了「输入框为空才同步」
            // ——导航后输入框永远停在旧路径（用户实测：进了 congcong，
            // 路径栏还显示 /root）。
            let editing = input.read(cx).focus_handle(cx).is_focused(window);
            let current = input.read(cx).value().to_string();
            if current != self.path && !self.loading && !editing {
                input.update(cx, |s, cx| {
                    s.set_value(self.path.clone(), window, cx)
                });
            }
        }
        let path_input = self.path_input.clone().unwrap();
        panel = panel.child(
            div()
                .h_flex()
                .items_center()
                .gap_1()
                .px_2()
                .pb_1()
                .child(
                    Button::new("sftp-home")
                        .icon(ic(IconName::House))
                        .tooltip("远端家目录")
                        .compact()
                        .on_click(cx.listener(|this, _, _, _| this.sftp.request_home())),
                )
                .child(Input::new(&path_input).flex_1().text_size(px(11.0))),
        );

        // ---- 选项 ----
        panel = panel.child(
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .px_2()
                .pb_1()
                .child(
                    Checkbox::new("sftp-show-hidden")
                        .label("显示隐藏文件")
                        .checked(self.show_hidden)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.sftp.show_hidden = *checked;
                            this.sftp.selected = None;
                            cx.notify();
                        })),
                )
                .when(!self.show_hidden && self.hidden_count > 0, |d| {
                    d.child(
                        div()
                            .text_size(px(10.0))
                            .text_color(dim)
                            .child(format!("已隐藏 {}", self.hidden_count)),
                    )
                }),
        );

        // ---- 列表 ----
        // 列表同时是**拖放目标**：从资源管理器把文件拖进来 → 上传到当前远端目录。
        // gpui-pre 在 Windows 上把 OLE 拖放翻译成 FileDropEvent，
        // 进入时携带 ExternalPaths（本地路径集合）。
        let mut list = div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .border_t_1()
            .border_color(border)
            .on_drop::<gpui_kit::ExternalPaths>(cx.listener(
                |this, paths: &gpui_kit::ExternalPaths, _, cx| {
                    let n = paths.paths().len();
                    if n == 0 {
                        return;
                    }
                    for p in paths.paths() {
                        this.sftp.start_dropped_upload(p.clone());
                    }
                    this.set_toast(format!("正在上传 {n} 个文件到 {}", this.sftp.path));
                    cx.notify();
                },
            ));
        if self.loading {
            list = list.child(div().p_2().text_color(dim).child("加载中…"));
        } else if self.entries.is_empty() {
            list = list.child(
                div().p_2().text_color(dim).child(if self.hidden_count > 0 {
                    "目录内全是隐藏项"
                } else {
                    "空目录"
                }),
            );
        } else {
            for idx in 0..self.entries.len() {
                let entry = self.entries[idx].clone();
                if !self.show_hidden && entry.hidden {
                    continue;
                }
                let selected = self.selected == Some(idx);
                let tag = if entry.is_dir { "[D]" } else { "[F]" };
                let enter_path = entry.path.clone();
                let dl_path = entry.path.clone();
                let dl_name = entry.name.clone();
                let dl_path_btn = dl_path.clone();
                let dl_name_btn = dl_name.clone();
                let rn_path = entry.path.clone();
                let del_path = entry.path.clone();
                let del_dir = entry.is_dir;
                let sel_idx = idx;
                list = list.child(
                    div()
                        .id(SharedString::from(format!("sftp-e-{idx}")))
                        .h_flex()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .py_0p5()
                        .rounded(px(3.0))
                        .when(selected, |d| d.bg(theme.colors.secondary))
                        .hover(|d| d.bg(theme.colors.secondary_hover))
                        .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                            this.sftp.selected = Some(sel_idx);
                            if let ClickEvent::Mouse(e) = ev {
                                if e.up.click_count >= 2 {
                                    if is_dir_entry(this, sel_idx) {
                                        this.sftp.navigate(enter_path.clone());
                                    } else if !this.sftp.entries.is_empty() {
                                        let name = dl_name.clone();
                                        let path = dl_path.clone();
                                        // 「另存为」在后台弹；取消则什么都不做
                                        this.sftp.download_clicked(window, cx, path, name);
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
                                .text_color(if entry.is_dir { accent } else { theme.colors.foreground })
                                .child(format!("{tag} {}", entry.name)),
                        )
                        .when(!entry.is_dir, |d| {
                            d.child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(dim)
                                    .child(human_size(entry.size)),
                            )
                        })
                        .child(
                            Button::new(SharedString::from(format!("sftp-dl-{idx}")))
                                .icon(ic(IconName::ArrowDownToLine))
                                .tooltip("下载")
                                .compact()
                                .disabled(entry.is_dir)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.sftp.download_clicked(
                                        window,
                                        cx,
                                        dl_path_btn.clone(),
                                        dl_name_btn.clone(),
                                    );
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("sftp-rn-{idx}")))
                                .icon(ic(IconName::Pencil))
                                .tooltip("重命名")
                                .compact()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    let input = cx.new(|cx| {
                                        InputState::new(window, cx)
                                            .placeholder("新名称".to_string())
                                    });
                                    input.update(cx, |s, cx| {
                                        s.set_value(rn_name_of(&rn_path), window, cx)
                                    });
                                    this.sftp.renaming = Some((rn_path.clone(), input));
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("sftp-del-{idx}")))
                                .icon(ic(IconName::Trash))
                                .tooltip("删除")
                                .compact()
                                .on_click(cx.listener(move |this, _, _, _| {
                                    this.sftp.apply_delete(del_path.clone(), del_dir);
                                })),
                        ),
                );
            }
        }
        panel = panel.child(list);

        // ---- 内联输入：重命名 / 新建目录 ----
        if let Some((old, input)) = self.renaming.clone() {
            panel = panel.child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(border)
                    .child(div().text_size(px(11.0)).child("重命名"))
                    .child(Input::new(&input).flex_1().text_size(px(11.0)))
                    .child(
                        Button::new("rn-ok")
                            .icon(ic(IconName::Check))
                            .tooltip("确认")
                            .compact()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let name = this
                                    .sftp
                                    .renaming
                                    .as_ref()
                                    .and_then(|(_, s)| Some(s.read(cx).value().to_string()))
                                    .unwrap_or_default();
                                let old = old.clone();
                                this.sftp.apply_rename(old, name);
                                this.sftp.renaming = None;
                            })),
                    )
                    .child(
                        Button::new("rn-cancel")
                            .icon(ic(IconName::X))
                            .tooltip("取消")
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| this.sftp.renaming = None)),
                    ),
            );
        }
        if let Some(input) = self.mkdir_input.clone() {
            panel = panel.child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(border)
                    .child(div().text_size(px(11.0)).child("新目录"))
                    .child(Input::new(&input).flex_1().text_size(px(11.0)))
                    .child(
                        Button::new("mkdir-ok")
                            .icon(ic(IconName::Check))
                            .tooltip("确认")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                let name = this
                                    .sftp
                                    .mkdir_input
                                    .as_ref()
                                    .map(|s| s.read(cx).value().to_string())
                                    .unwrap_or_default();
                                this.sftp.apply_mkdir(name);
                                this.sftp.mkdir_input = None;
                            })),
                    )
                    .child(
                        Button::new("mkdir-cancel")
                            .icon(ic(IconName::X))
                            .tooltip("取消")
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| this.sftp.mkdir_input = None)),
                    ),
            );
        }

        // ---- 进度 + 状态 ----
        for (i, task) in self.tasks.iter().enumerate() {
            // total=0（远端 metadata 没给 size）时**不能**拿 sent 当分母——那会让
            // 进度条瞬间满格卡死。此时显示不确定态：条走半程示意 + 文本「未知大小」。
            // 已知大小：正常 sent/total。
            let known = task.total > 0;
            let ratio = if known {
                (task.sent as f32 / task.total as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
            panel = panel
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .pt_1()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_ellipsis()
                                .text_size(px(10.0))
                                .text_color(dim)
                                .child(task.label.clone()),
                        )
                        .when(task.finished && task.error.is_none(), |d| {
                            d.child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(theme.colors.success)
                                    .child("完成"),
                            )
                        })
                        .child(
                            Button::new(SharedString::from(format!("sftp-task-{i}")))
                                .icon(ic(IconName::X))
                                .tooltip(if task.finished { "移除记录" } else { "取消传输" })
                                .compact()
                                .on_click(cx.listener(move |this, _, _, _| {
                                    if let Some(task) = this.sftp.tasks.get(i) {
                                        if task.finished {
                                            this.sftp.tasks.remove(i);
                                        } else {
                                            task.cancel.request_user_cancel();
                                        }
                                    }
                                })),
                        ),
                )
                .child(
                    div()
                        .h_flex()
                        .px_2()
                        .gap_1()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .h(px(4.0))
                                .rounded(px(2.0))
                                .bg(theme.colors.secondary)
                                .child(
                                    div()
                                        .h_full()
                                        .w(relative(if known {
                                            ratio
                                        } else {
                                            0.5 // 未知大小（远端 metadata 没给 size）：半程示意，别满格误导
                                        }))
                                        .rounded(px(2.0))
                                        .bg(theme.colors.accent),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(9.0))
                                .text_color(dim)
                                .child(if known {
                                    format!(
                                        "{} / {}",
                                        human_size(task.sent),
                                        human_size(task.total)
                                    )
                                } else {
                                    format!("{}（未知大小）", human_size(task.sent))
                                }),
                        ),
                );
            let idx = i;
            if let Some(err) = &task.error {
                panel = panel.child(
                    div()
                        .px_2()
                        .text_size(px(10.0))
                        .text_color(danger)
                        .child(err.clone()),
                );
            }
            let _ = idx;
        }
        let visible = self
            .entries
            .iter()
            .filter(|e| self.show_hidden || !e.hidden)
            .count();
        panel = panel.child(
            div()
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
                        .child(format!("{visible} 项")),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(dim)
                        .min_w_0()
                        .text_ellipsis()
                        .child(self.bound_label.clone()),
                ),
        );

        Some(panel.into_any_element())
    }

    // ------------------------------------------------------------------
    // 轮询后台结果
    // ------------------------------------------------------------------

    fn poll(&mut self) {
        let got = self.connect_rx.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(result) = got {
            self.connect_rx = None;
            self.connecting = false;
            match result {
                Ok((client, home, entries)) => {
                    self.client = Some(client);
                    self.path = home.clone();
                    self.set_entries(entries);
                    self.bind_error = None;
                }
                Err(e) => self.bind_error = Some(e),
            }
        }

        let got = self.list_rx.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(result) = got {
            self.list_rx = None;
            self.loading = false;
            match result {
                Ok((path, entries)) => {
                    self.path = path;
                    self.set_entries(entries);
                }
                Err(e) => self.bind_error = Some(e),
            }
        }

        // 传输/删除完成且需要刷新的（多个同时完成只刷一次）
        let mut need_list = false;
        for task in self.tasks.iter_mut() {
            while let Ok((sent, total)) = task.progress_rx.try_recv() {
                task.sent = sent;
                task.total = total;
            }
            if let Ok(res) = task.result_rx.try_recv() {
                task.finished = true;
                task.error = res.err();
                if task.refresh_on_done && !task.refreshed {
                    task.refreshed = true;
                    need_list = true;
                }
            }
        }
        if need_list {
            self.request_list();
        }
    }

    fn set_entries(&mut self, entries: Vec<RemoteEntry>) {
        self.hidden_count = entries.iter().filter(|e| e.hidden).count();
        self.entries = entries;
        self.selected = None;
    }

    // ------------------------------------------------------------------
    // 动作
    // ------------------------------------------------------------------

    fn navigate(&mut self, path: String) {
        // 先清空：否则新目录返回前，列表还是旧目录的内容（还能被双击）
        self.path = path;
        self.entries.clear();
        self.selected = None;
        self.request_list();
    }

    fn request_list(&mut self) {
        let Some(client) = self.client.clone() else { return };
        let Some(rt) = self.rt.clone() else { return };
        self.loading = true;
        let (tx, rx) = std::sync::mpsc::channel();
        self.list_rx = Some(rx);
        let path = self.path.clone();
        rt.spawn(async move {
            let r = client
                .list_dir(&path)
                .await
                .map(|e| (path.clone(), e))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(r);
        });
    }

    fn request_home(&mut self) {
        let Some(client) = self.client.clone() else { return };
        let Some(rt) = self.rt.clone() else { return };
        self.loading = true;
        let (tx, rx) = std::sync::mpsc::channel();
        self.list_rx = Some(rx);
        rt.spawn(async move {
            let r = async {
                let home = client.home_dir().await?;
                let entries = client.list_dir(&home).await?;
                Ok::<_, anyhow::Error>((home, entries))
            }
            .await
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(r);
        });
    }

    /// 上传：先在后台线程弹「多选文件」框，再把选中的文件逐个入队。
    ///
    /// 对话框不能留在 UI 线程（Windows 上同步弹 rfd 会崩），拿到路径后
    /// 回投到本面板再启动传输；用户取消 → 什么都不发生（保持原语义）。
    fn upload_clicked(&mut self, window: &mut Window, cx: &mut Context<crate::app::RustTermApp>) {
        cx.spawn_in(window, async move |this, cx| {
            let picked = cx
                .background_executor()
                .spawn(async { rfd::FileDialog::new().pick_files() })
                .await;
            // 取消：什么都不做
            let Some(files) = picked else { return };
            this.update_in(cx, |this, _window, cx| {
                for p in files {
                    this.sftp.start_upload(p);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 把一个本地文件上传到当前远端目录（⇧ 按钮入口）。
    fn start_upload(&mut self, local: std::path::PathBuf) {
        let Some(client) = self.client.clone() else { return };
        let Some(rt) = self.rt.clone() else { return };
        let dir = self.path.clone();

        let name = local
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "文件".to_string());
        self.spawn_transfer(
            rt.clone(),
            format!("上传 {name}"),
            true,
            move |cancel, tx| async move {
                client
                    .upload(&local, &dir, cancel, move |sent, total| {
                        let _ = tx.send((sent, total));
                    })
                    .await
            },
        );
    }

    /// 拖拽上传：文件直接上传；目录则先建远端目录再逐个上传里面的文件。
    ///
    /// 资源管理器里拖进来的是**本地路径**，目标是当前浏览的远端目录。
    fn start_dropped_upload(&mut self, local: std::path::PathBuf) {
        let Some(client) = self.client.clone() else { return };
        let Some(rt) = self.rt.clone() else { return };
        let dir = self.path.clone();

        let name = local
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "文件".to_string());

        // 收集待上传内容：(本地文件, 远端相对路径) + 需要先建的远端目录
        let mut files: Vec<(std::path::PathBuf, String)> = Vec::new();
        let mut dirs: Vec<String> = Vec::new();
        if local.is_dir() {
            dirs.push(name.clone());
            collect_drop_tree(&local, &name, &mut files, &mut dirs, MAX_DROP_FILES);
        } else {
            files.push((local.clone(), name.clone()));
        }

        if files.is_empty() && dirs.is_empty() {
            self.hint = Some("拖入的内容为空".into());
            return;
        }

        self.spawn_transfer(
            rt,
            format!("上传 {name}"),
            true,
            move |cancel, tx| async move {
                // 先建目录（自顶向下，保证父目录先存在）
                for d in &dirs {
                    client.mkdir(&join_remote(&dir, d)).await?;
                }
                for (local_path, rel) in &files {
                    // 远端父目录（相对当前目录）
                    let parent = parent_remote(rel).unwrap_or_else(|| ".".to_string());
                    let remote_dir = join_remote(&dir, &parent);
                    let tx = tx.clone();
                    client
                        .upload(local_path, &remote_dir, cancel.clone(), move |sent, total| {
                            let _ = tx.send((sent, total));
                        })
                        .await?;
                }
                Ok(())
            },
        );
    }

    /// 下载：先在后台线程弹「另存为」拿本地路径，再开始传输。
    ///
    /// 对话框不能留在 UI 线程（Windows 上同步弹 rfd 会崩）；拿到路径后
    /// 回投到本面板再启动传输，用户取消 → 什么都不发生（保持原语义）。
    fn download_clicked(
        &mut self,
        window: &mut Window,
        cx: &mut Context<crate::app::RustTermApp>,
        remote: String,
        name: String,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let dialog_name = name.clone();
            let picked = cx
                .background_executor()
                .spawn(async move {
                    rfd::FileDialog::new()
                        .set_file_name(&dialog_name)
                        .save_file()
                })
                .await;
            // 取消：什么都不做
            let Some(local) = picked else { return };
            this.update_in(cx, |this, _window, cx| {
                this.sftp.start_download(remote, name, local);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 开始下载到**指定的**本地路径（路径由调用方先通过「另存为」拿到）。
    fn start_download(&mut self, remote: String, name: String, local: std::path::PathBuf) {
        let Some(client) = self.client.clone() else { return };
        let Some(rt) = self.rt.clone() else { return };
        self.spawn_transfer(rt, format!("下载 {name}"), false, move |cancel, tx| async move {
            client
                .download(&remote, &local, cancel, move |sent, total| {
                    let _ = tx.send((sent, total));
                })
                .await
        });
    }

    fn apply_delete(&mut self, path: String, is_dir: bool) {
        let Some(client) = self.client.clone() else { return };
        let Some(rt) = self.rt.clone() else { return };
        // 删除走传输队列：完成后自动刷新列表（此前立即列目录会读到删除前的旧状态）
        let label = if is_dir {
            format!("删除目录 {}", short_name(&path))
        } else {
            format!("删除 {}", short_name(&path))
        };
        self.spawn_transfer(rt, label, true, move |_cancel, _tx| async move {
            client.remove(&path, is_dir).await
        });
    }

    fn apply_mkdir(&mut self, name: String) {
        let Some(client) = self.client.clone() else { return };
        let Some(rt) = self.rt.clone() else { return };
        let path = join_remote(&self.path, name.trim());
        self.spawn_transfer(
            rt,
            format!("新建目录 {}", short_name(&path)),
            true,
            move |_cancel, _tx| async move { client.mkdir(&path).await },
        );
    }

    fn apply_rename(&mut self, old: String, name: String) {
        let Some(client) = self.client.clone() else { return };
        let Some(rt) = self.rt.clone() else { return };
        let old_name = short_name(&old).to_string();
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        // 目录部分保持原样（根目录必须拼回 "/"，否则会变成相对路径被搬到 home）
        let parent = match old.rfind('/') {
            Some(0) => "/".to_string(),
            Some(i) => old[..i].to_string(),
            None => String::new(),
        };
        let new_path = join_remote(&parent, &name);
        self.spawn_transfer(
            rt,
            format!("重命名 {old_name} → {name}"),
            true,
            move |_cancel, _tx| async move { client.rename(&old, &new_path).await },
        );
    }

    /// 统一的传输启动：进度 + 结果各一条通道。
    /// `refresh_on_done`：结束后自动刷新远端列表（上传/删除等会改远端的操作）。
    fn spawn_transfer<F, Fut>(&mut self, rt: tokio::runtime::Handle, label: String, refresh_on_done: bool, make: F)
    where
        F: FnOnce(Arc<TransferCancel>, std::sync::mpsc::Sender<(u64, u64)>) -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let cancel = TransferCancel::new();
        let (prog_tx, prog_rx) = std::sync::mpsc::channel();
        let (res_tx, res_rx) = std::sync::mpsc::channel();
        let fut = make(cancel.clone(), prog_tx);
        rt.spawn(async move {
            let r = fut.await;
            let _ = res_tx.send(r.map_err(|e| format!("{e:#}")));
        });
        self.tasks.push(TransferTask {
            label,
            sent: 0,
            total: 0,
            cancel,
            progress_rx: prog_rx,
            result_rx: res_rx,
            error: None,
            finished: false,
            refresh_on_done,
            refreshed: false,
        });
    }
}

/// 列表条目是否为目录（双击行为判定用）。
fn is_dir_entry(this: &crate::app::RustTermApp, idx: usize) -> bool {
    this.sftp.entries.get(idx).map(|e| e.is_dir).unwrap_or(false)
}

/// 递归收集拖入目录里的文件与子目录。
///
/// - `files`：`(本地路径, 相对当前远端目录的路径)`
/// - `dirs`：需要在远端先建出来的目录（相对路径）
///
/// `limit` 用来兜底：拖一个几百 GB 的目录进来会一次性开出几百个传输任务。
fn collect_drop_tree(
    root: &std::path::Path,
    rel: &str,
    files: &mut Vec<(std::path::PathBuf, String)>,
    dirs: &mut Vec<String>,
    limit: usize,
) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for entry in rd.flatten() {
        if files.len() >= limit {
            return;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let child_rel = join_remote(rel, &name);
        if path.is_dir() {
            dirs.push(child_rel.clone());
            collect_drop_tree(&path, &child_rel, files, dirs, limit);
        } else {
            files.push((path, child_rel));
        }
    }
}

/// 取远端路径末段作为重命名初始值。
fn rn_name_of(path: &str) -> String {
    short_name(path).to_string()
}

/// 从远端路径取末段名称（显示用）。
fn short_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    }
}
