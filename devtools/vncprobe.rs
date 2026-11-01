//! VNC 在线探针：直连服务器打印每一个协议事件，定位解码失败的真实原因。
//!
//! 用法：`cargo run --features devtools --bin vncprobe -- <host> <port> [password]`
//!
//! 与 `src/remote/vnc.rs` 使用完全相同的连接参数（像素格式 / 编码 / 限额），
//! 区别只在于：把所有事件原样打印出来，并在错误时把**出错矩形**也打出来，
//! 这样才能判断「Invalid framebuffer or rectangle dimensions」到底是
//! 零尺寸光标伪编码、还是分辨率协商越界、还是别的什么。

use std::time::Duration;
use tokio::net::TcpStream;
use vnc::{
    PixelFormat, Rect, VncConnector, VncEncoding, VncError, VncEvent, VncLimits,
    VncSecurityPolicy, VncServerKey, X11Event,
};

const MAX_FRAMEBUFFER_WIDTH: u16 = 7680;
const MAX_FRAMEBUFFER_HEIGHT: u16 = 4320;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // vnc-rs 用 tracing（带 log 特性）打印内部细节，这里转成 log 输出。
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("trace")).init();

    let args: Vec<String> = std::env::args().collect();
    let host = args.get(1).cloned().unwrap_or_else(|| "192.168.1.16".into());
    let port: u16 = args
        .get(2)
        .and_then(|p| p.parse().ok())
        .unwrap_or(5900);
    let password = args.get(3).cloned().unwrap_or_default();

    println!("== 连接 {host}:{port} ==\n");

    let tcp = TcpStream::connect((host.as_str(), port)).await?;
    tcp.set_nodelay(true)?;

    let pwd = password.clone();
    let connector = VncConnector::new(tcp)
        .set_auth_method(async move { Ok(pwd) })
        // 与 src/remote/vnc.rs 一致：只要设置了认证回调就认为「有凭据」，
        // 空口令也要走 VncAuth（服务器若配的是空密码就能过）。
        .set_credentials_available(true)
        .set_username(String::new())
        .set_security_policy(VncSecurityPolicy::Auto)
        .set_server_key_verifier(|key: VncServerKey| async move {
            println!("[probe] 服务器密钥 {} bit，已接受", key.bits());
            Ok(())
        })
        .set_pixel_format(PixelFormat::bgra())
        .add_encoding(VncEncoding::Zrle)
        .add_encoding(VncEncoding::Tight)
        .add_encoding(VncEncoding::CopyRect)
        .add_encoding(VncEncoding::Raw)
        .add_encoding(VncEncoding::DesktopSizePseudo)
        .add_encoding(VncEncoding::CursorPseudo)
        .allow_shared(true)
        .set_limits(VncLimits {
            max_framebuffer_width: MAX_FRAMEBUFFER_WIDTH,
            max_framebuffer_height: MAX_FRAMEBUFFER_HEIGHT,
            max_framebuffer_pixels: usize::from(MAX_FRAMEBUFFER_WIDTH)
                * usize::from(MAX_FRAMEBUFFER_HEIGHT),
            max_rectangles_per_update: 1024,
            max_encoded_payload_bytes: 64 * 1024 * 1024,
            max_decoded_payload_bytes: usize::from(MAX_FRAMEBUFFER_WIDTH)
                * usize::from(MAX_FRAMEBUFFER_HEIGHT)
                * 4,
            channel_capacity: 32,
            ..VncLimits::default()
        })
        .build()?;

    let state = match tokio::time::timeout(Duration::from_secs(20), connector.try_start()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            println!("[probe] 握手失败: {e:?}");
            return Ok(());
        }
        Err(_) => {
            println!("[probe] 握手超时");
            return Ok(());
        }
    };
    let mut client = state.finish()?;
    println!("[probe] 握手成功\n");

    let mut frames = 0u32;
    let mut rects_seen = 0u32;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut last_refresh = tokio::time::Instant::now();
    // 与 src/remote/vnc.rs 一致：首帧必须是非增量（全量）请求，
    // 否则部分服务器不回像素帧。
    let mut need_full = true;
    let mut last_full = tokio::time::Instant::now() - Duration::from_secs(2);

    while tokio::time::Instant::now() < deadline {
        // poll_event 是 try_recv 语义：空的时候立刻返回 Ok(None)，
        // 所以刷新请求必须挂在 Ok(None) 分支上（只看超时分支会永远不发）。
        match tokio::time::timeout(Duration::from_millis(50), client.poll_event()).await {
            Ok(Ok(Some(event))) => match event {
                VncEvent::SetResolution(screen) => {
                    println!("[probe] 分辨率 {}x{}", screen.width, screen.height);
                }
                VncEvent::SetPixelFormat(pf) => {
                    println!("[probe] 像素格式 {pf:?}");
                }
                VncEvent::RawImage(rect, data) => {
                    frames += 1;
                    rects_seen += 1;
                    if frames <= 10 || frames % 50 == 0 {
                        println!(
                            "[probe] RawImage #{} rect=({},{} {}x{}) bytes={}",
                            frames,
                            rect.x,
                            rect.y,
                            rect.width,
                            rect.height,
                            data.len()
                        );
                    }
                }
                VncEvent::Copy(dst, src) => {
                    println!("[probe] Copy dst={dst:?} src={src:?}");
                }
                VncEvent::JpegImage(rect, data) => {
                    println!("[probe] JpegImage rect={rect:?} bytes={}", data.len());
                }
                VncEvent::SetCursor(rect, data) => {
                    println!(
                        "[probe] SetCursor rect=({},{} {}x{}) bytes={}",
                        rect.x,
                        rect.y,
                        rect.width,
                        rect.height,
                        data.len()
                    );
                }
                VncEvent::Bell => println!("[probe] Bell"),
                VncEvent::Text(t) => println!("[probe] Text {t:?}"),
                VncEvent::Error(msg) => {
                    println!("\n[probe] *** 服务器错误事件: {msg} ***");
                    println!("[probe] 到此为止共收到 {frames} 帧");
                    break;
                }
                other => println!("[probe] 其他事件 {other:?}"),
            },
            Ok(Ok(None)) => {
                // 通道空 —— 按应用节奏补发刷新请求
                if last_refresh.elapsed() >= Duration::from_millis(16) {
                    last_refresh = tokio::time::Instant::now();
                    let full = need_full || last_full.elapsed() >= Duration::from_secs(2);
                    if full {
                        need_full = false;
                        last_full = tokio::time::Instant::now();
                        println!("[probe] → FullRefresh");
                        let _ = client.input(X11Event::FullRefresh).await;
                    } else {
                        let _ = client.input(X11Event::Refresh).await;
                    }
                }
            }
            Ok(Err(e)) => {
                println!("\n[probe] *** poll 错误: {e:?} ***");
                match e {
                    VncError::InvalidDimensions => {
                        println!("[probe] → 尺寸非法：零宽/零高，或 x+width 溢出 u16");
                    }
                    other => println!("[probe] → {other:?}"),
                }
                break;
            }
            Err(_) => {
                // 轮询超时：按应用策略发刷新请求
                if last_refresh.elapsed() >= Duration::from_millis(16) {
                    last_refresh = tokio::time::Instant::now();
                    let full = need_full || last_full.elapsed() >= Duration::from_secs(2);
                    if full {
                        need_full = false;
                        last_full = tokio::time::Instant::now();
                        println!("[probe] → FullRefresh");
                        let _ = client.input(X11Event::FullRefresh).await;
                    } else {
                        let _ = client.input(X11Event::Refresh).await;
                    }
                }
            }
        }
    }

    println!("\n[probe] 统计: {frames} 帧 / {rects_seen} 个矩形");
    let _ = client.close().await;
    Ok(())
}

/// 让 Rect 可读（探针打印用）。
trait RectDebug {
    fn dbg(&self) -> String;
}
impl RectDebug for Rect {
    fn dbg(&self) -> String {
        format!("({},{} {}x{})", self.x, self.y, self.width, self.height)
    }
}
