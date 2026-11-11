//! 进程内 X11 服务端（自研，纯 Rust）。
//!
//! 设计与里程碑见 `docs/x11-server.md`。当前进度：**M2 + M5/M6 的精简版**
//! —— M1（握手、分帧、资源与窗口语义、核心事件）之上有绘制能力（圆弧/多边形、
//! 线宽与虚线、tile/stipple、核心字体），并且已经接进应用：
//! `host.rs` 起进程内服务端并分配 display，`viewer.rs` 把合成结果当帧推给
//! 前端 canvas，`input.rs` 把画布上的键鼠翻译成 X 事件。
//!
//! 边界：线格式（请求解析、回复/事件编码、资源 ID 分配、认证）复用
//! `x11rb-protocol`，本模块只实现服务端语义。这样精力花在协议行为上，
//! 而不是字节拼装与长度回填这类容易写错的地方。

pub mod draw;
pub mod error;
pub mod xinput;
pub mod xkb;
pub mod extensions;
pub mod font;
pub mod font_data;
pub mod handshake;
pub mod host;
pub mod input;
pub mod misc;
pub mod raster;
pub mod region;
pub mod render;
pub mod request;
pub mod server;
pub mod state;
pub mod viewer;

pub use handshake::{resource_id_base, SetupError, SETUP_PREFIX_LEN};
pub use host::{HostServer, XInputEvent};
pub use server::{serve_stream, serve_tcp, ServerConfig, ServerShared};

/// 子系统计数的全局口径：心跳日志用它把 CPU 归因到具体路径
/// （请求数涨 → X 请求路径忙；合成数涨 → 客户端在重绘；帧数/字节涨 → 编码与传输忙；
/// 全都不涨但 CPU 仍高 → 问题在 X 之外，比如终端渲染或 UI）。
pub mod counters {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub static REQUESTS: AtomicU64 = AtomicU64::new(0);
    pub static COMPOSES: AtomicU64 = AtomicU64::new(0);
    pub static FRAMES: AtomicU64 = AtomicU64::new(0);
    pub static FRAME_BYTES: AtomicU64 = AtomicU64::new(0);
    /// 客户端连接数（握手成功）。
    pub static CONNECTIONS: AtomicU64 = AtomicU64::new(0);
    /// 连接结束数（客户端断开或服务端报错）。
    pub static DISCONNECTS: AtomicU64 = AtomicU64::new(0);
    /// InternAtom 探针日志计数（限 200 条）。
    pub static ATOM_LOGS: AtomicU64 = AtomicU64::new(0);
    /// 字形/填充矩形探针日志计数（限 800 条，中文插入符归因）。
    pub static GLYPH_LOGS: AtomicU64 = AtomicU64::new(0);

    /// UI 是否有新内容需要立刻重绘（终端 PTY 数据 / 远程画面帧 / 扫描结果）。
    ///
    /// 应用层的"续帧"循环据此决定要不要马上再画一帧。原先是无条件每帧续帧
    /// （60fps 全 UI 重绘），实测把主线程占满 `cpu=+4~5s/5s`，输入事件被饿死，
    /// 表现就是"过一会界面就操作不了"。
    pub static UI_DIRTY: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    /// 标记 UI 有新内容（有新画面/新输出）。
    pub fn mark_ui_dirty() {
        UI_DIRTY.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// 取走并清除脏标记。`true` = 上次取走后有过新内容。
    pub fn take_ui_dirty() -> bool {
        UI_DIRTY.swap(false, std::sync::atomic::Ordering::Relaxed)
    }

    /// 请求类型直方图的共享表（note_request 写、take_histo 读清）。
    /// 必须是**同一个** static——函数内各自声明 static 会变成两张独立的表。
    static REQUEST_HISTO: std::sync::Mutex<Option<std::collections::HashMap<u16, u64>>> =
        std::sync::Mutex::new(None);

    /// 请求类型直方图：(major<<8 | minor) → 计数。心跳日志打印 top-N，
    /// 用于归因"客户端在刷什么请求"（firefox 卡顿归因）。
    pub fn note_request(major: u8, minor: u8) {
        let key = ((major as u16) << 8) | minor as u16;
        if let Ok(mut guard) = REQUEST_HISTO.lock() {
            guard.get_or_insert_with(std::collections::HashMap::new).entry(key).and_modify(|c| *c += 1).or_insert(1);
        }
    }

    /// 取直方图 top-N（顺带清零，心跳窗口增量）。key = (major<<8)|minor。
    pub fn take_histo() -> Vec<(u16, u64)> {
        let mut out = Vec::new();
        if let Ok(mut guard) = REQUEST_HISTO.lock() {
            if let Some(map) = guard.as_mut() {
                out = map.iter().map(|(k, v)| (*k, *v)).collect();
                out.sort_by(|a, b| b.1.cmp(&a.1));
                out.truncate(6);
                map.clear();
            }
        }
        out
    }

    pub fn snapshot() -> (u64, u64, u64, u64) {
        (
            REQUESTS.load(Ordering::Relaxed),
            COMPOSES.load(Ordering::Relaxed),
            FRAMES.load(Ordering::Relaxed),
            FRAME_BYTES.load(Ordering::Relaxed),
        )
    }
}

/// 廉价诊断日志：窗口生命周期/错误这类**低频**事件常开，写 `NYATERM_RDPDR_LOG`
///
/// 关键点是**缓存文件句柄**：早先的探针每条都 open+write+close，在逐 PDU 的
/// 热路径上每秒上千次同步 I/O，把输入回显都拖慢了。这里只在首次调用时打开，
/// 并限制总行数，避免客户端异常刷屏时反噬。
pub fn log_line(args: std::fmt::Arguments<'_>) {
    use std::io::Write as _;
    use std::sync::{Mutex, OnceLock, atomic::{AtomicUsize, Ordering}};

    /// 上限：超过后只丢弃，宁可丢诊断也不能拖慢会话。
    const MAX_LINES: usize = 20_000;

    static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    static WRITTEN: AtomicUsize = AtomicUsize::new(0);

    if WRITTEN.load(Ordering::Relaxed) >= MAX_LINES {
        return;
    }
    let Some(file) = FILE
        .get_or_init(|| {
            let path = std::env::var_os("NYATERM_RDPDR_LOG")?;
            std::fs::OpenOptions::new().create(true).append(true).open(path).ok().map(Mutex::new)
        })
        .as_ref()
    else {
        return;
    };
    if let Ok(mut file) = file.lock() {
        let _ = file.write_all(format!("{args}\n").as_bytes());
        WRITTEN.fetch_add(1, Ordering::Relaxed);
    }
}