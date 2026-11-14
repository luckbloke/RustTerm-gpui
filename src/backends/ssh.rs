//! SSH 后端：与主工程 src-tauri/src/ssh.rs 相同的 vendor russh（0.62 fork）。
//!
//! P1 内容：
//! - 算法协商与主工程一致：默认安全算法之后追加 SHA-1/CBC/des 兜底；
//! - 主机密钥校验：accept-new（TOFU）策略，密钥变更一律拒绝；
//! - 认证：password 失败后 fallback 到 keyboard-interactive（PAM 场景）。

use anyhow::{anyhow, bail, Result};
use russh::client::{self, ChannelOpenHandle, Handle, Session};
use russh::keys::PrivateKeyWithHashAlg;
use russh::{Channel, ChannelMsg, ChannelOpenFailure};
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

use super::TermIo;

/// task-22 多显示：X 屏幕编号源（每条启用 X11 的 SSH 连接递增一个）。
static NEXT_X11_SCREEN: AtomicUsize = AtomicUsize::new(1);

/// SSH X11 转发：服务端反向打开的 X11 协议通道（`x11` 与兼容模式的
/// `forwarded-tcpip`），交给内置 X server（`crate::remote::x11`）。
pub type X11Channel = Channel<client::Msg>;

/// 等 `x11-req` 回复的上限。超过就不能再等：用户在等一个能用的终端，
/// 而「没有回复」本身就是要报出去的结论。
const X11_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);
/// 等 `tcpip-forward` 回复的上限（真实服务端都会回）。
const X11_FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// 生成 X11 转发的认证 cookie：16 个随机字节 → 32 个 hex 字符。
/// 内置 X server 不校验 cookie，只求格式合法且每次连接不同。
fn x11_cookie() -> String {
    let uuid = uuid::Uuid::new_v4();
    let mut out = String::with_capacity(32);
    for byte in uuid.as_bytes() {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// 等一个通道请求的回复：`Some(true)` = Success，`Some(false)` = Failure，
/// `None` = 超时或通道结束。
///
/// 调用点保证此时只有这一条请求要回复（pty-req 与 shell 都是 want_reply=false）。
async fn await_channel_reply(channel: &mut Channel<client::Msg>) -> Option<bool> {
    let deadline = tokio::time::Instant::now() + X11_REPLY_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        match tokio::time::timeout(left, channel.wait()).await {
            Ok(Some(ChannelMsg::Success)) => return Some(true),
            Ok(Some(ChannelMsg::Failure)) => return Some(false),
            Ok(Some(_)) => continue,
            Ok(None) => return None,
            Err(_) => return None,
        }
    }
}

/// SSH 认证方式。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SshAuth {
    /// 密码（失败后自动 fallback 到 keyboard-interactive）
    Password(String),
    /// 私钥文件（可带 passphrase）
    Key {
        path: String,
        passphrase: String,
    },
}

impl SshAuth {
    pub fn is_empty(&self) -> bool {
        match self {
            SshAuth::Password(p) => p.is_empty(),
            SshAuth::Key { path, .. } => path.is_empty(),
        }
    }
}

/// 跳板机（ProxyJump）配置。
///
/// 实现方式：先连跳板机并完成认证，再用 `direct-tcpip` 通道把
/// 「跳板机 → 目标机」的 TCP 流接到目标机的 SSH 握手上（真正的 ProxyJump，
/// 不是本地端口转发）。
#[derive(Clone, Debug)]
pub struct JumpHost {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// 跳板机凭据；多数场景与目标机相同，故沿用同一种认证方式
    pub auth: SshAuth,
}

impl Default for JumpHost {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: SshAuth::Password(String::new()),
        }
    }
}

impl JumpHost {
    /// 解析 `user@host:port` 形式（各部分可省略）。
    pub fn parse(spec: &str, default_user: &str, auth: SshAuth) -> Option<Self> {
        let spec = spec.trim();
        if spec.is_empty() {
            return None;
        }
        let (user, rest) = match spec.rsplit_once('@') {
            Some((u, r)) => (u.to_string(), r),
            None => (default_user.to_string(), spec),
        };
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.trim().parse::<u16>().unwrap_or(22),
            ),
            None => (rest.to_string(), 22),
        };
        if host.trim().is_empty() {
            return None;
        }
        Some(Self {
            host: host.trim().to_string(),
            port,
            user: if user.trim().is_empty() {
                default_user.to_string()
            } else {
                user.trim().to_string()
            },
            auth,
        })
    }
}

/// known_hosts 路径：显式取主目录（不用 russh 默认的 env::home_dir）。
fn known_hosts_path() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(Path::new(&home).join(".ssh").join("known_hosts"))
}

/// accept-new 策略的主机密钥校验：
/// - 命中记录 → 信任；
/// - 全新主机 → 写入 known_hosts 后放行（TOFU）；
/// - 记录不一致 → 拒绝（防中间人，无可协商）。
///
/// 启用 SSH X11 转发时，它还负责接收服务端反向打开的 X11 通道
/// （`x11` 与兼容模式的 `forwarded-tcpip`），交给内置 X server。
struct HostKeyChecker {
    host: String,
    port: u16,
    path: Option<PathBuf>,
    /// 服务端反向打开的 `x11` 通道（sshd 自己的转发）。
    x11_sink: Option<mpsc::UnboundedSender<X11Channel>>,
    /// 兼容模式：`tcpip_forward` 建立反向端口后，服务端把每条远端连接
    /// 反向打开成 `forwarded-tcpip` 通道。承载的同样是 X11 协议字节流。
    forward_sink: Option<mpsc::UnboundedSender<X11Channel>>,
}

impl HostKeyChecker {
    /// 让该连接接受服务端反向打开的 X11 通道。两种通道承载的都是
    /// X11 协议字节流，交给同一个接收端。
    fn with_x11_sink(mut self, sink: mpsc::UnboundedSender<X11Channel>) -> Self {
        self.forward_sink = Some(sink.clone());
        self.x11_sink = Some(sink);
        self
    }
}

impl client::Handler for HostKeyChecker {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        let Some(path) = self.path.clone() else {
            return Ok(true); // 取不到主目录时退化为全放行（与 P0 行为一致）
        };
        match russh::keys::check_known_hosts_path(&self.host, self.port, key, &path) {
            // 命中已有记录
            Ok(true) => Ok(true),
            // 全新主机：写入失败不阻断连接（accept-new 语义）
            Ok(false) => {
                match russh::keys::known_hosts::learn_known_hosts_path(
                    &self.host,
                    self.port,
                    key,
                    &path,
                ) {
                    Ok(()) => {
                        eprintln!("[ssh] 首次连接 {host}:{port}，已记录主机密钥", host = self.host, port = self.port);
                        Ok(true)
                    }
                    Err(e) => {
                        eprintln!("[ssh] 警告：主机密钥写入 known_hosts 失败: {e}");
                        Ok(true)
                    }
                }
            }
            // 已有记录但密钥不一致：一律拒绝
            Err(russh::keys::Error::KeyChanged { line }) => {
                eprintln!(
                    "[ssh] 拒绝连接 {host}:{port}：known_hosts 第 {line} 行的主机密钥已变更",
                    host = self.host,
                    port = self.port
                );
                Ok(false)
            }
            Err(e) => {
                eprintln!("[ssh] 主机密钥校验 IO 错误: {e}");
                Ok(false)
            }
        }
    }

    /// 远端 X 客户端连到 sshd 时，sshd 会反向打开一条 `x11` 通道。
    /// 接受后把通道交给内置 X server；没有启用转发的连接直接拒绝。
    async fn server_channel_open_x11(
        &mut self,
        channel: X11Channel,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        match self.x11_sink.as_ref() {
            Some(sink) => {
                reply.accept().await;
                if sink.send(channel).is_err() {
                    eprintln!("[ssh] X11 通道接收端已关闭，丢弃本次连接");
                }
            }
            None => {
                crate::xsrv::log_line(format_args!(
                    "[x11-fwd] x11 通道被拒：本会话未启用 X11 转发（x11_sink=None）"
                ));
                reply
                    .reject(ChannelOpenFailure::AdministrativelyProhibited)
                    .await;
            }
        }
        Ok(())
    }

    /// 兼容模式：`tcpip_forward` 出来的远端端口被连接时，sshd 反向打开
    /// `forwarded-tcpip` 通道。字节流同样是 X11 协议，交给同一个 X server。
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: X11Channel,
        _connected_address: &str,
        _connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        match self.forward_sink.as_ref() {
            Some(sink) => {
                reply.accept().await;
                if sink.send(channel).is_err() {
                    eprintln!("[ssh] 反向转发通道接收端已关闭，丢弃本次连接");
                }
            }
            None => {
                crate::xsrv::log_line(format_args!(
                    "[x11-fwd] forwarded-tcpip 通道被拒：本会话未启用 X11 转发"
                ));
                reply
                    .reject(ChannelOpenFailure::AdministrativelyProhibited)
                    .await;
            }
        }
        Ok(())
    }
}

/// 显式配置：在默认安全算法之后，追加旧算法兜底（与主工程一致）。
fn hardened_config() -> client::Config {
    let mut config = client::Config::default();
    {
        let p = &mut config.preferred;

        // 旧 KEX：DH group1 / group14 (SHA-1)
        let mut kex: Vec<russh::kex::Name> = p.kex.iter().copied().collect();
        kex.push(russh::kex::DH_G1_SHA1);
        kex.push(russh::kex::DH_G14_SHA1);
        kex.push(russh::kex::DH_GEX_SHA1);
        p.kex = Cow::Owned(kex);

        // 旧 cipher：AES-CBC / 3DES
        let mut cipher: Vec<russh::cipher::Name> = p.cipher.iter().copied().collect();
        cipher.push(russh::cipher::AES_128_CBC);
        cipher.push(russh::cipher::AES_192_CBC);
        cipher.push(russh::cipher::AES_256_CBC);
        cipher.push(russh::cipher::TRIPLE_DES_CBC);
        p.cipher = Cow::Owned(cipher);

        // 旧 MAC：HMAC-SHA1
        let mut mac: Vec<russh::mac::Name> = p.mac.iter().copied().collect();
        mac.push(russh::mac::HMAC_SHA1);
        p.mac = Cow::Owned(mac);

        // 旧主机密钥算法：ssh-rsa (SHA-1)
        let mut key: Vec<russh::keys::Algorithm> = p.key.iter().cloned().collect();
        key.push(russh::keys::Algorithm::Rsa { hash: None });
        p.key = Cow::Owned(key);

        // NyaTerm: 压缩——X11 转发的 PutImage 是大块 RGB 像素（整窗重绘一次
        // 1~4MB），zlib 对 UI 内容压缩比 2~5x，直接决定远程 X 应用是否流畅。
        // ZLIB 立即生效；ZLIB_LEGACY（zlib@openssh.com）认证后才启用，兜底。
        // 服务器不支持时协商回落 none，无害。
        let mut compression: Vec<russh::compression::Name> =
            p.compression.iter().copied().collect();
        compression.insert(0, russh::compression::ZLIB);
        compression.insert(1, russh::compression::ZLIB_LEGACY);
        p.compression = Cow::Owned(compression);
    }
    config
}

/// 认证：密码优先，失败后 fallback 到 keyboard-interactive；或直接用私钥。
async fn authenticate(
    handle: &mut Handle<HostKeyChecker>,
    user: &str,
    auth: &SshAuth,
) -> Result<()> {
    match auth {
        SshAuth::Password(password) => {
            // 1. password
            let resp = handle
                .authenticate_password(user, password)
                .await
                .map_err(|e| anyhow!("password 认证失败: {e}"))?;
            if resp.success() {
                return Ok(());
            }
            eprintln!("[ssh] password 认证失败: {resp:?}，尝试 keyboard-interactive");

            // 2. keyboard-interactive：把密码作为提示响应（PAM 常见 "Password: " 提示）
            let resp = handle
                .authenticate_keyboard_interactive_start(user, None::<String>)
                .await
                .map_err(|e| anyhow!("keyboard-interactive 失败: {e}"))?;

            match resp {
                KeyboardInteractiveAuthResponse::Success => Ok(()),
                KeyboardInteractiveAuthResponse::Failure { .. } => {
                    bail!("认证失败：密码错误")
                }
                KeyboardInteractiveAuthResponse::InfoRequest { .. } => {
                    bail!("认证失败：服务器要求多轮键盘交互")
                }
            }
        }
        SshAuth::Key { path, passphrase } => {
            if path.trim().is_empty() {
                bail!("未填写私钥路径");
            }
            let pp = if passphrase.is_empty() {
                None
            } else {
                Some(passphrase.as_str())
            };
            // 读私钥（能解出 passphrase 保护的 OpenSSH/PEM 私钥）
            let key = russh::keys::load_secret_key(path.trim(), pp)
                .map_err(|e| anyhow!("读取私钥 {path} 失败: {e}"))?;
            // RSA 需要协商签名哈希（sha-rsa / rsa-sha2-256/512）
            let hash_alg = handle.best_supported_rsa_hash().await.ok().flatten().flatten();
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg);
            let resp = handle
                .authenticate_publickey(user, key)
                .await
                .map_err(|e| anyhow!("公钥认证失败: {e}"))?;
            if resp.success() {
                Ok(())
            } else {
                bail!("密钥认证失败：请确认该私钥已在服务器的 authorized_keys 中")
            }
        }
    }
}

use russh::client::KeyboardInteractiveAuthResponse;

pub async fn spawn(
    host: String,
    port: u16,
    user: String,
    auth: SshAuth,
    jump: Option<JumpHost>,
    cols: u16,
    rows: u16,
    x11: bool,
) -> Result<TermIo> {
    // 启用 X11 转发时：接收端通道把服务端反向打开的 X11 通道交给内置 X server
    let (x11_tx, x11_rx) = mpsc::unbounded_channel::<X11Channel>();
    let x11_rx = x11.then_some(x11_rx);
    let x11_sink = if x11 { Some(x11_tx) } else { None };

    let jump = jump.filter(|j| !j.host.trim().is_empty());
    match jump {
        None => {
            // 直连：把 TCP 流交给统一下游处理
            let tcp = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                tokio::net::TcpStream::connect((host.as_str(), port)),
            )
            .await
            .map_err(|_| anyhow!("连接 {host}:{port} 超时"))?
            .map_err(|e| anyhow!("连接 {host}:{port} 失败: {e}"))?;
            let _ = tcp.set_nodelay(true);
            open_shell(tcp, host, port, user, auth, cols, rows, None, x11, x11_rx, x11_sink).await
        }
        Some(j) => {
            // 经跳板机：在跳板机连接上开 direct-tcpip 通道
            let jcfg = Arc::new(hardened_config());
            let checker = HostKeyChecker {
                host: j.host.clone(),
                port: j.port,
                path: known_hosts_path(),
                x11_sink: None,
                forward_sink: None,
            };
            let mut jh: Handle<HostKeyChecker> = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                client::connect(jcfg, (j.host.as_str(), j.port), checker),
            )
            .await
            .map_err(|_| anyhow!("连接跳板机 {}:{} 超时", j.host, j.port))?
            .map_err(|e| anyhow!("连接跳板机 {}:{} 失败: {e}", j.host, j.port))?;

            authenticate(&mut jh, &j.user, &j.auth)
                .await
                .map_err(|e| anyhow!("跳板机 {} 认证失败: {e}", j.host))?;

            let channel = jh
                .channel_open_direct_tcpip(host.clone(), port as u32, "127.0.0.1", 0)
                .await
                .map_err(|e| {
                    anyhow!("无法在跳板机上打开到 {host}:{port} 的通道: {e}")
                })?;
            let stream = channel.into_stream();
            open_shell(
                stream,
                host,
                port,
                user,
                auth,
                cols,
                rows,
                Some(Arc::new(jh)),
                x11,
                x11_rx,
                x11_sink,
            )
            .await
        }
    }
}

/// 已在认证后的 SSH 连接上开子通道。
///
/// 终端的 IO 走独立的 session channel，SFTP 需要在这条**同一条 TCP 连接**上
/// 再开一个 channel 并请求 `sftp` 子系统——这样不用重新认证，
/// 也不用再连一次（跳板机场景尤其重要）。
pub struct SshHandle {
    handle: Arc<Handle<HostKeyChecker>>,
    /// 建链信息，供 UI 显示（user@host:port）
    pub target: String,
    /// task-22 多显示：本会话的 X 屏幕 id（启用 X11 转发时才有）。
    ///
    /// UI 据此把「X 屏幕」标签绑定到**本会话独享**的 server（互不重叠）。
    pub x11_screen_id: Option<String>,
}

impl SshHandle {
    /// 打开一个 SFTP 子系统会话。
    pub async fn open_sftp(
        &self,
    ) -> Result<russh_sftp::client::SftpSession> {
        let channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| anyhow!("打开 SFTP 通道失败: {e}"))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| anyhow!("请求 sftp 子系统失败: {e}"))?;
        russh_sftp::client::SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| anyhow!("SFTP 握手失败: {e}"))
    }
}

/// 在给定字节流上完成目标机 SSH 握手、认证并开 shell。
///
/// `x11 = true` 时：会话通道发 `x11-req`（必须在 shell 之前，sshd 才会写
/// `DISPLAY`），sshd 反向打开的 `x11` / `forwarded-tcpip` 通道由接收端交给
/// 内置 X server；`x11-req` 被拒/未回时退回**兼容模式**——`tcpip_forward`
/// 反向端口 + 往 shell 写 `export DISPLAY=localhost:{p-6000}.0`。
async fn open_shell<S>(
    stream: S,
    host: String,
    port: u16,
    user: String,
    auth: SshAuth,
    cols: u16,
    rows: u16,
    keep_jump: Option<Arc<Handle<HostKeyChecker>>>,
    x11: bool,
    x11_rx: Option<mpsc::UnboundedReceiver<X11Channel>>,
    x11_sink: Option<mpsc::UnboundedSender<X11Channel>>,
) -> Result<TermIo>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let config = Arc::new(hardened_config());
    let checker = HostKeyChecker {
        host: host.clone(),
        port,
        path: known_hosts_path(),
        x11_sink,
        forward_sink: None,
    };
    let mut handle: Handle<HostKeyChecker> =
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            client::connect_stream(config, stream, checker),
        )
        .await
        .map_err(|_| anyhow!("到 {host}:{port} 的 SSH 握手超时"))?
        .map_err(|e| anyhow!("连接 {host}:{port} 失败: {e}"))?;

    authenticate(&mut handle, &user, &auth).await?;

    let mut channel: Channel<client::Msg> = handle.channel_open_session().await?;
    channel
        .request_pty(false, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
        .await?;

    // `x11-req` 必须在 `request_shell` 之前发：sshd 只有在 shell 启动前收到它，
    // 才会给远端设置 DISPLAY/XAUTHORITY，并把后续的 `x11` 通道转回来。
    let mut x11_status: Option<bool> = None;
    if x11 {
        channel
            .request_x11(true, false, "MIT-MAGIC-COOKIE-1", x11_cookie(), 0)
            .await?;
        x11_status = await_channel_reply(&mut channel).await;
        if x11_status != Some(true) {
            eprintln!(
                "[ssh] sshd 未接受 x11-req（{:?}），尝试反向端口转发兼容模式",
                x11_status
            );
        }
    }

    channel.request_shell(false).await?;

    // 兼容模式：`x11-req` 没被接受时，把远端 loopback 的一个端口反向转回
    // 本机 X server，再往 shell 写一条 `DISPLAY`。不依赖 xauth / X11Forwarding。
    if x11 && x11_status != Some(true) {
        let asked =
            tokio::time::timeout(X11_FORWARD_TIMEOUT, handle.tcpip_forward("127.0.0.1", 0)).await;
        match asked {
            // port == 0 由服务端挑空闲端口并回传；X 的 display 编号是 端口 - 6000
            Ok(Ok(port)) if (6001..=65535).contains(&port) => {
                let name = format!("localhost:{}.0", port - 6000);
                channel
                    .data(format!("export DISPLAY={name}\n").as_bytes())
                    .await?;
                eprintln!(
                    "[ssh] sshd 未转发 x11（{x11_status:?}），已用反向端口 {port} 设置 DISPLAY={name}"
                );
            }
            Ok(Ok(port)) => {
                eprintln!("[ssh] sshd 未转发 x11（{x11_status:?}），反向端口申请返回 {port}，放弃兼容模式");
            }
            Ok(Err(e)) => {
                eprintln!("[ssh] sshd 未转发 x11（{x11_status:?}），反向端口申请失败（远端可能禁用了 AllowTcpForwarding）：{e}");
            }
            Err(_) => {
                eprintln!("[ssh] sshd 未转发 x11（{x11_status:?}），反向端口申请超时，放弃兼容模式");
            }
        }
    }

    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();
    let (output_tx, output_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // task-22 多显示：**每条连接**独享一个 X 屏幕（MobaXterm 式：同一主机的
    // 两条 SSH 连接也有各自的屏幕，互不重叠）。id 同时交给 SshHandle（UI 的
    // X 屏幕标签按它绑定）与消费循环（通道按它接入）。会话结束时消费循环会
    // `close_screen` 回收该屏幕（display 编号一并释放）。
    let x11_screen_id = if x11_rx.is_some() {
        Some(format!(
            "ssh-{}",
            NEXT_X11_SCREEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    } else {
        None
    };

    // 保持 Handle 存活直到循环退出（跳板机 Handle 也要一并保活）
    let keep = Arc::new(handle);
    // 同一个连接上给 SFTP 面板留一个句柄（不额外建链、不重复认证）
    let ssh_handle = Arc::new(SshHandle {
        handle: keep.clone(),
        target: format!("{}@{}:{}", user, host, port),
        x11_screen_id: x11_screen_id.clone(),
    });
    let keep_for_task = keep.clone();

    // 启用 X11 转发：把服务端反向打开的 X11 通道接进**本会话独享**的 X server。
    // 每条通道的 X11 协议字节流直接灌进 `serve_stream`，不需要 TCP 端口。
    if let (Some(mut x11_rx), Some(screen_id)) = (x11_rx, x11_screen_id.clone()) {
        tokio::spawn(async move {
            let server = match crate::remote::x11::ensure_screen(&screen_id, 1280, 800).await {
                Ok(server) => server,
                Err(e) => {
                    eprintln!("[ssh] 启动内置 X server 失败，X11 转发不可用：{e}");
                    crate::xsrv::log_line(format_args!(
                        "[x11-fwd] 启动内置 X server 失败（端口被旧实例占用？）：{e}"
                    ));
                    return;
                }
            };
            eprintln!(
                "[ssh] X11 转发就绪（屏幕 {screen_id}，display :{}）：远端 X 程序将显示在内置 X server",
                server.display
            );
            while let Some(channel) = x11_rx.recv().await {
                crate::xsrv::log_line(format_args!(
                    "[x11-fwd] 收到 sshd 反向 X11 通道，接入屏幕 {screen_id}"
                ));
                let index = server.next_client_index();
                server.serve_channel(channel.into_stream(), index);
            }
            // 会话结束（x11_rx 关闭）：回收本会话的屏幕（display 编号一并释放）
            crate::remote::x11::close_screen(&screen_id);
        });
    }

    tokio::spawn(async move {
        let handle = keep_for_task.clone(); // 心跳用它（Handle 是 Arc，发包不占通道）
        let _keep_alive = (keep_for_task, keep_jump);

        // 心跳：SSH 服务器 / NAT / 墙上的空闲连接常常几分钟就被回收。
        // 每秒醒一次读当前配置（用户可能刚在配置面板改过间隔），
        // 攒够 `keep_alive_interval` 秒**且这段时间真的没数据**才发一个
        // `keepalive@openssh.com`（want_reply=true 顺带探活）。
        // 0 = 关闭心跳。
        let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(1));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut idle_secs: u64 = 0;

        loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    let every = crate::config::keep_alive_interval();
                    if every == 0 {
                        idle_secs = 0;
                        continue;
                    }
                    idle_secs += 1;
                    if idle_secs >= every {
                        idle_secs = 0;
                        // 发不出去说明连接已经死了：让 channel.wait() 那边收尾，
                        // 这里不 break（否则收不到「SSH 连接已断开」的提示）
                        let _ = handle.send_keepalive(true).await;
                    }
                }
                msg = channel.wait() => {
                    idle_secs = 0;
                    match msg {
                        Some(ChannelMsg::Data { ref data }) => {
                            if output_tx.send(data.to_vec()).is_err() {
                                break;
                            }
                        }
                        Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                            let _ = output_tx.send(
                                "\r\n\x1b[90m[SSH 连接已断开]\x1b[0m".as_bytes().to_vec(),
                            );
                            break;
                        }
                        _ => {}
                    }
                }
                Some(data) = input_rx.recv() => {
                    idle_secs = 0;
                    let _ = channel.data(&data[..]).await;
                }
                Some((c, r)) = resize_rx.recv() => {
                    let _ = channel.window_change(c as u32, r as u32, 0, 0).await;
                }
            }
        }
    });

    Ok(TermIo {
        input_tx,
        resize_tx,
        output_rx,
        ssh: Some(ssh_handle),
    })
}
