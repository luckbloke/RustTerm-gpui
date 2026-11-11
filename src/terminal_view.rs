//! 终端面板：dock 里的一个可停靠终端。
//!
//! 与「app 自己管 canvas / 快照 / 鼠标命中」的做法相比，这里每个终端是一个
//! 独立 Entity，自己吸数据、自己 `fit_to`、自己绘制、自己处理键鼠；分屏、
//! 标签组、拖拽到边缘分裂、单格缩放、整树持久化都交给 `gpui-base` 的 dock。
//!
//! 分工：
//! - `term::TermPane` 仍是 VT 状态机 + IO + 输入编码（本文件不动它）；
//! - 本文件是它的「视图」：一块 canvas、一个焦点句柄、一份本帧快照；
//! - app 只负责创建 / 关闭面板，以及把键盘事件转给当前聚焦的那一个。

use crate::term::{self, TermPane, TermSnapshot};
use crate::ui_icons::ic;
use gpui_kit::assets::IconName;
use gpui_kit::base::dock::{Panel as BasePanel, PanelEvent, PanelState};
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::dock::{Panel as SkinPanel, PanelControl};
use gpui_kit::component::theme::Theme;
use gpui_kit::component::Sizable as _;
use gpui_kit::*;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

/// 一个终端面板。
///
/// 面板先于连接存在：SSH 分屏要建链，不能卡住 UI，所以面板可以以
/// 「连接中」状态进入 dock（显示占位），连上后再把 `TermPane` 装进来；
/// 失败则保留错误占位（可拖动、可关闭）。
pub struct TerminalView {
    /// VT 状态机 + 后端 IO；连接中/失败时为 None
    pane: Option<TermPane>,
    /// SSH 连接中：后台线程回传
    pending: Option<std::sync::mpsc::Receiver<anyhow::Result<TermPane>>>,
    /// 连接失败信息（保留在面板里显示）
    error: Option<String>,
    /// 连接中/失败时标签栏显示的名字
    pending_title: String,
    /// 面板自己的焦点：dock 切标签、点击都会落到这里
    focus: FocusHandle,
    /// 本帧绘制快照（canvas 消费）
    snapshot: Option<Arc<TermSnapshot>>,
    /// 上一帧 canvas 的 bounds（下一帧据此 fit，鼠标据此换算列/行）
    area: Rc<Cell<Bounds<Pixels>>>,
    /// 行级 shaping 缓存（每格终端一份）。
    ///
    /// 用 `Rc<RefCell<…>>` 而不是普通字段：绘制发生在一个 `'static`
    /// 的 paint 闭包里，拿不到 `&mut self`。
    shaper: Rc<std::cell::RefCell<term::LineShapeCache>>,
    /// 字号（配置变更时由 app 推给所有面板）
    font_size: f32,
    /// dock 告知：本面板是所在标签组里正在显示的那一个
    /// （决定光标画实心还是空心）
    active: bool,
    /// dock 告知：本组处于单格缩放状态
    zoomed: bool,
    /// 有新数据被消费、快照需要重建。
    ///
    /// 脏驱动重绘后，数据由 app 的低频泵（`TerminalView::pump_cheap`）吸走，
    /// 而快照只在 `tick`（render 期间）里重建：没有这个标记，数据被吸走了
    /// 却没有任何一帧会把它画出来。
    dirty: bool,
}

impl TerminalView {
    pub fn new(pane: TermPane, font_size: f32, cx: &mut Context<Self>) -> Self {
        let title = pane.title.clone();
        Self {
            pane: Some(pane),
            pending: None,
            error: None,
            pending_title: title,
            focus: cx.focus_handle(),
            snapshot: None,
            area: Rc::new(Cell::new(Bounds::default())),
            shaper: Rc::new(std::cell::RefCell::new(term::LineShapeCache::new())),
            font_size,
            active: false,
            zoomed: false,
            dirty: true,
        }
    }

    /// 建实体（app 创建终端时用这个）。
    pub fn entity(pane: TermPane, font_size: f32, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| Self::new(pane, font_size, cx))
    }

    /// 以「连接中」状态建面板：`rx` 由后台线程喂入连接结果。
    pub fn connecting(
        title: String,
        rx: std::sync::mpsc::Receiver<anyhow::Result<TermPane>>,
        font_size: f32,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| Self {
            pane: None,
            pending: Some(rx),
            error: None,
            pending_title: title,
            focus: cx.focus_handle(),
            snapshot: None,
            area: Rc::new(Cell::new(Bounds::default())),
            shaper: Rc::new(std::cell::RefCell::new(term::LineShapeCache::new())),
            font_size,
            active: false,
            zoomed: false,
            dirty: true,
        })
    }

    pub fn is_connecting(&self) -> bool {
        self.pending.is_some()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn title(&self) -> String {
        match &self.pane {
            Some(p) => p.title.clone(),
            None => self.pending_title.clone(),
        }
    }

    /// 仅面板还活着时返回底层状态机。
    pub fn pane_ref(&self) -> Option<&TermPane> {
        self.pane.as_ref()
    }

    pub fn pane_mut_ref(&mut self) -> Option<&mut TermPane> {
        self.pane.as_mut()
    }

    pub fn spawn(&self) -> Option<&crate::term::SpawnCfg> {
        self.pane.as_ref().map(|p| &p.spawn)
    }

    pub fn is_dead(&self) -> bool {
        self.pane.as_ref().map(|p| p.dead).unwrap_or(false)
    }

    pub fn dims(&self) -> (u16, u16) {
        self.pane
            .as_ref()
            .map(|p| (p.dims.cols, p.dims.rows))
            .unwrap_or((80, 24))
    }

    pub fn ssh_handle(&self) -> Option<std::sync::Arc<crate::backends::ssh::SshHandle>> {
        self.pane.as_ref().and_then(|p| p.ssh_handle())
    }

    pub fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        if (self.font_size - size).abs() > f32::EPSILON {
            self.font_size = size;
            cx.notify();
        }
    }

    /// app 把键盘事件转到这里（焦点判断在 app 侧做）。
    pub fn handle_key(&mut self, ks: &Keystroke, cx: &mut App) {
        if let Some(pane) = &mut self.pane {
            pane.handle_keystroke(ks, cx);
        }
    }

    pub fn set_search(&mut self, needle: &str) {
        if let Some(pane) = &mut self.pane {
            pane.set_search(needle);
        }
    }

    pub fn search_next(&mut self) {
        if let Some(pane) = &mut self.pane {
            pane.search_next();
        }
    }

    pub fn search_prev(&mut self) {
        if let Some(pane) = &mut self.pane {
            pane.search_prev();
        }
    }

    pub fn clear_search(&mut self) {
        if let Some(pane) = &mut self.pane {
            pane.clear_search();
        }
    }

    pub fn search_progress(&self) -> (usize, usize) {
        let Some(pane) = &self.pane else {
            return (0, 0);
        };
        let n = pane.search_matches.len();
        (if n == 0 { 0 } else { pane.search_index + 1 }, n)
    }

    pub fn search_needle(&self) -> &str {
        self.pane.as_ref().map(|p| p.search.as_str()).unwrap_or("")
    }

    pub fn has_selection(&self) -> bool {
        self.pane.as_ref().map(|p| p.has_selection()).unwrap_or(false)
    }

    pub fn copy_selection(&self, cx: &mut App) -> bool {
        self.pane
            .as_ref()
            .map(|p| p.copy_selection(cx))
            .unwrap_or(false)
    }

    pub fn paste_clipboard(&mut self, cx: &App) -> bool {
        self.pane
            .as_mut()
            .map(|p| p.paste_clipboard(cx))
            .unwrap_or(false)
    }

    pub fn select_all(&mut self) {
        if let Some(pane) = &mut self.pane {
            pane.select_all();
        }
    }

    pub fn write_input(&self, bytes: Vec<u8>) {
        if let Some(pane) = &self.pane {
            pane.write_input(bytes);
        }
    }

    pub fn write_input_local(&mut self, bytes: Vec<u8>) {
        if let Some(pane) = &mut self.pane {
            pane.write_input_local(bytes);
        }
    }

    pub fn set_mirror(&mut self, mirror: Option<std::sync::mpsc::Sender<Vec<u8>>>) {
        if let Some(pane) = &mut self.pane {
            pane.mirror = mirror;
        }
    }

    pub fn context_text(&self, max_lines: usize, max_line_len: usize) -> String {
        self.pane
            .as_ref()
            .map(|p| p.context_text(max_lines, max_line_len))
            .unwrap_or_default()
    }

    /// 当前是否真的握着键盘焦点（dock 显示中 + 焦点在本面板）。
    pub fn focused(&self, window: &Window) -> bool {
        self.active && self.focus.is_focused(window)
    }

    /// 状态栏用：光标所在的行列（1-based）。
    pub fn cursor_row_col(&self) -> Option<(i32, i32)> {
        let snap = self.snapshot.as_ref()?;
        let cur = snap.cursor.as_ref()?;
        let r = (cur.bounds.origin.y.as_f32() / snap.cell.height.as_f32()).floor() as i32 + 1;
        let c = (cur.bounds.origin.x.as_f32() / snap.cell.width.as_f32()).floor() as i32 + 1;
        Some((r, c))
    }

    pub fn alt_screen(&self) -> bool {
        self.snapshot.as_ref().map(|s| s.alt_screen).unwrap_or(false)
    }

    pub fn mouse_mode(&self) -> bool {
        self.snapshot.as_ref().map(|s| s.mouse_mode).unwrap_or(false)
    }

    /// 窗口坐标 → 本面板的（列, 行）。
    fn col_row(&self, pos: Point<Pixels>) -> (usize, i32) {
        let b = self.area.get();
        let origin = Point::new(b.origin.x + px(term::TERM_PAD), b.origin.y + px(term::TERM_PAD));
        let cell = self
            .pane
            .as_ref()
            .map(|p| p.cell)
            .unwrap_or_else(|| size(px(9.0), px(17.0)));
        let col = ((pos.x - origin.x).as_f32() / cell.width.as_f32())
            .floor()
            .max(0.0) as usize;
        let row = ((pos.y - origin.y).as_f32() / cell.height.as_f32())
            .floor()
            .max(0.0) as i32;
        (col, row)
    }

    /// 只吸数据 / 排事件，不做任何绘制准备（不测字体、不建快照）。
    ///
    /// 为什么单独有这么一条路径：改成脏驱动重绘后，空闲时不再有每帧 render，
    /// 而 PTY 输出、连接结果、标题事件都必须有人接住——否则打字回显要等下一次
    /// 偶发重绘才出现（这正是历史上"打字变慢镜头"的成因）。
    /// app 的 UI 泵按活跃度以 8ms/200ms 的节奏调用它；真正昂贵的部分（字体
    /// 测量、快照生成、绘制）仍然只在 `tick`/`render` 里做。
    ///
    /// 返回 true = 有新内容，调用方负责 `cx.notify()`。
    pub fn pump_cheap(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = self.take_connection_result();
        let Some(pane) = &mut self.pane else {
            return changed;
        };
        let (title_changed, clip) = pane.drain_pane_events();
        if let Some(text) = clip {
            term::copy_to_clipboard(cx, text);
        }
        let data_changed = pane.pump();
        if data_changed || title_changed {
            // 数据已经离开后端通道：标记"快照待重建"，否则这一批内容谁也画不出来
            self.dirty = true;
            changed = true;
        }
        changed
    }

    /// 收「连接中」面板的结果（连上 → 换真身；失败 → 留错误占位）。
    /// 返回是否有状态变化。抽出来是为了让 `tick` 与 `pump_cheap` 共用。
    fn take_connection_result(&mut self) -> bool {
        let result = match &self.pending {
            Some(rx) => match rx.try_recv() {
                Ok(Ok(pane)) => Some(Ok(pane)),
                Ok(Err(e)) => Some(Err(format!("{e:#}"))),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Some(Err("连接线程异常退出".to_string()))
                }
            },
            None => None,
        };
        let Some(result) = result else {
            return false;
        };
        match result {
            Ok(pane) => {
                self.pending_title = pane.title.clone();
                self.pane = Some(pane);
                self.pending = None;
                self.error = None;
            }
            Err(e) => {
                self.error = Some(e);
                self.pending = None;
            }
        }
        self.dirty = true;
        true
    }

    /// 每帧一次：收连接结果 → 吸后端数据 → 按上一帧量到的尺寸 fit → 生成快照。
    ///
    /// 放在 `render` 里是因为这些都需要 `&mut Window`（字体测量、画布绘制）。
    /// 帧什么时候来由 app 的脏驱动调度决定（见 `app.rs` 的 `spawn_ui_pump`），
    /// 不再无条件每帧重绘。
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 1) 连接中的面板：拿到结果就换上真身（或留下错误）
        if self.take_connection_result() {
            cx.notify();
        }

        let Some(pane) = &mut self.pane else {
            // 还没连上：不生成快照，画面是占位
            self.snapshot = None;
            return;
        };

        // 2) 事件 + 数据（app 的 UI 泵通常已经吸过一遍，这里是渲染路径上的兜底）
        let (title_changed, clip) = pane.drain_pane_events();
        if let Some(text) = clip {
            term::copy_to_clipboard(cx, text);
        }
        let data_changed = pane.pump();
        if data_changed || title_changed {
            self.dirty = true;
        }

        // 3) 尺寸：首帧还没有测量值就保持创建时的列数/行数，下一帧再对齐
        let b = self.area.get();
        if b.size.width.as_f32() > 1.0 && b.size.height.as_f32() > 1.0 {
            let avail = size(
                px((b.size.width.as_f32() - term::TERM_PAD * 2.0).max(80.0)),
                px((b.size.height.as_f32() - term::TERM_PAD * 2.0).max(80.0)),
            );
            let _ = pane.fit_to(window, self.font_size, avail);
        }
        // 防抖落定：拖分屏结束后把挂起的最终尺寸补发给 pty。
        // 尺寸真的变了 → 网格重排了，必须重画（否则画面停在旧列数上）。
        if pane.flush_pending_resize() {
            self.dirty = true;
        }

        // 4) 光标实心/空心：dock 里显示中 + 键盘焦点在本面板
        let focus_now = self.active && self.focus.is_focused(window);
        let focus_changed = focus_now != pane.has_focus;
        pane.has_focus = focus_now;

        // 5) 快照**按需重建**：每帧无条件重建整屏快照是主线程的固定开销
        //    （空闲时也在烧 CPU）。内容没变就不重建；另外每 250ms 兜底重建一次，
        //    覆盖光标闪烁以及这里没显式跟踪的状态变化。
        let due = TERM_SNAPSHOT_MS.load(std::sync::atomic::Ordering::Relaxed) == 0
            || term_snapshot_now_ms().saturating_sub(
                TERM_SNAPSHOT_MS.load(std::sync::atomic::Ordering::Relaxed),
            ) >= 250;
        if self.snapshot.is_none()
            || data_changed
            || title_changed
            || focus_changed
            || self.dirty
            || due
        {
            TERM_SNAPSHOT_MS.store(term_snapshot_now_ms(), std::sync::atomic::Ordering::Relaxed);
            self.snapshot = Some(Arc::new(pane.snapshot()));
            self.dirty = false;
        }
    }
}

/// 进程启动以来的毫秒数（终端快照节流用）。
fn term_snapshot_now_ms() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// 上一次终端快照重建的时间（毫秒）。0 = 还没建过。
static TERM_SNAPSHOT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.tick(window, cx);

        let paint_snap = self.snapshot.clone();
        let prepaint_area = self.area.clone();
        let shaper = self.shaper.clone();
        let live = self.pane.is_some();

        let mut root = div()
            .id("term-view")
            .v_flex()
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(term::bg_color())
            .track_focus(&self.focus)
            .key_context("Terminal")
            // 点击：把焦点收回本终端（dock 切标签会走 set_active）
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                window.focus(&this.focus, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus, cx);
                    let (col, row) = this.col_row(ev.position);
                    let _ = this
                        .pane
                        .as_mut()
                        .map(|p| p.mouse_down(col, row, &ev.modifiers));
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    let (col, row) = this.col_row(ev.position);
                    let _ = this
                        .pane
                        .as_mut()
                        .map(|p| p.mouse_middle(col, row, &ev.modifiers, true));
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    let (col, row) = this.col_row(ev.position);
                    // 右键也要把焦点收过来：右键菜单作用于「刚才那个终端」，
                    // 焦点不跟着走的话，在 A 格右键、菜单却去操作 B 格。
                    window.focus(&this.focus, cx);
                    // 终端自己吃掉（鼠标上报模式）就不再冒泡，否则让 app 弹右键菜单
                    let consumed = this
                        .pane
                        .as_mut()
                        .map(|p| p.mouse_right(col, row, &ev.modifiers, true))
                        .unwrap_or(false);
                    if consumed {
                        cx.stop_propagation();
                    }
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseUpEvent, _, cx| {
                    let (col, row) = this.col_row(ev.position);
                    let _ = this
                        .pane
                        .as_mut()
                        .map(|p| p.mouse_up(col, row, &ev.modifiers));
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, ev: &MouseUpEvent, _, cx| {
                    let (col, row) = this.col_row(ev.position);
                    let _ = this
                        .pane
                        .as_mut()
                        .map(|p| p.mouse_middle(col, row, &ev.modifiers, false));
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseUpEvent, _, cx| {
                    let (col, row) = this.col_row(ev.position);
                    let _ = this
                        .pane
                        .as_mut()
                        .map(|p| p.mouse_right(col, row, &ev.modifiers, false));
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                let (col, row) = this.col_row(ev.position);
                let consumed = match (&mut this.pane, ev.pressed_button) {
                    (Some(p), Some(_)) => p.mouse_drag(col, row, &Modifiers::default()),
                    (Some(p), None) => p.mouse_motion(col, row),
                    _ => false,
                };
                if consumed {
                    cx.notify();
                }
            }))
            .on_scroll(cx.listener(|this, ev: &ScrollEvent, _, cx| {
                if let Some(pane) = &mut this.pane {
                    pane.scroll(ev.delta);
                    cx.notify();
                }
            }));

        root = if live {
            root.child(
                canvas(
                    move |bounds, _window, _cx| {
                        // 记录本面板绘制区的原始 bounds（含内边距）；
                        // 文本网格原点 = 左上 + TERM_PAD，与绘制共用。
                        //
                        // 布局变了（窗口缩放 / 分屏拖动）→ 标脏：本帧 fit_to 用的
                        // 还是上一帧的区尺寸，网格重排要下一帧才生效。脏驱动重绘
                        // 之后没有无条件续帧了，不标这一下，窗口缩放后终端会停在
                        // 旧的列数上（画面被拉伸/裁切）。
                        let prev = prepaint_area.get();
                        if prev.size.width != bounds.size.width
                            || prev.size.height != bounds.size.height
                            || prev.origin.x != bounds.origin.x
                            || prev.origin.y != bounds.origin.y
                        {
                            prepaint_area.set(bounds);
                            crate::xsrv::counters::mark_ui_dirty();
                        }
                    },
                    move |bounds, (), window, cx| {
                        if let Some(snap) = &paint_snap {
                            let origin = Point::new(
                                bounds.origin.x + px(term::TERM_PAD),
                                bounds.origin.y + px(term::TERM_PAD),
                            );
                            let mut cache = shaper.borrow_mut();
                            term::paint_snapshot(snap, origin, &mut cache, window, cx);
                        }
                    },
                )
                .flex_1()
                .min_h_0(),
            )
        } else {
            root.child(self.placeholder(cx))
        };

        root
    }
}

impl TerminalView {
    /// 连接中 / 连接失败的占位（面板仍在 dock 里：可拖动、可关闭）。
    fn placeholder(&self, cx: &App) -> AnyElement {
        let theme = Theme::global(cx).clone();
        let dim = theme.colors.muted_foreground;
        let (icon, text, color) = match (&self.error, self.pending.is_some()) {
            (Some(e), _) => (
                IconName::TriangleAlert,
                format!("连接失败：{e}"),
                theme.colors.danger,
            ),
            (None, true) => (
                IconName::Loader,
                format!("{} 正在连接…", self.pending_title),
                dim,
            ),
            _ => (IconName::SquareTerminal, "终端".to_string(), dim),
        };
        div()
            .v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_2()
            .px_4()
            .child(ic(icon).small().text_color(color))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(color)
                    .max_w(px(420.0))
                    .child(text),
            )
            .into_any_element()
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EventEmitter<PanelEvent> for TerminalView {}

impl BasePanel for TerminalView {
    fn panel_name(&self) -> &'static str {
        "Terminal"
    }

    fn closable(&self, _: &App) -> bool {
        true
    }

    fn zoomable(&self, _: &App) -> bool {
        true
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        if self.active != active {
            self.active = active;
            cx.notify();
        }
    }

    fn set_zoomed(&mut self, zoomed: bool, _: &mut Window, cx: &mut Context<Self>) {
        if self.zoomed != zoomed {
            self.zoomed = zoomed;
            cx.notify();
        }
    }

    /// 面板被移出 dock（关闭标签）。真正的清理由实体销毁完成：
    /// `TermPane` 一 drop，pty / SSH 通道随之关闭。
    fn on_removed(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = &mut self.pane {
            pane.mirror = None;
        }
        cx.notify();
    }

    /// 持久化：面板名 + 自己的启动配置。
    ///
    /// 目前只写名字（`load` 时不重建会话）；把 `SpawnCfg` 塞进
    /// `PanelInfo::Panel` 需要给启动配置加 serde，属于下一阶段。
    fn dump(&self, _: &App) -> PanelState {
        PanelState::new(self.panel_name())
    }
}

impl SkinPanel for TerminalView {
    fn tab_name(&self, _: &App) -> Option<SharedString> {
        Some(SharedString::from(self.title()))
    }

    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let icon = if self.error.is_some() {
            IconName::TriangleAlert
        } else if self.pending.is_some() {
            IconName::Loader
        } else {
            IconName::SquareTerminal
        };
        div()
            .h_flex()
            .items_center()
            .gap_1()
            .child(ic(icon).xsmall())
            // 显式路径：方法名与本 trait 的 `title` 撞名，直接调用会解析到 2 参数的 trait 方法
            .child(SharedString::from(TerminalView::title(self)))
    }

    /// 终端要贴边铺满，标签组不要给它加内边距。
    fn inner_padding(&self, _: &App) -> bool {
        false
    }

    /// 保留标题栏（拖拽标签、缩放按钮都在上面）。
    fn title_bar(&self, _: &App) -> bool {
        true
    }

    fn zoom_control(&self, _: &App) -> Option<PanelControl> {
        Some(PanelControl::Both)
    }
}
