//! 终端字体族选择。
//!
//! GPUI 在 Windows 上用系统字体栈（DirectWrite / font-kit），CJK 与 UI
//! 字体开箱即用，不再需要 egui 那样的手动注入。这里只负责给终端网格
//! 挑一个等宽字体族名（Consolas → Cascadia 回退）。

/// 终端等宽字体族名（第一个存在的生效；GPUI 找不到族名会回退默认）。
pub const MONO_FAMILIES: [&str; 3] = ["Consolas", "Cascadia Mono", "Cascadia Code"];

/// 返回终端使用的字体族名。
pub fn terminal_font_family() -> &'static str {
    #[cfg(windows)]
    {
        for fam in MONO_FAMILIES {
            let path = match fam {
                "Consolas" => r"C:\Windows\Fonts\consola.ttf",
                "Cascadia Mono" => r"C:\Windows\Fonts\CascadiaMono.ttf",
                _ => r"C:\Windows\Fonts\CascadiaCode.ttf",
            };
            if std::path::Path::new(path).exists() {
                return fam;
            }
        }
    }
    "monospace"
}
