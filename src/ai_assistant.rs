//! AI 助手面板（右侧停靠式，GPUI 版）。
//!
//! - 顶部栏：标题 + Agent 开关 + 截图 / 清空 / 设置 / 关闭按钮
//! - 对话区：用户消息气泡、助手消息气泡（命令附「发送到终端」）
//! - 底部：输入框 + 发送按钮（Enter 发送）
//! - 后端：多提供方（OpenAI 兼容 / Ollama / Anthropic），阻塞调用放线程，
//!   结果经通道回传。

use crate::ai::{self, AiError, AiMessage};
use crate::config::AiSettings;
use crate::i18n::tr;
use crate::ui_icons::ic;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, TryRecvError};

use gpui_kit::assets::IconName;
use gpui_kit::base::SelectableText;
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::theme::Theme;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Role {
    User,
    Assistant,
    Error,
}

/// 待发送的截图附件。
pub struct Attachment {
    pub name: String,
    /// PNG 的 base64（不含 data: 前缀）
    pub b64: String,
    pub width: u32,
    pub height: u32,
}

/// 待发送的文件附件（📎 选择 / 拖入窗口）。
#[derive(Debug)]
pub struct FileAttachment {
    pub name: String,
    pub payload: FilePayload,
}

/// 文件附件的载荷形态。
#[derive(Debug)]
pub enum FilePayload {
    /// 文本文件：内容以代码块内联进 prompt
    Text(String),
    /// 图片文件：统一转成 PNG base64（不含 data: 前缀），走视觉通路
    ImageB64(String),
}

/// 文本附件截断字符数（几十 KB 足够 AI 理解，过多会挤爆 token）。
const FILE_TEXT_MAX_CHARS: usize = 64_000;

/// 附件总数上限（截图 + 文件）：一次消息塞太多会挤爆 token。
pub const MAX_ATTACHMENTS: usize = 6;
/// 截图张数上限（截图比文件更贵：整屏图）。
pub const MAX_SCREENSHOTS: usize = 3;
/// 单个附件的原始字节上限。
///
/// 读取发生在后台线程，但没有这个上限时，误选一个 100MB 的文件会先把内存
/// 吃满、再复制一份 base64（×1.34）。超过直接拒绝并给出可读提示。
pub const FILE_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// 图片解码后的像素上限（4000 万像素 ≈ 160MB RGBA）。
///
/// 挡住"8MB 的高压缩 JPEG 解出上千 MB"这类内存炸弹——先读尺寸再解码。
pub const IMAGE_MAX_PIXELS: u64 = 40_000_000;
/// 图片附件最长边：等比缩小到这个尺寸再编码（控制 base64 体积与 token）。
pub const IMAGE_MAX_DIM: u32 = 1600;
/// 历史消息内容的字符上限（只有最近两条保持完整，更早的按此截断）。
///
/// 附件文本（最多 `FILE_TEXT_MAX_CHARS` 字）是内联进用户消息的：不做这个
/// 上限，之后每一轮都会把那份长文本重新发给模型，token 随轮数线性膨胀。
pub const HISTORY_MSG_MAX_CHARS: usize = 4_000;
/// 最近几条历史保持完整（不截断）。
pub const HISTORY_FULL_TAIL: usize = 2;
/// 等待 AI 回复的兜底超时（秒）。
///
/// 底层 HTTP 自己 120s 超时，这里留余量：只兜"线程卡死 / 渠道永不返回"，
/// 正常请求碰不到。用户可以随时点「停止」主动取消。
pub const AI_WAIT_TIMEOUT_SECS: u64 = 180;
/// 对话消息保留上限：只留最近这么多条，避免内存与每帧快照克隆无限增长
///（发给模型的条数另由 `ai.max_history` 控制）。
pub const MAX_MESSAGES: usize = 200;

/// 按**字符**截断（不是字节）：中文 / emoji 都不会被切成半个字符。
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// 历史里第 `i` 条消息（共 `len` 条）允许的字符预算（纯函数，可单测）。
pub fn history_budget_chars(i: usize, len: usize) -> usize {
    if len.saturating_sub(i) <= HISTORY_FULL_TAIL {
        usize::MAX
    } else {
        HISTORY_MSG_MAX_CHARS
    }
}

/// 文本附件截断：保留头部 + 一句说明（字符级，绝不切坏 UTF-8）。
pub fn clip_text_attachment(text: String) -> String {
    let total = text.chars().count();
    if total <= FILE_TEXT_MAX_CHARS {
        return text;
    }
    let mut out = truncate_chars(&text, FILE_TEXT_MAX_CHARS);
    out.push_str(&format!(
        "\n（……内容过长已截断，共 {total} 字符，仅展示前 {FILE_TEXT_MAX_CHARS} 个）"
    ));
    out
}

/// 读取一个文件 → 附件（PNG / JPEG 走视觉通路，其余按 UTF-8 文本内联）。
///
/// 刻意做成**不碰面板状态的纯函数**：读盘 / 图片解码 / base64 都是秒级重活，
/// 必须能在后台线程里跑（UI 线程一压就卡/崩）；顺便也让它可离线单测
/// （大小上限、图片判定、文本截断）。
pub fn ingest_path(path: &Path) -> Result<FileAttachment, String> {
    use base64::Engine as _;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(String::from)
        .ok_or_else(|| format!("文件名无法识别：{}", path.display()))?;

    let meta = std::fs::metadata(path).map_err(|e| format!("读取 {name} 失败：{e}"))?;
    if meta.len() == 0 {
        return Err(format!("{name} 是空文件，跳过。"));
    }
    if meta.len() > FILE_MAX_BYTES {
        return Err(format!(
            "{name} 太大（{:.1} MB），单个附件上限 {} MB。",
            meta.len() as f64 / (1024.0 * 1024.0),
            FILE_MAX_BYTES / (1024 * 1024)
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("读取 {name} 失败：{e}"))?;

    // ---- 图片：PNG / JPEG 统一「解码 → 等比缩小 → PNG → base64」----
    // 不再让 PNG 原样透传：4000×3000 的截图 base64 后体积巨大又费 token，
    // 缩到 IMAGE_MAX_DIM 后视觉信息基本无损、体积降一个数量级。
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) || bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        // 先读尺寸再解码：挡住"小文件解出巨图"的内存炸弹
        let reader = image::ImageReader::new(std::io::Cursor::new(&bytes))
            .with_guessed_format()
            .map_err(|e| format!("解析图片 {name} 失败：{e}"))?;
        if let Ok((w, h)) = reader.into_dimensions() {
            if u64::from(w) * u64::from(h) > IMAGE_MAX_PIXELS {
                return Err(format!("图片 {name} 分辨率过高（{w}×{h}），请先压缩。"));
            }
        }
        let img =
            image::load_from_memory(&bytes).map_err(|e| format!("解析图片 {name} 失败：{e}"))?;
        let img = if img.width() > IMAGE_MAX_DIM || img.height() > IMAGE_MAX_DIM {
            img.resize(
                IMAGE_MAX_DIM,
                IMAGE_MAX_DIM,
                image::imageops::FilterType::Triangle,
            )
        } else {
            img
        };
        let mut png: Vec<u8> = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut png),
            image::ImageFormat::Png,
        )
        .map_err(|e| format!("{name} 转换 PNG 失败：{e}"))?;
        return Ok(FileAttachment {
            name,
            payload: FilePayload::ImageB64(
                base64::engine::general_purpose::STANDARD.encode(&png),
            ),
        });
    }

    // ---- 文本文件：UTF-8（或近 ASCII）→ 以代码块内联 ----
    match String::from_utf8(bytes) {
        Ok(text) => Ok(FileAttachment {
            name,
            payload: FilePayload::Text(clip_text_attachment(text)),
        }),
        Err(_) => Err(format!(
            "{name} 不是文本文件（也不是 PNG/JPEG 图片），暂不支持。"
        )),
    }
}

#[derive(Clone, Debug)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    pub time: String,
    /// AI 回复中提取出的命令，供 UI 提供「发送到终端」
    pub commands: Vec<String>,
}

pub struct AiAssistantPanel {
    pub open: bool,
    pub agent_mode: bool,
    pub messages: Vec<ChatMessage>,
    /// 输入框文本（与 InputState 实体经订阅保持同步）
    pub input: String,
    pub input_state: Option<Entity<InputState>>,
    pub waiting: bool,
    /// 上一轮发给 AI 的终端上下文（展示用）
    pub last_context: String,
    /// 用户点击「发送到终端」产生的请求（命令, 是否危险）
    pub command_request: Option<(String, bool)>,
    /// 待发送的截图（最多 3 张）
    pub attachments: Vec<Attachment>,
    /// 待发送的文件附件（文本内联 / 图片走视觉通路）
    pub files: Vec<FileAttachment>,
    /// 输入区上方的一条提示：发送时会自动附带的终端信息
    pub attach_hint: Option<String>,
    /// 用户按下发送（由 app 层取走并附加终端上下文后真正发出）
    pending_send: bool,
    rx: Option<Receiver<Result<String, AiError>>>,
    /// 本次等待的开始时刻（兜底超时用；正常请求走不到超时）
    wait_started: Option<std::time::Instant>,
    /// 后台文件选择器：线程跑 rfd（UI 线程调 rfd 会崩，见 `pick_files_async`），
    /// 结果按 Vec<Result<附件, 错误>> 回投（读取/解码也在那个线程里做完了）。
    pick_rx: Option<Receiver<Vec<Result<FileAttachment, String>>>>,
    /// 面板当前语言（render 开头由 app 层传入存下；无 lang 参数的方法里的
    /// 用户可见串用它取 `tr`，跟随语言切换）。
    lang: crate::i18n::Lang,
    /// Agent 循环进行中（app 层维护状态行；停止按钮据此改道）。
    pub agent_busy: bool,
    /// Agent 状态行（「Agent：步骤 n/10 · 上一条 `cmd`」）。
    pub agent_status: Option<String>,
    /// 待确认的危险命令（有值 = 显示三选一确认卡）。
    pub agent_confirm_cmd: Option<String>,
    /// 确认卡的选择（app 层取走喂状态机）。
    pub agent_confirm: Option<AgentChoice>,
    /// 用户在 Agent 循环中点了「停止」（app 层取走）。
    pub agent_stop: bool,
}

/// Agent 危险命令确认卡的三选一。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentChoice {
    Run,
    Skip,
    Abort,
}

impl Default for AiAssistantPanel {
    fn default() -> Self {
        Self {
            open: false,
            agent_mode: false,
            messages: Vec::new(),
            input: String::new(),
            input_state: None,
            waiting: false,
            last_context: String::new(),
            command_request: None,
            attachments: Vec::new(),
            files: Vec::new(),
            attach_hint: None,
            pending_send: false,
            rx: None,
            wait_started: None,
            pick_rx: None,
            lang: crate::i18n::Lang::Zh,
            agent_busy: false,
            agent_status: None,
            agent_confirm_cmd: None,
            agent_confirm: None,
            agent_stop: false,
        }
    }
}

impl AiAssistantPanel {
    /// 取走「待发送」标记（app 层据此附加终端上下文后真正调用 send）。
    pub fn take_pending_send(&mut self) -> bool {
        std::mem::take(&mut self.pending_send)
    }

    /// 关闭面板：顺手清掉「待发送 / 待执行命令」这类一次性标记。
    ///
    /// 不清的话它们会留在结构体里，等下一次打开面板的第一帧被消费——
    /// 那时输入框里的内容或终端焦点早就不是用户按下按钮时的那份了。
    /// 文件选择器（`pick_rx`）**不清**：对话框已经弹出来了，用户点确认后
    /// 仍应把结果附加进来。
    pub fn close(&mut self) {
        self.open = false;
        self.pending_send = false;
        self.command_request = None;
    }

    /// 切换面板开关（菜单 / 工具栏 / 快捷键共用；关闭时走 `close` 清理）。
    pub fn toggle(&mut self) {
        if self.open {
            self.close();
        } else {
            self.open = true;
        }
    }

    /// 取消正在进行的请求（「停止」按钮）。
    ///
    /// 只是丢弃结果通道：后台线程会自己跑完并被回收（HTTP 自带 120s 超时），
    /// 它再 `send` 会因为接收端已丢弃而失败，不会影响下一次请求。
    pub fn cancel(&mut self) {
        if !self.waiting {
            return;
        }
        self.rx = None;
        self.waiting = false;
        self.wait_started = None;
        self.push_error(tr(self.lang, "ai.err_cancelled"));
    }

    /// 右侧停靠面板。None = 面板关闭。
    pub fn render(
        &mut self,
        ai: &AiSettings,
        lang: crate::i18n::Lang,
        window: &mut Window,
        cx: &mut Context<crate::app::RustTermApp>,
    ) -> Option<AnyElement> {
        // 存下面板级语言：无 lang 参数的方法（send/poll_async/cancel…）里的
        // 用户可见串用它取 tr——保证跟随语言切换。
        self.lang = lang;
        if !self.open {
            // 面板关着：照样把后台结果收进模型，**不要丢掉**。
            // 原来这里直接把 rx 丢掉——用户提问后随手关面板，回答就永远消失了
            //（重开只看到自己的问题，以为 AI 没回）。收进来只是内存里多一条
            // 消息，重开面板就能看到；UI 泵也会以同一节奏调用这里。
            self.poll_async();
            return None;
        }
        self.poll_async();

        let theme = Theme::global(cx).clone();
        let border = theme.colors.border;
        let dim = theme.colors.muted_foreground;

        // 输入框实体惰性创建；Change 订阅同步到 self.input
        if self.input_state.is_none() {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(tr(lang, "ai.placeholder"))
            });
            cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                match event {
                    InputEvent::Change => {
                        this.ai_assistant.input = this
                            .ai_assistant
                            .input_state
                            .as_ref()
                            .map(|s| s.read(cx).value().to_string())
                            .unwrap_or_default();
                    }
                    // Enter 发送（placeholder 承诺过的行为——此前只订阅了 Change，
                    // Enter 实际上没有任何处理，按键无反应）。与「发送」按钮走
                    // 同一条 pending_send 路径；Shift+Enter 不拦截，交给编辑器
                    // 的换行处理（多行输入）。
                    InputEvent::PressEnter { shift, .. } => {
                        if !shift && !this.ai_assistant.input.trim().is_empty() {
                            this.ai_assistant.pending_send = true;
                            cx.notify();
                        }
                    }
                    _ => {}
                }
            })
            .detach();
            self.input_state = Some(input);
        }

        let mut panel = div()
            .v_flex()
            .flex_none()
            .w(px(360.0))
            .h_full()
            .border_l_1()
            .border_color(border)
            .overflow_hidden()
            .text_size(px(12.0));

        // ---- 顶栏 ----
        panel = panel.child(
            div()
                .h_flex()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .child(div().text_size(px(13.0)).child(tr(lang, "ai.title")))
                .child(
                    Checkbox::new("ai-agent")
                        .label(tr(lang, "ai.agent"))
                        .checked(self.agent_mode)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.ai_assistant.agent_mode = *checked;
                            cx.notify();
                        })),
                )
                .child(div().flex_1())
                .child(
                    Button::new("ai-shot")
                        .icon(ic(IconName::Camera))
                        .tooltip(tr(lang, "ai.tooltip_screenshot"))
                        .compact()
                        .on_click(cx.listener(|this, _, _, _| {
                            this.ai_assistant.capture_screenshot();
                        })),
                )
                .child(
                    Button::new("ai-clear")
                        .icon(ic(IconName::Trash))
                        .tooltip(tr(lang, "ai.tooltip_clear"))
                        .compact()
                        .on_click(cx.listener(|this, _, _, _| {
                            this.ai_assistant.messages.clear();
                            this.ai_assistant.last_context.clear();
                            this.ai_assistant.attachments.clear();
                            this.ai_assistant.files.clear();
                        })),
                )
                .child(
                    Button::new("ai-config")
                        .icon(ic(IconName::Settings))
                        .tooltip(tr(lang, "ai.tooltip_settings"))
                        .compact()
                        .on_click(cx.listener(|this, _, _, _| {
                            this.ai_config_panel.open = true;
                        })),
                )
                .child(
                    Button::new("ai-close")
                        .icon(ic(IconName::X))
                        .tooltip(tr(lang, "ai.tooltip_close"))
                        .compact()
                        .on_click(cx.listener(|this, _, _, cx| {
                            // 走 close()：顺手清掉 pending_send / command_request，
                            // 避免下次打开时拿旧输入或旧焦点执行（见 close 的说明）
                            this.ai_assistant.close();
                            cx.notify();
                        })),
                ),
        );

        // ---- Agent 状态行 / 确认卡 ----
        if let Some(status) = &self.agent_status {
            panel = panel.child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .text_size(px(11.0))
                    .text_color(theme.colors.info)
                    .child(status.clone()),
            );
        }
        if let Some(cmd) = &self.agent_confirm_cmd {
            panel = panel.child(
                div()
                    .v_flex()
                    .mx_2()
                    .mb_1()
                    .p_2()
                    .gap_2()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.colors.danger)
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.colors.danger)
                            .child(tr(lang, "ai.agent_confirm_title")),
                    )
                    .child(
                        div()
                            .font_family("monospace")
                            .text_size(px(11.0))
                            .child(format!("$ {cmd}")),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap_2()
                            .child(
                                Button::new("agent-confirm-run")
                                    .label(tr(lang, "ai.agent_confirm_run"))
                                    .compact()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.ai_assistant.agent_confirm =
                                            Some(AgentChoice::Run);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("agent-confirm-skip")
                                    .label(tr(lang, "ai.agent_confirm_skip"))
                                    .compact()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.ai_assistant.agent_confirm =
                                            Some(AgentChoice::Skip);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("agent-confirm-abort")
                                    .label(tr(lang, "ai.agent_confirm_abort"))
                                    .compact()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.ai_assistant.agent_confirm =
                                            Some(AgentChoice::Abort);
                                        cx.notify();
                                    })),
                            ),
                    ),
            );
        }

        // ---- 提供方状态条 ----
        let provider = ai.provider();
        panel = panel.child(
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .px_2()
                .pb_1()
                .border_b_1()
                .border_color(border)
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(dim)
                        .child(format!("{} · {}", provider.label(), ai.model)),
                )
                .child(div().flex_1())
                .when(ai.read_only, |d| {
                    d.child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.colors.warning)
                            .child(tr(lang, "ai.read_only")),
                    )
                }),
        );

        // ---- 对话区 ----
        let mut chat = div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .p_2()
            .gap_2();
        if self.messages.is_empty() && !self.waiting {
            chat = chat
                .child(
                    div()
                        .mt_8()
                        .text_center()
                        .text_size(px(14.0))
                        .child(tr(lang, "ai.welcome_title")),
                )
                .child(
                    div()
                        .text_center()
                        .text_size(px(11.0))
                        .text_color(dim)
                        .child(tr(lang, "ai.welcome_sub")),
                );
            if ai.api_key.trim().is_empty() && ai.provider().needs_api_key() {
                chat = chat.child(
                    div()
                        .text_center()
                        .text_size(px(11.0))
                        .text_color(theme.colors.info)
                        .child(tr(lang, "ai.no_api_key")),
                );
            }
        }

        // 快照消息，避免渲染时借用冲突
        let snapshot: Vec<ChatMessage> = self.messages.clone();
        for (mi, msg) in snapshot.iter().enumerate() {
            let color = match msg.role {
                Role::User => theme.colors.button_primary,
                Role::Assistant => theme.colors.secondary,
                Role::Error => theme.colors.danger,
            };
            let fg = match msg.role {
                Role::User => theme.colors.button_primary_foreground,
                _ => theme.colors.foreground,
            };
            // 右键复制：GPUI 的 div 文本不可选中，所以提供「右键整条消息 = 复制全文」，
            // 外加气泡右下角的 📋 按钮（发现性）。右键比按钮快，两者写同一剪贴板。
            let copy_text = msg.content.clone();
            let btn_text = msg.content.clone();
            chat = chat.child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(
                        div()
                            .v_flex()
                            .gap_1()
                            .p_2()
                            .rounded(px(8.0))
                            .bg(color)
                            .text_color(fg)
                            .max_w_full()
                            // 可拖选文本（GPUI SelectableText）：像 SSH 控制台一样
                            // 划选、Ctrl+C 复制；替代纯 div（不可选中）。之前只提供
                            // 右键整条复制，用户反馈要逐字选择。右键复制整条保留。
                            .child(
                                SelectableText::new(
                                    ("ai-msg", mi),
                                    msg.content.clone(),
                                )
                                .text_style(gpui_kit::TextStyleRefinement {
                                    ..Default::default()
                                }),
                            )
                            .on_mouse_down(
                                gpui_kit::MouseButton::Right,
                                // 右键弹上下文菜单（ai_menu 由 app.rs 的
                                // ai_context_menu_ui 渲染：复制选中 / 复制整条 /
                                // 复制全部对话）。此前这里被并发编辑覆盖回了
                                // 「直接复制」旧版——右键菜单从未出现。
                                // SelectableText 只处理 Left，Right 正常冒泡到气泡。
                                cx.listener(move |this, ev: &gpui_kit::MouseDownEvent, window, cx| {
                                    // 右键瞬间抓取划选文字：此刻窗口选区一定还在；
                                    // 菜单弹出后的任何交互都可能清掉它，所以必须在
                                    // 这里读，不能等到菜单项点击时再读。空 = 没划选，
                                    // 菜单里「复制选中」会自动降级复制整条消息。
                                    let sel = gpui_kit::base::TextSelection::selected_text(
                                        window, cx,
                                    );
                                    this.ai_menu = Some((ev.position, copy_text.clone(), sel));
                                    cx.notify();
                                }),
                            ),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(dim)
                                    .child(msg.time.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(dim)
                                    .cursor_pointer()
                                    .child(tr(lang, "ai.copy_btn"))
                                    .on_mouse_down(
                                        gpui_kit::MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            cx.write_to_clipboard(
                                                gpui_kit::ClipboardItem::new_string(
                                                    btn_text.clone(),
                                                ),
                                            );
                                            this.set_toast(tr(lang, "ai.toast_copied"));
                                            cx.notify();
                                        }),
                                    ),
                            ),
                    ),
            );

            // 任务完成标记 + 命令按钮（仅助手消息）
            if msg.role == Role::Assistant {
                if ai::is_task_done(&msg.content) {
                    chat = chat.child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.colors.success)
                            .child(tr(lang, "ai.task_done")),
                    );
                }
                for (ci, cmd) in msg.commands.iter().enumerate() {
                    let dangerous = ai::is_dangerous(cmd);
                    let is_write = ai::is_write_command(cmd);
                    let blocked = ai.read_only && is_write;
                    let cmd_text = cmd.clone();
                    // 两个分支各要一份：闭包是 move 的，共用同一个 String 会被
                    // 第一个闭包拿走（编译期借用错误）
                    let cmd_text_blocked = cmd.clone();
                    let cmd_color = if dangerous {
                        theme.colors.danger
                    } else {
                        theme.colors.foreground
                    };
                    chat = chat.child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_ellipsis()
                                    .font_family("monospace")
                                    .text_size(px(11.0))
                                    .text_color(cmd_color)
                                    .child(format!("$ {cmd}")),
                            )
                            .when(blocked, |d| {
                                d.child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(theme.colors.warning)
                                        .child(tr(lang, "ai.readonly_blocked")),
                                )
                            })
                            .when(!blocked && dangerous, |d| {
                                d.child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(theme.colors.danger)
                                        .child(tr(lang, "ai.dangerous")),
                                )
                            })
                            .when(!blocked && !dangerous, |d| {
                                d.child(
                                    Button::new(SharedString::from(format!(
                                        "ai-send-cmd-{mi}-{ci}"
                                    )))
                                    .icon(ic(IconName::Play))
                                    .label(tr(lang, "ai.send_to_terminal"))
                                    .compact()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.ai_assistant.command_request =
                                            Some((cmd_text.clone(), false));
                                        cx.notify();
                                    })),
                                )
                            })
                            .when(!blocked && dangerous, |d| {
                                // 危险命令此前是**按钮直接不渲染**：用户只看到
                                // 「⚠ 危险」四个字，点了没反应、也不知道为什么。
                                // 现在按钮在，点击由 app 侧统一拦截并 toast 说明
                                //（app.rs 的 command_request 消费端，含 read_only）。
                                d.child(
                                    Button::new(SharedString::from(format!(
                                        "ai-send-cmd-blocked-{mi}-{ci}"
                                    )))
                                    .icon(ic(IconName::TriangleAlert))
                                    .label(tr(lang, "ai.dangerous_disabled"))
                                    .compact()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.ai_assistant.command_request =
                                            Some((cmd_text_blocked.clone(), true));
                                        cx.notify();
                                    })),
                                )
                            }),
                    );
                }
            }
        }

        if self.waiting || self.agent_busy {
            // 等待中的两件事：告诉用户"怎么退出等待"（此前只能干等，
            // 后端卡住时面板就永远停在「思考中…」），以及给出取消入口。
            // Agent 循环进行中同样给出停止（立即终止，已发的命令不撤回）。
            let label = if self.agent_busy {
                tr(lang, "ai.agent_stop")
            } else {
                tr(lang, "ai.stop")
            };
            chat = chat.child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_color(dim).child(tr(lang, "ai.thinking")))
                    .child(
                        Button::new("ai-cancel")
                            .label(label)
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.ai_assistant.agent_busy {
                                    this.ai_assistant.agent_stop = true;
                                }
                                this.ai_assistant.cancel();
                                cx.notify();
                            })),
                    ),
            );
        }
        panel = panel.child(chat);

        // ---- 输入区 ----
        if let Some(hint) = self.attach_hint.as_ref() {
            if !hint.is_empty() {
                panel = panel.child(
                    div()
                        .px_2()
                        .text_size(px(10.0))
                        .text_color(dim)
                        .child(
                            tr(lang, "ai.attach_hint")
                                .replace("{hint}", hint)
                                .replace(
                                    "{n}",
                                    &ai.context_lines.to_string(),
                                ),
                        ),
                );
            }
        }

        // 已附加的文件（点击移除）
        let files_snapshot: Vec<(usize, String, String)> = self
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let size = match &f.payload {
                    FilePayload::Text(t) => {
                        tr(lang, "ai.file_size_chars").replace("{n}", &t.chars().count().to_string())
                    }
                    FilePayload::ImageB64(b) => tr(lang, "ai.file_size_img").replace(
                        "{n}",
                        &(b.len() * 3 / 4 / 1024).to_string(),
                    ),
                };
                (i, f.name.clone(), size)
            })
            .collect();
        for (i, name, size) in files_snapshot {
            panel = panel.child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .child(
                        Button::new(SharedString::from(format!("ai-file-{i}")))
                            .label(
                                tr(lang, "ai.file_chip")
                                    .replace("{name}", &name)
                                    .replace("{size}", &size),
                            )
                            .compact()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if i < this.ai_assistant.files.len() {
                                    this.ai_assistant.files.remove(i);
                                }
                                cx.notify();
                            })),
                    ),
            );
        }

        // 已附加的截图（点击移除）
        let shots_snapshot: Vec<(usize, String, u32, u32)> = self
            .attachments
            .iter()
            .enumerate()
            .map(|(i, a)| (i, a.name.clone(), a.width, a.height))
            .collect();
        for (i, name, w, h) in shots_snapshot {
            panel = panel.child(
                div().px_2().child(
                    Button::new(SharedString::from(format!("ai-shot-{i}")))
                        .label(
                            tr(lang, "ai.shot_chip")
                                .replace("{name}", &name)
                                .replace("{w}", &w.to_string())
                                .replace("{h}", &h.to_string()),
                        )
                        .compact()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if i < this.ai_assistant.attachments.len() {
                                this.ai_assistant.attachments.remove(i);
                            }
                            cx.notify();
                        })),
                ),
            );
        }

        let input_state = self.input_state.clone().unwrap();
        panel = panel
            .child(
                div()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(border)
                    .child(Input::new(&input_state).text_size(px(12.0))),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .pb_2()
                    .child(
                        Button::new("ai-attach")
                            .icon(ic(IconName::Paperclip))
                            .label(tr(lang, "ai.attach"))
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| {
                                // 只起线程：rfd 必须离开 UI 线程（见函数注释）
                                this.ai_assistant.pick_files_async();
                            })),
                    )
                    .child(
                        Button::new("ai-send")
                            .icon(ic(IconName::Send))
                            .label(tr(lang, "ai.send"))
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| {
                                this.ai_assistant.pending_send = true;
                            })),
                    )
                    .child(
                        Button::new("ai-input-clear")
                            .label(tr(lang, "ai.clear_input"))
                            .compact()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.ai_assistant.input.clear();
                                if let Some(s) = &this.ai_assistant.input_state {
                                    s.update(cx, |s, cx| {
                                        s.set_value(String::new(), window, cx)
                                    });
                                }
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(dim)
                            .child(
                                tr(lang, "ai.history_count")
                                    .replace("{n}", &self.messages.len().to_string()),
                            ),
                    ),
            );

        Some(panel.into_any_element())
    }

    /// 发送一条消息。
    ///
    /// `terminal_context` 为终端最近输出（可为空），会作为上下文附加到
    /// 用户消息之前，让 AI 了解当前终端状态。
    pub fn send(
        &mut self,
        ai: &AiSettings,
        terminal_context: Option<String>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let text = self.input.trim().to_string();
        if text.is_empty() || self.waiting {
            return;
        }

        // ---- 预检配置（先检查再清输入框）----
        // 顺序很重要：原来是先把输入框清掉再检查配置，配置不全时用户刚打的
        // 问题就没了，只剩一条错误提示——只能重打一遍（静默丢输入）。
        // API Key 允许为空：本地无鉴权网关很常见。
        if !ai.has_endpoint() {
            self.push_error(tr(self.lang, "ai.err_no_base_url"));
            return;
        }
        if ai.model.trim().is_empty() {
            self.push_error(tr(self.lang, "ai.err_no_model"));
            return;
        }

        // 输入框控件（InputState）显示的内容必须一起清，否则界面上的
        // 文字会残留在框里（self.input 只是内部缓冲）。
        if let Some(s) = &self.input_state {
            s.update(cx, |s, cx| s.set_value(String::new(), window, cx));
        }
        self.input.clear();

        // 组装发给模型的消息（含终端上下文）
        // 历史条数按 ai.max_history 截取；内容按字符预算截断：附件文本可能
        // 有 6.4 万字，不截的话之后每一轮都会把它整份重发给模型。
        let mut history: Vec<AiMessage> = Vec::new();
        let recent: Vec<&ChatMessage> = self.messages.iter().collect();
        let keep = ai.max_history.max(1);
        let start = recent.len().saturating_sub(keep);
        for (i, m) in recent.iter().enumerate().skip(start) {
            if m.role == Role::Error {
                continue;
            }
            let role = if m.role == Role::User { "user" } else { "assistant" };
            let budget = history_budget_chars(i, recent.len());
            let content = if m.content.chars().count() > budget {
                let mut c = truncate_chars(&m.content, budget);
                c.push_str(&tr(self.lang, "ai.hist_truncated"));
                c
            } else {
                m.content.clone()
            };
            history.push(AiMessage {
                role: role.to_string(),
                content,
                images: Vec::new(),
            });
        }

        // 终端上下文
        let mut ctx_for_display = terminal_context.unwrap_or_default();
        let mut prompt = if !ctx_for_display.trim().is_empty() {
            format!(
                "【当前终端输出（最近 {} 行）】\n{}\n\n【用户问题】\n{}",
                ai.context_lines, ctx_for_display, text
            )
        } else {
            text.clone()
        };

        // 截图附件（📷）：随本条用户消息一起发给模型
        let mut images: Vec<String> = self.attachments.iter().map(|a| a.b64.clone()).collect();
        // 文件附件（📎）：文本内联进 prompt；图片并入视觉通路（截图之后）
        for f in &self.files {
            match &f.payload {
                FilePayload::Text(t) => {
                    prompt.push_str(&format!(
                        "\n\n【附件文件：{}】\n```text\n{}\n```",
                        f.name, t
                    ));
                }
                FilePayload::ImageB64(b) => images.push(b.clone()),
            }
        }
        if !images.is_empty() {
            prompt.push_str(&format!(
                "\n\n（附 {} 张界面截图，请结合截图内容回答）",
                images.len()
            ));
        }
        let display = prompt.clone();
        history.push(AiMessage::user_with_images(prompt, images.clone()));
        self.last_context = std::mem::take(&mut ctx_for_display);

        // 用户气泡直接展示实际发送的完整内容（终端上下文 + 问题），
        // 让用户清楚这次请求带了什么。
        self.push_message(ChatMessage {
            role: Role::User,
            content: display,
            time: chrono::Local::now().format("%H:%M:%S").to_string(),
            commands: Vec::new(),
        });

        self.attachments.clear();
        self.files.clear();
        let agent_mode = self.agent_mode;
        let cfg = ai.clone();
        let (tx, rx) = channel::<Result<String, AiError>>();
        self.rx = Some(rx);
        self.waiting = true;
        self.wait_started = Some(std::time::Instant::now());

        std::thread::spawn(move || {
            let result = ai::chat(&cfg, agent_mode, &history);
            let _ = tx.send(result);
        });
    }

    /// 后台弹「选择文件」对话框，结果回投到 `pick_rx`。
    ///
    /// **必须在后台线程跑 rfd**：Windows 上 rfd 的模态对话框会在调用线程里泵
    /// 消息循环，在 GPUI 渲染线程上直接调会撞上外层已持有的借用
    ///（`RefCell already borrowed` → release 下 panic = abort，用户实测
    /// 「附加按钮点了会崩溃」）。同一个线程顺便把读盘 / 解码 / base64 也做完，
    /// 否则 100MB 级的文件一样能把 UI 冻住。
    pub fn pick_files_async(&mut self) {
        if self.pick_rx.is_some() {
            // 已经有一个选择器在等结果：再弹一个会覆盖通道，先来的结果就丢了
            return;
        }
        let (tx, rx) = channel::<Vec<Result<FileAttachment, String>>>();
        self.pick_rx = Some(rx);
        let title = tr(self.lang, "ai.pick_dialog_title");
        std::thread::spawn(move || {
            let paths: Vec<PathBuf> = rfd::FileDialog::new()
                .set_title(&title)
                .pick_files()
                .unwrap_or_default();
            let results = paths.iter().map(|p| ingest_path(p)).collect();
            let _ = tx.send(results);
        });
    }

    /// 对话区/附件列表的容量上限：只保留最近 `MAX_MESSAGES` 条。
    fn push_message(&mut self, msg: ChatMessage) {
        self.messages.push(msg);
        if self.messages.len() > MAX_MESSAGES {
            let drop = self.messages.len() - MAX_MESSAGES;
            self.messages.drain(..drop);
        }
    }

    /// 把后台结果搬进模型：AI 回复 + 文件选择结果。
    ///
    /// 返回是否有 UI 需要重绘（供脏驱动重绘的调度方决定要不要出帧）。
    pub fn poll_async(&mut self) -> bool {
        let mut changed = false;

        // ---- 文件选择结果 ----
        let picked = match &self.pick_rx {
            Some(rx) => match rx.try_recv() {
                Ok(v) => Some(v),
                Err(TryRecvError::Empty) => None,
                // 线程异常退出：当成"没选文件"收尾，不能让 pick_rx 一直挂着
                Err(TryRecvError::Disconnected) => {
                    self.push_error(tr(self.lang, "ai.err_pick_thread"));
                    Some(Vec::new())
                }
            },
            None => None,
        };
        if let Some(results) = picked {
            self.pick_rx = None;
            for r in results {
                match r {
                    Ok(f) => {
                        // 总数上限要在这里再判一次：对话框是异步的，等待期间
                        // 用户可能已经加过截图/文件了
                        if self.attachments.len() + self.files.len() >= MAX_ATTACHMENTS {
                            self.push_error(
                                tr(self.lang, "ai.err_too_many_attachments")
                                    .replace("{n}", &MAX_ATTACHMENTS.to_string()),
                            );
                        } else {
                            self.files.push(f);
                        }
                    }
                    Err(e) => self.push_error(e),
                }
            }
            changed = true;
        }

        // ---- AI 回复 ----
        // 三种结局分开处理：拿到结果 / 通道断开（线程没了）/ 还在等（可能超时）。
        // 关键是**任何一种都必须解除 waiting**，否则面板会永远停在「思考中…」。
        let mut got: Option<Result<String, AiError>> = None;
        let mut disconnected = false;
        let mut timed_out = false;
        if let Some(rx) = &self.rx {
            match rx.try_recv() {
                Ok(r) => got = Some(r),
                Err(TryRecvError::Empty) => {
                    timed_out = self
                        .wait_started
                        .map(|t| t.elapsed().as_secs() >= AI_WAIT_TIMEOUT_SECS)
                        .unwrap_or(false);
                }
                Err(TryRecvError::Disconnected) => disconnected = true,
            }
        }
        if disconnected {
            self.rx = None;
            self.waiting = false;
            self.wait_started = None;
            self.push_error(tr(self.lang, "ai.err_worker_died"));
            changed = true;
        } else if timed_out {
            self.rx = None;
            self.waiting = false;
            self.wait_started = None;
            self.push_error(
                tr(self.lang, "ai.err_timeout")
                    .replace("{n}", &AI_WAIT_TIMEOUT_SECS.to_string()),
            );
            changed = true;
        } else if let Some(result) = got {
            self.rx = None;
            self.waiting = false;
            self.wait_started = None;
            let (role, content) = match result {
                Ok(text) => (Role::Assistant, text),
                Err(e) => {
                    // 配置类错误额外提示去哪里配置
                    if e.config_error {
                        (
                            Role::Error,
                            format!(
                                "{}{}",
                                e.message,
                                tr(self.lang, "ai.config_error_hint")
                            ),
                        )
                    } else {
                        (Role::Error, e.message)
                    }
                }
            };
            let commands = if role == Role::Assistant {
                ai::extract_commands(&content)
            } else {
                Vec::new()
            };
            self.push_message(ChatMessage {
                role,
                content,
                time: chrono::Local::now().format("%H:%M:%S").to_string(),
                commands,
            });
            changed = true;
        }
        changed
    }

    /// 抓取主窗口客户区，压到最长边 1280 后作为附件。
    ///
    /// 这一步仍在 UI 线程上（截屏要读窗口 DC，放到别的线程容易踩窗口消息的坑）：
    /// 代价被 `downscaled(1280)` 钉住了——一次几十毫秒、且是用户主动点击触发，
    /// 不会连续占用。真正的重活（用户选的任意大文件）已经挪到后台线程。
    fn capture_screenshot(&mut self) {
        if self.attachments.len() >= MAX_SCREENSHOTS {
            self.push_error(
                tr(self.lang, "ai.err_shot_max").replace("{n}", &MAX_SCREENSHOTS.to_string()),
            );
            return;
        }
        match crate::screenshot::capture_main_window() {
            Ok(cap) => {
                let cap = cap.downscaled(1280);
                match cap.to_png() {
                    Ok(png) => {
                        use base64::Engine as _;
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
                        self.attachments.push(Attachment {
                            name: chrono::Local::now().format("shot-%H%M%S.png").to_string(),
                            b64,
                            width: cap.width,
                            height: cap.height,
                        });
                    }
                    Err(e) => self
                        .push_error(tr(self.lang, "ai.err_shot_encode").replace("{e}", &e.to_string())),
                }
            }
            Err(e) => self
                .push_error(tr(self.lang, "ai.err_shot_capture").replace("{e}", &e.to_string())),
        }
    }

    /// 写一条错误消息（app 层的 Agent Abort 动作也走这里）。
    pub fn push_error(&mut self, msg: String) {
        self.push_message(ChatMessage {
            role: Role::Error,
            content: msg,
            time: chrono::Local::now().format("%H:%M:%S").to_string(),
            commands: Vec::new(),
        });
    }

    /// Agent 循环的进度/总结消息（app 层的状态机 Say 动作落到这里）。
    pub fn push_agent_message(&mut self, content: String) {
        self.push_message(ChatMessage {
            role: Role::Assistant,
            content,
            time: chrono::Local::now().format("%H:%M:%S").to_string(),
            commands: Vec::new(),
        });
    }
}

#[cfg(test)]
mod tests {
    // 刻意**不用** `use super::*`：本文件经父模块引入了 `gpui_kit::*` 通配，
    // 通配里的名字会盖掉内置的 `#[test]` 属性宏，展开时递归爆掉
    //（"recursion limit reached while expanding #[test]"）。显式导入最稳。
    use super::{
        clip_text_attachment, history_budget_chars, ingest_path, truncate_chars,
        AiAssistantPanel, ChatMessage, FilePayload, Role, AI_WAIT_TIMEOUT_SECS, FILE_MAX_BYTES,
        FILE_TEXT_MAX_CHARS, HISTORY_MSG_MAX_CHARS, IMAGE_MAX_DIM, MAX_MESSAGES,
    };
    use std::path::PathBuf;
    use std::sync::mpsc::channel;

    fn tmp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("target")
            .join("ai-assistant-test-tmp")
            .join(format!("{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn truncate_chars_never_splits_utf8() {
        // 中文按字符截断：绝不是按字节切（切出半个字符会让请求体变成非法 UTF-8）
        let s = "终端输出：中文测试 abc";
        let cut = truncate_chars(s, 4);
        assert_eq!(cut, "终端输出");
        assert_eq!(cut.chars().count(), 4);
        // emoji（多字节）也要整块保留
        let e = "🙂🙃😀x";
        assert_eq!(truncate_chars(e, 2), "🙂🙃");
        // 不超限就原样返回
        assert_eq!(truncate_chars("abc", 10), "abc");
    }

    #[test]
    fn history_budget_keeps_recent_turns_intact() {
        // 最近两条完整，更早的按上限截断（附件长文本只在本轮完整发送）
        assert_eq!(history_budget_chars(0, 10), HISTORY_MSG_MAX_CHARS);
        assert_eq!(history_budget_chars(7, 10), HISTORY_MSG_MAX_CHARS);
        assert_eq!(history_budget_chars(8, 10), usize::MAX);
        assert_eq!(history_budget_chars(9, 10), usize::MAX);
    }

    #[test]
    fn text_attachment_is_clipped_with_marker() {
        let long = "字".repeat(FILE_TEXT_MAX_CHARS + 500);
        let clipped = clip_text_attachment(long);
        assert!(clipped.chars().count() > FILE_TEXT_MAX_CHARS);
        assert!(clipped.contains("已截断"));
        // 短文本不动
        assert_eq!(clip_text_attachment("hello".into()), "hello");
    }

    #[test]
    fn ingest_rejects_big_and_empty_files_and_reads_text() {
        let dir = tmp_dir("ingest");

        // 文本文件：正常读入 + 内容一致
        let txt = dir.join("a.txt");
        std::fs::write(&txt, "hello 终端").unwrap();
        let f = ingest_path(&txt).expect("文本应可读");
        assert_eq!(f.name, "a.txt");
        match f.payload {
            FilePayload::Text(t) => assert_eq!(t, "hello 终端"),
            FilePayload::ImageB64(_) => panic!("文本被当成图片了"),
        }

        // 空文件：报错而不是静默忽略
        let empty = dir.join("empty.txt");
        std::fs::write(&empty, b"").unwrap();
        assert!(ingest_path(&empty).unwrap_err().contains("空文件"));

        // 超过上限的文件：报错（防止把 100MB 读进内存 + base64）
        let big = dir.join("big.bin");
        std::fs::write(&big, vec![0u8; (FILE_MAX_BYTES + 1) as usize]).unwrap();
        let err = ingest_path(&big).unwrap_err();
        assert!(err.contains("太大"), "实际: {err}");

        // 二进制非图片：给出可读错误
        let bin = dir.join("b.bin");
        std::fs::write(&bin, [0u8, 159, 146, 150, 0, 1]).unwrap();
        assert!(ingest_path(&bin).unwrap_err().contains("不支持"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ingest_downscales_oversized_png() {
        let dir = tmp_dir("png");
        // 造一张超过 IMAGE_MAX_DIM 的 PNG（纯色，压缩后很小）
        let img = image::RgbaImage::from_pixel(2400, 1200, image::Rgba([10, 20, 30, 255]));
        let path = dir.join("big.png");
        img.save(&path).unwrap();
        let f = ingest_path(&path).expect("PNG 应可读");
        match f.payload {
            FilePayload::ImageB64(b64) => {
                use base64::Engine as _;
                let png = base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .unwrap();
                let decoded = image::load_from_memory(&png).unwrap();
                // 等比：2400x1200 → 1600x800
                assert_eq!(decoded.width(), IMAGE_MAX_DIM);
                assert_eq!(decoded.height(), IMAGE_MAX_DIM / 2);
            }
            FilePayload::Text(_) => panic!("PNG 没有被当成图片"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disconnected_worker_clears_waiting_instead_of_hanging() {
        let mut panel = AiAssistantPanel::default();
        // 模拟"线程 panic 后通道断开"：发送端立刻析构
        let (tx, rx) = channel::<Result<String, super::AiError>>();
        drop(tx);
        panel.rx = Some(rx);
        panel.waiting = true;
        panel.wait_started = Some(std::time::Instant::now());

        assert!(panel.poll_async(), "必须有状态变化（否则永远不会重绘）");
        assert!(!panel.waiting, "通道断开后 waiting 必须解除");
        assert!(panel.rx.is_none());
        assert_eq!(panel.messages.len(), 1);
        assert_eq!(panel.messages[0].role, Role::Error);
        assert!(panel.messages[0].content.contains("异常退出"));
    }

    #[test]
    fn waiting_times_out_and_message_cap_holds() {
        let mut panel = AiAssistantPanel::default();
        // 通道还活着但永远不返回 → 超时后必须解除 waiting
        let (_tx, rx) = channel::<Result<String, super::AiError>>();
        panel.rx = Some(rx);
        panel.waiting = true;
        panel.wait_started = Some(
            std::time::Instant::now() - std::time::Duration::from_secs(AI_WAIT_TIMEOUT_SECS + 1),
        );
        assert!(panel.poll_async());
        assert!(!panel.waiting);
        assert!(panel.messages[0].content.contains("超时"));

        // 消息上限：无限追加只保留最近 MAX_MESSAGES 条
        for i in 0..(MAX_MESSAGES + 50) {
            panel.push_message(ChatMessage {
                role: Role::Assistant,
                content: format!("m{i}"),
                time: "00:00:00".into(),
                commands: Vec::new(),
            });
        }
        assert_eq!(panel.messages.len(), MAX_MESSAGES);
        assert_eq!(
            panel.messages.last().unwrap().content,
            format!("m{}", MAX_MESSAGES + 49)
        );
    }

    #[test]
    fn close_clears_one_shot_flags() {
        let mut panel = AiAssistantPanel::default();
        panel.open = true;
        panel.pending_send = true;
        panel.command_request = Some(("rm -rf /".into(), true));
        panel.close();
        assert!(!panel.open);
        assert!(!panel.pending_send, "关闭面板后不能留下待发送标记");
        assert!(panel.command_request.is_none(), "关闭面板后不能留下待执行命令");
        // toggle 用同一套清理逻辑
        panel.toggle();
        assert!(panel.open);
        panel.toggle();
        assert!(!panel.open);
    }
}
