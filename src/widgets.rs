//! 通用小部件。
//!
//! 目前提供「带查看密码眼睛按钮」的密码输入框，统一全项目密码框的交互：
//! 默认密文显示，右侧眼睛图标点击后切换成明文，再点切回。
//!
//! 旧 egui 版的 `password_field` 在迁移到 GPUI 后由 `InputState::masked(true)`
//! 承担「密文渲染」，本模块补上 egui 版没有的「查看密码」开关。

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::Entity;

/// 带「查看密码」眼睛按钮的密码输入框。
///
/// - 眼睛按钮由 gpui-component 的 `Input::mask_toggle()` 提供：内置
///   Eye / EyeOff 矢量图标，随密文状态自动切换，点击即翻转掩码。
/// - 输入框本体必须以 `masked(true)` 创建（见 `app.rs::mk_password`），
///   否则「显示密码」会把明文又掩回去，语义是反的。
/// - 返回的是 `Input`，调用方可以继续链式设置宽度等样式，
///   例如 `password_field(&pw).flex_1()`。
pub fn password_field(state: &Entity<InputState>) -> Input {
    Input::new(state).mask_toggle()
}
