//! Win32 主窗口句柄注册表。
//!
//! gpui 的 Window 实现了 `HasWindowHandle`，启动后把 HWND 存进全局，
//! 供浏览器子窗口定位（wry）、截图（GDI）与拖拽落点换算共用。
//!
//! 旧 egui 版本用 FindWindowW("RustTerm Egui") 按标题找窗口，
//! 这里改为启动时显式注册，避免标题改动后失效。

use std::sync::OnceLock;

static MAIN_HWND: OnceLock<isize> = OnceLock::new();

/// 注册主窗口 HWND（重复调用忽略）。
pub fn register_main_hwnd(hwnd: isize) {
    let _ = MAIN_HWND.set(hwnd);
}

/// 主窗口 HWND（未注册时返回 None）。
pub fn main_hwnd() -> Option<isize> {
    MAIN_HWND.get().copied()
}

/// 把键盘焦点交回主窗口。
///
/// WebView2 子窗口创建后会抢占焦点，导致主窗口的输入框收不到键盘事件。
/// 点击地址栏 / 切回主窗口时必须抢回来。
pub fn focus_main_window() {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    if let Some(hwnd) = main_hwnd() {
        unsafe {
            SetFocus(hwnd as _);
        }
    }
}

/// 隐藏本地鼠标光标（远程桌面用）。
///
/// 远程桌面画面里已经带了远端系统绘制的光标，如果本地系统光标还显示，
/// 用户就会看到**两个鼠标**（一个跟着真实鼠标动，一个画在远端画面上）。
///
/// 实现说明：Windows 在每次鼠标移动时都会发 WM_SETCURSOR，gpui 的窗口过程
/// 收到后会把光标重置成样式里的那个（默认箭头）。WM_SETCURSOR 是**先于**
/// WM_MOUSEMOVE 处理的，所以我们在鼠标移动回调里再 `SetCursor(NULL)`，
/// 就能盖掉它 —— 效果等价于 CSS 的 `cursor: none`。
///
/// 鼠标移出远程桌面区域后，gpui 下一次处理 WM_SETCURSOR 会自动恢复箭头，
/// 不需要手动还原。
#[cfg(windows)]
pub(crate) fn hide_cursor() {
    use windows_sys::Win32::UI::WindowsAndMessaging::SetCursor;
    unsafe {
        SetCursor(std::ptr::null_mut());
    }
}

#[cfg(not(windows))]
pub(crate) fn hide_cursor() {}

#[cfg(windows)]
pub(crate) fn raw_hwnd_from_window(window: &gpui_kit::Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // Window 有同名固有方法 window_handle() -> AnyWindowHandle，
    // 必须显式调用 HasWindowHandle trait 方法拿 raw window handle。
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(win) => Some(win.hwnd.get() as isize),
        _ => None,
    }
}

#[cfg(not(windows))]
pub(crate) fn raw_hwnd_from_window(_window: &gpui_kit::Window) -> Option<isize> {
    None
}

/// 本机 CapsLock 是否开启。
///
/// 远程桌面要知道这个状态：RFB 与 RDP 都**没有**「远端锁定键状态」的回传
/// 通道（没有 LED 反馈），客户端只能假定远端初始为关，之后发现本地开灯/关灯
/// 就补发一次 Caps_Lock 让远端跟上。否则会出现「本地大写、远端小写」
/// 的输入错位。
///
/// 用 `GetKeyState` 而不是 `GetAsyncKeyState`：调用点在 UI 线程的按键回调里，
/// 此时系统已经把这次按键的 toggle 状态写进了本线程的输入队列，
/// `GetKeyState` 读到的就是「按下之后」的状态，直接可用于同步。
#[cfg(windows)]
pub(crate) fn caps_lock_on() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CAPITAL};
    unsafe { GetKeyState(VK_CAPITAL as i32) & 1 != 0 }
}

#[cfg(not(windows))]
pub(crate) fn caps_lock_on() -> bool {
    false
}
