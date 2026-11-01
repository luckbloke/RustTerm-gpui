//! 键宏管理（GPUI 版对话框 + 数据模型）。
//!
//! 数据模型与终端字节流编码保持不变；UI 换为 gpui 对话框：
//! - 列表：启用开关 + 执行 / 编辑 / 复制 / 删除
//! - 编辑器：名称 / 描述 / 启用 + 动作行（类型徽标 + 值输入框 + 上移/下移/删除）

use chrono;
use rfd;
use uuid;

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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct KeyMacro {
    pub id: String,
    pub name: String,
    pub description: String,
    pub keys: Vec<KeyAction>,
    pub enabled: bool,
    pub trigger: MacroTrigger,
    pub created_at: String,
    pub use_count: u64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum KeyAction {
    Key(String),
    KeyCombo(String),
    Text(String),
    Delay(u64),
    MouseClick(MouseButton, i32, i32),
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum MacroTrigger {
    Manual,
    Hotkey(String),
    OnConnect,
    OnDisconnect,
    Schedule(String),
}

impl MacroTrigger {
    /// 展示用文本。
    pub fn label(&self) -> String {
        match self {
            MacroTrigger::Manual => "手动".to_string(),
            MacroTrigger::Hotkey(hk) => format!("快捷键 {hk}"),
            MacroTrigger::OnConnect => "连接时".to_string(),
            MacroTrigger::OnDisconnect => "断开时".to_string(),
            MacroTrigger::Schedule(cron) => format!("定时 {cron}"),
        }
    }

    /// 归类到编辑器可选的触发方式。
    pub fn kind(&self) -> TriggerKind {
        match self {
            MacroTrigger::Manual => TriggerKind::Manual,
            MacroTrigger::Hotkey(_) => TriggerKind::Hotkey,
            MacroTrigger::OnConnect => TriggerKind::OnConnect,
            MacroTrigger::OnDisconnect => TriggerKind::OnDisconnect,
            MacroTrigger::Schedule(_) => TriggerKind::Schedule,
        }
    }

    /// 触发参数（快捷键文本 / cron 表达式）。
    pub fn arg(&self) -> Option<&str> {
        match self {
            MacroTrigger::Hotkey(hk) => Some(hk.as_str()),
            MacroTrigger::Schedule(c) => Some(c.as_str()),
            _ => None,
        }
    }
}

/// 宏的触发方式（编辑器里用一排按钮切换）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TriggerKind {
    /// 手动：点「发送到终端」才执行
    Manual,
    /// 快捷键：在终端里按下指定组合键自动执行
    Hotkey,
    /// 连接时：SSH 连接建立后自动执行
    OnConnect,
    /// 断开时：关闭 SSH 标签前自动执行
    OnDisconnect,
    /// 定时：按 cron 表达式触发（预留）
    Schedule,
}

impl TriggerKind {
    /// 全部可选项（编辑器按钮组用）。
    pub const ALL: [TriggerKind; 5] = [
        TriggerKind::Manual,
        TriggerKind::Hotkey,
        TriggerKind::OnConnect,
        TriggerKind::OnDisconnect,
        TriggerKind::Schedule,
    ];

    /// 按钮上的文字。
    pub fn label(&self) -> &'static str {
        match self {
            TriggerKind::Manual => "手动",
            TriggerKind::Hotkey => "快捷键",
            TriggerKind::OnConnect => "连接时",
            TriggerKind::OnDisconnect => "断开时",
            TriggerKind::Schedule => "定时",
        }
    }

    /// 是否需要一个参数输入框（快捷键文本 / cron）。
    pub fn needs_arg(&self) -> bool {
        matches!(self, TriggerKind::Hotkey | TriggerKind::Schedule)
    }
}

/// 规范化快捷键文本：`"ctrl+shift+m"` → `"Ctrl+Shift+M"`。
///
/// 宏的快捷键由用户手填（`Ctrl+Shift+M` / `ctrl+shift+m` / `Ctrl-Shift-M` 都可能），
/// 而按键事件侧给出的是「修饰键位 + 键名」（如 `ctrl` + `m`）。两边都过一遍
/// 这个函数再比较，才能做到「怎么写都能匹配上」。
pub fn normalize_hotkey(s: &str) -> String {
    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;
    let mut super_key = false;
    let mut key = String::new();
    for part in s.split(['+', '-']) {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        match p.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "ctl" => ctrl = true,
            "shift" => shift = true,
            "alt" | "option" | "opt" => alt = true,
            "super" | "win" | "meta" | "cmd" | "command" => super_key = true,
            _ => key = p.to_string(),
        }
    }
    let mut out = String::new();
    if ctrl {
        out.push_str("Ctrl+");
    }
    if shift {
        out.push_str("Shift+");
    }
    if alt {
        out.push_str("Alt+");
    }
    if super_key {
        out.push_str("Super+");
    }
    out.push_str(&key.to_ascii_uppercase());
    out
}

/// 把一次按键事件转成规范化快捷键文本（与 [`normalize_hotkey`] 同格式）。
pub fn hotkey_of(keystroke: &gpui_kit::Keystroke) -> String {
    let m = &keystroke.modifiers;
    let mut out = String::new();
    if m.control {
        out.push_str("Ctrl+");
    }
    if m.shift {
        out.push_str("Shift+");
    }
    if m.alt {
        out.push_str("Alt+");
    }
    if m.platform {
        out.push_str("Super+");
    }
    // Windows 上 gpui 给的键名是小写（enter / escape / f5 / 字母），
    // 统一大写后与用户输入的快捷键文本可比。
    out.push_str(&keystroke.key.to_ascii_uppercase());
    out
}

impl Default for KeyMacro {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: String::new(),
            description: String::new(),
            keys: Vec::new(),
            enabled: true,
            trigger: MacroTrigger::Manual,
            created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            use_count: 0,
        }
    }
}

impl KeyAction {
    fn type_label(&self) -> &'static str {
        match self {
            KeyAction::Key(_) => "单键",
            KeyAction::KeyCombo(_) => "组合键",
            KeyAction::Text(_) => "文本",
            KeyAction::Delay(_) => "延迟ms",
            KeyAction::MouseClick(_, _, _) => "鼠标",
        }
    }
    /// 取可编辑的字符串载荷（MouseClick 无）。
    fn payload(&self) -> Option<String> {
        match self {
            KeyAction::Key(k) | KeyAction::KeyCombo(k) | KeyAction::Text(k) => Some(k.clone()),
            KeyAction::Delay(ms) => Some(format!("{ms}")),
            KeyAction::MouseClick(_, _, _) => None,
        }
    }
    /// 用新字符串重建动作（保持类型）。
    fn with_payload(&self, text: String) -> KeyAction {
        match self {
            KeyAction::Key(_) => KeyAction::Key(text),
            KeyAction::KeyCombo(_) => KeyAction::KeyCombo(text),
            KeyAction::Text(_) => KeyAction::Text(text),
            KeyAction::Delay(_) => {
                KeyAction::Delay(text.trim().parse().unwrap_or(100))
            }
            KeyAction::MouseClick(_, _, _) => self.clone(),
        }
    }
}

/// 编辑器状态：正在编辑的宏快照 + 与 keys 平行的输入实体。
#[derive(Clone)]
pub struct MacroEditor {
    /// 正在编辑的宏（保存时写回）
    pub draft: KeyMacro,
    /// 与 draft.keys 平行的输入实体（MouseClick 行为 None）
    pub inputs: Vec<Option<Entity<InputState>>>,
    pub name: Entity<InputState>,
    pub desc: Entity<InputState>,
    /// 当前选择的触发方式
    pub trigger: TriggerKind,
    /// 触发参数输入框（快捷键文本 / cron 表达式）
    pub trigger_arg: Entity<InputState>,
}

#[derive(Clone)]
pub struct MacroPanel {
    pub macros: Vec<KeyMacro>,
    pub editing_index: Option<usize>,
    pub filter: String,
    pub filter_input: Option<Entity<InputState>>,
    pub editor: Option<MacroEditor>,
}

impl Default for MacroPanel {
    fn default() -> Self {
        Self {
            macros: vec![
                KeyMacro {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "清屏".to_string(),
                    description: "发送 Ctrl+L 清屏命令".to_string(),
                    keys: vec![KeyAction::KeyCombo("Ctrl+L".to_string())],
                    enabled: true,
                    trigger: MacroTrigger::Manual,
                    created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                    use_count: 0,
                },
                KeyMacro {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "列出文件详情".to_string(),
                    description: "ls -la 并回车".to_string(),
                    keys: vec![
                        KeyAction::Text("ls -la".to_string()),
                        KeyAction::Key("Enter".to_string()),
                    ],
                    enabled: true,
                    trigger: MacroTrigger::Manual,
                    created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                    use_count: 0,
                },
                KeyMacro {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "Git 状态".to_string(),
                    description: "显示 git status".to_string(),
                    keys: vec![
                        KeyAction::Text("git status".to_string()),
                        KeyAction::Key("Enter".to_string()),
                    ],
                    enabled: true,
                    trigger: MacroTrigger::Manual,
                    created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                    use_count: 0,
                },
                KeyMacro {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "Docker 容器列表".to_string(),
                    description: "docker ps -a".to_string(),
                    keys: vec![
                        KeyAction::Text("docker ps -a".to_string()),
                        KeyAction::Key("Enter".to_string()),
                    ],
                    enabled: true,
                    trigger: MacroTrigger::Manual,
                    created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                    use_count: 0,
                },
            ],
            editing_index: None,
            filter: String::new(),
            filter_input: None,
            editor: None,
        }
    }
}

impl MacroPanel {
    /// 渲染键宏管理器面板（标签页内容：列表 / 编辑器二选一）。
    pub fn render_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<crate::app::RustTermApp>,
    ) -> AnyElement {
        let body = if self.editor.is_some() {
            self.render_editor(window, cx)
        } else {
            self.render_list(window, cx)
        };
        let theme = Theme::global(cx).clone();
        div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .p_4()
            .gap_2()
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(15.0))
                            .text_color(theme.colors.foreground)
                            .child("键宏管理器"),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.colors.muted_foreground)
                            .child("记录常用输入序列，一键发送到终端"),
                    ),
            )
            .child(body)
            .into_any_element()
    }

    fn render_list(
        &mut self,
        window: &mut Window,
        cx: &mut Context<crate::app::RustTermApp>,
    ) -> AnyElement {
        let theme = Theme::global(cx).clone();
        let dim = theme.colors.muted_foreground;

        // 过滤输入框（惰性创建 + Change 订阅）
        if self.filter_input.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("搜索宏".to_string()));
            cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.macros.filter = this
                        .macros
                        .filter_input
                        .as_ref()
                        .map(|s| s.read(cx).value().to_string())
                        .unwrap_or_default();
                    cx.notify();
                }
            })
            .detach();
            self.filter_input = Some(input);
        }
        let filter_input = self.filter_input.clone().unwrap();

        let mut indices: Vec<usize> = (0..self.macros.len()).collect();
        if !self.filter.is_empty() {
            let f = self.filter.to_lowercase();
            indices.retain(|&i| {
                let m = &self.macros[i];
                m.name.to_lowercase().contains(&f) || m.description.to_lowercase().contains(&f)
            });
        }
        indices.sort_by(|&a, &b| self.macros[a].name.cmp(&self.macros[b].name));

        let rows: Vec<(usize, String, String, String, usize, u64, bool)> = indices
            .iter()
            .map(|&i| {
                let m = &self.macros[i];
                let trigger = m.trigger.label();
                (
                    i,
                    m.name.clone(),
                    m.description.clone(),
                    trigger,
                    m.keys.len(),
                    m.use_count,
                    m.enabled,
                )
            })
            .collect();

        let mut list = div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .border_t_1()
            .border_color(theme.colors.border)
            .text_size(px(12.0));
        for (idx, name, desc, trigger, n_keys, use_count, enabled) in rows {
            list = list.child(
                div()
                    .v_flex()
                    .gap_0p5()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.colors.border)
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_1()
                            .child(
                                Checkbox::new(SharedString::from(format!("macro-en-{idx}")))
                                    .checked(enabled)
                                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                        if let Some(m) = this.macros.macros.get_mut(idx) {
                                            m.enabled = *checked;
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_ellipsis()
                                    .child(name.clone()),
                            )
                            .child(
                                Button::new(SharedString::from(format!("macro-x-{idx}")))
                                    .icon(ic(IconName::Play))
                                    .tooltip("发送到终端")
                                    .compact()
                                    .disabled(!enabled)
                                    .on_click(cx.listener(move |this, _, _, _| {
                                        this.send_macro_to_active(idx);
                                    })),
                            )
                            .child(
                                Button::new(SharedString::from(format!("macro-e-{idx}")))
                                    .icon(ic(IconName::Pencil))
                                    .tooltip("编辑")
                                    .compact()
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.macros.start_edit(idx, window, cx);
                                    })),
                            )
                            .child(
                                Button::new(SharedString::from(format!("macro-c-{idx}")))
                                    .icon(ic(IconName::Copy))
                                    .tooltip("复制")
                                    .compact()
                                    .on_click(cx.listener(move |this, _, _, _| {
                                        this.macros.duplicate_macro(idx);
                                    })),
                            )
                            .child(
                                Button::new(SharedString::from(format!("macro-d-{idx}")))
                                    .icon(ic(IconName::Trash))
                                    .tooltip("删除")
                                    .compact()
                                    .on_click(cx.listener(move |this, _, _, _| {
                                        if idx < this.macros.macros.len() {
                                            this.macros.macros.remove(idx);
                                        }
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap_2()
                            .text_size(px(10.0))
                            .text_color(dim)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_ellipsis()
                                    .child(desc.clone()),
                            )
                            .child(div().child(trigger))
                            .child(div().child(format!("{n_keys} 动作")))
                            .child(div().child(format!("{use_count} 次"))),
                    ),
            );
        }

        div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .child(Input::new(&filter_input).w(px(180.0)).text_size(px(12.0)))
                    .child(div().flex_1())
                    .child(
                        Button::new("macro-new")
                            .icon(ic(IconName::Plus))
                            .label("新建宏")
                            .compact()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.macros.start_new_macro(window, cx);
                            })),
                    )
                    .child(
                        Button::new("macro-import")
                            .icon(ic(IconName::Download))
                            .label("导入")
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| {
                                this.macros.import_macros();
                            })),
                    )
                    .child(
                        Button::new("macro-export")
                            .icon(ic(IconName::Upload))
                            .label("导出")
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| {
                                this.macros.export_macros();
                            })),
                    ),
            )
            .child(list)
            .into_any_element()
    }

    fn render_editor(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<crate::app::RustTermApp>,
    ) -> AnyElement {
        let theme = Theme::global(cx).clone();
        let dim = theme.colors.muted_foreground;
        let Some(editor) = &self.editor else {
            return div().into_any_element();
        };
        let draft = editor.draft.clone();

        // 动作行快照：(行号, 类型徽标, 初始值)
        let rows: Vec<(usize, String, Option<String>)> = draft
            .keys
            .iter()
            .enumerate()
            .map(|(i, a)| (i, a.type_label().to_string(), a.payload()))
            .collect();

        let mut keys_list = div().v_flex().gap_1();
        for (i, type_label, value) in rows {
            let mut row = div()
                .h_flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .w(px(56.0))
                        .text_size(px(10.0))
                        .text_color(dim)
                        .child(format!("{}. {type_label}", i + 1)),
                );
            if let (Some(_value), Some(Some(input))) = (value, editor.inputs.get(i)) {
                row = row.child(Input::new(input).flex_1().text_size(px(11.0)));
            } else {
                row = row.child(
                    div()
                        .flex_1()
                        .text_size(px(11.0))
                        .text_color(dim)
                        .child("（不可编辑）"),
                );
            }
            row = row
                .child(
                    Button::new(SharedString::from(format!("mk-up-{i}")))
                        .icon(ic(IconName::ArrowUp))
                        .tooltip("上移")
                        .compact()
                        .disabled(i == 0)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.macros.swap_keys(i, i - 1);
                            cx.notify();
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("mk-dn-{i}")))
                        .icon(ic(IconName::ArrowDown))
                        .tooltip("下移")
                        .compact()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.macros.swap_keys(i, i + 1);
                            cx.notify();
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("mk-del-{i}")))
                        .icon(ic(IconName::Trash))
                        .tooltip("删除该动作")
                        .compact()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(ed) = this.macros.editor.as_mut() {
                                if i < ed.draft.keys.len() {
                                    ed.draft.keys.remove(i);
                                    ed.inputs.remove(i);
                                }
                            }
                            cx.notify();
                        })),
                );
            keys_list = keys_list.child(row);
        }

        div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_size(px(12.0)).child("名称"))
                    .child(Input::new(&editor.name).flex_1().text_size(px(12.0))),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_size(px(12.0)).child("描述"))
                    .child(Input::new(&editor.desc).flex_1().text_size(px(12.0))),
            )
            .child(
                Checkbox::new("macro-editor-enabled")
                    .label("启用")
                    .checked(draft.enabled)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        if let Some(ed) = this.macros.editor.as_mut() {
                            ed.draft.enabled = *checked;
                        }
                        cx.notify();
                    })),
            )
            // ---- 触发方式 ----
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_size(px(12.0)).child("触发方式")),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .children(TriggerKind::ALL.into_iter().map(|kind| {
                        let selected = editor.trigger == kind;
                        Button::new(SharedString::from(format!("trig-{:?}", kind)))
                            .label(kind.label())
                            .compact()
                            .toggled(selected)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(ed) = this.macros.editor.as_mut() {
                                    ed.trigger = kind;
                                }
                                cx.notify();
                            }))
                    })),
            )
            .when(editor.trigger.needs_arg(), |d| {
                d.child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w(px(52.0))
                                .text_size(px(12.0))
                                .child(if editor.trigger == TriggerKind::Hotkey {
                                    "快捷键"
                                } else {
                                    "cron"
                                }),
                        )
                        .child(Input::new(&editor.trigger_arg).flex_1().text_size(px(12.0)))
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(dim)
                                .child(if editor.trigger == TriggerKind::Hotkey {
                                    "如 Ctrl+Shift+M"
                                } else {
                                    "如 */5 * * * *"
                                }),
                        ),
                )
            })
            .child(
                div().text_size(px(11.0)).text_color(dim).child(
                    "按键序列（Text=原样发送；单键 Enter/Tab/Escape；组合键支持 Ctrl+字母）",
                ),
            )
            .child(keys_list)
            .child(
                Button::new("mk-add")
                    .icon(ic(IconName::Plus))
                    .label("添加文本动作")
                    .compact()
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let Some(ed) = this.macros.editor.as_mut() {
                            ed.draft.keys.push(KeyAction::Text(String::new()));
                            ed.inputs.push(Some(
                                cx.new(|cx| {
                                    InputState::new(window, cx)
                                        .placeholder("输入要发送的文本".to_string())
                                }),
                            ));
                        }
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(
                div()
                    .h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("mk-cancel")
                            .label("取消")
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| {
                                this.macros.editor = None;
                            })),
                    )
                    .child(
                        Button::new("mk-save")
                            .label("保存")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.macros.save_editor(cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    // ------------------------------------------------------------------
    // 编辑器动作
    // ------------------------------------------------------------------

    pub fn start_new_macro(&mut self, window: &mut Window, cx: &mut Context<crate::app::RustTermApp>) {
        self.macros.push(KeyMacro::default());
        let idx = self.macros.len() - 1;
        self.start_edit(idx, window, cx);
    }

    pub fn start_edit(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<crate::app::RustTermApp>,
    ) {
        let Some(m) = self.macros.get(idx).cloned() else {
            return;
        };
        let m_trigger_kind = m.trigger.kind();
        let inputs = m
            .keys
            .iter()
            .map(|a| {
                a.payload().map(|v| {
                    let e = cx.new(|cx| InputState::new(window, cx).placeholder(String::new()));
                    e.update(cx, |s, cx| s.set_value(v, window, cx));
                    e
                })
            })
            .collect();
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("名称".to_string()));
        name.update(cx, |s, cx| s.set_value(m.name.clone(), window, cx));
        let desc = cx.new(|cx| InputState::new(window, cx).placeholder("描述".to_string()));
        desc.update(cx, |s, cx| s.set_value(m.description.clone(), window, cx));
        // 触发参数：快捷键文本或 cron 表达式
        let arg_text = m.trigger.arg().unwrap_or_default().to_string();
        let trigger_arg = cx.new(|cx| InputState::new(window, cx).placeholder("快捷键，如 Ctrl+Shift+M".to_string()));
        trigger_arg.update(cx, |s, cx| s.set_value(arg_text, window, cx));
        self.editor = Some(MacroEditor {
            draft: m,
            inputs,
            name,
            desc,
            trigger: m_trigger_kind,
            trigger_arg,
        });
        self.editing_index = Some(idx);
    }

    fn swap_keys(&mut self, a: usize, b: usize) {
        let Some(ed) = self.editor.as_mut() else {
            return;
        };
        if a >= ed.draft.keys.len() || b >= ed.draft.keys.len() {
            return;
        }
        ed.draft.keys.swap(a, b);
        ed.inputs.swap(a, b);
    }

    /// 编辑器输入实体的名称/描述访问（渲染用）。
    #[allow(dead_code)]
    fn editor_name(&self) -> Option<&Entity<InputState>> {
        self.editor.as_ref().map(|e| &e.name)
    }

    /// 保存编辑器内容到宏列表。
    fn save_editor(&mut self, cx: &mut Context<crate::app::RustTermApp>) {
        let Some(idx) = self.editing_index else { return };
        let Some(ed) = self.editor.take() else { return };
        let Some(m) = self.macros.get_mut(idx) else { return };
        if let Some(name_input) = Some(&ed.name) {
            m.name = name_input.read(cx).value().to_string();
        }
        m.description = ed.desc.read(cx).value().to_string();
        m.enabled = ed.draft.enabled;
        // 触发方式 + 参数
        let arg = ed.trigger_arg.read(cx).value().trim().to_string();
        m.trigger = match ed.trigger {
            TriggerKind::Manual => MacroTrigger::Manual,
            TriggerKind::Hotkey => MacroTrigger::Hotkey(arg),
            TriggerKind::OnConnect => MacroTrigger::OnConnect,
            TriggerKind::OnDisconnect => MacroTrigger::OnDisconnect,
            TriggerKind::Schedule => MacroTrigger::Schedule(arg),
        };
        m.keys = ed
            .draft
            .keys
            .iter()
            .zip(ed.inputs.iter())
            .map(|(action, input)| match (action, input) {
                (a, Some(input)) => {
                    let text = input.read(cx).value().to_string();
                    a.with_payload(text)
                }
                (a, None) => a.clone(),
            })
            .collect();
        self.editor = None;
        self.editing_index = None;
    }

    fn duplicate_macro(&mut self, idx: usize) {
        if let Some(macro_) = self.macros.get(idx).cloned() {
            let mut new_macro = macro_;
            new_macro.id = uuid::Uuid::new_v4().to_string();
            new_macro.name = format!("{} (副本)", new_macro.name);
            new_macro.created_at =
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            new_macro.use_count = 0;
            self.macros.insert(idx + 1, new_macro);
        }
    }

    /// 供外部（如侧栏宏列表）直接执行指定宏，返回应写入终端的字节序列。
    pub fn send_macro_bytes(&mut self, idx: usize) -> Option<Vec<u8>> {
        self.execute_macro(idx)
    }

    /// 执行宏：把动作序列编码为终端字节流。
    fn execute_macro(&mut self, idx: usize) -> Option<Vec<u8>> {
        let macro_ = self.macros.get_mut(idx)?;
        macro_.use_count += 1;
        let mut out = Vec::new();
        for action in &macro_.keys {
            match action {
                KeyAction::Text(t) => out.extend_from_slice(t.as_bytes()),
                KeyAction::Key(k) => match k.as_str() {
                    "Enter" => out.push(b'\r'),
                    "Tab" => out.push(b'\t'),
                    "Escape" => out.extend_from_slice(b"\x1b"),
                    "Space" => out.push(b' '),
                    "Backspace" => out.push(0x7f),
                    _ => {}
                },
                KeyAction::KeyCombo(c) => {
                    // "Ctrl+L" → ^L（0x0C）等控制码；其余组合暂不支持
                    let parts: Vec<&str> = c.split('+').collect();
                    if parts.len() == 2 && parts[0].eq_ignore_ascii_case("ctrl") {
                        if let Some(ch) = parts[1].chars().next() {
                            if ch.is_ascii_alphabetic() {
                                out.push((ch.to_ascii_uppercase() as u8) & 0x1f);
                            }
                        }
                    }
                }
                KeyAction::Delay(_) | KeyAction::MouseClick(_, _, _) => {}
            }
        }
        (!out.is_empty()).then_some(out)
    }

    fn import_macros(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("JSON", &["json"])
            .pick_file()
        {
            if let Ok(content) = std::fs::read_to_string(path) {
                if let Ok(imported) = serde_json::from_str::<Vec<KeyMacro>>(&content) {
                    self.macros.extend(imported);
                }
            }
        }
    }

    fn export_macros(&self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("JSON", &["json"])
            .set_file_name("macros.json")
            .save_file()
        {
            let _ = std::fs::write(
                path,
                serde_json::to_string_pretty(&self.macros).unwrap_or_default(),
            );
        }
    }

    /// 按快捷键查找宏，返回其在列表中的下标。
    ///
    /// 比较前两边都做规范化（`Ctrl+Shift+M` ≡ `ctrl+shift+m`），
    /// 因此用户随手写成什么样都能命中。
    pub fn get_macro_by_hotkey(&self, hotkey: &str) -> Option<usize> {
        let want = normalize_hotkey(hotkey);
        if want.is_empty() {
            return None;
        }
        self.macros.iter().position(|m| {
            m.enabled
                && matches!(&m.trigger, MacroTrigger::Hotkey(hk) if normalize_hotkey(hk) == want)
        })
    }

    /// 「连接时」触发的宏下标列表（SSH 连接建立后自动执行）。
    pub fn get_on_connect_macros(&self) -> Vec<usize> {
        self.macros
            .iter()
            .enumerate()
            .filter(|(_, m)| m.enabled && matches!(&m.trigger, MacroTrigger::OnConnect))
            .map(|(i, _)| i)
            .collect()
    }

    /// 「断开时」触发的宏下标列表（关闭 SSH 标签前自动执行）。
    pub fn get_on_disconnect_macros(&self) -> Vec<usize> {
        self.macros
            .iter()
            .enumerate()
            .filter(|(_, m)| m.enabled && matches!(&m.trigger, MacroTrigger::OnDisconnect))
            .map(|(i, _)| i)
            .collect()
    }
}

pub fn apply_keep_alive() {
    // keep-alive 边缘处理：发送空包保持 SSH 连接
}

#[cfg(test)]
mod tests {
    // 注意：不能用 `use super::*` —— macros.rs 顶层有 `use gpui_kit::*`，
    // 会把 gpui_kit 的 `test` **模块**带进来，导致 `#[test]` 属性解析到它
    // 而不是内建测试宏（展开时无限递归）。
    use super::{normalize_hotkey, MacroTrigger, TriggerKind};

    #[test]
    fn hotkey_normalization_is_case_and_sep_insensitive() {
        assert_eq!(normalize_hotkey("Ctrl+Shift+M"), "Ctrl+Shift+M");
        assert_eq!(normalize_hotkey("ctrl+shift+m"), "Ctrl+Shift+M");
        assert_eq!(normalize_hotkey("Ctrl-Shift-M"), "Ctrl+Shift+M");
        assert_eq!(normalize_hotkey("  ctrl + shift + m  "), "Ctrl+Shift+M");
        // 修饰键顺序固定，书写顺序不影响
        assert_eq!(normalize_hotkey("shift+ctrl+m"), "Ctrl+Shift+M");
    }

    #[test]
    fn hotkey_normalization_keeps_named_keys() {
        assert_eq!(normalize_hotkey("Ctrl+Enter"), "Ctrl+ENTER");
        assert_eq!(normalize_hotkey("F5"), "F5");
        assert_eq!(normalize_hotkey("Alt+F4"), "Alt+F4");
    }

    #[test]
    fn trigger_round_trips_between_kind_and_text() {
        assert_eq!(MacroTrigger::Manual.kind(), TriggerKind::Manual);
        assert_eq!(
            MacroTrigger::Hotkey("Ctrl+M".into()).kind(),
            TriggerKind::Hotkey
        );
        assert_eq!(
            MacroTrigger::Hotkey("Ctrl+M".into()).arg(),
            Some("Ctrl+M")
        );
        assert_eq!(MacroTrigger::OnConnect.arg(), None);
        assert_eq!(MacroTrigger::Manual.label(), "手动");
    }
}
