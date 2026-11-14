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

    // ==== task-24 挂死取证信标 ====
    // 冻结后 rdpdr.log 整齐停在半路、crash.log 无 panic、进程随后消失，无法区分
    // 「应用挂死」与「被外部结束」。这些原子量由看门狗（spawn_watchdog）每 2s
    // 写进**独立文件** alive.txt / stall.txt，绕开可能被阻塞的日志锁。
    pub static UI_TICK: AtomicU64 = AtomicU64::new(0);
    pub static UI_PHASE: AtomicU64 = AtomicU64::new(0);
    pub static FRAME_PHASE: AtomicU64 = AtomicU64::new(0);
    pub static LAST_REQ_KEY: AtomicU64 = AtomicU64::new(0);
    pub static LAST_REQ_MS: AtomicU64 = AtomicU64::new(0);
    pub static RASTER_BUSY_ORDER: AtomicU64 = AtomicU64::new(0);

    /// 进程内单调毫秒时钟（所有信标共用一个基准）。
    pub fn now_ms() -> u64 {
        use std::sync::OnceLock;
        static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
        EPOCH.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
    }

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
        // 看门狗信标：卡死时"正在处理哪条请求"是唯一能定位到请求级的线索
        LAST_REQ_KEY.store(u64::from(key), Ordering::Relaxed);
        LAST_REQ_MS.store(now_ms(), Ordering::Relaxed);
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

/// 挂死取证看门狗（task-24）：每 2s 把各子系统信标写进 `alive.txt`；UI tick
/// 停摆 >=5s 时另写 `stall.txt` 快照。**刻意不走 `log_line`**——它自己可能被
/// 阻塞或毒化，那一刻主日志一个字也写不出来。首次调用才启动。
pub fn spawn_watchdog() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::Builder::new()
        .name("rustterm-watchdog".to_string())
        .spawn(|| {
            let Some(dir) = dirs::config_dir().map(|d| d.join("rustterm-gpui")) else {
                return;
            };
            let _ = std::fs::create_dir_all(&dir);
            let alive_path = dir.join("alive.txt");
            let stall_path = dir.join("stall.txt");
            let _ = std::fs::remove_file(&stall_path);
            let mut last_tick = counters::UI_TICK.load(Ordering::Relaxed);
            let mut last_change = counters::now_ms();
            let mut reported = false;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(2000));
                let now = counters::now_ms();
                let tick = counters::UI_TICK.load(Ordering::Relaxed);
                let (reqs, composes, frames, bytes) = counters::snapshot();
                let phase = counters::UI_PHASE.load(Ordering::Relaxed);
                let fphase = counters::FRAME_PHASE.load(Ordering::Relaxed);
                let freq = counters::LAST_REQ_KEY.load(Ordering::Relaxed);
                let fage = now.saturating_sub(counters::LAST_REQ_MS.load(Ordering::Relaxed));
                let raster = counters::RASTER_BUSY_ORDER.load(Ordering::Relaxed);
                let line = format!(
                    "pid={} now_ms={now} ui_tick={tick} ui_phase={phase} frame_phase={fphase} last_req={freq:#06x} last_req_age_ms={fage} raster_order={raster} reqs={reqs} composes={composes} frames={frames} bytes={bytes} alloc_cur={}MB\n",
                    std::process::id(),
                    crate::ALLOC_CURRENT.load(std::sync::atomic::Ordering::Relaxed) / (1024 * 1024)
                );
                let _ = std::fs::write(&alive_path, &line);
                if tick != last_tick {
                    last_tick = tick;
                    last_change = now;
                    reported = false;
                    continue;
                }
                let stalled = now.saturating_sub(last_change);
                if stalled >= 5_000 && !reported {
                    reported = true;
                    let popup = crate::remote::x11::POPUP_REQUESTS
                        .lock()
                        .map(|g| g.len())
                        .unwrap_or(0);
                    let text = format!("=== UI 停摆 {stalled}ms ===\npopup_pending={popup}\n{line}");
                    let _ = std::fs::write(&stall_path, text);
                    log_line(format_args!(
                        "[stall] UI 停摆 {stalled}ms ui_tick={tick} ui_phase={phase} frame_phase={fphase} last_req={freq:#06x} raster_order={raster} popup={popup}"
                    ));
                }
            }
        })
        .ok();
}

/// 廉价诊断日志：窗口生命周期/错误这类**低频**事件常开，写 `NYATERM_RDPDR_LOG`
///
/// 关键点是**缓存文件句柄**：早先的探针每条都 open+write+close，在逐 PDU 的
/// 热路径上每秒上千次同步 I/O，把输入回显都拖慢了。这里只在首次调用时打开，
/// 并限制总行数，避免客户端异常刷屏时反噬。
pub fn log_line(args: std::fmt::Arguments<'_>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, OnceLock};

    /// 上限：超过后只丢弃，宁可丢诊断也不能拖慢会话。
    const MAX_LINES: usize = 20_000;

    static WRITTEN: AtomicUsize = AtomicUsize::new(0);
    static SENDER: OnceLock<Option<mpsc::Sender<String>>> = OnceLock::new();

    if WRITTEN.load(Ordering::Relaxed) >= MAX_LINES {
        return;
    }
    let sender = SENDER.get_or_init(|| {
        let path = std::env::var_os("NYATERM_RDPDR_LOG")?;
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::Builder::new()
            .name("rustterm-log".to_string())
            .spawn(move || {
                use std::io::Write as _;
                let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                else {
                    // 打不开就持续排空，避免发送端无界累积
                    for _ in rx.iter() {}
                    return;
                };
                for line in rx.iter() {
                    let _ = file.write_all(line.as_bytes());
                }
                let _ = file.flush();
            })
            .ok()?;
        Some(tx)
    });
    let Some(sender) = sender.as_ref() else {
        return;
    };
    // **绝不阻塞调用方**：这里只做一次格式化 + 入队，磁盘 I/O 全在专用写线程。
    //
    // 历史故障形态（本改动要根除的）：原先在调用线程上持锁 write_all，磁盘一旦
    // 卡住，UI 线程（每帧 [rd-paint]）、心跳线程、X 任务全部堵在同一把锁上——
    // 表现是"整个应用无响应、日志整齐停在半路、crash.log 无 panic、进程还活着
    // 但连关窗消息都处理不了"，只能强杀，事后无法归因。
    if sender.send(format!("{args}\n")).is_ok() {
        WRITTEN.fetch_add(1, Ordering::Relaxed);
    }
}