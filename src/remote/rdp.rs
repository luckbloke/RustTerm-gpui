//! RDP 客户端 —— 基于 vendor 的 IronRDP。
//!
//! 与 VNC 一样把帧统一成 RGBA8 脏矩形，UI 侧用同一套贴图逻辑。
//! 证书验证采用「首次信任」策略：不弹窗、不落盘，直接接受
//! （与参考实现里 accept-temporarily 的策略等价）。

use anyhow::{anyhow, Result};
use ironrdp::client::config::{
    ClipboardType, ConfigBuilder, Destination, DirectTransport, ServerCertificateVerifier,
    ServerCertificateVerifyFuture, TransportKind,
};
use ironrdp::client::rdp::{RdpClient, RdpInputEvent, RdpOutputEvent};
use ironrdp::input::{Database, MouseButton, MousePosition, Operation, Scancode, WheelRotations};
use ironrdp::pdu::geometry::InclusiveRectangle;
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use super::{KeyInput, RdCommand, RdEvent, RdParams, RdState};
use std::sync::Arc;

use super::rdp_clipboard::{ClipboardBridgeGuard, RdpClipboardBackendFactory, RdpClipboardBridge};

const FRAME_INTERVAL: Duration = Duration::from_millis(33);
const POLL_INTERVAL: Duration = Duration::from_millis(8);

/// 接受任意服务器证书。
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

/// 连接时采用的安全层。
///
/// - `Nla`：CredSSP/NLA + TLS，现代服务器首选；
/// - `TlsOnly`：只用 TLS，凭据在 Client Info 里自动登录 —— 旧版
///   Windows Server 2008 / 2008 R2 的兼容路径；
/// - `RdpStandard`：Standard RDP Security（传统 RC4 加密层）。服务器把
///   「RDP 安全层」策略设为「RDP」时只接受这一种（报
///   SSL_NOT_ALLOWED_BY_SERVER），vendored IronRDP 已实现完整 RC4 路径。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SecurityMode {
    Nla,
    TlsOnly,
    RdpStandard,
}

impl SecurityMode {
    fn uses_credssp(self) -> bool {
        matches!(self, SecurityMode::Nla)
    }
}

/// 判断错误是否为「服务器只支持 Standard RDP Security」。
///
/// 对应 X.224 协商失败码 SSL_NOT_ALLOWED_BY_SERVER，连接器报
/// "server only supports Standard RDP Security"。
fn looks_like_standard_rdp_only_failure(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_lowercase();
    text.contains("standard rdp security")
        || text.contains("ssl_not_allowed_by_server")
        || text.contains("ssl not allowed by server")
}

fn build_config(
    params: &RdParams,
    security: SecurityMode,
) -> Result<ironrdp::client::config::Config> {
    let connector_host = params.host.clone();
    let connector_port = params.port;
    // 只有非空目录才挂盘符：空字符串会让 RDPDR 后端挂一个根目录不明的盘
    let share_dir = params
        .share_dir
        .as_deref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let color_depth = match params.color_depth {
        16 => 16u32,
        24 => 24,
        _ => 32,
    };
    ConfigBuilder::new()
        .with_destination(Destination::from_parts(params.host.clone(), params.port))
        .with_transport(TransportKind::Direct)
        .with_username(params.username.clone())
        .with_domain(params.domain.clone())
        .with_password(params.password.clone())
        .with_desktop_width(params.width)
        .with_desktop_height(params.height)
        .with_desktop_scale_factor(100)
        .with_color_depth(color_depth)
        // NLA（CredSSP）与 TLS 是两条独立的安全层：旧版服务器只走 TLS。
        // TLS-only 时服务器会显示登录界面，自动登录标志让它在 Client Info
        // 里直接用表单里的用户名/密码登录，体验与 mstsc 一致。
        // RdpStandard：整个连接跑在传统 RC4 加密层上，同样用自动登录。
        .with_credssp(security.uses_credssp())
        .with_tls(!matches!(security, SecurityMode::RdpStandard))
        .with_standard_rdp_security(matches!(security, SecurityMode::RdpStandard))
        .with_autologon(matches!(security, SecurityMode::TlsOnly | SecurityMode::RdpStandard))
        // 剪贴板同步：Enable 时由 ironrdp-client 在 Windows 上自建 WinClipboard
        // 后端 —— 远端→本机直接写系统剪贴板，本机→远端由它监听系统剪贴板变化，
        // UI 层不经手数据。Disable 则完全不协商 CLIPRDR 通道。
        .with_clipboard(if params.clipboard {
            ClipboardType::Enable
        } else {
            ClipboardType::Disable
        })
        .with_platform(MajorPlatformType::WINDOWS)
        .with_client_build(client_build())
        .with_client_dir("C:\\Windows\\System32\\mstscax.dll")
        .with_client_name(client_name())
        // 共享目录：必须同时打开 RDPDR 通道（设备重定向）才会协商盘符
        .with_rdpdr(share_dir.is_some())
        .with_server_certificate_verifier(std::sync::Arc::new(TrustAnyCert))
        // 与参考实现一致：自己拨号，绕开 connector 内部的 DNS/地址处理差异
        .with_direct_transport_connector(std::sync::Arc::new(move |_destination| {
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
        .map_err(|e| anyhow!("RDP 配置失败: {e:?}"))
}

type Dirty = Option<(u32, u32, u32, u32)>;

fn union(dirty: Dirty, r: (u32, u32, u32, u32)) -> Dirty {
    if r.2 == 0 || r.3 == 0 {
        return dirty;
    }
    Some(match dirty {
        None => r,
        Some(d) => {
            let x1 = d.0.min(r.0);
            let y1 = d.1.min(r.1);
            let x2 = (d.0 + d.2).max(r.0 + r.2);
            let y2 = (d.1 + d.3).max(r.1 + r.3);
            (x1, y1, x2 - x1, y2 - y1)
        }
    })
}

struct Fb {
    pixels: Vec<u32>,
    width: u32,
    height: u32,
}

impl Fb {
    fn new(width: u32, height: u32) -> Self {
        Self {
            pixels: vec![0u32; width as usize * height as usize],
            width,
            height,
        }
    }

    fn apply_region(&mut self, buffer: &[u32], region: InclusiveRectangle) -> (u32, u32, u32, u32) {
        let rx = region.left as usize;
        let ry = region.top as usize;
        let rw = (region.right as usize).saturating_sub(rx) + 1;
        let rh = (region.bottom as usize).saturating_sub(ry) + 1;
        let fw = self.width as usize;
        let fh = self.height as usize;
        let cw = rw.min(fw.saturating_sub(rx));
        let ch = rh.min(fh.saturating_sub(ry));
        if cw == 0 || ch == 0 {
            return (0, 0, 0, 0);
        }
        for row in 0..ch {
            let so = row * rw;
            if so + cw > buffer.len() {
                break;
            }
            let doff = (ry + row) * fw + rx;
            self.pixels[doff..doff + cw].copy_from_slice(&buffer[so..so + cw]);
        }
        (rx as u32, ry as u32, cw as u32, ch as u32)
    }

    fn sub_rgba(&self, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
        let stride = self.width as usize;
        let mut out = Vec::with_capacity(w as usize * h as usize * 4);
        for row in 0..h as usize {
            let off = (y as usize + row) * stride + x as usize;
            for col in 0..w as usize {
                let p = self.pixels[off + col];
                // ironrdp 的 u32 像素是 0x00RRGGBB
                out.push(((p >> 16) & 0xff) as u8);
                out.push(((p >> 8) & 0xff) as u8);
                out.push((p & 0xff) as u8);
                out.push(255);
            }
        }
        out
    }
}

/// 共享目录 → 远端资源管理器里显示的名字。
///
/// 取目录名（整卷 `D:\` 取盘符，和 mstsc 的习惯一致）。Windows 实际把
/// RDPDR 的 DeviceData 按 8-bit 字符解析（上游 IronRDP #2342），vendored
/// 补丁已改为 8-bit 广播，目录名可以完整显示；名字里的非 ASCII 与非法字符
/// 由广播层替换成 `_`。
fn share_drive_name(root: &str) -> String {
    let directory = std::path::Path::new(root)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty());
    match directory {
        Some(name) => name,
        None => root
            .chars()
            .next()
            .map(|letter| letter.to_ascii_uppercase().to_string())
            .unwrap_or_else(|| "RustTerm".to_string()),
    }
}

/// 共享目录 → RDPDR 后端工厂。
///
/// IronRDP 的 `rdpdr-native` 已经把「本地目录 ↔ RDPDR 文件 IRP」这一层
/// 做完了（打开 / 读 / 写 / 列目录 / 查询信息 / 卷信息 / 锁），
/// 我们只需要指定「哪个目录、挂成什么名字、只读与否」。
#[cfg(windows)]
fn rdpdr_factory(root: &str) -> Result<Box<dyn ironrdp::client::rdp::RdpdrBackendFactory + Send>> {
    use ironrdp_rdpdr_native::{RedirectedDrive, WindowsRdpdrBackendFactory};
    // 后端把「共享目录」当成 RDPDR 的根句柄，只接受本机绝对路径：
    // `D:\`、`D:\rustterm` 都行，`D:rustterm`（盘符相对）、`\rustterm`、
    // `\\server\share`、`\\?\D:\...` 会被解析到别处，这里先拦下来给人话提示。
    let bytes = root.as_bytes();
    let is_absolute_drive_path = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    if !is_absolute_drive_path {
        return Err(anyhow!(
            "共享目录必须是本机绝对路径，例如 D:\\rustterm（当前填的是“{root}”）"
        ));
    }
    // 设备号 0 保留给智能卡通道，盘符从 1 开始。
    let drive = RedirectedDrive::new(1, share_drive_name(root), root, false)
        .map_err(|e| anyhow!("共享目录配置失败（{root}）: {e}"))?;
    Ok(Box::new(WindowsRdpdrBackendFactory::new(drive)))
}

#[cfg(not(windows))]
fn rdpdr_factory(_root: &str) -> Result<Box<dyn ironrdp::client::rdp::RdpdrBackendFactory + Send>> {
    Err(anyhow!("共享目录目前只在 Windows 客户端上可用"))
}

/// 把归一化键输入转成 ironrdp 操作。
fn key_operations(k: &KeyInput) -> Vec<Operation> {
    let extended = k.scancode & 0xFF00 == 0xE000;
    let code = (k.scancode & 0x00FF) as u8;
    // 有扫描码就按物理键发；否则退回 Unicode 通道（输入法/特殊字符）
    if code != 0 {
        let sc = Scancode::from_u8(extended, code);
        return vec![if k.down {
            Operation::KeyPressed(sc)
        } else {
            Operation::KeyReleased(sc)
        }];
    }
    match k.ch {
        Some(ch) if k.down => vec![
            Operation::UnicodeKeyPressed(ch),
            Operation::UnicodeKeyReleased(ch),
        ],
        _ => Vec::new(),
    }
}

/// 一批操作交给 Database 转成 fast-path 输入事件再发出去。
fn flush_ops(
    db: &mut Database,
    sender: &ironrdp::client::rdp::RdpInputSender,
    ops: Vec<Operation>,
) {
    if ops.is_empty() {
        return;
    }
    let fast = db.apply(ops);
    if !fast.is_empty() {
        let _ = sender.try_send(RdpInputEvent::FastPath(fast));
    }
}

/// 一次连接尝试的失败信息。
struct AttemptFailed {
    /// 是否已经真正连上（收到过桌面帧）。连上之后的错误不再回退重试。
    connected: bool,
    error: anyhow::Error,
}

/// 判断错误是否像「NLA 与旧版服务器不兼容」。
///
/// Windows Server 2008 / 2008 R2 在 CredSSP 的 NTLM AUTHENTICATE 之后会回
/// TLS alert 80，或者服务端解析 TSRequest 失败（`invalid sequence tag`）。
/// 这两种情况下 TLS-only 通常能正常连上，值得自动重试一次。
///
/// 凭据错误（密码错、账户锁定等）不算：换 TLS 也解决不了，直接报给用户。
fn looks_like_nla_failure(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_lowercase();
    const CREDENTIAL: &[&str] = &[
        "logon failure",
        "status_logon_failure",
        "wrong password",
        "password expired",
        "account disabled",
        "account restriction",
        "invalid credentials",
    ];
    if CREDENTIAL.iter().any(|needle| text.contains(needle)) {
        return false;
    }
    const NLA: &[&str] = &[
        "credssp",
        "tsrequest",
        "ts_request",
        "invalid sequence tag",
        "sequence tag",
        "tls alert",
        "alertreceived",
        "pub_key_auth",
        "pubkeyauth",
        "ntlm",
        "sspi",
        "nla",
        "internal_error",
        "handshake",
        "output channel closed",
        "输出通道关闭",
        "会话终止",
    ];
    NLA.iter().any(|needle| text.contains(needle))
}

/// 服务器在协商响应里明确选择了 CredSSP（PROTOCOL_HYBRID）——这种情况下
/// TLS-only 与 Standard RDP 都会被拒，必须走 NLA。用户勾选的「兼容模式」
/// 只是偏好，服务器的明示要求优先：据此触发一次 NLA 重试。
fn looks_like_credssp_required(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_lowercase();
    text.contains("credssp")
        && (text.contains("requires") || text.contains("enhanced rdp security") || text.contains("hybrid"))
}

pub async fn run(
    params: &RdParams,
    ev: &Sender<RdEvent>,
    cmds: &Receiver<RdCommand>,
    cancel: &AtomicBool,
) -> Result<()> {
    // 勾选兼容模式时直接 TLS-only，不再浪费一次 NLA 握手；
    // 默认先试 NLA，失败且符合旧服务器特征时自动回退一次。
    let first = if params.legacy_compat {
        SecurityMode::TlsOnly
    } else {
        SecurityMode::Nla
    };
    match run_once(params, first, ev, cmds, cancel).await {
        Ok(()) => Ok(()),
        // 兼容模式下服务器明确要求 CredSSP：忽略勾选项，改用 NLA 重试一次。
        Err(fail)
            if !fail.connected
                && first == SecurityMode::TlsOnly
                && looks_like_credssp_required(&fail.error) =>
        {
            eprintln!("[RDP] 服务器要求 CredSSP (NLA)，改用 NLA 重试：{}", fail.error);
            let _ = ev.send(RdEvent::Notice(
                "服务器要求 NLA 认证，正在用 CredSSP 重试…".to_string(),
            ));
            run_once(params, SecurityMode::Nla, ev, cmds, cancel)
                .await
                .map_err(|fail| fail.error)
        }
        Err(fail)
            if !fail.connected
                && first == SecurityMode::Nla
                && looks_like_nla_failure(&fail.error) =>
        {
            eprintln!("[RDP] NLA 握手失败，改用 TLS 兼容模式重试：{}", fail.error);
            let _ = ev.send(RdEvent::Notice(
                "NLA 握手失败，正在用 TLS 兼容模式重试…".to_string(),
            ));
            match run_once(params, SecurityMode::TlsOnly, ev, cmds, cancel).await {
                Ok(()) => Ok(()),
                Err(fail)
                    if !fail.connected && looks_like_standard_rdp_only_failure(&fail.error) =>
                {
                    eprintln!("[RDP] 服务器只支持 Standard RDP Security，改用 RC4 安全层重试：{}", fail.error);
                    let _ = ev.send(RdEvent::Notice(
                        "服务器只支持 RDP 安全层，正在用 Standard RDP Security 重试…".to_string(),
                    ));
                    run_once(params, SecurityMode::RdpStandard, ev, cmds, cancel)
                        .await
                        .map_err(|fail| fail.error)
                }
                Err(fail) => Err(fail.error),
            }
        }
        Err(fail)
            if !fail.connected && looks_like_standard_rdp_only_failure(&fail.error) =>
        {
            eprintln!("[RDP] 服务器只支持 Standard RDP Security，改用 RC4 安全层重试：{}", fail.error);
            let _ = ev.send(RdEvent::Notice(
                "服务器只支持 RDP 安全层，正在用 Standard RDP Security 重试…".to_string(),
            ));
            run_once(params, SecurityMode::RdpStandard, ev, cmds, cancel)
                .await
                .map_err(|fail| fail.error)
        }
        Err(fail) => Err(fail.error),
    }
}

async fn run_once(
    params: &RdParams,
    security: SecurityMode,
    ev: &Sender<RdEvent>,
    cmds: &Receiver<RdCommand>,
    cancel: &AtomicBool,
) -> Result<(), AttemptFailed> {
    let _ = ev.send(RdEvent::State(RdState::Connecting));
    let mut connected = false;
    let result: Result<()> = async {
        let config = build_config(params, security)?;
        let (out_tx, mut out_rx) = ironrdp::client::output_channel::output_channel(2);
        let mut client = RdpClient::new(config, out_tx).with_desktop_updates();
        // 共享目录：挂一个本地目录当远端的盘符（RDPDR 通道）。
        // 目录不存在时直接报错，别让用户连上以后在「我的电脑」里找不到盘。
        if let Some(dir) = params
            .share_dir
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !std::path::Path::new(dir).is_dir() {
                return Err(anyhow!("共享目录不存在或不是目录: {dir}"));
            }
            eprintln!(
                "[RDP] 共享目录 {dir} → 远端「{} on {}」（RDPDR 驱动器重定向；要排查细节请设 NYATERM_RDPDR_TRACE=1）",
                share_drive_name(dir),
                client_name()
            );
            client = client.with_rdpdr_backend_factory(rdpdr_factory(dir)?);
        }
        let input_sender = client.input_sender();

        let _ = ev.send(RdEvent::State(RdState::Authenticating));

        // ---- 剪贴板：文本 + 文件 ----
        // 文件传输两条路只能选一条，否则远端会同时看到「盘符」和「剪贴板文件」，
        // 用户粘贴时容易撞上锁冲突：
        // - 配了共享目录 → 文件走 RDPDR 盘符（大文件、目录树、断点续传都更稳）
        // - 没配共享目录 → 文件走剪贴板（本机复制 → 远端粘贴，零配置）
        // 文本始终走剪贴板。
        let mut _clip_guard: Option<ClipboardBridgeGuard> = None;
        if params.clipboard {
            let file_over_clipboard = params
                .share_dir
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_none();
            let bridge = Arc::new(RdpClipboardBridge::new(ev.clone(), file_over_clipboard));
            client = client.with_cliprdr_backend_factory(Box::new(
                RdpClipboardBackendFactory::from_input_sender(bridge.clone(), input_sender.clone()),
            ));
            // 会话结束后自动停掉轮询线程
            _clip_guard = Some(ClipboardBridgeGuard(bridge));
        }

        // client.run() 不是 Send：按参考实现放到独立线程 + 独立单线程运行时
        std::thread::Builder::new()
            .name("rdp-client".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        eprintln!("[RDP] runtime 创建失败: {e}");
                        return;
                    }
                };
                rt.block_on(client.run());
            })
            .map_err(|e| anyhow!("启动 RDP 客户端线程失败: {e}"))?;

        let mut fb: Option<Fb> = None;
        let mut dirty: Dirty = None;
        let mut last_frame = Instant::now() - FRAME_INTERVAL;
        let mut db = Database::new();
        let mut buttons: u8 = 0;
        let mut last_pos = (0u16, 0u16);
        let mut active = false;

        loop {
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }

            // ---- 上行输入 ----
            while let Ok(cmd) = cmds.try_recv() {
                match cmd {
                    RdCommand::Close => return Ok(()),
                    RdCommand::Key(k) => {
                        let ops = key_operations(&k);
                        if !ops.is_empty() {
                            flush_ops(&mut db, &input_sender, ops);
                        }
                    }
                    RdCommand::SyncLockKeys { scroll, num, caps } => {
                        // Input Event Sync：绝对设置远端锁定键（不受修饰键影响，
                        // 是扫描码 CapsLock 做不到的——Windows 忽略 Shift 按住时
                        // 的 CapsLock 切换）。绕过 Database：锁定键不属于
                        // Database 跟踪的按下/抬起状态。
                        use ironrdp::pdu::input::fast_path::SynchronizeFlags;
                        let mut flags = SynchronizeFlags::empty();
                        if scroll {
                            flags |= SynchronizeFlags::SCROLL_LOCK;
                        }
                        if num {
                            flags |= SynchronizeFlags::NUM_LOCK;
                        }
                        if caps {
                            flags |= SynchronizeFlags::CAPS_LOCK;
                        }
                        let _ = input_sender.try_send(RdpInputEvent::FastPath(
                            smallvec::smallvec![ironrdp::pdu::input::fast_path::FastPathInputEvent::SyncEvent(flags)],
                        ));
                    }
                    RdCommand::Pointer { x, y, buttons: nb } => {
                        let mut ops = vec![Operation::MouseMove(MousePosition { x, y })];
                        if nb != buttons {
                            for (bit, btn) in [
                                (1u8, MouseButton::Left),
                                (2u8, MouseButton::Middle),
                                (4u8, MouseButton::Right),
                            ] {
                                let was = buttons & bit != 0;
                                let now = nb & bit != 0;
                                if was != now {
                                    ops.push(if now {
                                        Operation::MouseButtonPressed(btn)
                                    } else {
                                        Operation::MouseButtonReleased(btn)
                                    });
                                }
                            }
                            buttons = nb;
                        }
                        last_pos = (x, y);
                        flush_ops(&mut db, &input_sender, ops);
                    }
                    RdCommand::Wheel { dy } => {
                        if dy != 0 {
                            let ops = vec![
                                Operation::MouseMove(MousePosition {
                                    x: last_pos.0,
                                    y: last_pos.1,
                                }),
                                Operation::WheelRotations(WheelRotations {
                                    is_vertical: true,
                                    rotation_units: dy.saturating_mul(120),
                                }),
                            ];
                            flush_ops(&mut db, &input_sender, ops);
                        }
                    }
                    // 剪贴板由 CLIPRDR 通道 + WinClipboard 后端自行双向同步
                    //（本机剪贴板变化由后端线程直接监听，无需 UI 转发）。
                    RdCommand::Clipboard(_) => {}
                }
            }

            // ---- 下行帧 ----
            match tokio::time::timeout(POLL_INTERVAL, out_rx.recv()).await {
                Ok(Some(out)) => match out {
                    RdpOutputEvent::DesktopUpdate(update) => {
                        let (buffer, w, h, region) = update.into_parts();
                        let (w, h) = (w.get() as u32, h.get() as u32);
                        let created = fb.is_none();
                        let f = fb.get_or_insert_with(|| Fb::new(w, h));
                        if f.width != w || f.height != h {
                            *f = Fb::new(w, h);
                            let _ = ev.send(RdEvent::Resized {
                                width: w,
                                height: h,
                            });
                            dirty = None;
                        } else if created {
                            let _ = ev.send(RdEvent::Resized {
                                width: w,
                                height: h,
                            });
                        }
                        let r = f.apply_region(&buffer, region);
                        dirty = union(dirty, r);
                        if !active {
                            active = true;
                            connected = true;
                            let _ = ev.send(RdEvent::State(RdState::Active));
                        }
                    }
                    RdpOutputEvent::Image {
                        buffer,
                        width,
                        height,
                    } => {
                        let (w, h) = (width.get() as u32, height.get() as u32);
                        let mut f = Fb::new(w, h);
                        let region = InclusiveRectangle {
                            left: 0,
                            top: 0,
                            right: w.saturating_sub(1) as u16,
                            bottom: h.saturating_sub(1) as u16,
                        };
                        f.apply_region(&buffer, region);
                        fb = Some(f);
                        let _ = ev.send(RdEvent::Resized {
                            width: w,
                            height: h,
                        });
                        dirty = Some((0, 0, w, h));
                        if !active {
                            active = true;
                            connected = true;
                            let _ = ev.send(RdEvent::State(RdState::Active));
                        }
                    }
                    RdpOutputEvent::Connected => {
                        // 安全协商与通道激活都已完成：之后如果再失败，就不是
                        // NLA 兼容问题，不能再回退成 TLS-only。
                        connected = true;
                        eprintln!("[RDP] 连接激活完成（安全层={security:?}）");
                    }
                    RdpOutputEvent::LoginComplete => {
                        eprintln!("[RDP] 远端登录完成");
                    }
                    RdpOutputEvent::Terminated(reason) => {
                        return Err(anyhow!("RDP 会话终止: {reason:?}"));
                    }
                    RdpOutputEvent::ConnectionFailure(err) => {
                        // report() 会带上 source 链（CredSSP/TSRequest 的具体原因），
                        // 否则只剩「连接失败」四个字，NLA 兼容问题很难定位。
                        return Err(anyhow!("RDP 连接失败: {}", err.report()));
                    }
                    // ---- 远端光标 ----
                    // 服务器下发光标形状与位置，客户端负责绘制。
                    // 不处理的话画面里就没有远端光标，而本地系统光标还在 →
                    // 用户看到「两个鼠标」或者「远端没有指针」。
                    RdpOutputEvent::PointerBitmap(ptr) => {
                        let _ = ev.send(RdEvent::Cursor {
                            width: ptr.width,
                            height: ptr.height,
                            hotspot_x: ptr.hotspot_x,
                            hotspot_y: ptr.hotspot_y,
                            // ironrdp 的 bitmap_data 已是 RGBA8 预乘 alpha
                            rgba: ptr.bitmap_data.clone(),
                        });
                    }
                    RdpOutputEvent::PointerPosition { x, y } => {
                        let _ = ev.send(RdEvent::CursorPos { x, y });
                    }
                    RdpOutputEvent::PointerHidden => {
                        let _ = ev.send(RdEvent::CursorHidden);
                    }
                    RdpOutputEvent::PointerDefault => {
                        // 恢复默认形状：宽度/高度为 0 表示「无自定义光标」，
                        // 客户端据此隐藏远端光标绘制，退回本地系统光标。
                        let _ = ev.send(RdEvent::CursorHidden);
                    }
                    RdpOutputEvent::KeyboardIndicators(flags) => {
                        let _ = ev.send(RdEvent::KeyboardIndicators(flags));
                    }
                    _ => {}
                },
                Ok(None) => return Err(anyhow!("RDP 输出通道关闭")),
                Err(_) => {}
            }

            // ---- 推送脏区 ----
            if let (Some(rect), Some(f)) = (dirty, fb.as_ref()) {
                if last_frame.elapsed() >= FRAME_INTERVAL {
                    last_frame = Instant::now();
                    dirty = None;
                    let (x, y, w, h) = rect;
                    let w = w.min(f.width.saturating_sub(x));
                    let h = h.min(f.height.saturating_sub(y));
                    if w > 0 && h > 0 {
                        let rgba = f.sub_rgba(x, y, w, h);
                        let _ = ev.send(RdEvent::Frame { x, y, w, h, rgba });
                    }
                }
            }
        }
    }
    .await;
    result.map_err(|error| AttemptFailed { connected, error })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 旧版 Server 2008 的典型报错要能触发自动回退；
    /// 凭据错误和本地配置错误则不能白白多连一次。
    #[test]
    fn nla_failure_detection_matches_old_server_errors() {
        assert!(looks_like_nla_failure(&anyhow::anyhow!(
            "invalid sequence tag identifier"
        )));
        assert!(looks_like_nla_failure(&anyhow::anyhow!(
            "RDP 连接失败: TLS alert received: alert 80"
        )));
        assert!(looks_like_nla_failure(&anyhow::anyhow!(
            "CredSSP NTLM authentication failed"
        )));
        assert!(!looks_like_nla_failure(&anyhow::anyhow!(
            "logon failure: unknown user name or bad password"
        )));
        assert!(!looks_like_nla_failure(&anyhow::anyhow!(
            "共享目录不存在或不是目录: D:\\share"
        )));
    }

    #[cfg(windows)]
    #[test]
    fn rdpdr_factory_requires_an_absolute_local_path() {
        assert!(rdpdr_factory(r"D:\share").is_ok());
        assert!(rdpdr_factory(r"D:\").is_ok());
        assert!(rdpdr_factory(r"D:share").is_err());
        assert!(rdpdr_factory(r"\share").is_err());
        assert!(rdpdr_factory(r"\\server\share").is_err());
        assert!(rdpdr_factory("relative").is_err());
    }

    /// 远端显示的名字要能区分共享的是哪个目录；整卷共享用盘符（与 mstsc 一致）。
    #[test]
    fn share_drive_name_follows_the_shared_directory() {
        assert_eq!(share_drive_name(r"D:\rustterm"), "rustterm");
        assert_eq!(share_drive_name(r"D:\rustterm\sub"), "sub");
        assert_eq!(share_drive_name(r"D:\我的 目录"), "我的 目录");
        assert_eq!(share_drive_name(r"D:\"), "D");
        assert_eq!(share_drive_name("D:/"), "D");
    }
}
