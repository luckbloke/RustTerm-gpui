//! 「端口扫描」对话框（工具菜单）：IP 范围 / CIDR + 端口列表（含范围段的 TCP 扫描）。
//!
//! 并发模型对齐主工程 `src-tauri/src/portscan.rs`：并发数 = 同时打开的连接数。
//! 线程版用 worker 池直接等量限流（11 个 worker 最多 11 个并发 connect），
//! 不至于扫一个 /24 就耗尽本机文件描述符或被对端当成攻击。
//! 停止用 `Arc<AtomicBool>`；未完成的作业在停止后不再回传。

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// 单个开放端口的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenPort {
    pub ip: String,
    pub port: u16,
    /// 从发起连接到握手成功的毫秒数，便于判断网络质量
    pub latency_ms: u64,
}

/// worker 回传到 UI 线程的消息。
enum ScanMsg {
    Hit(OpenPort),
    /// 完成一个 (IP, 端口) 对
    Prog,
    Done(Option<String>), // Some(错误) = 异常收尾；None = 正常结束
}

pub struct PortScanPanel {
    pub target: String,
    pub ports: String,
    pub concurrency: usize,
    pub timeout_ms: u64,

    running: bool,
    /// 已完成探测数 / 总探测数
    pub scanned: u64,
    pub total: u64,
    /// 扫到的开放端口（完成时排序去重）
    pub hits: Vec<OpenPort>,
    /// 当前选中的命中行（高亮显示）
    pub selected: Option<usize>,
    pub error: Option<String>,
    stop: Arc<AtomicBool>,
    rx: Option<Receiver<ScanMsg>>,
    /// 对话框输入实体（打开时播种，关闭时丢弃）
    pub inputs: Option<ScanInputs>,
}

impl Default for PortScanPanel {
    fn default() -> Self {
        Self {
            target: String::new(),
            ports: String::new(),
            concurrency: 256,
            timeout_ms: 800,
            running: false,
            scanned: 0,
            total: 0,
            hits: Vec::new(),
            selected: None,
            error: None,
            stop: Arc::new(AtomicBool::new(false)),
            rx: None,
            inputs: None,
        }
    }
}

/// 简易作业通道：发送端全部 drop 即关闭，多个接收端各取各的作业。
mod job_chan {
    use std::collections::VecDeque;
    use std::sync::{Arc, Condvar, Mutex};

    #[derive(Clone)]
    pub struct JobSender<T> {
        inner: Arc<Inner<T>>,
    }
    #[derive(Clone)]
    pub struct JobReceiver<T> {
        inner: Arc<Inner<T>>,
    }
    struct Inner<T> {
        q: Mutex<VecDeque<T>>,
        cv: Condvar,
        closed: Mutex<bool>,
    }

    impl<T> JobSender<T> {
        pub fn send(&self, item: T) -> Result<(), ()> {
            if *self.inner.closed.lock().unwrap() {
                return Err(());
            }
            self.inner.q.lock().unwrap().push_back(item);
            self.inner.cv.notify_one();
            Ok(())
        }
    }
    impl<T> JobReceiver<T> {
        /// 阻塞取一条；通道关闭且取空 → None。
        pub fn recv(&self) -> Option<T> {
            let mut q = self.inner.q.lock().unwrap();
            loop {
                if let Some(v) = q.pop_front() {
                    return Some(v);
                }
                if *self.inner.closed.lock().unwrap() {
                    return None;
                }
                q = self.inner.cv.wait(q).unwrap();
            }
        }
    }

    pub fn pair<T>() -> (JobSender<T>, JobReceiver<T>) {
        let inner = Arc::new(Inner {
            q: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            closed: Mutex::new(false),
        });
        (JobSender { inner: inner.clone() }, JobReceiver { inner })
    }

    impl<T> Drop for JobSender<T> {
        fn drop(&mut self) {
            // 全部发送端消失才关闭；克隆共享同一 Inner
            *self.inner.closed.lock().unwrap() = true;
            self.inner.cv.notify_all();
        }
    }
}

impl PortScanPanel {
    /// 轮询 worker 消息。app 层每帧调用。
    ///
    /// 返回值表示「本轮是否有新数据」——UI 层据此决定要不要重绘。
    /// 扫描在后台线程跑，命中是**异步**送回来的：不看这个返回值，
    /// 面板上的进度与命中列表会一直停在 0。
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else { return false };
        let mut changed = false;
        let mut done_msg: Option<Option<String>> = None;
        while let Ok(msg) = rx.try_recv() {
            changed = true;
            match msg {
                ScanMsg::Hit(h) => self.hits.push(h),
                ScanMsg::Prog => self.scanned += 1,
                ScanMsg::Done(err) => done_msg = Some(err),
            }
        }
        if let Some(err) = done_msg {
            self.running = false;
            self.rx = None;
            if let Some(e) = err {
                self.error = Some(e);
            } else {
                // 各 worker 完成时间不同顺序不稳：完成后排序去重
                self.hits
                    .sort_by(|a, b| (&a.ip, a.port).cmp(&(&b.ip, b.port)));
                self.hits.dedup();
            }
        }
        changed
    }

    /// 是否正在跑（app 层关窗前据此先停止）。
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// 校验并启动扫描。返回 Err(错误信息)。
    pub fn start(&mut self) -> Result<(), String> {
        if self.running {
            return Ok(());
        }
        let ips = parse_targets(&self.target).map_err(|e| format!("目标格式错误：{e}"))?;
        let port_list = parse_ports(&self.ports).map_err(|e| format!("端口格式错误：{e}"))?;
        let total = ips.len().saturating_mul(port_list.len());
        // 上限保护：单次扫描 IP×端口 不超过 65536（对齐主工程）
        if total > 65536 {
            return Err(format!("组合数过大（{total}），请缩小目标或端口范围"));
        }
        if total == 0 {
            return Err("没有可扫描的目标".into());
        }
        // 并发上限默认 256：单机扫描够快又不至于打爆对端；上限 1024。
        let workers = self.concurrency.clamp(1, 1024);
        let timeout = Duration::from_millis(self.timeout_ms.clamp(100, 5000));

        self.stop.store(false, Ordering::Relaxed);
        self.hits.clear();
        self.selected = None;
        self.error = None;
        self.scanned = 0;
        self.total = total as u64;
        self.running = true;

        let (tx, rx) = channel::<ScanMsg>();
        self.rx = Some(rx);
        let stop = Arc::clone(&self.stop);

        thread::spawn(move || {
            let (job_tx, job_rx) = job_chan::pair::<(Ipv4Addr, u16)>();

            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    let job_rx = job_rx.clone();
                    let tx = tx.clone();
                    let stop = Arc::clone(&stop);
                    thread::spawn(move || loop {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        let Some((ip, port)) = job_rx.recv() else { return };
                        // 连上了 → 端口开放；立刻 drop 连接，只关心握手
                        let addr = SocketAddr::new(IpAddr::V4(ip), port);
                        let start = Instant::now();
                        let latency = TcpStream::connect_timeout(&addr, timeout).ok().map(|_| {
                            start.elapsed().as_millis() as u64
                        });
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        if let Some(latency_ms) = latency {
                            let _ = tx.send(ScanMsg::Hit(OpenPort {
                                ip: ip.to_string(),
                                port,
                                latency_ms,
                            }));
                        }
                        let _ = tx.send(ScanMsg::Prog);
                    })
                })
                .collect();

            'outer: for ip in &ips {
                for port in &port_list {
                    if stop.load(Ordering::Relaxed) {
                        break 'outer;
                    }
                    if job_tx.send((*ip, *port)).is_err() {
                        break 'outer;
                    }
                }
            }
            drop(job_tx); // 关闭作业通道 → worker 取空后退出
            for h in handles {
                let _ = h.join();
            }
            let _ = tx.send(ScanMsg::Done(None));
        });
        Ok(())
    }

    /// 请求停止。
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// 渲染「端口扫描」面板（作为标签页内容铺满中央区）。
    pub fn render_panel(
        &mut self,
        window: &mut gpui_kit::Window,
        cx: &mut gpui_kit::Context<crate::app::RustTermApp>,
    ) -> gpui_kit::AnyElement {
        use gpui_kit::base::StyledExt as _;
        use gpui_kit::component::scroll::ScrollableElement as _;
        use gpui_kit::component::button::Button;
        use gpui_kit::component::input::{Input, InputState};
        use gpui_kit::component::theme::Theme;
        use gpui_kit::prelude::FluentBuilder;
        use gpui_kit::*;

        // 输入实体（首次渲染时播种）
        if self.inputs.is_none() {
            let mut mk = |text: String, placeholder: &str| {
                let e = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.to_string()));
                e.update(cx, |s, cx| s.set_value(text, window, cx));
                e
            };
            self.inputs = Some(ScanInputs {
                target: mk(self.target.clone(), "192.168.1.1-50 或 192.168.1.0/24"),
                ports: mk(self.ports.clone(), "22,80,443,3389,8000-8100"),
                concurrency: mk(format!("{}", self.concurrency), "256"),
                timeout_ms: mk(format!("{}", self.timeout_ms), "800"),
            });
        }
        let inputs = self.inputs.clone().unwrap();
        let theme = Theme::global(cx).clone();
        let dim = theme.colors.muted_foreground;

        let field = |label: &str, input: &gpui_kit::Entity<InputState>| {
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .child(div().w(px(70.0)).text_size(px(12.0)).child(label.to_string()))
                .child(Input::new(input).flex_1().text_size(px(12.0)))
        };

        // ---- 进度 + 结果 ----
        let mut results = div().v_flex().flex_1().min_h_0().overflow_y_scrollbar();
        for (i, h) in self.hits.iter().enumerate() {
            let selected = self.selected == Some(i);
            let ip = h.ip.clone();
            let ip_btn = ip.clone();
            let port = h.port;
            let latency = h.latency_ms;
            results = results.child(
                div()
                    .id(SharedString::from(format!("scan-hit-{i}")))
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_2()
                    .py_0p5()
                    .rounded(px(3.0))
                    .when(selected, |d| d.bg(theme.colors.secondary))
                    .hover(|d| d.bg(theme.colors.secondary_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                            // 单击选中；双击直接用该端口打开 SSH 连接表单
                            this.port_scan.selected = Some(i);
                            if ev.click_count >= 2 {
                                this.open_ssh_for(&ip, port);
                            }
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_ellipsis()
                            .child(format!("{}:{}", h.ip, h.port)),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(dim)
                            .child(format!("{latency} ms")),
                    )
                    .child(
                        Button::new(SharedString::from(format!("scan-connect-{i}")))
                            .icon(crate::ui_icons::ic(gpui_kit::assets::IconName::Plug))
                            .tooltip("用该端口打开 SSH 连接")
                            .compact()
                            .on_click(cx.listener(move |this, _, _, _| {
                                this.open_ssh_for(&ip_btn, port);
                            })),
                    ),
            );
        }
        if self.hits.is_empty() && !self.running {
            results = results.child(
                div()
                    .text_size(px(11.0))
                    .text_color(dim)
                    .child(if self.total > 0 {
                        "没有发现开放端口"
                    } else {
                        "暂无结果"
                    }),
            );
        }

        let progress = if self.total > 0 {
            format!(
                "{}/{}（{:.0}%）",
                self.scanned,
                self.total,
                100.0 * self.scanned as f32 / self.total as f32
            )
        } else {
            String::new()
        };

        let content = div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(field("目标", &inputs.target))
            .child(field("端口", &inputs.ports))
            .child(field("并发", &inputs.concurrency))
            .child(field("超时ms", &inputs.timeout_ms))
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        if self.running {
                            Button::new("scan-stop")
                                .label("停止")
                                .compact()
                                .on_click(cx.listener(|this, _, _, _| this.port_scan_stop()))
                                .into_any_element()
                        } else {
                            Button::new("scan-start")
                                .label("开始扫描")
                                .compact()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    // 输入值回填
                                    let get = |pick: fn(&ScanInputs) -> &gpui_kit::Entity<InputState>| -> String {
                                        this.port_scan
                                            .inputs
                                            .as_ref()
                                            .map(pick)
                                            .map(|s| s.read(cx).value().to_string())
                                            .unwrap_or_default()
                                    };
                                    this.port_scan.target = get(|i| &i.target);
                                    this.port_scan.ports = get(|i| &i.ports);
                                    this.port_scan.concurrency = get(|i| &i.concurrency)
                                        .trim()
                                        .parse()
                                        .unwrap_or(256);
                                    this.port_scan.timeout_ms = get(|i| &i.timeout_ms)
                                        .trim()
                                        .parse()
                                        .unwrap_or(800);
                                    if let Err(e) = this.port_scan.start() {
                                        this.port_scan.error = Some(e);
                                    }
                                    cx.notify();
                                }))
                                .into_any_element()
                        },
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(dim)
                            .child(progress),
                    ),
            )
            .when_some(self.error.clone(), |d, e| {
                d.child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.colors.danger)
                        .child(e),
                )
            })
            .child(
                div()
                    .border_t_1()
                    .border_color(theme.colors.border)
                    .pt_1()
                    .h_flex()
                    .justify_between()
                    .child(
                        div().text_size(px(11.0)).text_color(dim).child("开放端口"),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(dim)
                            .child(format!("{} 个", self.hits.len())),
                    ),
            )
            .child(results);

        div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .p_4()
            .gap_2()
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(15.0))
                            .text_color(theme.colors.foreground)
                            .child("端口扫描"),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(dim)
                            .child("双击结果可直接用该端口连接 SSH"),
                    ),
            )
            .child(content)
            .into_any_element()
    }
}

/// 扫描对话框输入实体（关闭时丢弃）。
#[derive(Clone)]
pub struct ScanInputs {
    pub target: gpui_kit::Entity<gpui_kit::component::input::InputState>,
    pub ports: gpui_kit::Entity<gpui_kit::component::input::InputState>,
    pub concurrency: gpui_kit::Entity<gpui_kit::component::input::InputState>,
    pub timeout_ms: gpui_kit::Entity<gpui_kit::component::input::InputState>,
}

/// 解析目标字段：范围（a-b，上限 4096 个地址）/ CIDR（/20 及以上，
/// 跳过网络地址与广播地址）/ 单个 IP；逗号分隔多段；仅 IPv4。
fn parse_targets(text: &str) -> Result<Vec<Ipv4Addr>, String> {
    let mut out: Vec<Ipv4Addr> = Vec::new();
    for seg in text.split(',') {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        if let Some((start, end)) = seg.split_once('-') {
            let a: IpAddr = start
                .trim()
                .parse()
                .map_err(|_| format!("范围起点「{start}」不是合法 IP"))?;
            let b: IpAddr = end
                .trim()
                .parse()
                .map_err(|_| format!("范围终点「{end}」不是合法 IP"))?;
            let (s, e) = match (a, b) {
                (IpAddr::V4(a), IpAddr::V4(b)) => (u32::from(a), u32::from(b)),
                _ => return Err("范围仅支持 IPv4".into()),
            };
            if e < s {
                return Err(format!("范围起点大于终点：{seg}"));
            }
            // 上限保护：超过 4096 个地址不让扫，避免误输入整段公网
            if e - s > 4096 {
                return Err(format!("范围过大（{} 个地址）：{seg}", e - s + 1));
            }
            out.extend((s..=e).map(Ipv4Addr::from));
        } else if let Some((base, prefix)) = seg.split_once('/') {
            let ip: IpAddr = base
                .trim()
                .parse()
                .map_err(|_| format!("「{base}」不是合法 IP"))?;
            let base = match ip {
                IpAddr::V4(ip) => ip,
                _ => return Err("CIDR 仅支持 IPv4".into()),
            };
            let prefix: u32 = prefix
                .trim()
                .parse()
                .map_err(|_| format!("CIDR 前缀长度「{prefix}」非法"))?;
            if prefix > 32 {
                return Err(format!("CIDR 前缀长度「{prefix}」非法"));
            }
            // 只允许 /20 及以上：/20 = 4096 个地址，正好是范围上限
            if prefix < 20 {
                return Err(format!("CIDR 过大（{seg}）：前缀必须 ≥ /20"));
            }
            let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
            let network = u32::from(base) & mask;
            let broadcast = network | !mask;
            // 跳过网络地址和广播地址
            out.extend(
                ((network + 1)..broadcast).map(Ipv4Addr::from),
            );
        } else {
            let ip: IpAddr = seg
                .parse()
                .map_err(|_| format!("「{seg}」不是合法 IP / 范围 / CIDR"))?;
            match ip {
                IpAddr::V4(ip) => out.push(ip),
                IpAddr::V6(_) => return Err(format!("暂仅支持 IPv4：{seg}")),
            }
        }
    }
    if out.is_empty() {
        return Err("目标为空".into());
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// 解析端口字段：逗号分隔，支持 a-b 范围；端口 0 非法；去重排序。
fn parse_ports(text: &str) -> Result<Vec<u16>, String> {
    let mut ports = Vec::new();
    for part in text.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((lo, hi)) = part.split_once('-') {
            let lo: u16 = lo.trim().parse().map_err(|_| format!("端口「{lo}」非法"))?;
            let hi: u16 = hi.trim().parse().map_err(|_| format!("端口「{hi}」非法"))?;
            if lo == 0 || hi < lo {
                return Err(format!("端口范围非法：{part}"));
            }
            ports.extend(lo..=hi);
        } else {
            let p: u16 = part.parse().map_err(|_| format!("端口「{part}」非法"))?;
            if p == 0 {
                return Err(format!("端口「{part}」非法"));
            }
            ports.push(p);
        }
    }
    ports.sort_unstable();
    ports.dedup();
    if ports.is_empty() {
        return Err("端口为空".into());
    }
    Ok(ports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parsing() {
        let ips = parse_targets("192.168.1.1-192.168.1.3").unwrap();
        assert_eq!(ips.len(), 3);
        assert_eq!(ips[0].to_string(), "192.168.1.1");
        assert_eq!(ips[2].to_string(), "192.168.1.3");
    }

    #[test]
    fn cidr_parsing_skips_network_and_broadcast() {
        let ips = parse_targets("192.168.1.0/30").unwrap();
        // /30 有 4 个地址，去掉网络地址和广播地址剩 2 个
        assert_eq!(ips.len(), 2);
        assert_eq!(ips[0].to_string(), "192.168.1.1");
        assert_eq!(ips[1].to_string(), "192.168.1.2");
    }

    #[test]
    fn port_list_parsing() {
        let ports = parse_ports("22,80,443,3389,8080").unwrap();
        assert_eq!(ports, vec![22, 80, 443, 3389, 8080]);
        let mixed = parse_ports("22,8000-8003").unwrap();
        assert_eq!(mixed, vec![22, 8000, 8001, 8002, 8003]);
    }

    #[test]
    fn oversized_and_invalid_rejected() {
        assert!(parse_targets("10.0.0.1-10.1.0.1").is_err());
        assert!(parse_targets("10.0.0.0/8").is_err());
        assert!(parse_targets("10.0.0.0/32").is_err());
        assert!(parse_ports("0").is_err());
        assert!(parse_ports("900-100").is_err());
    }
}
