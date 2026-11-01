//! AI 助手面板（右侧停靠式，GPUI 版）。
//!
//! - 顶部栏：标题 + Agent 开关 + 截图 / 清空 / 设置 / 关闭按钮
//! - 对话区：用户消息气泡、助手消息气泡（命令附「发送到终端」）
//! - 底部：输入框 + 发送按钮（Enter 发送）
//! - 后端：多提供方（OpenAI 兼容 / Ollama / Anthropic），阻塞调用放线程，
//!   结果经通道回传。

use crate::ai::{self, AiError, AiMessage};
use crate::config::AiSettings;
use crate::ui_icons::ic;
use std::sync::mpsc::{channel, Receiver};

use gpui_kit::assets::IconName;
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
pub struct FileAttachment {
    pub name: String,
    pub payload: FilePayload,
}

/// 文件附件的载荷形态。
pub enum FilePayload {
    /// 文本文件：内容以代码块内联进 prompt
    Text(String),
    /// 图片文件：统一转成 PNG base64（不含 data: 前缀），走视觉通路
    ImageB64(String),
}

/// 文本附件截断字符数（几十 KB 足够 AI 理解，过多会挤爆 token）。
const FILE_TEXT_MAX_CHARS: usize = 64_000;

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
        }
    }
}

impl AiAssistantPanel {
    /// 取走「待发送」标记（app 层据此附加终端上下文后真正调用 send）。
    pub fn take_pending_send(&mut self) -> bool {
        std::mem::take(&mut self.pending_send)
    }

    /// 右侧停靠面板。None = 面板关闭。
    pub fn render(
        &mut self,
        ai: &AiSettings,
        window: &mut Window,
        cx: &mut Context<crate::app::RustTermApp>,
    ) -> Option<AnyElement> {
        if !self.open {
            self.rx = None;
            return None;
        }
        self.poll_result();

        let theme = Theme::global(cx).clone();
        let border = theme.colors.border;
        let dim = theme.colors.muted_foreground;

        // 输入框实体惰性创建；Change 订阅同步到 self.input
        if self.input_state.is_none() {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("问点什么...（Enter 发送）".to_string())
            });
            cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.ai_assistant.input = this
                        .ai_assistant
                        .input_state
                        .as_ref()
                        .map(|s| s.read(cx).value().to_string())
                        .unwrap_or_default();
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
                .child(div().text_size(px(13.0)).child("AI 助手"))
                .child(
                    Checkbox::new("ai-agent")
                        .label("Agent")
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
                        .tooltip("截取屏幕并附加")
                        .compact()
                        .on_click(cx.listener(|this, _, _, _| {
                            this.ai_assistant.capture_screenshot();
                        })),
                )
                .child(
                    Button::new("ai-clear")
                        .icon(ic(IconName::Trash))
                        .tooltip("清空对话")
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
                        .tooltip("AI 设置")
                        .compact()
                        .on_click(cx.listener(|this, _, _, _| {
                            this.ai_config_panel.open = true;
                        })),
                )
                .child(
                    Button::new("ai-close")
                        .icon(ic(IconName::X))
                        .tooltip("关闭 AI 面板")
                        .compact()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ai_assistant.open = false;
                            cx.notify();
                        })),
                ),
        );

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
                            .child("🔒 只读"),
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
                        .child("你好，我是 RustTerm AI 助手"),
                )
                .child(
                    div()
                        .text_center()
                        .text_size(px(11.0))
                        .text_color(dim)
                        .child("可以询问运维命令、排查问题、分析日志等"),
                );
            if ai.api_key.trim().is_empty() && ai.provider().needs_api_key() {
                chat = chat.child(
                    div()
                        .text_center()
                        .text_size(px(11.0))
                        .text_color(theme.colors.info)
                        .child("ℹ 当前未填 API Key，将按无鉴权方式请求（本地网关可用）"),
                );
            }
        }

        // 快照消息，避免渲染时借用冲突
        let snapshot: Vec<ChatMessage> = self.messages.clone();
        for msg in &snapshot {
            let color = match msg.role {
                Role::User => theme.colors.button_primary,
                Role::Assistant => theme.colors.secondary,
                Role::Error => theme.colors.danger,
            };
            let fg = match msg.role {
                Role::User => theme.colors.button_primary_foreground,
                _ => theme.colors.foreground,
            };
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
                            .child(div().min_w_0().child(msg.content.clone())),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(dim)
                            .child(msg.time.clone()),
                    ),
            );

            // 任务完成标记 + 命令按钮（仅助手消息）
            if msg.role == Role::Assistant {
                if ai::is_task_done(&msg.content) {
                    chat = chat.child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.colors.success)
                            .child("✅ 任务已完成"),
                    );
                }
                for (ci, cmd) in msg.commands.iter().enumerate() {
                    let dangerous = ai::is_dangerous(cmd);
                    let is_write = ai::is_write_command(cmd);
                    let blocked = ai.read_only && is_write;
                    let cmd_text = cmd.clone();
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
                                        .child("🔒 只读模式已禁用"),
                                )
                            })
                            .when(!blocked && dangerous, |d| {
                                d.child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(theme.colors.danger)
                                        .child("⚠ 危险"),
                                )
                            })
                            .when(!blocked && !dangerous, |d| {
                                d.child(
                                    Button::new(SharedString::from(format!(
                                        "ai-send-cmd-{ci}"
                                    )))
                                    .icon(ic(IconName::Play))
                                    .label("发送到终端")
                                    .compact()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.ai_assistant.command_request =
                                            Some((cmd_text.clone(), false));
                                        cx.notify();
                                    })),
                                )
                            }),
                    );
                }
            }
        }

        if self.waiting {
            chat = chat.child(
                div()
                    .text_color(dim)
                    .child("思考中…"),
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
                        .child(format!(
                            "📎 发送时自动附带：{hint} + 最近 {} 行终端输出",
                            ai.context_lines
                        )),
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
                    FilePayload::Text(t) => format!("{} 字", t.len()),
                    FilePayload::ImageB64(b) => format!("图片 {} KB", b.len() * 3 / 4 / 1024),
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
                            .label(format!("📄 {name}（{size}）点击移除"))
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
                        .label(format!("🖼 {name}（{w}×{h}）点击移除"))
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
                            .label("附加")
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| {
                                this.ai_assistant.pick_files();
                            })),
                    )
                    .child(
                        Button::new("ai-send")
                            .icon(ic(IconName::Send))
                            .label("发送")
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| {
                                this.ai_assistant.pending_send = true;
                            })),
                    )
                    .child(
                        Button::new("ai-input-clear")
                            .label("清空输入")
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
                            .child(format!("历史 {} 条", self.messages.len())),
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
        // 输入框控件（InputState）显示的内容必须一起清，否则界面上的
        // 文字会残留在框里（self.input 只是内部缓冲）。
        if let Some(s) = &self.input_state {
            s.update(cx, |s, cx| s.set_value(String::new(), window, cx));
        }
        self.input.clear();

        // 组装发给模型的消息（含终端上下文）
        let mut history: Vec<AiMessage> = Vec::new();
        let recent: Vec<&ChatMessage> = self.messages.iter().collect();
        let keep = ai.max_history.max(1);
        let start = recent.len().saturating_sub(keep);
        for m in &recent[start..] {
            if m.role == Role::Error {
                continue;
            }
            let role = if m.role == Role::User { "user" } else { "assistant" };
            history.push(AiMessage {
                role: role.to_string(),
                content: m.content.clone(),
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
        self.messages.push(ChatMessage {
            role: Role::User,
            content: display,
            time: chrono::Local::now().format("%H:%M:%S").to_string(),
            commands: Vec::new(),
        });

        // 预检配置（API Key 允许为空：本地无鉴权网关常见）
        if !ai.has_endpoint() {
            self.push_error("未配置 Base URL，请点击右上角 ⚙ 打开 AI 配置。".into());
            return;
        }
        if ai.model.trim().is_empty() {
            self.push_error("尚未配置模型名，请点击右上角 ⚙ 打开 AI 配置。".into());
            return;
        }

        self.attachments.clear();
        self.files.clear();
        let agent_mode = self.agent_mode;
        let cfg = ai.clone();
        let (tx, rx) = channel::<Result<String, AiError>>();
        self.rx = Some(rx);
        self.waiting = true;

        std::thread::spawn(move || {
            let result = ai::chat(&cfg, agent_mode, &history);
            let _ = tx.send(result);
        });
    }

    /// 打开多选文件对话框，选中的文件附加到对话。
    fn pick_files(&mut self) {
        let Some(paths) = rfd::FileDialog::new()
            .set_title("选择要附加的文件")
            .pick_files()
        else {
            return;
        };
        for p in paths {
            self.ingest_file(&p);
        }
    }

    /// 读取并附加一个文件：PNG 直接用；JPEG 转 PNG；其余按文本尝试，失败报错。
    fn ingest_file(&mut self, path: &std::path::Path) {
        use base64::Engine as _;
        // 附件总数（截图 + 文件）上限，避免一次消息塞爆
        if self.attachments.len() + self.files.len() >= 6 {
            self.push_error("附件太多（最多 6 个：截图 + 文件），先移除一些再试。".into());
            return;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
            return;
        };
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                self.push_error(format!("读取 {} 失败：{e}", name));
                return;
            }
        };
        if bytes.is_empty() {
            self.push_error(format!("{name} 是空文件，跳过。"));
            return;
        }

        // ---- 图片文件：PNG 直接用；JPEG 用 image crate 转 PNG ----
        if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
            self.files.push(FileAttachment {
                name,
                payload: FilePayload::ImageB64(
                    base64::engine::general_purpose::STANDARD.encode(&bytes),
                ),
            });
            return;
        }
        if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            match image::load_from_memory(&bytes) {
                Ok(img) => {
                    let mut png_buf: Vec<u8> = Vec::new();
                    match image::DynamicImage::ImageRgb8(img.into_rgb8()).write_to(
                        &mut std::io::Cursor::new(&mut png_buf),
                        image::ImageFormat::Png,
                    ) {
                        Ok(()) => {
                            self.files.push(FileAttachment {
                                name,
                                payload: FilePayload::ImageB64(
                                    base64::engine::general_purpose::STANDARD.encode(&png_buf),
                                ),
                            });
                            return;
                        }
                        Err(e) => {
                            self.push_error(format!("{name} 转换 PNG 失败：{e}"));
                            return;
                        }
                    }
                }
                Err(e) => {
                    self.push_error(format!("解析图片 {name} 失败：{e}"));
                    return;
                }
            }
        }

        // ---- 文本文件：UTF-8（或近 ASCII）→ 以代码块内联 ----
        match String::from_utf8(bytes) {
            Ok(text) => {
                let mut text = text;
                if text.chars().count() > FILE_TEXT_MAX_CHARS {
                    let total = text.chars().count();
                    text = text.chars().take(FILE_TEXT_MAX_CHARS).collect();
                    text.push_str(&format!(
                        "\n（……内容过长已截断，共 {total} 字符，仅展示前 {} 个）",
                        FILE_TEXT_MAX_CHARS
                    ));
                }
                self.files.push(FileAttachment {
                    name,
                    payload: FilePayload::Text(text),
                });
            }
            Err(_) => {
                self.push_error(format!(
                    "{name} 不是文本文件（也不是 PNG/JPEG 图片），暂不支持。"
                ));
            }
        }
    }

    /// 抓取主窗口客户区，压到最长边 1280 后作为附件。
    fn capture_screenshot(&mut self) {
        if self.attachments.len() >= 3 {
            self.push_error("最多附加 3 张截图，先移除一张再截。".into());
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
                    Err(e) => self.push_error(format!("截图编码失败: {e}")),
                }
            }
            Err(e) => self.push_error(format!("截图失败: {e}")),
        }
    }

    fn push_error(&mut self, msg: String) {
        self.messages.push(ChatMessage {
            role: Role::Error,
            content: msg,
            time: chrono::Local::now().format("%H:%M:%S").to_string(),
            commands: Vec::new(),
        });
    }

    fn poll_result(&mut self) {
        let got = match &self.rx {
            Some(rx) => rx.try_recv().ok(),
            None => None,
        };
        if let Some(result) = got {
            self.rx = None;
            self.waiting = false;
            let (role, content) = match result {
                Ok(text) => (Role::Assistant, text),
                Err(e) => {
                    // 配置类错误额外提示去哪里配置
                    if e.config_error {
                        (Role::Error, format!("{}\n\n（点击右上角 ⚙ 打开 AI 配置）", e.message))
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
            self.messages.push(ChatMessage {
                role,
                content,
                time: chrono::Local::now().format("%H:%M:%S").to_string(),
                commands,
            });
        }
    }
}
