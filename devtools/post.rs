//! 开发用：向 RustTerm 窗口投递鼠标/键盘消息（不依赖前台焦点）。
//!
//! 用法：
//!   postclick <x> <y>        # 客户区坐标点击
//!   postmove <x> <y>         # 仅移动鼠标
//!   postkey <key>            # 键名（enter / tab / ctrl+a / ctrl+c ...）
//!
//! 与 devtools/ui.rs 的区别：ui.rs 走 SetCursorPos + mouse_event，
//! 需要窗口真正在前台且没有被遮挡；这里直接 PostMessage 到窗口，
//! 因此在自动化环境里也稳定。

use anyhow::{anyhow, Result};
use std::sync::atomic::{AtomicIsize, Ordering};
use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{keybd_event, KEYEVENTF_KEYUP};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowTextW, IsWindowVisible, PostMessageW,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
};

static FOUND: AtomicIsize = AtomicIsize::new(0);

unsafe extern "system" fn cb(h: HWND, _l: isize) -> i32 {
    if IsWindowVisible(h) == 0 {
        return 1;
    }
    let mut buf = [0u16; 256];
    let len = GetWindowTextLengthW(h);
    let _ = GetWindowTextW(h, buf.as_mut_ptr(), 256);
    let title = String::from_utf16_lossy(&buf[..len.max(0) as usize]);
    if title.contains("RustTerm") {
        FOUND.store(h as isize, Ordering::Relaxed);
        return 0;
    }
    1
}

fn find_window() -> HWND {
    FOUND.store(0, Ordering::Relaxed);
    unsafe {
        EnumWindows(Some(cb), 0);
    }
    FOUND.load(Ordering::Relaxed) as HWND
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        return Err(anyhow!("usage: postclick|postmove|postkey ..."));
    }
    let hwnd = find_window();
    if hwnd.is_null() {
        return Err(anyhow!("RustTerm window not found"));
    }

    match args[1].as_str() {
        "postclick" | "postmove" => {
            let x: i32 = args[2].parse()?;
            let y: i32 = args[3].parse()?;
            let lp = ((y as u32) << 16) | (x as u32 & 0xFFFF);
            let lp = LPARAM::from(lp as i32 as isize);
            unsafe {
                PostMessageW(hwnd, WM_MOUSEMOVE, 0, lp);
            }
            std::thread::sleep(std::time::Duration::from_millis(80));
            if args[1] == "postclick" {
                unsafe {
                    PostMessageW(hwnd, WM_LBUTTONDOWN, 1, lp);
                    PostMessageW(hwnd, WM_LBUTTONUP, 0, lp);
                }
                println!("posted click ({x},{y})");
            } else {
                println!("posted move ({x},{y})");
            }
        }
        "postkey" => {
            let key = args.get(2).map(|s| s.as_str()).unwrap_or("enter");
            let (vk, ctrl) = match key {
                "enter" => (0x0Du8, false),
                "tab" => (0x09, false),
                "esc" => (0x1B, false),
                "backspace" => (0x08, false),
                "ctrl+a" => (0x41, true),
                "ctrl+c" => (0x43, true),
                "ctrl+v" => (0x56, true),
                "ctrl+n" => (0x4E, true),
                "ctrl+t" => (0x54, true),
                "ctrl+w" => (0x57, true),
                other => return Err(anyhow!("unknown key: {other}")),
            };
            if ctrl {
                unsafe {
                    keybd_event(0x11, 0, 0, 0);
                }
            }
            unsafe {
                keybd_event(vk, 0, 0, 0);
                keybd_event(vk, 0, KEYEVENTF_KEYUP, 0);
            }
            if ctrl {
                unsafe {
                    keybd_event(0x11, 0, KEYEVENTF_KEYUP, 0);
                }
            }
            println!("posted key {key}");
        }
        _ => return Err(anyhow!("unknown command: {}", args[1])),
    }
    Ok(())
}
