//! 内置 X11 服务后端：进程内 X server（`xsrv`）+ 统一的 RdCommand/RdEvent 契约。
//!
//! 与 VNC / RDP / SPICE 不同，这里没有"远端主机"——X server 就跑在本进程里，
//! SSH 会话启用 X11 转发后，远端 X 程序的连接从 `x11` 通道直接灌进来
//! （见 `backends/ssh.rs`）。本模块负责三件事：
//!
//! 1. **宿主**：按需启动/持有全局唯一的 `xsrv::HostServer`（多个 SSH 会话、
//!    多个 X 客户端共享同一个屏幕）；
//! 2. **帧**：`FramePump` 的 RGBA 脏矩形 → `RdEvent::Frame`；
//! 3. **输入**：`RdCommand`（keysym / 指针 / 滚轮）→ `xsrv::XInputEvent`。
//!
//! 远端要显示程序，还需要 SSH 会话做两件事（`backends/ssh.rs`）：
//! 会话通道发 `x11-req`（sshd 才会把 `DISPLAY` 写进远端 shell），
//! 并把服务器反向打开的 `x11` 通道接进 [`ensure_xserver`] 返回的宿主。

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};

use super::{RdCommand, RdEvent, RdState, KeyInput};
use crate::xsrv::host::{HostServer, XInputEvent};

/// 输入轮询间隔：与 RDP / VNC 后端一致的节奏。
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(8);

/// 按 **screen id** 注册的 X server 表。
///
/// task-22 多显示：每个 SSH 会话一个独立屏幕（像 MobaXterm 的多 display），
/// 互不重叠——窗口树、display 编号、帧泵都各自独立。id 由 `backends/ssh.rs`
/// 在启用 X11 转发时生成，经 `TermIo.ssh`（`SshHandle.x11_screen_id`）带回 UI；
/// X 屏幕标签（`RdParams.x11_screen_id`）按同一个 id 挂到对应 server 上。
/// （Vec 而不是 HashMap：`HashMap::new` 不是 const，无法在 static 里初始化。）
static SCREENS: Mutex<Vec<(String, Arc<HostServer>)>> = Mutex::new(Vec::new());

/// 屏幕编号源（display 标签命名用）。
static NEXT_SCREEN: AtomicUsize = AtomicUsize::new(1);

/// 待提示的屏幕：有远端程序映射了窗口、但屏幕标签还没打开（task-22「按需
/// 显示」）。UI 的周期 tick 消费它——`x11_auto_popup` 开（默认）自动弹出对应
/// 屏幕，关则在状态栏提示。
pub static POPUP_REQUESTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 取走一条待提示的屏幕 id（去重，由调用方逐条消费）。
pub fn take_popup_request() -> Option<String> {
    let mut guard = POPUP_REQUESTS.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_empty() {
        None
    } else {
        Some(guard.remove(0))
    }
}

/// 把屏幕 id 放回待提示队列（自动弹出失败 / 关闭自动弹出的提示模式用）。
pub fn queue_popup_request(id: String) {
    let mut guard = POPUP_REQUESTS.lock().unwrap_or_else(|e| e.into_inner());
    if !guard.iter().any(|req| req == &id) {
        guard.push(id);
    }
}

/// 清除某个屏幕的待提示项（用户打开了对应屏幕时调用）。
pub fn clear_popup_request(id: &str) {
    POPUP_REQUESTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|req| req != id);
}

/// 获取（或创建）某个屏幕的 X server。
pub async fn ensure_screen(id: &str, width: u16, height: u16) -> Result<Arc<HostServer>, String> {
    // 快路径：已在运行（锁不能跨 await，先查后放）
    {
        let guard = SCREENS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, server)) = guard.iter().find(|(sid, _)| sid == id) {
            return Ok(Arc::clone(server));
        }
    }
    let server = Arc::new(HostServer::start_screened(id, width.max(320), height.max(240)).await?);
    let mut guard = SCREENS.lock().unwrap_or_else(|e| e.into_inner());
    // 双检：并发调用者可能已建好
    if let Some((_, existing)) = guard.iter().find(|(sid, _)| sid == id) {
        return Ok(Arc::clone(existing));
    }
    guard.push((id.to_string(), Arc::clone(&server)));
    Ok(server)
}

/// 对某个屏幕做一次操作。没在运行返回 `None`。
pub fn with_screen<T>(id: &str, f: impl FnOnce(&Arc<HostServer>) -> T) -> Option<T> {
    let guard = SCREENS.lock().unwrap_or_else(|e| e.into_inner());
    guard.iter().find(|(sid, _)| sid == id).map(|(_, server)| f(server))
}

/// 列出当前在运行的屏幕 id（UI「X 服务」菜单用）。
pub fn running_screens() -> Vec<String> {
    SCREENS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(sid, _)| sid.clone())
        .collect()
}

/// 关闭并移除一个屏幕（SSH 会话结束 / X11 转发关闭时调用）。
pub fn close_screen(id: &str) {
    let server = {
        let mut guard = SCREENS.lock().unwrap_or_else(|e| e.into_inner());
        match guard.iter().position(|(sid, _)| sid == id) { Some(pos) => Some(guard.remove(pos).1), None => None }
    };
    if let Some(server) = server {
        server.shutdown();
        POPUP_REQUESTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|req| req != id);
        eprintln!("[xsrv] 屏幕 {id} 已关闭（display :{}）", server.display);
    }
}

/// 兼容入口：全局唯一场景（如独立 X 屏幕标签没有绑定会话时）使用固定 id。
pub async fn ensure_xserver(width: u16, height: u16) -> Result<Arc<HostServer>, String> {
    ensure_screen("default", width, height).await
}

/// X server 是否在运行（供 UI 决定「启用 X11 转发」的默认值）。
/// 是否有屏幕在运行（供 UI 决定「启用 X11 转发」的默认值）。
pub fn xserver_running() -> bool {
    !running_screens().is_empty()
}

/// 对运行中的屏幕做一次操作（`id` 不存在返回 `None`）。
pub fn with_screen_by<T>(id: &str, f: impl FnOnce(&Arc<HostServer>) -> T) -> Option<T> {
    with_screen(id, f)
}

/// 停掉全部屏幕（进程退出前调用；一般不需要）。
pub fn shutdown_xserver() {
    for id in running_screens() {
        close_screen(&id);
    }
}

/// X11 后端主循环：宿主 X server，把帧送回 UI、把输入注入 X server。
///
/// `params.width / height` 是 X 屏幕的初始尺寸。
pub async fn run(
    params: &super::RdParams,
    ev: &std::sync::mpsc::Sender<super::RdEvent>,
    cmds: &std::sync::mpsc::Receiver<super::RdCommand>,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    use std::sync::atomic::Ordering;

    let _ = ev.send(RdEvent::State(RdState::Connecting));

    // task-22 多显示：X 屏幕标签按 profile 带来的 screen id 绑定到对应会话的
    // server；未指定（旧配置/独立屏幕）用 "default"。
    let screen_id = params
        .x11_screen_id
        .clone()
        .unwrap_or_else(|| "default".to_string());
    let server = ensure_screen(&screen_id, params.width, params.height)
        .await
        .map_err(|e| anyhow!("启动内置 X server 失败：{e}"))?;

    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::unbounded_channel();
    server.attach_viewer(frame_tx);

    // 首帧前先报尺寸；分辨率变化（极少）也要报
    let (mut last_w, mut last_h) = server.screen_size();
    let _ = ev.send(RdEvent::Resized {
        width: last_w as u32,
        height: last_h as u32,
    });
    let _ = ev.send(RdEvent::State(RdState::Active));

    // 帧转发任务：XFrame → RdEvent::Frame
    //
    // task-24 背压：通道无界，而 UI 消费不动时会无限堆积（3.7MB/帧 × 几千帧
    // = 数 GB 内存 + 之后逐条处理的 CPU 死亡螺旋）。积压超过 32 条就**丢弃
    // 积压、只留最新一条**，并让服务端作废已发基准帧——下一拍自动重发**全屏**，
    // UI 侧短暂残影后自愈。丢带只造成瞬时花屏，不会错误渲染。
    let server_for_invalidate = Arc::clone(&server);
    let ev_frames = ev.clone();
    let frame_task = tokio::spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
            // 排空积压（不等待）：统计丢弃数，保留最后一条
            let mut backlog = 0usize;
            let mut last = Some(frame);
            while backlog < 4096 {
                match frame_rx.try_recv() {
                    Ok(f) => {
                        backlog += 1;
                        last = Some(f);
                    }
                    Err(_) => break,
                }
            }
            if backlog > 32 {
                // 消费端明显跟不上：丢掉这批带，请求全屏重发自愈
                server_for_invalidate.invalidate_sent_frame();
                crate::xsrv::log_line(format_args!(
                    "[x11-fwd] 帧积压 {} 条已丢弃，已请求全屏重发",
                    backlog
                ));
                continue;
            }
            let frame = last.unwrap();
            let _ = ev_frames.send(RdEvent::Frame {
                x: frame.rect.x as u32,
                y: frame.rect.y as u32,
                w: frame.rect.width as u32,
                h: frame.rect.height as u32,
                rgba: frame.rgba,
            });
        }
    });

    // 指针按键的边沿检测：gpui 给的是位掩码（1=左 2=中 4=右），X 用键号 1/2/3
    let mut last_buttons: u8 = 0;
    let mut size_check = tokio::time::interval(std::time::Duration::from_secs(1));
    size_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let result = loop {
        if cancel.load(Ordering::Relaxed) {
            break Ok(());
        }

        // ---- 上行输入 ----
        let mut close_requested = false;
        while let Ok(cmd) = cmds.try_recv() {
            match cmd {
                RdCommand::Key(key) => inject_key(&server, &key),
                RdCommand::Pointer { x, y, buttons } => {
                    let x = x.min(i16::MAX as u16) as i16;
                    let y = y.min(i16::MAX as u16) as i16;
                    // 带上 UI 的权威按键掩码：服务端据此对账，UI 少报的抬起会
                    // 被补上，隐式抓取不会卡死（见 pointer_move_with_buttons）。
                    last_buttons = buttons;
                    let _ = server.send_input(&[XInputEvent::PointerWithButtons { x, y, buttons }]);
                }
                RdCommand::Wheel { dy } => {
                    // dy > 0 = 向上滚（X 4 号键）；dy < 0 = 向下滚（5 号键）
                    let _ = server.send_input(&[XInputEvent::Scroll { up: dy > 0 }]);
                }
                // X selections ↔ 本机剪贴板的桥接暂未实现；收到文本先忽略
                RdCommand::Clipboard(_) => {}
                RdCommand::SyncLockKeys { .. } => {}
                RdCommand::Close => {
                    close_requested = true;
                    break;
                }
            }
        }
        if close_requested || cancel.load(Ordering::Relaxed) {
            break Ok(());
        }

        // ---- 分辨率变化（顺带，1s 一次） ----
        tokio::select! {
            _ = size_check.tick() => {
                let (w, h) = server.screen_size();
                if (w, h) != (last_w, last_h) {
                    (last_w, last_h) = (w, h);
                    let _ = ev.send(RdEvent::Resized { width: w as u32, height: h as u32 });
                }
            }
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    };

    // 标签关闭只解除帧订阅；X server 本身继续服务其它 SSH 会话的 X 客户端
    server.detach_viewer();
    frame_task.abort();
    result
}

fn inject_key(server: &Arc<HostServer>, key: &KeyInput) {
    // NyaTerm 探针：键盘注入归因（keysym 是否到达 X 侧）
    crate::xsrv::log_line(format_args!(
        "[key] keysym={:#x} down={}",
        key.keysym, key.down
    ));
    let _ = server.send_input(&[XInputEvent::Key {
        keysym: key.keysym,
        pressed: key.down,
    }]);
}
