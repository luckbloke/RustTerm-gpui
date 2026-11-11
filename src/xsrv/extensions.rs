//! X11 扩展支持：注册表与各扩展请求的服务端语义。
//!
//! 线格式编解码（请求解析、回复/事件序列化）全部复用 `x11rb-protocol`
//! 对应模块的类型，本文件只实现"解析之后做什么"：
//!
//! - **BIG-REQUESTS**：`Enable` 回复最大请求长度；分帧逻辑在 `server.rs`；
//! - **XC-MISC**：`GetXIDRange` / `GetXIDList`，XCB 初始化时靠它拿资源 ID 区间；
//! - **XTEST**：`FakeInput` 直接转成真实输入注入（复用 `input.rs` 的注入入口）；
//! - **MIT-SHM**：`QueryVersion` 报 1.2，但 `Attach` 按协议报错——over SSH 没有
//!   共享内存，让 toolkit 走普通 PutImage 的回退路径是标准做法；
//! - **SHAPE**：窗口 bounding/clip 形状（矩形列表），影响合成与输入命中；
//! - **XFIXES**：Region 类运算（复用 `region.rs`），其余最小化或报错；
//! - **Composite**：`QueryVersion` 成功 + Redirect 系列空操作（很多 toolkit
//!   只要 QueryVersion 成功就不再纠缠）；
//! - **DAMAGE**：create/destroy 记录 + 按协议发 `DamageNotify`；
//! - **RENDER**：版本 0.11 核心子集（Picture/Glyphs/Composite/FillRectangles/
//!   形状），语义在 `render.rs`。
//!
//! opcode 用固定值（参考 Xorg 的常见分配），first_event/error 基址避开核心
//! 协议的事件（2..=34）与错误（1..=17）即可。

use x11rb_protocol::protocol::{bigreq, composite, damage, render, shape, shm, xc_misc, xfixes, xinput, xproto, xtest, xkb};
use x11rb_protocol::x11_utils::{ExtInfoProvider, ExtensionInformation};
use x11rb_protocol::x11_utils::Serialize;

use super::error::{BAD_ACCESS, XError};
use super::region::{Rects, RegionRow};
use super::state::{DamageData, ServerState};
use super::handshake::RESOURCE_ID_MASK;

// ---- 扩展注册表 ----

/// BIG-REQUESTS 的 major opcode。
pub const OPCODE_BIG_REQUESTS: u8 = 135;
/// XC-MISC / XTEST / MIT-SHM / SHAPE / XFIXES / Composite / DAMAGE 的 major opcode。
/// 注意：扩展 opcode 必须落在 128..=255（核心协议占用 1..=127，`Request::parse`
/// 会先按核心分发），SHAPE 不能抄某些参考实现里的 87——那是核心 AllocColorPlanes。
pub const OPCODE_XC_MISC: u8 = 128;
pub const OPCODE_XTEST: u8 = 129;
/// XInput2 扩展（消灭 GTK 的指针轮询）。
pub const OPCODE_XINPUT: u8 = 131;
pub const OPCODE_MIT_SHM: u8 = 131;
pub const OPCODE_SHAPE: u8 = 132;
pub const OPCODE_XFIXES: u8 = 138;
pub const OPCODE_COMPOSITE: u8 = 139;
/// RENDER 的 major opcode（Composite 之后的第一个空闲值；语义在 `render.rs`）。
pub const OPCODE_RENDER: u8 = 140;
pub const OPCODE_DAMAGE: u8 = 143;
/// XKEYBOARD 的 major opcode（DAMAGE 之后的第一个空闲值）。
/// GTK3 的 `use_xkb` 开关靠它宣告成功（gdkkeys-x11.c 核心键表路径存在
/// 首次按键必然 NULL 解引用的死代码 bug，详见 `xkb.rs` 模块注释）。
pub const OPCODE_XKEYBOARD: u8 = 145;

/// 事件基址：只需要避开核心事件段，本服务器内部自洽即可
/// （QueryExtension 回复与 `ExtInfoProvider` 必须用同一组数字）。
pub const FIRST_EVENT_SHAPE: u8 = 90;
pub const FIRST_EVENT_MIT_SHM: u8 = 91;
pub const FIRST_EVENT_XFIXES: u8 = 92;
pub const FIRST_EVENT_DAMAGE: u8 = 94;
/// XKB 的事件基址：XKB 只占 1 个 X 事件码（XkbEventCode=0），子类型在
/// xkbType 字节。GDK 用 QueryExtension 返回的 event_base 作为
/// xkb_event_type——**不能为 0**（会与核心事件冲突）。
pub const FIRST_EVENT_XKB: u8 = 95;

/// 错误基址：XFIXES 的 BadRegion 与 DAMAGE 的 BadDamage。
pub const FIRST_ERROR_XFIXES: u8 = 128;
pub const FIRST_ERROR_DAMAGE: u8 = 129;
/// XKB 的错误基址（只有 BadKeyboard 一种）。
pub const FIRST_ERROR_XKB: u8 = 130;
/// RENDER 的错误基址（PictFormat/Picture/PictOp/GlyphSet/Glyph 五种扩展错误）。
pub const FIRST_ERROR_RENDER: u8 = 150;
/// RENDER 没有事件（注册表约定用 0）。
pub const FIRST_EVENT_RENDER: u8 = 0;
pub const BAD_REGION: u8 = FIRST_ERROR_XFIXES + xfixes::BAD_REGION_ERROR;
pub const BAD_DAMAGE: u8 = FIRST_ERROR_DAMAGE + damage::BAD_DAMAGE_ERROR;

/// 启用 BIG-REQUESTS 后允许的最大请求长度：16 MiB，按 4 字节单位计。
pub const MAX_BIG_REQUEST_UNITS: u32 = 0x0040_0000;

/// 一个扩展在注册表里的条目。
pub struct ExtensionEntry {
    pub name: &'static str,
    pub opcode: u8,
    pub first_event: u8,
    /// 这个扩展占用的事件码个数（`Event::parse` 靠范围判断归属）。
    pub event_count: u8,
    pub first_error: u8,
    /// 错误码个数。
    pub error_count: u8,
}

/// 固定 opcode 的扩展表（顺序不影响行为，只影响日志可读性）。
///
/// NyaTerm: **不宣告 MIT-SHM**——我们无法读客户端（在 SSH 远端机器上）的
/// SysV 共享内存，宣告支持是最坏组合：客户端（WebRender/cairo）选定 SHM
/// 路径后每次 Attach/PutImage 都失败，WebRender 的帧被静默丢弃（页面不更新、
/// 子进程被 X 错误杀死），cairo 虽有回退但要白试一轮。不宣告则客户端从起点
/// 就走普通 XPutImage 路径（OpenSSH 远程 X 转发同理）。
pub const EXTENSIONS: &[ExtensionEntry] = &[
    ExtensionEntry { name: xc_misc::X11_EXTENSION_NAME, opcode: OPCODE_XC_MISC, first_event: 0, event_count: 0, first_error: 0, error_count: 0 },
    ExtensionEntry { name: xtest::X11_EXTENSION_NAME, opcode: OPCODE_XTEST, first_event: 0, event_count: 0, first_error: 0, error_count: 0 },
    // NyaTerm: XI2 二期（2026-11-10）重新宣告——一期撤下时修复了全部探测雷
    // （GetExtensionVersion/XIGetProperty/XIChangeCursor/"Extra reply data"），
    // 二期两轮审计把全部应答改为框架序列化（send_reply，length 按实际字节
    // 回填，天然免"Extra reply data"）+ 每类型 round-trip 零残留测试 +
    // 事件投递全类型 wire 断言（mods 16B、Crossing 72B、Focus 56B、按钮
    // 掩码 bit8 起、抑制矩阵、多客户端）。109+ 测试全绿后由用户实测验收。
    ExtensionEntry { name: xinput::X11_EXTENSION_NAME, opcode: OPCODE_XINPUT, first_event: 0, event_count: 0, first_error: 0, error_count: 0 },
    ExtensionEntry { name: shape::X11_EXTENSION_NAME, opcode: OPCODE_SHAPE, first_event: FIRST_EVENT_SHAPE, event_count: 1, first_error: 0, error_count: 0 },
    ExtensionEntry { name: xfixes::X11_EXTENSION_NAME, opcode: OPCODE_XFIXES, first_event: FIRST_EVENT_XFIXES, event_count: 2, first_error: FIRST_ERROR_XFIXES, error_count: 1 },
    ExtensionEntry { name: composite::X11_EXTENSION_NAME, opcode: OPCODE_COMPOSITE, first_event: 0, event_count: 0, first_error: 0, error_count: 0 },
    ExtensionEntry { name: render::X11_EXTENSION_NAME, opcode: OPCODE_RENDER, first_event: FIRST_EVENT_RENDER, event_count: 0, first_error: FIRST_ERROR_RENDER, error_count: 5 },
    ExtensionEntry { name: damage::X11_EXTENSION_NAME, opcode: OPCODE_DAMAGE, first_event: FIRST_EVENT_DAMAGE, event_count: 1, first_error: FIRST_ERROR_DAMAGE, error_count: 1 },
    ExtensionEntry { name: bigreq::X11_EXTENSION_NAME, opcode: OPCODE_BIG_REQUESTS, first_event: 0, event_count: 0, first_error: 0, error_count: 0 },
    ExtensionEntry { name: xkb::X11_EXTENSION_NAME, opcode: OPCODE_XKEYBOARD, first_event: FIRST_EVENT_XKB, event_count: 1, first_error: FIRST_ERROR_XKB, error_count: 1 },
];

/// `ExtInfoProvider` 实现：`Request::parse` / `Event::parse` 靠它识别扩展
/// 请求与扩展事件（server.rs 里的连接循环持有它）。
#[derive(Default)]
pub struct ExtTable;

impl ExtTable {
    fn by_major(&self, opcode: u8) -> Option<&'static ExtensionEntry> {
        EXTENSIONS.iter().find(|entry| entry.opcode == opcode)
    }
}

impl ExtInfoProvider for ExtTable {
    fn get_from_major_opcode(&self, major_opcode: u8) -> Option<(&str, ExtensionInformation)> {
        self.by_major(major_opcode).map(|entry| {
            (
                entry.name,
                ExtensionInformation {
                    major_opcode: entry.opcode,
                    first_event: entry.first_event,
                    first_error: entry.first_error,
                },
            )
        })
    }

    fn get_from_event_code(&self, event_code: u8) -> Option<(&str, ExtensionInformation)> {
        EXTENSIONS
            .iter()
            .find(|entry| {
                entry.event_count > 0
                    && event_code >= entry.first_event
                    && event_code < entry.first_event + entry.event_count
            })
            .map(|entry| {
                (
                    entry.name,
                    ExtensionInformation {
                        major_opcode: entry.opcode,
                        first_event: entry.first_event,
                        first_error: entry.first_error,
                    },
                )
            })
    }

    fn get_from_error_code(&self, error_code: u8) -> Option<(&str, ExtensionInformation)> {
        EXTENSIONS
            .iter()
            .find(|entry| {
                entry.error_count > 0
                    && error_code >= entry.first_error
                    && error_code < entry.first_error + entry.error_count
            })
            .map(|entry| {
                (
                    entry.name,
                    ExtensionInformation {
                        major_opcode: entry.opcode,
                        first_event: entry.first_event,
                        first_error: entry.first_error,
                    },
                )
            })
    }
}

/// `QueryExtension` 的名字查找：返回 `(present, major_opcode, first_event, first_error)`。
pub fn query_extension(name: &str) -> Option<(u8, u8, u8)> {
    // 输入协议开关（config.rs）：Core 模式不宣告 XInputExtension——客户端
    // （GDK/firefox）据此自动退回核心 Motion/Button/Key 路径（兼容排障）。
    // 只影响新连接的握手查表；其余扩展不受影响。
    if name.eq_ignore_ascii_case(xinput::X11_EXTENSION_NAME)
        && !crate::config::xinput_enabled()
    {
        return None;
    }
    EXTENSIONS
        .iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(name))
        .map(|entry| (entry.opcode, entry.first_event, entry.first_error))
}

/// `ListExtensions` 的名字列表（保持注册表顺序；Core 输入协议下不列
/// XInputExtension——与 `query_extension` 的可见性保持一致）。
pub fn extension_names() -> Vec<Vec<u8>> {
    let xinput_hidden = !crate::config::xinput_enabled();
    EXTENSIONS
        .iter()
        .filter(|entry| {
            !(xinput_hidden && entry.name == xinput::X11_EXTENSION_NAME)
        })
        .map(|entry| entry.name.as_bytes().to_vec())
        .collect()
}

// ---- XC-MISC ----

impl ServerState {
    /// XCB 初始化时靠 `GetXIDRange` 拿到自己的资源 ID 区间。
    pub fn xc_misc_get_xid_range(&mut self, client: u32, sequence: u16) -> Result<(), XError> {
        let base = match self.clients.get(&client) {
            Some(handle) => handle.resource_id_base,
            None => return Err(XError::bad_access()),
        };
        // 区间大小固定为 2^21；已占用的部分从 count 里扣掉，
        // 客户端拿 start_id..start_id+count 自己挑空位。
        let total = RESOURCE_ID_MASK + 1;
        let used = self.used_ids_in_range(base) as u32;
        self.send_reply(
            client,
            &xc_misc::GetXIDRangeReply {
                sequence,
                length: 0,
                start_id: base,
                count: total - used.min(total),
            },
        );
        Ok(())
    }

    /// 一次发一批空闲资源 ID（XCB 的 `xcb_generate_id` 批量模式用）。
    pub fn xc_misc_get_xid_list(
        &mut self,
        client: u32,
        sequence: u16,
        count: u32,
    ) -> Result<(), XError> {
        let base = match self.clients.get(&client) {
            Some(handle) => handle.resource_id_base,
            None => return Err(XError::bad_access()),
        };
        let mut ids = Vec::new();
        for offset in 0..=RESOURCE_ID_MASK {
            if ids.len() >= count as usize {
                break;
            }
            let id = base + offset;
            if !self.resource_exists(id) {
                ids.push(id);
            }
        }
        let length = ids.len() as u32;
        self.send_reply(
            client,
            &xc_misc::GetXIDListReply {
                sequence,
                length,
                ids,
            },
        );
        Ok(())
    }

    /// 统计客户端区间内已被占用的资源 ID 数。
    fn used_ids_in_range(&self, base: u32) -> usize {
        let in_range = |id: &u32| id & !RESOURCE_ID_MASK == base;
        self.windows.keys().filter(|id| in_range(id)).count()
            + self.pixmaps.keys().filter(|id| in_range(id)).count()
            + self.gcs.keys().filter(|id| in_range(id)).count()
            + self.colormaps.keys().filter(|id| in_range(id)).count()
            + self.fonts.keys().filter(|id| in_range(id)).count()
            + self.cursors.keys().filter(|id| in_range(id)).count()
            + self.pictures.keys().filter(|id| in_range(id)).count()
            + self.glyph_sets.keys().filter(|id| in_range(id)).count()
    }
}

// ---- XTEST ----

impl ServerState {
    pub fn xtest_get_version(&mut self, client: u32, sequence: u16) -> Result<(), XError> {
        self.send_reply(
            client,
            &xtest::GetVersionReply {
                major_version: 2,
                sequence,
                length: 0,
                minor_version: 2,
            },
        );
        Ok(())
    }

    /// `FakeInput`：XTEST 的 fake 事件按 **keycode / 按键号** 走（不是 keysym），
    /// 直接复用前端注入用的同一批入口（见 `input.rs`）。
    pub fn xtest_fake_input(&mut self, request: &xtest::FakeInputRequest) -> Result<(), XError> {
        match request.type_ {
            // KeyPress / KeyRelease：detail 就是 keycode
            2 => self.key_event(request.detail, true, false),
            3 => self.key_event(request.detail, false, false),
            // ButtonPress / ButtonRelease：4/5 是滚轮方向，其余是实体键
            4 => self.pointer_scroll(true),
            5 => self.pointer_scroll(false),
            6 => {
                // MotionNotify：root_x/root_y 是绝对坐标（-1 表示相对移动，不支持）
                if request.root_x >= 0 && request.root_y >= 0 {
                    self.pointer_motion(request.root_x, request.root_y);
                }
            }
            // Enter/Leave（7/8）等：没有对应的注入语义，接受但不产生事件
            _ => {}
        }
        Ok(())
    }
}

// ---- MIT-SHM ----

impl ServerState {
    pub fn shm_query_version(&mut self, client: u32, sequence: u16) -> Result<(), XError> {
        self.send_reply(
            client,
            &shm::QueryVersionReply {
                // shared_pixmaps = false：没有共享 pixmap，图像走普通 PutImage
                shared_pixmaps: false,
                sequence,
                length: 0,
                major_version: 1,
                minor_version: 2,
                uid: 0,
                gid: 0,
                pixmap_format: u8::from(xproto::ImageFormat::Z_PIXMAP),
            },
        );
        Ok(())
    }

    /// 没有 shared memory：按协议报错让客户端走普通 PutImage 回退路径
    /// （GTK/Qt 的 XShmQueryVersion → XShmAttach 失败 → 回退，是标准流程）。
    pub fn shm_attach(&mut self, request: &shm::AttachRequest) -> Result<(), XError> {
        Err(XError::new(BAD_ACCESS, request.shmid))
    }

    /// 从未 Attach 成功过，Detach 只是礼貌性的存在。
    pub fn shm_detach(&mut self, _request: &shm::DetachRequest) -> Result<(), XError> {
        Ok(())
    }
}

// ---- SHAPE ----

impl ServerState {
    pub fn shape_query_version(&mut self, client: u32, sequence: u16) -> Result<(), XError> {
        self.send_reply(
            client,
            &shape::QueryVersionReply {
                sequence,
                length: 0,
                major_version: 1,
                minor_version: 1,
            },
        );
        Ok(())
    }

    /// 当前 kind 的形状；没有记录时按规范返回"完整窗口矩形"的隐式形状。
    fn shape_region(&self, window: u32, kind: shape::SK) -> Rects {
        let stored = self.shapes.get(&window).and_then(|shape| match kind {
            shape::SK::BOUNDING => shape.bounding.as_ref(),
            shape::SK::CLIP => shape.clip.as_ref(),
            shape::SK::INPUT => shape.input.as_ref(),
            _ => None,
        });
        match stored {
            Some(rects) => rects.clone(),
            None => self.implicit_shape(window),
        }
    }

    /// 未设置形状时的隐式区域：窗口内容区的全尺寸矩形（窗口局部坐标）。
    fn implicit_shape(&self, window: u32) -> Rects {
        match self.windows.get(&window) {
            Some(data) => Rects::from_rectangle(xproto::Rectangle {
                x: 0,
                y: 0,
                width: data.width,
                height: data.height,
            }),
            None => Rects::default(),
        }
    }

    fn set_shape_kind(&mut self, window: u32, kind: shape::SK, rects: Rects) {
        let entry = self.shapes.entry(window).or_default();
        match kind {
            shape::SK::BOUNDING => entry.bounding = Some(rects),
            shape::SK::CLIP => entry.clip = Some(rects),
            shape::SK::INPUT => entry.input = Some(rects),
            _ => {}
        }
    }

    pub fn shape_rectangles(
        &mut self,
        request: &shape::RectanglesRequest<'_>,
    ) -> Result<(), XError> {
        self.window(request.destination_window)?;
        let mut rects = Rects::from_rectangles(&request.rectangles);
        rects.translate(request.x_offset, request.y_offset);
        if u8::from(request.operation) == u8::from(shape::SO::SET) && rects.is_empty() {
            // SET + 空矩形 = 取消形状（恢复矩形窗口）；本来就没形状就无事可做
            if let Some(entry) = self.shapes.get_mut(&request.destination_window) {
                match request.destination_kind {
                    shape::SK::BOUNDING => entry.bounding = None,
                    shape::SK::CLIP => entry.clip = None,
                    shape::SK::INPUT => entry.input = None,
                    _ => {}
                }
            }
        } else {
            let current = self.shape_region(request.destination_window, request.destination_kind);
            let result = match request.operation {
                shape::SO::SET => rects,
                shape::SO::UNION => current.union(&rects),
                shape::SO::INTERSECT => current.intersect(&rects),
                shape::SO::SUBTRACT => current.subtract(&rects),
                shape::SO::INVERT => rects.invert(&current),
                _ => return Err(XError::bad_value(u8::from(request.operation) as u32)),
            };
            self.set_shape_kind(request.destination_window, request.destination_kind, result);
        }
        self.mark_compose_dirty();
        self.shape_notify(request.destination_window, request.destination_kind);
        Ok(())
    }

    /// `ShapeMask`：用 1 位深度位图作为形状掩码（xeyes 等自绘形状窗口的
    /// 标准路径）。之前未实现（直接 BadMatch），导致这类客户端直接报错退出。
    ///
    /// 源位图为 None（0）时按规范取消形状掩码。
    pub fn shape_mask(&mut self, request: &shape::MaskRequest) -> Result<(), XError> {
        self.window(request.destination_window)?;
        let kind = request.destination_kind;
        // SHAPE 有三种 kind：Bounding / Clip / **Input**。GTK 会用
        // `ShapeMask(ShapeInput)` 给窗口挖掉阴影/圆角处的点击区，只认前两种
        // 会让客户端拿到 BadValue 并直接 abort。
        if !matches!(kind, shape::SK::BOUNDING | shape::SK::CLIP | shape::SK::INPUT) {
            return Err(XError::bad_value(u8::from(kind) as u32));
        }
        if !matches!(
            request.operation,
            shape::SO::SET
                | shape::SO::UNION
                | shape::SO::INTERSECT
                | shape::SO::SUBTRACT
                | shape::SO::INVERT
        ) {
            return Err(XError::bad_value(u8::from(request.operation) as u32));
        }

        // 源位图为 None：按规范取消掩码（SET 空区域语义）。
        if request.source_bitmap == 0 {
            if let Some(entry) = self.shapes.get_mut(&request.destination_window) {
                match kind {
                    shape::SK::BOUNDING => entry.bounding = None,
                    shape::SK::CLIP => entry.clip = None,
                    shape::SK::INPUT => entry.input = None,
                    _ => {}
                }
            }
            self.mark_compose_dirty();
            return Ok(());
        }

        let pixmap = self
            .pixmaps
            .get(&request.source_bitmap)
            .ok_or_else(|| XError::bad_pixmap(u32::from(request.source_bitmap)))?;
        if pixmap.depth != 1 {
            // 规范要求掩码位图深度为 1。
            return Err(XError::bad_match());
        }

        // 置位像素 → 行扫描 → 区域。1 位深度的像素值非 0 即"保留"。
        let mut rows = Vec::new();
        for y in 0..i32::from(pixmap.height) {
            let row_base = y as usize * pixmap.width as usize;
            let mut spans = Vec::new();
            let mut span_start: Option<i32> = None;
            for x in 0..i32::from(pixmap.width) {
                let index = row_base + x as usize;
                if pixmap.pixels.get(index).copied().unwrap_or(0) != 0 {
                    if span_start.is_none() {
                        span_start = Some(x);
                    }
                } else if let Some(x0) = span_start.take() {
                    spans.push((x0, x));
                }
            }
            if let Some(x0) = span_start.take() {
                spans.push((x0, i32::from(pixmap.width)));
            }
            if !spans.is_empty() {
                rows.push(RegionRow { y, spans });
            }
        }

        let mut mask = Rects::from_row_spans(rows);
        mask.translate(request.x_offset, request.y_offset);

        let current = self.shape_region(request.destination_window, kind);
        let result = match request.operation {
            shape::SO::SET => mask,
            shape::SO::UNION => current.union(&mask),
            shape::SO::INTERSECT => current.intersect(&mask),
            shape::SO::SUBTRACT => current.subtract(&mask),
            shape::SO::INVERT => mask.invert(&current),
            _ => return Err(XError::bad_value(u8::from(request.operation) as u32)),
        };
        self.set_shape_kind(request.destination_window, kind, result);
        self.mark_compose_dirty();
        self.shape_notify(request.destination_window, kind);
        Ok(())
    }

    pub fn shape_combine(&mut self, request: &shape::CombineRequest) -> Result<(), XError> {
        self.window(request.destination_window)?;
        self.window(request.source_window)?;
        let mut source =
            self.shape_region(request.source_window, request.source_kind);
        source.translate(request.x_offset, request.y_offset);
        let dest = self.shape_region(request.destination_window, request.destination_kind);
        let result = match request.operation {
            shape::SO::SET => source,
            shape::SO::UNION => dest.union(&source),
            shape::SO::INTERSECT => dest.intersect(&source),
            shape::SO::SUBTRACT => dest.subtract(&source),
            shape::SO::INVERT => source.invert(&dest),
            _ => return Err(XError::bad_value(u8::from(request.operation) as u32)),
        };
        self.set_shape_kind(request.destination_window, request.destination_kind, result);
        self.mark_compose_dirty();
        self.shape_notify(request.destination_window, request.destination_kind);
        Ok(())
    }

    pub fn shape_offset(&mut self, request: &shape::OffsetRequest) -> Result<(), XError> {
        self.window(request.destination_window)?;
        let kind = request.destination_kind;
        let current = self.shape_region(request.destination_window, kind);
        let mut moved = current;
        moved.translate(request.x_offset, request.y_offset);
        self.set_shape_kind(request.destination_window, kind, moved);
        self.mark_compose_dirty();
        self.shape_notify(request.destination_window, kind);
        Ok(())
    }

    pub fn shape_query_extents(
        &mut self,
        client: u32,
        sequence: u16,
        window: u32,
    ) -> Result<(), XError> {
        self.window(window)?;
        let shape = self.shapes.get(&window);
        let bounding = shape.and_then(|s| s.bounding.as_ref());
        let clip = shape.and_then(|s| s.clip.as_ref());
        let (bounding_shaped, clip_shaped) = (bounding.is_some(), clip.is_some());
        let empty = xproto::Rectangle { x: 0, y: 0, width: 0, height: 0 };
        let bounding_extents = bounding.and_then(|rects| rects.extents()).unwrap_or(empty);
        let clip_extents = clip.and_then(|rects| rects.extents()).unwrap_or(empty);
        self.send_reply(
            client,
            &shape::QueryExtentsReply {
                sequence,
                length: 0,
                bounding_shaped,
                clip_shaped,
                bounding_shape_extents_x: bounding_extents.x,
                bounding_shape_extents_y: bounding_extents.y,
                bounding_shape_extents_width: bounding_extents.width,
                bounding_shape_extents_height: bounding_extents.height,
                clip_shape_extents_x: clip_extents.x,
                clip_shape_extents_y: clip_extents.y,
                clip_shape_extents_width: clip_extents.width,
                clip_shape_extents_height: clip_extents.height,
            },
        );
        Ok(())
    }

    /// ShapeSelectInput：登记/撤销形状变化事件选择（SHAPE 协议要求变化时
    /// 给选择者发 ShapeNotify；GDK 的 shape 支持靠它避免"查询一次然后过期"）。
    pub fn shape_select_input(
        &mut self,
        client: u32,
        request: &shape::SelectInputRequest,
    ) -> Result<(), XError> {
        self.window(request.destination_window)?;
        let selections = self
            .shape_event_selections
            .entry(request.destination_window)
            .or_default();
        if request.enable {
            selections.insert(client);
        } else {
            selections.remove(&client);
        }
        Ok(())
    }

    pub fn shape_input_selected(&mut self, client: u32, sequence: u16) -> Result<(), XError> {
        // 协议语义：本客户端是否在任何窗口上选择了形状事件
        let enabled = self
            .shape_event_selections
            .values()
            .any(|set| set.contains(&client));
        self.send_reply(
            client,
            &shape::InputSelectedReply {
                enabled,
                sequence,
                length: 0,
            },
        );
        Ok(())
    }

    /// 形状变化后向**该窗口上选择了 ShapeNotifyMask 的客户端**发 ShapeNotify。
    /// 没有选择者时静默（协议语义）。
    ///
    /// `shaped` 语义 = 该 kind 是否被**显式设置**（与 QueryExtentsReply 的
    /// `bounding_shaped` 一致）：取消形状（SET 空）后 shaped=0，extents 回
    /// 隐式全窗口矩形——真实 X.org 服务器同款行为。
    pub fn shape_notify(&mut self, window: u32, kind: shape::SK) {
        let Some(selectors) = self.shape_event_selections.get(&window) else {
            return;
        };
        let clients: Vec<u32> = selectors.iter().copied().collect();
        let stored = self
            .shapes
            .get(&window)
            .and_then(|shape| match kind {
                shape::SK::BOUNDING => shape.bounding.as_ref(),
                shape::SK::CLIP => shape.clip.as_ref(),
                shape::SK::INPUT => shape.input.as_ref(),
                _ => None,
            });
        let shaped = stored.is_some();
        let empty = xproto::Rectangle { x: 0, y: 0, width: 0, height: 0 };
        let extents = match stored {
            Some(rects) => rects.extents().unwrap_or(empty),
            None => self.implicit_shape(window).extents().unwrap_or(empty),
        };
        for client in clients {
            let frame = Self::shape_notify_frame_for(window, kind, shaped, extents);
            self.send_client_event(client, frame);
        }
    }

    pub fn shape_get_rectangles(
        &mut self,
        client: u32,
        sequence: u16,
        request: &shape::GetRectanglesRequest,
    ) -> Result<(), XError> {
        self.window(request.window)?;
        let rects = self.shape_region(request.window, request.source_kind);
        let rectangles: Vec<xproto::Rectangle> = rects
            .rows()
            .iter()
            .flat_map(|row| {
                row.spans.iter().map(move |&(x0, x1)| xproto::Rectangle {
                    x: x0 as i16,
                    y: row.y as i16,
                    width: (x1 - x0) as u16,
                    height: 1,
                })
            })
            .collect();
        // ordering 必须诚实反映矩形列表的真实排布：我们是 `rects.rows()` 逐行展开
        // ——每行一条 span（height=1）、按 y 递增、行内按 x 递增，同 y 的矩形同高
        // 且按 x 有序，**这正是 YXBanded 的定义**（band = 同 y 同高的一组，按 x 排序）。
        // 之前写 UNSORTED 会让 GDK 打印
        //   `Gdk-WARNING: non YXBanded shape masks not supported`
        // 然后按 banded 语义解释这份数据（2026-11-11 实测：该告警出现在 firefox
        // 崩溃前毫秒级、且 X11 标签同时"很卡"，怀疑与反复处理 shape/区域有关）。
        // 注意 ClipOrdering 的值：Unsorted=0 / YSorted=1 / YXSorted=2 / YXBanded=3。
        let ordering = xproto::ClipOrdering::YX_BANDED;
        debug_assert!(
            {
                // 不变式：y 非递减；同 y 的矩形 height 一致且 x 递增
                let mut ok = true;
                let mut prev_y: Option<i16> = None;
                let mut prev_x: Option<i16> = None;
                let mut band_h: Option<u16> = None;
                for r in &rectangles {
                    match prev_y {
                        Some(y) if r.y == y => {
                            if band_h != Some(r.height) || prev_x.is_some_and(|px| r.x <= px) {
                                ok = false;
                                break;
                            }
                        }
                        Some(y) if r.y < y => {
                            ok = false;
                            break;
                        }
                        _ => band_h = Some(r.height),
                    }
                    prev_y = Some(r.y);
                    prev_x = Some(r.x);
                }
                ok
            },
            "shape 矩形列表必须满足 YXBanded 不变式（否则 ordering 字段就是谎报）"
        );
        let length = rectangles.len() as u32 * 2;
        self.send_reply(
            client,
            &shape::GetRectanglesReply {
                ordering,
                sequence,
                length,
                rectangles,
            },
        );
        Ok(())
    }

    /// 编码一条 ShapeNotify 事件帧（事件码 = FIRST_EVENT_SHAPE + 0，
    /// 发送前由 `send_client_event` 盖写接收方的序列号）。
    pub(crate) fn shape_notify_frame_for(
        window: u32,
        kind: shape::SK,
        shaped: bool,
        extents: xproto::Rectangle,
    ) -> Vec<u8> {
        shape::NotifyEvent {
            response_type: FIRST_EVENT_SHAPE + shape::NOTIFY_EVENT,
            shape_kind: kind,
            sequence: 0,
            affected_window: window,
            extents_x: extents.x,
            extents_y: extents.y,
            extents_width: extents.width,
            extents_height: extents.height,
            server_time: 0,
            shaped,
        }
        .serialize()
        .to_vec()
    }
}

// ---- XFIXES ----

impl ServerState {
    pub fn xfixes_query_version(
        &mut self,
        client: u32,
        sequence: u16,
        request: &xfixes::QueryVersionRequest,
    ) -> Result<(), XError> {
        // 回客户端要的版本（不超过我们支持的 6.0）
        let (major, minor) = if request.client_major_version >= 6 {
            (6, 0)
        } else {
            (
                request.client_major_version as u32,
                request.client_minor_version as u32,
            )
        };
        self.send_reply(
            client,
            &xfixes::QueryVersionReply {
                sequence,
                length: 0,
                major_version: major,
                minor_version: minor,
            },
        );
        Ok(())
    }

    fn region(&self, id: u32) -> Result<&Rects, XError> {
        self.regions
            .get(&id)
            .ok_or(XError::new(BAD_REGION, id))
    }

    /// 写入一个 region：已存在就覆盖，新 ID 要过 `check_new_id`。
    fn store_region(&mut self, client: u32, id: u32, rects: Rects) -> Result<(), XError> {
        if !self.regions.contains_key(&id) {
            self.check_new_id(client, id)?;
        }
        self.regions.insert(id, rects);
        Ok(())
    }

    pub fn xfixes_create_region(
        &mut self,
        client: u32,
        request: &xfixes::CreateRegionRequest<'_>,
    ) -> Result<(), XError> {
        self.store_region(client, request.region, Rects::from_rectangles(&request.rectangles))
    }

    pub fn xfixes_set_region(
        &mut self,
        _client: u32,
        request: &xfixes::SetRegionRequest<'_>,
    ) -> Result<(), XError> {
        // SetRegion 的目标必须已存在（与 CreateRegion 的区别）
        if !self.regions.contains_key(&request.region) {
            return Err(XError::new(BAD_REGION, request.region));
        }
        self.regions
            .insert(request.region, Rects::from_rectangles(&request.rectangles));
        Ok(())
    }

    pub fn xfixes_destroy_region(&mut self, request: &xfixes::DestroyRegionRequest) -> Result<(), XError> {
        if self.regions.remove(&request.region).is_none() {
            return Err(XError::new(BAD_REGION, request.region));
        }
        Ok(())
    }

    pub fn xfixes_copy_region(&mut self, client: u32, request: &xfixes::CopyRegionRequest) -> Result<(), XError> {
        let source = self.region(request.source)?.clone();
        self.store_region(client, request.destination, source)
    }

    pub fn xfixes_union_region(&mut self, client: u32, request: &xfixes::UnionRegionRequest) -> Result<(), XError> {
        let a = self.region(request.source1)?.clone();
        let b = self.region(request.source2)?.clone();
        self.store_region(client, request.destination, a.union(&b))
    }

    pub fn xfixes_intersect_region(
        &mut self,
        client: u32,
        request: &xfixes::IntersectRegionRequest,
    ) -> Result<(), XError> {
        let a = self.region(request.source1)?.clone();
        let b = self.region(request.source2)?.clone();
        self.store_region(client, request.destination, a.intersect(&b))
    }

    pub fn xfixes_subtract_region(
        &mut self,
        client: u32,
        request: &xfixes::SubtractRegionRequest,
    ) -> Result<(), XError> {
        let a = self.region(request.source1)?.clone();
        let b = self.region(request.source2)?.clone();
        self.store_region(client, request.destination, a.subtract(&b))
    }

    pub fn xfixes_invert_region(
        &mut self,
        client: u32,
        request: &xfixes::InvertRegionRequest,
    ) -> Result<(), XError> {
        let source = self.region(request.source)?.clone();
        let bounds = Rects::from_rectangle(request.bounds);
        self.store_region(client, request.destination, source.invert(&bounds))
    }

    pub fn xfixes_translate_region(&mut self, request: &xfixes::TranslateRegionRequest) -> Result<(), XError> {
        if !self.regions.contains_key(&request.region) {
            return Err(XError::new(BAD_REGION, request.region));
        }
        self.regions
            .get_mut(&request.region)
            .expect("上面刚确认存在")
            .translate(request.dx, request.dy);
        Ok(())
    }

    pub fn xfixes_region_extents(
        &mut self,
        client: u32,
        request: &xfixes::RegionExtentsRequest,
    ) -> Result<(), XError> {
        let extents = self
            .region(request.source)?
            .extents()
            .map(Rects::from_rectangle)
            .unwrap_or_default();
        self.store_region(client, request.destination, extents)
    }

    pub fn xfixes_fetch_region(
        &mut self,
        client: u32,
        sequence: u16,
        request: &xfixes::FetchRegionRequest,
    ) -> Result<(), XError> {
        let rects = self.region(request.region)?.clone();
        let extents = rects.extents().unwrap_or(xproto::Rectangle {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        });
        let rectangles: Vec<xproto::Rectangle> = rects
            .rows()
            .iter()
            .flat_map(|row| {
                row.spans.iter().map(move |&(x0, x1)| xproto::Rectangle {
                    x: x0 as i16,
                    y: row.y as i16,
                    width: (x1 - x0) as u16,
                    height: 1,
                })
            })
            .collect();
        // FetchRegionReply 没有 length 字段：serialize 内部按 rectangles 数量自算
        self.send_reply(
            client,
            &xfixes::FetchRegionReply {
                sequence,
                extents,
                rectangles,
            },
        );
        Ok(())
    }

    /// XFIXES 的光标/保存集部分：能"无副作用地成功"的就成功（很多 toolkit
    /// 只是例行调用），真正需要画面反馈的（GetCursorImage 等）回 BadMatch。
    pub fn xfixes_change_save_set(
        &mut self,
        request: &xfixes::ChangeSaveSetRequest,
    ) -> Result<(), XError> {
        self.window(request.window).map(|_| ())
    }

    pub fn xfixes_hide_or_show_cursor(&mut self) -> Result<(), XError> {
        // 画面上本来就不渲染光标，隐藏/显示都是空操作
        Ok(())
    }
}

// ---- Composite ----

impl ServerState {
    pub fn composite_query_version(
        &mut self,
        client: u32,
        sequence: u16,
    ) -> Result<(), XError> {
        self.send_reply(
            client,
            &composite::QueryVersionReply {
                sequence,
                length: 0,
                major_version: 0,
                minor_version: 4,
            },
        );
        Ok(())
    }

    /// RedirectWindow 系列：不做重定向合成，但按协议回成功——toolkit 的
    /// error 检查点在 QueryVersion，Redirect 失败才会真的放弃。
    pub fn composite_redirect(&mut self, request: &composite::RedirectWindowRequest) -> Result<(), XError> {
        self.window(request.window).map(|_| ())
    }

    pub fn composite_unredirect(&mut self, request: &composite::UnredirectWindowRequest) -> Result<(), XError> {
        self.window(request.window).map(|_| ())
    }

    /// `CreateRegionFromBorderClip`：窗口可见区域（内容矩形）转成 region。
    pub fn composite_create_region_from_border_clip(
        &mut self,
        client: u32,
        request: &composite::CreateRegionFromBorderClipRequest,
    ) -> Result<(), XError> {
        let window = self.window(request.window)?;
        let rects = Rects::from_rectangle(xproto::Rectangle {
            x: 0,
            y: 0,
            width: window.width,
            height: window.height,
        });
        self.store_region(client, request.region, rects)
    }
}

// ---- DAMAGE ----

impl ServerState {
    pub fn damage_query_version(&mut self, client: u32, sequence: u16) -> Result<(), XError> {
        self.send_reply(
            client,
            &damage::QueryVersionReply {
                sequence,
                length: 0,
                major_version: 1,
                minor_version: 1,
            },
        );
        Ok(())
    }

    pub fn damage_create(&mut self, client: u32, request: &damage::CreateRequest) -> Result<(), XError> {
        self.check_new_id(client, request.damage)?;
        let (width, height, _) = self.drawable_geometry(request.drawable)?;
        let (origin_x, origin_y) = match self.windows.get(&request.drawable) {
            Some(window) => (window.x as i32, window.y as i32),
            None => (0, 0),
        };
        let data = DamageData {
            drawable: request.drawable,
            level: u8::from(request.level),
            owner: client,
            dirty: false,
        };
        self.damages.insert(request.damage, data.clone());
        // 初始报告：damage 为空，按协议发一个空矩形通知（GTK 靠它同步起点）
        let area = xproto::Rectangle { x: 0, y: 0, width, height };
        let geometry = xproto::Rectangle {
            x: origin_x as i16,
            y: origin_y as i16,
            width,
            height,
        };
        let frame = damage_notify_frame_for(request.damage, u8::from(request.level), request.drawable, area, geometry);
        self.send_client_event(client, frame);
        Ok(())
    }

    pub fn damage_destroy(&mut self, request: &damage::DestroyRequest) -> Result<(), XError> {
        if self.damages.remove(&request.damage).is_none() {
            return Err(XError::new(BAD_DAMAGE, request.damage));
        }
        Ok(())
    }

    /// Subtract：客户端已处理完上报的区域，服务端把累计的脏区清掉。
    pub fn damage_subtract(&mut self, request: &damage::SubtractRequest) -> Result<(), XError> {
        let Some(data) = self.damages.get_mut(&request.damage) else {
            return Err(XError::new(BAD_DAMAGE, request.damage));
        };
        data.dirty = false;
        Ok(())
    }

    pub fn damage_add(&mut self, request: &damage::AddRequest) -> Result<(), XError> {
        // Add：手工往 damage 里加区域。我们没有精确的区域累计，标记脏即可
        self.drawable_geometry(request.drawable)?;
        if let Some(data) = self.damages.get_mut(&request.region) {
            data.dirty = true;
        } else {
            return Err(XError::new(BAD_DAMAGE, request.region));
        }
        Ok(())
    }
}

/// DamageNotify 事件里 `geometry` 的窗口位置直接取窗口的 x/y（pixmap 为 0）。
/// 编码一条 DamageNotify 事件帧（紧凑序列化，发送前由 `send_client_event`
/// 补齐 32 字节并盖写接收方的序列号）。
pub(crate) fn damage_notify_frame_for(
    damage: u32,
    level: u8,
    drawable: u32,
    area: xproto::Rectangle,
    geometry: xproto::Rectangle,
) -> Vec<u8> {
    damage::NotifyEvent {
        response_type: FIRST_EVENT_DAMAGE + damage::NOTIFY_EVENT,
        level: damage::ReportLevel::from(level),
        sequence: 0,
        drawable,
        damage,
        timestamp: 0,
        area,
        geometry,
    }
    .serialize()
    .to_vec()
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;
    use x11rb_protocol::protocol::Request;
    use x11rb_protocol::x11_utils::TryParse;

    /// 起一个干净的状态 + 两个客户端通道，方便断言回复/事件。
    fn fixture() -> (ServerState, u32, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
        let mut state = ServerState::new(Default::default());
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, sender);
        (state, 0, receiver)
    }

    /// 处理一条请求并断言成功。
    fn ok(state: &mut ServerState, client: u32, sequence: u16, request: Request<'static>) {
        state
            .handle(client, sequence, &request)
            .unwrap_or_else(|e| panic!("请求 {request:?} 失败：{e:?}"));
    }

    /// 处理一条请求并断言返回指定错误码。
    fn fails(state: &mut ServerState, client: u32, sequence: u16, request: Request<'static>, code: u8) {
        match state.handle(client, sequence, &request) {
            Err(e) => assert_eq!(e.code, code, "期望错误码 {code}，实际 {e:?}"),
            Ok(()) => panic!("请求 {request:?} 应当失败（错误码 {code}）"),
        }
    }

    #[test]
    fn query_extension_reports_the_registry() {
        // BIG-REQUESTS、XC-MISC 与 RENDER 都在表里
        assert_eq!(query_extension("BIG-REQUESTS").map(|e| e.0), Some(OPCODE_BIG_REQUESTS));
        assert_eq!(query_extension("XC-MISC").map(|e| e.0), Some(OPCODE_XC_MISC));
        assert_eq!(query_extension("RENDER").map(|e| e.0), Some(OPCODE_RENDER), "RENDER 已注册");

    /// 输入协议开关（task-8）：Core 模式下 QueryExtension("XInputExtension")
    /// 必须回不存在、ListExtensions 不列它；Xi2 模式恢复后重新可见。
    #[test]
    fn core_input_mode_hides_xinput_extension() {
        crate::config::set_xinput_mode(crate::config::XInputMode::Core);
        assert_eq!(
            query_extension("XInputExtension"),
            None,
            "Core 模式不得宣告 XInputExtension"
        );
        assert!(
            !extension_names()
                .iter()
                .any(|n| n.as_slice() == b"XInputExtension"),
            "ListExtensions 也不得列出它"
        );
        // 其余扩展不受影响
        assert!(query_extension("XKEYBOARD").is_some(), "XKB 与开关无关");

        crate::config::set_xinput_mode(crate::config::XInputMode::Xi2);
        assert!(
            query_extension("XInputExtension").is_some(),
            "Xi2 模式恢复后重新宣告"
        );
    }
        // 事件基址自洽：ExtInfoProvider 与 QueryExtension 用同一组数字
        let table = ExtTable;
        let (_, info) = table
            .get_from_major_opcode(OPCODE_DAMAGE)
            .expect("DAMAGE 应当在注册表里");
        assert_eq!(info.first_event, FIRST_EVENT_DAMAGE);
        // DamageNotify 的响应类型能反查回 DAMAGE
        let (_, hit) = table
            .get_from_event_code(FIRST_EVENT_DAMAGE + damage::NOTIFY_EVENT)
            .expect("DamageNotify 的事件码应当命中 DAMAGE");
        assert_eq!(hit.major_opcode, OPCODE_DAMAGE);
    }

    /// firefox/GTK 会创建 `depth = 0 AND class = InputOnly` 的 1x1 子窗口
    /// （CreateWindow request_code 1 的 BadMatch 就是这里来的）：规范要求
    /// InputOnly 的 depth/visual 是 CopyFromParent，绝不能展开成父窗口的
    /// 深度后再按 InputOnly 规则校验。
    #[test]
    fn input_only_windows_accept_copy_from_parent_depth() {
        let (mut state, client, mut rx) = fixture();
        let parent = 0x0020_0001u32;
        let input_only = 0x0020_0002u32;

        ok(&mut state, client, 1, Request::CreateWindow(xproto::CreateWindowRequest {
            depth: 0,
            wid: parent,
            parent: super::super::handshake::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 64,
            height: 64,
            border_width: 0,
            class: xproto::WindowClass::COPY_FROM_PARENT,
            visual: 0,
            value_list: Cow::default(),
        }));
        let parent_depth = state.window(parent).expect("parent window").depth;
        assert_eq!(parent_depth, super::super::handshake::SCREEN_DEPTH);

        // InputOnly：depth 0 / visual 0 / border 0 —— 必须成功。
        ok(&mut state, client, 2, Request::CreateWindow(xproto::CreateWindowRequest {
            depth: 0,
            wid: input_only,
            parent,
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            border_width: 0,
            class: xproto::WindowClass::INPUT_ONLY,
            visual: 0,
            value_list: Cow::default(),
        }));
        let window = state.window(input_only).expect("input-only window");
        assert_eq!(window.depth, 0, "InputOnly 窗口的 depth 恒为 0");
        assert!(window.pixels.is_empty(), "InputOnly 窗口不参与合成");
        let _ = rx;
    }
    #[test]
    fn xc_misc_returns_the_client_id_range() {
        let (mut state, client, mut rx) = fixture();
        ok(&mut state, client, 1, Request::XcMiscGetXIDRange(xc_misc::GetXIDRangeRequest));
        let frame = rx.try_recv().expect("GetXIDRange 应当有回复");
        let (reply, _) = xc_misc::GetXIDRangeReply::try_parse(&frame).expect("解析回复");
        assert_eq!(reply.start_id, super::super::handshake::resource_id_base(0));
        assert!(reply.count > 0, "空区间会让 XCB 无 ID 可用");

        // 先占掉一个 ID，count 应当减一，而 GetXIDList 不返回它
        ok(&mut state, client, 2, Request::CreateWindow(xproto::CreateWindowRequest {
            depth: 0,
            wid: reply.start_id,
            parent: super::super::handshake::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 8,
            height: 8,
            border_width: 0,
            class: xproto::WindowClass::INPUT_OUTPUT,
            visual: 0,
            value_list: Cow::default(),
        }));
        ok(&mut state, client, 3, Request::XcMiscGetXIDList(xc_misc::GetXIDListRequest { count: 4 }));
        let frame = rx.try_recv().expect("GetXIDList 应当有回复");
        let (list, _) = xc_misc::GetXIDListReply::try_parse(&frame).expect("解析回复");
        assert_eq!(list.ids.len(), 4);
        assert!(!list.ids.contains(&reply.start_id), "已占用的 ID 不应当再发出去");
    }

    #[test]
    fn xtest_fake_input_injects_real_input() {
        let (mut state, client, mut rx) = fixture();
        ok(&mut state, client, 1, Request::XtestGetVersion(xtest::GetVersionRequest { major_version: 2, minor_version: 2 }));
        let frame = rx.try_recv().expect("XTEST 版本回复");
        let (reply, _) = xtest::GetVersionReply::try_parse(&frame).expect("解析回复");
        assert_eq!((reply.major_version, reply.minor_version), (2, 2));

        // 建一个带 KeyPress 掩码的窗口并给它焦点，然后 fake 一个按键
        let window = 0x0020_0001u32;
        ok(&mut state, client, 2, Request::CreateWindow(xproto::CreateWindowRequest {
            depth: 0,
            wid: window,
            parent: super::super::handshake::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 32,
            height: 32,
            border_width: 0,
            class: xproto::WindowClass::INPUT_OUTPUT,
            visual: 0,
            value_list: Cow::Owned(xproto::CreateWindowAux {
                event_mask: Some(xproto::EventMask::KEY_PRESS),
                ..Default::default()
            }),
        }));
        state.set_input_focus(window, u8::from(xproto::InputFocus::PARENT));
        ok(&mut state, client, 3, Request::XtestFakeInput(xtest::FakeInputRequest {
            type_: 2, // KeyPress
            detail: 38, // 'a'
            time: 0,
            root: 0,
            root_x: 0,
            root_y: 0,
            deviceid: 0,
        }));
        let frame = rx.try_recv().expect("KeyPress 应当注入并投递");
        let event = x11rb_protocol::protocol::Event::parse(&frame, &ExtTable).expect("解析事件");
        match event {
            x11rb_protocol::protocol::Event::KeyPress(press) => {
                assert_eq!(press.event, window);
                assert_eq!(press.detail, 38);
            }
            other => panic!("期望 KeyPress，实际 {other:?}"),
        }

        // fake 一个指针移动：QueryPointer 能看到新位置
        ok(&mut state, client, 4, Request::XtestFakeInput(xtest::FakeInputRequest {
            type_: 6, // MotionNotify
            detail: 0,
            time: 0,
            root: 0,
            root_x: 40,
            root_y: 50,
            deviceid: 0,
        }));
        assert_eq!(
            (state.input.pointer_x, state.input.pointer_y),
            (40, 50),
            "FakeInput 的绝对坐标应当落到指针状态里"
        );
    }

    #[test]
    fn shm_reports_version_but_attach_fails() {
        let (mut state, client, mut rx) = fixture();
        ok(&mut state, client, 1, Request::ShmQueryVersion(shm::QueryVersionRequest));
        let frame = rx.try_recv().expect("SHM 版本回复");
        let (reply, _) = shm::QueryVersionReply::try_parse(&frame).expect("解析回复");
        assert_eq!((reply.major_version, reply.minor_version), (1, 2));
        assert!(!reply.shared_pixmaps);

        // Attach 必须失败（BadAccess）：没有共享内存，客户端走 PutImage 回退
        fails(
            &mut state,
            client,
            2,
            Request::ShmAttach(shm::AttachRequest { shmseg: 0x0020_0001, shmid: 1234, read_only: false }),
            BAD_ACCESS,
        );
        // Detach 忽略
        ok(&mut state, client, 3, Request::ShmDetach(shm::DetachRequest { shmseg: 0x0020_0001 }));
    }

    /// `ShapeGetRectangles` 的 `ordering` 字段必须**诚实反映真实排布**：我们输出的是
    /// 逐行 span（同 y 同高、行内按 x 有序）= 协议里的 **YXBanded(3)**。
    ///
    /// 此前写死 `UNSORTED`，GDK 会打印
    /// `Gdk-WARNING: non YXBanded shape masks not supported`，然后**仍按 banded
    /// 语义**解释这份数据。2026-11-11 用户在 firefox 上实测到该告警出现在崩溃前
    /// 毫秒级（同时 X11 标签"很卡"），所以这里既断言字段值，也断言列表本身满足
    /// YXBanded 不变式——两者必须同时成立，否则 ordering 就是谎报。
    #[test]
    fn shape_get_rectangles_reports_yx_banded_ordering() {
        let (mut state, client, mut rx) = fixture();
        let window = 0x0021_0001u32;
        ok(&mut state, client, 1, Request::CreateWindow(xproto::CreateWindowRequest {
            depth: 0,
            wid: window,
            parent: super::super::handshake::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 64,
            height: 64,
            border_width: 0,
            class: xproto::WindowClass::INPUT_OUTPUT,
            visual: 0,
            value_list: Cow::default(),
        }));
        state.map_window(window, 2).unwrap();

        // L 形 bounding：上半 64 宽、下半只留左边 16 像素
        ok(&mut state, client, 3, Request::ShapeRectangles(shape::RectanglesRequest {
            operation: shape::SO::SET,
            destination_kind: shape::SK::BOUNDING,
            ordering: xproto::ClipOrdering::YX_BANDED,
            destination_window: window,
            x_offset: 0,
            y_offset: 0,
            rectangles: Cow::Owned(vec![
                xproto::Rectangle { x: 0, y: 0, width: 64, height: 32 },
                xproto::Rectangle { x: 0, y: 32, width: 16, height: 32 },
            ]),
        }));

        while rx.try_recv().is_ok() {} // 清掉前面的回复，只留 GetRectangles
        ok(&mut state, client, 4, Request::ShapeGetRectangles(shape::GetRectanglesRequest {
            window,
            source_kind: shape::SK::BOUNDING,
        }));
        let frame = rx.try_recv().expect("ShapeGetRectangles 应当有回复");

        // ① wire 偏移 1 就是 ordering；协议值 YXBanded = 3
        assert_eq!(frame[0], 1, "回复类型");
        assert_eq!(
            frame[1], 3,
            "ordering 必须是 YXBanded(3)：写 UNSORTED 会让 GDK 打印 non YXBanded 告警"
        );
        let (reply, _) = shape::GetRectanglesReply::try_parse(&frame).expect("解析回复");
        assert_eq!(u8::from(reply.ordering), 3);

        // ② 列表本身必须真是 YXBanded：y 非递减；同 y 内 height 一致且 x 递增
        let mut prev_y: Option<i16> = None;
        let mut prev_x: Option<i16> = None;
        let mut band_h: Option<u16> = None;
        let mut area: i64 = 0;
        for r in &reply.rectangles {
            match prev_y {
                Some(y) if r.y == y => {
                    assert_eq!(band_h, Some(r.height), "同一 band 内 height 必须一致");
                    assert!(prev_x.is_none_or(|px| r.x > px), "同一 band 内 x 必须递增");
                }
                Some(y) => assert!(r.y > y, "y 必须非递减（yx-banded 的前提）"),
                None => band_h = Some(r.height),
            }
            prev_y = Some(r.y);
            prev_x = Some(r.x);
            area += i64::from(r.width) * i64::from(r.height);
        }
        // ③ 面积守恒：L 形 = 64*32 + 16*32
        assert_eq!(area, 64 * 32 + 16 * 32, "区域面积必须与设置的一致");
    }

    #[test]
    fn shape_rectangles_affect_extents_and_hit_testing() {
        let (mut state, client, _rx) = fixture();
        let window = 0x0020_0001u32;
        ok(&mut state, client, 1, Request::CreateWindow(xproto::CreateWindowRequest {
            depth: 0,
            wid: window,
            parent: super::super::handshake::ROOT_WINDOW,
            x: 10,
            y: 10,
            width: 64,
            height: 64,
            border_width: 0,
            class: xproto::WindowClass::INPUT_OUTPUT,
            visual: 0,
            value_list: Cow::default(),
        }));
        state.map_window(window, 2).unwrap();

        // 设一个 L 形 bounding：上半宽 64，下半只留左边 16 像素
        ok(&mut state, client, 3, Request::ShapeRectangles(shape::RectanglesRequest {
            operation: shape::SO::SET,
            destination_kind: shape::SK::BOUNDING,
            ordering: xproto::ClipOrdering::UNSORTED,
            destination_window: window,
            x_offset: 0,
            y_offset: 0,
            rectangles: Cow::Owned(vec![
                xproto::Rectangle { x: 0, y: 0, width: 64, height: 32 },
                xproto::Rectangle { x: 0, y: 32, width: 16, height: 32 },
            ]),
        }));

        // QueryExtents：包围盒应当是整窗（两块矩形的并）
        ok(&mut state, client, 4, Request::ShapeQueryExtents(shape::QueryExtentsRequest { destination_window: window }));
        // （回复在 _rx 里，这里直接断言状态——见下面的命中测试）

        // 命中测试：右上角在形状之外，左边仍在形状之内
        assert_eq!(state.window_at(70, 20), window, "上半部分完整");
        assert_eq!(state.window_at(20, 50), window, "下半左侧仍然命中");
        assert_ne!(state.window_at(70, 50), window, "下半右侧被 shape 裁掉了");

        // GetRectangles 往返：逐行展开的结果并回去应当还是 L 形
        ok(&mut state, client, 5, Request::ShapeGetRectangles(shape::GetRectanglesRequest {
            window,
            source_kind: shape::SK::BOUNDING,
        }));
    }

    /// GetRectangles 应答的 `ordering` 字节（偏移 1）必须诚实为 **YXBanded=3**，
    /// 且矩形集合满足 YXBanded 不变式（同 y 同高、行内 x 递增、y 非递减）。
    /// 回归点：曾谎报 UNSORTED，GDK 打印
    /// `Gdk-WARNING: non YXBanded shape masks not supported`（2026-11-11
    /// 实测：告警出现在 firefox 崩溃前毫秒级，且伴随 X11 标签"很卡"）。
    #[test]
    fn get_rectangles_reply_declares_yx_banded_ordering() {
        let (mut state, client, mut rx) = fixture();
        let window = 0x0020_0001u32;
        ok(&mut state, client, 1, Request::CreateWindow(xproto::CreateWindowRequest {
            depth: 0,
            wid: window,
            parent: super::super::handshake::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 64,
            height: 64,
            border_width: 0,
            class: xproto::WindowClass::INPUT_OUTPUT,
            visual: 0,
            value_list: Cow::default(),
        }));
        ok(&mut state, client, 2, Request::ShapeRectangles(shape::RectanglesRequest {
            operation: shape::SO::SET,
            destination_kind: shape::SK::BOUNDING,
            ordering: xproto::ClipOrdering::UNSORTED,
            destination_window: window,
            x_offset: 0,
            y_offset: 0,
            rectangles: Cow::Owned(vec![
                xproto::Rectangle { x: 0, y: 0, width: 64, height: 32 },
                xproto::Rectangle { x: 0, y: 32, width: 16, height: 32 },
            ]),
        }));

        ok(&mut state, client, 3, Request::ShapeGetRectangles(shape::GetRectanglesRequest {
            window,
            source_kind: shape::SK::BOUNDING,
        }));
        let frame = rx.try_recv().expect("GetRectangles 应当有回复");
        // **字节级断言**：应答偏移 1 的 ordering == YXBanded(3)
        assert_eq!(frame[1], 3, "ordering 必须是 YXBanded=3（GDK 的 banded 快速路径）");
        // 变长部分直接从应答字节解码矩形，检查 YXBanded 不变式
        let rect_count = u16::from_ne_bytes([frame[8], frame[9]]) as usize;
        assert!(rect_count > 0);
        let mut prev_y: Option<i16> = None;
        let mut prev_x: Option<i16> = None;
        let mut band_h: Option<u16> = None;
        for i in 0..rect_count {
            let base = 32 + i * 8;
            let x = i16::from_ne_bytes(frame[base..base + 2].try_into().unwrap());
            let y = i16::from_ne_bytes(frame[base + 2..base + 4].try_into().unwrap());
            let w = u16::from_ne_bytes(frame[base + 4..base + 6].try_into().unwrap());
            let h = u16::from_ne_bytes(frame[base + 6..base + 8].try_into().unwrap());
            match prev_y {
                Some(py) if y == py => {
                    assert_eq!(band_h, Some(h), "同 band 的矩形必须同高");
                    assert!(prev_x.is_some_and(|px| x > px), "band 内按 x 递增");
                }
                Some(py) => assert!(y > py, "y 必须递增（band 边界）"),
                None => {}
            }
            prev_y = Some(y);
            prev_x = Some(x);
            band_h = Some(h);
        }
    }

    /// ShapeNotify：形状变化必须发给**选择了** ShapeNotifyMask 的客户端，
    /// 未选择者不得收到；ShapeInputSelected 如实回报告。
    #[test]
    fn shape_notify_is_sent_only_to_selecting_clients() {
        let (mut state, client, mut rx) = fixture();
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(1, tx2);
        let window = 0x0020_0001u32;
        ok(&mut state, client, 1, Request::CreateWindow(xproto::CreateWindowRequest {
            depth: 0,
            wid: window,
            parent: super::super::handshake::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 64,
            height: 64,
            border_width: 0,
            class: xproto::WindowClass::INPUT_OUTPUT,
            visual: 0,
            value_list: Cow::default(),
        }));
        // 客户端 0 选择形状事件；客户端 1 不选
        ok(&mut state, client, 2, Request::ShapeSelectInput(shape::SelectInputRequest {
            destination_window: window,
            enable: true,
        }));
        ok(&mut state, client, 3, Request::ShapeInputSelected(shape::InputSelectedRequest {
            destination_window: window,
        }));
        let frame = rx.try_recv().expect("InputSelected 应当有回复");
        let (reply, _) = shape::InputSelectedReply::try_parse(&frame).expect("应能解析");
        assert!(reply.enabled, "已选择 → InputSelected 必须回 true");

        // 改形状 → 选择者收到 ShapeNotify，未选择者收不到
        ok(&mut state, client, 4, Request::ShapeRectangles(shape::RectanglesRequest {
            operation: shape::SO::SET,
            destination_kind: shape::SK::BOUNDING,
            ordering: xproto::ClipOrdering::UNSORTED,
            destination_window: window,
            x_offset: 0,
            y_offset: 0,
            rectangles: Cow::Owned(vec![xproto::Rectangle { x: 0, y: 0, width: 32, height: 64 }]),
        }));
        let notify = rx.try_recv().expect("选择者必须收到 ShapeNotify");
        assert_eq!(notify[0], FIRST_EVENT_SHAPE, "事件码 = SHAPE 的 event base");
        assert_eq!(notify[1], u8::from(shape::SK::BOUNDING), "shape_kind = Bounding");
        let affected = u32::from_ne_bytes(notify[4..8].try_into().unwrap());
        assert_eq!(affected, window, "affected_window 字段");
        let (event, _) = shape::NotifyEvent::try_parse(&notify).expect("ShapeNotify 必须能解析");
        assert!(event.shaped, "有形状 → shaped=true");
        assert_eq!(event.extents_width, 32);

        assert!(
            rx2.try_recv().is_err(),
            "未选择者严禁收到 ShapeNotify"
        );

        // 取消选择后 InputSelected 如实回 false
        ok(&mut state, client, 5, Request::ShapeSelectInput(shape::SelectInputRequest {
            destination_window: window,
            enable: false,
        }));
        ok(&mut state, client, 6, Request::ShapeInputSelected(shape::InputSelectedRequest {
            destination_window: window,
        }));
        let frame = rx.try_recv().expect("InputSelected 应当有回复");
        let (reply, _) = shape::InputSelectedReply::try_parse(&frame).expect("应能解析");
        assert!(!reply.enabled);

        // 重新选择后：SET 空区域 = 取消形状 → shaped=false 的边界事件
        ok(&mut state, client, 7, Request::ShapeSelectInput(shape::SelectInputRequest {
            destination_window: window,
            enable: true,
        }));
        assert!(
            rx.try_recv().is_err() && rx2.try_recv().is_err(),
            "step8 之前两个通道都不得有滞留帧"
        );
        ok(&mut state, client, 8, Request::ShapeRectangles(shape::RectanglesRequest {
            operation: shape::SO::SET,
            destination_kind: shape::SK::BOUNDING,
            ordering: xproto::ClipOrdering::UNSORTED,
            destination_window: window,
            x_offset: 0,
            y_offset: 0,
            rectangles: Cow::Owned(Vec::new()),
        }));
        let notify = rx.try_recv().expect("取消形状也必须发 ShapeNotify");
        let (event, _) = shape::NotifyEvent::try_parse(&notify).expect("应能解析");
        assert!(!event.shaped, "SET 空区域 → shaped=0");
        // 取消形状后 extents 回隐式全窗口矩形（真实 X.org 行为）
        assert_eq!(event.extents_width, 64, "无形状 → extents = 隐式窗口矩形");
        assert_eq!(event.extents_height, 64);
    }

    /// 回复的 `length` 字段必须与总长自洽（单位 4 字节，不含 32 字节固定头）。
    ///
    /// 回归点：`send_reply` 曾把它恒设为 0——32 字节回复恰好正确，但
    /// GetWindowAttributes 的 44 字节回复会少报 12 字节，客户端把余下的字节
    /// 当成下一条回复的开头，整个协议流错位（firefox 建完探针窗口即静默卡死）。
    #[test]
    fn reply_length_field_matches_the_frame_size() {
        let (mut state, client, mut rx) = fixture();
        let window = 0x0020_0001u32;
        ok(&mut state, client, 1, Request::CreateWindow(xproto::CreateWindowRequest {
            depth: 0,
            wid: window,
            parent: super::super::handshake::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 8,
            height: 8,
            border_width: 0,
            class: xproto::WindowClass::INPUT_OUTPUT,
            visual: 0,
            value_list: Cow::default(),
        }));

        // 44 字节回复：固定 32 + 3 个变长单元
        ok(&mut state, client, 2, Request::GetWindowAttributes(xproto::GetWindowAttributesRequest {
            window,
        }));
        let frame = rx.try_recv().expect("GetWindowAttributes 应当有回复");
        assert_eq!(frame[0], 1, "回复以 1 开头");
        let length = u32::from_ne_bytes([frame[4], frame[5], frame[6], frame[7]]);
        assert_eq!(length, 3, "GetWindowAttributes 有 12 字节变长部分");
        assert_eq!(frame.len(), 32 + length as usize * 4, "length 字段与总长自洽");

        // 32 字节回复：length = 0
        ok(&mut state, client, 3, Request::GetGeometry(xproto::GetGeometryRequest {
            drawable: window,
        }));
        let frame = rx.try_recv().expect("GetGeometry 应当有回复");
        let length = u32::from_ne_bytes([frame[4], frame[5], frame[6], frame[7]]);
        assert_eq!(length, 0);
        assert_eq!(frame.len(), 32);
    }

    /// GTK 用 `ShapeMask(ShapeInput)` 给窗口挖掉阴影/圆角处的点击区；
    /// 只接受 Bounding/Clip 会让客户端拿到 BadValue 直接 abort
    /// （firefox: `BadValue request_code 132 minor_code 2`）。
    #[test]
    fn shape_mask_accepts_the_input_kind() {
        let (mut state, client, mut rx) = fixture();
        let window = 0x0020_0001u32;
        let pixmap = 0x0020_0002u32;

        ok(&mut state, client, 1, Request::CreateWindow(xproto::CreateWindowRequest {
            depth: 0,
            wid: window,
            parent: super::super::handshake::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 64,
            height: 64,
            border_width: 0,
            class: xproto::WindowClass::INPUT_OUTPUT,
            visual: 0,
            value_list: Cow::default(),
        }));
        state.map_window(window, 2).unwrap();

        // 1 位深度的掩码位图：左上 32x32 置位
        ok(&mut state, client, 3, Request::CreatePixmap(xproto::CreatePixmapRequest {
            depth: 1,
            pid: pixmap,
            drawable: window,
            width: 64,
            height: 64,
        }));
        let mut pixels = vec![0u32; 64 * 64];
        for y in 0..32 {
            for x in 0..32 {
                pixels[y * 64 + x] = 1;
            }
        }
        state.pixmaps.get_mut(&pixmap).expect("mask pixmap").pixels = pixels;

        ok(&mut state, client, 4, Request::ShapeMask(shape::MaskRequest {
            operation: shape::SO::SET,
            destination_kind: shape::SK::INPUT,
            destination_window: window,
            x_offset: 0,
            y_offset: 0,
            source_bitmap: pixmap,
        }));

        // 输入区域生效：只有左上角可点击，其余位置不再命中该窗口。
        assert_eq!(state.window_at(10, 10), window, "输入区域内的点仍然命中");
        assert_ne!(state.window_at(50, 50), window, "输入区域外的点被挖掉");
        let _ = rx;
    }
    #[test]
    fn xfixes_region_roundtrip_and_binary_ops() {
        let (mut state, client, mut rx) = fixture();
        let (region_a, region_b, region_u) = (0x0020_0001u32, 0x0020_0002u32, 0x0020_0003u32);
        let rect = |x, y, w, h| xproto::Rectangle { x, y, width: w, height: h };

        ok(&mut state, client, 1, Request::XfixesCreateRegion(xfixes::CreateRegionRequest {
            region: region_a,
            rectangles: Cow::Owned(vec![rect(0, 0, 10, 10)]),
        }));
        ok(&mut state, client, 2, Request::XfixesCreateRegion(xfixes::CreateRegionRequest {
            region: region_b,
            rectangles: Cow::Owned(vec![rect(5, 5, 10, 10)]),
        }));
        ok(&mut state, client, 3, Request::XfixesUnionRegion(xfixes::UnionRegionRequest {
            source1: region_a,
            source2: region_b,
            destination: region_u,
        }));

        // FetchRegion 拿回并集：包围盒 (0,0)-(15,15)
        ok(&mut state, client, 4, Request::XfixesFetchRegion(xfixes::FetchRegionRequest { region: region_u }));
        let frame = rx.try_recv().expect("FetchRegion 回复");
        let (reply, _) = xfixes::FetchRegionReply::try_parse(&frame).expect("解析回复");
        assert_eq!((reply.extents.x, reply.extents.y), (0, 0));
        assert_eq!((reply.extents.width, reply.extents.height), (15, 15));

        // 不存在的 region 报 BadRegion（扩展自定义错误码）
        fails(
            &mut state,
            client,
            5,
            Request::XfixesDestroyRegion(xfixes::DestroyRegionRequest { region: 0x0020_0099 }),
            BAD_REGION,
        );

        // 差集 + 平移 + extents
        ok(&mut state, client, 6, Request::XfixesSubtractRegion(xfixes::SubtractRegionRequest {
            source1: region_u,
            source2: region_a,
            destination: region_b, // 复用 b 作目标（覆盖）
        }));
        ok(&mut state, client, 7, Request::XfixesTranslateRegion(xfixes::TranslateRegionRequest {
            region: region_b,
            dx: 100,
            dy: 100,
        }));
        ok(&mut state, client, 8, Request::XfixesFetchRegion(xfixes::FetchRegionRequest { region: region_b }));
        let frame = rx.try_recv().expect("FetchRegion 回复");
        let (reply, _) = xfixes::FetchRegionReply::try_parse(&frame).expect("解析回复");
        // 差集是 (10..15)x(5..10) 与 (5..15)x(10..15)，平移 +100 后包围盒从 (105,105) 开始
        assert_eq!((reply.extents.x, reply.extents.y), (105, 105));
    }

    #[test]
    fn composite_and_damage_basic_roundtrip() {
        let (mut state, client, mut rx) = fixture();

        // Composite：QueryVersion 0.4 + Redirect 成功
        ok(&mut state, client, 1, Request::CompositeQueryVersion(composite::QueryVersionRequest {
            client_major_version: 0,
            client_minor_version: 4,
        }));
        let frame = rx.try_recv().expect("Composite 版本回复");
        let (reply, _) = composite::QueryVersionReply::try_parse(&frame).expect("解析回复");
        assert_eq!((reply.major_version, reply.minor_version), (0, 4));

        let window = 0x0020_0001u32;
        ok(&mut state, client, 2, Request::CreateWindow(xproto::CreateWindowRequest {
            depth: 0,
            wid: window,
            parent: super::super::handshake::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 16,
            height: 16,
            border_width: 0,
            class: xproto::WindowClass::INPUT_OUTPUT,
            visual: 0,
            value_list: Cow::default(),
        }));
        // 先映射：绘制入口只对已映射窗口触发 compose（damage 通知在 compose 里发）
        ok(&mut state, client, 3, Request::MapWindow(xproto::MapWindowRequest { window }));
        ok(&mut state, client, 4, Request::CompositeRedirectWindow(composite::RedirectWindowRequest {
            window,
            update: composite::Redirect::AUTOMATIC,
        }));

        // DAMAGE：QueryVersion + Create（应发初始空通知）+ 画一笔（compose 触发通知）+ Destroy
        ok(&mut state, client, 5, Request::DamageQueryVersion(damage::QueryVersionRequest {
            client_major_version: 1,
            client_minor_version: 1,
        }));
        let frame = rx.try_recv().expect("Damage 版本回复");
        let (_, rest) = damage::QueryVersionReply::try_parse(&frame).expect("解析回复");
        let _ = rest;

        let dmg = 0x0020_0002u32;
        ok(&mut state, client, 6, Request::DamageCreate(damage::CreateRequest {
            damage: dmg,
            drawable: window,
            level: damage::ReportLevel::RAW_RECTANGLES,
        }));
        // 初始通知：DamageNotify（事件码 = first_event）
        let frame = rx.try_recv().expect("初始 DamageNotify");
        assert_eq!(frame[0], FIRST_EVENT_DAMAGE, "响应类型应当是 DamageNotify 的事件码");
        // 布局：response_type(1) level(1) sequence(2) drawable(4) damage(4) timestamp(4)
        assert_eq!(
            u32::from_ne_bytes([frame[8], frame[9], frame[10], frame[11]]),
            dmg,
            "事件里要带 damage ID"
        );

        // 画一笔 → compose → dirty damage 发通知
        let gc = 0x0020_0003u32;
        ok(&mut state, client, 7, Request::CreateGC(xproto::CreateGCRequest {
            cid: gc,
            drawable: window,
            value_list: Cow::Owned(xproto::CreateGCAux { ..Default::default() }),
        }));
        ok(&mut state, client, 8, Request::PolyFillRectangle(xproto::PolyFillRectangleRequest {
            drawable: window,
            gc,
            rectangles: Cow::Owned(vec![xproto::Rectangle { x: 0, y: 0, width: 8, height: 8 }]),
        }));
        // NyaTerm: 合成批处理——绘制只标脏，DamageNotify 在合成时发出。
        // 帧泵不在这跑，测试里手动触发一次。
        state.ensure_composed();
        let frame = rx.try_recv().expect("绘制后的 DamageNotify");
        assert_eq!(frame[0], FIRST_EVENT_DAMAGE);

        // Subtract 清脏区：没有新绘制就没有新通知
        ok(&mut state, client, 9, Request::DamageSubtract(damage::SubtractRequest {
            damage: dmg,
            repair: 0,
            parts: 0,
        }));
        assert!(rx.try_recv().is_err(), "Subtract 后不应有额外通知");

        fails(
            &mut state,
            client,
            10,
            Request::DamageDestroy(damage::DestroyRequest { damage: 0x0020_0042 }),
            BAD_DAMAGE,
        );
        ok(&mut state, client, 11, Request::DamageDestroy(damage::DestroyRequest { damage: dmg }));
    }
}

