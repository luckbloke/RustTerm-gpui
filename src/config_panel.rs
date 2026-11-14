//! 「配置面板」对话框（GPUI 版）。
//!
//! 直接编辑 `AppConfig`，保存时写盘。数值项用文本框输入。
//! 面板关闭时丢弃输入实体，下次打开时从配置重新播种。

use crate::config::AppConfig;
use crate::i18n::I18n;

use gpui_kit::base::component_traits::Selectable as _;
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::theme::Theme;
use gpui_kit::*;

/// 配置面板的全部输入实体（关闭时整体丢弃）。
#[derive(Clone)]
pub struct ConfigInputs {
    pub font_size: Entity<InputState>,
    pub font_family: Entity<InputState>,
    pub keep_alive_interval: Entity<InputState>,
    pub ai_model: Entity<InputState>,
}

#[derive(Clone, Default)]
pub struct ConfigPanel {
    pub open: bool,
    pub inputs: Option<ConfigInputs>,
}

impl ConfigPanel {
    /// 渲染配置面板对话框。调用方以字段级借用传入 config / i18n。
    pub fn render(
        &mut self,
        config: &mut AppConfig,
        i18n: &mut I18n,
        window: &mut Window,
        cx: &mut Context<crate::app::RustTermApp>,
    ) -> Option<AnyElement> {
        if !self.open {
            self.inputs = None;
            return None;
        }
        if self.inputs.is_none() {
            let font_size = cx.new(|cx| {
                InputState::new(window, cx).placeholder("14".to_string())
            });
            font_size.update(cx, |s, cx| {
                s.set_value(format!("{}", config.font_size), window, cx)
            });
            let font_family = cx.new(|cx| InputState::new(window, cx).placeholder(String::new()));
            font_family.update(cx, |s, cx| {
                s.set_value(config.font_family.clone(), window, cx)
            });
            let keep_alive = cx.new(|cx| {
                InputState::new(window, cx).placeholder("30".to_string())
            });
            keep_alive.update(cx, |s, cx| {
                s.set_value(format!("{}", config.keep_alive_interval), window, cx)
            });
            let ai_model = cx.new(|cx| InputState::new(window, cx).placeholder(String::new()));
            ai_model.update(cx, |s, cx| s.set_value(config.ai.model.clone(), window, cx));
            self.inputs = Some(ConfigInputs {
                font_size,
                font_family,
                keep_alive_interval: keep_alive,
                ai_model,
            });
        }

        let theme = Theme::global(cx).clone();
        let dim = theme.colors.muted_foreground;
        let inputs = self.inputs.clone().unwrap();

        let field = |label: &str, input: &Entity<InputState>| {
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .child(div().w(px(110.0)).text_size(px(12.0)).child(label.to_string()))
                .child(Input::new(input).flex_1().text_size(px(12.0)))
        };

        let content = div()
            .v_flex()
            .gap_2()
            .max_h(px(520.0))
            .overflow_y_scrollbar()
            // ---- 常规 ----
            .child(div().text_size(px(12.0)).text_color(dim).child("🔧 常规设置"))
            .child(field("字体大小 (pt)", &inputs.font_size))
            .child(field("字体族", &inputs.font_family))
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(110.0)).text_size(px(12.0)).child("语言"))
                    .child(
                        Button::new("cfg-lang-zh")
                            .label("简体中文")
                            .compact()
                            .selected(config.language == "zh-CN")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.config.language = "zh-CN".into();
                                this.i18n.set_lang("zh-CN");
                                let _ = this.config.save();
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("cfg-lang-en")
                            .label("English")
                            .compact()
                            .selected(config.language == "en-US")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.config.language = "en-US".into();
                                this.i18n.set_lang("en-US");
                                let _ = this.config.save();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                Checkbox::new("cfg-status-bar")
                    .label("显示底部状态栏")
                    .checked(config.show_status_bar)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.config.show_status_bar = *checked;
                        cx.notify();
                    })),
            )
            .child(
                Checkbox::new("cfg-macros")
                    .label("启用键宏功能")
                    .checked(config.enable_key_macros)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.config.enable_key_macros = *checked;
                        cx.notify();
                    })),
            )
            .child(
                Checkbox::new("cfg-x11-auto-popup")
                    .label("X 程序启动时自动显示 X 屏幕")
                    .checked(config.x11_auto_popup)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.config.x11_auto_popup = *checked;
                        crate::config::set_x11_auto_popup(*checked);
                        let _ = this.config.save();
                        cx.notify();
                    })),
            )
            // ---- 连接 ----
            .child(div().text_size(px(12.0)).text_color(dim).child("🔗 连接设置"))
            .child(field("Keep-Alive (秒)", &inputs.keep_alive_interval))
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(110.0)).text_size(px(12.0)).child("输入协议"))
                    .child(
                        Button::new("cfg-xinput-xi2")
                            .label("XInput2（兼容）")
                            .compact()
                            .selected(config.xinput_mode == crate::config::XInputMode::Xi2)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.config.xinput_mode = crate::config::XInputMode::Xi2;
                                crate::config::set_xinput_mode(this.config.xinput_mode);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("cfg-xinput-core")
                            .label("核心事件（推荐）")
                            .compact()
                            .selected(config.xinput_mode == crate::config::XInputMode::Core)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.config.xinput_mode = crate::config::XInputMode::Core;
                                crate::config::set_xinput_mode(this.config.xinput_mode);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(dim)
                    .child("X11 输入协议：核心事件为默认（兼容排障）；XInput2 为事件推送。切换后对新连接生效——已有 X11 标签请重开。"),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(110.0)).text_size(px(12.0)).child("主题"))
                    .child(
                        Button::new("cfg-theme-dark")
                            .label("深色")
                            .compact()
                            .selected(config.theme == "dark")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.config.theme = "dark".into();
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("cfg-theme-light")
                            .label("浅色")
                            .compact()
                            .selected(config.theme == "light")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.config.theme = "light".into();
                                cx.notify();
                            })),
                    ),
            )
            // ---- AI ----
            .child(div().text_size(px(12.0)).text_color(dim).child("🤖 AI 设置"))
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(110.0)).text_size(px(12.0)).child("提供方"))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .child(config.ai.provider().label().to_string()),
                    ),
            )
            .child(field("模型", &inputs.ai_model))
            .child(
                Checkbox::new("cfg-ai-readonly")
                    .label("只读模式（禁用写命令）")
                    .checked(config.ai.read_only)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.config.ai.read_only = *checked;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("cfg-open-ai")
                    .label("打开 AI 配置面板…")
                    .compact()
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Err(e) = this.config.save() {
                            this.set_toast(e);
                        }
                        this.ai_config_panel.open = true;
                        this.config_panel.open = false;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("cfg-close")
                            .label("关闭")
                            .compact()
                            .on_click(cx.listener(|this, _, _, _| {
                                this.config_panel.open = false
                            })),
                    )
                    .child(
                        Button::new("cfg-save")
                            .label("保存并应用")
                            .compact()
                            .on_click(cx.listener(|this, _, window, cx| {
                                // 输入值回填
                                let font_size: f32 =
                                    read_num(&this.config_panel, |i| &i.font_size, cx, 14.0);
                                this.config.font_size = font_size.clamp(8.0, 32.0);
                                this.config.font_family =
                                    read_str(&this.config_panel, |i| &i.font_family, cx);
                                this.config.keep_alive_interval = read_num(
                                    &this.config_panel,
                                    |i| &i.keep_alive_interval,
                                    cx,
                                    30.0,
                                ) as u64;
                                this.config.ai.model =
                                    read_str(&this.config_panel, |i| &i.ai_model, cx);
                                this.config_panel.inputs = None;
                                this.config_panel.open = false;
                                // 心跳间隔立即生效（已在连的会话下个周期就按新值走）
                                crate::macros::apply_keep_alive(this.config.keep_alive_interval);
                                // 主题立即生效
                                let mode = if this.config.theme == "light" {
                                    gpui_kit::component::theme::ThemeMode::Light
                                } else {
                                    gpui_kit::component::theme::ThemeMode::Dark
                                };
                                gpui_kit::component::theme::Theme::change(mode, Some(window), cx);
                                if let Err(e) = this.config.save() {
                                    this.set_toast(e);
                                } else {
                                    this.set_toast("配置已保存");
                                }
                                cx.notify();
                            })),
                    ),
            );
        let _ = i18n;
        Some(Self::dialog(cx, "配置面板", 460.0, content))
    }

    fn dialog(cx: &App, title: &str, width: f32, content: impl IntoElement) -> AnyElement {
        let theme = Theme::global(cx).clone();
        div()
            .id(SharedString::from(format!("cfg-dialog-backdrop-{title}")))
            // 遮罩必须拦住鼠标，否则点击会漏到下层的终端，焦点被终端抢走，
            // 面板里的输入框就点不进去、也打不了字。
            .occlude()
            .absolute()
            .inset_0()
            .bg(hsla(0.0, 0.0, 0.0, 0.35))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id(SharedString::from(format!("cfg-dialog-card-{title}")))
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

/// 读取面板输入框文本。
fn read_str(
    panel: &ConfigPanel,
    pick: impl Fn(&ConfigInputs) -> &Entity<InputState>,
    cx: &App,
) -> String {
    panel
        .inputs
        .as_ref()
        .map(&pick)
        .map(|s| s.read(cx).value().to_string())
        .unwrap_or_default()
}

/// 读取面板输入框数值（失败用默认值）。
fn read_num(
    panel: &ConfigPanel,
    pick: impl Fn(&ConfigInputs) -> &Entity<InputState>,
    cx: &App,
    default: f32,
) -> f32 {
    read_str(panel, pick, cx).trim().parse().unwrap_or(default)
}
