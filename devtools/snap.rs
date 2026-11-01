//! 开发用屏幕截图工具：把当前桌面（或 RustTerm 主窗口）存成 PNG。
//!
//! 用法：
//!   snap <输出路径>            # 全屏
//!   snap <输出路径> window     # 只抓 RustTerm 主窗口客户区
//!
//! 仅在开发期手动运行，不参与主程序逻辑。

use anyhow::{anyhow, Result};
use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClientRect, GetSystemMetrics, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, IsWindowVisible, SetForegroundWindow, ShowWindow, SW_RESTORE,
};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let out = args
        .get(1)
        .cloned()
        .ok_or_else(|| anyhow!("用法: snap <输出.png> [window|full|list]"))?;
    let mode = args.get(2).cloned().unwrap_or_else(|| "full".into());

    if mode == "list" {
        list_windows();
        return Ok(());
    }
    if mode == "children" {
        list_children();
        return Ok(());
    }

    let (x0, y0, w, h) = if mode == "window" || mode == "windowfg" {
        let hwnd = find_main_window();
        if hwnd.is_null() {
            return Err(anyhow!("找不到 RustTerm 主窗口"));
        }
        // 只有 windowfg 模式才抢前台：SetForegroundWindow 会让 gpui 里的
        // 弹出菜单收到 DismissEvent 而自动关闭，干扰菜单点击的自动化测试。
        // 纯 window 模式直接抓客户区 DC，窗口被遮挡时也能拿到内容。
        if mode == "windowfg" {
            unsafe {
                ShowWindow(hwnd, SW_RESTORE);
                SetForegroundWindow(hwnd);
            }
            std::thread::sleep(std::time::Duration::from_millis(600));
        }
        let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        unsafe { GetClientRect(hwnd, &mut rc) };
        let w = (rc.right - rc.left).max(1);
        let h = (rc.bottom - rc.top).max(1);
        let mut origin = POINT { x: 0, y: 0 };
        unsafe { windows_sys::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut origin) };
        (origin.x, origin.y, w, h)
    } else {
        let w = unsafe { GetSystemMetrics(0) };
        let h = unsafe { GetSystemMetrics(1) };
        (0, 0, w, h)
    };

    let rgba = grab(x0, y0, w, h)?;
    let img = image::RgbaImage::from_raw(w as u32, h as u32, rgba)
        .ok_or_else(|| anyhow!("像素缓冲尺寸不匹配"))?;
    img.save(&out)?;
    println!("saved {} ({}x{})", out, w, h);
    Ok(())
}

/// 列出所有可见顶层窗口（标题 + 位置）。
fn list_windows() {
    unsafe extern "system" fn cb(h: HWND, _l: isize) -> i32 {
        let mut buf = [0u16; 256];
        let len = GetWindowTextW(h, buf.as_mut_ptr(), 256);
        let _ = GetWindowTextLengthW(h);
        let title = String::from_utf16_lossy(&buf[..len.max(0) as usize]);
        if IsWindowVisible(h) != 0 {
            let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            GetWindowRect(h, &mut rc);
            println!(
                "hwnd={:p} vis={} rect=({},{},{}x{}) title={:?}",
                h,
                IsWindowVisible(h),
                rc.left,
                rc.top,
                rc.right - rc.left,
                rc.bottom - rc.top,
                title
            );
        }
        1
    }
    unsafe {
        EnumWindows(Some(cb), 0);
    }
}

/// 列出主窗口的所有子窗口（含 WebView 容器）。
fn list_children() {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumChildWindows, GetClassNameW, IsWindowVisible,
    };
    static FOUND: AtomicIsize = AtomicIsize::new(0);
    FOUND.store(0, Ordering::Relaxed);
    unsafe extern "system" fn find_cb(h: HWND, _l: isize) -> i32 {
        if IsWindowVisible(h) == 0 {
            return 1;
        }
        let mut buf = [0u16; 256];
        let len = GetWindowTextW(h, buf.as_mut_ptr(), 256);
        let title = String::from_utf16_lossy(&buf[..len.max(0) as usize]);
        if title.contains("RustTerm") || process_name(h).contains("rustterm") {
            FOUND.store(h as isize, Ordering::Relaxed);
            return 0;
        }
        1
    }
    unsafe {
        EnumWindows(Some(find_cb), 0);
    }
    let parent = FOUND.load(Ordering::Relaxed) as HWND;
    if parent.is_null() {
        println!("找不到主窗口");
        return;
    }
    unsafe extern "system" fn cb(h: HWND, _l: isize) -> i32 {
        let mut cls = [0u16; 256];
        let n = GetClassNameW(h, cls.as_mut_ptr(), 256);
        let class = String::from_utf16_lossy(&cls[..n.max(0) as usize]);
        let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetWindowRect(h, &mut rc);
        let mut crc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetClientRect(h, &mut crc);
        let mut title = [0u16; 256];
        let tl = GetWindowTextW(h, title.as_mut_ptr(), 256);
        println!(
            "child hwnd={:p} class={:?} vis={} win=({},{},{}x{}) client=({},{},{}x{}) title={:?}",
            h,
            class,
            IsWindowVisible(h),
            rc.left,
            rc.top,
            rc.right - rc.left,
            rc.bottom - rc.top,
            crc.left,
            crc.top,
            crc.right - crc.left,
            crc.bottom - crc.top,
            String::from_utf16_lossy(&title[..tl.max(0) as usize])
        );
        1
    }
    unsafe {
        EnumChildWindows(parent, Some(cb), 0);
    }
}

/// 强制把窗口切到前台。
///
/// 后台进程调用 SetForegroundWindow 会被 Windows 的「前台锁」拒绝，
/// 这里用两个经典手段叠加：ALT 键按下放开 + AttachThreadInput 临时
/// 挂到前台线程的消息队列上。
pub fn force_foreground(hwnd: HWND) -> bool {
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{keybd_event, KEYEVENTF_KEYUP};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };
    unsafe {
        // 技巧 1：ALT 键会临时放开前台锁
        keybd_event(0x12, 0, 0, 0);
        keybd_event(0x12, 0, KEYEVENTF_KEYUP, 0);

        // 技巧 2：挂到当前前台线程，再切
        let fg = GetForegroundWindow();
        let fg_thread = GetWindowThreadProcessId(fg, std::ptr::null_mut());
        let my_thread = GetCurrentThreadId();
        if fg_thread != 0 && fg_thread != my_thread {
            AttachThreadInput(my_thread, fg_thread, 1);
        }
        ShowWindow(hwnd, SW_RESTORE);
        SetForegroundWindow(hwnd);
        if fg_thread != 0 && fg_thread != my_thread {
            AttachThreadInput(my_thread, fg_thread, 0);
        }
        GetForegroundWindow() == hwnd
    }
}

/// 找标题含 "RustTerm" 的可见顶层窗口。
fn find_main_window() -> HWND {
    use std::sync::atomic::{AtomicIsize, Ordering};
    static FOUND: AtomicIsize = AtomicIsize::new(0);
    FOUND.store(0, Ordering::Relaxed);
    unsafe extern "system" fn cb(h: HWND, _l: isize) -> i32 {
        if IsWindowVisible(h) == 0 {
            return 1;
        }
        let mut buf = [0u16; 256];
        let len = GetWindowTextW(h, buf.as_mut_ptr(), 256);
        let _ = GetWindowTextLengthW(h);
        let title = String::from_utf16_lossy(&buf[..len.max(0) as usize]);
        if title.contains("RustTerm") || process_name(h).contains("rustterm") {
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

/// 取窗口所属进程的可执行文件名（小写）。
fn process_name(hwnd: HWND) -> String {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let mut pid = 0u32;
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(hwnd, &mut pid);
    }
    if pid == 0 {
        return String::new();
    }
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return String::new();
        }
        let mut buf = [0u16; 512];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size);
        CloseHandle(h);
        if ok == 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..size as usize]).to_lowercase()
    }
}

fn grab(sx: i32, sy: i32, w: i32, h: i32) -> Result<Vec<u8>> {
    unsafe {
        let screen_dc = GetDC(std::ptr::null_mut());
        if screen_dc.is_null() {
            return Err(anyhow!("获取屏幕 DC 失败"));
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        let bmp = CreateCompatibleBitmap(screen_dc, w, h);
        let old = SelectObject(mem_dc, bmp);

        let ok = BitBlt(mem_dc, 0, 0, w, h, screen_dc, sx, sy, SRCCOPY);

        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [windows_sys::Win32::Graphics::Gdi::RGBQUAD {
                rgbBlue: 0,
                rgbGreen: 0,
                rgbRed: 0,
                rgbReserved: 0,
            }],
        };
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(
            mem_dc,
            bmp,
            0,
            h as u32,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            &mut info,
            DIB_RGB_COLORS,
        );

        SelectObject(mem_dc, old);
        DeleteObject(bmp);
        DeleteDC(mem_dc);
        ReleaseDC(std::ptr::null_mut(), screen_dc);

        if ok == 0 || lines == 0 {
            return Err(anyhow!("位图拷贝失败"));
        }
        let mut rgba = vec![0u8; buf.len()];
        for (dst, src) in rgba.chunks_exact_mut(4).zip(buf.chunks_exact(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = 255;
        }
        Ok(rgba)
    }
}
