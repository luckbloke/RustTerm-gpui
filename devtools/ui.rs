//! 开发用 UI 驱动工具：给 RustTerm 窗口发鼠标/键盘事件。
//!
//! 用法：
//!   ui click <x> <y>      # 客户区坐标点击
//!   ui type <text>        # 输入文本
//!   ui key <key>          # 键名（enter / tab / ctrl+a ...）
//!   ui hover <x> <y>      # 仅移动鼠标
//!
//! 坐标是客户区逻辑像素（相对窗口左上角）。

use anyhow::{anyhow, Result};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    keybd_event, mouse_event, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, IsWindowVisible, SetCursorPos, SetForegroundWindow,
};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        return Err(anyhow!("用法: ui <click|type|key|hover> ..."));
    }
    let hwnd = find_window();
    if hwnd.is_null() {
        return Err(anyhow!("找不到 RustTerm 窗口"));
    }
    // 抢前台失败不致命：后续的鼠标点击本身就会把键盘焦点交给目标窗口
    // （WebView2 子窗口常常占着前台，SetForegroundWindow 会被系统拒绝）。
    let _ = force_foreground(hwnd);
    std::thread::sleep(std::time::Duration::from_millis(250));

    match args[1].as_str() {
        "click" => {
            let x: i32 = args[2].parse()?;
            let y: i32 = args[3].parse()?;
            let right = args.get(4).map(|s| s == "right").unwrap_or(false);
            move_cursor(hwnd, x, y)?;
            std::thread::sleep(std::time::Duration::from_millis(80));
            if right {
                unsafe {
                    mouse_event(MOUSEEVENTF_RIGHTDOWN, 0, 0, 0, 0);
                    mouse_event(MOUSEEVENTF_RIGHTUP, 0, 0, 0, 0);
                }
            } else {
                unsafe {
                    mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
                    mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
                }
            }
            println!("clicked ({x},{y})");
        }
        "hover" => {
            let x: i32 = args[2].parse()?;
            let y: i32 = args[3].parse()?;
            move_cursor(hwnd, x, y)?;
            println!("hovered ({x},{y})");
        }
        // 拖拽：按下 → 分步移动 → 抬起（用于测试终端选区复制）
        "drag" => {
            let (x1, y1): (i32, i32) = (args[2].parse()?, args[3].parse()?);
            let (x2, y2): (i32, i32) = (args[4].parse()?, args[5].parse()?);
            move_cursor(hwnd, x1, y1)?;
            std::thread::sleep(std::time::Duration::from_millis(80));
            unsafe {
                mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
            }
            let steps = 12;
            for i in 1..=steps {
                let x = x1 + (x2 - x1) * i / steps;
                let y = y1 + (y2 - y1) * i / steps;
                move_cursor(hwnd, x, y)?;
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
            unsafe {
                mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
            }
            println!("dragged ({x1},{y1}) -> ({x2},{y2})");
        }
        "type" => {
            let text = args[2..].join(" ");
            for ch in text.chars() {
                type_char(ch);
                std::thread::sleep(std::time::Duration::from_millis(8));
            }
            println!("typed");
        }
        "key" => {
            let k = args.get(2).map(|s| s.as_str()).unwrap_or("enter");
            press_key(k);
            println!("key {k}");
        }
        other => return Err(anyhow!("未知命令: {other}")),
    }
    Ok(())
}

fn move_cursor(hwnd: HWND, x: i32, y: i32) -> Result<()> {
    let mut p = windows_sys::Win32::Foundation::POINT { x, y };
    unsafe {
        ClientToScreen(hwnd, &mut p);
        SetCursorPos(p.x, p.y);
    }
    Ok(())
}

fn type_char(ch: char) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        KEYEVENTF_UNICODE, KEYEVENTF_KEYUP as KU, VIRTUAL_KEY,
    };
    unsafe {
        let mut down = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT {
                wVk: 0,
                wScan: ch as u16,
                dwFlags: KEYEVENTF_UNICODE,
                time: 0,
                dwExtraInfo: 0,
            }},
        };
        let mut up = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT {
                wVk: 0,
                wScan: ch as u16,
                dwFlags: KEYEVENTF_UNICODE | KU,
                time: 0,
                dwExtraInfo: 0,
            }},
        };
        SendInput(1, &mut down, std::mem::size_of::<INPUT>() as i32);
        SendInput(1, &mut up, std::mem::size_of::<INPUT>() as i32);
        let _ = VIRTUAL_KEY::default;
    }
}

/// 发送按键：`ctrl+shift+c` / `enter` / `f5` 这类组合一律走虚拟键码。
///
/// 不能用 KEYEVENTF_UNICODE 代替：GPUI 只认真实 WM_KEYDOWN 里的 VK，
/// Unicode 注入只会产生 WM_CHAR，按键分发改发不到监听器。
fn press_key(name: &str) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{keybd_event, KEYEVENTF_KEYUP as KU};

    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;
    let mut key = name.to_ascii_lowercase();
    loop {
        if let Some(rest) = key.strip_prefix("ctrl+") {
            ctrl = true;
            key = rest.to_string();
        } else if let Some(rest) = key.strip_prefix("shift+") {
            shift = true;
            key = rest.to_string();
        } else if let Some(rest) = key.strip_prefix("alt+") {
            alt = true;
            key = rest.to_string();
        } else {
            break;
        }
    }

    let vk = match key.as_str() {
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "backspace" => 0x08,
        "delete" => 0x2E,
        "insert" => 0x2D,
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "space" => 0x20,
        "f1" => 0x70,
        "f2" => 0x71,
        "f3" => 0x72,
        "f4" => 0x73,
        "f5" => 0x74,
        "f6" => 0x75,
        "f11" => 0x7A,
        "f12" => 0x7B,
        other => {
            let mut it = other.chars();
            match (it.next(), it.next()) {
                (Some(c), None) if c.is_ascii_alphanumeric() => {
                    c.to_ascii_uppercase() as u8
                }
                _ => {
                    println!("无法识别的键名: {name}");
                    return;
                }
            }
        }
    };

    unsafe {
        if ctrl {
            keybd_event(0x11, 0, 0, 0);
        }
        if shift {
            keybd_event(0x10, 0, 0, 0);
        }
        if alt {
            keybd_event(0x12, 0, 0, 0);
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
        keybd_event(vk, 0, 0, 0);
        std::thread::sleep(std::time::Duration::from_millis(30));
        keybd_event(vk, 0, KU, 0);
        std::thread::sleep(std::time::Duration::from_millis(20));
        if alt {
            keybd_event(0x12, 0, KU, 0);
        }
        if shift {
            keybd_event(0x10, 0, KU, 0);
        }
        if ctrl {
            keybd_event(0x11, 0, KU, 0);
        }
    }
}

/// 强制把窗口切到前台（ALT 技巧 + AttachThreadInput）。
fn force_foreground(hwnd: HWND) -> bool {
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{keybd_event, KEYEVENTF_KEYUP};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, ShowWindow, SW_RESTORE,
    };
    unsafe {
        keybd_event(0x12, 0, 0, 0);
        keybd_event(0x12, 0, KEYEVENTF_KEYUP, 0);
        let fg = GetForegroundWindow();
        let fg_thread = GetWindowThreadProcessId(fg, std::ptr::null_mut());
        let my_thread = GetCurrentThreadId();
        if fg_thread != 0 && fg_thread != my_thread {
            AttachThreadInput(my_thread, fg_thread, 1);
        }
        // 只在最小化时恢复：若窗口处于最大化，SW_RESTORE 会把它还原成
        // 较小尺寸并移动位置，导致后续按客户端坐标点击落到别处。
        if windows_sys::Win32::UI::WindowsAndMessaging::IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd);
        if fg_thread != 0 && fg_thread != my_thread {
            AttachThreadInput(my_thread, fg_thread, 0);
        }
        GetForegroundWindow() == hwnd
    }
}

fn find_window() -> HWND {
    use std::sync::atomic::{AtomicIsize, Ordering};
    static FOUND: AtomicIsize = AtomicIsize::new(0);
    FOUND.store(0, Ordering::Relaxed);
    unsafe extern "system" fn cb(h: HWND, _l: isize) -> i32 {
        if IsWindowVisible(h) == 0 {
            return 1;
        }
        let mut buf = [0u16; 256];
        let len = GetWindowTextW(h, buf.as_mut_ptr(), 256);
        let title = String::from_utf16_lossy(&buf[..len.max(0) as usize]);
        if title.contains("RustTerm") {
            FOUND.store(h as isize, Ordering::Relaxed);
            return 0;
        }
        1
    }
    unsafe {
        EnumWindows(Some(cb), 0);
    }
    FOUND.load(Ordering::Relaxed) as HWND
}
