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

use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Result};

use super::{RdCommand, RdEvent, RdState, KeyInput};
use crate::xsrv::host::{HostServer, XInputEvent};

/// 输入轮询间隔：与 RDP / VNC 后端一致的节奏。
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(8);

/// 全局唯一的进程内 X server。多个 SSH 会话与多个 X 客户端共享同一个屏幕。
static XSERVER: Mutex<Option<Arc<HostServer>>> = Mutex::new(None);

/// 按需启动（或返回已在运行的）进程内 X server。
pub async fn ensure_xserver(width: u16, height: u16) -> Result<Arc<HostServer>, String> {
    // 快路径：已在运行（锁不能跨 await，先查后放）
    {
        let guard = XSERVER.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(server) = guard.as_ref() {
            return Ok(Arc::clone(server));
        }
    }

    let server = Arc::new(HostServer::start(width.max(320), height.max(240)).await?);

    // 双检：可能已有并发调用者先启动好了
    let mut guard = XSERVER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = guard.as_ref() {
        return Ok(Arc::clone(existing));
    }
    *guard = Some(Arc::clone(&server));
    Ok(server)
}

/// X server 是否在运行（供 UI 决定「启用 X11 转发」的默认值）。
pub fn xserver_running() -> bool {
    XSERVER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

/// 对运行中的 X server 做一次操作。没在运行返回 `None`。
pub fn with_xserver<T>(f: impl FnOnce(&Arc<HostServer>) -> T) -> Option<T> {
    let guard = XSERVER.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(f)
}

/// 停掉进程内 X server（一般不需要：它随进程退出）。
pub fn shutdown_xserver() {
    let guard = XSERVER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(server) = guard.as_ref() {
        server.shutdown();
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

    let server = ensure_xserver(params.width, params.height)
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
    let ev_frames = ev.clone();
    let frame_task = tokio::spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
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
