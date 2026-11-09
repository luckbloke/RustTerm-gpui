//! 进程内 X server 的"宿主"：端口分配、连接任务、帧泵、输入注入。
//!
//! 这一层是 `xsrv` 对外唯一的入口。它把三件互相独立的事装在一起：
//!
//! 1. **传输**：在 `127.0.0.1:6000+N` 上监听，把每条 TCP 连接交给
//!    `server::serve_stream`。生产路径最终会被 SSH 的 `x11` 通道替代（M7），
//!    但 `serve_stream` 的入参是通用字节流，换传输层不用改服务端。
//! 2. **显示**：帧泵把合成结果推给前端 canvas（`viewer.rs`）。
//! 3. **输入**：把前端画布上的键鼠翻译成 X 事件（`input.rs`）。
//!
//! 为什么还要监听 TCP：远端 `DISPLAY=127.0.0.1:N.0` 只对"客户端和远端同机"
//! 成立（WSL、本机 loopback）。这也是 M7 之前唯一能拿真实 X 客户端做验收的路。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::input::keycode_for_keysym;
use super::server::{serve_stream, ServerConfig, ServerShared};
use super::viewer::{FramePump, FrameSink};

/// display 扫号上限：0 留给系统上真正的 X server，从 1 开始。
const MAX_DISPLAY: u32 = 99;

/// 一次输入批最多包含的事件数。前端按 16ms 打包，正常情况下只有个位数；
/// 设上限是为了防止异常调用把事件循环卡住。
const MAX_INPUT_BATCH: usize = 256;

/// UI 发来的输入事件。
#[derive(Debug, Clone)]
pub enum XInputEvent {
    /// 键盘：keysym（客户端按系统布局算好字符），服务端反查 keycode。
    Key { keysym: u32, pressed: bool },
    /// 指针绝对位置（屏幕坐标）。
    Pointer { x: i16, y: i16 },
    /// 指针位置 + **权威按键掩码**（UI 当前的按键状态；1=左 2=中 4=右）。
    ///
    /// 服务端据此对账：掩码里没有、但服务端仍按着的键会补一个 Release。
    /// 没有这层对账的话，"按下后 UI 丢了抬起事件"（鼠标在画面外松开、窗口
    /// 失焦、滚轮拖滚动条时移出区域）会让隐式抓取永远不释放，之后所有指针
    /// 事件都投给那个旧窗口——表现就是"过一会界面就操作不了"。
    PointerWithButtons { x: i16, y: i16, buttons: u8 },
    /// 鼠标键按下/抬起，`button` 是 X 的键号（1=左 2=中 3=右）。
    Button { button: u8, pressed: bool },
    /// 滚轮：X 核心协议没有滚轮，用 4/5 号键的一按一放表示。
    Scroll { up: bool },
}

/// 一个运行中的进程内 X server。
pub struct HostServer {
    /// display 编号：监听端口 = 6000 + display。
    pub display: u32,
    pub port: u16,
    shared: Arc<ServerShared>,
    frame: Arc<FramePump>,
    /// 下一个客户端的资源 ID 区间编号。TCP 连接与 SSH `x11` 通道共用同一个
    /// 计数器：X 的资源 ID 高 11 位标识客户端，两套传输各用各的就会撞车。
    next_client: Arc<AtomicU32>,
    stop_tx: watch::Sender<bool>,
    /// 后台任务句柄（accept 循环、帧泵）。用 Mutex 包起来是为了让 `shutdown`
    /// 能拿到 `&self` 就停——服务端实例会被 `Arc` 共享给多个命令。
    tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
}

impl HostServer {
    /// 分配一个空闲 display 并启动服务端。
    pub async fn start(width: u16, height: u16) -> Result<Self, String> {
        let (display, listener) = bind_display().await?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("读取监听地址失败：{e}"))?
            .port();

        let shared = ServerShared::new(ServerConfig {
            screen_width: width.max(320),
            screen_height: height.max(240),
        });
        let frame = FramePump::new(Arc::clone(&shared));
        let (stop_tx, stop_rx) = watch::channel(false);
        let next_client = Arc::new(AtomicU32::new(0));

        let mut tasks = Vec::new();
        tasks.push(spawn_accept_loop(
            listener,
            Arc::clone(&shared),
            Arc::clone(&next_client),
            stop_rx.clone(),
        ));
        tasks.push(frame.spawn_loop());

        eprintln!("[xsrv] 进程内 X server 已启动：127.0.0.1:{port}（display :{display}）");
        Ok(Self {
            display,
            port,
            shared,
            frame,
            next_client,
            stop_tx,
            tasks: std::sync::Mutex::new(tasks),
        })
    }

    /// 写进远端 shell 的 DISPLAY 值。
    pub fn display_name(&self) -> String {
        format!("127.0.0.1:{}.0", self.display)
    }

    pub fn screen_size(&self) -> (u16, u16) {
        let state = self.shared.state();
        match state.screen() {
            Some((width, height, _)) => (width, height),
            None => (0, 0),
        }
    }

    /// UI 侧帧接收端挂上来（或切标签回来）时调用。帧泵会立刻推一张整屏。
    pub fn attach_viewer(&self, sink: FrameSink) {
        self.frame.attach(sink);
    }

    pub fn detach_viewer(&self) {
        self.frame.detach();
    }

    /// 为一条新连接分配资源 ID 区间。
    ///
    /// SSH 的 `x11` 通道和 TCP 连接都从这里取号，保证同一时刻两个客户端的
    /// 资源 ID 不会重叠。
    pub fn next_client_index(&self) -> u32 {
        self.next_client.fetch_add(1, Ordering::Relaxed)
    }

    /// 把一条已建立的字节流交给 X server。
    ///
    /// 生产路径是 SSH 的 `x11` 通道；TCP 监听只是为了本地调试。两者最终都
    /// 走 `serve_stream`，所以这里不需要关心底层是 TcpStream 还是 ChannelStream。
    pub fn serve_channel<S>(&self, stream: S, client_index: u32) -> JoinHandle<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let shared = Arc::clone(&self.shared);
        tokio::spawn(async move {
            if let Err(e) = serve_stream(stream, client_index, shared).await {
                eprintln!("[xsrv] X11 通道客户端结束：{e}");
            }
        })
    }

    /// 把一批前端输入注入到服务端状态里。
    ///
    /// 全部在同一把锁内按顺序执行：X 的事件顺序就是用户的操作顺序，
    /// 拆成多次加锁会让 Motion 和 Button 的顺序错乱（表现为"点击位置偏一格"）。
    pub fn send_input(&self, events: &[XInputEvent]) -> Result<(), String> {
        if events.len() > MAX_INPUT_BATCH {
            return Err(format!("输入批次超过 {MAX_INPUT_BATCH} 个事件"));
        }
        let mut state = self.shared.state();
        for event in events {
            match *event {
                XInputEvent::Pointer { x, y } => state.pointer_motion(x, y),
                XInputEvent::PointerWithButtons { x, y, buttons } => {
                    state.pointer_move_with_buttons(x, y, buttons)
                }
                XInputEvent::Button { button, pressed } => {
                    if (1..=7).contains(&button) {
                        state.pointer_button(button, pressed);
                    }
                }
                XInputEvent::Scroll { up } => state.pointer_scroll(up),
                XInputEvent::Key { keysym, pressed } => {
                    if let Some((keycode, forced_shift)) = keycode_for_keysym(keysym) {
                        state.key_event(keycode, pressed, forced_shift);
                    }
                }
            }
        }
        Ok(())
    }

    /// 停服务：先断开连接循环，再把帧泵停掉。
    ///
    /// 用 `&self` 而不是 `self`：实例被 `Arc` 共享在应用状态里，
    /// 停服务只需要通知后台任务，不需要销毁实例本身。
    pub fn shutdown(&self) {
        let _ = self.stop_tx.send(true);
        self.frame.stop();
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        for task in tasks.drain(..) {
            task.abort();
        }
        eprintln!("[xsrv] 进程内 X server 已停止（display :{}）", self.display);
    }
}

/// 找一个没被占用的 display。返回 (display, 已绑定的 listener)。
///
/// 端口即所有权：`bind` 成功就说明这个 display 归我们，不用再查锁文件
/// （Windows 上也根本没有 `/tmp/.X11-unix`）。
async fn bind_display() -> Result<(u32, TcpListener), String> {
    let mut last_error = String::from("没有可用的 display");
    for display in 1..=MAX_DISPLAY {
        let port = 6000u16.saturating_add(display as u16);
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => return Ok((display, listener)),
            Err(e) => last_error = format!("127.0.0.1:{port} 不可用：{e}"),
        }
    }
    Err(format!("X server 启动失败：{last_error}"))
}

fn spawn_accept_loop(
    listener: TcpListener,
    shared: Arc<ServerShared>,
    next_client: Arc<AtomicU32>,
    mut stop: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                // `changed()` 在发送端 drop 时也会返回，两种情况都该退出
                _ = stop.changed() => break,
                accepted = listener.accept() => {
                    let (socket, peer) = match accepted {
                        Ok(pair) => pair,
                        Err(e) => {
                            eprintln!("[xsrv] accept 失败：{e}");
                            break;
                        }
                    };
                    // X11 是"大量小包"协议（按键事件、请求、回复都只有几十字节），
                    // 不关 Nagle 的话小包会被攒着等 ACK，打字回显会有明显延迟。
                    socket.set_nodelay(true).ok();

                    let shared = Arc::clone(&shared);
                    let client_index = next_client.fetch_add(1, Ordering::Relaxed);
                    tokio::spawn(async move {
                        if let Err(e) = serve_stream(socket, client_index, shared).await {
                            eprintln!("[xsrv] 客户端 {peer} 连接结束：{e}");
                        }
                    });
                }
            }
        }
    })
}
