//! X11 转发端到端探针：连真实 SSH 主机，验证「x11-req → x11 通道 → 内置 X server → 帧」。
//!
//! 用法：
//! `cargo run --features devtools --bin x11test -- <host> <port> <user> <password> [--run "xeyes"]`
//!
//! 流程：
//! 1. 启动进程内 X server（与主程序同一套 `xsrv` 语义）；
//! 2. SSH 连接（accept-all 主机密钥），pty → x11-req（随机 cookie）→ shell；
//! 3. 服务端反向打开的 `x11` / `forwarded-tcpip` 通道全部路由进 `serve_stream`；
//! 4. 远端执行 `echo DISPLAY=$DISPLAY` 与 `which …`，输出原样打印；
//! 5. `--run "xeyes"` 可选：远端启动 X 程序，统计内置 server 收到的帧数。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use russh::client::{self, Handle, Session};
use russh::{Channel, ChannelMsg, ChannelOpenFailure};
use tokio::sync::mpsc;

#[path = "../src/xsrv/mod.rs"]
mod xsrv;

/// accept-all 主机密钥 + X11 通道路由的测试 Handler。
struct TestHandler {
    x11_sink: mpsc::UnboundedSender<Channel<client::Msg>>,
}

impl client::Handler for TestHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true) // 探针：不做 TOFU 校验
    }

    async fn server_channel_open_x11(
        &mut self,
        channel: Channel<client::Msg>,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        println!("[x11test] sshd 反向打开了 x11 通道 ✓");
        reply.accept().await;
        let _ = self.x11_sink.send(channel);
        Ok(())
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<client::Msg>,
        _connected_address: &str,
        _connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        println!("[x11test] sshd 反向打开了 forwarded-tcpip 通道（兼容模式）✓");
        reply.accept().await;
        let _ = self.x11_sink.send(channel);
        Ok(())
    }
}

fn x11_cookie() -> String {
    let uuid = uuid::Uuid::new_v4();
    let mut out = String::with_capacity(32);
    for byte in uuid.as_bytes() {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let run_app = args
        .iter()
        .position(|a| a == "--run")
        .and_then(|i| args.get(i + 1).cloned());
    if let Some(i) = args.iter().position(|a| a == "--run") {
        args.drain(i..=i + 1.min(args.len() - 1 - i));
    }

    let host = args.get(0).cloned().ok_or_else(|| anyhow!("用法: x11test <host> <port> <user> <password> [--run \"xeyes\"]"))?;
    let port: u16 = args.get(1).and_then(|p| p.parse().ok()).unwrap_or(22);
    let user = args.get(2).cloned().unwrap_or_else(|| "root".into());
    let password = args.get(3).cloned().unwrap_or_default();

    println!("== X11 转发端到端探针 == {host}:{port} user={user}");

    // 1. 进程内 X server（与主程序同一套 xsrv 语义）
    let server = xsrv::host::HostServer::start(1024, 768)
        .await
        .map_err(|e| anyhow!("启动 X server 失败: {e}"))?;
    println!("[x11test] X server 就绪：{}", server.display_name());

    // 2. 帧收集
    let (frame_tx, mut frame_rx) = mpsc::unbounded_channel::<xsrv::viewer::XFrame>();
    server.attach_viewer(frame_tx);
    let frames = Arc::new(AtomicU64::new(0));
    let last_frame = Arc::new(std::sync::Mutex::new(String::new()));
    {
        let frames = Arc::clone(&frames);
        let last_frame = Arc::clone(&last_frame);
        tokio::spawn(async move {
            while let Ok(frame) = frame_rx.recv().await {
                let n = frames.fetch_add(1, Ordering::Relaxed) + 1;
                *last_frame.lock().unwrap() = format!(
                    "{}x{} @ ({},{})",
                    frame.rect.width, frame.rect.height, frame.rect.x, frame.rect.y
                );
                if n % 10 == 1 {
                    println!("[x11test] 已收到 {n} 帧（最新：{}）", *last_frame.lock().unwrap());
                }
            }
        });
    }

    // 3. SSH 连接（x11 通道路由进 serve_stream）
    let (x11_tx, mut x11_rx) = mpsc::unbounded_channel::<Channel<client::Msg>>();
    let config = Arc::new(client::Config::default());
    let mut handle: Handle<TestHandler> =
        client::connect(config, (host.as_str(), port), TestHandler { x11_sink: x11_tx })
            .await
            .map_err(|e| anyhow!("SSH 连接失败: {e}"))?;
    println!("[x11test] SSH 已连接 ✓");

    let mut channel = handle.channel_open_session().await?;
    channel
        .request_pty(false, "xterm-256color", 120, 32, 0, 0, &[])
        .await?;
    channel
        .request_x11(true, false, "MIT-MAGIC-COOKIE-1", x11_cookie(), 0)
        .await?;

    // 等 x11-req 的回复（Success/Failure）
    let mut accepted = false;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(deadline.saturating_duration_since(tokio::time::Instant::now()), channel.wait()).await {
            Ok(Some(ChannelMsg::Success)) => {
                accepted = true;
                break;
            }
            Ok(Some(ChannelMsg::Failure)) => break,
            Ok(Some(_)) => continue,
            _ => break,
        }
    }
    println!(
        "[x11test] x11-req 回复：{}",
        if accepted { "接受 ✓" } else { "拒绝/未回复 ✗（sshd 需开启 X11Forwarding）" }
    );

    channel.request_shell(false).await?;

    // 4. 路由：x11 / forwarded-tcpip 通道 → serve_stream
    let server_route = std::sync::Arc::clone(&server);
    tokio::spawn(async move {
        while let Some(ch) = x11_rx.recv().await {
            let index = server_route.next_client_index();
            server_route.serve_channel(ch.into_stream(), index);
        }
    });

    // 5. 输出收集 + 命令下发
    let (mut read_half, write_half) = channel.split();
    let output = Arc::new(std::sync::Mutex::new(String::new()));
    {
        let output = Arc::clone(&output);
        tokio::spawn(async move {
            while let Some(msg) = read_half.wait().await {
                if let ChannelMsg::Data { ref data } = msg {
                    output.lock().unwrap().push_str(&String::from_utf8_lossy(data));
                }
            }
        });
    }

    let send = |write: &russh::channels::ChannelWriteHalf<client::Msg>, cmd: String| {
        let write = write.clone();
        tokio::spawn(async move {
            let _ = write.data(cmd.as_bytes()).await;
        });
    };

    send(&write_half, "echo \"DISPLAY=$DISPLAY\"\n".into());
    tokio::time::sleep(Duration::from_secs(2)).await;
    {
        let text = output.lock().unwrap().clone();
        let display_line = text
            .lines()
            .find(|l| l.starts_with("DISPLAY="))
            .unwrap_or("(未见 DISPLAY —— x11-req 未生效)");
        println!("[x11test] {display_line}");
    }

    send(
        &write_half,
        "which xeyes xclock xterm xdpyinfo xset 2>/dev/null; echo PROBE-DONE\n".into(),
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    {
        let text = output.lock().unwrap().clone();
        let tail = text
            .lines()
            .filter(|l| !l.starts_with("DISPLAY="))
            .rev()
            .take(8)
            .collect::<Vec<_>>();
        println!("[x11test] 远端可用的 X 客户端：{:?}", tail);
    }

    if let Some(app) = run_app {
        println!("[x11test] 远端启动 X 程序：{app}");
        send(&write_half, format!("{app}\n"));
    }

    // 6. 帧统计窗口
    let wait = if run_app.is_some() { 20u64 } else { 8 };
    println!("[x11test] 观察 {wait} 秒内的帧产出…");
    tokio::time::sleep(Duration::from_secs(wait)).await;
    let n = frames.load(Ordering::Relaxed);
    let size = last_frame.lock().unwrap().clone();
    if n > 0 {
        println!("[x11test] ✓ 收到 {n} 帧（最后：{size}）—— X 协议与渲染链路工作正常");
    } else {
        println!("[x11test] ✗ 没有收到帧——远端 X 程序未连接或未绘制");
    }

    let _ = handle.disconnect().await;
    Ok(())
}
