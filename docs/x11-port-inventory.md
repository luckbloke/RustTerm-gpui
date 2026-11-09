# X11 实现移植盘点报告（rustterm-tauri → rustterm-gpui）

> 分析对象：`D:\rustterm\rustterm-tauri\src-tauri`（xsrv 全部 14 文件 + xserver.rs + ssh.rs + lib.rs + Cargo.toml）
> 移植目标：`D:\rustterm\rustterm-gpui`（GPUI 框架，无 tauri / webview）
> 日期：2026-11-06（只读分析，未修改任何文件）

## 0. 重要现状：xsrv 已经被原样拷入目标工程

目标工程 `src/xsrv/` 下已存在全部 14 个文件（git 未跟踪 `?? src/xsrv/`），且经哈希比对与源工程**逐字节一致**。因此：

- 真正未完成的移植工作是：**依赖（Cargo.toml 还没有 x11rb-protocol）、host.rs / viewer.rs 的 tauri 依赖替换、ssh.rs 的 X11 转发链路（gpui 版 backends/ssh.rs 目前完全没有 x11 代码）、帧输出改接 GPUI 纹理、输入改接 GPUI 事件**。
- mod.rs / server.rs / state.rs / request.rs / draw.rs / raster.rs / input.rs / handshake.rs / font.rs / font_data.rs / misc.rs / error.rs 这 12 个文件**零 tauri 依赖、零工程内部依赖**，补一条依赖声明即可直接编译。

## 1. xsrv 模块对外 API 与完整调用链

### 1.1 类型与方法签名（抄录自 mod.rs 的 re-export 与源文件）

`mod.rs:27-29` 对外只导出三组：

```rust
pub use handshake::{resource_id_base, SetupError, SETUP_PREFIX_LEN};
pub use host::{HostServer, XInputEvent};
pub use server::{serve_stream, serve_tcp, ServerConfig, ServerShared};
```

**server.rs**

```rust
pub struct ServerConfig { pub screen_width: u16, pub screen_height: u16 }   // Default = 1280x800
pub struct ServerShared { state: Mutex<ServerState> }
impl ServerShared {
    pub fn new(config: ServerConfig) -> Arc<Self>;                  // :57
    pub fn state(&self) -> MutexGuard<'_, ServerState>;             // :63（毒锁恢复）
}
pub async fn serve_stream<S>(stream: S, client_index: u32, shared: Arc<ServerShared>) -> io::Result<()>
where S: AsyncRead + AsyncWrite + Unpin + Send + 'static;           // :87 —— 传输层唯一入口
pub async fn serve_tcp(listener: tokio::net::TcpListener, shared: Arc<ServerShared>) -> io::Result<()>; // :265（仅调试）
```

**host.rs**

```rust
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum XInputEvent {
    Key { keysym: u32, pressed: bool },
    Pointer { x: i16, y: i16 },
    Button { button: u8, pressed: bool },   // 1=左 2=中 3=右
    Scroll { up: bool },                    // 4/5 号键一按一放
}

pub struct HostServer {
    pub display: u32, pub port: u16,
    shared: Arc<ServerShared>, frame: Arc<FramePump>,
    next_client: Arc<AtomicU32>,
    stop_tx: watch::Sender<bool>,
    tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}
impl HostServer {
    pub async fn start(width: u16, height: u16) -> Result<Self, String>;  // :66，绑定 127.0.0.1:6000+N
    pub fn display_name(&self) -> String;        // "127.0.0.1:{display}.0"
    pub fn screen_size(&self) -> (u16, u16);     // :107
    pub fn attach_viewer(&self, channel: Channel<InvokeResponseBody>);   // :116 ← tauri 类型
    pub fn detach_viewer(&self);                 // :120
    pub fn next_client_index(&self) -> u32;      // :128，AtomicU32 取号（TCP 与 SSH 通道共用）
    pub fn serve_channel<S>(&self, stream: S, client_index: u32) -> JoinHandle<()>
        where S: AsyncRead + AsyncWrite + Unpin + Send + 'static;       // :136，SSH x11 通道入口
    pub fn send_input(&self, events: &[XInputEvent]) -> Result<(), String>; // :152，同一把锁内按序注入
    pub fn shutdown(&self);                      // :180，&self 即可停
}
```

**viewer.rs（帧泵）**

```rust
pub struct FrameRect { pub x: u16, pub y: u16, pub width: u16, pub height: u16 } // :38
pub struct FramePump { shared, channel: Mutex<Option<Channel<InvokeResponseBody>>>, stopped: AtomicBool }
impl FramePump {
    pub fn new(shared: Arc<ServerShared>) -> Arc<Self>;       // :70
    pub fn attach(&self, channel: Channel<InvokeResponseBody>); // :79 ← tauri 类型
    pub fn detach(&self); pub fn stop(&self);
    pub fn spawn_loop(self: &Arc<Self>) -> tokio::task::JoinHandle<()>; // :112，33ms 间隔
}
pub fn dirty_rect(previous: &[u32], current: &[u32], width: usize, height: usize) -> Option<FrameRect>; // :177
```

注意：模块内**没有名为 `Transport` / `Viewer` 的类型**。传输抽象就是 `serve_stream` 的泛型参数 `S: AsyncRead + AsyncWrite`；"Viewer" 就是 `FramePump`（帧）与 `XInputEvent`（输入）。

### 1.2 一个 X 客户端连接的完整调用链（字节流 → 帧）

1. **字节流到达**：TCP（`serve_tcp` accept 循环，`server.rs:265`）或 SSH x11 通道（`host.rs serve_channel` → tokio spawn）→ 统一进 `serve_stream`。
2. **握手**（`server.rs:87-153`）：读 12 字节 setup 前缀（`handshake::setup_prefix` 算出剩余长度，只收 LSBFirst，MSBFirst 回 setup-failed）→ 读完整 setup request（`handshake::parse_setup`，x11rb `SetupRequest::try_parse`）→ 写回 `handshake::setup_reply(client_index, w, h)`（x11rb `Setup::serialize`，含 24 位 TrueColor 单视觉 screen、资源 ID 基址 `(n+1)<<21`）。
3. **注册 + writer 任务**（`server.rs:155-167`）：`tokio::sync::mpsc::unbounded_channel::<Vec<u8>>`，sender 注册进 `ServerState`（`state.rs:398 register_client`），spawn 一个专职 writer 任务把队列帧按序写回 socket——单写者保证事件/reply 顺序。
4. **请求循环**（`server.rs:182-259 request_loop`）：读 4 字节头（major/minor opcode + 长度，长度字段含头、×4 字节；`total_len==0` 即 BIG-REQUESTS，直接断开）→ 读净 body → `x11rb_protocol::protocol::Request::parse(header, &body, &mut fds, &NoExtensions)` → `shared.state().handle(client_index, sequence, &request)`（`request.rs:33`，巨型 match 分发）→ 出错时 `error::encode(XError, sequence, minor, major)` 回 32 字节错误帧。
5. **语义层**（`state.rs` `ServerState`，~1334 行）：资源表（windows/pixmaps/GC/colormaps/fonts/atoms/cursors，跨连接共享）、事件分发 `deliver(window, mask, build)`（含 `send_to_owner` 处理 Selection 事件）、绘制入口 `draw_with_gc` → `draw::Canvas`（`draw.rs`，像素级光栅：GCFunction/gx、tile/stipple、线/矩形/文字）+ `raster.rs`（圆弧/多边形的 span 扫描）→ `misc.rs`（颜色名解析、colormap、Grab、键盘映射查询等杂项请求）。
6. **合成与帧号**：每次窗口/绘制请求结束 `compose()`（`state.rs:983`）从根窗口按叠放顺序把子窗口画进根像素缓冲 `u32` 数组（格式 `0x00RRGGBB`），并 `frame_seq += 1`（`state.rs:1012`）。
7. **帧导出**（`viewer.rs`）：`FramePump` 每 33ms tick → `snapshot()` 拿 `(frame_seq, w, h, pixels)`；`frame_seq` 没变就跳过（空闲零 CPU）→ 变了则 `dirty_rect()` 与上一帧逐行比较求包围盒（变化面积 >60% 屏幕则整屏）→ 裁矩形、转 RGB8、`image::codecs::jpeg::JpegEncoder`（质量 65，`spawn_blocking` 编码）→ 打包 12 字节头 + JPEG → 经 tauri Channel 发给前端。
8. **输入回灌**：前端 → `send_input(&[XInputEvent])`（`host.rs:152`，单锁内按序）→ `input.rs`：`keycode_for_keysym` 反查 Xorg 标准 keycode + 是否强制 Shift → `ServerState::key_event / pointer_motion / pointer_button / pointer_scroll` 按 X 协议传播规则（子窗口→祖先、`do_not_propagate`、Enter/Leave、FocusIn/Out）构造 32 字节事件帧 `deliver` 给选了掩码的客户端。

### 1.3 传输层现状

- TCP 监听（6000+N）只为本地调试/WSL；**生产路径是 SSH 的 x11 通道字节流交给同一个 `serve_stream()`**（`server.rs:13-14` 注释明确）。
- 没有实现任何扩展：`NoExtensions`（`server.rs:70-84`）实现空 `ExtInfoProvider`；`Request::parse` 遇到扩展请求直接 BadRequest。（Cargo.toml 里的 composite/damage/randr/… features 只是让 x11rb-protocol 能**解析**这些请求，服务端并不实现语义。）

## 2. 依赖审计（逐文件）

### 2.1 外部 crate

| 文件 | 外部依赖 | 移植判定 |
|---|---|---|
| server.rs | `tokio::io`、`tokio::sync::mpsc`、`x11rb_protocol::protocol::Request`、`x11_utils::{ExtInfoProvider, ExtensionInformation, RequestHeader}` | ✅ 可原样移植（gpui 需 tokio `io-util`/`sync`/`macros`，已有；补 x11rb-protocol） |
| handshake.rs | 仅 `x11rb_protocol::protocol::xproto::*` + `x11_utils::{Serialize, TryParse}` | ✅ 原样 |
| state.rs | `tokio::sync::mpsc::UnboundedSender` + x11rb | ✅ 原样 |
| request.rs | 仅 x11rb | ✅ 原样 |
| draw.rs / raster.rs / error.rs / misc.rs | 仅 x11rb | ✅ 原样 |
| input.rs | 仅 x11rb | ✅ 原样 |
| font.rs / font_data.rs | 无任何依赖（纯静态表） | ✅ 原样 |
| viewer.rs | `image`（JPEG 编码）、`tauri::ipc::{Channel, InvokeResponseBody}` | ⚠️ **需替换**（见 2.2） |
| host.rs | `tauri::ipc::{Channel, InvokeResponseBody}`、tokio、`serde::Deserialize` | ⚠️ **需替换**（见 2.2） |

**x11rb-protocol features**（源工程 `Cargo.toml:24-37`）：`default-features=false` + `["std", "request-parsing", "composite", "damage", "randr", "render", "shape", "shm", "xfixes", "xinput", "xkb", "xtest"]`。**代码实际只用到 `std` + `request-parsing`**（Request::parse / Serialize / TryParse 全在核心）；其余 features 只影响扩展请求能否被 parse 出来（然后被 BadRequest 拒绝）。gpui 最小集：`{ version = "0.13", default-features = false, features = ["std", "request-parsing"] }` 即可跑通 xeyes；保留完整列表也无害，但按需裁剪可省编译。

### 2.2 tauri / 工程内部依赖（全部集中在两个文件）

| 位置 | 依赖 | 替换方案 |
|---|---|---|
| host.rs:17,116 | `tauri::ipc::{Channel, InvokeResponseBody}` | `attach_viewer` 改成收 `tokio::sync::mpsc::UnboundedSender<Vec<u8>>`（或直接 `crossbeam` / gpui 侧回调）；帧泵把 12 字节头+JPEG 的包发进该 channel，由 gpui 层的任务收下来解码上纹理 |
| viewer.rs:25,64,103 | 同上 | `FramePump.channel: Mutex<Option<Channel<InvokeResponseBody>>>` → `Mutex<Option<UnboundedSender<Vec<u8>>>>`；`send()` 里 `channel.send(InvokeResponseBody::Raw(p))` → `tx.send(p)`。帧格式本身不变 |
| host.rs:35-46 | `serde::Deserialize`（XInputEvent 的 tagged enum） | gpui 无 serde 需求的话可改为普通 enum 构造调用，或保留 serde（gpui 工程已有 serde） |
| 整个模块 | 无工程内部依赖（不引用 ssh.rs / lib.rs / hostkey） | —— |

### 2.3 tauri 耦合点清单（逐条：文件+行号 → gpui 替代方案）

| # | 位置 | tauri 依赖 | 建议的 gpui 替代方案 |
|---|---|---|---|
| 1 | xsrv/host.rs:17 | `use tauri::ipc::{Channel, InvokeResponseBody};` | 改为 `tokio::sync::mpsc::UnboundedSender<Vec<u8>>` 或自定义 `FrameSink` trait（`fn send_frame(&self, packet: Vec<u8>)`） |
| 2 | xsrv/host.rs:35-36 | `serde::Deserialize`（tauri 前端 JSON 反序列化） | gpui 直接构造 enum；若经 JSON 通道再留 serde |
| 3 | xsrv/host.rs:116 | `attach_viewer(&self, channel: Channel<InvokeResponseBody>)` | `attach_viewer(&self, tx: UnboundedSender<Vec<u8>>)`；gpui 侧任务收包 → 解码 → 上纹理 |
| 4 | xsrv/viewer.rs:25 | `use tauri::ipc::{Channel, InvokeResponseBody};` | 同 #1 |
| 5 | xsrv/viewer.rs:64 | `channel: Mutex<Option<Channel<InvokeResponseBody>>>` | `Mutex<Option<UnboundedSender<Vec<u8>>>>` |
| 6 | xsrv/viewer.rs:103 | `channel.send(InvokeResponseBody::Raw(packet))` | `tx.send(packet)`；发送失败仍走 detach 逻辑 |
| 7 | ssh.rs:9 | `use tauri::{AppHandle, Emitter};`（ssh.rs:322 `app.emit("ssh:x11", …)`） | X11Report 通过 `connect()` 返回值 / 状态回调带出，不再走事件总线 |
| 8 | ssh.rs:322 | `emit("ssh:x11", json)` | 同 #7：`SshHandle` 增加 `pub x11_report: X11Report` 字段（X11Status 可派生 Display/Debug） |
| 9 | lib.rs:581 | `pub struct XServerState(pub StdMutex<Option<XBackend>>)` + tauri `.manage` | gpui：自有 `struct XServerManager(Mutex<Option<XBackend>>)`，挂到 AppState/global |
| 10 | lib.rs:1722-1896 各 `#[tauri::command]` | `tauri::State<'_, XServerState>` 参数、`Channel<InvokeResponseBody>`（x11_attach_viewer） | 改成普通 Rust 方法调用（见 §6bis 签名表，输入输出类型不变） |
| 11 | xserver.rs | 无 tauri 依赖（serde/serde_json/directories 仅用于用户配置文件） | 直接复制；可顺手砍掉 xserver.json 配置功能减少依赖 |
| 12 | hostkey.rs | 无 tauri 依赖（russh 回调） | 直接复制 X11 相关 ~60 行 |

除此之外，xsrv 的其余 12 个文件与 ssh.rs 的 X11 部分**没有任何** `tauri::Manager` / `AppHandle` / `emit` / `tauri::State` 引用。

### 2.4 工程外部模块（ssh.rs / lib.rs / xserver.rs）

- `ssh.rs` 依赖 `hostkey::{HostKeyChecker, X11Channel}`（`pub type X11Channel = russh::Channel<Msg>`）、`tauri::AppHandle::emit`（`ssh:x11` 事件，ssh.rs:322——gpui 版改为直接返回/回调）、`rand`+`hex`（cookie）。
- `lib.rs` 的 X11 命令依赖 `xsrv::HostServer`、`xserver::*`、tauri State/Channel。gpui 版应把这部分逻辑挪进自己的 app/session 管理层。

## 3. 线程与同步模型

- **runtime**：tokio 多线程（t0 原工程在 tauri setup 里建 runtime；gpui 版可复用 backends 既有 tokio runtime 或自建一个专用多线程 runtime）。Server 本身不在专用 OS 线程上，而是 tokio 任务：每条连接一个 `serve_stream` 任务 + 每连接一个 writer 任务 + 一个 accept 任务 + 一个帧泵任务。
- **共享状态**：`ServerShared.state: std::sync::Mutex<ServerState>`（**同步 Mutex，不是 tokio Mutex**）。关键设计：持锁期间零 I/O——回复/事件只是 push 进各连接的 `UnboundedSender<Vec<u8>>`，绝不因某客户端不读而阻塞他人。毒锁用 `unwrap_or_else(poisoned.into_inner)` 恢复（`server.rs:63-66`）。
- **帧 → Viewer**：`FramePump.channel: Mutex<Option<Channel<...>>>`（换 gpui 后即 `Mutex<Option<UnboundedSender<Vec<u8>>>>`），33ms `tokio::time::interval` 驱动；`stopped: AtomicBool`（Release/Acquire）停止；`frame_seq: u64`（在 ServerState 里，由 Mutex 保护）做帧差判定。
- **输入 → Server**：`HostServer::send_input(&[XInputEvent])` 在**同一把 state Mutex 内批量按序执行**（Motion/Button 顺序不能拆多次加锁，否则点击偏一格，`host.rs:148-152`）；批次上限 `MAX_INPUT_BATCH = 256`。
- **客户端序号**：`next_client: Arc<AtomicU32>`（Relaxed）——TCP 连接与 SSH 通道共用，保证 X 资源 ID 区间（每客户端 `(n+1)<<21`）不重叠。
- **注入事件的 sequence**：投递时用该客户端当前推进到的 sequence 盖写（`state.rs:569 current_sequence`），否则 XCB 报 Unknown sequence abort（xclock 实测踩坑，见 server.rs 测试注释 :461-467）。
- **停机**：`watch::channel(false)` 通知 accept 循环退出 + `frame.stop()` + abort 全部任务句柄（`host.rs:180-188`）。

## 4. ssh.rs 的 X11 流程（含关键函数与行号）

> gpui 版 `src/backends/ssh.rs` 目前**完全没有**任何 x11 代码，本节全部需要移植。

| 函数/项 | 行号 | 内容 |
|---|---|---|
| `SshHandle.x11_rx` / `take_x11_channels()` | ssh.rs:19, 34 | 启用转发时持有 `mpsc::UnboundedReceiver<X11Channel>`，调用方取走交给 X server；只能取一次 |
| `x11_cookie()` | ssh.rs:43-48 | 16 随机字节 → 32 hex 字符（`rand::thread_rng().fill_bytes` + `hex::encode`）。**内建/`-ac` 外部 server 都不校验 cookie**，只求格式合法且每次不同 |
| `x11_channels(x11)` | ssh.rs:365 | 启用时建 unbounded channel (tx,rx)；tx 挂到 `HostKeyChecker::with_x11_sink` |
| `connect_target(..., x11_sink)` | ssh.rs:55-113 | 把 sink 装进 hostkey.rs 的 checker（hostkey.rs:155 `with_x11_sink`） |
| `open_session_channel()` | ssh.rs:249-306 | 核心时序：`request_pty(false)` → **`request_x11(true, false, "MIT-MAGIC-COOKIE-1", cookie, 0)`**（want_reply=true，single_connection=false，screen=0；**必须在 request_shell 之前**）→ `await_channel_reply`（:224，1.5s 上限）得到 `X11Status::{Accepted,Rejected,NoReply}` → `request_shell(false)` |
| 兼容模式（反向端口转发兜底） | ssh.rs:277-303 | 触发条件：`x11 && status != Accepted`。发 `handle.tcpip_forward("127.0.0.1", 0)`（port=0 由远端挑空闲端口，3s 超时）；拿到 6001..=65535 的端口 p 后往 shell 写 `export DISPLAY=localhost:{p-6000}.0\n`。不依赖 sshd 的 X11Forwarding/xauth；失败（远端禁 AllowTcpForwarding / 超时 / 端口越界）则放弃并打日志 |
| `X11Report` / `X11Status` | ssh.rs:180-207 | `emit("ssh:x11", {sessionId, status, display})` 同步告知前端（ssh.rs:322）——gpui 版改为经事件回调/直接返回 |
| `X11Channel` 类型 | hostkey.rs:19 | `pub type X11Channel = russh::Channel<Msg>` |
| `server_channel_open_x11` | hostkey.rs:191 | sshd 反向打开 `x11` 通道 → `x11_sink.send(channel)` |
| `server_channel_open_forwarded_tcpip` | hostkey.rs:219 | 兼容模式下承载 X11 字节流的 `forwarded-tcpip` 通道 → **同一个 sink**（hostkey.rs:133-155 注释明确） |

**通道 → xsrv 的接线**（原在 lib.rs:617-657，gpui 版要新写等价物）：

```
prepare_x11_for_ssh() (lib.rs:604)：x11=true 时先 ensure_xserver，再取 X11Route
X11Route::Internal(Arc<HostServer>) | External(port: u16)
attach_x11_route (lib.rs:618)：handle.take_x11_channels() → spawn_x11_router
spawn_x11_router (lib.rs:628)：每条 X11Channel →
    Internal: index = server.next_client_index(); server.serve_channel(channel.into_stream(), index)
    External: channel.into_stream() 与 TcpStream("127.0.0.1", port) copy_bidirectional
```

**`setup_x11` tauri command**（lib.rs:1308-1352）：与 open_session_channel 的兜底是两套但逻辑同型——读当前 X 后端的 `display_name()`；若会话 `x11_forwarding` 则对 `h.client` 发 `tcpip_forward("127.0.0.1", 0)`（3s 超时）得 `DISPLAY=localhost:{port-6000}.0`，否则直接写 `export DISPLAY={local}\n` 进 shell。错误码：`xserver-not-running` / `x11-forward-failed:timeout|{e}|bad-port:{n}`。

**X11 全链路触发关系（谁调谁）**：

```
前端 UI 勾选 X11 → invoke("ssh_connect", {x11: true})
  ├→ prepare_x11_for_ssh (lib.rs:604)：ensure_xserver 起内建/外部 X server → x11_route()
  ├→ ssh::connect(...x11=true) (ssh.rs:384)
  │    ├→ x11_channels(true)：建 unbounded (tx,rx)，tx 挂 HostKeyChecker::with_x11_sink
  │    └→ open_session_channel (ssh.rs:249)：pty-req → x11-req(cookie,screen=0) → shell
  │         └─ sshd 拒绝/超时 → tcpip_forward("127.0.0.1",0) → shell 写
  │            "export DISPLAY=localhost:{port-6000}.0\n"   ← 兜底远端命令原文
  ├→ attach_x11_route (lib.rs:618)：take_x11_channels() → spawn_x11_router(rx, route)
  │    （此后 sshd 反向打开的 x11 / forwarded-tcpip 通道全部进 serve_stream / 外部转发）
  └→ 前端按需 invoke("setup_x11", {sessionId})：手动补一条 export DISPLAY=… 进 shell
cookie：ssh.rs:43 x11_cookie() = 16 随机字节 hex（32 hex 字符，每次不同，无校验方）
screen 号：恒 0（request_x11 第 4 参，ssh.rs:265）
```

## 5. 帧格式与输入格式

### 5.1 帧包（viewer.rs:5-12，与 RDP/VNC 前端共用格式）

```text
byte 0-1  : x      (u16 LE)
byte 2-3  : y      (u16 LE)
byte 4-5  : width  (u16 LE)
byte 6-7  : height (u16 LE)
byte 8-11 : jpeg 字节数 (u32 LE)
byte 12.. : JPEG (质量 65，SOI 0xFFD8 开头)
```

- 像素源：根窗口缓冲，`Vec<u32>`（小端机器上按 `RR GG BB 00` 字节序落在内存），行优先，格式 `0x00RRGGBB`。
- 帧率控制：FRAME_INTERVAL=33ms（≈30fps，`MissedTickBehavior::Delay`），空闲（frame_seq 不变）零编码零发送；attach 前无观众时作废基准帧，重新 attach 必走整屏。
- 30fps 节流（FRAME_INTERVAL=33ms）、`frame_seq` 按需编码、脏矩形包围盒（FULL_FRAME_RATIO=0.6 时退整屏）。
- **gpui 适配点**：GPUI 纹理不吃 JPEG。建议把 `encode_frame`（viewer.rs:221-264）改为直接返回 `FrameRect + RGBA8`（像素已是 0x00RRGGBB，转 RGBA8 只是一次重排，且**省掉 JPEG 编解码**——GPUI 场景同进程无 IPC 带宽压力），事件对齐目标工程 `RdEvent::Frame { x, y, w, h, rgba }`（remote/mod.rs:115）。

### 5.2 输入事件（host.rs:35-46 XInputEvent）

| 事件 | 编码 | xsrv 内部处理 |
|---|---|---|
| `Key { keysym: u32, pressed: bool }` | X keysym | `input.rs keycode_for_keysym`（:207）反查 Xorg 标准 keycode + forced_shift → `key_event`（:569）；修饰位 Mask（Shift=1/Ctrl=4/Mod1=8/Mod4=64）在事件之后更新 state |
| `Pointer { x: i16, y: i16 }` | 屏幕绝对坐标 | clamp 到屏幕 → `pointer_motion`（:392）：窗口命中、Enter/Leave、MotionNotify 按 `propagate_pointer` 传播 |
| `Button { button: u8, pressed: bool }` | X 键号 1..=7（1 左 2 中 3 右） | `pointer_button`（:487）：按下时把焦点给指针下窗口（无 WM 的权宜） |
| `Scroll { up: bool }` | 方向布尔 | `pointer_scroll`（:559）：button 4（上）/5（下）一按一放 |

键位表：US 布局 2 层（`KEYSYMS_PER_KEYCODE=2`），keycode 8..255，覆盖主键区/小键盘/F1-F12/导航键/修饰键（input.rs:54-165）。GetKeyboardMapping/GetModifierMapping 回复由 `keysym_table`（:177）/`modifier_mapping`（:191）生成——toolkit 启动必查，缺了快捷键失效。

**gpui 适配点**：`RdCommand` 已有 `Key(KeyInput)`（含 `keysym` 字段！）、`Pointer{x,y,buttons}`、`Wheel{dy}`——语义几乎一一对应，写一个 `RdCommand → XInputEvent` 的转换函数即可（Wheel: `dy>0 → Scroll{up:true}`；Pointer 的 buttons 位掩码 1/2/4 → Button 1/2/3）。

### 5.3 lib.rs 相关 tauri command 签名一览（gpui 版等价方法）

| command（lib.rs 行号） | 输入 | 输出 | 依赖 |
|---|---|---|---|
| `xserver_start` (:1722) | `width: Option<u16>, height: Option<u16>, prefer_external: Option<bool>` | `Result<String/*DISPLAY*/, String>` | `ensure_xserver` (:1747) |
| `xserver_status` (:1804) | — | `Option<String>`（DISPLAY，未运行 None） | XServerState |
| `xserver_backend` (:1818) | — | `Option<String>`（"internal"/"external"） | XServerState |
| `xserver_get_config` / `xserver_save_config` (:1829/:1835) | `Vec<XServerConfig>` | `Vec<XServerConfig>` / `Result<(),String>` | xserver.rs |
| `xserver_stop` (:1840) | — | `Result<(),String>` | External.stop / Internal.shutdown |
| `x11_attach_viewer` (:1868) | `frame_channel: Channel<InvokeResponseBody>` | `Result<(),String>` | Internal 后端；外部后端报 `xserver-external-backend` |
| `x11_detach_viewer` (:1878) | — | `Result<(),String>` | 同上 |
| `x11_send_input` (:1885) | `events: Vec<xsrv::XInputEvent>` | `Result<(),String>` | HostServer::send_input |
| `x11_screen_size` (:1894) | — | `Result<(u16,u16),String>` | HostServer::screen_size |
| `setup_x11` (:1309) | `session_id: String` | `Result<String/*DISPLAY*/,String>` | XServerState + SshHandle.client.tcpip_forward |
| `ssh_connect / ssh_connect_key / ssh_connect_jump` (:728/:748/:767) | `..., x11: Option<bool>` | `Result<(),String>`；副作用：emit `ssh:x11` {status, display} | prepare_x11_for_ssh + attach_x11_route |

## 6. 外部 VcXsrv 方案（xserver.rs，335 行）概要

- **display 分配**：`free_display()`（:205）从 :1..=:20 试绑 `0.0.0.0:6000+N`（绑 0.0.0.0 是为了检出 VcXsrv 已占用的端口），成功即分配；`x11_port = 6000+display`，DISPLAY = `127.0.0.1:{display}.0`。
- **程序发现**：用户配置 `xserver.json`（config 目录，`XServerConfig{program, args}`）优先；否则 Windows 候选 Xming→VcXsrv 四个安装路径，非 Windows 用 Xvfb。
- **启动参数**：`:{display}` + 候选默认 `["-multiwindow", "-clipboard", "-ac"]`。`-ac` 关访问控制，免去 xauth 整套配置。
- **就绪探测**：`x11_handshake_ok`（:237）——连 TCP 后**等服务端先发 12 字节**，校验 `buf[0]==0x01 && buf[2]==11`（major version 在第 3 字节；TCP 可达≠X server），3s 读超时，总 deadline 10s，每 150ms 重试；期间进程提前退出/超时都会清理并报错（错误码 `xserver-not-found` / `xserver-no-free-display` / `xserver-spawn-failed:*` / `xserver-ready-timeout`）。
- **进程模型坑**：VcXsrv 会 fork 子进程，只 kill 父进程留孤儿占端口——`stop` 用 `taskkill /T /F /PID`（:39-52），`start` 前先 `kill_leftover_xservers`（:266）taskkill /IM 清残留。
- **是否保留外部回退（建议）**：**保留，价值明确**。① 原工程的设计理由在 gpui 版同样成立：内建服务端未实现扩展（BIG-REQUESTS 都不支持），某些 toolkit 起不来，外部程序是"能用"的最后保障（lib.rs:1743-1746 注释原话）。② 实现与 GUI 框架零耦合（纯 std::process + tokio TCP 探测），移植成本 ≈0，唯一 tauri 无关。③ gpui 工程当前没有任何 x11 回退，一旦内建版跑不动 xterm/Xt 系应用将无路可走。

## 7. 移植建议（分档 + 顺序）

### 7.1 分档

| 文件 | 档位 | 工作量 | 说明 |
|---|---|---|---|
| mod/server/state/request/draw/raster/input/handshake/font/font_data/misc/error（12 个） | **直接复制**（已完成，哈希一致） | 0 | 零外部耦合；只需 Cargo.toml 加 x11rb-protocol |
| Cargo.toml | 小改 | 5 分钟 | `x11rb-protocol = { version="0.13", default-features=false, features=["std","request-parsing"] }` |
| viewer.rs | **小改** | ~1h | 删 tauri Channel → `UnboundedSender<Vec<u8>>`（或自定义 FrameSink trait）；决策：JPEG 路径改 RGBA 直出（建议，省 image 依赖与编解码，直接对齐 RdEvent::Frame） |
| host.rs | **小改** | ~1-2h | 删 tauri Channel；`XInputEvent` 去 serde（或留 serde）；其余逻辑（bind_display/serve_channel/send_input/shutdown）原样可用 |
| **新增** xsrv↔gpui 桥（帧泵 → gpui 纹理 + 输入） | **重写（新代码）** | 1-2 天 | gpui 侧：收 Frame 事件上纹理（Entity/ImageSource），XInputEvent 从 gpui 键鼠事件转换（可借力 `RdCommand`→`XInputEvent` 映射，工程已有 keysym 归一化 `remote/mod.rs map_key/printable_keysym`） |
| ssh.rs X11 部分（cookie/x11-req/兜底/X11Channel） | **小改→重写** | 半天-1 天 | 逻辑照抄但落到 gpui 的 backends/ssh.rs；需要先移植 hostkey.rs 的 `X11Channel`/`with_x11_sink`/`server_channel_open_x11/forwarded_tcpip`（约 60 行）；`emit("ssh:x11")` 改回调/状态字段 |
| lib.rs 路由层（XBackend/X11Route/ensure_xserver/spawn_x11_router/setup_x11/命令） | **重写** | 1 天 | 同样的分层，但挂到 gpui 的 app/session 管理；无 tauri State，改为自有 struct + Arc |
| xserver.rs（外部回退） | **直接复制** | ~0 | 无 tauri/GUI 依赖（serde_json/directories 需确认 gpui 有；或砍掉用户配置功能只留候选发现） |

### 7.2 移植顺序（最快端到端跑通 xeyes）

1. **加依赖 + 编译现有 src/xsrv/**（Cargo.toml + 12 个免改文件）——立刻得到可测试的核心。先用模块内 `serve_tcp` + 临时 `DISPLAY=127.0.0.1:N.0` 对本机 WSL 跑 xeyes 验证语义层完好（server.rs 测试已内置大量互操作断言）。
2. **改 host.rs + viewer.rs**：viewer 帧通道换 `UnboundedSender<Vec<u8>>`，帧格式先保 JPEG 也可以（gpui 解码后上纹理）；此时起 `HostServer` + 写一个临时 gpui 渲染面，本地 TCP 路径端到端看到画面。**这是第一个可见里程碑。**
3. **输入桥**：RdCommand→XInputEvent 映射 + gpui 事件接线 → xeyes 能跟随鼠标、点击。
4. **SSH 链路**：hostkey.rs 的 X11 通道接收（x11 + forwarded-tcpip）→ backends/ssh.rs 的 `request_x11` + 兼容模式兜底 → 路由层 `spawn_x11_router`。此后远端 `xeyes` 经 x11-req 直连内建 server。
5. **可选**：setup_x11 等价命令、外部 VcXsrv 回退（xserver.rs 原样拷）、把 JPEG 换成 RGBA 直出对齐 RdEvent::Frame。

### 7.3 移植时需要保留的关键坑位清单（踩过的坑，勿丢）

- x11-req 必须在 request_shell 之前发（ssh.rs:262 注释）。
- 反向端口 p → DISPLAY 用 `localhost:{p-6000}.0`，且只收 6001..=65535（display 编号 = 端口-6000）。
- 注入事件必须盖写该客户端当前 sequence（否则 XCB abort，server.rs:461-467）。
- 输入批量在单锁内按序执行（Motion/Button 顺序敏感）。
- VcXsrv 需 taskkill /T 杀进程树 + 启动前清残留。
- 就绪探测校验握手第 3 字节 major=11，而非只看 TCP 可达。
- BIG-REQUESTS（total_len==0）暂不支持，须断开连接而非错位解析。
