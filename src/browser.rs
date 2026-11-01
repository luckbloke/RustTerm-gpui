//! 内置浏览器（WebView2 子窗口），每个浏览器标签页持有独立 WebView。
//!
//! 标签页内嵌在主窗口客户区：WebView 作为主窗口的子窗口，通过
//! `set_bounds` 与 gpui 绘制的占位矩形对齐，实现"浏览器像普通标签页一样
//! 出现在标签栏里"的观感。
//!
//! ## 为什么 WebView 必须跑在独立线程
//!
//! wry 创建 WebView2 时会调用 `webview2_com::wait_with_pump` —— 它在**调用线程**
//! 上 `GetMessage` + `DispatchMessage`，等待 WebView2 的 COM 回调。如果这个调用
//! 发生在 gpui 的渲染帧里，消息泵会把 gpui 自己的 `WM_PAINT` 也派发出去，
//! 导致 gpui **重入渲染**，帧调度器的 `AppCell` 二次借用直接 panic，
//! 表现为「点一下浏览器就崩」/「卡死刷 RefCell already borrowed」。
//!
//! 所以这里把 WebView 的创建与全部调用都放到一个**专属线程**上：
//! 该线程自己跑消息泵，gpui 主线程只通过 channel 下发命令、读取事件，
//! 两边彻底解耦。
//!
//! 其他要点：
//! - 非活动标签页的 WebView 会被 `set_visible(false)` 隐藏，避免叠在终端上。
//! - WebView 事件（标题/URL）通过 mpsc 通道回传，避免闭包借用 self。

use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use urlencoding;

/// about:blank 新标签页展示的欢迎页（真实 WebView 中渲染）
const WELCOME_HTML: &str = r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>新标签页</title>
<style>
  html,body{margin:0;height:100%;background:#1e1f24;color:#d0d0d5;
    font-family:"Segoe UI","Microsoft YaHei",sans-serif;
    display:flex;flex-direction:column;align-items:center;justify-content:center;}
  h1{font-size:42px;font-weight:300;margin-bottom:8px;color:#e8e8ee;}
  p{color:#8a8a95;margin:4px 0;}
  .tip{margin-top:28px;font-size:13px;color:#6a6a75;}
  .keys{margin-top:8px;font-size:12px;color:#55555f;}
</style></head>
<body>
  <h1>RustTerm 浏览器</h1>
  <p>基于 WebView2 的内置浏览器</p>
  <div class="tip">在上方地址栏输入网址或搜索关键词，回车访问</div>
  <div class="keys">Ctrl+T 新标签页 · Ctrl+W 关闭标签 · F5 刷新 · Ctrl+H 历史</div>
</body></html>"#;

/// UI → WebView 线程的命令。
#[derive(Debug)]
enum WebCmd {
    /// 设置子窗口边界（客户区逻辑坐标）
    SetBounds { x: f32, y: f32, w: f32, h: f32 },
    SetVisible(bool),
    LoadUrl(String),
    /// 直接渲染一段 HTML（欢迎页）
    LoadHtml(String),
    Reload,
    /// 执行一段 JS（后退/前进走 history API）
    EvalScript(String),
    Shutdown,
}

/// WebView 线程 → UI 的回传事件
#[derive(Debug)]
enum WebEvent {
    /// 页面标题变化
    Title(String),
    /// 页面 URL 导航变化
    Url(String),
    /// WebView 创建成功
    Ready,
    /// WebView 创建失败（带可读原因）
    Failed(String),
}

/// 主窗口 HWND（由 app 首帧注册到 win32 注册表）。
pub(crate) fn main_hwnd() -> Option<isize> {
    crate::win32::main_hwnd()
}

/// 把键盘焦点交回主窗口。
///
/// WebView2 子窗口创建后会抢占焦点，导致主窗口的输入框
/// （如地址栏）收不到键盘事件。点击地址栏 / 按 Ctrl+L 时必须抢回来。
pub fn focus_main_window() {
    crate::win32::focus_main_window();
}

/// 书签
#[derive(Clone, Debug)]
pub struct Bookmark {
    pub name: String,
    pub url: String,
    pub folder: String,
}

/// WebView 专属线程的运行时句柄。
///
/// WebView2 的 COM 对象绑定在创建它的线程（STA 单元）上，所有调用都必须
/// 回到那个线程执行，因此这里只保留 channel 与线程 id。
struct BrowserRuntime {
    cmd_tx: Sender<WebCmd>,
    /// WebView 线程的线程 id，用于 `PostThreadMessage` 唤醒消息循环。
    ///
    /// 由 WebView 线程**启动时**直接写入（而不是等 UI 轮询事件），
    /// 否则非活动标签页永远学不到 id → 命令发不出去（关标签时 Shutdown
    /// 不生效，WebView 子窗口会一直挂在那里）。
    thread_id: Arc<AtomicU32>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl BrowserRuntime {
    /// 下发命令并唤醒 WebView 线程的消息循环。
    fn send(&self, cmd: WebCmd) {
        if self.cmd_tx.send(cmd).is_ok() {
            wake(self.thread_id.load(Ordering::Acquire));
        }
    }
}

/// 唤醒 WebView 线程的消息循环（让它立刻处理刚下发的命令）。
fn wake(thread_id: u32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::PostThreadMessageW;
    const WM_APP_WAKE: u32 = 0x8000 + 42;
    if thread_id == 0 {
        return;
    }
    unsafe {
        PostThreadMessageW(thread_id as _, WM_APP_WAKE, 0, 0);
    }
}

/// 单个浏览器标签页：状态 + 自己的 WebView（运行在专属线程上）。
pub struct BrowserTab {
    pub id: usize,
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub history: VecDeque<String>,
    pub history_index: usize,
    /// 地址栏编辑缓冲：必须与 url 分开保存，否则每帧会被 url 覆盖，
    /// 导致用户输入被丢弃。
    pub address_bar: String,
    /// 地址栏当前是否获得焦点（获得焦点时不用 url 覆盖编辑缓冲）
    pub address_focused: bool,
    /// WebView 运行时（惰性创建）
    runtime: Option<BrowserRuntime>,
    /// WebView 事件接收端
    event_rx: Option<Receiver<WebEvent>>,
    /// WebView2 初始化失败原因
    pub webview_error: Option<String>,
    /// 是否已经发起过创建（避免重复起线程）
    creating: bool,
    /// WebView 是否已就绪（收到 Ready 事件）
    pub ready: bool,
    /// 最近一次内容区矩形（WebView 就绪后需要重新对齐一次）
    last_area: Option<gpui_kit::Bounds<gpui_kit::Pixels>>,
    /// 上一次设置的边界（逻辑坐标），避免每帧重复 set_bounds
    last_px: Option<[f32; 4]>,
    /// 上一次的可见状态（避免每帧重复下发）
    visible: Option<bool>,
    /// 地址栏输入实体（gpui，惰性创建）
    pub address_input: Option<gpui_kit::Entity<gpui_kit::component::input::InputState>>,
}

impl BrowserTab {
    pub fn new(id: usize, url: String) -> Self {
        let mut tab = Self {
            id,
            url: url.clone(),
            title: "新标签页".to_string(),
            loading: false,
            can_go_back: false,
            can_go_forward: false,
            history: VecDeque::new(),
            history_index: 0,
            address_bar: url.clone(),
            address_focused: false,
            runtime: None,
            event_rx: None,
            webview_error: None,
            creating: false,
            ready: false,
            last_area: None,
            last_px: None,
            visible: None,
            address_input: None,
        };
        tab.history.push_back(url);
        tab.update_title();
        tab
    }

    fn update_title(&mut self) {
        if self.url == "about:blank" || self.url.is_empty() {
            self.title = "新标签页".to_string();
        } else if let Ok(parsed) = url::Url::parse(&self.url) {
            self.title = parsed
                .host_str()
                .unwrap_or(self.url.as_str())
                .to_string();
        } else {
            self.title = self.url.clone();
        }
    }

    /// 标签栏上显示的名字（页面标题优先，回退到 URL）。
    pub fn display_title(&self) -> String {
        if self.title == "新标签页" && self.url != "about:blank" && !self.url.is_empty() {
            self.url.clone()
        } else {
            self.title.clone()
        }
    }

    pub fn navigate(&mut self, url: String) {
        while self.history.len() > self.history_index + 1 {
            self.history.pop_back();
        }
        self.history.push_back(url.clone());
        self.history_index = self.history.len() - 1;
        self.url = url;
        self.update_title();
        self.can_go_back = self.history_index > 0;
        self.can_go_forward = self.history_index < self.history.len() - 1;
        self.address_bar = self.url.clone();
    }

    /// 记录一次**非地址栏发起**的导航（页面内链接跳转、后退、前进）。
    ///
    /// 判定规则：
    /// - 命中上一条 → 认为是「后退」，指针左移；
    /// - 命中下一条 → 认为是「前进」，指针右移；
    /// - 否则 → 新增历史条目（并截断「前进」分支）。
    ///
    /// 这样后退 / 前进按钮既走 WebView 自己的历史（保留页面状态），
    /// 又能让地址栏和「能否后退」的指示保持同步。
    fn record_navigation(&mut self, url: &str) {
        if self.history_index > 0 && self.history[self.history_index - 1] == url {
            self.history_index -= 1;
        } else if self.history_index + 1 < self.history.len()
            && self.history[self.history_index + 1] == url
        {
            self.history_index += 1;
        } else {
            while self.history.len() > self.history_index + 1 {
                self.history.pop_back();
            }
            self.history.push_back(url.to_string());
            self.history_index = self.history.len() - 1;
        }
        self.url = url.to_string();
        self.update_title();
        self.can_go_back = self.history_index > 0;
        self.can_go_forward = self.history_index + 1 < self.history.len();
        self.address_bar = self.url.clone();
    }

    /// 后退（走 WebView 自己的历史，保留滚动位置等页面状态）。
    pub fn go_back_web(&mut self) {
        if !self.can_go_back {
            return;
        }
        if let Some(rt) = &self.runtime {
            rt.send(WebCmd::EvalScript("window.history.back()".into()));
        }
    }

    /// 前进（走 WebView 自己的历史）。
    pub fn go_forward_web(&mut self) {
        if !self.can_go_forward {
            return;
        }
        if let Some(rt) = &self.runtime {
            rt.send(WebCmd::EvalScript("window.history.forward()".into()));
        }
    }

    /// 惰性启动 WebView 线程。
    ///
    /// **绝不能**在渲染帧里同步创建 WebView：wry 会抽消息泵，重入 gpui 渲染
    /// 会直接把帧调度器的 App 借用搞崩。这里改成后台线程创建，UI 侧
    /// 只显示「正在创建 WebView2…」占位，等 Ready 事件到了再对齐边界。
    pub fn ensure_webview(&mut self) {
        if self.runtime.is_some() || self.webview_error.is_some() || self.creating {
            return;
        }
        let Some(hwnd) = main_hwnd() else {
            self.webview_error = Some("无法获取主窗口句柄".to_string());
            return;
        };
        self.creating = true;

        let (cmd_tx, cmd_rx) = channel::<WebCmd>();
        let (ev_tx, ev_rx) = channel::<WebEvent>();
        let url = self.url.clone();
        let thread_id = Arc::new(AtomicU32::new(0));
        let tid = thread_id.clone();

        let join = std::thread::Builder::new()
            .name(format!("webview-{}", self.id))
            .spawn(move || {
                webview_thread(hwnd, url, cmd_rx, ev_tx, tid);
            })
            .ok();

        let Some(join) = join else {
            self.creating = false;
            self.webview_error = Some("无法创建 WebView 线程".to_string());
            return;
        };

        self.event_rx = Some(ev_rx);
        self.runtime = Some(BrowserRuntime {
            cmd_tx,
            thread_id,
            join: Some(join),
        });
    }

    /// 轮询 WebView 事件，同步标题 / 地址 / 就绪状态。
    pub fn poll_events(&mut self) {
        let events: Vec<WebEvent> = match &self.event_rx {
            Some(rx) => {
                let mut v = Vec::new();
                while let Ok(ev) = rx.try_recv() {
                    v.push(ev);
                }
                v
            }
            None => return,
        };
        if events.is_empty() {
            return;
        }
        let mut loading_done = false;
        for ev in events {
            match ev {
                WebEvent::Title(t) => {
                    if self.title != t {
                        self.title = t;
                    }
                    loading_done = true;
                }
                WebEvent::Url(u) => {
                    // 忽略内部 data: 导航（load_html 产生）
                    if u.starts_with("data:") {
                        continue;
                    }
                    if self.url != u {
                        // 页面内跳转 / 后退 / 前进：记进历史，地址栏同步
                        self.record_navigation(&u);
                        // 用户正在地址栏输入时不要覆盖
                        if !self.address_focused {
                            self.address_bar = self.url.clone();
                        }
                    }
                    loading_done = false;
                }
                WebEvent::Ready => {
                    self.creating = false;
                    self.ready = true;
                    // 就绪后重新对齐一次边界（创建时可能还没有内容区矩形）
                    self.last_px = None;
                    if let Some(area) = self.last_area {
                        self.apply_bounds(area);
                    }
                    // 补发可见状态：标签页激活时的命令是在 WebView 起来之前
                    // 发的，那会儿没有 runtime，命令会丢
                    self.push_visible();
                    // WebView2 创建后会抢焦点，交回主窗口
                    focus_main_window();
                }
                WebEvent::Failed(msg) => {
                    self.creating = false;
                    self.ready = false;
                    self.webview_error = Some(msg);
                }
            }
        }
        if loading_done {
            self.loading = false;
        }
    }

    /// 把内容区矩形同步到 WebView 子窗口边界。
    ///
    /// 坐标系说明：
    /// - gpui 的 `Bounds<Pixels>` 以**逻辑点**表示，原点在客户区左上角。
    /// - `SetWindowPos` 设置**子窗口**位置时使用父窗口客户区坐标，
    ///   因此不能加屏幕原点偏移（加了就会整体偏移）。
    /// - wry 的 `LogicalPosition`/`LogicalSize` 会再乘一次 DPI 缩放因子，
    ///   正好把逻辑点换算成物理像素。
    pub fn sync_bounds(&mut self, rect: gpui_kit::Bounds<gpui_kit::Pixels>) {
        self.last_area = Some(rect);
        self.apply_bounds(rect);
    }

    fn apply_bounds(&mut self, rect: gpui_kit::Bounds<gpui_kit::Pixels>) {
        let lx = rect.origin.x.as_f32();
        let ly = rect.origin.y.as_f32();
        let lw = rect.size.width.as_f32().max(1.0);
        let lh = rect.size.height.as_f32().max(1.0);
        let bounds = [lx, ly, lw, lh];
        if self.last_px == Some(bounds) {
            return;
        }
        self.last_px = Some(bounds);
        if let Some(rt) = &self.runtime {
            rt.send(WebCmd::SetBounds { x: lx, y: ly, w: lw, h: lh });
        }
    }

    /// WebView 是否已成功创建。
    pub fn has_webview(&self) -> bool {
        self.ready
    }

    /// 显示 / 隐藏 WebView（非活动标签页必须隐藏，否则会盖住终端）。
    pub fn set_visible(&mut self, visible: bool) {
        // 只在状态变化时下发，避免每帧都往 WebView 线程塞消息
        if self.visible == Some(visible) {
            return;
        }
        self.visible = Some(visible);
        self.push_visible();
    }

    /// 把当前期望的可见状态推给 WebView 线程。
    ///
    /// WebView 是异步创建的：标签页刚激活时 `runtime` 还不存在，
    /// 那时的可见性命令会丢失。所以 WebView 就绪后必须**补发一次**，
    /// 否则子窗口会一直保持隐藏（表现为内容区一片空白）。
    fn push_visible(&self) {
        if let (Some(rt), Some(v)) = (&self.runtime, self.visible) {
            rt.send(WebCmd::SetVisible(v));
        }
    }

    pub fn reload(&mut self) {
        self.loading = true;
        if let Some(rt) = &self.runtime {
            rt.send(WebCmd::Reload);
        } else {
            self.loading = false;
        }
    }

    pub fn load_url(&mut self, url: &str) {
        if let Some(rt) = &self.runtime {
            if url == "about:blank" || url.is_empty() {
                rt.send(WebCmd::LoadHtml(WELCOME_HTML.to_string()));
            } else {
                rt.send(WebCmd::LoadUrl(url.to_string()));
            }
        }
    }

    /// 标签页关闭时销毁 WebView（线程退出 → WebView 在自己的线程上被 drop）。
    ///
    /// **绝不能在主线程 join**：WebView 线程销毁子窗口时，`DestroyWindow` 会向
    /// 父窗口（gpui 主窗口）**同步**投递 `WM_PARENTNOTIFY`，`controller.Close()`
    /// 也可能有依赖父线程消息泵的回调。主线程一旦卡在 `join()` 里不再抽消息，
    /// 两边就互等 —— 表现为「点关闭标签，整个程序未响应」。
    ///
    /// 因此这里只发命令 + 断开引用：线程在后台自己 `drop(webview)`，
    /// 主线程立刻返回继续渲染。`JoinHandle` 直接丢弃（= detach）。
    pub fn shutdown(&mut self) {
        if let Some(mut rt) = self.runtime.take() {
            // 先隐藏再退出，避免子窗口在销毁前的几毫秒里盖在界面上
            rt.send(WebCmd::SetVisible(false));
            rt.send(WebCmd::Shutdown);
            // JoinHandle 在此 drop → detach，不阻塞调用线程
            let _ = rt.join.take();
        }
        self.event_rx = None;
        self.last_px = None;
        self.last_area = None;
        self.ready = false;
        self.creating = false;
    }
}

impl Drop for BrowserTab {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// WebView 专属线程
// ---------------------------------------------------------------------------

/// WebView 线程主体：创建 WebView → 进入消息循环 → 按命令驱动。
///
/// 这一层是整个浏览器能跑起来的关键：
/// - wry 抽消息泵只会派发**本线程**的消息，gpui 主线程不会被重入；
/// - WebView2 的 COM 回调（标题变化 / 导航）也能正常送达。
fn webview_thread(
    parent_hwnd: isize,
    url: String,
    cmd_rx: Receiver<WebCmd>,
    ev_tx: Sender<WebEvent>,
    thread_id: Arc<AtomicU32>,
) {
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    // 线程 id 第一时间写入共享原子量：UI 侧靠它 PostThreadMessage 唤醒消息循环。
    // 不能走事件通道 —— 非活动标签页不轮询事件，会一直拿不到 id。
    thread_id.store(unsafe { GetCurrentThreadId() }, Ordering::Release);

    // 把 HWND 包装成 HasWindowHandle
    struct HwndParent(raw_window_handle::Win32WindowHandle);
    impl raw_window_handle::HasWindowHandle for HwndParent {
        fn window_handle(
            &self,
        ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError>
        {
            Ok(unsafe {
                raw_window_handle::WindowHandle::borrow_raw(
                    raw_window_handle::RawWindowHandle::Win32(self.0),
                )
            })
        }
    }
    let Some(nz) = std::num::NonZeroIsize::new(parent_hwnd) else {
        let _ = ev_tx.send(WebEvent::Failed("主窗口句柄无效".to_string()));
        return;
    };
    let parent = HwndParent(raw_window_handle::Win32WindowHandle::new(nz));

    // 标题 / 导航事件直接复用 UI 的事件通道（所有事件一条道）
    let tx_title = ev_tx.clone();
    let tx_nav = tx_title.clone();

    let mut builder = wry::WebViewBuilder::new()
        .with_transparent(false)
        .with_navigation_handler(move |u: String| {
            // load_html 会产生 data:text/html 导航，不应覆盖用户可见的地址
            if !u.starts_with("data:") {
                let _ = tx_nav.send(WebEvent::Url(u));
            }
            true
        })
        .with_document_title_changed_handler(move |t: String| {
            let _ = tx_title.send(WebEvent::Title(t));
        });
    if url != "about:blank" && !url.is_empty() {
        builder = builder.with_url(url.clone());
    }

    // WebView2 环境初始化在某些机器上会直接 panic（例如未安装
    // WebView2 Runtime，或 wry 内部 unwrap 失败），而非返回 Err。
    // 这里用 catch_unwind 兜底，把 panic 转成可读错误。
    let res = panic::catch_unwind(AssertUnwindSafe(|| builder.build_as_child(&parent)));
    let webview = match res {
        Ok(Ok(wv)) => wv,
        Ok(Err(e)) => {
            let _ = ev_tx.send(WebEvent::Failed(format!(
                "WebView2 初始化失败: {e}\n\n请安装 Microsoft Edge WebView2 Runtime 后重试。"
            )));
            return;
        }
        Err(_) => {
            let _ = ev_tx.send(WebEvent::Failed(
                "WebView2 初始化时发生内部错误（panic）。\n\n请确认已安装 Microsoft Edge WebView2 Runtime，然后重试。"
                    .to_string(),
            ));
            return;
        }
    };

    if url == "about:blank" || url.is_empty() {
        let _ = webview.load_html(WELCOME_HTML);
    }
    // 初始隐藏，等标签页被激活时再显示
    let _ = webview.set_visible(false);
    let _ = ev_tx.send(WebEvent::Ready);

    // ---- 消息循环：处理命令 + 派发 WebView2 的窗口消息 ----
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, TranslateMessage, MSG,
    };
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        // 先把命令处理干净
        let mut shutdown = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                WebCmd::SetBounds { x, y, w, h } => {
                    let _ = webview.set_bounds(wry::Rect {
                        position: wry::dpi::LogicalPosition::new(x, y).into(),
                        size: wry::dpi::LogicalSize::new(w, h).into(),
                    });
                }
                WebCmd::SetVisible(v) => {
                    let _ = webview.set_visible(v);
                }
                WebCmd::LoadUrl(u) => {
                    let _ = webview.load_url(&u);
                }
                WebCmd::LoadHtml(h) => {
                    let _ = webview.load_html(&h);
                }
                WebCmd::Reload => {
                    let _ = webview.evaluate_script("location.reload()");
                }
                WebCmd::EvalScript(s) => {
                    let _ = webview.evaluate_script(&s);
                }
                WebCmd::Shutdown => {
                    shutdown = true;
                    break;
                }
            }
        }
        if shutdown {
            break;
        }
        // 阻塞等消息（命令到达时 UI 侧会 PostThreadMessage 唤醒）
        let r = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if r <= 0 {
            break;
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    // webview 在本线程 drop → WebView2 控制器正常释放
    drop(webview);
}

/// 浏览器共享状态（书签、历史、搜索引擎）——所有浏览器标签页共用。
#[derive(Clone)]
pub struct BrowserService {
    pub bookmarks: Vec<Bookmark>,
    pub history: VecDeque<String>,
    pub search_engine: String,
    pub show_bookmarks: bool,
}

impl Default for BrowserService {
    fn default() -> Self {
        Self {
            bookmarks: vec![
                Bookmark { name: "GitHub".into(), url: "https://github.com".into(), folder: "开发".into() },
                Bookmark { name: "Rust".into(), url: "https://rust-lang.org".into(), folder: "开发".into() },
                Bookmark { name: "Crates.io".into(), url: "https://crates.io".into(), folder: "开发".into() },
                Bookmark { name: "Google".into(), url: "https://google.com".into(), folder: "搜索".into() },
                Bookmark { name: "Bing".into(), url: "https://www.bing.com".into(), folder: "搜索".into() },
            ],
            history: VecDeque::new(),
            search_engine: "https://www.bing.com/search?q=".to_string(),
            show_bookmarks: false,
        }
    }
}

impl BrowserService {
    pub fn add_to_history(&mut self, url: String) {
        if !url.starts_with("about:") {
            self.history.push_front(url);
            if self.history.len() > 1000 {
                self.history.pop_back();
            }
        }
    }

    /// 把地址栏输入解析为 URL（域名直接访问，其余按搜索处理）。
    pub fn resolve_address(&self, input: &str) -> String {
        let input = input.trim();
        if input.starts_with("http://")
            || input.starts_with("https://")
            || input.starts_with("file://")
            || input.starts_with("about:")
        {
            return input.to_string();
        }
        if input.contains('.') && !input.contains(' ') && !input.contains('/') {
            format!("https://{input}")
        } else {
            format!("{}{}", self.search_engine, urlencoding::encode(input))
        }
    }
}
