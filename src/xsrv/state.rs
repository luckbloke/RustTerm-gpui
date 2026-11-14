//! 服务端共享状态：资源表、atom 表、窗口树和屏幕像素。
//!
//! 所有客户端连接共享同一份 `ServerState`（X11 的资源本来就是跨连接可见的：
//! 一个客户端创建的窗口，另一个客户端可以查询、绘制、监听事件）。并发访问用
//! `Mutex` 串行化——每个请求的处理都很短，且不持有锁做 I/O（回复/事件只是往
//! 各连接的发送队列里塞字节）。
//!
//! 像素模型（M1）：
//! - 每个窗口 / pixmap 各自持有一块 `u32` 像素缓冲，格式 `0x00RRGGBB`；
//! - 窗口内容用**窗口自身坐标**，绘制不关心它在屏幕上的位置；
//! - `compose()` 从根窗口开始按叠放顺序把子窗口画进根缓冲（含边框裁剪），
//!   供 `GetImage(root)` 和后续的 Viewer 使用。
//!
//! 这是"先正确、后高效"的版本：每次绘制都整屏重合成。M8 再做脏矩形/帧差分。

use std::collections::HashMap;

use tokio::sync::mpsc::UnboundedSender;
use x11rb_protocol::protocol::xproto::*;
use x11rb_protocol::x11_utils::Serialize;

use super::draw::{Bitmap, Canvas, Clip};
use super::error::XError;
use super::input::InputState;
use super::handshake::{
    resource_id_base, ARGB32_VISUAL, RESOURCE_ID_MASK, ROOT_COLORMAP, ROOT_VISUAL, ROOT_WINDOW, SCREEN_DEPTH,
};
use super::region::Rects;
use super::server::ServerConfig;

/// X11 预定义 atom。协议规定这些名字必须占用固定 ID（1..=68），
/// 客户端会直接拿数字当 atom 用（比如 `WM_NAME = 39`），不能按需分配。
const PREDEFINED_ATOMS: &[&str] = &[
    "PRIMARY",
    "SECONDARY",
    "ARC",
    "ATOM",
    "BITMAP",
    "CARDINAL",
    "COLORMAP",
    "CURSOR",
    "CUT_BUFFER0",
    "CUT_BUFFER1",
    "CUT_BUFFER2",
    "CUT_BUFFER3",
    "CUT_BUFFER4",
    "CUT_BUFFER5",
    "CUT_BUFFER6",
    "CUT_BUFFER7",
    "DRAWABLE",
    "FONT",
    "INTEGER",
    "PIXMAP",
    "POINT",
    "RECTANGLE",
    "RESOURCE_MANAGER",
    "RGB_COLOR_MAP",
    "RGB_BEST_MAP",
    "RGB_BLUE_MAP",
    "RGB_DEFAULT_MAP",
    "RGB_GRAY_MAP",
    "RGB_GREEN_MAP",
    "RGB_RED_MAP",
    "STRING",
    "VISUALID",
    "WINDOW",
    "WM_COMMAND",
    "WM_HINTS",
    "WM_CLIENT_MACHINE",
    "WM_ICON_NAME",
    "WM_ICON_SIZE",
    "WM_NAME",
    "WM_NORMAL_HINTS",
    "WM_SIZE_HINTS",
    "WM_ZOOM_HINTS",
    "MIN_SPACE",
    "NORM_SPACE",
    "MAX_SPACE",
    "END_SPACE",
    "SUPERSCRIPT_X",
    "SUPERSCRIPT_Y",
    "SUBSCRIPT_X",
    "SUBSCRIPT_Y",
    "UNDERLINE_POSITION",
    "UNDERLINE_THICKNESS",
    "STRIKEOUT_ASCENT",
    "STRIKEOUT_DESCENT",
    "ITALIC_ANGLE",
    "X_HEIGHT",
    "QUAD_WIDTH",
    "WEIGHT",
    "POINT_SIZE",
    "RESOLUTION",
    "COPYRIGHT",
    "NOTICE",
    "FONT_NAME",
    "FAMILY_NAME",
    "FULL_NAME",
    "CAP_HEIGHT",
    "WM_CLASS",
    "WM_TRANSIENT_FOR",
];

/// XA_LAST_PREDEFINED：新 atom 从它后面开始分配。
pub const LAST_PREDEFINED_ATOM: u32 = PREDEFINED_ATOMS.len() as u32;

/// 把事件帧补齐到协议规定的 32 字节。
///
/// `event.serialize()`（x11rb 的 `Serialize`）只写有意义的字段：比如 `MapNotify`
/// 返回 16 字节、`Expose` 返回 20 字节，**不含**尾部填充，而线上每一帧事件都必须
/// 正好 32 字节。紧凑序列化是线格式的前缀，补零即可。
pub fn pad_event(mut frame: Vec<u8>) -> Vec<u8> {
    debug_assert!(frame.len() <= 32, "事件帧不该超过 32 字节");
    frame.resize(32, 0);
    frame
}

/// 一个已连接的客户端。
pub struct ClientHandle {
    pub sender: UnboundedSender<Vec<u8>>,
    /// 本客户端可用的资源 ID 基址（高位固定）。
    pub resource_id_base: u32,
}

/// 窗口上的一个属性。
pub struct Property {
    pub type_: Atom,
    pub format: u8,
    /// 按 format 展开后的原始字节（大端元素顺序？不：X11 属性里的 16/32 位
    /// 元素按客户端字节序存放，我们只做透传，不做数值解释）。
    pub data: Vec<u8>,
}

/// 窗口的 SHAPE 扩展形状（窗口局部坐标，外角为原点）。
/// `None` 表示该 kind 未 shaped。
#[derive(Debug, Default, Clone)]
pub struct WindowShape {
    pub bounding: Option<Rects>,
    pub clip: Option<Rects>,
    /// SHAPE 的第三种 kind：输入区域。不影响绘制，只影响命中测试
    /// （GTK 用它给窗口挖掉阴影/圆角处的点击区）。
    pub input: Option<Rects>,
}

/// 一个 XFIXES Damage 对象。
#[derive(Debug, Clone)]
pub struct DamageData {
    pub drawable: Drawable,
    pub level: u8,
    pub owner: u32,
    /// 上次通知以来可绘制对象被画过（compose 时据此发通知）。
    pub dirty: bool,
}

/// 窗口（含根窗口）。
pub struct WindowData {
    pub id: Window,
    pub parent: Window,
    pub depth: u8,
    pub class: WindowClass,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub border_width: u16,
    pub mapped: bool,
    pub override_redirect: bool,
    pub background_pixel: u32,
    pub border_pixel: u32,
    pub colormap: Colormap,
    pub visual: Visualid,
    pub bit_gravity: Gravity,
    pub win_gravity: Gravity,
    pub do_not_propagate: u32,
    /// 每个客户端单独选择的 event mask（X11 语义：mask 是每客户端独立的）。
    pub event_masks: HashMap<u32, u32>,
    /// 子窗口，按叠放顺序从下到上。
    pub children: Vec<Window>,
    pub pixels: Vec<u32>,
    pub properties: HashMap<Atom, Property>,
    pub owner: u32,
}

impl WindowData {
    pub fn event_mask(&self, client: u32) -> u32 {
        self.event_masks.get(&client).copied().unwrap_or(0)
    }

    fn content_len(&self) -> usize {
        self.width as usize * self.height as usize
    }
}

/// 位图 / 离屏绘制目标。
pub struct PixmapData {
    pub id: Pixmap,
    pub depth: u8,
    pub width: u16,
    pub height: u16,
    pub pixels: Vec<u32>,
    pub owner: u32,
}
impl Clone for PixmapData {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            depth: self.depth,
            width: self.width,
            height: self.height,
            pixels: self.pixels.clone(),
            owner: self.owner,
        }
    }
}

/// 图形上下文。M1 只实现影响核心绘制的那部分字段，其余字段照收不报错。
#[derive(Clone)]
pub struct GcData {
    pub id: Gcontext,
    pub depth: u8,
    pub foreground: u32,
    /// `None` 表示未设置：X11 语义下"不填背景"，文字绘制时字格里除字形外保持原样。
    pub background: Option<u32>,
    pub function: u32,
    pub plane_mask: u32,
    pub line_width: u32,
    pub line_style: u32,
    pub fill_style: u32,
    pub tile: Option<Pixmap>,
    pub stipple: Option<Pixmap>,
    pub font: Option<Font>,
    pub graphics_exposures: bool,
    pub clip_x_origin: i32,
    pub clip_y_origin: i32,
    pub clip_mask: Option<Pixmap>,
    /// SetClipRectangles 设置的矩形裁剪；M1 只保存不使用（TODO M2）。
    pub clip_rectangles: Vec<Rectangle>,
    pub dashes: Vec<u8>,
    pub dash_offset: u32,
    pub arc_mode: u32,
    pub owner: u32,
}

impl GcData {
    pub fn new(id: Gcontext, owner: u32, depth: u8) -> Self {
        Self {
            id,
            depth,
            foreground: 0x0000_0000,
            background: None,
            function: u32::from(GX::COPY),
            plane_mask: 0xffff_ffff,
            line_width: 0,
            line_style: u32::from(LineStyle::SOLID),
            fill_style: u32::from(FillStyle::SOLID),
            tile: None,
            stipple: None,
            font: None,
            graphics_exposures: true,
            clip_x_origin: 0,
            clip_y_origin: 0,
            clip_mask: None,
            clip_rectangles: Vec::new(),
            dashes: Vec::new(),
            dash_offset: 0,
            arc_mode: 0,
            owner,
        }
    }
}

pub struct ColormapData {
    pub id: Colormap,
    pub visual: Visualid,
    pub owner: u32,
}

/// RENDER picture 的取源方式。
#[derive(Debug, Clone)]
pub enum PictureSource {
    /// 包装一个 window / pixmap：取它的像素（格式 `0x00RRGGBB`，alpha 视为 1）。
    Drawable(Drawable),
    /// CreateSolidFill：整个 picture 是一块纯色（未预乘的 16 位通道）。
    Solid {
        red: u16,
        green: u16,
        blue: u16,
        alpha: u16,
    },
    /// CreateLinearGradient：P1→P2 轴向线性渐变。
    LinearGradient {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        stops: Vec<GradientStop>,
    },
    /// CreateRadialGradient：双圆同心渐变（RENDER 规范 7.6 的 t 参数公式）。
    RadialGradient {
        x1: f64,
        y1: f64,
        r1: f64,
        x2: f64,
        y2: f64,
        r2: f64,
        stops: Vec<GradientStop>,
    },
    /// CreateConicalGradient：绕中心的锥形角度渐变。
    ConicalGradient {
        x: f64,
        y: f64,
        /// 起始角（度，RENDER 16.16 定点已换算）。
        angle: f64,
        stops: Vec<GradientStop>,
    },
}

/// 渐变停靠点：offset ∈ [0,1]，颜色为未预乘 16 位通道。
#[derive(Debug, Clone)]
pub struct GradientStop {
    pub offset: f64,
    pub red: u16,
    pub green: u16,
    pub blue: u16,
    pub alpha: u16,
}

/// 一个 RENDER Picture。picture 不拥有像素，只是 drawable 的一层"视图" +
/// 属性（格式 / 重复 / 裁剪 / 滤波）。
#[derive(Debug, Clone)]
pub struct PictureData {
    pub id: u32,
    pub owner: u32,
    /// Pictformat ID（`render.rs` 的 FORMAT_* 常量）。
    pub format: u32,
    pub source: PictureSource,
    /// repeat=true 时取源越界回卷；false 时越界部分 alpha=0。
    pub repeat: bool,
    /// SetPictureClipRectangles 设置的裁剪区域（矩形坐标相对 clip origin）。
    pub clip: Option<Rects>,
    pub clip_x_origin: i16,
    pub clip_y_origin: i16,
    /// SetPictureFilter 记录的名字（Nearest/Best 被接受并忽略）。
    pub filter: String,
}

/// 一个 RENDER 字形：位图（ARGB32 预乘或 A8）+ 度量。
#[derive(Debug, Clone)]
pub struct Glyph {
    pub width: u16,
    pub height: u16,
    /// 绘制时字形位图相对笔画原点的偏移（画在 pen_x - x, pen_y - y）。
    pub x: i16,
    pub y: i16,
    /// 画完这个字形后笔画的推进量（pen += (x_off, y_off)）。
    pub x_off: i16,
    pub y_off: i16,
    /// 每像素 `bytes_per_pixel` 字节的位图数据（行按 4 字节对齐）。
    pub bits: Vec<u8>,
    /// 4 = ARGB32，1 = A8。
    pub bytes_per_pixel: u8,
}

/// 一个 RENDER GlyphSet：格式 + 字形表。
#[derive(Debug, Clone)]
pub struct GlyphSetData {
    pub id: u32,
    /// 字形位图的 Pictformat（A8 / ARGB32）。
    pub format: u32,
    pub glyphs: HashMap<u32, Glyph>,
    /// ReferenceGlyphSet 建立的别名：指向规范 glyph set 的 ID。
    pub alias: Option<u32>,
    pub owner: u32,
}

impl GlyphSetData {
    /// 解析别名链，返回存字形的那一份（最多一层，ReferenceGlyphSet 直接指向规范表）。
    pub fn canonical<'a>(&'a self, sets: &'a HashMap<u32, GlyphSetData>) -> &'a GlyphSetData {
        match self.alias.and_then(|id| sets.get(&id)) {
            Some(canonical) => canonical,
            None => self,
        }
    }
}

/// 一条水平脏区带（task-17 阶段 B）：连续变化行聚成的矩形 + 矩形本地像素。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameBand {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    /// 行优先紧密排列的 `width * height` 个 u32 像素。
    pub pixels: Vec<u32>,
}

/// 一次帧 diff 的结果（task-18：读阶段产物，写阶段据此提交基准帧）。
#[derive(Debug, Clone)]
pub struct FrameDiff {
    /// 产生这份 diff 的屏幕版本号（提交时写入 sent_seq）。
    pub seq: u64,
    /// 屏幕尺寸相对基准帧发生了变化（提交时重建基准帧缓冲）。
    pub size_changed: bool,
    /// 变化带（像素已拷出）。
    pub bands: Vec<FrameBand>,
}

/// 一个已打开的字体。
///
/// 所有名字都映射到同一份内置点阵字体（见 `font.rs`），这里只保留名字用于
/// `QueryFont` 的属性回显和排查日志。
pub struct FontData {
    pub id: Font,
    pub name: String,
    pub owner: u32,
}

/// 光标条目：属主客户端 + 引用的 picture（RENDER 光标）。
///
/// NyaTerm: X11 的引用计数语义——FreePicture 只释放客户端引用，被光标
/// 引用的 picture 必须保持存活（firefox 的流程：建 picture → CreateCursor →
/// 立刻 FreePicture → 再建第二个 cursor 引用同一 picture；不实现引用计数
/// 就会在第二次 CreateCursor 得到 BadPicture）。
pub struct CursorRef {
    pub owner: u32,
    pub picture: Option<u32>,
}

/// 服务端全部状态。
pub struct ServerState {
    pub config: ServerConfig,
    pub clients: HashMap<u32, ClientHandle>,
    /// atom ID -> 名字；下标 0 固定为空（atom 0 = None）。
    pub atoms: Vec<Option<String>>,
    pub atom_ids: HashMap<String, Atom>,
    pub windows: HashMap<Window, WindowData>,
    pub pixmaps: HashMap<Pixmap, PixmapData>,
    pub gcs: HashMap<Gcontext, GcData>,
    pub colormaps: HashMap<Colormap, ColormapData>,
    pub fonts: HashMap<Font, FontData>,
    /// 光标 ID → 属主客户端 + 引用的 picture（RENDER 光标）。画面上不渲染
    /// 光标，但**必须记住它存在**：`XDefineCursor` / `FreeCursor` /
    /// `RecolorCursor` 都要求光标有效，回 BadCursor 会让 Xt/Xaw 程序在
    /// 启动阶段就报错退出。
    ///
    /// NyaTerm: X11 的引用计数语义——FreePicture 只释放客户端引用，被光标
    /// 引用的 picture 必须保持存活（firefox 的流程：建 picture → CreateCursor →
    /// 立刻 FreePicture → 再建第二个 cursor 引用同一 picture；不实现引用计数
    /// 就会在第二次 CreateCursor 得到 BadPicture）。
    pub cursors: HashMap<Cursor, CursorRef>,
    /// 窗口的 SHAPE 形状（`extensions.rs` 的 SHAPE 请求维护，合成与命中测试消费）。
    pub shapes: HashMap<Window, WindowShape>,
    /// SHAPE 事件选择（`ShapeSelectInput`）：窗口 → 选择它的客户端集合。
    /// 形状变化时向选择者发 ShapeNotify（`extensions.rs` 的 shape_notify）。
    pub shape_event_selections: HashMap<Window, std::collections::HashSet<u32>>,
    /// XFIXES Region ID → 矩形区域（`region.rs` 的规范形）。
    pub regions: HashMap<u32, Rects>,
    /// DAMAGE 的 damage ID → 记录（compose 后按 dirty 发 DamageNotify）。
    pub damages: HashMap<u32, DamageData>,
    /// RENDER 的 picture ID → 记录（`render.rs` 维护与消费）。
    pub pictures: HashMap<u32, PictureData>,
    /// RENDER 的 glyph set ID → 字形表（`render.rs` 维护与消费）。
    pub glyph_sets: HashMap<u32, GlyphSetData>,
    /// 选择区（剪贴板/PRIMARY）的属主窗口。只有属主记录，内容交给后续里程碑。
    pub selections: HashMap<Atom, Window>,
    /// 每个客户端推进到的请求序列号。注入输入产生的事件不在任何请求的处理
    /// 过程里，事件帧的 sequence 要按接收方各自的进度改写（见 `stamp_sequence`）：
    /// 发一个 sequence=0 的事件，客户端的 XCB 会报
    /// `Unknown sequence number while processing queue` 并 abort（xclock 崩在这）。
    pub sequences: HashMap<u32, u16>,
    /// 指针/焦点/修饰键状态（由 Viewer 注入的输入驱动，见 `input.rs`）。
    pub input: InputState,
    /// 合成版本号：每次 `compose()` 自增。Viewer 用它判断"这一帧要不要发"，
    /// 避免每 30ms 都把整屏 JPEG 重新编码一遍。
    pub frame_seq: u64,
    /// 帧泵已取走的最后一帧像素（屏幕尺寸）与当时的 `frame_seq`。
    ///
    /// 帧泵用它与当前屏幕做**脏区对比**——有了它就不必每 33ms 做整屏快照 +
    /// 整屏比较（1080p 下约 750MB/s 的纯内存带宽，能把一个核吃满）。
    sent_frame: Vec<u32>,
    sent_size: (usize, usize),
    sent_seq: u64,
    /// NyaTerm: 合成批处理——绘制/结构请求只标记脏位，帧泵（33ms tick）取帧
    /// 前统一合成一次。此前每个绘制请求都触发一次 1280x800 全屏重建
    /// （填背景+重画所有窗口），firefox 一次重绘发几百个绘制请求 =
    /// 几百次全屏合成，画面变成慢镜头（4fps）。
    compose_dirty: bool,
    /// 服务端启动时刻：输入事件的时间戳基准（X 时间戳 = 启动后毫秒数，32 位循环）。
    pub start_time: std::time::Instant,
    /// XI2（XInput2）状态：版本协商 + 窗口事件选择。
    pub xi2_clients: std::collections::HashMap<u32, crate::xsrv::xinput::Xi2Client>,
    pub xi2_selections: std::collections::HashMap<Window, std::collections::HashMap<u32, crate::xsrv::xinput::Xi2Selection>>,
    /// 自上次取帧以来可能发生变化的屏幕区域 `(x0, y0, x1, y1)`（右/下开）。
    ///
    /// compose / 窗口映射变化时维护；`None` 表示未知（按全屏处理）。
    /// 帧泵据此把对比范围缩到真正会变的窗口区域。
    damage_hint: Option<(i32, i32, i32, i32)>,
    /// task-22 按需显示：UI 观众是否在场（与 `ServerShared.viewer_attached`
    /// 共享同一个 Arc；FramePump attach/detach 维护）。
    pub viewer_attached: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 有窗口映射且无观众时触发（HostServer::start 填写；去重在回调里）。
    pub on_map_no_viewer: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

impl ServerState {
    pub fn new(config: ServerConfig) -> Self {
        let mut atoms = vec![None];
        let mut atom_ids = HashMap::new();
        for name in PREDEFINED_ATOMS {
            let id = atoms.len() as Atom;
            atoms.push(Some((*name).to_string()));
            atom_ids.insert((*name).to_string(), id);
        }

        let mut windows = HashMap::new();
        windows.insert(
            ROOT_WINDOW,
            WindowData {
                id: ROOT_WINDOW,
                parent: 0,
                depth: SCREEN_DEPTH,
                class: WindowClass::INPUT_OUTPUT,
                x: 0,
                y: 0,
                width: config.screen_width,
                height: config.screen_height,
                border_width: 0,
                mapped: true,
                override_redirect: true,
                background_pixel: 0x0000_0000,
                border_pixel: 0x0000_0000,
                colormap: ROOT_COLORMAP,
                visual: ROOT_VISUAL,
                bit_gravity: Gravity::NORTH_WEST,
                win_gravity: Gravity::NORTH_WEST,
                do_not_propagate: 0,
                event_masks: HashMap::new(),
                children: Vec::new(),
                pixels: vec![0; config.screen_width as usize * config.screen_height as usize],
                properties: HashMap::new(),
                owner: u32::MAX, // 服务端自己
            },
        );

        let mut colormaps = HashMap::new();
        colormaps.insert(
            ROOT_COLORMAP,
            ColormapData {
                id: ROOT_COLORMAP,
                visual: ROOT_VISUAL,
                owner: u32::MAX,
            },
        );

        let mut state = Self {
            config,
            clients: HashMap::new(),
            atoms,
            atom_ids,
            windows,
            pixmaps: HashMap::new(),
            gcs: HashMap::new(),
            colormaps,
            fonts: HashMap::new(),
            cursors: HashMap::new(),
            shapes: HashMap::new(),
            shape_event_selections: HashMap::new(),
            regions: HashMap::new(),
            damages: HashMap::new(),
            pictures: HashMap::new(),
            glyph_sets: HashMap::new(),
            selections: HashMap::new(),
            sequences: HashMap::new(),
            input: InputState::default(),
            frame_seq: 0,
            sent_frame: Vec::new(),
            sent_size: (0, 0),
            sent_seq: 0,
            compose_dirty: false,
            start_time: std::time::Instant::now(),
            xi2_clients: std::collections::HashMap::new(),
            xi2_selections: std::collections::HashMap::new(),
            damage_hint: None,
            viewer_attached: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            on_map_no_viewer: None,
        };
        state.intern_server_atoms();
        state.setup_xsettings_manager();
        state
    }

    /// 服务端持有 `_XSETTINGS_S0` 选择（扮演 XSETTINGS manager，如同真实桌面
    /// 的 gnome-settings-daemon）。
    ///
    /// 根因（源码级确认）：GTK3 `check_manager_window()`
    /// (gdk/x11/xsettings-client.c) 对 `XGetSelectionOwner` 的结果**不判 None**
    /// 直接 `gdk_x11_window_foreign_new_for_display(display, owner)`——owner=0
    /// 时会以一个 xid=0 的"窗口"继续走 XSelectInput，错误无陷阱保护，
    /// 客户端 `_exit(1)`（"Channel closing" 的最终源头）。真实桌面永远持有
    /// 该选择，所以 GTK 的这个缺陷从不触发。我们开机创建一个真实的 1x1
    /// InputOnly 窗口并占住 `_XSETTINGS_S0`，GTK 探测拿到非零 owner → 包装
    /// 真实窗口 → 读 `_XSETTINGS_SETTINGS` 属性（不存在）→ 用默认设置，
    /// 与真实"无设置管理器但桌面活着"的环境完全一致。
    fn setup_xsettings_manager(&mut self) {
        let atom = match self.intern_atom(b"_XSETTINGS_S0", false) {
            Ok(a) => a,
            Err(_) => return,
        };
        let wid = ROOT_WINDOW.wrapping_sub(16);
        // 避免与服务端/客户端 id 冲突：取 root 附近向下的一段（server 专用段）
        let wid = if self.windows.contains_key(&wid) {
            return; // 理论上不可能；保守放弃（GTK 走 defaults）
        } else {
            wid
        };
        self.windows.insert(
            wid,
            WindowData {
                id: wid,
                parent: ROOT_WINDOW,
                depth: SCREEN_DEPTH,
                class: WindowClass::INPUT_ONLY,
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                border_width: 0,
                mapped: true,
                override_redirect: true,
                background_pixel: 0,
                border_pixel: 0,
                colormap: ROOT_COLORMAP,
                visual: ROOT_VISUAL,
                bit_gravity: Gravity::NORTH_WEST,
                win_gravity: Gravity::NORTH_WEST,
                do_not_propagate: 0,
                event_masks: HashMap::new(),
                children: Vec::new(),
                pixels: vec![0],
                properties: HashMap::new(),
                owner: u32::MAX, // 服务端自己
            },
        );
        // root 的 children 里也要有它（QueryTree 一致性）
        self.windows.get_mut(&ROOT_WINDOW).map(|root| {
            root.children.push(wid);
        });
        self.selections.insert(atom, wid);
    }

    /// 服务端自己需要用到的、但协议没有预定义的 atom（字体属性表用）。
    ///
    /// 开机就 intern 好，这样 `QueryFont` 里回显的属性名在所有客户端看到的是同一份
    /// atom ID——客户端会拿 `InternAtom` 的结果去比对，两边不一致就会当成未知属性。
    pub fn intern_server_atoms(&mut self) -> Vec<Atom> {
        const NAMES: &[&str] = &[
            "DEFAULT_CHAR",
            "PIXEL_SIZE",
            "POINT_SIZE",
            "RESOLUTION_X",
            "RESOLUTION_Y",
            "SPACING",
            "X_HEIGHT",
            "QUAD_WIDTH",
            "WEIGHT",
        ];
        let mut atoms: Vec<Atom> = NAMES
            .iter()
            .filter_map(|name| self.intern_atom(name.as_bytes(), false).ok())
            .collect();

        // FontSet 相关：CHARSET_REGISTRY / CHARSET_ENCODING 这两个属性名**不是**
        // 预定义 atom，但 Xlib 建 FontSet 时会拿它们查字体属性；FONT 属性的值是
        // "解析出的完整字体名"对应的 atom，CHARSET_REGISTRY / CHARSET_ENCODING
        // 的值是 "iso8859" 和 "1"。Xlib 拿 XGetAtomName 还原出这些字符串来判断
        // 字符集，缺了就报 "Unable to load any usable fontset"。
        for name in ["CHARSET_REGISTRY", "CHARSET_ENCODING"] {
            if let Ok(id) = self.intern_atom(name.as_bytes(), false) {
                atoms.push(id);
            }
        }
        for name in super::font::FONT_NAMES {
            if let Ok(id) = self.intern_atom(name.as_bytes(), false) {
                atoms.push(id);
            }
        }
        for name in ["iso8859", "1"] {
            if let Ok(id) = self.intern_atom(name.as_bytes(), false) {
                atoms.push(id);
            }
        }
        atoms
    }

    // ---- 客户端 ----

    pub fn register_client(&mut self, index: u32, sender: UnboundedSender<Vec<u8>>) {
        self.clients.insert(
            index,
            ClientHandle {
                sender,
                resource_id_base: resource_id_base(index),
            },
        );
    }

    pub fn send(&self, client: u32, frame: &[u8]) {
        if let Some(handle) = self.clients.get(&client) {
            // 客户端断开后队列可能已关闭；发不出去就算了，清理在连接结束时统一做
            let _ = handle.sender.send(frame.to_vec());
        }
    }

    pub fn send_reply<T: Serialize>(&self, client: u32, reply: &T) {
        // `Serialize::Bytes` 可能是数组也可能是 Vec，统一用 serialize_into 落到同一个缓冲
        let mut bytes = Vec::with_capacity(32);
        reply.serialize_into(&mut bytes);
        // 回复的固定部分是 32 字节，变长部分必须是 4 字节的整数倍；x11rb 只写有意义的
        // 字段（GetGeometry 是 24 字节、GetProperty 的 value 不带尾部填充），这里补齐。
        let padded = bytes.len().max(32).div_ceil(4) * 4;
        bytes.resize(padded, 0);

        // NyaTerm: **必须回填 length 字段**（偏移 4..8，单位是 4 字节，不含 32 字节
        // 固定头）。构造 reply 时该字段恒为 0，若不回填：32 字节的回复（QueryExtension /
        // InternAtom / GetGeometry）恰好正确，但 44 字节的 GetWindowAttributesReply
        // 会声明 length=0 —— 客户端只消费 32 字节，多出来的 12 字节被当成下一条回复的
        // 开头，整个协议流从此错位（表现为后续请求里出现 id=0 的窗口、客户端静默卡死，
        // 例如 firefox 建完探针窗口就不再建主窗口且不报错）。
        let length_units = (padded / 4) as u32 - 8; /* 8 units = the 32-byte fixed part */
        bytes[4..8].copy_from_slice(&length_units.to_ne_bytes());

        self.send(client, &bytes);
    }

    /// 连接断开：销毁该客户端拥有的全部资源（X11 默认 close-down mode = DestroyAll），
    /// 并向其他客户端补发 DestroyNotify。
    pub fn remove_client(&mut self, client: u32) {
        self.clients.remove(&client);

        // NyaTerm: 解除该客户端持有的抓取。否则一个退出的客户端会把指针/键盘
        // 永久钉在某个窗口上——之后所有输入都投给那个已经消失的窗口，界面
        // 彻底没反应。
        self.remove_xi2_client(client);
        if self.input.pointer_grab.map(|g| g.client) == Some(client) {
            self.input.pointer_grab = None;
        }
        if let Some(window) = self.input.keyboard_grab {
            let stale = !self.windows.contains_key(&window)
                || self.windows.get(&window).map(|w| w.owner) == Some(client);
            if stale {
                self.input.keyboard_grab = None;
            }
        }

        let owned_windows: Vec<Window> = self
            .windows
            .values()
            .filter(|w| w.owner == client && w.id != ROOT_WINDOW)
            .map(|w| w.id)
            .collect();
        for id in owned_windows {
            // 父窗口可能已经被同一轮销毁；destroy_window 内部会忽略不存在的 id
            self.destroy_window(id, 0);
        }

        self.pixmaps.retain(|_, p| p.owner != client);
        self.gcs.retain(|_, g| g.owner != client);
        self.colormaps.retain(|_, c| c.owner != client);
        self.fonts.retain(|_, f| f.owner != client);
        self.cursors.retain(|_, r| r.owner != client);
        // 扩展资源：XFIXES region / DAMAGE 都从客户端的 ID 区间分配，
        // 客户端断开时按区间销毁（与 windows/pixmaps 同一套规则）。
        // 注意此时该客户端已从 clients 表移除，直接用静态区间公式。
        let base = resource_id_base(client);
        self.regions.retain(|id, _| id & !RESOURCE_ID_MASK != base);
        self.damages
            .retain(|id, damage| id & !RESOURCE_ID_MASK != base && damage.owner != client);
        // RENDER 资源同样按区间销毁
        self.pictures
            .retain(|id, picture| id & !RESOURCE_ID_MASK != base && picture.owner != client);
        self.glyph_sets.retain(|id, _| id & !RESOURCE_ID_MASK != base);
        // 选择区的属主窗口没了，属主关系也就没了
        self.selections.retain(|_, owner| self.windows.contains_key(owner));
        self.sequences.remove(&client);

        for window in self.windows.values_mut() {
            window.event_masks.remove(&client);
        }

        self.mark_compose_dirty();
    }

    // ---- 资源 ----

    fn client_owns_id(&self, client: u32, id: u32) -> bool {
        match self.clients.get(&client) {
            Some(handle) => id & !RESOURCE_ID_MASK == handle.resource_id_base,
            None => false,
        }
    }

    pub fn resource_exists(&self, id: u32) -> bool {
        self.windows.contains_key(&id)
            || self.pixmaps.contains_key(&id)
            || self.gcs.contains_key(&id)
            || self.colormaps.contains_key(&id)
            || self.fonts.contains_key(&id)
            || self.cursors.contains_key(&id)
            || self.pictures.contains_key(&id)
            || self.glyph_sets.contains_key(&id)
    }

    /// 校验一个"即将创建"的资源 ID：必须落在本客户端区间内且没有被占用。
    pub fn check_new_id(&self, client: u32, id: u32) -> Result<(), XError> {
        if !self.client_owns_id(client, id) {
            return Err(XError::bad_id_choice(id));
        }
        if self.resource_exists(id) {
            return Err(XError::bad_id_choice(id));
        }
        Ok(())
    }

    pub fn window(&self, id: Window) -> Result<&WindowData, XError> {
        self.windows.get(&id).ok_or_else(|| XError::bad_window(id))
    }

    pub fn window_mut(&mut self, id: Window) -> Result<&mut WindowData, XError> {
        self.windows.get_mut(&id).ok_or_else(|| XError::bad_window(id))
    }

    pub fn gc(&self, id: Gcontext) -> Result<&GcData, XError> {
        self.gcs.get(&id).ok_or_else(|| XError::bad_gc(id))
    }

    /// 可绘制对象的几何信息：宽、高、深度。
    pub fn drawable_geometry(&self, id: Drawable) -> Result<(u16, u16, u8), XError> {
        if let Some(window) = self.windows.get(&id) {
            Ok((window.width, window.height, window.depth))
        } else if let Some(pixmap) = self.pixmaps.get(&id) {
            Ok((pixmap.width, pixmap.height, pixmap.depth))
        } else {
            Err(XError::bad_drawable(id))
        }
    }

    /// 取 drawable 几何，并**修复像素缓冲长度不变量**（`len == width * height`）。
    ///
    /// 为什么需要：所有绘制/合成路径都按 `width * height` 直接索引像素缓冲。
    /// 借还漏配（`BorrowedPixels`/`BorrowedBitmap` 是显式归还，没有 Drop 兜底）
    /// 会把某个 drawable 的缓冲永久抽空；此后任何一次索引都会越界 panic——
    /// 而绘制跑在**唯一的光栅化 worker** 上、合成跑在**帧泵任务**上，两者任一
    /// panic 都表现为「窗口都在、整个界面再也不刷新」，日志里没有任何异常。
    /// 这里在使用前把缓冲补齐到声明尺寸（内容留背景色），把"致命"降级成
    /// "那一块暂时是空的"，且下一次绘制就会自愈。
    pub fn drawable_geometry_checked(&mut self, id: Drawable) -> Result<(u16, u16, u8), XError> {
        if let Some(window) = self.windows.get_mut(&id) {
            let (w, h, depth) = (window.width, window.height, window.depth);
            let need = usize::from(w) * usize::from(h);
            if window.pixels.len() != need {
                window.pixels.resize(need, window.background_pixel);
            }
            Ok((w, h, depth))
        } else if let Some(pixmap) = self.pixmaps.get_mut(&id) {
            let (w, h, depth) = (pixmap.width, pixmap.height, pixmap.depth);
            let need = usize::from(w) * usize::from(h);
            // pixmap 没有背景色概念，补齐用 0（透明黑）
            if pixmap.pixels.len() != need {
                pixmap.pixels.resize(need, 0);
            }
            Ok((w, h, depth))
        } else {
            Err(XError::bad_drawable(id))
        }
    }

    // ---- atoms ----

    pub fn intern_atom(&mut self, name: &[u8], only_if_exists: bool) -> Result<Atom, XError> {
        // atom 名字按 Latin-1 处理；用 lossy 足够（协议只要求原样回显）
        let name = String::from_utf8_lossy(name).into_owned();
        if let Some(&atom) = self.atom_ids.get(&name) {
            return Ok(atom);
        }
        if only_if_exists {
            return Ok(0);
        }
        let atom = self.atoms.len() as Atom;
        self.atoms.push(Some(name.clone()));
        self.atom_ids.insert(name, atom);
        Ok(atom)
    }

    pub fn atom_name(&self, atom: Atom) -> Result<&str, XError> {
        self.atoms
            .get(atom as usize)
            .and_then(|name| name.as_deref())
            .ok_or_else(|| XError::bad_atom(atom))
    }

    // ---- 事件 ----

    /// 把事件投递给在 `window` 上选择了 `mask` 的客户端。
    ///
    /// `build` 的参数是事件帧里的 `event` 字段：同一次结构变化，窗口自己收到的是
    /// `event = window`，父窗口收到的是 `event = parent`，所以帧要按接收者重建。
    pub fn deliver(&self, window: Window, mask: u32, build: impl Fn(Window) -> Vec<u8>) {
        let Some(target) = self.windows.get(&window) else {
            return;
        };
        for (&client, &selected) in &target.event_masks {
            if selected & mask == 0 {
                continue;
            }
            if let Some(handle) = self.clients.get(&client) {
                // sequence 按接收方各自的进度盖写（build 里填的只是占位）
                let mut frame = pad_event(build(window));
                self.stamp_sequence(client, &mut frame);
                let _ = handle.sender.send(frame);
            }
        }
    }

    /// 核心输入事件投递（带 XI2 抑制）：跳过已协商 XI2 且在链上做过对应
    /// XI2 选择的客户端——XI 投递后核心投递终止（XI2 规范），双投会毁掉
    /// 点击/按键状态机。
    pub fn deliver_input_suppressed(
        &self,
        window: Window,
        mask: u32,
        evtype: u16,
        // task-24 输入绕行：true = 对 XI2 激活的客户端也投核心事件，帧带
        // send_event 位（GDK 的 XI2 管理器只为这种核心事件开例外，
        // gdkdevicemanager-xi2.c L1413——滚轮/键盘绕行就靠它）。
        xi2_bypass: bool,
        build: impl Fn(Window) -> Vec<u8>,
    ) {
        let Some(target) = self.windows.get(&window) else {
            return;
        };
        let mut delivered: Vec<u32> = Vec::new();
        // 第一轮：核心掩码选择者（XI2 激活的客户端按 xi2_bypass 决定去留）
        for (&client, &selected) in &target.event_masks {
            if selected & mask == 0 {
                continue;
            }
            let xi2_active = self.core_suppressed(client, evtype, true);
            if xi2_active && !xi2_bypass {
                // XI 投递后核心投递终止（XI2 规范）——正常路径跳过核心
                continue;
            }
            if let Some(handle) = self.clients.get(&client) {
                let mut frame = pad_event(build(window));
                if xi2_active {
                    // 绕行路径：GDK 的 XI2 管理器只翻译带 send_event 位的核心
                    // 事件（源码 L1413 的官方后门），置位才能穿过 XI2 客户端
                    frame[0] |= 0x80;
                }
                self.stamp_sequence(client, &mut frame);
                let _ = handle.sender.send(frame);
                delivered.push(client);
            }
        }
        // 第二轮（仅绕行）：**只选了 XI2、没有对应核心掩码**的客户端——GDK 就
        // 是这样（窗口上只选 XI2 键盘掩码），第一轮的 event_masks 遍历够不到
        // 它们；这里按 XI2 选择表补投（带 send_event 位），并按已投去重。
        if xi2_bypass {
            if let Some(selectors) = self.xi2_selections.get(&window) {
                for (&client, sel) in selectors {
                    if delivered.contains(&client) {
                        continue;
                    }
                    if !super::xinput::xi2_client_selects(selectors, client, evtype) {
                        continue;
                    }
                    if let Some(handle) = self.clients.get(&client) {
                        let mut frame = pad_event(build(window));
                        frame[0] |= 0x80;
                        self.stamp_sequence(client, &mut frame);
                        let _ = handle.sender.send(frame);
                    }
                }
            }
        }
    }
    /// 把事件帧的 sequence 字段改成**接收方客户端**当前的序列号。
    ///
    /// XCB 按序列号给事件与回复排序：注入输入产生的事件不在任何请求的处理
    /// 过程里（构造时只能填 0），发送前必须按接收方各自推进到的序列号改写，
    /// 否则客户端报 `[xcb] Unknown sequence number while processing queue`
    /// 并直接 abort。build 里填的值只是占位。
    pub fn stamp_sequence(&self, client: u32, frame: &mut [u8]) {
        let sequence = self.current_sequence(client).to_ne_bytes();
        frame[2..4].copy_from_slice(&sequence);
    }

    /// 某客户端当前推进到的序列号；还没发过请求就是 0。
    pub fn current_sequence(&self, client: u32) -> u16 {
        self.sequences.get(&client).copied().unwrap_or(0)
    }

    /// 给某个窗口的属主客户端直接发一条事件（不看 event mask）。
    ///
    /// `SelectionNotify` 这类"点对点"事件按协议就是发给请求方窗口的属主，
    /// 与它选了哪些 mask 无关。
    pub fn send_to_owner(&self, window: Window, mut frame: Vec<u8>) {
        let Some(target) = self.windows.get(&window) else {
            return;
        };
        let owner = target.owner;
        self.stamp_sequence(owner, &mut frame);
        if let Some(handle) = self.clients.get(&owner) {
            let _ = handle.sender.send(pad_event(frame));
        }
    }

    /// 给指定客户端发一条扩展事件帧：补齐 32 字节并按接收方的序列号盖写。
    /// DamageNotify 这类"发给创建者"的扩展事件走这里。
    pub fn send_client_event(&self, client: u32, frame: Vec<u8>) {
        let Some(handle) = self.clients.get(&client) else {
            return;
        };
        let mut frame = pad_event(frame);
        self.stamp_sequence(client, &mut frame);
        let _ = handle.sender.send(frame);
    }

    
/// 一个点（屏幕坐标）是否落在窗口的 bounding shape 内。
    /// 没有 bounding shape 的窗口恒为 true。shape 坐标以窗口**外角**为原点。
    pub fn shape_contains(&self, window: Window, screen_x: i32, screen_y: i32) -> bool {
        let Some(shape) = self.shapes.get(&window) else {
            return true;
        };
        let Some(data) = self.windows.get(&window) else {
            return true;
        };
        let border = i32::from(data.border_width);
        let outer_x = self.window_origin(window).0 - border;
        let outer_y = self.window_origin(window).1 - border;
        // bounding 决定可见区域，input 进一步决定可点击区域——两者都要满足，
        // 这正是 GTK 用 ShapeInput 挖掉阴影/圆角点击区的目的。
        if let Some(rects) = shape.bounding.as_ref() {
            if !rects.contains(screen_x - outer_x, screen_y - outer_y) {
                return false;
            }
        }
        if let Some(rects) = shape.input.as_ref() {
            if !rects.contains(screen_x - outer_x, screen_y - outer_y) {
                return false;
            }
        }
        true
    }

    /// 标记绑定在 `drawable` 上的所有 damage 为脏（绘制入口统一调用）。
    pub fn mark_damage(&mut self, drawable: Drawable) {
        for damage in self.damages.values_mut() {
            if damage.drawable == drawable {
                damage.dirty = true;
            }
        }
    }

    /// 对所有脏 damage 发 DamageNotify（compose 收尾时调用）。
    fn notify_damages(&mut self) {
        // 先收集要发的帧（收集阶段需要 &self.windows），最后统一发送
        let mut pending = Vec::new();
        for (&damage_id, data) in &self.damages {
            if !data.dirty {
                continue;
            }
            let Some((width, height, origin_x, origin_y)) = (|| {
                if let Some(window) = self.windows.get(&data.drawable) {
                    Some((window.width, window.height, i32::from(window.x), i32::from(window.y)))
                } else {
                    let pixmap = self.pixmaps.get(&data.drawable)?;
                    Some((pixmap.width, pixmap.height, 0, 0))
                }
            })() else {
                continue;
            };
            let area = Rectangle { x: 0, y: 0, width, height };
            let geometry = Rectangle {
                x: origin_x as i16,
                y: origin_y as i16,
                width,
                height,
            };
            let level = data.level;
            let drawable = data.drawable;
            let owner = data.owner;
            let frame = super::extensions::damage_notify_frame_for(damage_id, level, drawable, area, geometry);
            pending.push((owner, frame));
        }
        for (owner, frame) in pending {
            self.send_client_event(owner, frame);
        }
        for data in self.damages.values_mut() {
            data.dirty = false;
        }
    }

    /// 光标必须存在（`FreeCursor` / `RecolorCursor` 走这里）。
    pub fn cursor(&self, id: Cursor) -> Result<(), XError> {
        if self.cursors.contains_key(&id) {
            Ok(())
        } else {
            Err(XError::bad_cursor(id))
        }
    }

    /// 窗口当前是否可见（自己和所有祖先都 mapped）。
    pub fn map_state(&self, id: Window) -> MapState {
        let Some(window) = self.windows.get(&id) else {
            return MapState::UNMAPPED;
        };
        if !window.mapped {
            return MapState::UNMAPPED;
        }
        let mut current = window.parent;
        while current != 0 && current != ROOT_WINDOW {
            let Some(parent) = self.windows.get(&current) else {
                return MapState::UNMAPPED;
            };
            if !parent.mapped {
                return MapState::UNVIEWABLE;
            }
            current = parent.parent;
        }
        MapState::VIEWABLE
    }

    // ---- 窗口 ----

    pub fn create_window(
        &mut self,
        client: u32,
        sequence: u16,
        request: &CreateWindowRequest<'_>,
    ) -> Result<(), XError> {
        // NyaTerm diagnostic (极廉价，常开)：窗口生命周期。firefox 建窗后窗口不
        // 出现、以及它在空转重绘，都要靠这几条才能定位。
        crate::xsrv::log_line(format_args!(
            "[x11] CreateWindow wid={:#x} parent={:#x} depth={} class={:?} visual={:#x} {}x{}",
            request.wid,
            request.parent,
            request.depth,
            request.class,
            request.visual,
            request.width,
            request.height
        ));

        self.check_new_id(client, request.wid)?;

        let parent = self.window(request.parent)?.clone_geometry();
        if request.width == 0 || request.height == 0 {
            return Err(XError::bad_value(0));
        }

        let class = if request.class == WindowClass::COPY_FROM_PARENT {
            WindowClass::INPUT_OUTPUT
        } else {
            request.class
        };
        // InputOnly 窗口没有可绘制内容：depth 必须是 CopyFromParent(0)，这个
        // 分流必须发生在 "depth == 0 表示继承父窗口" 展开之前，否则 firefox/GTK
        // 的 1x1 InputOnly 子窗口会继承父窗口的 depth 而被误判为 BadMatch。
        // visual 对 InputOnly 没有意义——firefox/GTK 的 10x10 InputOnly 窗口会
        // 顺手传默认视觉(0x21)，Xorg 也忽略它；照做，否则窗口会被整个丢掉，
        // 客户端随后的 ChangeProperty 全部变成 BadWindow。
        let input_only = class == WindowClass::INPUT_ONLY;
        if input_only && request.depth != 0 {
            return Err(XError::bad_match());
        }

        let depth = if input_only {
            0
        } else if request.depth == 0 {
            parent.depth
        } else {
            request.depth
        };
        // NyaTerm: 接受 depth 24（根视觉）与 depth 32（ARGB32 视觉）。
        // firefox/GTK 会创建 depth 32 的 ARGB 窗口做透明合成——规范允许
        // 子窗口深度不同于父窗口，只要 depth/visual 组合受支持。
        let depth32 = depth == 32;
        if !input_only && depth != parent.depth && !depth32 {
            // NyaTerm diagnostic: depth mismatch (firefox/GTK depth-32 ARGB).
            if let Some(path) = std::env::var_os("NYATERM_RDPDR_LOG") {
                if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                    use std::io::Write as _;
                    let _ = writeln!(
                        file,
                        "[x11] CreateWindow BadMatch(depth): wid={:#x} depth={} parent_depth={} visual={:#x}",
                        request.wid, depth, parent.depth, request.visual
                    );
                }
            }
            return Err(XError::bad_match());
        }

        let visual = if input_only {
            0
        } else if request.visual == 0 {
            if depth32 { ARGB32_VISUAL } else { parent.visual }
        } else {
            request.visual
        };
        let visual_ok = if input_only {
            true
        } else if depth32 {
            visual == ARGB32_VISUAL
        } else {
            visual == ROOT_VISUAL
        };
        if !visual_ok {
            // NyaTerm diagnostic: log what the client actually requested so a
            // BadMatch from firefox/GTK (depth-32 ARGB windows) can be traced.
            if let Some(path) = std::env::var_os("NYATERM_RDPDR_LOG") {
                if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                    use std::io::Write as _;
                    let _ = writeln!(
                        file,
                        "[x11] CreateWindow BadMatch: wid={:#x} parent={:#x} depth={} (parent {}) visual={:#x} class={:?}",
                        request.wid,
                        request.parent,
                        request.depth,
                        parent.depth,
                        request.visual,
                        request.class
                    );
                }
            }
            return Err(XError::bad_match());
        }

        let aux = &request.value_list;
        let background_pixel = match (aux.background_pixel, aux.background_pixmap) {
            (Some(pixel), _) => pixel,
            // ParentRelative(1) / CopyFromParent(0)：继承父窗口背景
            (None, Some(1)) | (None, None) => parent.background_pixel,
            // 其他 pixmap 背景 M1 不支持，退化成黑色
            (None, Some(_)) => 0,
        };
        let border_pixel = aux.border_pixel.unwrap_or(0);
        let override_redirect = aux.override_redirect.unwrap_or(0) != 0;
        let colormap = aux.colormap.unwrap_or(ROOT_COLORMAP);
        if colormap != ROOT_COLORMAP && !self.colormaps.contains_key(&colormap) {
            return Err(XError::bad_colormap(colormap));
        }

        let width = request.width;
        let height = request.height;
        let window = WindowData {
            id: request.wid,
            parent: request.parent,
            depth,
            class,
            x: request.x,
            y: request.y,
            width,
            height,
            border_width: request.border_width,
            mapped: false,
            override_redirect,
            background_pixel,
            border_pixel,
            colormap,
            visual,
            bit_gravity: aux.bit_gravity.unwrap_or(Gravity::NORTH_WEST),
            win_gravity: aux.win_gravity.unwrap_or(Gravity::NORTH_WEST),
            do_not_propagate: aux.do_not_propogate_mask.map(u32::from).unwrap_or(0),
            event_masks: HashMap::new(),
            children: Vec::new(),
            pixels: if input_only {
                // InputOnly 窗口不显示任何内容。
                Vec::new()
            } else {
                vec![background_pixel; width as usize * height as usize]
            },
            properties: HashMap::new(),
            owner: client,
        };
        let mut event_masks = HashMap::new();
        if let Some(mask) = aux.event_mask {
            event_masks.insert(client, u32::from(mask));
        }

        self.windows.insert(request.wid, window);
        if let Some(parent) = self.windows.get_mut(&request.parent) {
            parent.children.push(request.wid);
        }
        if let Some(window) = self.windows.get_mut(&request.wid) {
            window.event_masks = event_masks;
        }

        // CreateNotify 只发给在父窗口上选了 SubstructureNotify 的客户端。
        // 它没有序列号参数：由分发层在事件帧里回填当前序列号。
        self.deliver(request.parent, u32::from(EventMask::SUBSTRUCTURE_NOTIFY), |event| {
            CreateNotifyEvent {
                response_type: CREATE_NOTIFY_EVENT,
                sequence,
                parent: event,
                window: request.wid,
                x: request.x,
                y: request.y,
                width,
                height,
                border_width: request.border_width,
                override_redirect,
            }
            .serialize()
            .to_vec()
        });

        Ok(())
    }

    pub fn destroy_window(&mut self, id: Window, sequence: u16) -> Result<(), XError> {
        // NyaTerm diagnostic (极廉价，常开)：窗口销毁时序。
        crate::xsrv::log_line(format_args!(
            "[x11] DestroyWindow id={id:#x} owner={:?}",
            self.windows.get(&id).map(|window| window.owner)
        ));

        if id == ROOT_WINDOW {
            return Err(XError::bad_window(id));
        }
        let Some(window) = self.windows.get(&id) else {
            return Err(XError::bad_window(id));
        };
        let parent = window.parent;
        let children = window.children.clone();
        if let Some((x, y, x1, y1)) = self.damage_rect_of(id) {
            self.damage_rect(x, y, x1 - x, y1 - y);
        }

        for child in children {
            let _ = self.destroy_window(child, sequence);
        }

        // DestroyNotify 发给谁，取决于窗口和父窗口上各自选的事件掩码，所以窗口
        // 自己那份必须在它离开资源表之前投递（deliver 需要窗口上的 event_masks）。
        let notify = |event| {
            DestroyNotifyEvent {
                response_type: DESTROY_NOTIFY_EVENT,
                sequence,
                event,
                window: id,
            }
            .serialize()
            .to_vec()
        };
        self.deliver(id, u32::from(EventMask::STRUCTURE_NOTIFY), notify);

        if let Some(parent) = self.windows.get_mut(&parent) {
            parent.children.retain(|child| *child != id);
        }
        // **X 语义：窗口销毁后必须忘掉它在输入状态里的一切痕迹**（指针缓存、
        // 焦点、抓取、XI2 选择）。否则命中测试/键盘焦点会一直指向一个**不存在
        // 的窗口**：客户端（GDK）按事件里的 `event` 字段查自己的窗口表得到
        // NULL，在 `translate_event` 开头直接 `return FALSE` **静默丢弃**——
        // 症状是"远端画面照旧刷新，但点击/滚轮/键盘全部无反应"，且没有任何
        // X 错误或 Gdk-CRITICAL（实测用户正是此现象）。
        if self.input.pointer_window == Some(id) {
            self.input.pointer_window = None;
        }
        if self.input.implicit_grab == Some(id) {
            self.input.implicit_grab = None;
        }
        if self.input.pointer_grab.as_ref().map(|grab| grab.window) == Some(id) {
            self.input.pointer_grab = None;
        }
        if self.input.keyboard_grab == Some(id) {
            self.input.keyboard_grab = None;
        }
        if self.input.focus == id {
            // X：焦点窗口被销毁 → 焦点回到 PointerRoot（跟随指针）
            self.input.focus = super::input::POINTER_ROOT;
        }
        self.xi2_selections.remove(&id);
        self.windows.remove(&id);
        // 扩展资源跟着窗口走：SHAPE 形状、绑定在这个窗口上的 damage、
        // 以及包装这个窗口的 RENDER picture
        self.shapes.remove(&id);
        self.damages.retain(|_, damage| damage.drawable != id);
        self.pictures.retain(|_, picture| match picture.source {
            PictureSource::Drawable(drawable) => drawable != id,
            PictureSource::Solid { .. } => true,
            // 渐变源不依附 drawable（附着在创建它的客户端上）
            PictureSource::LinearGradient { .. }
            | PictureSource::RadialGradient { .. }
            | PictureSource::ConicalGradient { .. } => true,
        });

        self.deliver(parent, u32::from(EventMask::SUBSTRUCTURE_NOTIFY), notify);

        self.mark_compose_dirty();
        Ok(())
    }

    /// task-24 内存账目：本服务端持有的像素字节（窗口 + pixmap 缓冲）。
    /// 返回 `(窗口数, pixmap 数, 像素字节)`——用来一刀切开"内存涨在服务端缓冲"
    /// 还是"涨在 UI/渲染器侧"（用户实测进程涨到 7.8GB，必须先定位归属）。
    pub fn pixel_bytes(&self) -> (usize, usize, usize) {
        let win: usize = self.windows.values().map(|w| w.pixels.len() * 4).sum();
        let pix: usize = self.pixmaps.values().map(|p| p.pixels.len() * 4).sum();
        (self.windows.len(), self.pixmaps.len(), win + pix)
    }

    pub fn map_window(&mut self, id: Window, sequence: u16) -> Result<(), XError> {
        crate::xsrv::log_line(format_args!("[x11] MapWindow id={id:#x}"));
        let (parent, width, height, override_redirect) = {
            let window = self.window(id)?;
            (window.parent, window.width, window.height, window.override_redirect)
        };
        if self.window(id)?.mapped {
            return Ok(());
        }
        self.window_mut(id)?.mapped = true;

        self.deliver(id, u32::from(EventMask::STRUCTURE_NOTIFY), |event| {
            MapNotifyEvent {
                response_type: MAP_NOTIFY_EVENT,
                sequence,
                event,
                window: id,
                override_redirect,
            }
            .serialize()
            .to_vec()
        });
        self.deliver(parent, u32::from(EventMask::SUBSTRUCTURE_NOTIFY), |event| {
            MapNotifyEvent {
                response_type: MAP_NOTIFY_EVENT,
                sequence,
                event,
                window: id,
                override_redirect,
            }
            .serialize()
            .to_vec()
        });
        self.deliver(id, u32::from(EventMask::EXPOSURE), |_| {
            ExposeEvent {
                response_type: EXPOSE_EVENT,
                sequence,
                window: id,
                x: 0,
                y: 0,
                width,
                height,
                count: 0,
            }
            .serialize()
            .to_vec()
        });

        // task-22 按需显示：有窗口映射而 UI 没挂观众时通知上层（自动弹出
        // X 屏幕 / 状态栏提示）。去重与弹窗逻辑在回调里，这里只管触发。
        if !self.viewer_attached.load(std::sync::atomic::Ordering::Acquire) {
            if let Some(cb) = self.on_map_no_viewer.clone() {
                cb();
            }
        }

        self.mark_compose_dirty();
        Ok(())
    }

    /// 把一个屏幕矩形并入脏区提示（窗口消失/移动时，旧位置会露出背景）。
    fn damage_rect(&mut self, x: i32, y: i32, width: i32, height: i32) {
        if width <= 0 || height <= 0 {
            return;
        }
        let rect = (x, y, x + width, y + height);
        self.damage_hint = Some(match self.damage_hint {
            Some((x0, y0, x1, y1)) => (x0.min(rect.0), y0.min(rect.1), x1.max(rect.2), y1.max(rect.3)),
            None => rect,
        });
    }

    /// 窗口当前内容区（含边框）的屏幕矩形——dirty 记录用。
    fn damage_rect_of(&self, id: Window) -> Option<(i32, i32, i32, i32)> {
        let data = self.windows.get(&id)?;
        let (ox, oy) = self.window_origin(id);
        Some((
            ox,
            oy,
            ox + i32::from(data.width) + 2 * i32::from(data.border_width),
            oy + i32::from(data.height) + 2 * i32::from(data.border_width),
        ))
    }

    pub fn unmap_window(&mut self, id: Window, sequence: u16) -> Result<(), XError> {
        let (parent, mapped) = {
            let window = self.window(id)?;
            (window.parent, window.mapped)
        };
        if !mapped {
            return Ok(());
        }
        if let Some((x, y, x1, y1)) = self.damage_rect_of(id) {
            self.damage_rect(x, y, x1 - x, y1 - y);
        }
        self.window_mut(id)?.mapped = false;

        self.deliver(id, u32::from(EventMask::STRUCTURE_NOTIFY), |event| {
            UnmapNotifyEvent {
                response_type: UNMAP_NOTIFY_EVENT,
                sequence,
                event,
                window: id,
                from_configure: false,
            }
            .serialize()
            .to_vec()
        });
        self.deliver(parent, u32::from(EventMask::SUBSTRUCTURE_NOTIFY), |event| {
            UnmapNotifyEvent {
                response_type: UNMAP_NOTIFY_EVENT,
                sequence,
                event,
                window: id,
                from_configure: false,
            }
            .serialize()
            .to_vec()
        });

        self.mark_compose_dirty();
        Ok(())
    }

    pub fn configure_window(
        &mut self,
        id: Window,
        sequence: u16,
        request: &ConfigureWindowRequest<'_>,
    ) -> Result<(), XError> {
        let aux = &request.value_list;
        let (parent, old_width, old_height, mapped, override_redirect) = {
            let window = self.window(id)?;
            (window.parent, window.width, window.height, window.mapped, window.override_redirect)
        };

        let width = match aux.width {
            Some(0) => return Err(XError::bad_value(0)),
            Some(width) => u16::try_from(width).map_err(|_| XError::bad_value(width))?,
            None => old_width,
        };
        let height = match aux.height {
            Some(0) => return Err(XError::bad_value(0)),
            Some(height) => u16::try_from(height).map_err(|_| XError::bad_value(height))?,
            None => old_height,
        };
        let x = aux
            .x
            .map(|value| i16::try_from(value).map_err(|_| XError::bad_value(value as u32)))
            .transpose()?;
        let y = aux
            .y
            .map(|value| i16::try_from(value).map_err(|_| XError::bad_value(value as u32)))
            .transpose()?;
        let border_width = aux
            .border_width
            .map(|value| u16::try_from(value).map_err(|_| XError::bad_value(value)))
            .transpose()?;

        let resized = width != old_width || height != old_height;
        // 移动/缩放会让旧位置露出背景：旧矩形并入脏区提示。
        if mapped && (x.is_some() || y.is_some() || resized || border_width.is_some()) {
            if let Some((ox, oy, x1, y1)) = self.damage_rect_of(id) {
                self.damage_rect(ox, oy, x1 - ox, y1 - oy);
            }
        }
        {
            let window = self.window_mut(id)?;
            if let Some(x) = x {
                window.x = x;
            }
            if let Some(y) = y {
                window.y = y;
            }
            if let Some(border) = border_width {
                window.border_width = border;
            }
            if resized {
                window.width = width;
                window.height = height;
                let background = window.background_pixel;
                window.pixels = vec![background; window.content_len()];
            }
        }

        self.deliver(id, u32::from(EventMask::STRUCTURE_NOTIFY), |event| {
            ConfigureNotifyEvent {
                response_type: CONFIGURE_NOTIFY_EVENT,
                sequence,
                event,
                window: id,
                above_sibling: 0,
                x: self.window(id).map(|w| w.x).unwrap_or(0),
                y: self.window(id).map(|w| w.y).unwrap_or(0),
                width,
                height,
                border_width: self.window(id).map(|w| w.border_width).unwrap_or(0),
                override_redirect,
            }
            .serialize()
            .to_vec()
        });
        self.deliver(parent, u32::from(EventMask::SUBSTRUCTURE_NOTIFY), |event| {
            ConfigureNotifyEvent {
                response_type: CONFIGURE_NOTIFY_EVENT,
                sequence,
                event,
                window: id,
                above_sibling: 0,
                x: self.window(id).map(|w| w.x).unwrap_or(0),
                y: self.window(id).map(|w| w.y).unwrap_or(0),
                width,
                height,
                border_width: self.window(id).map(|w| w.border_width).unwrap_or(0),
                override_redirect,
            }
            .serialize()
            .to_vec()
        });

        if resized && mapped {
            self.deliver(id, u32::from(EventMask::EXPOSURE), |_| {
                ExposeEvent {
                    response_type: EXPOSE_EVENT,
                    sequence,
                    window: id,
                    x: 0,
                    y: 0,
                    width,
                    height,
                    count: 0,
                }
                .serialize()
                .to_vec()
            });
        }

        self.mark_compose_dirty();
        Ok(())
    }

    // ---- 合成 ----

    /// 从根窗口重画整个屏幕。
    ///
    /// 先在局部缓冲里画，最后再写回根窗口的 `pixels`：递归需要同时读窗口树
    /// （`&self.windows`）和写目标缓冲，先 `take` 出来可以避免借用冲突。
    /// 缓冲是**复用**的（`mem::take` 后原样放回），不是每次重画都重新分配——
    /// 每个绘制请求都会走这里，1280x800 的缓冲按 60fps 分配会很浪费。
    /// 绘制/结构变化后的收尾：只标记，不立即合成（见 compose_dirty 注释）。
    pub fn mark_compose_dirty(&mut self) {
        self.compose_dirty = true;
    }

    /// 有待合成时立即合成一次（帧泵取帧、GetImage 读根窗口前调用）。
    pub fn ensure_composed(&mut self) {
        if self.compose_dirty {
            self.compose_dirty = false;
            self.compose();
        }
    }

    pub fn compose(&mut self) {
        let (width, height, background) = match self.windows.get(&ROOT_WINDOW) {
            Some(root) => (root.width as usize, root.height as usize, root.background_pixel),
            None => return,
        };
        let mut screen = match self.windows.get_mut(&ROOT_WINDOW) {
            Some(root) => std::mem::take(&mut root.pixels),
            None => return,
        };
        screen.resize(width * height, background);
        screen.fill(background);

        // 合成的绘制体做 panic 隔离：根窗口缓冲此刻是 `mem::take` 出来的，
        // 而 `compose_dirty` 已在 ensure_composed 里清零。若这里 panic 直接
        // 穿透，缓冲再也放不回去、脏标志也不会重置 → 之后每帧都 panic（画面
        // 永久冻结且"看起来只是没变化"）。所以：先把缓冲放回去、把脏标志置回，
        // 再 resume（帧泵侧也有 catch_unwind，下一次 tick 会重试）。
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            compose_children(
                &mut screen,
                width,
                height,
                0,
                0,
                width as i32,
                height as i32,
                ROOT_WINDOW,
                &self.windows,
                &self.shapes,
            );
        }));
        let panicked = outcome.is_err();
        if let Some(root) = self.windows.get_mut(&ROOT_WINDOW) {
            if panicked && screen.len() != width * height {
                screen.resize(width * height, background);
            }
            root.pixels = screen;
        }
        if panicked {
            self.compose_dirty = true;
            std::panic::resume_unwind(outcome.unwrap_err());
        }

        // 版本号只增不减：Viewer 拿它和上次发送的版本比较，相同就跳过编码
        self.frame_seq = self.frame_seq.wrapping_add(1);

        // NyaTerm: 维护脏区提示——本轮合成把所有映射的 INPUT_OUTPUT 窗口都重画
        // 了一遍，所以"可能变化的区域"就是它们的并集。帧泵据此把像素对比缩到
        // 窗口区域，空闲桌面（只有根窗口背景）几乎零成本。
        let mut hint = self.damage_hint;
        for (id, data) in &self.windows {
            if !data.mapped || *id == ROOT_WINDOW || data.class == WindowClass::INPUT_ONLY {
                continue;
            }
            let (ox, oy) = self.window_origin(*id);
            let rect = (
                ox,
                oy,
                ox + i32::from(data.width) + 2 * i32::from(data.border_width),
                oy + i32::from(data.height) + 2 * i32::from(data.border_width),
            );
            hint = Some(match hint {
                Some((x0, y0, x1, y1)) => (x0.min(rect.0), y0.min(rect.1), x1.max(rect.2), y1.max(rect.3)),
                None => rect,
            });
        }
        self.damage_hint = hint;

        crate::xsrv::counters::COMPOSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // DAMAGE：合成完一轮，把脏 damage 的通知发出去（初始通知在 DamageCreate）
        self.notify_damages();
    }

    /// 屏幕尺寸与当前像素缓冲（只读）。帧导出走这里，Viewer 不直接碰窗口树。
    pub fn screen(&self) -> Option<(u16, u16, &[u32])> {
        self.windows
            .get(&ROOT_WINDOW)
            .map(|root| (root.width, root.height, root.pixels.as_slice()))
    }

    /// 观众离开：作废已发送帧的基准，下次取帧一定走全屏。
    pub fn invalidate_sent_frame(&mut self) {
        // frame_seq 从 1 开始自增，0 一定不等于当前版本 → 触发全帧。
        self.sent_seq = 0;
        self.damage_hint = None;
    }

    /// 帧泵取帧（task-17 阶段 B：脏区分带）：与上次发送的帧在"脏区提示"范围
    /// 内逐行比对，把连续变化行聚成最多 8 条**水平带**，每带独立抠取返回。
    ///
    /// 返回 `None` 表示自上次取帧以来屏幕没有任何变化——此时**不做任何
    /// 内存拷贝**。这是把空闲会话的 CPU 从"每 33ms 三遍整屏"降下来的关键。
    ///
    /// 一次取走后基准帧就地更新，所以同一版本不会被重复发送。
    /// 旧调用方兼容：`take_frame_if_changed` 返回所有带的并集矩形。
    pub fn take_frame_if_changed(&mut self) -> Option<(u64, u16, u16, u16, u16, Vec<u32>)> {
        let (seq, bands) = self.take_frame_if_changed_banded()?;
        // 并集矩形（基准帧此刻 == 当前屏幕，直接从基准帧抠）
        let width = usize::from(self.sent_size.0);
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, 0, 0);
        for b in &bands {
            x0 = x0.min(i32::from(b.x));
            y0 = y0.min(i32::from(b.y));
            x1 = x1.max(i32::from(b.x) + i32::from(b.width));
            y1 = y1.max(i32::from(b.y) + i32::from(b.height));
        }
        let rect_w = (x1 - x0) as usize;
        let rect_h = (y1 - y0) as usize;
        let mut pixels = Vec::with_capacity(rect_w * rect_h);
        for row in y0..y1 {
            let start = row as usize * width + x0 as usize;
            pixels.extend_from_slice(&self.sent_frame[start..start + rect_w]);
        }
        Some((seq, x0 as u16, y0 as u16, rect_w as u16, rect_h as u16, pixels))
    }

    /// 分带版取帧（task-18 前的行为：diff + 提交基准帧一步完成，`&mut self`）。
    /// RwLock 快照路径请用 `diff_frame_bands` + `commit_frame_bands`
    /// （`ServerShared::take_frame_snapshot` 已封装：读锁 diff、写锁提交）。
    pub fn take_frame_if_changed_banded(&mut self) -> Option<(u64, Vec<FrameBand>)> {
        // NyaTerm: 绘制请求只标脏，这里（33ms 一次）统一合成——全屏重建的
        // 频率从"每绘制请求一次"降到 30fps 上限。
        self.ensure_composed();
        let diff = self.diff_frame_bands()?;
        self.commit_frame_bands(&diff);
        Some((diff.seq, diff.bands))
    }

    /// **读阶段（纯读 `&self`）**：与基准帧在脏区提示范围内逐行比对，把连续
    /// 变化行聚成 ≤8 条水平带并把像素**拷出来**。不碰基准帧/版本号/脏区提示。
    /// 返回 `None` = 屏幕没有变化（零拷贝）。
    pub fn diff_frame_bands(&self) -> Option<FrameDiff> {
        let seq = self.frame_seq;
        if seq == self.sent_seq {
            return None;
        }
        let Some(root) = self.windows.get(&ROOT_WINDOW) else {
            return None;
        };
        let width = usize::from(root.width);
        let height = usize::from(root.height);
        if width == 0 || height == 0 {
            return None;
        }

        let size_changed = self.sent_size != (width, height) || self.sent_frame.is_empty();

        // 对比范围：脏区提示（钳到屏幕内），尺寸变化/无提示时退回全屏。
        let (rx0, ry0, rx1, ry1) = if size_changed {
            (0i32, 0i32, width as i32, height as i32)
        } else {
            match self.damage_hint {
                Some((x0, y0, x1, y1)) => {
                    let x0 = x0.clamp(0, width as i32);
                    let y0 = y0.clamp(0, height as i32);
                    let x1 = x1.clamp(0, width as i32);
                    let y1 = y1.clamp(0, height as i32);
                    if x1 <= x0 || y1 <= y0 {
                        // 提示区域为空：版本变了但区域无效，保守起见走全屏
                        (0, 0, width as i32, height as i32)
                    } else {
                        (x0, y0, x1, y1)
                    }
                }
                None => (0, 0, width as i32, height as i32),
            }
        };

        // 拷贝区域像素（矩形本地的行优先）。借用在此结束，后面才能改 self。
        let region_pixels: Vec<u32> = {
            let pixels = &root.pixels;
            let mut buffer = Vec::with_capacity((rx1 - rx0) as usize * (ry1 - ry0) as usize);
            for row in ry0..ry1 {
                let start = row as usize * width + rx0 as usize;
                buffer.extend_from_slice(&pixels[start..start + (rx1 - rx0) as usize]);
            }
            buffer
        };

        // 与基准帧在区域内逐行比对：记录每个变化行的 x 范围（绝对坐标）。
        let region_w = (rx1 - rx0) as usize;
        let mut changed_rows: Vec<(i32, i32, i32)> = Vec::new(); // (row, first_x, last_x+1)
        let mut any_diff = size_changed; // 尺寸变了就整块都算脏
        if !size_changed {
            let sent_w = self.sent_size.0;
            let sent = &self.sent_frame;
            for row in ry0..ry1 {
                let y = (row - ry0) as usize;
                let start = y * region_w;
                let current = &region_pixels[start..start + region_w];
                let base = row as usize * sent_w + rx0 as usize;
                let previous = &sent[base..base + region_w];
                let mut first: Option<i32> = None;
                let mut last = 0i32;
                for (offset, (a, b)) in current.iter().zip(previous.iter()).enumerate() {
                    if a != b {
                        let x = rx0 + offset as i32;
                        if first.is_none() {
                            first = Some(x);
                        }
                        last = x + 1;
                    }
                }
                if let Some(first) = first {
                    changed_rows.push((row, first, last));
                    any_diff = true;
                }
            }
            if !any_diff {
                // 提示区域内其实没变（提示偏保守）：纯读路径不消费版本，
                // 版本与提示由提交阶段（consume_frame_version）消费
                return None;
            }
        } else {
            changed_rows.push((ry0, rx0, rx1));
        }

        // task-17 阶段 B：把连续的变化行聚成**带**（band）——上下分离的两块
        // 小改动（如顶栏 + 光标行）不再被外接矩形夹着大片未变区域整包上传。
        // 每带像素量 ≈ 变化区本体；damage 通常只占屏幕 20~40%，上传字节降 2~4 倍。
        // 带数上限：超过就退化回单一外接矩形（老行为），避免帧头数量爆炸。
        const MAX_FRAME_BANDS: usize = 8;
        let mut segments: Vec<(i32, i32, i32, i32)> = Vec::new(); // (y0, y1, x0, x1)
        {
            let mut iter = changed_rows.into_iter().peekable();
            while let Some((row0, fx, lx)) = iter.next() {
                let (mut y_end, mut x0, mut x1) = (row0 + 1, fx, lx);
                while let Some(&(row, fx, lx)) = iter.peek() {
                    if row == y_end {
                        y_end += 1;
                        x0 = x0.min(fx);
                        x1 = x1.max(lx);
                        iter.next();
                    } else {
                        break;
                    }
                }
                segments.push((row0, y_end, x0, x1));
            }
            if segments.len() > MAX_FRAME_BANDS {
                // 退化：单一外接矩形
                let (mut x0, mut y0, mut x1, mut y1) =
                    (rx1, ry1, rx0, ry0);
                for (sy0, sy1, sx0, sx1) in &segments {
                    y0 = y0.min(*sy0);
                    y1 = y1.max(*sy1);
                    x0 = x0.min(*sx0);
                    x1 = x1.max(*sx1);
                }
                segments = vec![(y0, y1, x0, x1)];
            }
        }

        // 每带独立抠像素（数据拷出；基准帧更新在 commit_frame_bands 写阶段）。
        let mut bands = Vec::with_capacity(segments.len());
        for (by0, by1, bx0, bx1) in segments {
            let band_w = (bx1 - bx0) as usize;
            let band_h = (by1 - by0) as usize;
            let mut band_pixels = Vec::with_capacity(band_w * band_h);
            for row in by0..by1 {
                let start = ((row - ry0) as usize) * region_w + (bx0 - rx0) as usize;
                band_pixels.extend_from_slice(&region_pixels[start..start + band_w]);
            }
            bands.push(FrameBand {
                x: bx0 as u16,
                y: by0 as u16,
                width: band_w as u16,
                height: band_h as u16,
                pixels: band_pixels,
            });
        }

        Some(FrameDiff { seq, size_changed, bands })
    }

    /// 提交阶段的一部分：diff 报告「提示区域内其实没变」时，消费版本号与
    /// 提示，避免每帧重复扫同一区域（写锁窗口极小，无拷贝）。
    pub fn consume_frame_version(&mut self) {
        self.sent_seq = self.frame_seq;
        self.damage_hint = None;
    }

    /// 是否有待消费的屏幕版本（诊断/提交阶段判断用）。
    pub fn frame_version_pending(&self) -> bool {
        self.frame_seq != self.sent_seq
    }
}

impl WindowData {
    /// 拷贝一份构造窗口时需要的父窗口信息，避免长时间持有不可变借用。
    fn clone_geometry(&self) -> ParentGeometry {
        ParentGeometry {
            depth: self.depth,
            visual: self.visual,
            background_pixel: self.background_pixel,
        }
    }
}

/// 绘制与读取的入口：窗口绘制后自动重新合成屏幕。
impl ServerState {
    /// **写阶段（`&mut self`，窗口极小）**：把 diff 取走的带写回基准帧 +
    /// 消费版本号与脏区提示。按行覆写，不重新 diff（diff 在
    /// `diff_frame_bands` 读阶段完成）。
    pub fn commit_frame_bands(&mut self, diff: &FrameDiff) {
        let (cur_w, cur_h) = {
            let root = self.windows.get(&ROOT_WINDOW).expect("根窗口");
            (usize::from(root.width), usize::from(root.height))
        };
        if diff.size_changed {
            self.sent_frame = vec![0; cur_w * cur_h];
            self.sent_size = (cur_w, cur_h);
        }
        for band in &diff.bands {
            let band_w = band.width as usize;
            for row in 0..band.height as usize {
                let dst = (band.y as usize + row) * self.sent_size.0 + band.x as usize;
                let src = row * band_w;
                self.sent_frame[dst..dst + band_w]
                    .copy_from_slice(&band.pixels[src..src + band_w]);
            }
        }
        self.sent_seq = diff.seq;
        self.damage_hint = None;
    }

    /// 只读获取可绘制对象的像素、宽、高、深度。
    pub fn drawable_pixels(&self, id: Drawable) -> Result<(&[u32], usize, usize, u8), XError> {
        if let Some(window) = self.windows.get(&id) {
            Ok((
                &window.pixels,
                window.width as usize,
                window.height as usize,
                window.depth,
            ))
        } else if let Some(pixmap) = self.pixmaps.get(&id) {
            Ok((
                &pixmap.pixels,
                pixmap.width as usize,
                pixmap.height as usize,
                pixmap.depth,
            ))
        } else {
            Err(XError::bad_drawable(id))
        }
    }

    /// 在可绘制对象上执行一次绘制，**不带 GC**（ClearArea 这类自带颜色的操作用）。
    pub fn draw_on<F>(&mut self, id: Drawable, draw: F) -> Result<(), XError>
    where
        F: FnOnce(&mut Canvas<'_>),
    {
        let placeholder = GcData::new(0, u32::MAX, self.drawable_geometry(id)?.2);
        self.draw_with_gc(id, &placeholder, draw)
    }

    /// 带 GC 的绘制：裁剪矩形 / clip-mask / tile / stipple 都按 GC 生效。
    ///
    /// 窗口（如果已映射）绘制完立即重新合成屏幕。
    pub fn draw_with_gc<F>(&mut self, id: Drawable, gc: &GcData, draw: F) -> Result<(), XError>
    where
        F: FnOnce(&mut Canvas<'_>),
    {
        // 先确认目标存在（并修复缓冲长度不变量），再借用辅助位图——
        // 借用之后就没法回头做错误检查了
        let (width, height) = {
            let (w, h, _) = self.drawable_geometry_checked(id)?;
            (usize::from(w), usize::from(h))
        };
        let mapped = self.windows.get(&id).map(|window| window.mapped);

        let tile = BorrowedBitmap::take(self, gc.tile, id);
        let stipple = BorrowedBitmap::take(self, gc.stipple, id);
        let mask = BorrowedBitmap::take(self, gc.clip_mask, id);

        let clip = Clip {
            x_origin: gc.clip_x_origin,
            y_origin: gc.clip_y_origin,
            rectangles: &gc.clip_rectangles,
            mask: mask.view(),
        };
        let clip = if clip.rectangles.is_empty() && clip.mask.is_none() {
            None
        } else {
            Some(clip)
        };

        {
            let target = match self.windows.get_mut(&id) {
                Some(window) => &mut window.pixels,
                None => &mut self.pixmaps.get_mut(&id).expect("上面已确认存在").pixels,
            };
            let mut canvas = Canvas::with_gc(
                target.as_mut_slice(),
                width,
                height,
                gc,
                clip,
                tile.view(),
                stipple.view(),
            );
            // 绘制体做 panic 隔离：`BorrowedBitmap` 是**显式归还**（没有 Drop），
            // panic 穿过借用窗口就会让借走的那块位图永久为空（此后每次用它都
            // 越界 panic）。所以这里先记下 panic，归还之后再 resume。
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                draw(&mut canvas);
            }));
            drop(canvas);
            // **按借用的逆序归还**：tile/stipple/clip_mask 允许指向同一个 pixmap
            //（协议只做 `!= 0` 过滤，没有互异校验）。同一 pixmap 被借两次时，
            // 第二次借到的是空 Vec；若按借用同序归还，最后写回的就是那个空 Vec
            // ——等于把它永久抽空（症状：一次合法绘制之后该 pixmap 变成全空，
            // 下一次用它就 panic）。
            mask.restore(self);
            stipple.restore(self);
            tile.restore(self);
            // DAMAGE/重合成标记**无条件**执行：绘制体可能已经写进了一部分像素
            // 才 panic，那部分同样要上屏；标完再把 panic 继续抛给上层（worker
            // 有 catch_unwind，只废这一条命令）。
            self.mark_damage(id);
            if mapped == Some(true) {
                self.mark_compose_dirty();
            }
            if let Err(payload) = outcome {
                std::panic::resume_unwind(payload);
            }
        }
        Ok(())
    }

    /// 取出 GC，并校验它的深度与绘制目标一致。
    pub fn gc_for(&self, gc: Gcontext, drawable: Drawable) -> Result<&GcData, XError> {
        let gc = self.gc(gc)?;
        let (_, _, depth) = self.drawable_geometry(drawable)?;
        if gc.depth != 0 && gc.depth != depth {
            return Err(XError::bad_match());
        }
        Ok(gc)
    }
}

/// 绘制期间从资源表里"借走"的一块辅助位图（tile / stipple / clip-mask）。
///
/// 绘制目标要 `&mut`，辅助位图要 `&`，两者都在 `ServerState` 里，直接借会违反
/// 借用规则。`mem::take` 只移动 `Vec` 的头指针，没有像素拷贝；用完放回原处。
/// 辅助位图恰好就是绘制目标时（自己给自己当遮罩）直接跳过，否则会被取空。
struct BorrowedBitmap {
    id: Option<Pixmap>,
    pixels: Vec<u32>,
    width: usize,
    height: usize,
}

impl BorrowedBitmap {
    fn take(state: &mut ServerState, id: Option<Pixmap>, target: Drawable) -> Self {
        let empty = Self { id: None, pixels: Vec::new(), width: 0, height: 0 };
        let Some(id) = id else { return empty };
        if id == target {
            return empty;
        }
        match state.pixmaps.get_mut(&id) {
            Some(pixmap) => Self {
                id: Some(id),
                pixels: std::mem::take(&mut pixmap.pixels),
                width: pixmap.width as usize,
                height: pixmap.height as usize,
            },
            None => empty,
        }
    }

    fn view(&self) -> Option<Bitmap<'_>> {
        self.id.map(|_| Bitmap {
            pixels: &self.pixels,
            width: self.width,
            height: self.height,
        })
    }

    fn restore(self, state: &mut ServerState) {
        if let Some(id) = self.id {
            if let Some(pixmap) = state.pixmaps.get_mut(&id) {
                pixmap.pixels = self.pixels;
            }
        }
    }
}

struct ParentGeometry {
    depth: u8,
    visual: Visualid,
    background_pixel: u32,
}

/// 递归把 `parent` 的子窗口画进 `screen`。
///
/// `origin_x/origin_y` 是父窗口内容区在屏幕上的左上角；`clip_w/clip_h` 是父窗口
/// 内容区大小，子窗口超出父窗口的部分要被裁掉（X11 的窗口层次裁剪）。
/// `shapes` 是 SHAPE 扩展的窗口形状表：有 bounding shape 的窗口只画形状覆盖
/// 的部分（逐行按区间绘制，见 `draw_shaped_window`）。
#[allow(clippy::too_many_arguments)]
/// NyaTerm: depth 32 (ARGB) 窗口的 alpha 感知 blit——alpha=0 的像素跳过，
/// 其余按不透明覆盖（GTK 透明窗口边距的正确渲染）。
fn blit_clipped_argb(
    screen: &mut [u32],
    screen_w: usize,
    screen_h: usize,
    dst_x: i32,
    dst_y: i32,
    src: &[u32],
    src_w: usize,
    clip_w: i32,
    clip_h: i32,
    origin_x: i32,
    origin_y: i32,
    clip_w_screen: i32,
    clip_h_screen: i32,
) {
    let _ = (clip_w_screen, clip_h_screen);
    for row in 0..clip_h.max(0) as usize {
        let sy = row;
        let dy = dst_y + row as i32;
        if dy < 0 || dy as usize >= screen_h {
            continue;
        }
        for col in 0..clip_w.max(0) as usize {
            let sx = col;
            let dx = dst_x + col as i32;
            if dx < 0 || dx as usize >= screen_w {
                continue;
            }
            let src_index = sy * src_w + sx;
            let Some(pixel) = src.get(src_index) else {
                continue;
            };
            // ARGB：高 8 位是 alpha（与 X11 ARGB32 视觉的掩码布局一致）
            let alpha = (pixel >> 24) & 0xff;
            if alpha == 0 {
                continue;
            }
            let dst_index = dy as usize * screen_w + dx as usize;
            if dst_index < screen.len() {
                screen[dst_index] = *pixel & 0x00ff_ffff;
            }
        }
    }
}

fn compose_children(
    screen: &mut [u32],
    screen_w: usize,
    screen_h: usize,
    origin_x: i32,
    origin_y: i32,
    clip_w: i32,
    clip_h: i32,
    parent: Window,
    windows: &HashMap<Window, WindowData>,
    shapes: &HashMap<Window, WindowShape>,
) {
    let Some(parent) = windows.get(&parent) else {
        return;
    };
    for child_id in &parent.children {
        let Some(child) = windows.get(child_id) else {
            continue;
        };
        if !child.mapped {
            continue;
        }
        // InputOnly 窗口没有可绘制内容，合成时直接跳过（它们只收事件）。
        if child.class == WindowClass::INPUT_ONLY {
            continue;
        }

        let x = origin_x + child.x as i32;
        let y = origin_y + child.y as i32;
        let border = child.border_width as i32;
        let content_x = x + border;
        let content_y = y + border;

        // SHAPE：有 bounding shape 的窗口走逐行绘制路径
        let bounding = shapes
            .get(child_id)
            .and_then(|shape| shape.bounding.as_ref())
            .filter(|rects| !rects.is_empty());
        if let Some(bounding) = bounding {
            let clip_shape = shapes
                .get(child_id)
                .and_then(|shape| shape.clip.as_ref())
                .filter(|rects| !rects.is_empty());
            draw_shaped_window(
                screen,
                screen_w,
                screen_h,
                bounding,
                clip_shape,
                x - border,
                y - border,
                content_x,
                child.width as i32,
                child.height as i32,
                child.border_pixel,
                &child.pixels,
                origin_x,
                origin_y,
                clip_w,
                clip_h,
            );
            compose_children(
                screen,
                screen_w,
                screen_h,
                content_x,
                content_y,
                child.width as i32,
                child.height as i32,
                *child_id,
                windows,
                shapes,
            );
            continue;
        }

        // 边框矩形（含内容，内容会覆盖上去）
        fill_clipped(
            screen,
            screen_w,
            screen_h,
            x,
            y,
            child.width as i32 + border * 2,
            child.height as i32 + border * 2,
            child.border_pixel,
            origin_x,
            origin_y,
            clip_w,
            clip_h,
        );

        // 内容
        let content_clip_w = (child.width as i32).min(clip_w - (content_x - origin_x));
        let content_clip_h = (child.height as i32).min(clip_h - (content_y - origin_y));
        if content_clip_w > 0 && content_clip_h > 0 {
            if child.depth == 32 {
                // NyaTerm: depth 32 (ARGB) 窗口——alpha 感知 blit：alpha=0 的
                // 像素透出下层，其余按不透明处理（GTK 透明边距的正确渲染）。
                blit_clipped_argb(
                    screen,
                    screen_w,
                    screen_h,
                    content_x,
                    content_y,
                    &child.pixels,
                    child.width as usize,
                    content_clip_w,
                    content_clip_h,
                    origin_x,
                    origin_y,
                    clip_w,
                    clip_h,
                );
                compose_children(
                    screen,
                    screen_w,
                    screen_h,
                    content_x,
                    content_y,
                    child.width as i32,
                    child.height as i32,
                    *child_id,
                    windows,
                    shapes,
                );
                continue;
            }
            blit_clipped(
                screen,
                screen_w,
                screen_h,
                content_x,
                content_y,
                &child.pixels,
                child.width as usize,
                content_clip_w,
                content_clip_h,
                origin_x,
                origin_y,
                clip_w,
                clip_h,
            );
        }

        compose_children(
            screen,
            screen_w,
            screen_h,
            content_x,
            content_y,
            child.width as i32,
            child.height as i32,
            *child_id,
            windows,
            shapes,
        );
    }
}

/// 按 bounding shape 逐行画一个窗口（SHAPE 扩展）。
///
/// shape 坐标以窗口外角为原点；每一行每一区间先铺边框色，再把内容区重叠的
/// 部分拷上去（与无形状时的"先框后内容"一致），同时与 clip shape 及父窗口
/// 裁剪求交。
#[allow(clippy::too_many_arguments)]
fn draw_shaped_window(
    screen: &mut [u32],
    screen_w: usize,
    screen_h: usize,
    bounding: &Rects,
    clip_shape: Option<&Rects>,
    outer_x: i32,
    outer_y: i32,
    content_x: i32,
    width: i32,
    height: i32,
    border_pixel: u32,
    pixels: &[u32],
    parent_x: i32,
    parent_y: i32,
    parent_w: i32,
    parent_h: i32,
) {
    let border = content_x - outer_x;
    for row in bounding.rows() {
        let screen_y = outer_y + row.y;
        if screen_y < 0 || screen_y >= screen_h as i32 {
            continue;
        }
        if screen_y < parent_y || screen_y >= parent_y + parent_h {
            continue;
        }
        for &(span_x0, span_x1) in &row.spans {
            // 与父窗口裁剪求交（这一段只受"窗口层次裁剪"约束）
            let sx0 = (outer_x + span_x0).max(parent_x).max(0);
            let sx1 = (outer_x + span_x1)
                .min(parent_x + parent_w)
                .min(screen_w as i32);
            if sx1 <= sx0 {
                continue;
            }

            // 内容：本窗口内容区在 shape 局部坐标里是 [border, border+width)
            let local_x0 = sx0 - outer_x;
            let local_x1 = sx1 - outer_x;
            let in_content_y = row.y >= border && row.y < border + height;
            let content_x0 = local_x0.max(border);
            let content_x1 = local_x1.min(border + width);
            let content_y0 = row.y - border;

            // clip shape（同一局部坐标系）进一步裁内容
            let content_spans: Vec<(i32, i32)> = if in_content_y && content_x1 > content_x0 {
                match clip_shape {
                    Some(clip) => match clip.rows().binary_search_by_key(&row.y, |r| r.y) {
                        Ok(index) => {
                            intersect_spans(&[(content_x0, content_x1)], &clip.rows()[index].spans)
                        }
                        Err(_) => Vec::new(),
                    },
                    None => vec![(content_x0, content_x1)],
                }
            } else {
                Vec::new()
            };

            let row_start = screen_y as usize * screen_w;
            // 边框：整段先铺边框色（有内容的区间马上覆盖上来，与无形状路径一致）
            let fill_start = row_start + sx0 as usize;
            let fill_end = row_start + sx1 as usize;
            if fill_end <= screen.len() && fill_start < fill_end {
                screen[fill_start..fill_end].fill(border_pixel);
            }
            // 内容：按 clip shape 裁过的区间逐段拷贝
            for (cx0, cx1) in content_spans {
                let screen_x0 = outer_x + cx0;
                let screen_x1 = outer_x + cx1;
                if screen_x1 <= screen_x0 || screen_x0 < 0 || screen_x1 > screen_w as i32 {
                    continue;
                }
                let len = (screen_x1 - screen_x0) as usize;
                let src_start = content_y0 as usize * width as usize + (cx0 - border) as usize;
                let dst_start = row_start + screen_x0 as usize;
                if dst_start + len <= screen.len() && src_start + len <= pixels.len() {
                    screen[dst_start..dst_start + len]
                        .copy_from_slice(&pixels[src_start..src_start + len]);
                }
            }
        }
    }
}

/// 两个已排序、已合并的区间列表求交（与 `region.rs` 内部同一套逻辑）。
fn intersect_spans(a: &[(i32, i32)], b: &[(i32, i32)]) -> Vec<(i32, i32)> {
    let mut result = Vec::new();
    for &(ax0, ax1) in a {
        for &(bx0, bx1) in b {
            let (x0, x1) = (ax0.max(bx0), ax1.min(bx1));
            if x1 > x0 {
                result.push((x0, x1));
            }
        }
    }
    result
}

/// 在裁剪矩形内填充实心矩形。
#[allow(clippy::too_many_arguments)]
fn fill_clipped(
    screen: &mut [u32],
    screen_w: usize,
    screen_h: usize,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    pixel: u32,
    clip_x: i32,
    clip_y: i32,
    clip_w: i32,
    clip_h: i32,
) {
    let left = x.max(clip_x).max(0);
    let top = y.max(clip_y).max(0);
    let right = (x + width).min(clip_x + clip_w).min(screen_w as i32);
    let bottom = (y + height).min(clip_y + clip_h).min(screen_h as i32);
    if right <= left || bottom <= top {
        return;
    }
    for row in top..bottom {
        let start = row as usize * screen_w + left as usize;
        let end = row as usize * screen_w + right as usize;
        screen[start..end].fill(pixel);
    }
}

/// 把一块窗口内容拷贝到屏幕（带裁剪；源坐标从 0,0 开始）。
#[allow(clippy::too_many_arguments)]
fn blit_clipped(
    screen: &mut [u32],
    screen_w: usize,
    screen_h: usize,
    x: i32,
    y: i32,
    source: &[u32],
    source_w: usize,
    width: i32,
    height: i32,
    clip_x: i32,
    clip_y: i32,
    clip_w: i32,
    clip_h: i32,
) {
    let left = x.max(clip_x).max(0);
    let top = y.max(clip_y).max(0);
    let right = (x + width).min(clip_x + clip_w).min(screen_w as i32);
    let bottom = (y + height).min(clip_y + clip_h).min(screen_h as i32);
    if right <= left || bottom <= top {
        return;
    }
    for row in top..bottom {
        let src_row = (row - y) as usize;
        let src_start = src_row * source_w + (left - x) as usize;
        let dst_start = row as usize * screen_w + left as usize;
        let len = (right - left) as usize;
        // 防御式切片：源缓冲短于声明尺寸（借还漏配留下的空/短缓冲）时跳过该行，
        // 而不是 panic。这条路径跑在**帧泵任务**上——panic 会让帧泵直接退出，
        // 表现同样是"窗口都在、画面永久不再刷新"，且日志里没有任何线索。
        let (Some(src), Some(dst)) = (
            source.get(src_start..src_start + len),
            screen.get_mut(dst_start..dst_start + len),
        ) else {
            continue;
        };
        dst.copy_from_slice(src);
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：脏区提示覆盖整屏时，**某一行整行都不同**不能让行扫描提前退出。
    ///
    /// 历史 bug：内层循环在"本行 x 范围撑满区域"时执行 `break 'rows`，跳出了
    /// 所有行——脏矩形高度被截断到那一行，下面的变化永远发不出去；而 `sent_seq`
    /// 已经推进，剩余行再也不会补发。症状就是用户看到的"画面在顶部一条条慢慢
    /// 展开、只有动鼠标才继续展开"。
    #[test]
    fn dirty_rect_covers_rows_after_a_full_width_row() {
        let mut state = ServerState::new(Default::default());
        let (w, h) = {
            let root = state.windows.get(&ROOT_WINDOW).expect("根窗口");
            (usize::from(root.width), usize::from(root.height))
        };
        assert!(w > 8 && h > 8, "测试屏幕太小：{w}x{h}");

        // 基准帧全 0；手工写像素，所以不要让帧泵重新合成把像素覆盖掉。
        state.sent_size = (w, h);
        state.sent_frame = vec![0u32; w * h];
        state.sent_seq = 0;
        state.frame_seq = 1;
        state.compose_dirty = false;
        state.damage_hint = Some((0, 0, w as i32, h as i32));

        // 当前帧：第 0 行与第 5 行**整行**都不同，其余行不变。
        {
            let root = state.windows.get_mut(&ROOT_WINDOW).expect("根窗口");
            root.pixels = vec![0u32; w * h];
            for x in 0..w {
                root.pixels[x] = 0x00ff_0000;
                root.pixels[5 * w + x] = 0x0000_ff00;
            }
        }

        let (_, x, y, fw, fh, _) = state.take_frame_if_changed().expect("应当有帧");
        assert_eq!((x, y), (0, 0));
        assert_eq!(fw as usize, w, "整行不同，宽度必须是整个区域");
        assert!(
            fh as usize >= 6,
            "脏矩形必须覆盖到第 5 行（高度 >= 6），实际高度 {fh}"
        );
    }

    // ---- task-17 阶段 B：脏区分带上传 ----

    /// 搭一个「基准帧全 0 + 当前帧手工改像素」的状态（不经过 compose）。
    fn frame_fixture() -> (ServerState, usize, usize) {
        let mut state = ServerState::new(Default::default());
        let (w, h) = {
            let root = state.windows.get(&ROOT_WINDOW).expect("根窗口");
            (usize::from(root.width), usize::from(root.height))
        };
        state.sent_size = (w, h);
        state.sent_frame = vec![0u32; w * h];
        state.sent_seq = 0;
        state.frame_seq = 1;
        state.compose_dirty = false;
        state.damage_hint = Some((0, 0, w as i32, h as i32));
        let root = state.windows.get_mut(&ROOT_WINDOW).expect("根窗口");
        root.pixels = vec![0u32; w * h];
        (state, w, h)
    }

    /// 上下分离的两块小改动 → 两条独立带（而不是一个夹着大片未变区的矩形）。
    #[test]
    fn separate_changes_become_separate_bands() {
        let (mut state, w, _h) = frame_fixture();
        // 第 0 行改 1 个像素；隔 4 行再改 1 个像素
        {
            let root = state.windows.get_mut(&ROOT_WINDOW).expect("根窗口");
            root.pixels[0] = 0x0011_1111;
            root.pixels[5 * w + 3] = 0x0022_2222;
        }
        let (_, bands) = state.take_frame_if_changed_banded().expect("应当有帧");
        assert_eq!(bands.len(), 2, "上下分离的变化应当是两条独立带");
        assert_eq!(
            (bands[0].x, bands[0].y, bands[0].width, bands[0].height),
            (0, 0, 1, 1),
            "第一条带只含第 0 行的那个像素"
        );
        assert_eq!(
            (bands[1].x, bands[1].y, bands[1].width, bands[1].height),
            (3, 5, 1, 1),
            "第二条带只含第 5 行的那个像素"
        );
        // 像素值正确
        assert_eq!(bands[0].pixels, [0x0011_1111]);
        assert_eq!(bands[1].pixels, [0x0022_2222]);
        // 基准帧已就地更新：再取一次没有帧
        assert!(state.take_frame_if_changed_banded().is_none(), "不应重复发送");
    }

    /// 相邻行变化 → 合并为一条带；超过 8 条带退化回单一外接矩形。
    #[test]
    fn bands_merge_and_degrade_to_full_rect() {
        let (mut state, w, _h) = frame_fixture();
        // 相邻两行各改 1 像素（同一列）→ 1 条带
        {
            let root = state.windows.get_mut(&ROOT_WINDOW).expect("根窗口");
            root.pixels[2 * w + 1] = 0x0033_3333;
            root.pixels[3 * w + 1] = 0x0033_3333;
        }
        let (_, bands) = state.take_frame_if_changed_banded().expect("应当有帧");
        assert_eq!(bands.len(), 1, "相邻行必须合并成一条带");
        assert_eq!((bands[0].y, bands[0].height), (2, 2));

        // 9 处上下分散的变化 → 超过带数上限，退化为单一外接矩形
        let (mut state, w, _h) = frame_fixture();
        {
            let root = state.windows.get_mut(&ROOT_WINDOW).expect("根窗口");
            for i in 0..9usize {
                root.pixels[i * 3 * w] = 0x0044_4444;
            }
        }
        let (_, bands) = state.take_frame_if_changed_banded().expect("应当有帧");
        assert_eq!(bands.len(), 1, "超过带数上限必须退化成单一矩形");
        assert_eq!(bands[0].y, 0);
        assert_eq!(usize::from(bands[0].height), 8 * 3 + 1, "退化矩形覆盖全部 9 行");
    }

    /// 回归（task-19，验证者 C2 探针转正）：GC 的 tile 与 stipple 指向**同一个
    /// pixmap**（协议允许，只做 `!= 0` 过滤）时，绘制不得把该 pixmap 抽空。
    ///
    /// 同一 pixmap 被借两次：第二次借到的是空 Vec；若按借用**同序**归还，
    /// 最后写回的就是那个空 Vec → pixmap 永久变空，之后再用它就 panic/静默全空。
    /// 修复 = 按借用**逆序**归还（mask → stipple → tile）。
    #[test]
    fn draw_with_gc_shared_tile_and_stipple_keeps_pixmap() {
        let mut state = ServerState::new(Default::default());
        // 一个 2x2 的 pixmap（兼作 tile 与 stipple）
        let pixmap = 0x0020_0002u32;
        state.pixmaps.insert(
            pixmap,
            PixmapData {
                id: pixmap,
                owner: 0,
                depth: 24,
                width: 2,
                height: 2,
                pixels: vec![0x0011_2233; 4],
            },
        );
        // 目标窗口
        let window = 0x0020_0001u32;
        ok_create_window_and_map(&mut state, window);

        let gc = GcData::new(0x0020_0003, 0, 24);
        let mut gc = gc;
        gc.foreground = 0x00ff_0000;
        gc.tile = Some(pixmap);
        gc.stipple = Some(pixmap);
        gc.fill_style = 0; // Solid：绘制体不读 tile/stipple，但借用照常发生

        state
            .draw_with_gc(window, &gc, |canvas| {
                canvas.fill_rect(0, 0, 4, 4);
            })
            .expect("绘制应当成功");
        assert_eq!(
            state.pixmaps.get(&pixmap).map(|p| p.pixels.len()),
            Some(4),
            "tile==stipple 的合法 GC 不得把 pixmap 抽空（逆序归还）"
        );
    }

    /// 回归（task-19，验证者 D5 探针转正）：绘制体 panic 之后，借走的辅助位图
    /// 必须**先归还**再抛出——panic 穿过借用窗口会把 tile/stipple/clip_mask
    /// 永久抽空（没有 Drop 兜底），此后每次使用都命中空缓冲。
    #[test]
    fn draw_with_gc_panic_still_restores_borrowed_bitmaps() {
        let mut state = ServerState::new(Default::default());
        let pixmap = 0x0020_0002u32;
        state.pixmaps.insert(
            pixmap,
            PixmapData {
                id: pixmap,
                owner: 0,
                depth: 24,
                width: 2,
                height: 2,
                pixels: vec![0x0044_5566; 4],
            },
        );
        let window = 0x0020_0001u32;
        ok_create_window_and_map(&mut state, window);

        let mut gc = GcData::new(0x0020_0003, 0, 24);
        gc.tile = Some(pixmap);

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = state.draw_with_gc(window, &gc, |_canvas| {
                panic!("模拟绘制体 panic");
            });
        }));
        assert!(outcome.is_err(), "panic 应当继续向上传播（worker 侧负责隔离）");
        assert_eq!(
            state.pixmaps.get(&pixmap).map(|p| p.pixels.len()),
            Some(4),
            "panic 后借走的 tile 位图必须已归还（否则永久抽空）"
        );
    }

    /// 测试辅助：建一个映射了的 4x4 窗口（挂根下；client 0 要先注册，
    /// `check_new_id` 需要 `clients` 表里的 resource_id_base）。
    fn ok_create_window_and_map(state: &mut ServerState, id: Window) {
        use x11rb_protocol::protocol::Request;
        use x11rb_protocol::protocol::xproto;
        let (_tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        state.register_client(0, _tx);
        let mut request = |seq: u16, req: Request<'static>| {
            state
                .handle(0, seq, &req)
                .unwrap_or_else(|e| panic!("请求 {seq} 失败：{e:?}"));
        };
        request(
            1,
            Request::CreateWindow(xproto::CreateWindowRequest {
                depth: 0,
                wid: id,
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 4,
                height: 4,
                border_width: 0,
                class: WindowClass::INPUT_OUTPUT,
                visual: 0,
                value_list: std::borrow::Cow::default(),
            }),
        );
        request(2, Request::MapWindow(xproto::MapWindowRequest { window: id }));
    }
}
