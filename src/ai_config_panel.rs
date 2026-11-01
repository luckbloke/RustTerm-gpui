//! 「AI 配置」面板（GPUI 版）：布局与字段对齐设计稿。
//!
//! 字段顺序：提供方 → API Key → Base URL → 模型 → aiMaxTokens →
//! aiTemperature → 只读模式 → 对话历史最多保留 → 终端上下文最近行数 →
//! 上下文单行最大字符数。

use crate::config::{AiProvider, AppConfig};

use gpui_kit::base::component_traits::Selectable as _;
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::theme::Theme;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

/// AI 配置面板输入实体（关闭时整体丢弃）。
#[derive(Clone)]
pub struct AiConfigInputs {
    pub api_key: Entity<InputState>,
    pub base_url: Entity<InputState>,
    pub model: Entity<InputState>,
    pub max_tokens: Entity<InputState>,
    pub temperature: Entity<InputState>,
    pub max_history: Entity<InputState>,
    pub context_lines: Entity<InputState>,
    pub context_max_line_len: Entity<InputState>,
}

#[derive(Clone, Default)]
pub struct AiConfigPanel {
    pub open: bool,
    /// API Key 明文显示开关
    pub show_key: bool,
    pub inputs: Option<AiConfigInputs>,
}

impl AiConfigPanel {
    /// 渲染 AI 配置面板对话框。调用方以字段级借用传入 config。
    pub fn render(
        &mut self,
        config: &mut AppConfig,
        window: &mut Window,
        cx: &mut Context<crate::app::RustTermApp>,
    ) -> Option<AnyElement> {
        if !self.open {
            self.inputs = None;
            return None;
        }
        if self.inputs.is_none() {
            let mut mk = |text: String, placeholder: &str| {
                let e = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.to_string()));
                e.update(cx, |s, cx| s.set_value(text, window, cx));
                e
            };
            let ai = &config.ai;
            self.inputs = Some(AiConfigInputs {
                api_key: mk(ai.api_key.clone(), "sk-..."),
                base_url: mk(ai.base_url.clone(), ai.provider().default_base_url()),
                model: mk(ai.model.clone(), ai.provider().default_model()),
                max_tokens: mk(format!("{}", ai.max_tokens), "4096"),
                temperature: mk(format!("{}", ai.temperature), "0.7"),
                max_history: mk(format!("{}", ai.max_history), "20"),
                context_lines: mk(format!("{}", ai.context_lines), "50"),
                context_max_line_len: mk(format!("{}", ai.context_max_line_len), "500"),
            });
        }
        let inputs = self.inputs.clone().unwrap();
        let ai = &config.ai;
        let theme = Theme::global(cx).clone();
        let dim = theme.colors.muted_foreground;

        let field = |label: &str, input: &Entity<InputState>| {
            div()
                .v_flex()
                .gap_0p5()
                .child(div().text_size(px(11.0)).text_color(dim).child(label.to_string()))
                .child(Input::new(input).text_size(px(12.0)))
        };

        // ---- 提供方选择按钮行 ----
        let mut provider_row = div()
            .h_flex()
            .items_center()
            .gap_1()
            .child(div().text_size(px(11.0)).text_color(dim).child("提供方"));
        for p in AiProvider::ALL {
            let id = p.id().to_string();
            provider_row = provider_row.child(
                Button::new(SharedString::from(format!("ai-p-{}", p.id())))
                    .label(p.label())
                    .compact()
                    .selected(ai.provider == id)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let prev = this.config.ai.provider();
                        this.config.ai.provider = id.clone();
                        let new_p = this.config.ai.provider();
                        if new_p.should_reset_url(&this.config.ai.base_url, prev) {
                            this.config.ai.base_url = new_p.default_base_url().to_string();
                        }
                        if new_p.should_reset_model(&this.config.ai.model, prev) {
                            this.config.ai.model = new_p.default_model().to_string();
                        }
                        // 输入框播种失效，下次打开重建
                        this.ai_config_panel.inputs = None;
                        cx.notify();
                    })),
            );
        }

        let key_hint = match ai.provider() {
            AiProvider::Ollama => "Ollama 不需要 Key",
            _ => "sk-...",
        };

        let content = div()
            .v_flex()
            .gap_2()
            .max_h(px(560.0))
            .overflow_y_scrollbar()
            .child(provider_row)
            .child(field("API Key", &inputs.api_key))
            .when(self.show_key, |d| {
                d.child(
                    div()
                        .text_size(px(11.0))
                        .text_color(dim)
                        .child(ai.api_key.clone()),
                )
            })
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .child(
                        Button::new("ai-key-show")
                            .label(if self.show_key { "隐藏" } else { "明文" })
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.ai_config_panel.show_key = !this.ai_config_panel.show_key;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(dim)
                            .child(key_hint.to_string()),
                    ),
            )
            .child(field("Base URL", &inputs.base_url))
            .child(field("模型", &inputs.model))
            .child(div().border_t_1().border_color(theme.colors.border))
            .child(field("aiMaxTokens", &inputs.max_tokens))
            .child(field("aiTemperature", &inputs.temperature))
            .child(
                Checkbox::new("aicfg-readonly")
                    .label("只读模式（禁用写命令）")
                    .checked(ai.read_only)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.config.ai.read_only = *checked;
                        cx.notify();
                    })),
            )
            .child(field("对话历史最多保留（条）", &inputs.max_history))
            .child(field("终端上下文最近行数", &inputs.context_lines))
            .child(field("上下文单行最大字符数", &inputs.context_max_line_len))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(dim)
                    .child("API Key 仅保存在本地。Ollama 不需要 Key。"),
            )
            .child(
                div()
                    .h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("aicfg-cancel")
                            .label("取消")
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| {
                                this.ai_config_panel.open = false;
                                this.ai_config_panel.inputs = None;
                            })),
                    )
                    .child(
                        Button::new("aicfg-save")
                            .label("保存")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.ai_config_save(cx);
                            })),
                    ),
            );
        Some(Self::dialog(cx, "🤖 AI 配置", 400.0, content))
    }

    fn dialog(cx: &App, title: &str, width: f32, content: impl IntoElement) -> AnyElement {
        let theme = Theme::global(cx).clone();
        div()
            .id(SharedString::from(format!("ai-dialog-backdrop-{title}")))
            // 遮罩必须拦住鼠标，否则点击漏到下层终端后焦点会被终端抢走。
            .occlude()
            .absolute()
            .inset_0()
            .bg(hsla(0.0, 0.0, 0.0, 0.35))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id(SharedString::from(format!("ai-dialog-card-{title}")))
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
                    .child(div().text_size(px(15.0)).child(title.to_string()))
                    .child(content),
            )
            .into_any_element()
    }
}

/// 保存 AI 配置：输入值回填 → 写盘。
impl crate::app::RustTermApp {
    pub(crate) fn ai_config_save(&mut self, cx: &mut Context<Self>) {
        fn get(
            panel: &AiConfigPanel,
            pick: &dyn Fn(&AiConfigInputs) -> &Entity<InputState>,
            cx: &App,
        ) -> String {
            panel
                .inputs
                .as_ref()
                .map(|i| pick(i).read(cx).value().to_string())
                .unwrap_or_default()
        }
        let panel = &self.ai_config_panel;
        self.config.ai.api_key = get(panel, &|i| &i.api_key, cx);
        self.config.ai.base_url = get(panel, &|i| &i.base_url, cx);
        self.config.ai.model = get(panel, &|i| &i.model, cx);
        self.config.ai.max_tokens = get(panel, &|i| &i.max_tokens, cx)
            .trim()
            .parse::<f64>()
            .map(|v| v.clamp(1.0, 1_000_000.0) as u32)
            .unwrap_or(self.config.ai.max_tokens);
        self.config.ai.temperature = get(panel, &|i| &i.temperature, cx)
            .trim()
            .parse()
            .map(|v: f32| v.clamp(0.0, 2.0))
            .unwrap_or(self.config.ai.temperature);
        self.config.ai.max_history = get(panel, &|i| &i.max_history, cx)
            .trim()
            .parse::<f64>()
            .map(|v| v.clamp(1.0, 1000.0) as usize)
            .unwrap_or(self.config.ai.max_history);
        self.config.ai.context_lines = get(panel, &|i| &i.context_lines, cx)
            .trim()
            .parse::<f64>()
            .map(|v| v.clamp(0.0, 5000.0) as usize)
            .unwrap_or(self.config.ai.context_lines);
        self.config.ai.context_max_line_len = get(panel, &|i| &i.context_max_line_len, cx)
            .trim()
            .parse::<f64>()
            .map(|v| v.clamp(1.0, 100_000.0) as usize)
            .unwrap_or(self.config.ai.context_max_line_len);
        self.ai_config_panel.open = false;
        self.ai_config_panel.inputs = None;
        match self.config.save() {
            Ok(_) => self.set_toast("AI 配置已保存"),
            Err(e) => self.set_toast(e),
        }
        cx.notify();
    }
}
