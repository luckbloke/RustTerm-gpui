//! RustTerm GPUI - 全功能终端客户端（GPUI Kit 版）
//!
//! P1: 产品骨架（统一标签页/SFTP双栏/状态栏/配置面板/i18n）
//! P2: 远程桌面（VNC/RDP/SPICE 帧渲染，后端零改动）
//! P3: 兼容层（wry浏览器标签 + AI 助手多提供方 + AI 截屏分析面板）
//! P4: 打磨（键宏、keep-alive、装配打包、性能与内存调优）
//!
//! UI 框架：GPUI Kit（gpui-kit 0.8，longbridge）替代原 eframe/egui。

// Windows 下不挂载控制台：纯 GUI 程序
#![cfg_attr(windows, windows_subsystem = "windows")]
// app.rs 体量很大，测试宏展开时容易撞到默认递归深度


mod ai;
mod ai_assistant;
mod ai_config_panel;
mod app;
mod backends;
mod browser;
mod config;
mod config_panel;
mod filebrowser;
mod framebuf;
mod fonts;
mod i18n;
mod macros;
mod portscan;
mod rdp;
mod remote;
mod screenshot;
mod sessions;
mod sftp;
mod sftp_client;
mod tabs;
mod term;
mod terminal_view;
mod tunnel;
mod ui_icons;
mod widgets;
mod win32;

use gpui_kit::{AppContext as _, Bounds, WindowBounds, WindowOptions, px, point, size};

/// 计算初始窗口尺寸与位置（屏幕物理像素 → 逻辑点），保证窗口整体落在工作区内。
#[cfg(windows)]
fn initial_geometry() -> (f32, f32, Option<(f32, f32)>) {
    use windows_sys::Win32::UI::WindowsAndMessaging::SystemParametersInfoW;

    #[repr(C)]
    struct WRect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    let mut r = WRect { left: 0, top: 0, right: 0, bottom: 0 };
    let ok = unsafe {
        SystemParametersInfoW(0x0030, 0, &mut r as *mut _ as *mut core::ffi::c_void, 0)
    };
    if ok == 0 {
        return (1400.0, 860.0, None);
    }

    let scale = unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForSystem() } as f32 / 96.0;
    let scale = if scale > 0.0 { scale } else { 1.0 };

    let wl = r.left as f32 / scale;
    let wt = r.top as f32 / scale;
    let ww = (r.right - r.left) as f32 / scale;
    let wh = (r.bottom - r.top) as f32 / scale;

    let iw = (ww - 24.0).min(1500.0).max(900.0);
    let ih = (wh - 70.0).min(940.0).max(560.0);
    let x = wl + (ww - iw) * 0.5;
    let y = wt + (wh - ih) * 0.5;
    (iw, ih, Some((x, y)))
}

#[cfg(not(windows))]
fn initial_geometry() -> (f32, f32, Option<(f32, f32)>) {
    (1400.0, 880.0, None)
}

/// 关闭 gpui 的 DirectComposition 合成，让 HWND 子窗口能显示在界面之上。
///
/// gpui 默认用 `CreateSwapChainForComposition`（DirectComposition）渲染窗口内容。
/// DComp 的视觉树里**不包含普通 HWND 子窗口**，而内置浏览器正是把
/// WebView2 挂成主窗口的子窗口（wry 的 `build_as_child`）——结果就是：
/// WebView 确实创建成功、页面也确实加载了，但被 DComp 内容整块盖住，
/// 用户看到的是一片空白。
///
/// 关闭后 gpui 改用 `CreateSwapChainForHwnd`，走 GDI 互操作路径，
/// 子窗口可以正常叠加显示。
///
/// 用户若显式设置了该环境变量则以用户为准。
fn enable_child_window_embedding() {
    if std::env::var_os("GPUI_DISABLE_DIRECT_COMPOSITION").is_none() {
        std::env::set_var("GPUI_DISABLE_DIRECT_COMPOSITION", "true");
    }
}

/// 安装 panic 钩子：把崩溃现场写到 `%APPDATA%/rustterm-gpui/crash.log`。
///
/// 窗口是 GUI 子系统程序，panic 只会弹一个「XX 已停止工作」的框，
/// 堆栈信息全丢。这里落一份文本日志，用户把文件发回来就能定位。
fn install_crash_log() {
    let Some(dir) = dirs::config_dir() else { return };
    let path = dir.join("rustterm-gpui").join("crash.log");
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        use std::io::Write;
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(
                f,
                "=== panic @ {} ===\n{}\n",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                info
            );
            let _ = f.flush();
        }
        prev(info);
    }));
}

/// RDPDR 诊断日志：GUI（windows 子系统）启动看不到 stderr，把 RDP 共享目录
/// 的每条失败 IRP 落到 `%APPDATA%/rustterm-gpui/rdpdr.log`，复现一次就能定位。
///
/// vendored IronRDP 里通过 `NYATERM_RDPDR_LOG` 读取该路径；每次启动清空旧日志。
/// `NYATERM_RDPDR_TRACE=1` 追加每条 IRP 的全量跟踪（排查拷贝失败期间开启，
/// 问题修复后应移除）。
fn install_rdpdr_log() {
    let Some(dir) = dirs::config_dir() else { return };
    let path = dir.join("rustterm-gpui").join("rdpdr.log");
    let _ = std::fs::remove_file(&path);
    std::env::set_var("NYATERM_RDPDR_LOG", &path);
    std::env::set_var("NYATERM_RDPDR_TRACE", "1");
}

/// 主逻辑跑在**大栈线程**上。
///
/// GPUI 的元素树是递归布局/递归绘制的，本项目的元素树嵌套很深
/// （根容器 → 侧栏 → 会话行 → 按钮 → 组件内部结构 …），再加上每帧重建
/// 整棵树、Windows 默认主线程栈只有 1MB，实测会偶发
/// `thread 'main' has overflowed its stack`。
///
/// 栈溢出不是 panic，走不到 panic hook，日志里什么都留不下，只有一句
/// abort 信息。因此这里直接把整个应用挪到 32MB 栈的线程上跑
/// （Zed / gpui 应用的标准做法）。
fn main() {
    // 栈溢出检测本身也需要一点栈空间，先把 panic / 日志装好再进新线程。
    env_logger::init();
    install_crash_log();
    install_rdpdr_log();
    enable_child_window_embedding();

    let handle = std::thread::Builder::new()
        .name("rustterm-main".to_string())
        .stack_size(32 * 1024 * 1024)
        .spawn(run_app)
        .expect("无法创建主线程");

    match handle.join() {
        Ok(()) => {}
        Err(e) => {
            // 线程 panic：把 payload 写进崩溃日志再退出。
            if let Some(msg) = e.downcast_ref::<&str>() {
                eprintln!("RustTerm 崩溃: {msg}");
            } else if let Some(msg) = e.downcast_ref::<String>() {
                eprintln!("RustTerm 崩溃: {msg}");
            } else {
                eprintln!("RustTerm 崩溃");
            }
            std::process::exit(1);
        }
    }
}

/// 真正的应用入口（运行在 32MB 栈的线程上）。
fn run_app() {
    let (w, h, pos) = initial_geometry();

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            pos.map(|(x, y)| point(px(x), px(y))).unwrap_or(point(px(100.0), px(60.0))),
            size(px(w), px(h)),
        ))),
        titlebar: Some(gpui_kit::TitlebarOptions {
            title: Some("RustTerm GPUI".into()),
            ..Default::default()
        }),
        window_min_size: Some(size(px(900.0), px(560.0))),
        ..Default::default()
    };

    gpui_kit::application()
        // 注册内置 SVG 图标资源。这里用完整目录（AllAssets，1830 个 Lucide
        // 图标，约 2.8MB），工具栏 / 侧栏 / 空状态都要用到；只注册默认子集
        // 的话，gpui-component 里的图标元素会全部渲染为空白。
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            // 必须在使用任何 GPUI Component 功能之前调用
            gpui_kit::init(cx);

            let _ = gpui_kit::open_window(options, cx, |_, cx| {
                cx.new(|_| app::RustTermApp::new())
            })
            .expect("打开主窗口失败");
        });
}
