//! 统一图标辅助。
//!
//! gpui-kit 自带 Lucide 图标集（`gpui_kit::assets::IconName`），但组件层的
//! `Icon` 只接受 `gpui_component::IconName`（默认子集）。这里用
//! `Icon::default().path(...)` 绕过这个限制，从而让全项目（工具栏、侧栏、
//! SFTP、宏面板、文件浏览器……）共用同一套矢量图标，避免 emoji 混用导致的
//! 字号不齐、跨平台渲染差异等问题。
//!
//! 注意：图标资源必须在 `main.rs` 里通过
//! `gpui_kit::application().with_assets(gpui_kit::assets::AllAssets)` 注册，
//! 否则图标元素会渲染为空白。

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;

/// 用内置 Lucide 图标构造一个可直接放进元素树的 Icon。
pub fn ic(name: IconName) -> Icon {
    Icon::default().path(name.path())
}
