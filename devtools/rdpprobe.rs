//! RDP 在线探针：直连服务器打印每一个输出事件，定位「连不上 / 连上没有画面」的真实原因。
//!
//! 用法：
//! `cargo run --features devtools --bin rdpprobe -- <host> <port> [user] [password] [domain]`
//!
//! 可选参数：
//! - `--tls-only`：跳过 NLA/CredSSP，只用 TLS（Windows Server 2008 兼容路径）
//! - `--share <目录>`：启用 RDPDR 驱动器重定向，验证共享目录是否被服务器接受
//!
//! 与 `src/remote/rdp.rs` 使用同一套 ConfigBuilder（CredSSP + TLS + 接受任意证书），
//! 区别只在于：把所有 RdpOutputEvent 原样打印出来，失败时把完整错误打出来。

use std::sync::Arc;
use std::time::Duration;

use ironrdp::client::config::{
    ConfigBuilder, Destination, DirectTransport, ServerCertificateVerifier,
    ServerCertificateVerifyFuture, TransportKind,
};
use ironrdp::client::rdp::{RdpClient, RdpOutputEvent};
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;

#[derive(Debug)]
struct TrustAnyCert;

impl ServerCertificateVerifier for TrustAnyCert {
    fn verify_server_certificate<'a>(
        &'a self,
        _host: &'a str,
        _port: u16,
        _der: Vec<u8>,
    ) -> ServerCertificateVerifyFuture<'a> {
        Box::pin(async move { Ok(()) })
    }
}

fn client_build() -> u32 {
    env!("CARGO_PKG_VERSION")
        .split('.')
        .take(3)
        .fold(0_u32, |acc, part| {
            acc.saturating_mul(100)
                .saturating_add(part.parse::<u32>().unwrap_or(0))
        })
}

fn client_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "RustTerm".to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().collect();
    // 先摘掉开关，剩下的仍是位置参数，保持旧用法可用
    let tls_only = args.iter().any(|a| a == "--tls-only");
    args.retain(|a| a != "--tls-only");
    let share = args
        .iter()
        .position(|a| a == "--share")
        .and_then(|i| {
            if i + 1 < args.len() {
                let path = args[i + 1].clone();
                args.drain(i..=i + 1);
                Some(path)
            } else {
                None
            }
        });
    let host = args.get(1).cloned().unwrap_or_else(|| "192.168.1.16".into());
    let port: u16 = args
        .get(2)
        .and_then(|p| p.parse().ok())
        .unwrap_or(3389);
    let username = args.get(3).cloned().unwrap_or_default();
    let password = args.get(4).cloned().unwrap_or_default();
    let domain = args.get(5).cloned().unwrap_or_default();

    println!("== RDP 连接 {host}:{port} ==");
    println!(
        "   用户名={username:?} 域={domain:?} 密码长度={} 安全层={}",
        password.len(),
        if tls_only { "TLS-only" } else { "NLA" }
    );
    if let Some(dir) = &share {
        println!("   共享目录={dir:?}（RDPDR）");
    }

    let connector_host = host.clone();
    let connector_port = port;

    let config = ConfigBuilder::new()
        .with_destination(Destination::from_parts(host.clone(), port))
        .with_transport(TransportKind::Direct)
        .with_username(username.clone())
        .with_domain(domain.clone())
        .with_password(password.clone())
        .with_desktop_width(1024)
        .with_desktop_height(768)
        .with_desktop_scale_factor(100)
        .with_color_depth(32)
        .with_credssp(!tls_only)
        .with_tls(true)
        .with_autologon(tls_only)
        .with_platform(MajorPlatformType::WINDOWS)
        .with_client_build(client_build())
        .with_client_dir("C:\\Windows\\System32\\mstscax.dll")
        .with_client_name(client_name())
        // RDPDR 通道由配置里的开关决定：只给工厂但不开通道，盘符不会广播。
        .with_rdpdr(share.is_some())
        .with_server_certificate_verifier(Arc::new(TrustAnyCert))
        .with_direct_transport_connector(Arc::new(move |_destination| {
            let host = connector_host.clone();
            let port = connector_port;
            Box::pin(async move {
                let stream = tokio::net::TcpStream::connect((host.as_str(), port))
                    .await
                    .map_err(|e| {
                        std::io::Error::new(
                            e.kind(),
                            format!("无法连接 RDP 服务器 {host}:{port}: {e}"),
                        )
                    })?;
                Ok(DirectTransport {
                    stream: Box::new(stream),
                    local_addr: None,
                    peer_addr: None,
                })
            })
        }))
        .build()
        .map_err(|e| anyhow::anyhow!("RDP 配置失败: {e:?}"))?;

    let (out_tx, mut out_rx) = ironrdp::client::output_channel::output_channel(2);
    let mut client = RdpClient::new(config, out_tx).with_desktop_updates();
    if let Some(dir) = &share {
        #[cfg(windows)]
        {
            use ironrdp_rdpdr_native::{RedirectedDrive, WindowsRdpdrBackendFactory};
            let drive = RedirectedDrive::new(1, "RTShare", dir, false)
                .map_err(|e| anyhow::anyhow!("共享目录配置失败（{dir}）: {e}"))?;
            client = client.with_rdpdr_backend_factory(Box::new(WindowsRdpdrBackendFactory::new(drive)));
            println!("[probe] 已启用驱动器重定向：{dir} → \\\\tsclient\\RTShare");
        }
        #[cfg(not(windows))]
        {
            anyhow::bail!("共享目录探针目前只在 Windows 上可用");
        }
    }

    // client.run() 不是 Send：放到独立线程 + 独立单线程运行时（与应用一致）
    std::thread::Builder::new()
        .name("rdp-probe-client".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("[probe] runtime 创建失败: {e}");
                    return;
                }
            };
            rt.block_on(client.run());
        })?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut frames = 0u32;

    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), out_rx.recv()).await {
            Ok(Some(out)) => match out {
                RdpOutputEvent::Connected => {
                    println!("[probe] *** 连接激活完成（安全协商通过） ***");
                }
                RdpOutputEvent::LoginComplete => {
                    println!("[probe] *** 远端登录完成 ***");
                }
                RdpOutputEvent::DesktopUpdate(update) => {
                    frames += 1;
                    let (_buffer, w, h, _region) = update.into_parts();
                    println!("[probe] DesktopUpdate #{} {}x{}", frames, w.get(), h.get());
                }
                RdpOutputEvent::Image { width, height, .. } => {
                    frames += 1;
                    println!(
                        "[probe] Image #{} {}x{}",
                        frames,
                        width.get(),
                        height.get()
                    );
                }
                RdpOutputEvent::Terminated(reason) => {
                    println!("\n[probe] *** 会话终止: {reason:?} ***");
                    break;
                }
                RdpOutputEvent::ConnectionFailure(err) => {
                    println!("\n[probe] *** 连接失败: {err:?} ***");
                    break;
                }
                RdpOutputEvent::PointerBitmap(p) => {
                    println!(
                        "[probe] PointerBitmap {}x{} hotspot=({},{})",
                        p.width, p.height, p.hotspot_x, p.hotspot_y
                    );
                }
                RdpOutputEvent::PointerPosition { x, y } => {
                    println!("[probe] PointerPosition ({x},{y})");
                }
                RdpOutputEvent::PointerHidden => println!("[probe] PointerHidden"),
                RdpOutputEvent::PointerDefault => println!("[probe] PointerDefault"),
                other => {
                    if frames == 0 {
                        println!("[probe] 其它事件 {other:?}");
                    }
                }
            },
            Ok(None) => {
                println!("\n[probe] *** 输出通道关闭 ***");
                break;
            }
            Err(_) => {
                // 超时：说明还没拿到任何事件，打印心跳
                if frames == 0 {
                    println!("[probe] 等待中…（尚无事件）");
                }
            }
        }
    }

    println!("\n[probe] 统计: {frames} 帧");
    Ok(())
}
