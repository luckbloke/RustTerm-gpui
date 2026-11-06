//! 远程桌面后端：VNC / RDP / SPICE 统一接口。
//!
//! 三种协议各有自己的线程内运行时，统一向上层暴露：
//! - 下行：`RdCommand`（键盘 / 指针 / 滚轮 / 关闭）
//! - 上行：`RdEvent`（状态 / 分辨率 / 脏矩形 RGBA 帧 / 错误）
//!
//! 帧以 **RGBA8 + 脏矩形** 传递，UI 侧只需 `TextureHandle::set_partial`，
//! 不必整帧重传（各协议内部维护完整帧缓冲，脏区合并后节流推送）。

pub mod rdp;
pub mod rdp_clipboard;
pub mod spice;
pub mod vnc;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RdProtocol {
    Vnc,
    Rdp,
    Spice,
}

#[derive(Clone, Debug)]
pub struct RdParams {
    pub protocol: RdProtocol,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub domain: String,
    /// 初始桌面尺寸（RDP 会按此协商）
    pub width: u16,
    pub height: u16,
    /// 16 / 24 / 32
    pub color_depth: u8,
    /// 是否开启剪贴板同步。
    ///
    /// - RDP：协商 CLIPRDR 通道，文本双向同步；**没配共享目录时**文件也走剪贴板
    ///   （配了共享目录则文件只走 RDPDR 盘符，剪贴板只传文本）；
    /// - VNC：`VncEvent::Text` 上报远端剪贴板、`X11Event::CopyText` 下发本机剪贴板；
    /// - SPICE：后端尚未实现剪贴板通道，此开关暂无效果。
    ///
    /// 通道是建连时协商的：会话中途切换开关只对**下次连接**生效。
    pub clipboard: bool,
    /// 共享给远端的本机目录（RDP 专用）。
    ///
    /// RDP 走 RDPDR 通道把本地目录挂成远端的一个盘符，用户可以像操作
    /// 本地磁盘一样往远端拷文件；不需要在远端安装任何服务。
    /// VNC 与 SPICE 没有标准的文件通道（RFB 只有剪贴板），这里为 `None`。
    pub share_dir: Option<String>,
    /// 旧版服务器兼容模式（Windows Server 2008 / 2008 R2 等）。
    ///
    /// 这些服务器的 CredSSP/NLA 实现与客户端 sspi 存在已知兼容问题：
    /// 协商阶段会收到 TLS alert 80，或服务端无法解析 TSRequest
    ///（`invalid sequence tag identifier`）。开启后直接跳过 NLA，
    /// 只用 TLS 安全层；凭据改由 Client Info PDU 以自动登录方式发送。
    ///
    /// 不开启时也会在 NLA 握手失败后自动回退一次，只是不记住这次选择。
    pub legacy_compat: bool,
}

impl Default for RdParams {
    fn default() -> Self {
        Self {
            protocol: RdProtocol::Vnc,
            host: String::new(),
            port: 5900,
            username: String::new(),
            password: String::new(),
            domain: String::new(),
            width: 1280,
            height: 800,
            color_depth: 32,
            clipboard: true,
            share_dir: None,
            legacy_compat: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RdState {
    Connecting,
    Authenticating,
    Active,
    Disconnected,
    Failed(String),
}

/// 剪贴板文件传输的进度（仅 RDP，走 CLIPRDR 的文件通道）。
///
/// mstsc 的「本机复制文件 → 远端粘贴」走的是延迟渲染：先是文件列表
/// （`FileGroupDescriptorW`），再按 `FileContentsRequest` 一块块拉内容。
/// 后端把进度折算成这个结构，UI 只负责显示。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferUpdate {
    /// 正在传输：已传 / 总字节（`total == 0` 表示远端没告知大小）
    Progress { sent: u64, total: u64 },
    /// 传输完成，附带一句给用户看的提示
    Done(String),
    /// 传输失败 / 被远端取消
    Failed(String),
}

/// 后端 → UI
#[derive(Debug)]
pub enum RdEvent {
    State(RdState),
    /// 桌面尺寸（首帧或分辨率变化）
    Resized { width: u32, height: u32 },
    /// 脏矩形（RGBA8，行优先紧密排列）
    Frame {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        rgba: Vec<u8>,
    },
    /// 远端光标位图（RGBA8，行优先紧密排列）+ 热点偏移。
    ///
    /// RDP 的 `PointerBitmap` / VNC 的 `SetCursor` 伪编码都会下发光标形状。
    /// 客户端必须自己把它画到画面上，**同时隐藏本地系统光标**，
    /// 否则屏幕上会同时出现本地箭头和远端光标（俗称「两个鼠标」）。
    Cursor {
        width: u16,
        height: u16,
        hotspot_x: u16,
        hotspot_y: u16,
        rgba: Vec<u8>,
    },
    /// 远端光标位置（相对远端桌面像素）。
    ///
    /// 远端只在光标移动时下发位置；客户端要据此把光标位图画在正确位置。
    CursorPos { x: u16, y: u16 },
    /// 远端要求隐藏光标（例如鼠标移出远端窗口、或进入全屏视频）。
    CursorHidden,
    /// 远端剪贴板文本（VNC `ServerCutText`；RDP 走后端原生的系统剪贴板，
    /// 不经过这条事件）。
    Clipboard(String),
    /// 非致命的连接提示（例如 NLA 握手失败后自动改用 TLS 兼容模式）。
    ///
    /// 与 [`RdEvent::Error`] 不同：收到它仍然继续连接，UI 只把它显示在
    /// 连接中的状态行里，不进入失败状态。
    Notice(String),
    /// 剪贴板文件传输状态（仅 RDP）。
    Transfer(TransferUpdate),
    Error(String),
}

/// 归一化键盘输入。
#[derive(Clone, Copy, Debug)]
pub struct KeyInput {
    /// PC/AT set-1 扫描码，扩展键为 0xE000 | code
    pub scancode: u16,
    /// X11 keysym（VNC 用）
    pub keysym: u32,
    /// 可打印字符（RDP 走 Unicode 通道）
    pub ch: Option<char>,
    pub down: bool,
}

/// UI → 后端
#[derive(Debug)]
pub enum RdCommand {
    Key(KeyInput),
    /// 绝对指针位置 + 按键位掩码（1=左 2=中 4=右）
    Pointer { x: u16, y: u16, buttons: u8 },
    Wheel { dy: i16 },
    /// 本机剪贴板文本 → 远端（VNC `ClientCutText`；RDP 由后端原生通道自行处理）。
    Clipboard(String),
    Close,
}

pub struct RdSession {
    tx: Sender<RdCommand>,
    pub events: Receiver<RdEvent>,
    cancel: Arc<AtomicBool>,
}

impl RdSession {
    pub fn send(&self, cmd: RdCommand) {
        if self.cancel.load(Ordering::Relaxed) && !matches!(cmd, RdCommand::Close) {
            return;
        }
        let _ = self.tx.send(cmd);
    }

    pub fn close(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        let _ = self.tx.send(RdCommand::Close);
    }
}

impl Drop for RdSession {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        let _ = self.tx.send(RdCommand::Close);
    }
}

/// 把后端原始错误翻译成「用户能照着做」的提示。
///
/// 远程桌面最常见的失败其实都不是协议问题，而是**没填凭据**：
/// VNC 没填密码 → 服务器拒绝认证；RDP 没填用户名 → CredSSP 报
/// "Got empty identity"。原始报错对普通用户毫无意义，这里补一句怎么办。
fn friendly_error(protocol: RdProtocol, msg: &str) -> String {
    let lower = msg.to_lowercase();
    let hint = if protocol == RdProtocol::Rdp
        && (lower.contains("rdpdr") || lower.contains("drive redirection") || lower.contains("redirected"))
    {
        Some(
            "RDP 驱动器重定向失败：请确认远端组策略没有禁用「驱动器重定向」\
             （计算机配置 → 管理模板 → Windows 组件 → 远程桌面服务 → \
             远程桌面会话主机 → 设备和资源重定向），并确认共享目录存在且当前用户可读",
        )
    } else if lower.contains("invalid sequence tag")
        || lower.contains("tsrequest")
        || lower.contains("tls alert")
    {
        // Windows Server 2008 / 2008 R2 的 CredSSP 与客户端 sspi 有已知兼容问题
        Some(
            "服务器可能是 Windows Server 2008 / 2008 R2：NLA 握手不兼容，\
             请在连接表单勾选「旧版服务器兼容（禁用 NLA）」后重试",
        )
    } else if lower.contains("authentication failed") || lower.contains("wrongpassword") {
        match protocol {
            RdProtocol::Vnc => Some("VNC 密码错误或未填写：在远程桌面连接表单的「密码」栏填写（也可右键会话 → 编辑… 填好后重连）"),
            RdProtocol::Spice => Some("SPICE 密码错误或未填写：在远程桌面连接表单的「密码」栏填写（也可右键会话 → 编辑… 填好后重连）"),
            RdProtocol::Rdp => Some("RDP 认证失败：请确认用户名与密码都正确（也可右键会话 → 编辑… 填好后重连）"),
        }
    } else if lower.contains("empty identity") || lower.contains("credssp") {
        Some("RDP 必须填写用户名：在远程桌面连接表单的「用户」栏填用户名、「密码」栏填密码（也可右键会话 → 编辑… 填好后重连）")
    } else if lower.contains("10061") || lower.contains("actively refused") {
        Some("目标端口没有服务在监听：请确认地址/端口与服务是否已启动")
    } else if lower.contains("超时") || lower.contains("timed out") {
        Some("连接超时：请检查网络与防火墙")
    } else if lower.contains("unsupported vnc security type") {
        Some("服务器要求的认证方式不支持：请确认是否需要密码")
    } else if lower.contains("invalid framebuffer or rectangle dimensions") {
        // 旧版 vnc-rs 会把服务器下发的 0×0 光标伪编码误判成非法尺寸，
        // 已修复；这里仍留一条兜底提示，便于遇到同类报文时定位。
        Some("服务器下发的画面矩形尺寸异常：请确认远端桌面分辨率设置")
    } else {
        None
    };
    match hint {
        Some(h) => format!("{msg}\n提示：{h}"),
        None => msg.to_string(),
    }
}

/// 启动一个远程桌面会话（每次调用起一个独立线程 + 单线程运行时）。
pub fn spawn(params: RdParams) -> RdSession {
    let (cmd_tx, cmd_rx) = channel::<RdCommand>();
    let (ev_tx, ev_rx) = channel::<RdEvent>();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_thread = cancel.clone();

    let proto = params.protocol;
    // 线程创建失败时也要能把错误送回 UI，所以留一个克隆
    let ev_tx_boot = ev_tx.clone();
    let spawn_res = std::thread::Builder::new()
        .name(format!("remote-{:?}", proto))
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ev_tx.send(RdEvent::Error(format!("创建运行时失败: {e}")));
                    let _ = ev_tx.send(RdEvent::State(RdState::Disconnected));
                    return;
                }
            };
            // 协议线程里任何 panic 都不能让 UI 永远停在「已连接」：
            // 用 catch_unwind 兜住，统一转成 Error 事件。
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                rt.block_on(async {
                    match params.protocol {
                        RdProtocol::Vnc => {
                            vnc::run(&params, &ev_tx, &cmd_rx, &cancel_thread).await
                        }
                        RdProtocol::Rdp => {
                            rdp::run(&params, &ev_tx, &cmd_rx, &cancel_thread).await
                        }
                        RdProtocol::Spice => {
                            spice::run(&params, &ev_tx, &cmd_rx, &cancel_thread).await
                        }
                    }
                })
            }));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    let msg = friendly_error(proto, &format!("{e:#}"));
                    eprintln!("[remote] {proto:?} 连接失败: {msg}");
                    let _ = ev_tx.send(RdEvent::Error(msg));
                }
                Err(_) => {
                    // panic 已被兜住，转成可读错误（不要静默）
                    let msg = format!("{proto:?} 后端线程异常终止（panic）");
                    eprintln!("[remote] {msg}");
                    let _ = ev_tx.send(RdEvent::Error(msg));
                }
            }
            let _ = ev_tx.send(RdEvent::State(RdState::Disconnected));
        });

    // 线程都没起来也要让 UI 收到失败，否则永远显示「连接中」
    if let Err(e) = spawn_res {
        let _ = ev_tx_boot.send(RdEvent::Error(format!("启动远程桌面线程失败: {e}")));
        let _ = ev_tx_boot.send(RdEvent::State(RdState::Disconnected));
    }

    RdSession {
        tx: cmd_tx,
        events: ev_rx,
        cancel,
    }
}

// ---------------------------------------------------------------------------
// 键盘映射
// ---------------------------------------------------------------------------

/// gpui 键名 → (扫描码, X11 keysym)。
pub fn map_key(key: &str) -> Option<(u16, u32)> {
    // 常用按键表（PC/AT set-1 扫描码 + X11 keysym）
    let v = match key {
        "escape" => (0x01, 0xff1b),
        "1" => (0x02, 0x31),
        "2" => (0x03, 0x32),
        "3" => (0x04, 0x33),
        "4" => (0x05, 0x34),
        "5" => (0x06, 0x35),
        "6" => (0x07, 0x36),
        "7" => (0x08, 0x37),
        "8" => (0x09, 0x38),
        "9" => (0x0a, 0x39),
        "0" => (0x0b, 0x30),
        "-" => (0x0c, 0x2d),
        "=" => (0x0d, 0x3d),
        "backspace" => (0x0e, 0xff08),
        "tab" => (0x0f, 0xff09),
        "q" => (0x10, 0x71),
        "w" => (0x11, 0x77),
        "e" => (0x12, 0x65),
        "r" => (0x13, 0x72),
        "t" => (0x14, 0x74),
        "y" => (0x15, 0x79),
        "u" => (0x16, 0x75),
        "i" => (0x17, 0x69),
        "o" => (0x18, 0x6f),
        "p" => (0x19, 0x70),
        "[" => (0x1a, 0x5b),
        "]" => (0x1b, 0x5d),
        "enter" => (0x1c, 0xff0d),
        "a" => (0x1e, 0x61),
        "s" => (0x1f, 0x73),
        "d" => (0x20, 0x64),
        "f" => (0x21, 0x66),
        "g" => (0x22, 0x67),
        "h" => (0x23, 0x68),
        "j" => (0x24, 0x6a),
        "k" => (0x25, 0x6b),
        "l" => (0x26, 0x6c),
        ";" => (0x27, 0x3b),
        "'" => (0x28, 0x27),
        "`" => (0x29, 0x60),
        "\\" => (0x2b, 0x5c),
        "z" => (0x2c, 0x7a),
        "x" => (0x2d, 0x78),
        "c" => (0x2e, 0x63),
        "v" => (0x2f, 0x76),
        "b" => (0x30, 0x62),
        "n" => (0x31, 0x6e),
        "m" => (0x32, 0x6d),
        "," => (0x33, 0x2c),
        "." => (0x34, 0x2e),
        "/" => (0x35, 0x2f),
        "space" => (0x39, 0x20),
        "f1" => (0x3b, 0xffbe),
        "f2" => (0x3c, 0xffbf),
        "f3" => (0x3d, 0xffc0),
        "f4" => (0x3e, 0xffc1),
        "f5" => (0x3f, 0xffc2),
        "f6" => (0x40, 0xffc3),
        "f7" => (0x41, 0xffc4),
        "f8" => (0x42, 0xffc5),
        "f9" => (0x43, 0xffc6),
        "f10" => (0x44, 0xffc7),
        "f11" => (0x57, 0xffc8),
        "f12" => (0x58, 0xffc9),
        "home" => (0xE047, 0xff50),
        "up" => (0xE048, 0xff52),
        "pageup" => (0xE049, 0xff55),
        "left" => (0xE04b, 0xff51),
        "right" => (0xE04d, 0xff53),
        "end" => (0xE04f, 0xff57),
        "down" => (0xE050, 0xff54),
        "pagedown" => (0xE051, 0xff56),
        "insert" => (0xE052, 0xff63),
        "delete" => (0xE053, 0xffff),
        _ => return None,
    };
    Some(v)
}

/// 修饰键：左 Shift / 左 Ctrl / 左 Alt。
pub const MOD_SHIFT: KeyInput = KeyInput {
    scancode: 0x2a,
    keysym: 0xffe1,
    ch: None,
    down: false,
};
pub const MOD_CTRL: KeyInput = KeyInput {
    scancode: 0x1d,
    keysym: 0xffe3,
    ch: None,
    down: false,
};
pub const MOD_ALT: KeyInput = KeyInput {
    scancode: 0x38,
    keysym: 0xffe9,
    ch: None,
    down: false,
};

/// 计算可打印键在 US 键盘布局下的真实字符（考虑 Shift 按下）→ X11 keysym。
///
/// 背景：`map_key` 给的 keysym 恒为「未按 Shift」的基准字符（如 't' = 0x74）。
/// VNC 远端以 keysym 为准来决定敲出的字符：
/// - 上档字符（`T` / `!` / `@` …）必须改用上档 keysym（'T' = 0x54、'@' = 0x40），
///   否则大写字母输成小写、符号直接丢失；
/// - 数据链路上伴随补发的 Shift 按下/抬起（`apply_mods`）对 RDP（扫描码路径）
///   仍然有效；VNC 只看 keysym，补发 Shift 属冗余但无害。
///
/// 返回 `None` 表示该键不是可打印字符（功能键/方向键等沿用 `map_key` 的 keysym）。
pub fn printable_keysym(key: &str, shift: bool) -> Option<u32> {
    // 字母键：Shift → 大写
    let lower: char = match key {
        "q" => 'q', "w" => 'w', "e" => 'e', "r" => 'r', "t" => 't',
        "y" => 'y', "u" => 'u', "i" => 'i', "o" => 'o', "p" => 'p',
        "a" => 'a', "s" => 's', "d" => 'd', "f" => 'f', "g" => 'g',
        "h" => 'h', "j" => 'j', "k" => 'k', "l" => 'l',
        "z" => 'z', "x" => 'x', "c" => 'c', "v" => 'v', "b" => 'b',
        "n" => 'n', "m" => 'm',
        _ => {
            // 数字行 / 符号键：US 布局的（基准字符, Shift 上档字符）
            let (base, shifted) = match key {
                "1" => ('1', '!'),  "2" => ('2', '@'),   "3" => ('3', '#'),
                "4" => ('4', '$'),  "5" => ('5', '%'),   "6" => ('6', '^'),
                "7" => ('7', '&'),  "8" => ('8', '*'),   "9" => ('9', '('),
                "0" => ('0', ')'),
                "-" => ('-', '_'),
                "=" => ('=', '+'),
                "," => (',', '<'),
                "." => ('.', '>'),
                "/" => ('/', '?'),
                ";" => (';', ':'),
                "'" => ('\'', '"'),
                "`" => ('`', '~'),
                "[" => ('[', '{'),
                "]" => (']', '}'),
                "\\" => ('\\', '|'),
                "space" => (' ', ' '),
                _ => return None,
            };
            let ch = if shift { shifted } else { base };
            return Some(ch as u32);
        }
    };
    Some(if shift {
        lower.to_ascii_uppercase() as u32
    } else {
        lower as u32
    })
}

// ---------------------------------------------------------------------------
// 后端冒烟测试
//
// 「远程桌面连不上」时最忌讳的是：协议线程 panic 把整个进程带走，
// 或者 UI 永远停在「连接中」。这里对三个协议各跑一次
// 「连一个没人监听的端口」，要求：必须收到 Error 事件、必须收到
// Disconnected、进程不能挂。
// ---------------------------------------------------------------------------
