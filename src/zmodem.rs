//! ZMODEM（rz/sz）文件收发：触发检测 + `zmodem2` 状态机驱动 + 落盘/取文件。
//!
//! 为什么需要这一层：
//! - `zmodem2` 是 **caller-driven 的纯状态机**（无 I/O、全同步、no_std），
//!   正好嵌进 `Term::pump()` 的每帧同步循环，终端 IO 一行都不用改成 async；
//!   但它只提供 `poll()`/`submit_wire()` 这类原语，"什么时候进入传输模式、
//!   状态机要写的字节发去哪、收到的文件写哪、用户取消怎么办"全要调用方写，
//!   那就是这个模块的全部内容。
//! - 进入传输模式的判断点只有一个：远端字节流里出现 ZMODEM 头
//!   （`**\x18B` + 2 位十六进制帧类型）。这个判断必须保守——误判会把正常
//!   输出吞进状态机，屏幕直接花掉且无法恢复；所以只在"完整头 + 合法十六进制
//!   + 是开场帧（ZRQINIT/ZRINIT）"时命中，并且最多只扣住 5 字节等跨 chunk 补全。
//!
//! 角色对照（谁先说话）：
//! - 远端 `sz file`：先发 ZRQINIT(00) 等接收方 → 我们当 **Receiver**（收文件）
//! - 远端 `rz`：先发 ZRINIT(01) 等发送方 → 我们当 **Sender**（发文件）
//!
//! 驱动循环（`Session::drive`）严格照 crate 文档的顺序：
//! `WriteWire → 写传输层 + wire_written()`；`ReadFile → submit_file()`；
//! `WriteFile → 落盘 + file_written()`；`Event → 进度/结束`；
//! `Idle → submit_wire()` 吃新字节，没有新字节就退出等下一帧。

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use zmodem2::{Action, Event, FileInfo, Position, Receiver, Sender};

// ---------------------------------------------------------------------------
// 触发检测
// ---------------------------------------------------------------------------

/// ZPAD：ZMODEM 十六进制头的填充字节。
const ZPAD: u8 = b'*';
/// ZDLE：转义/帧头引导字节（同时是 CAN，中止序列也用它）。
const ZDLE: u8 = 0x18;
/// ZHEX：十六进制头编码标识（`**\x18B` 里的 B）。
const ZHEX: u8 = b'B';
/// 十六进制头固定前缀（4 字节）。
const HEAD_PREFIX: [u8; 4] = [ZPAD, ZPAD, ZDLE, ZHEX];
/// 完整头长度：前缀 + 2 位十六进制帧类型。
const HEADER_LEN: usize = 6;
/// ZRQINIT：远端 `sz` 在等接收方。
const FRAME_ZRQINIT: u8 = 0x00;
/// ZRINIT：远端 `rz` 在等发送方。
const FRAME_ZRINIT: u8 = 0x01;
/// 被扣住的半截头最多等多久：普通终端输出末尾恰好是 `*` / `**` 时，
/// 不能把它无限期扣在检测器里，否则提示符的最后几个字符不显示。
const HOLD_MAX_MS: u64 = 120;

/// 检测到的开场帧 → 我们要扮演的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// ZRQINIT：远端在发文件，我们收。
    Receive,
    /// ZRINIT：远端在收文件，我们发。
    Send,
}

/// 一批远端字节的扫描结果。
pub enum Scan<'a> {
    /// 没命中：`bytes` 照常喂 vte（可能只是输入本身，也可能是扣住的尾巴 + 输入）。
    Miss { bytes: Cow<'a, [u8]> },
    /// 命中：`before` 喂 vte，`head`（从 ZMODEM 头起）整段交给状态机。
    Hit {
        trigger: Trigger,
        before: Cow<'a, [u8]>,
        head: Vec<u8>,
    },
}

/// 跨 chunk 的 ZMODEM 头检测器。
///
/// 只保留"结尾正好是半个头"的那几个字节，而不是固定保留 N 字节尾巴：
/// 固定尾巴会在每个输出批次末尾扣下几个字符，shell 提示符（无换行结尾）
/// 就会被显示成缺尾巴的样子。这里只有在真的可能是头前缀时才扣。
pub struct Detector {
    tail: Vec<u8>,
    held_at: Instant,
}

impl Default for Detector {
    fn default() -> Self {
        Self::new()
    }
}

impl Detector {
    pub fn new() -> Self {
        Self {
            tail: Vec::new(),
            held_at: Instant::now(),
        }
    }

    /// 是否扣着半截头（等下一批字节）。
    pub fn has_pending(&self) -> bool {
        !self.tail.is_empty()
    }

    /// 扫描一批远端字节。命中时返回会话需要的全部字节。
    pub fn scan<'a>(&mut self, data: &'a [u8]) -> Scan<'a> {
        if data.is_empty() {
            return Scan::Miss {
                bytes: Cow::Borrowed(data),
            };
        }
        // 扣住的尾巴必须和本批拼起来再找，否则会被 chunk 边界切断
        let combined: Cow<'a, [u8]> = if self.tail.is_empty() {
            Cow::Borrowed(data)
        } else {
            let mut v = std::mem::take(&mut self.tail);
            v.extend_from_slice(data);
            Cow::Owned(v)
        };

        if let Some(i) = find_header(&combined) {
            let hi = hex_val(combined[i + 4]).unwrap_or(0);
            let lo = hex_val(combined[i + 5]).unwrap_or(0);
            let frame = (hi << 4) | lo;
            let trigger = match frame {
                FRAME_ZRQINIT => Some(Trigger::Receive),
                FRAME_ZRINIT => Some(Trigger::Send),
                _ => None,
            };
            if let Some(trigger) = trigger {
                let before = combined[..i].to_vec();
                let head = combined[i..].to_vec();
                self.tail.clear();
                return Scan::Hit {
                    trigger,
                    before: Cow::Owned(before),
                    head,
                };
            }
            // 完整头但不是开场帧（例如半路出现的 ZFILE）：不在会话里就没有
            // 意义，按普通输出放行，绝不吞字节。
        }

        let hold = partial_prefix_len(&combined);
        if hold > 0 {
            let n = combined.len() - hold;
            self.tail = combined[n..].to_vec();
            self.held_at = Instant::now();
            let bytes = match combined {
                Cow::Borrowed(b) => Cow::Borrowed(&b[..n]),
                Cow::Owned(mut v) => {
                    v.truncate(n);
                    Cow::Owned(v)
                }
            };
            Scan::Miss { bytes }
        } else {
            self.tail.clear();
            Scan::Miss { bytes: combined }
        }
    }

    /// 扣住的半截头超时未补全 → 还回去（当普通输出显示）。
    ///
    /// 由每帧的 `pump` 调用：输入停顿时也得让屏幕上的字符出全。
    pub fn flush_stale(&mut self) -> Option<Vec<u8>> {
        if self.tail.is_empty() || self.held_at.elapsed() < Duration::from_millis(HOLD_MAX_MS) {
            return None;
        }
        Some(std::mem::take(&mut self.tail))
    }

    /// 测试用：把"扣住时刻"往前拨，免得单测真的 sleep。
    #[cfg(test)]
    fn force_stale(&mut self) {
        self.held_at = Instant::now() - Duration::from_millis(HOLD_MAX_MS + 10);
    }
}

/// 找最早的完整 ZMODEM 十六进制头，返回起始下标。
fn find_header(buf: &[u8]) -> Option<usize> {
    if buf.len() < HEADER_LEN {
        return None;
    }
    (0..=buf.len() - HEADER_LEN).find(|&i| {
        buf[i..i + 4] == HEAD_PREFIX
            && hex_val(buf[i + 4]).is_some()
            && hex_val(buf[i + 5]).is_some()
    })
}

/// 结尾有多少字节是"还没长齐的头"，需要扣到下一批（最多 5 字节）。
fn partial_prefix_len(buf: &[u8]) -> usize {
    // 已经收齐 `**\x18B` + 1 位十六进制，还差最后一位
    if buf.len() >= 5
        && buf[buf.len() - 5..buf.len() - 1] == HEAD_PREFIX
        && hex_val(buf[buf.len() - 1]).is_some()
    {
        return 5;
    }
    // 结尾正好是 `*` / `**` / `**\x18` / `**\x18B`
    for k in (1..=HEAD_PREFIX.len()).rev() {
        if buf.ends_with(&HEAD_PREFIX[..k]) {
            return k;
        }
    }
    0
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 文件名消毒与落盘路径
// ---------------------------------------------------------------------------

/// 消毒后的文件名长度上限（字节）。远端可以发任意长度的名字，
/// 超长名在 Windows 上会直接创建失败，截断比失败好。
pub const MAX_NAME_LEN: usize = 180;

/// 远端文件名 → 本地安全文件名。
///
/// 文件名是**远端说了算的不可信输入**：可能是绝对路径、`../../` 穿越、
/// 带 NUL/控制字符、或者干脆是空串。这里只取 basename，并把危险字符换掉；
/// 最终落盘前还会用 `create_new` 保证不覆盖已有文件（见 `open_dest`）。
pub fn sanitize_name(raw: &[u8]) -> String {
    // 远端文件名不保证是合法 UTF-8（老 Unix 上就是任意字节），
    // 用替换字符兜底，别让整个文件因为编码问题收不下来。
    let s = String::from_utf8_lossy(raw);
    // 只取 basename：Unix 用 '/'，但 Windows 侧的 sz 也可能发 '\\'，
    // 两种都当分隔符切，才能同时挡掉 `../../etc/passwd` 与 `C:\x\y`。
    let base = s.rsplit(|c| c == '/' || c == '\\').next().unwrap_or("");
    // 控制字符（含 NUL）与 Windows 不允许的字符一律替换：
    // 远端发 `a\x18b` 这种名字不该让我们创建失败或写出奇怪的文件。
    let mut name: String = base
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    // Windows 会静默丢掉结尾的点和空格 → 名字会和我们记录的不一致，自己先去掉
    while name.ends_with(|c| c == '.' || c == ' ') {
        name.pop();
    }
    if name.is_empty() || name == "." || name == ".." {
        name = "zmodem_received".to_string();
    }
    if is_reserved_name(&name) {
        name.insert(0, '_');
    }
    truncate_name(&name, MAX_NAME_LEN)
}

/// Windows 设备保留名（CON/NUL/COM1…）：当文件名会打开设备而不是文件。
fn is_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    // COM1..COM9 / LPT1..LPT9
    let bytes = stem.as_bytes();
    bytes.len() == 4
        && (stem.starts_with("COM") || stem.starts_with("LPT"))
        && bytes[3].is_ascii_digit()
        && bytes[3] != b'0'
}

/// 按字节截断但保留扩展名：超长名的扩展名（.tar.gz 之类）决定文件类型，
/// 砍掉它比砍掉中间的字符更糟。
fn truncate_name(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i <= 16 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let keep = max.saturating_sub(ext.len());
    let mut cut = keep.min(stem.len());
    while cut > 0 && !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{}", &stem[..cut], ext)
}

/// 不覆盖已有文件：同名时依次尝试 `name.1` / `name.2` …
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = split_ext(name);
    for n in 1..=9999u32 {
        let cand = dir.join(format!("{stem}.{n}{ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    // 极端情况（同名文件上千个）：时间戳兜底，仍然不覆盖
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    dir.join(format!("{stem}.{ts}{ext}"))
}

fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// 接收目录：配置覆盖 → 系统下载目录 → `~/Downloads` → 家目录 → 临时目录。
pub fn download_dir() -> PathBuf {
    if let Some(d) = crate::config::zmodem_download_dir() {
        return d;
    }
    if let Some(d) = dirs::download_dir() {
        return d;
    }
    if let Some(home) = dirs::home_dir() {
        let d = home.join("Downloads");
        return if d.is_dir() { d } else { home };
    }
    std::env::temp_dir()
}

/// 字节数的短显示（进度条/提示用）。
pub fn fmt_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if f < K {
        format!("{n} B")
    } else if f < K * K {
        format!("{:.1} KB", f / K)
    } else if f < K * K * K {
        format!("{:.1} MB", f / (K * K))
    } else {
        format!("{:.2} GB", f / (K * K * K))
    }
}

// ---------------------------------------------------------------------------
// 会话结束后的协议尾巴过滤
// ---------------------------------------------------------------------------

/// 会话结束后残留的协议字节过滤器。
///
/// 为什么需要它：`zmodem2` 的接收端一旦进入 SessionEnd 就不再消费字节，而远端
/// `sz` 退出前还会发"最后一个应答头 + over-and-out 的 `OO`"。这些是协议字节，
/// 直接喂给 vte 会在终端里显示成 `**\x18B0b…OO` 这样的乱码。
///
/// 策略（保守优先，宁可按原样显示也不吞正常输出）：
/// - 只认两种尾巴：完整的十六进制头（`**\x18B` + 十六进制 + CR/LF + XON 尾）
///   和 `OO`；CAN(0x18)/退格(0x08) 也当协议字节；
/// - 认不出来的字节立刻整体放行并结束过滤——shell 提示符不会被误吃；
/// - 判不出结果时先扣住（跨 chunk 的头），窗口结束（`finish`）再还回去，
///   最坏情况只是延迟几百毫秒显示。
#[derive(Default)]
pub struct TailFilter {
    pending: Vec<u8>,
    active: bool,
}

impl TailFilter {
    /// 会话结束：开始过滤，返回可以立刻显示的部分（`leftover` 里的正常输出）。
    pub fn start(&mut self, leftover: &[u8]) -> Vec<u8> {
        self.pending.clear();
        self.active = true;
        self.pending.extend_from_slice(leftover);
        self.drain()
    }

    /// 过滤窗口内的新字节。
    pub fn push(&mut self, data: &[u8]) -> Vec<u8> {
        if !self.active {
            return data.to_vec();
        }
        self.pending.extend_from_slice(data);
        self.drain()
    }

    /// 窗口结束：把还扣着的字节还回去（宁可显示，也不要静默丢输出）。
    pub fn finish(&mut self) -> Vec<u8> {
        self.active = false;
        std::mem::take(&mut self.pending)
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// 尽量把 pending 里的协议尾巴剥掉；遇到正常输出就整体放行并停用过滤。
    fn drain(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            if self.pending.is_empty() {
                break;
            }
            if self.pending.starts_with(b"OO") {
                self.pending.drain(..2);
                continue;
            }
            if let Some(n) = complete_hex_header_len(&self.pending) {
                self.pending.drain(..n);
                continue;
            }
            if hex_header_prefix_len(&self.pending) > 0 || self.pending == b"O" {
                // 还可能是被 chunk 切断的协议尾巴：先扣着等后续字节
                break;
            }
            // 认不出来 = 正常输出：整段放行，过滤结束
            out.append(&mut self.pending);
            self.active = false;
            break;
        }
        out
    }
}

/// `bytes` 开头是否是完整的 ZMODEM 十六进制头；是则返回它（含 CR/LF 与 XON 尾）的长度。
fn complete_hex_header_len(bytes: &[u8]) -> Option<usize> {
    let prefix = hex_header_prefix_len(bytes);
    if prefix < HEADER_LEN {
        return None;
    }
    // 十六进制头以 CR/LF 结束，后面还可能跟 XON/XOFF
    let mut i = HEADER_LEN;
    while i < bytes.len() && !matches!(bytes[i], 0x0d | 0x0a) {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    while i < bytes.len() && matches!(bytes[i], 0x0d | 0x0a | 0x11 | 0x13 | 0x91 | 0x93) {
        i += 1;
    }
    Some(i)
}

/// `bytes` 开头有多少字节是合法的头前缀（0 = 不是；`HEADER_LEN` = 已经是一个完整头）。
fn hex_header_prefix_len(bytes: &[u8]) -> usize {
    let n = bytes.len().min(HEADER_LEN);
    if n < HEAD_PREFIX.len() {
        return usize::from(bytes[..n] == HEAD_PREFIX[..n]) * n;
    }
    if bytes[..HEAD_PREFIX.len()] != HEAD_PREFIX {
        return 0;
    }
    if n == HEAD_PREFIX.len() {
        return n; // 完整前缀（4 字节），还差 2 位十六进制
    }
    let hexes = (HEAD_PREFIX.len()..n)
        .take_while(|&i| hex_val(bytes[i]).is_some())
        .count();
    HEAD_PREFIX.len() + hexes
}

// ---------------------------------------------------------------------------
// 会话
// ---------------------------------------------------------------------------

/// 传输角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// 收文件（远端 `sz`）。
    Receive,
    /// 发文件（远端 `rz`）。
    Send,
}

/// 会话结束原因。
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// 正常结束：收/发过的文件列表。
    Ok { role: Role, files: Vec<PathBuf> },
    /// 本地取消（Ctrl+C / 取消选择 / 主动中止）。
    Cancelled,
    /// 协议或磁盘 IO 失败。
    Failed(String),
}

/// 一帧的传输进度（UI 用）。
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub role: Role,
    pub name: String,
    pub done: u64,
    pub total: Option<u64>,
    /// 批量传输时当前第几个 / 共几个
    pub index: usize,
    pub count: usize,
}

/// 正在落盘的文件。
struct ActiveFile {
    file: BufWriter<File>,
    path: PathBuf,
    written: u64,
    size: Option<u64>,
    /// 远端给的 mtime（秒，Unix 纪元），有就带过来
    modified: Option<u32>,
}

/// 待发送的文件。
#[derive(Clone)]
struct OutFile {
    path: PathBuf,
    name: String,
    size: u64,
}

/// 两个角色共用的状态机外壳。
enum Machine {
    Recv(Receiver),
    Send(Sender),
}

impl Machine {
    fn wire_written(&mut self, n: usize) {
        match self {
            Machine::Recv(rx) => rx.wire_written(n),
            Machine::Send(tx) => tx.wire_written(n),
        }
    }

    fn submit_wire(&mut self, input: &[u8]) -> Result<usize, zmodem2::Error> {
        match self {
            Machine::Recv(rx) => rx.submit_wire(input),
            Machine::Send(tx) => tx.submit_wire(input),
        }
    }
}

/// 状态机给驱动层的一次性动作。
///
/// 为什么要把 `Action<'a>` 拷成自有数据：`Action` 借用了状态机内部缓冲，
/// 拿着它的时候没法再 `&mut` 访问 `Session` 自己的字段（文件/队列/统计）。
enum Act {
    Wire(Vec<u8>),
    Read { offset: u32, max_len: usize },
    File(Vec<u8>),
    Event(Ev),
    Idle,
    Stop,
}

/// 拷贝出来的协议事件（`Event<'a>` 同样借用了状态机内部的文件名缓冲）。
enum Ev {
    FileStarted {
        name: Vec<u8>,
        size: Option<u64>,
        modified: Option<u32>,
    },
    FileCompleted,
    SessionCompleted,
    Aborted,
    Other,
}

fn own_event(e: Event<'_>) -> Ev {
    match e {
        Event::FileStarted(info) => Ev::FileStarted {
            name: info.name.to_vec(),
            size: info.size.map(|p| u64::from(p.get())),
            modified: info.modified,
        },
        Event::FileCompleted => Ev::FileCompleted,
        Event::SessionCompleted => Ev::SessionCompleted,
        Event::Aborted => Ev::Aborted,
        // Event 是 non_exhaustive：crate 以后加事件时按"无影响"处理
        _ => Ev::Other,
    }
}

/// 待决定的手动接收入口（Backpressure 挡回时下一轮重试）。
enum Decision {
    Accept(u32),
    Skip,
}

/// 一个 ZMODEM 传输会话。
///
/// 生命周期：`receive`/`send` 建会话 → `feed` 喂远端字节 → `drive` 反复推进
/// （写出的字节通过回调交给传输层）→ `is_done` 后取 `outcome`，并把
/// `take_leftover()` 里状态机没吃掉的尾字节还给终端。
pub struct Session {
    role: Role,
    st: Machine,
    /// 远端字节里状态机还没吃掉的部分。
    ///
    /// `submit_wire` 的调用约定是"能解析多少吃多少，返回消费字节数"，
    /// 剩下的（半截帧）必须由我们保管，等下一批字节拼上再喂。
    inbox: Vec<u8>,
    inbox_off: usize,
    /// 本次 `drive` 最多解析多少字节。
    ///
    /// `pump` 跑在 UI 线程：一帧内把几十 MB 全咽下去，界面会直接卡住。
    /// 超预算就退出，剩下的字节留在 inbox / mpsc 通道里下一帧继续。
    budget: usize,
    /// 接收目录（Receive 用）。
    dir: PathBuf,
    active: Option<ActiveFile>,
    received: Vec<PathBuf>,
    /// 接收时打不开的文件数（只跳过，不终止整批）
    skipped: usize,
    /// 发送队列（Send 用）。
    queue: VecDeque<OutFile>,
    sending: Option<OutFile>,
    sent_paths: Vec<PathBuf>,
    in_flight: bool,
    sent_bytes: u64,
    files_done: usize,
    finish_sent: bool,
    /// 手动接受/跳过被 Backpressure 挡回时挂起
    pending_decision: Option<Decision>,
    /// 已请求中止（用户取消或本地失败）：先把 CAN 序列发出去再收尾
    aborting: bool,
    /// 中止后已经空转过几轮（见 `drive` 里等 Aborted 事件的兜底）
    abort_rounds: u8,
    /// 本地失败原因：置位后即使收到 Aborted 也按 Failed 报告
    failed: Option<String>,
    outcome: Option<Outcome>,
    last_input: Instant,
}

impl Session {
    /// 建接收会话（远端 `sz` 在等我们）。
    pub fn receive(dir: PathBuf) -> Result<Self, String> {
        // 可靠且带流控的传输（SSH / TCP / 本地 pty）下声明"缓冲 0 + 边收边写
        // (CANOVIO)"，lrzsz 的 sz 才会连续发送；否则每 1KB 就要停下等确认，
        // 高延迟链路上慢到不可用。crate 的新建失败时退回保守默认值。
        let mut rx = Receiver::with_flow_control(0, true)
            .or_else(|_| Receiver::new())
            .map_err(|e| format!("ZMODEM 接收端初始化失败: {e}"))?;
        // 手动接受：ZFILE 里的文件名完全由远端控制，必须先消毒、定好落盘路径、
        // 判断能不能创建，再决定收（accept）还是拒（ZSKIP）。
        rx.set_manual_file_accept(true);
        Ok(Self::new(Role::Receive, Machine::Recv(rx), dir))
    }

    /// 建发送会话（远端 `rz` 在等我们）。
    ///
    /// 文件可以先不给：先把握手接上（回 ZRQINIT），用户选完文件再
    /// `add_files()`。这样 rz 不会因为我们弹对话框而超时退出。
    pub fn send() -> Result<Self, String> {
        let mut tx = Sender::new().map_err(|e| format!("ZMODEM 发送端初始化失败: {e}"))?;
        // 对端通告"非停等 I/O"时不再每 10 个子包等一次确认：
        // 每个等待都是一次完整往返，长链路下这是大文件传输的主要延迟来源。
        tx.set_streaming_window(usize::MAX);
        Ok(Self::new(Role::Send, Machine::Send(tx), PathBuf::new()))
    }

    fn new(role: Role, st: Machine, dir: PathBuf) -> Self {
        Self {
            role,
            st,
            inbox: Vec::new(),
            inbox_off: 0,
            budget: 512 * 1024,
            dir,
            active: None,
            received: Vec::new(),
            skipped: 0,
            queue: VecDeque::new(),
            sending: None,
            sent_paths: Vec::new(),
            in_flight: false,
            sent_bytes: 0,
            files_done: 0,
            finish_sent: false,
            pending_decision: None,
            aborting: false,
            abort_rounds: 0,
            failed: None,
            outcome: None,
            last_input: Instant::now(),
        }
    }

    pub fn role(&self) -> Role {
        self.role
    }

    /// 喂入远端字节。
    pub fn feed(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.inbox.extend_from_slice(data);
        self.last_input = Instant::now();
    }

    /// 排队待发文件，返回真正进了队列的数量。
    pub fn add_files(&mut self, paths: Vec<PathBuf>) -> usize {
        if self.role != Role::Send || self.outcome.is_some() {
            return 0;
        }
        let mut added = 0;
        for p in paths {
            let Ok(meta) = std::fs::metadata(&p) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            // ZMODEM 头里的长度字段是 32 位：超过 4GiB 的文件协议本身表达不了
            if meta.len() > u64::from(u32::MAX) {
                continue;
            }
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "file".to_string());
            self.queue.push_back(OutFile {
                path: p,
                name,
                size: meta.len(),
            });
            added += 1;
        }
        added
    }

    /// 请求中止（用户 Ctrl+C / 取消选择）：先发 CAN 序列让对方也停下。
    pub fn abort(&mut self) {
        if self.outcome.is_some() {
            return;
        }
        self.request_abort();
    }

    /// 本地失败：记下原因并中止（同样要把 CAN 发出去）。
    ///
    /// 公开给 `term.rs`：会话僵死超时那类"由外部观察到的失败"也走同一条路。
    pub fn fail(&mut self, msg: String) {
        if self.failed.is_none() {
            self.failed = Some(msg);
        }
        self.request_abort();
    }

    fn request_abort(&mut self) {
        if self.aborting {
            return;
        }
        self.aborting = true;
        self.abort_rounds = 0;
        match &mut self.st {
            Machine::Recv(rx) => {
                // 事件队列满时 abort 会失败：那就没有 Aborted 事件可等，
                // 直接收尾（CAN 序列已经尽量排进去了）。
                if rx.abort().is_err() {
                    self.finalize();
                }
            }
            Machine::Send(tx) => tx.abort(),
        }
    }

    /// 收尾：本地失败优先，其次才算取消。
    fn finalize(&mut self) {
        if self.outcome.is_some() {
            return;
        }
        self.outcome = Some(match self.failed.take() {
            Some(e) => Outcome::Failed(e),
            None => Outcome::Cancelled,
        });
    }

    pub fn is_done(&self) -> bool {
        self.outcome.is_some()
    }

    pub fn outcome(&self) -> Option<&Outcome> {
        self.outcome.as_ref()
    }

    /// 会话结束后把状态机没消费的尾字节还回去（远端退出后的 shell 提示符在这里）。
    pub fn take_leftover(&mut self) -> Vec<u8> {
        let left = if self.inbox_off < self.inbox.len() {
            self.inbox[self.inbox_off..].to_vec()
        } else {
            Vec::new()
        };
        self.inbox.clear();
        self.inbox_off = 0;
        left
    }

    /// 距上一批远端字节过去了多久（`pump` 用它判断会话是否僵死）。
    pub fn idle_for(&self) -> Duration {
        self.last_input.elapsed()
    }

    /// 当前进度（没有正在传的文件时返回 None）。
    pub fn progress(&self) -> Option<Progress> {
        match self.role {
            Role::Receive => {
                let af = self.active.as_ref()?;
                Some(Progress {
                    role: Role::Receive,
                    name: af
                        .path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    done: af.written,
                    total: af.size,
                    index: self.files_done + 1,
                    // 收端事先不知道远端要发几个文件，不显示批次
                    count: 0,
                })
            }
            Role::Send => {
                let cur = self.sending.as_ref().or_else(|| self.queue.front())?;
                // 已完成的 + 在传的 + 排队等待的 = 本批总数
                let remaining = self.queue.len() + usize::from(self.in_flight);
                Some(Progress {
                    role: Role::Send,
                    name: cur.name.clone(),
                    done: if self.in_flight { self.sent_bytes } else { 0 },
                    total: Some(cur.size),
                    index: self.files_done + 1,
                    count: self.files_done + remaining.max(1),
                })
            }
        }
    }

    /// 状态栏用的一行进度文字。
    pub fn status_text(&self) -> Option<String> {
        let p = self.progress()?;
        let arrow = match p.role {
            Role::Receive => "⬇ 收",
            Role::Send => "⬆ 发",
        };
        let total = match p.total {
            Some(t) => fmt_bytes(t),
            None => "?".to_string(),
        };
        let batch = if p.count > 1 {
            format!("（{}/{}）", p.index, p.count)
        } else {
            String::new()
        };
        Some(format!(
            "{arrow} {} {}/{total}{batch}",
            p.name,
            fmt_bytes(p.done)
        ))
    }

    /// 结束提示（一次 toast 用）。
    pub fn notice_text(&self) -> String {
        match &self.outcome {
            Some(Outcome::Ok {
                role: Role::Receive,
                files,
            }) => {
                let skipped = if self.skipped > 0 {
                    format!("（跳过 {} 个无法保存的文件）", self.skipped)
                } else {
                    String::new()
                };
                match files.first() {
                    Some(p) if files.len() == 1 => {
                        format!("ZMODEM 接收完成：{}{skipped}", p.display())
                    }
                    Some(_) => format!(
                        "ZMODEM 接收完成：{} 个文件 → {}{skipped}",
                        files.len(),
                        self.dir.display()
                    ),
                    None => "ZMODEM 接收结束（没有文件）".to_string(),
                }
            }
            Some(Outcome::Ok {
                role: Role::Send,
                files,
            }) => format!("ZMODEM 发送完成：{} 个文件", files.len()),
            Some(Outcome::Cancelled) => "ZMODEM 传输已取消".to_string(),
            Some(Outcome::Failed(e)) => format!("ZMODEM 传输失败：{e}"),
            None => "ZMODEM 传输结束".to_string(),
        }
    }

    /// 推进状态机直到没有可做的事。
    ///
    /// `wire_out` 是"把字节写到远端"的回调（终端里就是 `io.input_tx`）。
    pub fn drive(&mut self, wire_out: &mut dyn FnMut(&[u8])) {
        if self.outcome.is_some() {
            return;
        }
        let mut parsed = 0usize;
        // 防御上限：状态机或回调出现异常时，不能让 UI 线程在这里死循环
        let mut spins = 0usize;
        loop {
            // 一帧的解析预算：把单帧占用钉在上界，剩下的下一帧继续
            if parsed >= self.budget {
                break;
            }
            spins += 1;
            if spins > 100_000 {
                break;
            }
            match self.next_act() {
                Act::Wire(bytes) => {
                    wire_out(&bytes);
                    self.st.wire_written(bytes.len());
                    parsed += bytes.len();
                }
                Act::Read { offset, max_len } => {
                    let n = self.serve_read(offset, max_len);
                    parsed += n;
                }
                Act::File(bytes) => {
                    parsed += bytes.len();
                    self.persist(&bytes);
                }
                Act::Event(ev) => self.on_event(ev),
                Act::Stop => break,
                Act::Idle => {
                    let n = self.submit_inbox();
                    parsed += n;
                    let mut progress = n > 0;
                    if let Some(d) = self.pending_decision.take() {
                        if self.apply_decision(d) {
                            progress = true;
                        }
                    }
                    if self.try_start_next() {
                        progress = true;
                    }
                    if self.aborting {
                        // 中止流程：CAN 序列可能还排在状态机里没写出去，
                        // 再转几圈把它发掉并等 Aborted 事件；一直等不到
                        // （状态机/对端没响应）就自己收尾——不能让界面
                        // 永远停在"传输中"。
                        self.abort_rounds = self.abort_rounds.saturating_add(1);
                        if self.abort_rounds >= 3 {
                            self.finalize();
                            break;
                        }
                        progress = true;
                    }
                    if !progress {
                        break;
                    }
                }
            }
            if self.outcome.is_some() {
                break;
            }
        }

        // 收尾：会话已经结束，但 poll 的优先级是"事件先于待写字节"——收端收到的
        // ZFIN 应答、发端要发的 `OO` 都可能还压在状态机里，而事件一到我们就退出
        // 了循环。不补这一轮 drain，对端会一直等这个应答（实测：loopback 里发送端
        // 永远停在"等收尾"）。
        if self.outcome.is_some() {
            for _ in 0..16 {
                match self.next_act() {
                    Act::Wire(bytes) => {
                        wire_out(&bytes);
                        self.st.wire_written(bytes.len());
                    }
                    _ => break,
                }
            }
        }
    }

    /// 取下一个动作，把借用状态机内部缓冲的数据拷成自有数据。
    fn next_act(&mut self) -> Act {
        match &mut self.st {
            Machine::Recv(rx) => match rx.poll() {
                Action::WriteWire(b) => Act::Wire(b.to_vec()),
                Action::WriteFile(b) => Act::File(b.to_vec()),
                Action::Event(e) => Act::Event(own_event(e)),
                // 接收端不会被要求读文件；真出现就停下等下一帧
                Action::ReadFile { .. } => Act::Stop,
                Action::Idle => Act::Idle,
                _ => Act::Stop,
            },
            Machine::Send(tx) => match tx.poll() {
                Action::WriteWire(b) => Act::Wire(b.to_vec()),
                Action::ReadFile { offset, max_len } => Act::Read {
                    offset: offset.get(),
                    max_len,
                },
                Action::Event(e) => Act::Event(own_event(e)),
                Action::WriteFile(_) => Act::Stop,
                Action::Idle => Act::Idle,
                _ => Act::Stop,
            },
        }
    }

    /// 把 inbox 里能解析的字节喂给状态机，返回消费字节数。
    fn submit_inbox(&mut self) -> usize {
        if self.inbox_off >= self.inbox.len() {
            self.inbox.clear();
            self.inbox_off = 0;
            return 0;
        }
        let consumed = match self.st.submit_wire(&self.inbox[self.inbox_off..]) {
            Ok(n) => n,
            Err(e) => {
                // crate 约定：能自愈的坏子包（CRC 错）内部已经记下 ZRPOS 并以 Ok
                // 返回；真的返回 Err 说明传输已经没救了（重试预算耗尽等），
                // 继续喂同样的字节只会一直报同一个错 → 直接失败并中止。
                self.fail(format!("ZMODEM 协议错误: {e}"));
                return 0;
            }
        };
        self.inbox_off += consumed;
        if self.inbox_off == self.inbox.len() {
            self.inbox.clear();
            self.inbox_off = 0;
        } else if self.inbox_off > 64 * 1024 {
            self.inbox.drain(..self.inbox_off);
            self.inbox_off = 0;
        }
        consumed
    }

    fn on_event(&mut self, ev: Ev) {
        match ev {
            Ev::FileStarted {
                name,
                size,
                modified,
            } => {
                if self.role != Role::Receive {
                    return;
                }
                match self.open_dest(&name, size, modified) {
                    Ok(af) => {
                        self.active = Some(af);
                        self.try_accept(0);
                    }
                    Err(e) => {
                        // 单个文件存不下不该毁掉整批传输：拒收这个（ZSKIP），
                        // 让远端继续下一个文件。
                        log::warn!("ZMODEM: 拒绝远端文件：{e}");
                        self.pending_decision = Some(Decision::Skip);
                    }
                }
            }
            Ev::FileCompleted => {
                if self.role == Role::Receive {
                    self.close_active();
                } else {
                    if let Some(f) = self.sending.take() {
                        self.sent_paths.push(f.path);
                    }
                    self.in_flight = false;
                    self.files_done += 1;
                }
            }
            Ev::SessionCompleted => {
                self.outcome = Some(match self.failed.take() {
                    Some(e) => Outcome::Failed(e),
                    None => Outcome::Ok {
                        role: self.role,
                        files: match self.role {
                            Role::Receive => self.received.clone(),
                            Role::Send => self.sent_paths.clone(),
                        },
                    },
                });
            }
            Ev::Aborted => self.finalize(),
            Ev::Other => {}
        }
    }

    /// 建落盘文件：消毒名字 + 不覆盖 + 立刻创建（把权限/磁盘错误挡在接收之前）。
    fn open_dest(
        &mut self,
        raw_name: &[u8],
        size: Option<u64>,
        modified: Option<u32>,
    ) -> Result<ActiveFile, String> {
        let name = sanitize_name(raw_name);
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| format!("创建目录 {} 失败: {e}", self.dir.display()))?;
        let path = unique_path(&self.dir, &name);
        // create_new：即使有并发/竞态也绝不覆盖已有文件
        let f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("创建文件 {} 失败: {e}", path.display()))?;
        Ok(ActiveFile {
            file: BufWriter::new(f),
            path,
            written: 0,
            size,
            modified,
        })
    }

    /// 关掉当前接收文件（刷盘 + 带过 mtime），计入完成列表。
    fn close_active(&mut self) {
        let Some(mut af) = self.active.take() else {
            return;
        };
        if let Err(e) = af.file.flush() {
            self.fail(format!("写入 {} 失败: {e}", af.path.display()));
            return;
        }
        if let Some(m) = af.modified {
            let t = std::time::UNIX_EPOCH + Duration::from_secs(u64::from(m));
            // 失败无所谓：时间戳只是锦上添花
            let _ = af.file.get_ref().set_modified(t);
        }
        let p = af.path.clone();
        self.files_done += 1;
        self.received.push(p);
    }

    /// 手动接受模式下的决定：接受（从 offset 开始）。
    fn try_accept(&mut self, offset: u32) {
        self.pending_decision = Some(Decision::Accept(offset));
        if let Some(d) = self.pending_decision.take() {
            self.apply_decision(d);
        }
    }

    /// 返回是否真的完成了决定（false = 被 Backpressure 挡回，下一轮再试）。
    fn apply_decision(&mut self, d: Decision) -> bool {
        let res = match (&mut self.st, &d) {
            (Machine::Recv(rx), Decision::Accept(off)) => rx.accept_file_at(*off).err(),
            (Machine::Recv(rx), Decision::Skip) => rx.skip_file().err(),
            // 只有接收端有"接受/拒收"这回事
            _ => None,
        };
        match res {
            None => {
                if matches!(d, Decision::Skip) {
                    self.skipped += 1;
                }
                true
            }
            Some(zmodem2::Error::Backpressure) => {
                // 还有待写出的字节：先让 poll 把它们发掉，下一轮再决定
                self.pending_decision = Some(d);
                false
            }
            Some(e) => {
                self.fail(format!("ZMODEM 会话状态错误: {e}"));
                true
            }
        }
    }

    /// 落盘一块接收数据。
    fn persist(&mut self, bytes: &[u8]) {
        let n = bytes.len();
        let res = match self.active.as_mut() {
            Some(af) => af.file.write_all(bytes).map(|()| {
                af.written += n as u64;
            }),
            // 手动接受模式下没接受就不会有数据；真出现也不能卡住状态机
            None => Ok(()),
        };
        if let Err(e) = res {
            let p = self
                .active
                .as_ref()
                .map(|a| a.path.display().to_string())
                .unwrap_or_else(|| "文件".to_string());
            self.fail(format!("写入 {p} 失败: {e}"));
            return;
        }
        if let Machine::Recv(rx) = &mut self.st {
            if let Err(e) = rx.file_written(n) {
                self.fail(format!("ZMODEM 接收状态错误: {e}"));
            }
        }
    }

    /// 发送端：读一块文件数据交给状态机，返回提交的字节数。
    fn serve_read(&mut self, offset: u32, max_len: usize) -> usize {
        let Some(f) = self.sending.as_ref() else {
            self.fail("状态机请求文件数据，但当前没有正在发送的文件".to_string());
            return 0;
        };
        let path = f.path.clone();
        let mut buf = vec![0u8; max_len.min(64 * 1024)];
        let res = read_at(&path, offset, &mut buf);
        let n = match res {
            Ok(0) => {
                // 文件在传输途中被截断：不能提交空块（状态机会判 UnexpectedEof）
                self.fail(format!("待发文件在传输中变短: {}", path.display()));
                return 0;
            }
            Ok(n) => n,
            Err(e) => {
                self.fail(format!("读取待发文件失败 {}: {e}", path.display()));
                return 0;
            }
        };
        let r = match &mut self.st {
            Machine::Send(tx) => tx.submit_file(&buf[..n]),
            _ => return 0,
        };
        match r {
            Ok(()) => {
                self.sent_bytes += n as u64;
                n
            }
            Err(e) => {
                self.fail(format!("提交待发数据失败: {e}"));
                0
            }
        }
    }

    /// 发送端：状态机空闲时开始下一个文件；队列发空则请求结束会话。
    fn try_start_next(&mut self) -> bool {
        if self.role != Role::Send || self.in_flight || self.outcome.is_some() {
            return false;
        }
        let Some(front) = self.queue.front().cloned() else {
            // 全部发完（至少发过一个）→ 请求收尾（ZFIN + OO 握手）
            if self.files_done > 0 && !self.finish_sent {
                self.finish_sent = true;
                if let Machine::Send(tx) = &mut self.st {
                    return tx.finish().is_ok();
                }
            }
            return false;
        };
        let info = FileInfo::new(
            front.name.as_bytes(),
            Some(Position::new(front.size as u32)),
        );
        let ok = match &mut self.st {
            Machine::Send(tx) => tx.start_file(info).is_ok(),
            _ => false,
        };
        if ok {
            // 只有 start_file 成功（状态机已经进入"有文件"）才从队列取走
            self.queue.pop_front();
            self.sent_bytes = 0;
            self.sending = Some(front);
            self.in_flight = true;
            true
        } else {
            // 对端还没把 ZRINIT 送齐（状态机还在等接收方初始化）→ 下一轮再试
            false
        }
    }

    /// 测试用：一行状态快照（loopback 卡住时打出来定位）。
    #[cfg(test)]
    fn debug_snapshot(&self) -> String {
        format!(
            "role={:?} inbox={}(off {}) in_flight={} queue={} done_files={} written={:?} aborting={} failed={:?} outcome={:?}",
            self.role,
            self.inbox.len(),
            self.inbox_off,
            self.in_flight,
            self.queue.len(),
            self.files_done,
            self.active.as_ref().map(|a| a.written),
            self.aborting,
            self.failed,
            self.outcome,
        )
    }
}

/// 从指定偏移读满一块（文件系统/网络盘可能短读，短读不能当 EOF 报给协议）。
fn read_at(path: &Path, offset: u32, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(u64::from(offset)))?;
    let mut total = 0;
    while total < buf.len() {
        match f.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc;

    // ---- 触发检测 ----

    fn hit(scan: Scan<'_>) -> Option<(Trigger, Vec<u8>, Vec<u8>)> {
        match scan {
            Scan::Hit {
                trigger,
                before,
                head,
            } => Some((trigger, before.into_owned(), head)),
            Scan::Miss { .. } => None,
        }
    }

    fn miss_bytes(scan: Scan<'_>) -> Vec<u8> {
        match scan {
            Scan::Miss { bytes } => bytes.into_owned(),
            Scan::Hit { before, .. } => before.into_owned(),
        }
    }

    #[test]
    fn detector_hits_both_start_frames() {
        let mut d = Detector::new();
        let got = hit(d.scan(b"**\x18B00\x00\x00\x00\x00\r\n")).expect("ZRQINIT 应命中");
        assert_eq!(got.0, Trigger::Receive);
        assert!(got.1.is_empty(), "头之前没有字节");
        assert_eq!(got.2, b"**\x18B00\x00\x00\x00\x00\r\n");

        let mut d = Detector::new();
        let got = hit(d.scan(b"**\x18B0100000023be50\r\n\x11")).expect("ZRINIT 应命中");
        assert_eq!(got.0, Trigger::Send);
    }

    #[test]
    fn detector_keeps_bytes_before_header_for_vte() {
        let mut d = Detector::new();
        let got = hit(d.scan(b"user@host:~$ **\x18B00rest")).expect("应命中");
        assert_eq!(got.1, b"user@host:~$ ".to_vec(), "头之前的输出必须照常进终端");
        assert_eq!(got.2, b"**\x18B00rest".to_vec());
    }

    #[test]
    fn detector_spans_chunk_boundaries() {
        // 把一个完整的头切成三段分别喂：必须只在最后一段命中，且早先的普通
        // 输出不能被吞掉
        let mut d = Detector::new();
        let a = miss_bytes(d.scan(b"hello "));
        assert_eq!(a, b"hello ".to_vec());
        let b = miss_bytes(d.scan(b"**\x18"));
        assert!(b.is_empty(), "半个头要扣住，不能先进终端");
        assert!(d.has_pending());
        let got = hit(d.scan(b"B00xyz")).expect("补齐后应命中");
        assert_eq!(got.0, Trigger::Receive);
        assert!(got.1.is_empty());
        assert_eq!(got.2, b"**\x18B00xyz".to_vec());
        assert!(!d.has_pending());

        // 逐字节喂也要能命中（1 字节一个 chunk 是最恶劣的切法）
        let mut d = Detector::new();
        let full = b"**\x18B01\x00\x00\x00\x00\r\n";
        let mut found = None;
        for (i, byte) in full.iter().enumerate() {
            match d.scan(std::slice::from_ref(byte)) {
                Scan::Hit { trigger, head, .. } => {
                    found = Some((i, trigger, head));
                    break;
                }
                Scan::Miss { .. } => {}
            }
        }
        let (i, trigger, head) = found.expect("逐字节喂也必须命中");
        assert_eq!(i, HEADER_LEN - 1, "最后一位十六进制到齐时才命中");
        assert_eq!(trigger, Trigger::Send);
        // 头到齐的那一刻，检测器手里只有这 6 个字节（后面的还没到）
        assert_eq!(head, full[..HEADER_LEN].to_vec());
    }

    #[test]
    fn detector_ignores_lookalikes_and_normal_output() {
        // 普通输出（含 `**`、单独 0x18、合法的半个头前缀）一律原样放行
        let mut d = Detector::new();
        assert_eq!(miss_bytes(d.scan(b"total ** 42 files\n")), b"total ** 42 files\n".to_vec());
        assert_eq!(miss_bytes(d.scan(b"\x18[?25l")), b"\x18[?25l".to_vec());
        assert_eq!(
            miss_bytes(d.scan(b"a * b * c *")),
            b"a * b * c ".to_vec(),
            "结尾的 `*` 会被扣住等补全"
        );
        // 扣住的 `*` 超时后必须还回去
        d.force_stale();
        assert_eq!(d.flush_stale(), Some(b"*".to_vec()));
        assert!(!d.has_pending());

        // 十六进制位非法 → 不是 ZMODEM 头
        let mut d = Detector::new();
        assert_eq!(
            miss_bytes(d.scan(b"**\x18BZZ")),
            b"**\x18BZZ".to_vec(),
            "非法十六进制不能触发"
        );
        // 完整头但不是开场帧（ZFILE=04）→ 不在会话里，按普通输出放行
        let mut d = Detector::new();
        assert_eq!(miss_bytes(d.scan(b"**\x18B04")), b"**\x18B04".to_vec());
        // 只有前缀、后面没有十六进制位 → 不触发，超时后还回终端
        let mut d = Detector::new();
        assert!(miss_bytes(d.scan(b"**\x18B")).is_empty());
        d.force_stale();
        assert_eq!(d.flush_stale(), Some(b"**\x18B".to_vec()));
    }

    #[test]
    fn detector_flush_keeps_normal_output_flowing() {
        let mut d = Detector::new();
        // 尚未超时：继续扣着（等同一个头剩下的字节）
        assert!(miss_bytes(d.scan(b"***")).len() == 1);
        assert_eq!(d.flush_stale(), None);
        // 下一批字节补不齐 → 之前扣住的要回到终端
        let out = miss_bytes(d.scan(b"not a header"));
        assert_eq!(out, b"**not a header".to_vec());
    }

    // ---- 会话结束后的协议尾巴过滤 ----

    #[test]
    fn tail_filter_swallows_protocol_residue_but_keeps_output() {
        // 最后一个应答头 + `OO` + shell 提示符：只留提示符
        let mut f = TailFilter::default();
        let out = f.start(b"**\x18B0b00000000\r\n\x11OOroot@host:~$ ");
        assert_eq!(out, b"root@host:~$ ".to_vec(), "协议尾巴不能被显示出来");
        assert!(!f.is_active(), "见到正常输出后过滤应当停止");
    }

    #[test]
    fn tail_filter_handles_split_tail_across_chunks() {
        let mut f = TailFilter::default();
        // 第一批只有一个头的前半截：先扣着，什么都别显示
        assert!(f.start(b"**\x18B0b0000").is_empty());
        assert!(f.is_active());
        // 第二批补齐头尾，接上正常输出
        let out = f.push(b"0000\r\n\x11OOprompt$ ");
        assert_eq!(out, b"prompt$ ".to_vec());
        assert!(!f.is_active());
    }

    #[test]
    fn tail_filter_returns_held_bytes_at_window_end() {
        let mut f = TailFilter::default();
        assert!(f.start(b"**\x18B0b").is_empty());
        // 窗口结束：认不出结果的字节必须还回去，不能静默丢输出
        assert_eq!(f.finish(), b"**\x18B0b".to_vec());
        assert!(!f.is_active());
    }

    #[test]
    fn tail_filter_does_not_eat_normal_output() {
        // 正常输出里的 `*` / `O` / 十六进制字符都不能被吞掉
        let mut f = TailFilter::default();
        assert_eq!(f.start(b"hello * world").to_vec(), b"hello * world".to_vec());
        assert_eq!(f.push(b"OK").to_vec(), b"OK".to_vec());
    }

    // ---- 文件名消毒 ----

    #[test]
    fn sanitize_strips_paths_and_traversal() {
        assert_eq!(sanitize_name(b"../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name(b"a/b.txt"), "b.txt");
        assert_eq!(sanitize_name(b"C:\\x\\y"), "y");
        assert_eq!(sanitize_name(b"/etc/shadow"), "shadow");
        assert_eq!(sanitize_name(b"..\\..\\windows\\system32\\cmd.exe"), "cmd.exe");
    }

    #[test]
    fn sanitize_rejects_empty_and_hostile_names() {
        // 空名 / 只剩分隔符 / 全是点 → 兜底名（仍然是合法文件名）
        assert_eq!(sanitize_name(b""), "zmodem_received");
        assert_eq!(sanitize_name(b"/"), "zmodem_received");
        assert_eq!(sanitize_name(b".."), "zmodem_received");
        assert_eq!(sanitize_name(b"..."), "zmodem_received");
        // 控制字符（含 0x18 ZDLE）与 Windows 非法字符都要被换掉
        let n = sanitize_name(b"a\x18b\x00c:d*.txt");
        assert!(!n.chars().any(|c| c.is_control()));
        assert_eq!(n, "a_b_c_d_.txt");
        // Windows 设备保留名不能直接当文件名
        assert_eq!(sanitize_name(b"NUL"), "_NUL");
        assert_eq!(sanitize_name(b"con.txt"), "_con.txt");
        // 非 UTF-8 名字不能导致失败
        assert!(!sanitize_name(&[0xff, 0xfe, b'.', b'b', b'i', b'n']).is_empty());
    }

    #[test]
    fn sanitize_truncates_long_names_but_keeps_extension() {
        let long = format!("{}.bin", "x".repeat(500));
        let n = sanitize_name(long.as_bytes());
        assert!(n.len() <= MAX_NAME_LEN, "超长名必须截断: {}", n.len());
        assert!(n.ends_with(".bin"), "扩展名要保住: {n}");
        // 中文名按字节截断也不能切出半个字符
        let cjk = format!("{}.txt", "文".repeat(300));
        let n = sanitize_name(cjk.as_bytes());
        assert!(n.len() <= MAX_NAME_LEN);
        assert!(std::str::from_utf8(n.as_bytes()).is_ok());
        assert!(n.ends_with(".txt"));
    }

    #[test]
    fn unique_path_never_overwrites() {
        let dir = temp_dir("unique");
        let a = unique_path(&dir, "report.txt");
        assert!(a.ends_with("report.txt"));
        std::fs::write(&a, b"first").unwrap();

        let b = unique_path(&dir, "report.txt");
        assert!(b.ends_with("report.1.txt"), "重名要加序号: {}", b.display());
        std::fs::write(&b, b"second").unwrap();

        let c = unique_path(&dir, "report.txt");
        assert!(c.ends_with("report.2.txt"));
        std::fs::write(&c, b"third").unwrap();

        // 三个文件都在，旧内容没被覆盖
        assert_eq!(std::fs::read(&a).unwrap(), b"first");
        assert_eq!(std::fs::read(&b).unwrap(), b"second");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 端到端 loopback ----

    fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        // 刻意不用 std::env::temp_dir()：某些受限环境（本机/CI）里测试进程
        // 对 %TEMP% 没有写权限，用包根下的 target 目录最稳（cargo test 的
        // 工作目录就是包根）。
        let base = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("target")
            .join("zmodem-test-tmp");
        let p = base.join(format!("{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).expect("创建测试目录");
        p
    }

    /// 测试用：把一个本地文件按指定的"线上文件名"塞进发送队列。
    ///
    /// 真实链路里线上名字来自本地文件的 basename；这里可以造出
    /// `../../escape.bin` 这种远端才会遇到的名字，验证收端消毒。
    fn push_file_for_test(session: &mut Session, wire_name: &str, local: &Path) {
        let size = std::fs::metadata(local).unwrap().len();
        session.queue.push_back(OutFile {
            path: local.to_path_buf(),
            name: wire_name.to_string(),
            size,
        });
    }

    /// 测试用：把两个会话对接起来跑到双方都结束（用两条 mpsc 单向管道当传输层）。
    fn run_loopback(sender: &mut Session, receiver: &mut Session) {
        let (down_tx, down_rx) = mpsc::channel::<Vec<u8>>();
        let (up_tx, up_rx) = mpsc::channel::<Vec<u8>>();
        let mut steps = 0usize;
        let mut idle_steps = 0usize;
        let mut moved = 0usize;
        while !(sender.is_done() && receiver.is_done()) {
            steps += 1;
            assert!(
                steps < 200_000,
                "loopback 传输没有在预算内结束（协议死锁？）\n  发送端: {}\n  接收端: {}",
                sender.debug_snapshot(),
                receiver.debug_snapshot()
            );
            let before_bytes = moved;
            {
                let tx = down_tx.clone();
                let mut out = |b: &[u8]| {
                    let _ = tx.send(b.to_vec());
                };
                sender.drive(&mut out);
            }
            while let Ok(chunk) = down_rx.try_recv() {
                moved += chunk.len();
                receiver.feed(&chunk);
            }
            {
                let tx = up_tx.clone();
                let mut out = |b: &[u8]| {
                    let _ = tx.send(b.to_vec());
                };
                receiver.drive(&mut out);
            }
            while let Ok(chunk) = up_rx.try_recv() {
                moved += chunk.len();
                sender.feed(&chunk);
            }
            if moved == before_bytes {
                idle_steps += 1;
                assert!(
                    idle_steps < 50,
                    "loopback 卡住了（连续 50 步没有任何字节流动）\n  发送端: {}\n  接收端: {}",
                    sender.debug_snapshot(),
                    receiver.debug_snapshot()
                );
            } else {
                idle_steps = 0;
            }
        }
    }

    /// 造一份"对 ZMODEM 最不友好"的二进制内容：转义字节、CR/LF、
    /// 高位字节，还有一段会被误认为 ZMODEM 头的字面量。
    fn hostile_payload(len: usize) -> Vec<u8> {
        const SPECIAL: [u8; 8] = [0x18, 0x11, 0x0d, 0x0a, 0xff, 0x91, 0x13, 0x00];
        let mut v = Vec::with_capacity(len + 16);
        for i in 0..len {
            if i % 13 == 0 {
                v.push(SPECIAL[(i / 13) % SPECIAL.len()]);
            } else {
                v.push((i % 251) as u8);
            }
        }
        // 文件中间塞一个真实的 ZMODEM 头字面量：证明数据流里的 `**\x18B00`
        // 不会把传输本身搞乱（转义由协议层负责）
        v.splice(len / 2..len / 2, b"**\x18B00\x11\x18\r".iter().copied());
        v
    }

    /// 关键测试：**不需要真机、不需要 lrzsz**，用两个 mpsc 管道把我们自己的
    /// Receiver 与 Sender 对接，传一个含转义字节的二进制文件，断言收端落盘的
    /// 字节与发端**完全一致**、文件名正确、双方会话正常结束。
    #[test]
    fn loopback_binary_roundtrip_over_mpsc() {
        let src_dir = temp_dir("loop-src");
        let dst_dir = temp_dir("loop-dst");
        let contents = hostile_payload(64 * 1024 + 137);
        let src = src_dir.join("payload.bin");
        std::fs::write(&src, &contents).unwrap();

        let mut sender = Session::send().expect("建发送会话");
        assert_eq!(sender.add_files(vec![src.clone()]), 1);
        let mut receiver = Session::receive(dst_dir.clone()).expect("建接收会话");

        run_loopback(&mut sender, &mut receiver);

        // 双方都要正常收尾，而不是取消/失败
        match sender.outcome() {
            Some(Outcome::Ok { role: Role::Send, files }) => {
                assert_eq!(files.len(), 1);
                assert_eq!(files[0], src);
            }
            other => panic!("发送端没有正常结束: {other:?}"),
        }
        let received_path = match receiver.outcome() {
            Some(Outcome::Ok {
                role: Role::Receive,
                files,
            }) => {
                assert_eq!(files.len(), 1, "应该正好收到一个文件");
                assert_eq!(
                    files[0].file_name().unwrap().to_string_lossy(),
                    "payload.bin",
                    "收到的文件名必须是消毒后的原名"
                );
                files[0].clone()
            }
            other => panic!("接收端没有正常结束: {other:?}"),
        };

        // 核心断言：字节完全一致
        let got = std::fs::read(&received_path).unwrap();
        assert_eq!(got.len(), contents.len(), "长度不一致");
        assert_eq!(got, contents, "收到的字节与发端不一致");
        // 落盘目录就是会话指定的目录
        assert_eq!(received_path.parent().unwrap(), dst_dir.as_path());

        let _ = std::fs::remove_dir_all(&src_dir);
        let _ = std::fs::remove_dir_all(&dst_dir);
    }

    /// 单个文件的落盘路径同样是消毒后的名字（穿越名不会逃出目录）。
    #[test]
    fn loopback_sanitizes_remote_file_name() {
        let src_dir = temp_dir("loop-src-name");
        let dst_dir = temp_dir("loop-dst-name");
        let local = src_dir.join("evil");
        std::fs::write(&local, b"payload").unwrap();

        let mut sender = Session::send().unwrap();
        let mut receiver = Session::receive(dst_dir.clone()).unwrap();
        // 真实链路里这个名字直接来自远端 ZFILE，可以带 `../../`
        push_file_for_test(&mut sender, "../../escape.bin", &local);

        run_loopback(&mut sender, &mut receiver);

        let files = match receiver.outcome() {
            Some(Outcome::Ok { files, .. }) => files.clone(),
            other => panic!("接收端没有正常结束: {other:?}"),
        };
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_name().unwrap().to_string_lossy(), "escape.bin");
        assert_eq!(files[0].parent().unwrap(), dst_dir.as_path());
        assert_eq!(std::fs::read(&files[0]).unwrap(), b"payload");

        let _ = std::fs::remove_dir_all(&src_dir);
        let _ = std::fs::remove_dir_all(&dst_dir);
    }

    /// 取消：本地 abort 后双方都不能卡死，且必须把 CAN 序列发出去。
    #[test]
    fn abort_sends_cancel_and_finishes() {
        let mut receiver = Session::receive(temp_dir("abort")).unwrap();
        let mut wire: Vec<u8> = Vec::new();
        {
            let mut out = |b: &[u8]| wire.extend_from_slice(b);
            receiver.drive(&mut out);
        }
        wire.clear();
        receiver.abort();
        {
            let mut out = |b: &[u8]| wire.extend_from_slice(b);
            receiver.drive(&mut out);
        }
        assert!(receiver.is_done());
        assert_eq!(receiver.outcome(), Some(&Outcome::Cancelled));
        // lrzsz 的 canit()：10 个 CAN(0x18) + 10 个退格
        assert!(
            wire.starts_with(&[0x18u8; 10]),
            "取消必须发 CAN 序列，实际: {wire:?}"
        );
        assert!(wire[10..20].iter().all(|b| *b == 0x08));
    }

    /// 收端在磁盘写失败时必须收尾（不能把 UI 卡在"传输中"）。
    #[test]
    fn receive_fails_cleanly_when_destination_is_unusable() {
        // 用一个"目录路径其实是文件"的位置当接收目录
        let dir = temp_dir("bad-dest");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();

        let mut sender = Session::send().unwrap();
        let mut receiver = Session::receive(blocker.clone()).unwrap();
        let payload = hostile_payload(2048);
        let local = dir.join("local-src");
        std::fs::write(&local, &payload).unwrap();
        push_file_for_test(&mut sender, "payload.bin", &local);

        run_loopback(&mut sender, &mut receiver);
        // 存不下 → 拒收这个文件（ZSKIP），会话本身仍然是正常结束
        match receiver.outcome() {
            Some(Outcome::Ok { files, .. }) => assert!(files.is_empty()),
            other => panic!("接收端结局异常: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
