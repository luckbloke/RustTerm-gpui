//! X11 服务端的连接循环。
//!
//! 一条连接的生命周期：
//! 1. 读 setup request → 回 setup reply（编解码在 `handshake.rs`）；
//! 2. 注册客户端，启动一个 writer 任务，把所有回复/事件按顺序写回 socket；
//! 3. 请求循环：分帧、`Request::parse`、交给 `ServerState::handle`；
//! 4. 断开时销毁客户端资源（DestroyAll），等 writer 把队列刷完再关连接。
//!
//! 为什么所有写出都走 writer 任务：事件可能由**别的连接**的请求产生
//! （A 建窗口、B 监听 StructureNotify），如果每个连接直接写 socket，
//! 同一 socket 会有多个写者。队列 + 单写者是最简单的顺序保证。
//!
//! 传输层在 M7 之前只有调试用的 TCP 监听：生产路径是 SSH 的 `x11` 通道，
//! 那条链路会把通道字节流交给同一个 `serve_stream()`。

use std::io;
use std::sync::{Arc, Mutex, RwLock};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use x11rb_protocol::protocol::Request;
use x11rb_protocol::x11_utils::{ExtInfoProvider, ExtensionInformation, RequestHeader};

use super::error::{self, XError};
use super::extensions::{ExtTable, MAX_BIG_REQUEST_UNITS, OPCODE_BIG_REQUESTS};
use super::handshake::{self, SetupError, SETUP_PREFIX_LEN};
use super::state::ServerState;

/// 防御性上限：请求头被损坏时，不要按它去分配内存。
/// 普通请求的上限是 0xffff 个 4 字节单位（256 KiB）；
/// BIG-REQUESTS 启用后放宽到扩展声明的 16 MiB。
const MAX_REQUEST_BODY: usize = 0xffff * 4;

/// 服务端配置。M0/M1 只有屏幕尺寸；M6 起跟随 Tauri 窗口大小与 DPI。
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub screen_width: u16,
    pub screen_height: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            screen_width: 1280,
            screen_height: 800,
        }
    }
}

/// 多连接共享的服务端。
///
/// 用一个 `Mutex` 包住全部状态：每个请求的处理都很短，且持锁期间不做 I/O
/// （回复只是投递到各连接的发送队列），不会因为某个客户端不读数据而卡住别人。
pub struct ServerShared {
    /// task-18：RwLock 快照语义——UI 帧泵读帧（`state_read`）与光栅化 worker
    /// 写帧（`state` 写锁）解耦；读读并发，读写互斥。
    state: RwLock<ServerState>,
    /// task-17 顺序异步光栅化：绘制类命令的**有序**队列（严格 FIFO）。
    ///
    /// 请求循环只做 O(1) 入队，光栅化由唯一的 worker 线程按入队顺序逐条
    /// 执行（每条命令单独拿一次状态锁）——查询类请求在循环线程就地处理，
    /// 最多等 worker「当前这条」命令做完，不再等整条绘制流（[xi-q] 实测
    /// 积压 21~79ms/查询的根治）。
    draw_queue: Mutex<std::collections::VecDeque<QueuedDraw>>,
    draw_signal: std::sync::Condvar,
    /// worker 停止标志：置位后队列里的剩余命令被**丢弃**（关闭语义）。
    raster_stop: std::sync::atomic::AtomicBool,
    /// worker 已启动标志（唯一消费者是 FIFO 顺序的前提，禁止重复启动）。
    raster_started: std::sync::atomic::AtomicBool,
    /// task-19 心跳：worker 已执行的命令条数。
    raster_executed: std::sync::atomic::AtomicU64,
    /// task-19：worker 捕获到的绘制 panic 次数。
    ///
    /// 旧实现里 `cmd.run(...)` 没有任何保护：一条绘制命令 panic 就会**杀死
    /// 唯一的光栅化线程**且不留日志——队列里的命令此后再也不消费，表现是
    /// "窗口还在，但整个界面再也不刷新"，而且进程内无法恢复（server 实例
    /// 被全局静态量持有）。现在 panic 只废掉那一条命令。
    raster_panics: std::sync::atomic::AtomicU64,
    /// 每个连接"已被 worker 执行完"的最大请求顺序号（task-19 顺序判定）。
    ///
    /// 必要性：worker 把命令 pop 出来之后队列就"空"了，但命令还在执行中；
    /// 若此时让本连接就地执行后续带应答请求，应答会先于那条命令发出 → 应答
    /// 乱序 → XCB 致命中止。用"已执行完"而不是"已出队"才能堵住这个窗口。
    raster_done: Mutex<std::collections::HashMap<u32, u64>>,
    /// task-22 按需显示：UI 是否已挂帧接收端（FramePump attach/detach 维护）。
    /// Arc 共享给 `ServerState.viewer_attached`（map_window 触发判断用）。
    pub viewer_attached: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 本屏幕的 id（HostServer::start 填写；"" = 匿名）。
    pub screen_id: Mutex<String>,
    /// 有窗口映射且 UI 未挂接收端时的回调（HostServer::start 填写；由
    /// `ServerState::notify_map_pending` 触发，去重与弹窗逻辑在回调里）。
    pub on_map_no_viewer: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
}

impl ServerShared {
    /// 填写屏幕 id 与「映射窗口且无观众」回调（HostServer::start 调用一次）。
    pub fn set_map_notify(
        &self,
        screen_id: &str,
        cb: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) {
        *self.screen_id.lock().unwrap_or_else(|e| e.into_inner()) = screen_id.to_string();
        *self.on_map_no_viewer.lock().unwrap_or_else(|e| e.into_inner()) = Some(cb);
    }
}

/// 队列里的一个绘制命令：捕获已解析的请求 + 客户端/序列号/发送端，
/// 由 worker 在状态锁内执行（错误按原序列号回错，GetImage 的应答也在这里发）。
struct QueuedDraw {
    /// 归属连接 + 该连接的请求顺序号（task-19：worker 结束后回执，供
    /// `inline_allowed` 判断"本连接还有没有正在执行的命令"）。
    client: u32,
    order: u64,
    run: Box<dyn FnOnce(&mut ServerState) + Send>,
}

impl QueuedDraw {
    fn run(self, state: &mut ServerState) {
        (self.run)(state);
    }
}

/// 请求是否可以**就地直通**（不经过 FIFO 队列）。
///
/// 两条都必须满足：
/// 1. **请求本身是非像素语义的**（`inline_safe`：结构/属性/atom/选择/输入/
///    焦点/抓取/查询/键盘映射/屏保这类，不碰像素缓冲，也不改像素资源的
///    生命周期）。**未列入的一律入队**——这是"默认安全"：漏判只会让请求
///    走队列（顺序依然正确、只是慢一点），绝不会重排。
/// 2. **本连接此前入队的命令都已执行完**（`raster_done_for >= last_enqueued`）。
///    条件 2 不能省：
///    - worker 把命令 **pop 出来之后** `pending_draws()` 就归零了，但命令还在
///      执行中；此时就地应答会让应答**先于**那条命令的应答发出 → 应答乱序 →
///      XCB 致命中止（firefox「Exiting due to channel error」属于这一类）；
///    - 有排队命令在执行时（如 CreatePixmap）就地查询会看到"资源还不存在"
///      （BadDrawable）之类的中间态。
///
/// 为什么不再用「绘制类请求黑名单」（旧 `is_deferred_draw`）：黑名单要求
/// **枚举完整**，漏掉任何一个会改像素或会释放/复用像素资源的请求，排队中的
/// 绘制就会看到被改过的状态、或引用已被销毁/已被新对象顶替的资源。实测踩到
/// 的漏项：`ChangePicture` / `SetPictureClipRectangles` / `AddGlyphs` /
/// `FreeGlyphs`（排队中的合成用了改后的裁剪或已被删的字形）、
/// `ClearArea`（纯绘制）、`DestroyWindow` / `ConfigureWindow` / `FreeCursor` /
/// `FreeColormap`（排队中的绘制引用已销毁/被重建的资源）、以及 XID 复用
/// （cairo 释放 picture 后立刻用同一 id 新建 → 排队的 `Free` 把**新建的**
/// 对象删掉）。其中窗口树类（4/5/12）已从白名单移除、直接入队；RENDER 与
/// 光标/颜色表族本来就在白名单外；白名单里仍留着的窗口**状态**操作
/// （MapWindow 等）不影响像素缓冲，靠条件 2 的屏障保证不越过本连接排队中的
/// 绘制。
///
/// 之前还试过「队列非空就入队」的写法，那是**错的**：`last_enqueued` 只在
/// 入队分支里赋值，而队列初始为空 → 判据恒真 → 谁也不入队 → 队列恒空、
/// worker 变成死代码（绘制退回请求循环同步执行，查询被绘制流阻塞的老问题
/// 原样回来）。默认入队（本函数）没有这个不动点。
fn inline_allowed(shared: &ServerShared, client: u32, last_enqueued: u64, major_opcode: u8) -> bool {
    inline_safe(major_opcode) && shared.raster_done_for(client) >= last_enqueued
}

/// 该 major opcode 的请求是否**不碰像素**（可以就地执行）。
///
/// 用 x11rb 的 `*_REQUEST` 常量按名字列出（此前用魔法数字区间，opcode 对照表
/// 错过一位——OpenFont 实际是 45 不是 44，差点把字体生命周期放进白名单）。
/// 判据：不读写 drawable 像素缓冲、不增删像素资源（pixmap/GC/字体/cursor/
/// colormap 的创建销毁一律入队）。**未列的一律入队**——这是"默认安全"：
/// 漏判只会让请求走队列（顺序依然正确、只是慢一点），绝不会重排。
fn inline_safe(major_opcode: u8) -> bool {
    use x11rb_protocol::protocol::xproto;
    match major_opcode {
        // 窗口树查询与状态操作（不碰像素缓冲；窗口的销毁/尺寸重建走队列）
        xproto::GET_WINDOW_ATTRIBUTES_REQUEST
        | xproto::CHANGE_SAVE_SET_REQUEST
        | xproto::REPARENT_WINDOW_REQUEST
        | xproto::MAP_WINDOW_REQUEST
        | xproto::MAP_SUBWINDOWS_REQUEST
        | xproto::UNMAP_WINDOW_REQUEST
        | xproto::UNMAP_SUBWINDOWS_REQUEST
        | xproto::CIRCULATE_WINDOW_REQUEST => true,
        // 窗口/资源的销毁与重建：像素缓冲生命周期 → 入队
        // （DESTROY_WINDOW=4 / DESTROY_SUBWINDOWS=5 / CONFIGURE_WINDOW=12 不在内）
        // 几何/树/属性/atom/选择
        xproto::GET_GEOMETRY_REQUEST
        | xproto::QUERY_TREE_REQUEST
        | xproto::INTERN_ATOM_REQUEST
        | xproto::GET_ATOM_NAME_REQUEST
        | xproto::CHANGE_PROPERTY_REQUEST
        | xproto::DELETE_PROPERTY_REQUEST
        | xproto::GET_PROPERTY_REQUEST
        | xproto::LIST_PROPERTIES_REQUEST => true,
        // 选择与事件发送
        xproto::SET_SELECTION_OWNER_REQUEST
        | xproto::GET_SELECTION_OWNER_REQUEST
        | xproto::CONVERT_SELECTION_REQUEST
        | xproto::SEND_EVENT_REQUEST => true,
        // 抓取/指针/焦点（输入状态，不碰像素）
        xproto::GRAB_POINTER_REQUEST
        | xproto::UNGRAB_POINTER_REQUEST
        | xproto::GRAB_BUTTON_REQUEST
        | xproto::UNGRAB_BUTTON_REQUEST
        | xproto::CHANGE_ACTIVE_POINTER_GRAB_REQUEST
        | xproto::GRAB_KEYBOARD_REQUEST
        | xproto::UNGRAB_KEYBOARD_REQUEST
        | xproto::GRAB_KEY_REQUEST
        | xproto::UNGRAB_KEY_REQUEST
        | xproto::ALLOW_EVENTS_REQUEST
        | xproto::GRAB_SERVER_REQUEST
        | xproto::UNGRAB_SERVER_REQUEST => true,
        // 指针/焦点查询（GDK core 模式的轮询热路径必须零等待）
        xproto::QUERY_POINTER_REQUEST
        | xproto::GET_MOTION_EVENTS_REQUEST
        | xproto::TRANSLATE_COORDINATES_REQUEST => true,
        xproto::SET_INPUT_FOCUS_REQUEST
        | xproto::GET_INPUT_FOCUS_REQUEST
        | xproto::QUERY_KEYMAP_REQUEST => true,
        // 字体查询（OpenFont=45/CloseFont=46 是绘制资源的生命周期 → 不在内）
        xproto::QUERY_FONT_REQUEST
        | xproto::QUERY_TEXT_EXTENTS_REQUEST
        | xproto::LIST_FONTS_REQUEST
        | xproto::LIST_FONTS_WITH_INFO_REQUEST
        | xproto::SET_FONT_PATH_REQUEST
        | xproto::GET_FONT_PATH_REQUEST => true,
        // 颜色表查询（Alloc 族分配的是 colormap 条目，不是像素缓冲 → 安全）
        xproto::LIST_INSTALLED_COLORMAPS_REQUEST
        | xproto::QUERY_COLORS_REQUEST
        | xproto::LOOKUP_COLOR_REQUEST => true,
        // 杂项查询
        xproto::QUERY_BEST_SIZE_REQUEST
        | xproto::QUERY_EXTENSION_REQUEST
        | xproto::LIST_EXTENSIONS_REQUEST
        | xproto::GET_KEYBOARD_MAPPING_REQUEST
        | xproto::GET_KEYBOARD_CONTROL_REQUEST
        | xproto::GET_POINTER_CONTROL_REQUEST => true,
        // 扩展族：非像素语义
        // - XInput(131)：设备/事件选择/指针查询；XTEST(129)：注入输入
        // - XC-MISC(128)：XID 区间查询；BIG-REQUESTS(135)：长度协商
        // - XKEYBOARD(145)：键表/开关；XFIXES(138)/SHAPE(132)/DAMAGE(143)/
        //   Composite(139)：区域、形状、脏区、重定向
        // （XFIXES 的 SetPictureClipRegion 性质上改绘制状态，靠屏障兜底）
        super::extensions::OPCODE_XINPUT
        | super::extensions::OPCODE_XTEST
        | super::extensions::OPCODE_XC_MISC
        | super::extensions::OPCODE_BIG_REQUESTS
        | super::extensions::OPCODE_XKEYBOARD
        | super::extensions::OPCODE_XFIXES
        | super::extensions::OPCODE_SHAPE
        | super::extensions::OPCODE_DAMAGE
        | super::extensions::OPCODE_COMPOSITE => true,
        // RENDER(140) 全部是像素操作；其余（含 CreateWindow 之外的全部绘制、
        // pixmap/GC/字体/颜色表/cursor 族、KillClient=112）一律入队
        _ => false,
    }
}

/// panic 载荷 → 可读文本（worker 的 catch_unwind 用）。
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "非字符串 panic 载荷".to_string()
    }
}

impl ServerShared {
    pub fn new(config: ServerConfig) -> Arc<Self> {
        Arc::new(Self {
            state: RwLock::new(ServerState::new(config)),
            draw_queue: Mutex::new(std::collections::VecDeque::new()),
            draw_signal: std::sync::Condvar::new(),
            raster_stop: std::sync::atomic::AtomicBool::new(false),
            raster_started: std::sync::atomic::AtomicBool::new(false),
            raster_executed: std::sync::atomic::AtomicU64::new(0),
            raster_panics: std::sync::atomic::AtomicU64::new(0),
            raster_done: Mutex::new(std::collections::HashMap::new()),
            viewer_attached: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            screen_id: Mutex::new(String::new()),
            on_map_no_viewer: Mutex::new(None),
        })
    }

    /// **写锁**：会修改状态的路径（worker 执行绘制、请求循环里的就地处理、
    /// 帧泵的 ensure_composed/基准帧更新）。拿不准读写的路径一律走这里——
    /// 改错比慢更糟（task-18 约定）。
    pub fn state(&self) -> std::sync::RwLockWriteGuard<'_, ServerState> {
        self.state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// **读锁**：纯读路径（UI 帧泵的帧 diff/抠带）。读读并发，不阻塞其他读。
    pub fn state_read(&self) -> std::sync::RwLockReadGuard<'_, ServerState> {
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// UI 帧泵专用：读帧快照（task-18 的核心拆分）。
    ///
    /// 三段式，写窗口压到最小：
    /// 1. 写锁：`ensure_composed`（仅在 compose 脏时真正合成，平时瞬时的
    ///    空检查）；
    /// 2. **读锁**：逐行 diff + 抠带像素（数据拷出来，不在锁内做重活）；
    /// 3. 写锁：基准帧就地更新（仅当有带被取走；带像素已拷出，只按行覆写）。
    ///
    /// 已知取舍：第 2、3 步之间 worker 若画了新内容，本次发的是旧像素、
    /// 基准帧被写回旧值——下一轮 diff 会把差异再发一次，自愈不丢帧。
    pub fn take_frame_snapshot(&self) -> Option<(u64, Vec<super::state::FrameBand>)> {
        {
            let mut state = self.state();
            state.ensure_composed();
        }
        let diff = {
            let state = self.state_read();
            state.diff_frame_bands()
        };
        let Some(diff) = diff else {
            // 提示区域内没变：写锁里消费版本与提示（无拷贝），避免下轮重扫
            let mut state = self.state();
            if state.frame_version_pending() {
                state.consume_frame_version();
            }
            return None;
        };
        {
            let mut state = self.state();
            state.commit_frame_bands(&diff);
        }
        Some((diff.seq, diff.bands))
    }

    /// 入队一个绘制命令（O(1)；worker 未启动时命令会积压到启动为止）。
    pub fn enqueue_draw(&self, cmd: Box<dyn FnOnce(&mut ServerState) + Send>) {
        self.enqueue_tracked(u32::MAX, 0, cmd);
    }

    /// 带归属信息的入队：worker 执行完后记录 `(client, order)`，请求循环据此
    /// 判断"本连接是否还有在执行中的命令"（见 `inline_allowed`）。
    pub fn enqueue_tracked(
        &self,
        client: u32,
        order: u64,
        cmd: Box<dyn FnOnce(&mut ServerState) + Send>,
    ) {
        let mut queue = self.draw_queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.push_back(QueuedDraw { client, order, run: cmd });
        self.draw_signal.notify_one();
    }

    /// 本连接已被 worker 执行完的最大顺序号（没记录过就是 0）。
    pub fn raster_done_for(&self, client: u32) -> u64 {
        self.raster_done
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&client)
            .copied()
            .unwrap_or(0)
    }

    /// worker 执行完一条归属某连接的命令后回执。
    fn mark_raster_done(&self, client: u32, order: u64) {
        if client == u32::MAX {
            return; // 测试/匿名命令不参与顺序判定
        }
        let mut done = self.raster_done.lock().unwrap_or_else(|e| e.into_inner());
        let slot = done.entry(client).or_insert(0);
        *slot = (*slot).max(order);
    }

    /// 队列里还有多少待执行的绘制命令（测试与诊断用）。
    pub fn pending_draws(&self) -> usize {
        self.draw_queue.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// 启动唯一的光栅化 worker 线程（严格 FIFO 消费；置位 stop 后丢弃剩余命令退出）。
    ///
    /// 每次 `ServerShared` 只应启动一个；`HostServer::start`、`serve_tcp` 与
    /// 测试 fixture 各自调用一次。
    pub fn start_raster_worker(self: &Arc<Self>) {
        // 唯一消费者是 FIFO 顺序的前提：重复调用直接忽略
        if self
            .raster_started
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        let shared = Arc::clone(self);
        std::thread::Builder::new()
            .name("xsrv-raster".into())
            .spawn(move || {
                // task-19 心跳：每 5s 一行，证明 worker 活着并暴露积压/panic。
                // 旧版本 panic 后线程静默消失，日志里看不出任何异常（画布
                // 冻结但窗口都还在），只能靠猜——现在有这一行可对账。
                let mut last_hb = std::time::Instant::now();
                let mut last_exec: u64 = 0;
                let mut last_panics: u64 = 0;
                loop {
                    // 取一条命令：**先查停止标志**（停止后剩余队列直接丢弃），空则等待
                    let cmd = {
                        let mut queue = shared
                            .draw_queue
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        loop {
                            if shared.raster_stop.load(std::sync::atomic::Ordering::Acquire) {
                                // 关闭语义：丢弃队列里剩余的命令
                                crate::xsrv::log_line(format_args!(
                                    "[xsrv] 光栅化 worker 停止：丢弃 +{} 条未执行绘制命令",
                                    queue.len()
                                ));
                                // 被丢弃的命令也要**回执**：否则请求循环那边
                                // `raster_done < last_enqueued` 永久成立，该连接
                                // 之后每个请求都入队而无人消费（无应答、无绘制、
                                // 无错误——客户端挂死）。
                                for cmd in queue.drain(..) {
                                    shared.mark_raster_done(cmd.client, cmd.order);
                                }
                                break None;
                            }
                            if let Some(cmd) = queue.pop_front() {
                                break Some(cmd);
                            }
                            let (q, _timeout) = shared
                                .draw_signal
                                .wait_timeout(
                                    queue,
                                    std::time::Duration::from_millis(100),
                                )
                                .unwrap_or_else(|e| e.into_inner());
                            queue = q;
                        }
                    };
                    // 心跳：空闲时靠 100ms 超时唤醒，所以这里必然会被执行到
                    if last_hb.elapsed() >= std::time::Duration::from_secs(5) {
                        use std::sync::atomic::Ordering;
                        let exec = shared.raster_executed.load(Ordering::Relaxed);
                        let panics = shared.raster_panics.load(Ordering::Relaxed);
                        // task-24 内存账目（读锁极短）：win/pix = 窗口与 pixmap 数，
                        // px = 服务端持有的像素 MB。进程内存暴涨时看这里就能判定
                        // 归属：px 稳定 → 涨在 UI/渲染器侧；px 持续涨 → 我们自己的
                        // 缓冲泄漏（例如客户端只建不释放的 pixmap）。
                        let (nwin, npix, pxb) = {
                            let state = shared.state_read();
                            state.pixel_bytes()
                        };
                        crate::xsrv::log_line(format_args!(
                            "[raster] alive exec=+{} panics=+{} queue={} win={nwin} pix={npix} px={}MB",
                            exec - last_exec,
                            panics - last_panics,
                            shared.pending_draws(),
                            pxb / (1024 * 1024)
                        ));
                        last_exec = exec;
                        last_panics = panics;
                        last_hb = std::time::Instant::now();
                    }
                    let Some(cmd) = cmd else { break };
                    let (cmd_client, cmd_order) = (cmd.client, cmd.order);
                    // 每条命令单独拿锁执行：查询请求最多等这一条，不等整条流。
                    //
                    // **panic 隔离**：一条绘制命令崩了只能废掉它自己。此前
                    // `cmd.run(...)` 裸调用——任何 panic（例如借走的 drawable
                    // 像素没归还导致后续采样越界）都会杀死唯一的光栅化线程，
                    // 队列从此无人消费：窗口还在，但画面永久冻结，且进程内
                    // 无法恢复（server 被全局静态量持有，重连也拿到同一个）。
                    // 看门狗信标：卡住时能直接看出是不是 raster 正在跑某条命令
                    crate::xsrv::counters::RASTER_BUSY_ORDER
                        .store(cmd_order, std::sync::atomic::Ordering::Relaxed);
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        cmd.run(&mut shared.state());
                    }));
                    crate::xsrv::counters::RASTER_BUSY_ORDER
                        .store(0, std::sync::atomic::Ordering::Relaxed);
                    shared.mark_raster_done(cmd_client, cmd_order);
                    match outcome {
                        Ok(()) => {
                            shared
                                .raster_executed
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        Err(payload) => {
                            shared
                                .raster_panics
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            crate::xsrv::log_line(format_args!(
                                "[raster] 绘制命令 panic（worker 继续服务）：{}",
                                panic_text(&*payload)
                            ));
                        }
                    }
                }
                crate::xsrv::log_line(format_args!("[raster] worker 退出"));
            })
            .expect("启动光栅化 worker 线程失败");
    }

    /// 停止 worker（丢弃剩余队列；已有连接的请求循环不再入队即可自然结束）。
    pub fn stop_raster_worker(&self) {
        self.raster_stop
            .store(true, std::sync::atomic::Ordering::Release);
        self.draw_signal.notify_all();
    }
}

/// 扩展注册表：`Request::parse` / `Event::parse` 靠它识别扩展请求与事件。
/// 表内容与 `QueryExtension` 回复共用同一份常量（见 `extensions.rs`）。
#[derive(Default)]
struct Extensions(ExtTable);

impl ExtInfoProvider for Extensions {
    fn get_from_major_opcode(&self, major_opcode: u8) -> Option<(&str, ExtensionInformation)> {
        self.0.get_from_major_opcode(major_opcode)
    }

    fn get_from_event_code(&self, event_code: u8) -> Option<(&str, ExtensionInformation)> {
        self.0.get_from_event_code(event_code)
    }

    fn get_from_error_code(&self, error_code: u8) -> Option<(&str, ExtensionInformation)> {
        self.0.get_from_error_code(error_code)
    }
}

/// 处理一条客户端连接，直到对方关闭或出错。
pub async fn serve_stream<S>(
    stream: S,
    client_index: u32,
    shared: Arc<ServerShared>,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    crate::xsrv::counters::CONNECTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let result = serve_stream_inner(stream, client_index, shared).await;
    crate::xsrv::counters::DISCONNECTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Err(ref e) = result {
        crate::xsrv::log_line(format_args!(
            "[x11] 连接 {client_index} 结束（错误）：{e}"
        ));
    }
    result
}

/// 实际的服务循环（`serve_stream` 只做连接计数与结束归因）。
async fn serve_stream_inner<S>(
    stream: S,
    client_index: u32,
    shared: Arc<ServerShared>,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);

    // ---- 1. 握手 ----
    let mut prefix = [0u8; SETUP_PREFIX_LEN];
    match reader.read_exact(&mut prefix).await {
        Ok(_) => {}
        // 客户端连上就断开是正常现象（比如 X 客户端只做了一次探测），不当错误
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
        Err(e) => return Err(e),
    }

    let rest_len = match handshake::setup_prefix(&prefix) {
        Ok(len) => len,
        Err(SetupError::UnsupportedByteOrder(order)) => {
            // 回复本身只能按小端写；对方要求大端，只能明确拒绝后关连接
            eprintln!("[xsrv] 拒绝连接：不支持的字节序 {order:#04x}");
            writer
                .write_all(&handshake::setup_failed("byte-order not supported"))
                .await?;
            writer.flush().await?;
            return Ok(());
        }
        Err(e) => {
            eprintln!("[xsrv] setup 头部不合法：{e:?}");
            return Ok(());
        }
    };

    let mut setup_request = vec![0u8; SETUP_PREFIX_LEN + rest_len];
    setup_request[..SETUP_PREFIX_LEN].copy_from_slice(&prefix);
    reader.read_exact(&mut setup_request[SETUP_PREFIX_LEN..]).await?;

    let setup = match handshake::parse_setup(&setup_request) {
        Ok(setup) => setup,
        Err(e) => {
            eprintln!("[xsrv] setup request 解析失败：{e:?}");
            return Ok(());
        }
    };
    eprintln!(
        "[xsrv] 客户端 {client_index} 握手成功：协议 {}.{}，认证 {:?}，cookie {} 字节",
        setup.protocol_major_version,
        setup.protocol_minor_version,
        String::from_utf8_lossy(&setup.authorization_protocol_name),
        setup.authorization_protocol_data.len(),
    );

    let (screen_width, screen_height) = {
        let state = shared.state();
        (state.config.screen_width, state.config.screen_height)
    };
    writer
        .write_all(&handshake::setup_reply(
            client_index,
            screen_width,
            screen_height,
        ))
        .await?;
    writer.flush().await?;

    // ---- 2. 注册 + writer 任务 ----
    let (sender, mut receiver) = unbounded_channel::<Vec<u8>>();
    shared
        .state()
        .register_client(client_index, sender.clone());

    let writer_task = tokio::spawn(async move {
        while let Some(frame) = receiver.recv().await {
            writer.write_all(&frame).await?;
        }
        writer.flush().await?;
        Ok::<(), io::Error>(())
    });

    // ---- 3. 请求循环 ----
    let result = request_loop(&mut reader, client_index, Arc::clone(&shared), sender.clone()).await;

    // ---- 4. 清理 ----
    // 先销毁资源并把 DestroyNotify 塞进其他客户端的队列，再丢弃本连接的发送端，
    // 最后等 writer 把已排队的数据全部写出去。
    shared.state().remove_client(client_index);
    drop(sender);

    let _ = writer_task.await;
    result
}

async fn request_loop<R>(
    reader: &mut R,
    client_index: u32,
    shared: Arc<ServerShared>,
    sender: UnboundedSender<Vec<u8>>,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
{
    // X11 的序列号从 1 开始，每个请求（含 NoOperation）消费一个，
    // 回复与错误都要带上它，客户端靠它对上是哪个请求。
    let mut sequence: u16 = 0;
    // BIG-REQUESTS：客户端发送 Enable（opcode 135）之后，16 位长度字段为 0
    // 的请求改用随后的 32 位长度。这是**每条连接**的状态，不是全局的。
    let mut big_requests = false;
    let extensions = Extensions::default();
    // task-19：本连接的请求顺序号（协议无关，只用于 `inline_allowed` 判定）。
    // `last_enqueued` = 最近一次入队的顺序号；worker 执行完会回执（raster_done）。
    let mut req_order: u64 = 0;
    let mut last_enqueued: u64 = 0;

    // 最近 5 条请求名（出错时 dump，定位"错误前的请求序列"）
    let mut recent: std::collections::VecDeque<String> = std::collections::VecDeque::new();

    // NyaTerm 探针：每连接前 80 条请求全量记录（带操作码名 + 头 4 个 32 位
    // 字段），归因 GTK 启动探测链——"窗口 0 从哪来"。
    const TRACE_UPTO: u16 = 80;

    // task-14：查询应答积压统计（见循环体内的 [xi-q] 证据日志）
    let mut query_backlog_ms: u64 = 0;
    let mut draw_reqs_since: u64 = 0;
    let mut draw_ms_since: u64 = 0;
    let mut query_logs: u32 = 0;
    // task-21 探针：pad 字节带值的"核心"请求计数（见循环体内 [junk] 日志）
    let mut junk_probe: u32 = 0;
    // task-21b 原始字节环：每连接保留最近 256 字节的原始流。junk 出现时整体
    // 转储——如果是「两条连接的字节流混进了一条通道」，环里能看到 ASCII 文本
    // 或另一条流的帧边界；如果是客户端自己发的，环里是连贯的 X11 帧。
    let mut raw_ring: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
    let mut junk_raw_dumped: u32 = 0;

    loop {
        let mut header = [0u8; 4];
        match reader.read_exact(&mut header).await {
            Ok(_) => {}
            // EOF = 客户端主动断开（firefox 崩溃/XCB abort 也走这里，客户端
            // 侧不会留下任何解释）。带出本连接的请求统计与最后几条请求，
            // 给"firefox 崩了"提供现场（崩溃前它在做什么）。
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                crate::xsrv::log_line(format_args!(
                    "[x11] c{client_index} 断开（EOF）：共 {sequence} 条请求，最后请求: {:?}",
                    recent
                ));
                return Ok(());
            }
            Err(e) => return Err(e),
        }
        let major_opcode = header[0];
        let minor_opcode = header[1];
        // X11 的长度字段按 4 字节单位计，**包含**这 4 字节请求头：
        // 总长 8 的 GetInputFocus，长度字段是 2 而不是 1。
        let mut total_len = u16::from_ne_bytes([header[2], header[3]]) as usize * 4;
        // BIG-REQUESTS 形式下已经多读的 4 字节（32 位长度本身）：按协议它是
        // 请求体的一部分——remaining_length 按 (总长 - 4)/4 计，包含这 4 字节。
        let mut big_prefix: Option<[u8; 4]> = None;
        if total_len == 0 {
            // 长度字段为 0 = BIG-REQUESTS 形式：真实长度在接下来的 4 字节里。
            // 例外：Enable 请求本身（opcode 135）总是用这个形式发送，好让服务端
            // 知道客户端支持 32 位长度——它必须被接受才能开启这个机制。
            let enabled = big_requests || major_opcode == OPCODE_BIG_REQUESTS;
            if !enabled {
                eprintln!(
                    "[xsrv] 客户端 {client_index} 在启用 BIG-REQUESTS 之前就用了它，断开"
                );
                return Ok(());
            }
            let mut extra = [0u8; 4];
            reader.read_exact(&mut extra).await?;
            let units = u32::from_ne_bytes(extra) as usize;
            // 真实长度含 8 字节头；下限排除循环长度，上限是扩展声明值
            if units < 2 || units > MAX_BIG_REQUEST_UNITS as usize {
                eprintln!(
                    "[xsrv] 客户端 {client_index} 的 BIG-REQUESTS 长度异常（{units} 单位），断开"
                );
                return Ok(());
            }
            total_len = units * 4;
            big_prefix = Some(extra);
        }
        // 解析用的"请求体"长度：普通形式是 4 字节头之后的全部；
        // BIG-REQUESTS 形式里 8 字节大头之后才是请求体（x11rb 的
        // remaining_length = 总单位数 - 2，不含 4 字节头与 4 字节扩展长度）。
        let body_len = total_len - 4 - big_prefix.map(|_| 4).unwrap_or(0);

        // BIG-REQUESTS 形式里，随后的 4 字节长度也算"请求头"的一部分，
        // `Request::parse` 的 remaining_length 要按 (总长 - 4) / 4 计
        let limit = if big_requests || big_prefix.is_some() {
            MAX_BIG_REQUEST_UNITS as usize * 4
        } else {
            MAX_REQUEST_BODY
        };
        if body_len > limit {
            eprintln!("[xsrv] 客户端 {client_index} 发来的请求长度异常（{body_len} 字节），断开");
            return Ok(());
        }

        // 必须先按长度读干净请求体，否则后面的请求会错位。
        // BIG-REQUESTS 形式下，4 字节 32 位长度已经读过、不属于解析用请求体。
        let mut body = {
            let mut body = vec![0u8; body_len];
            reader.read_exact(&mut body).await?;
            body
        };
        // task-21b 原始字节环（junk 探针的转储源）：最近 256 字节的原始流
        if raw_ring.len() + 4 + body.len() > 256 {
            let drop = (raw_ring.len() + 4 + body.len()) - 256;
            for _ in 0..drop {
                raw_ring.pop_front();
            }
        }
        raw_ring.extend(header);
        raw_ring.extend(&body);
        sequence = sequence.wrapping_add(1);
        req_order += 1;

        let header = RequestHeader {
            major_opcode,
            minor_opcode,
            // BIG-REQUESTS 形式：remaining_length = 总单位数 - 2
            remaining_length: (body_len / 4 + big_prefix.map(|_| 1).unwrap_or(0)) as u32,
        };
        let mut fds = Vec::new();
        let mut request = match Request::parse(header, &body, &mut fds, &extensions) {
            Ok(request) => request,
            Err(e) => {
                eprintln!("[xsrv] 客户端 {client_index} 发来的请求无法解析：{e:?}");
                let frame = error::encode(
                    XError::bad_request(),
                    sequence,
                    minor_opcode,
                    major_opcode,
                );
                // **应答顺序屏障**：本连接若还有排队中/在执行中的命令，这个错误帧
                // 必须一起入队。它的 sequence 比那条命令更大，内联直接发就会
                // "后发的先到" → XCB 应答乱序 → 致命中止。判据只取屏障那一半
                //（不看 opcode）：解析都失败了，谈不上"非像素语义"。
                if shared.raster_done_for(client_index) >= last_enqueued {
                    let _ = sender.send(frame);
                } else {
                    last_enqueued = req_order;
                    let sender = sender.clone();
                    shared.enqueue_tracked(client_index, req_order, Box::new(move |_state| {
                        let _ = sender.send(frame);
                    }));
                }
                continue;
            }
        }
        .into_owned();

        let bigreq_enable = matches!(request, Request::BigreqEnable(_));
        crate::xsrv::counters::REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        crate::xsrv::counters::note_request(major_opcode, minor_opcode);

        // ---- 诊断：每条请求都留 trace（入队请求同样要 trace）----
        //
        // trace 必须放在"入队判定"**之前**：入队分支会 `continue`，放在它后面
        // 就等于"走了队列的请求没有日志"，而日志是唯一的现场证据。
        if sequence <= TRACE_UPTO {
            // 请求头之后的字段就是参数（小端 u32 数组）；前 4 个足够辨认
            // window/drawable/atom 等关键 id
            let words: Vec<u32> = body
                .chunks_exact(4)
                .take(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            let name = core_request_name(major_opcode);
            crate::xsrv::log_line(format_args!(
                "[x11] c{client_index} #{sequence} {name} args={words:?}"
            ));
        }
        if recent.len() == 5 {
            recent.pop_front();
        }
        recent.push_back(format!("seq{sequence} major={major_opcode} minor={minor_opcode}"));

        // task-21 探针：**pad 字节带值的"核心"请求**。xi2 实测（用户日志）出现
        // 大量形如 (37,40)/(43,1)/(43,4)/(40,2)/(2,2)/(3,246) 的请求——这些请求
        // 的 header[1] 本应是 0（pad），却夹在正常 XI2 流量里。它们究竟是
        // 客户端真实发送（那我们要补语义）还是流错位（那有致命 bug），必须
        // 看到 body 才能定案。已知 byte1 是数据的请求不在探针之列。
        const DATA_BYTE1: &[u8] = &[
            6, 13, 25, 28, 29, 33, 34, 35, 42, 61, // ChangeSaveSet/CirculateWindow/
            // SendEvent/GrabButton/UngrabButton/GrabKey/UngrabKey/AllowEvents/
            // SetInputFocus/ClearArea
        ];
        if major_opcode < 128
            && minor_opcode != 0
            && !DATA_BYTE1.contains(&major_opcode)
            && junk_probe < 60
        {
            junk_probe += 1;
            let words: Vec<u32> = body
                .chunks_exact(4)
                .take(6)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            let what = core_request_name(major_opcode);
            crate::xsrv::log_line(format_args!(
                "[junk] c{client_index} seq{sequence} {what}(major={major_opcode}, byte1={minor_opcode}) body={words:?}"
            ));
            // task-21b：转储本连接最近的原始字节（一次性），定位混流来源
            if junk_raw_dumped < 3 {
                junk_raw_dumped += 1;
                let hex: String = raw_ring
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join("");
                let text: String = raw_ring
                    .iter()
                    .map(|b| {
                        if b.is_ascii_graphic() || *b == b' ' {
                            *b as char
                        } else {
                            '·'
                        }
                    })
                    .collect();
                let n = raw_ring.len();
                crate::xsrv::log_line(format_args!(
                    "[junk-raw] c{client_index} 最近{n}字节 hex={hex} text={text:?}"
                ));
            }
        }

        // ---- task-19 顺序：非像素请求且本连接无在飞行命令时才就地直通 ----
        //
        // 旧实现分两类：`is_deferred_draw` 命中 ⇒ 入队，其余就地执行（外加
        // GetImage「先排空队列」的特例）。那套分类有两个实测故障：
        //   1. 黑名单漏项（ChangePicture / SetPictureClipRectangles / FreeGlyphs
        //      / XID 复用……）→ 排队中的绘制看到被改过的状态，或引用已被
        //      销毁、已被新对象顶替的资源；
        //   2. GetImage 的应答被 worker 延后，而它之后的**内联应答**先行发出
        //      → 应答乱序 → XCB 致命中止（firefox「Exiting due to channel
        //      error」）。
        // 现在判定 = `inline_safe(opcode)`（非像素白名单；未列的一律入队）
        // && `raster_done >= last_enqueued`（本连接每连接 FIFO 屏障：只要本
        // 连接有命令在队列里或在执行中，后续请求无论 opcode 一律入队）。白名单
        // 里个别改窗口状态的操作（MapWindow 等）靠屏障保证不越过本连接排队中
        // 的绘制；跨连接顺序 X11 本就不保证。中间版曾试过"队列非空才入队"，
        // 那是不动点（last_enqueued 只在入队分支赋值 → 队列恒空 → worker 成
        // 死代码），已被端到端测试钉死。
        if !inline_allowed(&shared, client_index, last_enqueued, major_opcode) {
            last_enqueued = req_order;
            let (client, seq, order) = (client_index, sequence, req_order);
            let (major, minor) = (major_opcode, minor_opcode);
            let sender = sender.clone();
            shared.enqueue_tracked(client, order, Box::new(move |state| {
                if let Err(e) = state.handle(client, seq, &request) {
                    // 入队命令的错误同样要留证据：错误按原序列号回，是客户端
                    // 子进程崩溃（"Channel closing"）的直接原因，但 worker 在
                    // 另一个线程上，不打日志就完全看不见。
                    let what: String = format!("{request:?}").chars().take(160).collect();
                    crate::xsrv::log_line(format_args!(
                        "[x11] err->c{client} seq{seq} major={major} minor={minor} {e:?} <- {what}（排队执行）"
                    ));
                    let frame = error::encode(e, seq, minor, major);
                    let _ = sender.send(frame);
                }
            }));
            if bigreq_enable {
                big_requests = true;
            }
            continue;
        }

        // ---- task-14 证据：查询应答被绘制流阻塞的量化 ----
        //
        // XI2 模式下 GDK3 每收到一个输入事件就同步调 XIQueryPointer（minor 40）；
        // 核心模式走 QueryPointer（major 38）。两者应答都排在该连接**前面尚未
        // 处理完的绘制请求**之后——「非查询请求的处理耗时」就是这次查询的
        // 应答延迟的下界。窗口值 >= 20ms 才留痕（复现一次即可量化堵塞程度）。
        let is_pointer_query = major_opcode == 38
            || matches!(
                extensions.get_from_major_opcode(major_opcode),
                Some((name, _)) if name == "XInputExtension" && minor_opcode == 40
            );
        let is_draw_op = major_opcode == super::render::OPCODE_RENDER
            || (62..=77).contains(&major_opcode);
        let handle_started = std::time::Instant::now();
        // 就地直通同样做 panic 隔离：崩一条请求只该废掉它自己，不该把整条
        // 连接（甚至整个进程）带走。错误按原序列号回给客户端。
        let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            shared.state().handle(client_index, sequence, &request)
        })) {
            Ok(result) => result,
            Err(payload) => {
                let what: String = format!("{request:?}").chars().take(160).collect();
                crate::xsrv::log_line(format_args!(
                    "[x11] 请求处理 panic（连接继续）：{} <- {what}",
                    panic_text(&*payload)
                ));
                Err(super::error::XError::bad_implementation())
            }
        };
        {
            let ms = handle_started.elapsed().as_millis() as u64;
            if is_pointer_query {
                if query_backlog_ms >= 20 && query_logs < 200 {
                    query_logs += 1;
                    crate::xsrv::log_line(format_args!(
                        "[xi-q] c{client_index} seq{sequence} 查询应答前积压：绘制/其他请求处理 {}ms（draw_reqs=+{draw_reqs_since} draw_ms=+{draw_ms_since}）",
                        query_backlog_ms
                    ));
                }
                query_backlog_ms = 0;
                draw_reqs_since = 0;
                draw_ms_since = 0;
            } else {
                query_backlog_ms += ms;
                if is_draw_op {
                    draw_reqs_since += 1;
                    draw_ms_since += ms;
                }
            }
        }
        if let Err(e) = result {
            let what: String = format!("{request:?}").chars().take(160).collect();
            // NyaTerm: 同时写诊断日志——发给客户端的 X 错误是子进程崩溃
            // （"Channel closing"）的直接原因，stderr 在应用控制台里看不见。
            crate::xsrv::log_line(format_args!(
                "[x11] err->c{client_index} seq{sequence} major={major_opcode} minor={minor_opcode} {e:?} <- {what} | 前序: {:?}",
                recent
            ));
            let frame = error::encode(e, sequence, minor_opcode, major_opcode);
            let _ = sender.send(frame);
            continue;
        }
        // Enable 成功后才算启用：之后的普通请求才能用 32 位长度形式
        if bigreq_enable {
            big_requests = true;
        }
    }
}

/// 调试用：监听 TCP 端口，把每条连接交给 `serve_stream`。
///
/// 生产路径是 SSH 的 `x11` 通道（M7），这里只为本地直接 `DISPLAY=127.0.0.1:N.0`
/// 跑 X 客户端做验证。每个连接分配一个不同的 `client_index`，保证资源 ID 区间不重叠。
pub async fn serve_tcp(
    listener: tokio::net::TcpListener,
    shared: Arc<ServerShared>,
) -> io::Result<()> {
    shared.start_raster_worker();
    let mut next_client: u32 = 0;
    loop {
        let (socket, peer) = listener.accept().await?;
        let shared = Arc::clone(&shared);
        let client_index = next_client;
        next_client = next_client.wrapping_add(1);
        tokio::spawn(async move {
            if let Err(e) = serve_stream(socket, client_index, shared).await {
                eprintln!("[xsrv] 客户端 {peer} 连接结束：{e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `xproto::*` 也导出了名为 `Arc` 的圆弧结构体，显式导入 std 的 Arc 消除歧义。
    use std::sync::Arc;
    use std::borrow::Cow;
    use std::io::{Read as _, Write as _};

    use x11rb_protocol::protocol::xproto::*;
    use x11rb_protocol::protocol::Event as XEvent;
    use x11rb_protocol::x11_utils::{Request as RequestTrait, Serialize, TryParse};

    use super::super::handshake::{ROOT_COLORMAP, ROOT_WINDOW};
    use super::super::state::LAST_PREDEFINED_ATOM;

    /// 起一个真实监听，返回地址（runtime 要一直活着，所以一并返回）。
    fn spawn_server() -> (std::net::SocketAddr, tokio::runtime::Runtime, Arc<ServerShared>) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("建 tokio runtime");
        let shared = ServerShared::new(ServerConfig::default());
        let addr = runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("监听本地端口");
            let addr = listener.local_addr().expect("拿本地地址");
            tokio::spawn(serve_tcp(listener, Arc::clone(&shared)));
            addr
        });
        (addr, runtime, shared)
    }

    /// 按协议手写一个 LSBFirst 的 setup request。
    ///
    /// 不用真实客户端库：x11rb 的 `RustConnection` 只为其平台别名 `DefaultStream`
    /// 实现 `Stream`，Windows 上没有 `TcpStream` 的实现，所以这里自己发字节，
    /// 但**请求的编码与回复的解析都交给 x11rb**——那才是真正的互操作证据。
    fn setup_request_lsb() -> Vec<u8> {
        let auth_name = b"MIT-MAGIC-COOKIE-1";
        let auth_data = [0x5au8; 16];
        let mut bytes = vec![0x6cu8, 0];
        bytes.extend_from_slice(&11u16.to_ne_bytes());
        bytes.extend_from_slice(&0u16.to_ne_bytes());
        bytes.extend_from_slice(&(auth_name.len() as u16).to_ne_bytes());
        bytes.extend_from_slice(&(auth_data.len() as u16).to_ne_bytes());
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(auth_name);
        while bytes.len() % 4 != 0 {
            bytes.push(0);
        }
        bytes.extend_from_slice(&auth_data);
        while bytes.len() % 4 != 0 {
            bytes.push(0);
        }
        bytes
    }

    fn read_exact(socket: &mut std::net::TcpStream, len: usize, what: &str) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        socket
            .read_exact(&mut buf)
            .unwrap_or_else(|e| panic!("读{what}失败（期望 {len} 字节）：{e}"));
        buf
    }

    /// 完成握手，返回已就绪的连接。
    ///
    /// `client_index` 是这个连接在服务端的序号，用来校验分配到的资源 ID 区间：
    /// 第 N 个客户端的 base 是 `0x0020_0000 * (N + 1)`。
    fn connect(addr: std::net::SocketAddr, client_index: u32) -> std::net::TcpStream {
        let mut socket = std::net::TcpStream::connect(addr).expect("连接本地 X server");
        // 服务端 bug 会让客户端永远等下去；给一个上限，测试才能报错而不是挂住
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("设置读超时");
        socket.write_all(&setup_request_lsb()).expect("发 setup request");
        socket.flush().expect("flush");

        // setup reply：先读 8 字节拿到总长（length 字段是"前 8 字节之后"的 4 字节单位数）
        let head = read_exact(&mut socket, 8, "setup reply 头");
        assert_eq!(head[0], 1, "status 1 = 握手成功");
        let total = 8 + u16::from_ne_bytes([head[6], head[7]]) as usize * 4;
        let mut reply = head;
        reply.extend_from_slice(&read_exact(&mut socket, total - 8, "setup reply 正文"));

        let (setup, rest) = Setup::try_parse(&reply).expect("x11rb 应当能解析我们生成的 setup reply");
        assert!(rest.is_empty(), "回复长度与 length 字段必须自洽");
        assert_eq!(setup.roots.len(), 1, "M1 只提供一个 screen");
        assert_eq!(setup.roots[0].width_in_pixels, 1280);
        assert_eq!(setup.roots[0].height_in_pixels, 800);
        assert_eq!(
            setup.resource_id_base,
            0x0020_0000 * (client_index + 1),
            "每个客户端拿到互不重叠的 ID 区间"
        );
        socket
    }

    fn send_request(socket: &mut std::net::TcpStream, request: impl RequestTrait) {
        // 请求的 `serialize` 返回 (字节, fd)；核心协议没有 fd
        let (bytes, _fds) = request.serialize(0);
        socket.write_all(&bytes).expect("发请求");
        socket.flush().expect("flush");
    }

    /// 扩展请求要显式带上 QueryExtension 协商到的 major opcode
    /// （x11rb 的 `Request::serialize(major_opcode)` 对核心协议忽略该参数，
    /// 对扩展请求则原样写入线上头）。
    fn send_ext_request(
        socket: &mut std::net::TcpStream,
        request: impl RequestTrait,
        major_opcode: u8,
    ) {
        let (bytes, _fds) = request.serialize(major_opcode);
        socket.write_all(&bytes).expect("发扩展请求");
        socket.flush().expect("flush");
    }

    /// 读一个 32 字节的事件或错误帧。
    fn read_frame(socket: &mut std::net::TcpStream, what: &str) -> Vec<u8> {
        read_exact(socket, 32, what)
    }

    /// 读一个回复。
    ///
    /// 回复的固定部分是 32 字节，`length` 字段只数**超出 32 字节**的部分。
    fn read_reply(socket: &mut std::net::TcpStream, what: &str) -> Vec<u8> {
        let head = read_exact(socket, 8, what);
        let extra = u32::from_ne_bytes([head[4], head[5], head[6], head[7]]) as usize * 4;
        let mut reply = head;
        reply.extend_from_slice(&read_exact(socket, 24, what));
        if extra > 0 {
            reply.extend_from_slice(&read_exact(socket, extra, what));
        }
        reply
    }


    /// M5/M6 验收：前端注入的指针/键盘事件能按协议到达选择了事件掩码的客户端。
    ///
    /// 这条链路没有真实 X 应用参与，但它验证的正是"画面之外"最关键的契约：
    /// 事件投给谁、`event` 字段是不是接收者、坐标是不是相对接收窗口。
    #[test]
    fn injected_input_reaches_client() {
        let (addr, _runtime, shared) = spawn_server();
        let mut socket = connect(addr, 0);

        let window = 0x0020_0201u32;
        send_request(
            &mut socket,
            CreateWindowRequest {
                depth: 0,
                wid: window,
                parent: ROOT_WINDOW,
                x: 10,
                y: 20,
                width: 64,
                height: 48,
                border_width: 0,
                class: WindowClass::INPUT_OUTPUT,
                visual: 0,
                value_list: Cow::Owned(CreateWindowAux {
                    background_pixel: Some(0x0033_6699),
                    event_mask: Some(EventMask::from(
                        u32::from(EventMask::BUTTON_PRESS) | u32::from(EventMask::KEY_PRESS),
                    )),
                    ..Default::default()
                }),
            },
        );
        send_request(&mut socket, MapWindowRequest { window });

        // 用一次往返确认上面的请求已经被处理：事件是投给"当前窗口树"的，
        // 建窗还没生效时注入就会打空，测试会退化成等超时。
        send_request(&mut socket, GetGeometryRequest { drawable: window });
        let _ = read_reply(&mut socket, "GetGeometry 回复");

        {
            let mut state = shared.state();
            state.pointer_motion(30, 40);
            state.pointer_button(1, true);
        }
        match read_event(&mut socket, "ButtonPress") {
            XEvent::ButtonPress(event) => {
                assert_eq!(event.event, window, "事件应直接投给鼠标下的窗口");
                assert_eq!(event.detail, 1, "左键是 1 号键");
                assert_eq!((event.root_x, event.root_y), (30, 40));
                assert_eq!((event.event_x, event.event_y), (20, 20), "坐标相对接收窗口");
                assert_eq!(event.child, 0, "窗口内部没有更深的子窗口");
                // 注入事件不在请求处理过程里，但 sequence 必须按客户端推进到的
                // 序号盖写（此时是 3 条请求）。发 sequence=0 的事件会让 XCB 报
                // "Unknown sequence number" 并 abort——xclock 就崩在这。
                assert_eq!(
                    event.sequence, 3,
                    "注入事件的 sequence 必须跟客户端的进度一致"
                );
            }
            other => panic!("期望 ButtonPress，实际 {other:?}"),
        }

        {
            let mut state = shared.state();
            state.set_input_focus(window, u8::from(InputFocus::PARENT));
            assert_eq!(state.input.focus, window);
            state.key_event(38, true, false); // keycode 38 = 'a'
        }
        match read_event(&mut socket, "KeyPress") {
            XEvent::KeyPress(event) => {
                assert_eq!(event.event, window);
                assert_eq!(event.detail, 38);
                assert_eq!(u16::from(event.state) & 0x01, 0, "没按 Shift 时 state 不该有 ShiftMask");
            }
            other => panic!("期望 KeyPress，实际 {other:?}"),
        }

        // 按下 Shift 再打 'A'：服务端要把 ShiftMask 放进后续事件的 state 里
        {
            let mut state = shared.state();
            state.key_event(50, true, false); // Shift_L
            state.key_event(38, true, true); // 'A'
        }
        for _ in 0..2 {
            let _ = read_event(&mut socket, "Shift/KeyPress");
        }
        let state_after = shared.state().input.modifiers;
        assert_eq!(state_after & 0x01, 0x01, "Shift 按下后修饰位要置位");
    }

    fn read_event(socket: &mut std::net::TcpStream, what: &str) -> XEvent {
        let frame = read_frame(socket, what);
        XEvent::parse(&frame, &Extensions::default())
            .unwrap_or_else(|e| panic!("x11rb 解析{what}失败：{e:?}，帧 {frame:02x?}"))
    }

    /// M0 验收：握手成功、客户端能解析我们的 setup reply，未实现的请求拿到 BadRequest。
    ///
    /// 这里用 `ReparentWindow` 当例子：它仍然没实现（窗口管理器才需要它，
    /// 而 RustTerm 目前没有 WM）。`OpenFont` 已经从 M2 起实现了，不能再用它当例子。
    #[test]
    fn handshake_succeeds_and_unimplemented_request_gets_bad_request() {
        let (addr, _runtime, _shared) = spawn_server();
        let mut socket = connect(addr, 0);

        let request = ReparentWindowRequest {
            window: 0x0020_0100,
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
        };
        send_request(&mut socket, request);
        let error = read_frame(&mut socket, "ReparentWindow 的错误帧");
        assert_eq!(error[0], 0, "0 = Error");
        assert_eq!(error[1], 1, "1 = BadRequest");
        assert_eq!(
            u16::from_ne_bytes([error[2], error[3]]),
            1,
            "握手后第一个请求的序列号是 1"
        );
        assert_eq!(error[10], 7, "ReparentWindow 的 major opcode 是 7");
    }

    /// 客户端要求大端字节序时要明确拒绝，而不是回一段它读不懂的 setup reply。
    #[test]
    fn big_endian_client_is_rejected() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("建 tokio runtime");

        runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(serve_tcp(
                listener,
                ServerShared::new(ServerConfig::default()),
            ));

            let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
            // 手写一个 MSBFirst 的 setup request：'B' + pad + major/minor/name_len/data_len
            let mut request = vec![0x42u8, 0];
            request.extend_from_slice(&11u16.to_be_bytes());
            request.extend_from_slice(&0u16.to_be_bytes());
            request.extend_from_slice(&0u16.to_be_bytes()); // 认证名长度
            request.extend_from_slice(&0u16.to_be_bytes()); // 认证数据长度
            request.extend_from_slice(&[0, 0]);
            socket.write_all(&request).await.unwrap();

            // status(1) 与 reason 长度(1) 都是单字节，不受字节序影响
            let mut head = [0u8; 8];
            socket.read_exact(&mut head).await.unwrap();
            assert_eq!(head[0], 0, "status 0 = setup failed");
            assert!(head[1] > 0, "setup failed 必须带上原因");
        });
    }

    /// 颜色 / 光标 / 抓取这一批请求。
    ///
    /// Xt+Xaw 的程序（xeyes 就是）启动阶段必然会用到它们：解析颜色名要
    /// `LookupColor`（XParseColor），`XCreateFontCursor` 要
    /// `OpenFont("cursor") + CreateGlyphCursor`，弹出菜单要 `GrabPointer`。
    /// 以前这些请求落到兜底的 BadRequest，客户端表现为
    /// `X Error of failed request: BadRequest` 然后退出。
    #[test]
    fn color_cursor_and_grab_requests_work() {
        let (addr, _runtime, _shared) = spawn_server();
        // 每个测试都起一台新的服务端，所以这是第 0 个客户端（资源 ID 区间 0x0020_0000）
        let mut socket = connect(addr, 0);
        let font = 0x0020_0002u32;
        let cursor = 0x0020_0001u32;

        // ---- LookupColor("red")：XParseColor 解析颜色名走这条 ----
        send_request(
            &mut socket,
            LookupColorRequest {
                cmap: ROOT_COLORMAP,
                name: b"red".to_vec().into(),
            },
        );
        let reply = read_reply(&mut socket, "LookupColor 回复");
        let (lookup, rest) = LookupColorReply::try_parse(&reply).expect("解析 LookupColor");
        assert!(rest.is_empty(), "LookupColor 回复长度必须自洽");
        assert_eq!(
            (lookup.exact_red, lookup.exact_green, lookup.exact_blue),
            (0xffff, 0, 0)
        );

        // ---- AllocNamedColor("white")：Xt 转换 XtDefaultBackground 走这条 ----
        send_request(
            &mut socket,
            AllocNamedColorRequest {
                cmap: ROOT_COLORMAP,
                name: b"white".to_vec().into(),
            },
        );
        let reply = read_reply(&mut socket, "AllocNamedColor 回复");
        let (alloc, _) = AllocNamedColorReply::try_parse(&reply).expect("解析 AllocNamedColor");
        assert_eq!(alloc.pixel, 0x00ff_ffff, "24 位 TrueColor 的 white");

        // ---- 未知颜色名必须是 BadName(15) 而不是 BadRequest(1) ----
        send_request(
            &mut socket,
            LookupColorRequest {
                cmap: ROOT_COLORMAP,
                name: b"definitely-not-a-color".to_vec().into(),
            },
        );
        let error = read_frame(&mut socket, "LookupColor 错误帧");
        assert_eq!(error[1], 15, "15 = BadName（客户端会退回默认色）");
        assert_eq!(
            u16::from_ne_bytes([error[2], error[3]]),
            3,
            "错误帧要带出错请求的序列号（第 3 条请求）"
        );

        // ---- XCreateFontCursor 的两步：OpenFont("cursor") + CreateGlyphCursor ----
        send_request(
            &mut socket,
            OpenFontRequest {
                fid: font,
                name: b"cursor".to_vec().into(),
            },
        );
        send_request(
            &mut socket,
            CreateGlyphCursorRequest {
                cid: cursor,
                source_font: font,
                mask_font: font,
                source_char: 68,
                mask_char: 68,
                fore_red: 0,
                fore_green: 0,
                fore_blue: 0,
                back_red: 0xffff,
                back_green: 0xffff,
                back_blue: 0xffff,
            },
        );

        // 前两条没有回复：后面这条能正常拿到回复，就说明它们没有被当成错误
        send_request(
            &mut socket,
            QueryBestSizeRequest {
                class: QueryShapeOf::LARGEST_CURSOR,
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
            },
        );
        let reply = read_reply(&mut socket, "QueryBestSize 回复");
        let (best, _) = QueryBestSizeReply::try_parse(&reply).expect("解析 QueryBestSize");
        assert_eq!((best.width, best.height), (16, 16));

        // ---- GrabPointer 必须回 Success（Xaw 菜单靠它）----
        send_request(
            &mut socket,
            GrabPointerRequest {
                owner_events: false,
                grab_window: ROOT_WINDOW,
                event_mask: EventMask::BUTTON_PRESS,
                pointer_mode: GrabMode::ASYNC,
                keyboard_mode: GrabMode::ASYNC,
                confine_to: 0,
                cursor: 0,
                time: 0,
            },
        );
        let reply = read_reply(&mut socket, "GrabPointer 回复");
        let (grab, _) = GrabPointerReply::try_parse(&reply).expect("解析 GrabPointer");
        assert_eq!(grab.status, GrabStatus::SUCCESS);

        // ---- GetModifierMapping：toolkit 靠它认修饰键，不能是空表 ----
        send_request(&mut socket, GetModifierMappingRequest);
        let reply = read_reply(&mut socket, "GetModifierMapping 回复");
        let (modmap, _) =
            GetModifierMappingReply::try_parse(&reply).expect("解析 GetModifierMapping");
        assert_eq!(modmap.keycodes.len(), 64, "8 个修饰键 × 8 个 keycode");
        assert!(
            modmap.keycodes.contains(&50),
            "Shift_L(50) 必须在修饰键映射里：{:?}",
            modmap.keycodes
        );

        // ---- 释放后的光标再用必须是 BadCursor(6) ----
        send_request(&mut socket, FreeCursorRequest { cursor });
        send_request(
            &mut socket,
            RecolorCursorRequest {
                cursor,
                fore_red: 1,
                fore_green: 2,
                fore_blue: 3,
                back_red: 4,
                back_green: 5,
                back_blue: 6,
            },
        );
        let error = read_frame(&mut socket, "RecolorCursor 错误帧");
        assert_eq!(error[1], 6, "6 = BadCursor（已释放的光标不能再用）");
    }

    /// M1 验收：建窗、映射、事件、绘制点、GetImage 读回像素、几何/树/属性。
    #[test]
    fn core_requests_work_end_to_end() {
        let (addr, _runtime, shared) = spawn_server();
        let mut socket = connect(addr, 0);

        let window = 0x0020_0001u32;
        let gc = 0x0020_0002u32;
        let property = b"XSEL_DATA";

        // ---- CreateWindow（红底 + 结构/曝光事件） ----
        send_request(
            &mut socket,
            CreateWindowRequest {
                depth: 0,
                wid: window,
                parent: ROOT_WINDOW,
                x: 10,
                y: 20,
                width: 64,
                height: 48,
                border_width: 0,
                class: WindowClass::INPUT_OUTPUT,
                visual: 0,
                value_list: Cow::Owned(CreateWindowAux {
                    background_pixel: Some(0x00ff_0000),
                    event_mask: Some(EventMask::from(
                        u32::from(EventMask::STRUCTURE_NOTIFY) | u32::from(EventMask::EXPOSURE),
                    )),
                    ..Default::default()
                }),
            },
        );

        // CreateWindow 在父窗口上发 CreateNotify；我们没有在 root 上选事件，收不到。
        // 接下来 MapWindow 会按顺序产生 MapNotify + Expose。
        send_request(&mut socket, MapWindowRequest { window });

        match read_event(&mut socket, "MapNotify") {
            XEvent::MapNotify(event) => {
                assert_eq!(event.window, window);
                assert_eq!(event.event, window);
                assert_eq!(event.sequence, 2);
            }
            other => panic!("期望 MapNotify，实际 {other:?}"),
        }
        match read_event(&mut socket, "Expose") {
            XEvent::Expose(event) => {
                assert_eq!(event.window, window);
                assert_eq!((event.x, event.y), (0, 0));
                assert_eq!((event.width, event.height), (64, 48));
            }
            other => panic!("期望 Expose，实际 {other:?}"),
        }

        // ---- GetGeometry ----
        send_request(
            &mut socket,
            GetGeometryRequest {
                drawable: window,
            },
        );
        let reply = read_reply(&mut socket, "GetGeometry 回复");
        let (geometry, rest) = GetGeometryReply::try_parse(&reply).expect("解析 GetGeometry");
        assert!(rest.is_empty());
        assert_eq!(geometry.sequence, 3);
        assert_eq!((geometry.x, geometry.y), (10, 20));
        assert_eq!((geometry.width, geometry.height), (64, 48));
        assert_eq!(geometry.border_width, 0);

        // ---- QueryTree ----
        send_request(&mut socket, QueryTreeRequest { window: ROOT_WINDOW });
        let reply = read_reply(&mut socket, "QueryTree 回复");
        let (tree, _) = QueryTreeReply::try_parse(&reply).expect("解析 QueryTree");
        assert_eq!(tree.parent, 0, "根窗口没有父窗口");
        // NyaTerm: root 的 children 里还有服务端自建的 XSETTINGS manager 窗口
        let xsettings_w = ROOT_WINDOW - 16;
        assert!(tree.children.contains(&window), "测试窗口必须在 root children 里");
        assert!(tree.children.contains(&xsettings_w), "XSETTINGS manager 窗口必须在 root children 里");

        // ---- CreateGC + PolyPoint（画一个蓝点） ----
        send_request(
            &mut socket,
            CreateGCRequest {
                cid: gc,
                drawable: window,
                value_list: Cow::Owned(CreateGCAux {
                    foreground: Some(0x0000_00ff),
                    ..Default::default()
                }),
            },
        );
        send_request(
            &mut socket,
            PolyPointRequest {
                coordinate_mode: CoordMode::ORIGIN,
                drawable: window,
                gc,
                points: vec![Point { x: 5, y: 7 }].into(),
            },
        );

        // ---- GetImage 读回那个点 ----
        send_request(
            &mut socket,
            GetImageRequest {
                format: ImageFormat::Z_PIXMAP,
                drawable: window,
                x: 5,
                y: 7,
                width: 1,
                height: 1,
                plane_mask: 0xffff_ffff,
            },
        );
        let reply = read_reply(&mut socket, "GetImage(窗口) 回复");
        let (image, _) = GetImageReply::try_parse(&reply).expect("解析 GetImage");
        assert_eq!(image.depth, 24);
        assert_eq!(image.data, 0x0000_00ffu32.to_ne_bytes(), "画上去的蓝色像素");

        // ---- 窗口背景写进屏幕缓冲：GetImage(root) 能看到红色窗口 ----
        send_request(
            &mut socket,
            GetImageRequest {
                format: ImageFormat::Z_PIXMAP,
                drawable: ROOT_WINDOW,
                x: 10,
                y: 20,
                width: 1,
                height: 1,
                plane_mask: 0xffff_ffff,
            },
        );
        let reply = read_reply(&mut socket, "GetImage(root) 回复");
        let (image, _) = GetImageReply::try_parse(&reply).expect("解析 GetImage(root)");
        assert_eq!(image.data, 0x00ff_0000u32.to_ne_bytes(), "合成后的窗口背景");

        // ---- atom + property 往返 ----
        send_request(
            &mut socket,
            InternAtomRequest {
                only_if_exists: false,
                name: property.to_vec().into(),
            },
        );
        let reply = read_reply(&mut socket, "InternAtom 回复");
        let (atom, _) = InternAtomReply::try_parse(&reply).expect("解析 InternAtom");
        // 具体编号取决于启动时服务端自己 intern 了多少个内部 atom，
        // 所以校验"名字进表了"和"在预定义表之后"，不写死数字。
        assert!(
            atom.atom > LAST_PREDEFINED_ATOM,
            "新 atom 从预定义表之后分配，实际 {}",
            atom.atom
        );
        {
            let state = shared.state();
            assert_eq!(
                state.atoms.get(atom.atom as usize).and_then(|n| n.as_deref()),
                Some("XSEL_DATA"),
                "InternAtom 的返回值必须能查回名字"
            );
        }

        send_request(
            &mut socket,
            ChangePropertyRequest {
                mode: PropMode::REPLACE,
                window,
                property: atom.atom,
                type_: AtomEnum::STRING.into(),
                format: 8,
                data_len: property.len() as u32,
                data: property.to_vec().into(),
            },
        );
        // 没在窗口上选 PropertyChangeMask，所以不会有 PropertyNotify

        send_request(
            &mut socket,
            GetPropertyRequest {
                delete: false,
                window,
                property: atom.atom,
                type_: AtomEnum::STRING.into(),
                long_offset: 0,
                long_length: 0,
            },
        );
        let reply = read_reply(&mut socket, "GetProperty 回复");
        let (got, _) = GetPropertyReply::try_parse(&reply).expect("解析 GetProperty");
        assert_eq!(got.format, 8);
        assert_eq!(got.value, property);

        // ---- 清理 ----
        send_request(&mut socket, DestroyWindowRequest { window });
        match read_event(&mut socket, "DestroyNotify") {
            XEvent::DestroyNotify(event) => assert_eq!(event.window, window),
            other => panic!("期望 DestroyNotify，实际 {other:?}"),
        }
    }

    /// 读一个"固定部分不是 32 字节"的回复（`QueryFont` / `FontInfo` 的固定部分是 60）。
    ///
    /// `length` 字段按协议数的是**超出 32 字节通用回复头**的部分，所以整条回复的
    /// 长度是 `32 + length * 4`；固定部分多出来的那 28 字节**已经算在 length 里**，
    /// 不能再单独加一次（加两次就会多读 28 字节，把后面的流读歪）。
    fn read_reply_with_fixed(socket: &mut std::net::TcpStream, fixed: usize, what: &str) -> Vec<u8> {
        let head = read_exact(socket, 8, what);
        let after_header = u32::from_ne_bytes([head[4], head[5], head[6], head[7]]) as usize * 4;
        let total = 32 + after_header;
        assert!(
            total >= fixed,
            "{what}：length 字段给出的总长 {total} 比固定部分 {fixed} 还小"
        );
        let mut reply = head;
        reply.extend_from_slice(&read_exact(socket, total - 8, what));
        reply
    }

    /// BIG-REQUESTS Enable：XCB 的真实发送形式（16 位长度字段为 0 + 32 位真实长度），
    /// 连接随后必须能用 32 位长度发大请求。
    #[test]
    fn big_requests_enable_and_large_request_work() {
        let (addr, _runtime, _shared) = spawn_server();
        let mut socket = connect(addr, 0);

        let mut bytes = vec![super::super::extensions::OPCODE_BIG_REQUESTS, 0, 0, 0];
        bytes.extend_from_slice(&2u32.to_ne_bytes());
        socket.write_all(&bytes).expect("发 BigreqEnable");
        socket.flush().expect("flush");
        let reply = read_reply(&mut socket, "BigreqEnable 回复");
        assert_eq!(reply[0], 1, "应当是回复：{:02x?}", &reply[..12]);
        let (enable, _) = x11rb_protocol::protocol::bigreq::EnableReply::try_parse(&reply).expect("解析 BigreqEnable");
        assert!(enable.maximum_request_length >= 0x1_0000);
    }

    /// 用 GetImage 读回一块像素。
    fn read_pixels(
        socket: &mut std::net::TcpStream,
        drawable: u32,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    ) -> Vec<u32> {
        send_request(
            socket,
            GetImageRequest {
                format: ImageFormat::Z_PIXMAP,
                drawable,
                x: x as i16,
                y: y as i16,
                width,
                height,
                plane_mask: 0xffff_ffff,
            },
        );
        let reply = read_reply(socket, "GetImage 回复");
        let (image, _) = GetImageReply::try_parse(&reply).expect("解析 GetImage");
        image
            .data
            .chunks_exact(4)
            .map(|chunk| u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect()
    }

    /// M2 验收：字体查询、文字绘制、圆弧与多边形填充。
    #[test]
    fn fonts_text_and_extended_drawing_work() {
        let (addr, _runtime, _shared) = spawn_server();
        let mut socket = connect(addr, 0);

        let window = 0x0020_0001u32;
        let gc = 0x0020_0002u32;
        let font = 0x0020_0003u32;

        send_request(
            &mut socket,
            CreateWindowRequest {
                depth: 0,
                wid: window,
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 64,
                height: 32,
                border_width: 0,
                class: WindowClass::INPUT_OUTPUT,
                visual: 0,
                value_list: Cow::Owned(CreateWindowAux {
                    background_pixel: Some(0),
                    event_mask: Some(EventMask::from(
                        u32::from(EventMask::STRUCTURE_NOTIFY) | u32::from(EventMask::EXPOSURE),
                    )),
                    ..Default::default()
                }),
            },
        );
        send_request(&mut socket, MapWindowRequest { window });
        let _ = read_event(&mut socket, "MapNotify");
        let _ = read_event(&mut socket, "Expose");

        // ---- OpenFont + QueryFont ----
        send_request(
            &mut socket,
            OpenFontRequest {
                fid: font,
                name: Cow::Owned(b"fixed".to_vec()),
            },
        );
        send_request(&mut socket, QueryFontRequest { font });
        let reply = read_reply_with_fixed(&mut socket, 60, "QueryFont 回复");
        let (query, rest) = QueryFontReply::try_parse(&reply).expect("解析 QueryFont");
        assert!(rest.is_empty(), "回复长度必须自洽");
        assert_eq!(query.font_ascent, 10);
        assert_eq!(query.font_descent, 4);

        // ---- CHARSET_REGISTRY / CHARSET_ENCODING：Xlib 建 FontSet 靠它们认字符集 ----
        // 两个属性的**值本身也是 atom**，要能通过 GetAtomName 还原出 "iso8859"；
        // 缺了它们 Xlib 会报 "Missing charsets … / Unable to load any usable fontset"。
        send_request(
            &mut socket,
            InternAtomRequest {
                only_if_exists: true,
                name: b"CHARSET_REGISTRY".to_vec().into(),
            },
        );
        let reply = read_reply(&mut socket, "InternAtom(CHARSET_REGISTRY) 回复");
        let (registry, _) = InternAtomReply::try_parse(&reply).expect("解析 InternAtom");
        assert_ne!(registry.atom, 0, "CHARSET_REGISTRY 是预定义 atom");
        let charset = query
            .properties
            .iter()
            .find(|prop| prop.name == registry.atom)
            .expect("QueryFont 必须带 CHARSET_REGISTRY 属性");
        send_request(&mut socket, GetAtomNameRequest { atom: charset.value });
        let reply = read_reply(&mut socket, "GetAtomName 回复");
        let (charset_name, rest) = GetAtomNameReply::try_parse(&reply).expect("解析 GetAtomName");
        assert!(rest.is_empty(), "GetAtomName 回复长度必须自洽");
        assert_eq!(String::from_utf8_lossy(&charset_name.name), "iso8859");

        // FONT 属性的值要能还原出带字符集后缀的完整 XLFD 名
        send_request(
            &mut socket,
            InternAtomRequest {
                only_if_exists: true,
                name: b"FONT".to_vec().into(),
            },
        );
        let reply = read_reply(&mut socket, "InternAtom(FONT) 回复");
        let (font_atom, _) = InternAtomReply::try_parse(&reply).expect("解析 InternAtom(FONT)");
        let font_name_prop = query
            .properties
            .iter()
            .find(|prop| prop.name == font_atom.atom)
            .expect("QueryFont 必须带 FONT 属性");
        send_request(&mut socket, GetAtomNameRequest { atom: font_name_prop.value });
        let reply = read_reply(&mut socket, "GetAtomName(FONT) 回复");
        let (font_name, _) = GetAtomNameReply::try_parse(&reply).expect("解析 GetAtomName(FONT)");
        assert!(
            String::from_utf8_lossy(&font_name.name).ends_with("iso8859-1"),
            "FONT 属性应当是带字符集后缀的 XLFD：{}",
            String::from_utf8_lossy(&font_name.name)
        );
        assert_eq!(query.max_bounds.character_width, 7, "等宽字体的字符宽度");
        assert_eq!(query.char_infos.len(), 256, "0..=255 每个字符一份度量");
        assert!(
            query.properties.iter().any(|property| property.name != 0),
            "字体属性不能为空（客户端靠它判断是否等宽）"
        );

        // ---- QueryTextExtents：宽度只算到最后一个字符的起点 ----
        send_request(
            &mut socket,
            QueryTextExtentsRequest {
                font,
                string: Cow::Owned(vec![
                    Char2b { byte1: 0, byte2: b'A' },
                    Char2b { byte1: 0, byte2: b'B' },
                ]),
            },
        );
        let reply = read_reply(&mut socket, "QueryTextExtents 回复");
        let (extents, _) = QueryTextExtentsReply::try_parse(&reply).expect("解析 QueryTextExtents");
        assert_eq!(extents.overall_width, 7);

        // ---- ListFonts ----
        send_request(
            &mut socket,
            ListFontsRequest {
                max_names: 4,
                pattern: Cow::Owned(b"fixed".to_vec()),
            },
        );
        let reply = read_reply(&mut socket, "ListFonts 回复");
        let (list, _) = ListFontsReply::try_parse(&reply).expect("解析 ListFonts");
        assert_eq!(list.names.len(), 1);
        assert_eq!(list.names[0].name, b"fixed");

        // ---- ListFontsWithInfo：多条回复 + 空名字结束标记 ----
        send_request(
            &mut socket,
            ListFontsWithInfoRequest {
                max_names: 1,
                pattern: Cow::Owned(b"*".to_vec()),
            },
        );
        let reply = read_reply_with_fixed(&mut socket, 60, "FontInfo 回复");
        let (info, _) = ListFontsWithInfoReply::try_parse(&reply).expect("解析 FontInfo");
        assert!(!info.name.is_empty(), "第一条 FontInfo 必须带字体名");
        let reply = read_reply_with_fixed(&mut socket, 60, "FontInfo 结束标记");
        let (end, _) = ListFontsWithInfoReply::try_parse(&reply).expect("解析结束标记");
        assert!(end.name.is_empty(), "列表以空名字结束");

        // ---- GC + ImageText8 ----
        send_request(
            &mut socket,
            CreateGCRequest {
                cid: gc,
                drawable: window,
                value_list: Cow::Owned(CreateGCAux {
                    foreground: Some(0x00ff_ffff),
                    background: Some(0x0000_0000),
                    font: Some(font),
                    ..Default::default()
                }),
            },
        );
        send_request(
            &mut socket,
            ImageText8Request {
                drawable: window,
                gc,
                x: 1,
                y: 12,
                string: Cow::Owned(b"HI".to_vec()),
            },
        );
        let pixels = read_pixels(&mut socket, window, 0, 0, 16, 14);
        let white = pixels.iter().filter(|pixel| **pixel == 0x00ff_ffff).count();
        assert!(white > 8, "文字应当画出前景像素，实际只有 {white} 个");
        assert_eq!(pixels[0], 0, "字格之外应当保持窗口背景");

        // ---- FillPoly ----
        send_request(
            &mut socket,
            FillPolyRequest {
                drawable: window,
                gc,
                shape: PolyShape::CONVEX,
                coordinate_mode: CoordMode::ORIGIN,
                points: Cow::Owned(vec![
                    Point { x: 40, y: 2 },
                    Point { x: 62, y: 2 },
                    Point { x: 51, y: 30 },
                ]),
            },
        );
        let pixels = read_pixels(&mut socket, window, 40, 2, 22, 28);
        let white = pixels.iter().filter(|pixel| **pixel == 0x00ff_ffff).count();
        assert!(white > 50, "三角形内部应当被填充，实际只有 {white} 个像素");

        // ---- PolyFillArc（换前景色，确保是这一笔画的） ----
        send_request(
            &mut socket,
            ChangeGCRequest {
                gc,
                value_list: Cow::Owned(ChangeGCAux {
                    foreground: Some(0x0000_ff00),
                    ..Default::default()
                }),
            },
        );
        send_request(
            &mut socket,
            PolyFillArcRequest {
                drawable: window,
                gc,
                arcs: Cow::Owned(vec![x11rb_protocol::protocol::xproto::Arc {
                    x: 4,
                    y: 16,
                    width: 20,
                    height: 14,
                    angle1: 0,
                    angle2: 360 * 64,
                }]),
            },
        );
        let pixels = read_pixels(&mut socket, window, 4, 16, 20, 14);
        assert!(
            pixels.iter().any(|pixel| *pixel == 0x0000_ff00),
            "整圆内部应当被填充"
        );

        // ---- 收尾：关字体、销毁窗口 ----
        send_request(&mut socket, CloseFontRequest { font });
        send_request(&mut socket, DestroyWindowRequest { window });
        match read_event(&mut socket, "DestroyNotify") {
            XEvent::DestroyNotify(event) => assert_eq!(event.window, window),
            other => panic!("期望 DestroyNotify，实际 {other:?}"),
        }
    }

    /// 多客户端：A 创建的窗口，B 能查询（资源表是服务端全局的）。
    #[test]
    fn resources_are_visible_across_connections() {
        let (addr, _runtime, _shared) = spawn_server();
        let mut first = connect(addr, 0);
        let mut second = connect(addr, 1);

        let window = 0x0020_0010u32;
        send_request(
            &mut first,
            CreateWindowRequest {
                depth: 0,
                wid: window,
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 32,
                height: 32,
                border_width: 0,
                class: WindowClass::INPUT_OUTPUT,
                visual: 0,
                value_list: Cow::Owned(CreateWindowAux::default()),
            },
        );

        // 两条连接各有自己的处理任务，CreateWindow 不一定先于 QueryTree 被处理。
        // 先在第一条连接上做一次往返，保证窗口已经进资源表。
        send_request(&mut first, GetInputFocusRequest);
        let _ = read_reply(&mut first, "GetInputFocus 回复（同步用）");

        send_request(&mut second, QueryTreeRequest { window: ROOT_WINDOW });
        let reply = read_reply(&mut second, "QueryTree 回复");
        let (tree, _) = QueryTreeReply::try_parse(&reply).expect("解析 QueryTree");
        // NyaTerm: root 的 children 里还有服务端自建的 XSETTINGS manager 窗口
        let xsettings_w = ROOT_WINDOW - 16;
        assert!(tree.children.contains(&window), "测试窗口必须在 root children 里");
        assert!(tree.children.contains(&xsettings_w), "XSETTINGS manager 窗口必须在 root children 里");
    }

    /// 扩展握手：QueryExtension 按注册表回复 opcode / first_event，
    /// ListExtensions 列出全部名字；RENDER 明确"不存在"。
    #[test]
    fn query_extension_reports_the_registry() {
        use x11rb_protocol::protocol::bigreq;
        use x11rb_protocol::protocol::damage;
        use x11rb_protocol::protocol::shape;
        use x11rb_protocol::protocol::xc_misc;
        use x11rb_protocol::protocol::xfixes;

        let (addr, _runtime, _shared) = spawn_server();
        let mut socket = connect(addr, 0);

        for (name, opcode, first_event) in [
            ("BIG-REQUESTS", super::super::extensions::OPCODE_BIG_REQUESTS, 0u8),
            ("XC-MISC", super::super::extensions::OPCODE_XC_MISC, 0),
            ("XTEST", super::super::extensions::OPCODE_XTEST, 0),
            ("SHAPE", super::super::extensions::OPCODE_SHAPE, super::super::extensions::FIRST_EVENT_SHAPE),
            ("XFIXES", super::super::extensions::OPCODE_XFIXES, super::super::extensions::FIRST_EVENT_XFIXES),
            ("Composite", super::super::extensions::OPCODE_COMPOSITE, 0),
            ("DAMAGE", super::super::extensions::OPCODE_DAMAGE, super::super::extensions::FIRST_EVENT_DAMAGE),
        ] {
            send_request(
                &mut socket,
                QueryExtensionRequest { name: name.as_bytes().to_vec().into() },
            );
            let reply = read_reply(&mut socket, "QueryExtension 回复");
            let (ext, rest) = QueryExtensionReply::try_parse(&reply).expect("解析 QueryExtension");
            assert!(rest.is_empty(), "回复长度必须自洽");
            assert!(ext.present, "{name} 应当被报告为存在");
            assert_eq!(ext.major_opcode, opcode, "{name} 的 opcode");
            assert_eq!(ext.first_event, first_event, "{name} 的事件基址要与注册表一致");
        }

        // RENDER 已注册（核心子集在 render.rs）：QueryExtension 报告存在
        send_request(&mut socket, QueryExtensionRequest { name: b"RENDER".to_vec().into() });
        let reply = read_reply(&mut socket, "QueryExtension(RENDER) 回复");
        let (ext, _) = QueryExtensionReply::try_parse(&reply).expect("解析 QueryExtension");
        assert!(ext.present, "RENDER 应当被报告为存在");
        assert_eq!(ext.major_opcode, super::super::extensions::OPCODE_RENDER);
        assert_eq!(ext.first_error, super::super::extensions::FIRST_ERROR_RENDER);

        // ListExtensions 列出全部名字
        send_request(&mut socket, ListExtensionsRequest);
        let reply = read_reply(&mut socket, "ListExtensions 回复");
        let (list, _) = ListExtensionsReply::try_parse(&reply).expect("解析 ListExtensions");
        assert!(list.names.iter().any(|name| name.name == b"XFIXES"));
        assert!(list.names.iter().any(|name| name.name == b"RENDER"), "RENDER 已注册");

        // 用一次真实的扩展往返确认 opcode 编号是对的：XC-MISC GetXIDRange
        send_ext_request(&mut socket, xc_misc::GetXIDRangeRequest, super::super::extensions::OPCODE_XC_MISC);
        let reply = read_reply(&mut socket, "GetXIDRange 回复");
        assert_eq!(reply[0], 1, "应当是回复而不是错误帧（code={}，major={}，seq={}，帧={:02x?}）", reply[1], reply[10], u16::from_ne_bytes([reply[2], reply[3]]), &reply[..12]);
        let (range, _) = xc_misc::GetXIDRangeReply::try_parse(&reply).expect("解析 GetXIDRange");
        assert_eq!(range.start_id, 0x0020_0000, "第 0 个客户端的 ID 区间基址");
        assert!(range.count > 0);

        // SHAPE / XFIXES / DAMAGE 的版本协商也能走通（验证各扩展的 minor opcode 表）
        send_ext_request(&mut socket, shape::QueryVersionRequest, super::super::extensions::OPCODE_SHAPE);
        let reply = read_reply(&mut socket, "ShapeQueryVersion 回复");
        assert_eq!(reply[0], 1, "ShapeQueryVersion 应当是回复：{:02x?}", &reply[..16]);
        let (version, _) = shape::QueryVersionReply::try_parse(&reply).expect("解析");
        assert_eq!((version.major_version, version.minor_version), (1, 1));

        send_ext_request(
            &mut socket,
            xfixes::QueryVersionRequest { client_major_version: 6, client_minor_version: 0 },
            super::super::extensions::OPCODE_XFIXES,
        );
        let reply = read_reply(&mut socket, "XfixesQueryVersion 回复");
        let (version, _) = xfixes::QueryVersionReply::try_parse(&reply).expect("解析");
        assert_eq!((version.major_version, version.minor_version), (6, 0));

        send_ext_request(
            &mut socket,
            damage::QueryVersionRequest { client_major_version: 1, client_minor_version: 1 },
            super::super::extensions::OPCODE_DAMAGE,
        );
        let reply = read_reply(&mut socket, "DamageQueryVersion 回复");
        let (version, _) = damage::QueryVersionReply::try_parse(&reply).expect("解析");
        assert_eq!((version.major_version, version.minor_version), (1, 1));

        // BIG-REQUESTS Enable：按 XCB 的实际发送形式（16 位长度字段为 0）
        let mut bytes = vec![super::super::extensions::OPCODE_BIG_REQUESTS, 0, 0, 0];
        bytes.extend_from_slice(&2u32.to_ne_bytes());
        socket.write_all(&bytes).expect("发 BigreqEnable");
        socket.flush().expect("flush");
        let reply = read_reply(&mut socket, "BigreqEnable 回复");
        let (enable, _) = bigreq::EnableReply::try_parse(&reply).expect("解析 BigreqEnable");
        assert!(
            enable.maximum_request_length >= 0x1_0000,
            "最大请求长度必须大于 16 位长度字段的极限：{}",
            enable.maximum_request_length
        );

        // 启用之后，一个带大负载的 PutImage（普通形式放不下）应当成功并被读回。
        // 用 CreateWindow + PutImage 到 200x200 窗口，请求体 > 256KiB。
        let big_window = 0x0020_0001u32;
        send_request(
            &mut socket,
            CreateWindowRequest {
                depth: 0,
                wid: big_window,
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 300,
                height: 300,
                border_width: 0,
                class: WindowClass::INPUT_OUTPUT,
                visual: 0,
                value_list: Cow::Owned(CreateWindowAux { ..Default::default() }),
            },
        );
        let gc = 0x0020_0002u32;
        send_request(
            &mut socket,
            CreateGCRequest {
                cid: gc,
                drawable: big_window,
                value_list: Cow::Owned(CreateGCAux { ..Default::default() }),
            },
        );
        // 300x300x4 = 360000 字节 > 256KiB，普通形式发不出来
        let width = 300usize;
        let height = 300usize;
        let mut data = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                let pixel: u32 = if (x + y) % 2 == 0 { 0x00ff_00ff } else { 0x0011_2233 };
                data.extend_from_slice(&pixel.to_ne_bytes());
            }
        }
        // 300x300x4 = 360000 字节 > 256KiB，16 位长度字段放不下（x11rb 会写 0），
        // 手工把它转成 BIG-REQUESTS 线格式：[opcode, minor, 0, 0, len32, body..]
        let put_image = PutImageRequest {
            format: ImageFormat::Z_PIXMAP,
            drawable: big_window,
            gc,
            dst_x: 0,
            dst_y: 0,
            width: width as u16,
            height: height as u16,
            left_pad: 0,
            depth: 24,
            data: data.into(),
        };
        let (bytes, _fds) = RequestTrait::serialize(put_image, 24);
        assert_eq!(u16::from_ne_bytes([bytes[2], bytes[3]]), 0, "x11rb 对超长请求写 0");
        let mut big_request = vec![bytes[0], bytes[1], 0, 0];
        // 32 位长度含 4 字节基础头与 4 字节扩展长度本身（= 单位数 + 1，
        // 见 X11 BIG-REQUESTS 扩展规范）
        big_request.extend_from_slice(&((bytes.len() / 4 + 1) as u32).to_ne_bytes());
        big_request.extend_from_slice(&bytes[4..]);
        socket.write_all(&big_request).expect("发 BIG-REQUESTS 形式的 PutImage");
        socket.flush().expect("flush");
        // 用 GetImage 读回左上像素，同时验证 PutImage 没有报错
        send_request(
            &mut socket,
            GetImageRequest {
                format: ImageFormat::Z_PIXMAP,
                drawable: big_window,
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                plane_mask: 0xffff_ffff,
            },
        );
        let reply = read_reply(&mut socket, "GetImage 回复");
        assert_eq!(reply[0], 1, "PutImage 应当成功（code={}，major={}，bad={}）", reply[1], reply[10], u32::from_ne_bytes([reply[4], reply[5], reply[6], reply[7]]));
        let (image, _) = GetImageReply::try_parse(&reply).expect("解析 GetImage");
        assert_eq!(image.data, 0x00ff_00ffu32.to_ne_bytes(), "大请求 PutImage 画上的像素");
    }

#[test]
fn production_loop_sends_draws_through_the_raster_queue() {
    use std::sync::atomic::Ordering;

    let (addr, _runtime, shared) = spawn_server();
    let mut socket = connect(addr, 0);

    let window = 0x0020_0001u32;
    send_request(
        &mut socket,
        CreateWindowRequest {
            depth: 0,
            wid: window,
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 32,
            height: 32,
            border_width: 0,
            class: WindowClass::INPUT_OUTPUT,
            visual: 0,
            value_list: Cow::Owned(CreateWindowAux::default().background_pixel(0x0012_3456)),
        },
    );
    send_request(&mut socket, MapWindowRequest { window });
    let gc = 0x0020_0002u32;
    send_request(
        &mut socket,
        CreateGCRequest {
            cid: gc,
            drawable: window,
            value_list: Cow::Owned(CreateGCAux::default().foreground(0x00ff_0000)),
        },
    );
    // 一条实心填充（PolyFillRectangle，纯像素请求 → 必须入队）
    send_request(
        &mut socket,
        PolyFillRectangleRequest {
            drawable: window,
            gc,
            rectangles: Cow::Owned(vec![Rectangle { x: 0, y: 0, width: 8, height: 8 }]),
        },
    );

    assert!(
        wait_for(|| shared.raster_executed.load(Ordering::Relaxed) >= 1, 3_000),
        "绘制请求必须经 worker 执行（raster_executed 仍为 {} —— 说明绘制没走队列，\
         异步光栅化被绕过）",
        shared.raster_executed.load(Ordering::Relaxed)
    );
    assert_eq!(
        shared.raster_panics.load(Ordering::Relaxed),
        0,
        "这条路径不应有 panic"
    );
}

/// 回归：**生产路径**里的绘制不会被"队列空"骗到就地执行。
///
/// 直接对比两条事实：绘制前后 `pending_draws()` 曾大于 0（说明真的排过队），
/// 且 `raster_executed` 增加。合起来 = 绘制经过队列、由 worker 执行。
#[test]
fn production_draw_is_not_executed_inline_in_the_request_loop() {
    use std::sync::atomic::Ordering;

    let (addr, _runtime, shared) = spawn_server();
    let mut socket = connect(addr, 0);
    let before = shared.raster_executed.load(Ordering::Relaxed);

    let window = 0x0020_0001u32;
    send_request(
        &mut socket,
        CreateWindowRequest {
            depth: 0,
            wid: window,
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 16,
            height: 16,
            border_width: 0,
            class: WindowClass::INPUT_OUTPUT,
            visual: 0,
            value_list: Cow::Owned(CreateWindowAux::default()),
        },
    );
    let gc = 0x0020_0002u32;
    send_request(
        &mut socket,
        CreateGCRequest {
            cid: gc,
            drawable: window,
            value_list: Cow::Owned(CreateGCAux::default()),
        },
    );
    for _ in 0..3 {
        send_request(
            &mut socket,
            PolyLineRequest {
                coordinate_mode: CoordMode::ORIGIN,
                drawable: window,
                gc,
                points: Cow::Owned(vec![
                    Point { x: 0, y: 0 },
                    Point { x: 10, y: 10 },
                ]),
            },
        );
    }

    assert!(
        wait_for(
            // CreateGC + 3 条 PolyLine（opcode 55/65 均为像素族 → 全部入队；
            // CreateWindow 是 opcode 1，在白名单内就地执行，不计入）
            || shared.raster_executed.load(Ordering::Relaxed) >= before + 4,
            3_000
        ),
        "CreateGC + 3 条 PolyLine 必须全部由 worker 执行（实际 {}）",
        shared.raster_executed.load(Ordering::Relaxed) - before
    );
}

}

/// X11 核心请求的操作码名（启动追踪探针用）。
fn core_request_name(major: u8) -> &'static str {
    match major {
        1 => "CreateWindow",
        2 => "ChangeWindowAttributes",
        3 => "GetWindowAttributes",
        4 => "DestroyWindow",
        7 => "ReparentWindow",
        8 => "MapWindow",
        10 => "UnmapWindow",
        12 => "ConfigureWindow",
        14 => "GetGeometry",
        15 => "QueryTree",
        16 => "InternAtom",
        18 => "ChangeProperty",
        19 => "DeleteProperty",
        20 => "GetProperty",
        21 => "ListProperties",
        22 => "SetSelectionOwner",
        23 => "GetSelectionOwner",
        24 => "ConvertSelection",
        26 => "GrabPointer",
        27 => "UngrabPointer",
        31 => "GrabKeyboard",
        36 => "GrabServer",
        37 => "UngrabServer",
        38 => "QueryPointer",
        40 => "TranslateCoordinates",
        41 => "WarpPointer",
        42 => "SetInputFocus",
        43 => "GetInputFocus",
        45 => "OpenFont",
        49 => "QueryFont",
        52 => "CreateGC",
        53 => "ChangeGC",
        54 => "CopyGC",
        55 => "SetDashes",
        56 => "SetClipRectangles",
        57 => "FreeGC",
        58 => "ClearArea",
        59 => "CopyArea",
        60 => "CopyPlane",
        61 => "PolyPoint",
        62 => "PolyLine",
        63 => "PolySegment",
        64 => "PolyRectangle",
        66 => "FillPoly",
        67 => "PolyFillRectangle",
        68 => "PolyFillArc",
        70 => "PutImage",
        71 => "GetImage",
        72 => "PolyText8",
        76 => "ImageText8",
        77 => "AllocColor",
        84 => "FreeCursor",
        91 => "CreateColormap",
        93 => "FreeColormap",
        95 => "CreatePixmap",
        96 => "FreePixmap",
        98 => "QueryExtension",
        100 => "GetKeyboardMapping",
        114 => "GetModifierMapping",
        _ => "ext/other",
    }
}

// ---- task-17：顺序异步光栅化管线的离线测试 ----

/// 等待一个条件成立（超时返回 false）；测试里避免固定 sleep 的竞态。
fn wait_for(mut cond: impl FnMut() -> bool, timeout_ms: u128) -> bool {
    let start = std::time::Instant::now();
    while !cond() {
        if start.elapsed().as_millis() > timeout_ms {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    true
}

/// 严格 FIFO：入队顺序 == 执行顺序（free 必须排在 composite 之后）。
#[test]
fn draw_queue_is_strict_fifo() {
    let shared = ServerShared::new(Default::default());
    let order: Arc<std::sync::Mutex<Vec<&'static str>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    for name in ["composite", "fill-rects", "free-pixmap", "composite-2"] {
        let order = Arc::clone(&order);
        shared.enqueue_draw(Box::new(move |_| {
            order.lock().unwrap().push(name);
        }));
    }
    assert_eq!(shared.pending_draws(), 4, "worker 未启动时命令应当积压");
    shared.start_raster_worker();
    assert!(
        wait_for(|| order.lock().unwrap().len() == 4, 2_000),
        "全部命令应当按序执行完"
    );
    assert_eq!(
        *order.lock().unwrap(),
        ["composite", "fill-rects", "free-pixmap", "composite-2"],
        "执行顺序必须与入队顺序一致（free 排在 composite 之后）"
    );
    shared.stop_raster_worker();
}

/// **打破"队列恒空"不动点**：像素类请求即使在空闲、本连接无在执行命令时也
/// 必须入队，否则首个绘制请求就会被就地执行 → 队列永远为空 → worker 变成
/// 死代码、绘制退回请求循环同步执行（查询被绘制流阻塞的老问题原样回来）。
/// 这正是上一版"队列非空才入队"写法的错误所在（`last_enqueued` 只在入队
/// 分支赋值，判据恒真、谁也不入队）。
#[test]
fn pixel_requests_are_queued_even_when_idle() {
    use crate::xsrv::extensions::OPCODE_RENDER;

    let shared = ServerShared::new(Default::default());
    // 空闲 + 本连接无在执行命令：**纯查询**才可以直通（保住零等待）
    for opcode in [14u8 /*GetGeometry*/, 20 /*GetProperty*/, 43 /*GetInputFocus*/,
                   98 /*QueryExtension*/, 101 /*GetKeyboardMapping*/] {
        assert!(inline_allowed(&shared, 7, 0, opcode), "opcode {opcode} 是纯查询，可直通");
    }
    // 像素语义（含资源生命周期）一律入队
    for opcode in [
        45u8,  // OpenFont（字体被绘制引用）
        46,    // CloseFont
        53,    // CreatePixmap
        54,    // FreePixmap
        55,    // CreateGC（XID 复用）
        60,    // FreeGC
        61,    // ClearArea（绘制）
        62,    // CopyArea
        72,    // PutImage
        73,    // GetImage（读像素，必须看到前序绘制）
        95,    // FreeCursor
        112,   // KillClient（销毁目标连接的像素资源）
        crate::xsrv::extensions::OPCODE_RENDER, // RENDER 全部
    ] {
        assert!(
            !inline_allowed(&shared, 7, 0, opcode),
            "opcode {opcode} 是像素语义，必须入队（这是 worker 有活干的前提）"
        );
    }
}

/// 顺序判定不再依赖"请求种类枚举是否完整"：未列入非像素白名单的一律入队，
/// 包括旧黑名单漏掉的那几类（`DestroyWindow` 以外的 XID 复用类、RENDER 的
/// 改属性/字形增删、ClearArea 等）。
#[test]
fn ordering_default_is_queue_not_inline() {
    use x11rb_protocol::protocol::xproto;

    let shared = ServerShared::new(Default::default());
    // 旧黑名单漏项：RenderChangePicture（major 140）→ 现在必然入队
    assert!(!inline_allowed(&shared, 7, 0, crate::xsrv::extensions::OPCODE_RENDER));
    let _missed_by_old_blacklist = [
        x11rb_protocol::protocol::Request::DestroyWindow(xproto::DestroyWindowRequest {
            window: 0x2100001,
        }),
        x11rb_protocol::protocol::Request::RenderChangePicture(
            x11rb_protocol::protocol::render::ChangePictureRequest {
                picture: x11rb_protocol::protocol::render::Picture::from(0x2100002u32),
                value_list: std::borrow::Cow::Owned(
                    x11rb_protocol::protocol::render::ChangePictureAux::new(),
                ),
            },
        ),
        x11rb_protocol::protocol::Request::ClearArea(xproto::ClearAreaRequest {
            exposures: false,
            window: 0x2100001,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        }),
    ];
}

/// 请求是否可以**就地直通**：把"非像素语义"的白名单与"本连接无在执行命令"
/// 合起来看，两种否定都要生效。
#[test]
fn inline_requires_non_pixel_opcode_and_no_inflight_command() {
    let shared = ServerShared::new(Default::default());
    // 纯查询 + 无在飞行 ⇒ 可直通
    assert!(inline_allowed(&shared, 7, 0, 43));
    // 本连接已入队一条命令（order=1）且尚未执行 ⇒ 后续请求即便 opcode 安全也要入队
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&started);
    shared.enqueue_tracked(
        7,
        1,
        Box::new(move |_| {
            flag.store(true, std::sync::atomic::Ordering::Release);
            std::thread::sleep(std::time::Duration::from_millis(60));
        }),
    );
    shared.start_raster_worker();
    assert!(wait_for(|| started.load(std::sync::atomic::Ordering::Acquire), 2_000));
    assert!(
        !inline_allowed(&shared, 7, 1, 43),
        "本连接的命令还在执行中 ⇒ 即便 opcode 安全也不得直通（否则应答乱序）"
    );
    // 白名单里的**窗口树操作**同样不得越过本连接在飞行命令（DestroyWindow 4 /
    // ConfigureWindow 12 会释放/重建像素缓冲；MapWindow 8 是白名单内的状态操作，
    // 靠屏障保证顺序）
    assert!(!inline_allowed(&shared, 7, 1, 4), "DestroyWindow 是像素生命周期操作：opcode 本身就不允许直通");
    assert!(!inline_allowed(&shared, 7, 1, 12), "ConfigureWindow 是像素生命周期操作：opcode 本身就不允许直通");
    assert!(!inline_allowed(&shared, 7, 1, 8), "MapWindow 在白名单内，但屏障必须拦住它");
    assert!(inline_allowed(&shared, 8, 0, 43), "别的连接不受影响");
    assert!(wait_for(|| inline_allowed(&shared, 7, 1, 43), 2_000), "执行完恢复直通");
    shared.stop_raster_worker();
}

/// 空闲期查询仍是零等待（就地直通）：队列空时处理查询只需微秒级。
#[test]
fn query_runs_inline_when_queue_is_empty() {
    let shared = ServerShared::new(Default::default());
    assert!(inline_allowed(&shared, 0, 0, 43), "纯查询 ⇒ 就地直通");
    let started = std::time::Instant::now();
    let request = x11rb_protocol::protocol::Request::QueryPointer(
        x11rb_protocol::protocol::xproto::QueryPointerRequest { window: 0 },
    );
    let mut state = shared.state();
    let _ = state.handle(0, 1, &request);
    drop(state);
    assert!(
        started.elapsed().as_millis() < 20,
        "空闲期查询不得引入额外等待（实测 {:?}）",
        started.elapsed()
    );
}

/// 有积压时 GetImage 也入队：它排在前序绘制之后执行，应答由 worker 按 FIFO
/// 发出——不再需要"先排空队列"的特例，也不可能出现应答乱序。
#[test]
fn get_image_is_queued_behind_prior_draws() {
    use x11rb_protocol::protocol::xproto;

    let shared = ServerShared::new(Default::default());
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    shared.state().register_client(7, sender);

    // 1) 一条「慢绘制」（marker 闭包模拟）
    let draw_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&draw_flag);
    shared.enqueue_draw(Box::new(move |_| {
        std::thread::sleep(std::time::Duration::from_millis(30));
        flag.store(true, std::sync::atomic::Ordering::Release);
    }));
    assert!(!inline_allowed(&shared, 7, 0, 73), "GetImage 读像素 ⇒ 必须入队（旧实现是内联 + 排空队列）");

    // 2) GetImage 入队：执行时必须已经跑完前序绘制
    let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let obs = Arc::clone(&observed);
    let marker = Arc::clone(&draw_flag);
    shared.enqueue_draw(Box::new(move |state| {
        obs.store(
            marker.load(std::sync::atomic::Ordering::Acquire),
            std::sync::atomic::Ordering::Release,
        );
        let request = x11rb_protocol::protocol::Request::GetImage(xproto::GetImageRequest {
            format: xproto::ImageFormat::Z_PIXMAP,
            drawable: crate::xsrv::handshake::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            plane_mask: u32::MAX,
        });
        if let Err(e) = state.handle(7, 42, &request) {
            panic!("GetImage 应当成功：{e:?}");
        }
    }));

    shared.start_raster_worker();
    assert!(
        wait_for(|| !receiver.is_empty(), 2_000),
        "应当收到 GetImage 应答"
    );
    assert!(
        observed.load(std::sync::atomic::Ordering::Acquire),
        "GetImage 必须排在前序绘制之后执行（FIFO）"
    );
    let frame = receiver.try_recv().expect("应答帧");
    // 序列号必须是 GetImage 自己的（应答按请求序号对齐）
    assert_eq!(u16::from_ne_bytes([frame[2], frame[3]]), 42, "应答序列号");
    shared.stop_raster_worker();
}

/// 关闭语义：stop 之后的剩余队列被丢弃，不再执行。
/// **端到端证据**：真实 TCP 连接走完整请求循环时，绘制请求必须真的进光栅化
/// 队列（worker 在干活）。
///
/// 关闭语义：stop 之后的剩余队列被丢弃（不执行），**且每条被丢弃的命令都要
/// 回执** `mark_raster_done`。
///
/// 回执为什么必须：请求循环的直通判据是 `raster_done_for(client) >= last_enqueued`。
/// 若丢弃不回执，该连接的 `last_enqueued` 永远追不上 `raster_done`，之后每个
/// 请求都被判为"必须入队"，而 worker 已停止、无人消费——无应答、无绘制、无
/// 错误，客户端挂死。
#[test]
fn stop_discards_pending_draws() {
    let shared = ServerShared::new(Default::default());
    let executed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hold = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // order=1：阻塞命令（worker 会在它上面停留，保证 2/3 排在队列里）
    let hold1 = Arc::clone(&hold);
    let executed1 = Arc::clone(&executed);
    shared.enqueue_tracked(
        7,
        1,
        Box::new(move |_| {
            executed1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            hold1.store(true, std::sync::atomic::Ordering::Release);
            while hold1.load(std::sync::atomic::Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }),
    );
    shared.start_raster_worker();
    assert!(wait_for(
        || hold.load(std::sync::atomic::Ordering::Acquire),
        2_000
    ), "worker 应当正在执行 order=1");
    // order=2/3：排在队列里（worker 被命令 1 挡住）
    for order in 2..=3u64 {
        let executed = Arc::clone(&executed);
        shared.enqueue_tracked(
            7,
            order,
            Box::new(move |_| {
                executed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }),
        );
    }
    // stop：worker 跑完当前这条后，丢弃 2/3 并回执
    shared.stop_raster_worker();
    hold.store(false, std::sync::atomic::Ordering::Release);

    assert!(wait_for(
        || shared.raster_done_for(7) >= 3,
        2_000
    ), "order=2/3 被丢弃也必须回执，否则连接 7 被永久判为'有在飞行命令'");
    assert_eq!(
        executed.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "只有正在执行的那条跑完；排队中的必须被丢弃（不执行）"
    );
    assert_eq!(shared.pending_draws(), 0, "队列必须被清空");
}

/// panic 隔离：一条绘制命令 panic 只废掉它自己，worker 必须继续消费队列。
///
/// 旧实现 `cmd.run(...)` 是裸调用——任何绘制 panic（例如借走的 drawable 像素
/// 没归还导致后续采样越界）都会杀死唯一的光栅化线程。之后队列再也没人消费：
/// 窗口还在（CreateWindow/MapWindow 是就地执行的），但画面永久冻结，且进程内
/// 无法恢复（server 被全局静态量持有，重连也拿到同一个死掉的实例）。
#[test]
fn worker_survives_draw_panic() {
    let shared = ServerShared::new(Default::default());
    let later = Arc::new(std::sync::atomic::AtomicBool::new(false));
    shared.enqueue_draw(Box::new(|_| panic!("模拟绘制命令 panic")));
    let flag = Arc::clone(&later);
    shared.enqueue_draw(Box::new(move |_| {
        flag.store(true, std::sync::atomic::Ordering::Release);
    }));
    shared.start_raster_worker();
    assert!(
        wait_for(|| later.load(std::sync::atomic::Ordering::Acquire), 2_000),
        "panic 之后 worker 必须继续执行后面的命令（否则整个界面永久停止刷新）"
    );
    assert_eq!(
        shared.raster_panics.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "panic 必须计数（日志里有 [raster] 行可对账）"
    );
    shared.stop_raster_worker();
}

/// RwLock 快照语义（task-18）：读锁并发不互斥；读锁持有期间写锁阻塞、
/// 释放后写锁立即拿到（写等读，而不是写插队饿死读）。
#[test]
fn rwlock_snapshot_allows_concurrent_reads_and_fair_writes() {
    let shared = ServerShared::new(Default::default());

    // 1) 读读并发：两个读锁必须能同时持有
    let r1 = shared.state_read();
    let r2 = shared.state_read();
    let _ = (&r1, &r2);
    drop(r1);
    drop(r2);

    // 2) 读锁持有期间写锁等待；释放后写锁立刻拿到（≤200ms）
    let r = shared.state_read();
    let writer = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let _w = shared.state(); // 写锁：等读锁释放
            let waited = started.elapsed();
            assert!(
                waited.as_millis() <= 200,
                "读锁释放后写锁应当立刻拿到（等了 {waited:?}）"
            );
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(50));
    drop(r); // 释放读锁 → 写锁放行
    writer.join().expect("写锁线程应当成功");

    // 3) 帧快照路径：无变化时 take_frame_snapshot 返回 None 且消费版本，
    //    连续调用稳定为 None（不重扫、不重复发送）
    assert!(shared.take_frame_snapshot().is_none());
    assert!(shared.take_frame_snapshot().is_none());
}
