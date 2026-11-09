//! 核心协议里"杂项"那一批请求：颜色、光标、抓取、选择区、指针/键盘参数。
//!
//! 它们几乎不参与绘制，但**缺一条就会让 Xt/Xaw 程序在启动阶段收到 BadRequest**。
//! 真实案例：`XParseColor("white")` 会发 `LookupColor`，`XCreateFontCursor()`
//! 会发 `CreateGlyphCursor`，`Xaw` 的弹出菜单会 `GrabPointer`/`UngrabPointer`；
//! 客户端拿到 BadRequest 时的表现就是
//! `X Error of failed request: BadRequest (invalid request code or no such operation)`，
//! 然后多半直接退出。
//!
//! 这里的实现原则：
//! - 有回复的请求（颜色查询、QueryBestSize、抓取状态、各种 Get*）必须给出**自洽的回复**；
//! - 无回复的请求（Store*/Free*/Grab*/Ungrab*/Bell/…）在语义允许时接受并忽略；
//! - 真正做不到的（TrueColor 上分配颜色单元、没有时间轴的运动历史）按协议回
//!   `BadMatch` / 空列表，客户端会走降级路径而不是崩掉。

use x11rb_protocol::protocol::xproto::*;
use x11rb_protocol::x11_utils::Serialize;

use super::error::XError;
use super::handshake::ROOT_COLORMAP;
use super::state::{ColormapData, CursorRef, ServerState};

/// X11 颜色名表（16 位 RGB 由 8 位值 × 0x0101 得到）。
///
/// 只收常用的这批：toolkit 默认色（black/white/gray）、常见主题色，
/// 以及 rgb.txt 里最常被写死的那些。名字按 X11 规则忽略大小写与空格。
const NAMED_COLORS: &[(&str, u8, u8, u8)] = &[
    ("black", 0, 0, 0),
    ("white", 255, 255, 255),
    ("red", 255, 0, 0),
    ("green", 0, 255, 0),
    ("blue", 0, 0, 255),
    ("yellow", 255, 255, 0),
    ("cyan", 0, 255, 255),
    ("aqua", 0, 255, 255),
    ("magenta", 255, 0, 255),
    ("fuchsia", 255, 0, 255),
    ("gray", 190, 190, 190),
    ("grey", 190, 190, 190),
    ("darkgray", 169, 169, 169),
    ("darkgrey", 169, 169, 169),
    ("lightgray", 211, 211, 211),
    ("lightgrey", 211, 211, 211),
    ("dimgray", 105, 105, 105),
    ("dimgrey", 105, 105, 105),
    ("slategray", 112, 128, 144),
    ("slategrey", 112, 128, 144),
    ("darkslategray", 47, 79, 79),
    ("darkslategrey", 47, 79, 79),
    ("lightslategray", 119, 136, 153),
    ("lightslategrey", 119, 136, 153),
    ("orange", 255, 165, 0),
    ("darkorange", 255, 140, 0),
    ("purple", 160, 32, 240),
    ("brown", 165, 42, 42),
    ("pink", 255, 192, 203),
    ("hotpink", 255, 105, 180),
    ("deeppink", 255, 20, 147),
    ("gold", 255, 215, 0),
    ("goldenrod", 218, 165, 32),
    ("silver", 192, 192, 192),
    ("navy", 0, 0, 128),
    ("darkblue", 0, 0, 139),
    ("midnightblue", 25, 25, 112),
    ("royalblue", 65, 105, 225),
    ("dodgerblue", 30, 144, 255),
    ("cornflowerblue", 100, 149, 237),
    ("steelblue", 70, 130, 180),
    ("skyblue", 135, 206, 235),
    ("lightblue", 173, 216, 230),
    ("lightsteelblue", 176, 196, 222),
    ("teal", 0, 128, 128),
    ("darkcyan", 0, 139, 139),
    ("olive", 128, 128, 0),
    ("maroon", 176, 48, 96),
    ("darkred", 139, 0, 0),
    ("firebrick", 178, 34, 34),
    ("indianred", 205, 92, 92),
    ("crimson", 220, 20, 60),
    ("salmon", 250, 128, 114),
    ("tomato", 255, 99, 71),
    ("coral", 255, 127, 80),
    ("chocolate", 210, 105, 30),
    ("sienna", 160, 82, 45),
    ("peru", 205, 133, 63),
    ("tan", 210, 180, 140),
    ("wheat", 245, 222, 179),
    ("beige", 245, 245, 220),
    ("ivory", 255, 255, 240),
    ("seashell", 255, 245, 238),
    ("lavender", 230, 230, 250),
    ("azure", 240, 255, 255),
    ("mintcream", 245, 255, 250),
    ("khaki", 240, 230, 140),
    ("darkkhaki", 189, 183, 107),
    ("darkgreen", 0, 100, 0),
    ("forestgreen", 34, 139, 34),
    ("seagreen", 46, 139, 87),
    ("mediumseagreen", 60, 179, 113),
    ("springgreen", 0, 255, 127),
    ("lightgreen", 144, 238, 144),
    ("palegreen", 152, 251, 152),
    ("olivedrab", 107, 142, 35),
    ("yellowgreen", 154, 205, 50),
    ("lime", 0, 255, 0),
    ("limegreen", 50, 205, 50),
    ("violet", 238, 130, 238),
    ("orchid", 218, 112, 214),
    ("plum", 221, 160, 221),
    ("thistle", 216, 191, 216),
    ("turquoise", 64, 224, 208),
    ("lightcyan", 224, 255, 255),
];

/// 解析一个 X11 颜色名或 `#RGB` / `#RRGGBB` / `#RRRGGGBBB` / `#RRRRGGGGBBBB`。
///
/// 名字匹配忽略大小写与空格（`"Light Blue"` 等于 `"lightblue"`），
/// 与 X 服务端的规则一致；查不到返回 `None`，调用方回 `BadName`。
pub fn parse_color(spec: &str) -> Option<(u16, u16, u16)> {
    let cleaned: String = spec.chars().filter(|c| !c.is_whitespace()).collect();
    let lowered = cleaned.to_ascii_lowercase();

    if let Some(hex) = lowered.strip_prefix('#') {
        if hex.is_empty() || hex.len() % 3 != 0 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let width = hex.len() / 3;
        let scale = |part: &str| -> Option<u16> {
            let value = u32::from_str_radix(part, 16).ok()?;
            let max = (1u32 << (4 * width)) - 1;
            // 按位深等比放大到 0..=65535：全 0 与全 1 精确，中间值线性
            Some(((value * 0xffff + max / 2) / max) as u16)
        };
        return Some((
            scale(&hex[0..width])?,
            scale(&hex[width..width * 2])?,
            scale(&hex[width * 2..])?,
        ));
    }

    NAMED_COLORS
        .iter()
        .find(|(name, _, _, _)| *name == lowered)
        .map(|(_, r, g, b)| {
            (
                u16::from(*r) * 0x0101,
                u16::from(*g) * 0x0101,
                u16::from(*b) * 0x0101,
            )
        })
}

/// 8 位分量 → 24 位 TrueColor 的 pixel 值（和 `AllocColor` 的算法一致）。
fn true_color_pixel(rgb: (u16, u16, u16)) -> u32 {
    (u32::from(rgb.0 >> 8) << 16) | (u32::from(rgb.1 >> 8) << 8) | u32::from(rgb.2 >> 8)
}

/// 24 位 pixel → 16 位 RGB。
fn pixel_to_rgb(pixel: u32) -> (u16, u16, u16) {
    (
        ((pixel >> 16) & 0xff) as u16 * 0x0101,
        ((pixel >> 8) & 0xff) as u16 * 0x0101,
        (pixel & 0xff) as u16 * 0x0101,
    )
}

impl ServerState {
    fn colormap_exists(&self, cmap: Colormap) -> Result<(), XError> {
        if self.colormaps.contains_key(&cmap) {
            Ok(())
        } else {
            Err(XError::bad_colormap(cmap))
        }
    }

    // ---- 颜色 ----

    pub(crate) fn lookup_color(
        &self,
        client: u32,
        sequence: u16,
        request: &LookupColorRequest<'_>,
    ) -> Result<(), XError> {
        self.colormap_exists(request.cmap)?;
        let (red, green, blue) =
            parse_color(&String::from_utf8_lossy(&request.name)).ok_or_else(XError::bad_name)?;
        // 我们是 24 位 TrueColor：精确值与实际值一致
        self.send_reply(
            client,
            &LookupColorReply {
                sequence,
                length: 0,
                exact_red: red,
                exact_green: green,
                exact_blue: blue,
                visual_red: red,
                visual_green: green,
                visual_blue: blue,
            },
        );
        Ok(())
    }

    pub(crate) fn alloc_named_color(
        &self,
        client: u32,
        sequence: u16,
        request: &AllocNamedColorRequest<'_>,
    ) -> Result<(), XError> {
        self.colormap_exists(request.cmap)?;
        let rgb =
            parse_color(&String::from_utf8_lossy(&request.name)).ok_or_else(XError::bad_name)?;
        self.send_reply(
            client,
            &AllocNamedColorReply {
                sequence,
                length: 0,
                pixel: true_color_pixel(rgb),
                exact_red: rgb.0,
                exact_green: rgb.1,
                exact_blue: rgb.2,
                visual_red: rgb.0,
                visual_green: rgb.1,
                visual_blue: rgb.2,
            },
        );
        Ok(())
    }

    pub(crate) fn query_colors(
        &self,
        client: u32,
        sequence: u16,
        request: &QueryColorsRequest<'_>,
    ) -> Result<(), XError> {
        self.colormap_exists(request.cmap)?;
        let colors: Vec<Rgb> = request
            .pixels
            .iter()
            .map(|pixel| {
                let (red, green, blue) = pixel_to_rgb(*pixel);
                Rgb { red, green, blue }
            })
            .collect();
        // 每个 Rgb 8 字节；length 只数超出 32 字节固定头的部分
        let length = (colors.len() * 8 / 4) as u32;
        self.send_reply(
            client,
            &QueryColorsReply {
                sequence,
                length,
                colors,
            },
        );
        Ok(())
    }

    /// `StoreColors` / `StoreNamedColor` / `FreeColors`：TrueColor 下没有可写
    /// 的颜色表，接受即可（回 BadRequest 会让调色板的客户端直接报错）。
    pub(crate) fn touch_colormap(&self, cmap: Colormap) -> Result<(), XError> {
        self.colormap_exists(cmap)
    }

    pub(crate) fn named_color_known(&self, name: &[u8]) -> Result<(), XError> {
        parse_color(&String::from_utf8_lossy(name))
            .map(|_| ())
            .ok_or_else(XError::bad_name)
    }

    // ---- 颜色表安装 ----

    pub(crate) fn install_colormap(&self, cmap: Colormap) -> Result<(), XError> {
        // 只有一个默认颜色表，装/卸都只是校验一下 ID
        self.colormap_exists(cmap)
    }

    pub(crate) fn list_installed_colormaps(
        &self,
        client: u32,
        sequence: u16,
        request: &ListInstalledColormapsRequest,
    ) -> Result<(), XError> {
        self.window(request.window)?;
        let cmaps = vec![ROOT_COLORMAP];
        let length = cmaps.len() as u32;
        self.send_reply(
            client,
            &ListInstalledColormapsReply {
                sequence,
                length,
                cmaps,
            },
        );
        Ok(())
    }

    pub(crate) fn copy_colormap_and_free(
        &mut self,
        client: u32,
        request: &CopyColormapAndFreeRequest,
    ) -> Result<(), XError> {
        self.check_new_id(client, request.mid)?;
        let visual = self
            .colormaps
            .get(&request.src_cmap)
            .map(|cmap| cmap.visual)
            .ok_or_else(|| XError::bad_colormap(request.src_cmap))?;
        self.colormaps.insert(
            request.mid,
            ColormapData {
                id: request.mid,
                visual,
                owner: client,
            },
        );
        Ok(())
    }

    // ---- 光标 ----

    pub(crate) fn create_cursor(
        &mut self,
        client: u32,
        request: &CreateCursorRequest,
    ) -> Result<(), XError> {
        self.check_new_id(client, request.cid)?;
        for pixmap in [request.source, request.mask] {
            // mask 可以是 None(0)
            if pixmap != 0 && !self.pixmaps.contains_key(&pixmap) {
                return Err(XError::bad_pixmap(pixmap));
            }
        }
        self.cursors.insert(request.cid, CursorRef { owner: client, picture: None });
        Ok(())
    }

    pub(crate) fn create_glyph_cursor(
        &mut self,
        client: u32,
        request: &CreateGlyphCursorRequest,
    ) -> Result<(), XError> {
        self.check_new_id(client, request.cid)?;
        // `XCreateFontCursor` 就是先 OpenFont("cursor") 再发这条请求：
        // 字体必须真的在表里，否则客户端拿到的光标 ID 是无效的。
        for font in [request.source_font, request.mask_font] {
            if !self.fonts.contains_key(&font) {
                return Err(XError::bad_font(font));
            }
        }
        self.cursors.insert(request.cid, CursorRef { owner: client, picture: None });
        Ok(())
    }

    pub(crate) fn free_cursor(&mut self, cursor: Cursor) -> Result<(), XError> {
        let Some(ref_) = self.cursors.remove(&cursor) else {
            return Err(XError::bad_cursor(cursor));
        };
        // 光标引用释放：如果 picture 仍存活（被光标引用才没被 FreePicture
        // 删掉），现在引用归零，销毁它。
        if let Some(picture_id) = ref_.picture {
            if self.pictures.remove(&picture_id).is_some() {
                self.mark_compose_dirty();
            }
        }
        Ok(())
    }

    pub(crate) fn recolor_cursor(&self, cursor: Cursor) -> Result<(), XError> {
        // 我们不渲染光标，颜色记下来也没用；但光标必须存在
        self.cursor(cursor)
    }

    pub(crate) fn query_best_size(
        &self,
        client: u32,
        sequence: u16,
        request: &QueryBestSizeRequest,
    ) -> Result<(), XError> {
        if !self.windows.contains_key(&request.drawable)
            && !self.pixmaps.contains_key(&request.drawable)
        {
            return Err(XError::bad_drawable(request.drawable));
        }
        // 只有"整屏"这一种尺寸约束：原样返回请求值。回比请求更小的值会让
        // 客户端（Xaw 给菜单算尺寸时）反复重试。
        self.send_reply(
            client,
            &QueryBestSizeReply {
                sequence,
                length: 0,
                width: request.width,
                height: request.height,
            },
        );
        Ok(())
    }

    // ---- 抓取 ----

    /// 指针/键盘抓取：**真正生效**。
    ///
    /// NyaTerm: 这里以前只回 `Success` 而不重定向事件，直接导致"界面卡死"——
    /// GTK 拿到成功状态后进入拖动/菜单状态机，期待后续指针事件都投到
    /// grab_window；我们却仍按位置投递，它的状态机永远等不到事件，于是退化
    /// 成 QueryPointer 轮询（实测 350 次/秒）并停止绘制。心跳还活着、画面却
    /// 完全不动，就是用户看到的"操作一会就卡死（约 1 分钟）"。
    pub(crate) fn grab_pointer(
        &mut self,
        client: u32,
        sequence: u16,
        request: &GrabPointerRequest,
    ) -> Result<(), XError> {
        self.window(request.grab_window)?;
        if request.confine_to != 0 {
            self.window(request.confine_to)?;
        }
        self.input.pointer_grab = Some(super::input::PointerGrab {
            window: request.grab_window,
            owner_events: request.owner_events,
            event_mask: u32::from(request.event_mask),
            client,
        });
        self.send_reply(
            client,
            &GrabPointerReply {
                status: GrabStatus::SUCCESS,
                sequence,
                length: 0,
            },
        );
        Ok(())
    }

    /// 解除指针抓取（`XUngrabPointer`）。客户端断开时也会统一解除，避免
    /// 一个死掉的客户端把指针永久钉在某个窗口上。
    pub(crate) fn ungrab_pointer(&mut self) {
        self.input.pointer_grab = None;
    }

    /// `XChangeActivePointerGrab`：只更新活动抓取的事件掩码。
    pub(crate) fn change_active_pointer_grab(
        &mut self,
        request: &ChangeActivePointerGrabRequest,
    ) {
        if let Some(grab) = self.input.pointer_grab.as_mut() {
            grab.event_mask = u32::from(request.event_mask);
        }
    }

    /// 键盘抓取：真正生效（`XGrabKeyboard`）。
    pub(crate) fn grab_keyboard(
        &mut self,
        client: u32,
        sequence: u16,
        request: &GrabKeyboardRequest,
    ) -> Result<(), XError> {
        self.window(request.grab_window)?;
        self.input.keyboard_grab = Some(request.grab_window);
        self.send_reply(
            client,
            &GrabKeyboardReply {
                status: GrabStatus::SUCCESS,
                sequence,
                length: 0,
            },
        );
        Ok(())
    }

    /// 解除键盘抓取（`XUngrabKeyboard`）。
    pub(crate) fn ungrab_keyboard(&mut self) {
        self.input.keyboard_grab = None;
    }

    // ---- 选择区（剪贴板） ----
    //
    // 目前只维护"谁是属主"和"拿不到数据时也要回 SelectionNotify"，
    // 真正的数据传输（SelectionRequest/SelectionNotify 往返）留给剪贴板里程碑。

    pub(crate) fn set_selection_owner(
        &mut self,
        request: &SetSelectionOwnerRequest,
    ) -> Result<(), XError> {
        if request.owner == 0 {
            self.selections.remove(&request.selection);
        } else {
            self.window(request.owner)?;
            self.selections.insert(request.selection, request.owner);
        }
        Ok(())
    }

    pub(crate) fn get_selection_owner(
        &self,
        client: u32,
        sequence: u16,
        request: &GetSelectionOwnerRequest,
    ) -> Result<(), XError> {
        let owner = self
            .selections
            .get(&request.selection)
            .copied()
            .unwrap_or(0);
        // NyaTerm 探针：记录客户端查询的选择 atom（前 100 条）
        if crate::xsrv::counters::ATOM_LOGS.load(std::sync::atomic::Ordering::Relaxed) < 300 {
            let name = self.atoms.get(request.selection as usize).and_then(|o| o.as_deref()).unwrap_or("?");
            crate::xsrv::log_line(format_args!("[x11] c{client} GetSelectionOwner({name}) -> {owner:#x}"));
        }
        self.send_reply(
            client,
            &GetSelectionOwnerReply {
                sequence,
                length: 0,
                owner,
            },
        );
        Ok(())
    }

    pub(crate) fn convert_selection(
        &self,
        request: &ConvertSelectionRequest,
    ) -> Result<(), XError> {
        self.window(request.requestor)?;
        // 回一条 property=None 的 SelectionNotify：请求方立刻知道"没有数据"，
        // 而不是一直等一条永远不来的事件（那才是真的挂住）。
        let event = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property: 0,
        };
        self.send_to_owner(request.requestor, event.serialize().to_vec());
        Ok(())
    }

    // ---- 坐标与指针 ----

    pub(crate) fn translate_coordinates(
        &self,
        client: u32,
        sequence: u16,
        request: &TranslateCoordinatesRequest,
    ) -> Result<(), XError> {
        self.window(request.src_window)?;
        self.window(request.dst_window)?;
        let (src_x, src_y) = self.window_origin(request.src_window);
        let (dst_x, dst_y) = self.window_origin(request.dst_window);
        let root_x = src_x + i32::from(request.src_x);
        let root_y = src_y + i32::from(request.src_y);
        self.send_reply(
            client,
            &TranslateCoordinatesReply {
                same_screen: true,
                sequence,
                length: 0,
                // 没有做子窗口命中测试：回 None 是协议允许的
                child: 0,
                dst_x: (root_x - dst_x) as i16,
                dst_y: (root_y - dst_y) as i16,
            },
        );
        Ok(())
    }

    pub(crate) fn warp_pointer(&mut self, request: &WarpPointerRequest) -> Result<(), XError> {
        if request.src_window != 0 {
            self.window(request.src_window)?;
        }
        let (x, y) = if request.dst_window == 0 {
            // dst_window = None：给的坐标就是根坐标
            (request.dst_x, request.dst_y)
        } else {
            let (origin_x, origin_y) = self.window_origin(request.dst_window);
            (
                (origin_x + i32::from(request.dst_x)) as i16,
                (origin_y + i32::from(request.dst_y)) as i16,
            )
        };
        self.input.pointer_x = x;
        self.input.pointer_y = y;
        let target = self.window_at(x, y);
        self.input.pointer_window = if target == 0 { None } else { Some(target) };
        Ok(())
    }

    /// 没有记录运动历史（设计文档里"GetMotionEvents 未做"）：回空列表，
    /// 客户端按"这段时间没有事件"处理，不影响正常交互。
    pub(crate) fn get_motion_events(
        &self,
        client: u32,
        sequence: u16,
        _request: &GetMotionEventsRequest,
    ) -> Result<(), XError> {
        self.send_reply(
            client,
            &GetMotionEventsReply {
                sequence,
                length: 0,
                events: Vec::new(),
            },
        );
        Ok(())
    }

    // ---- 键盘/指针/屏保参数 ----

    pub(crate) fn get_keyboard_control(
        &self,
        client: u32,
        sequence: u16,
    ) -> Result<(), XError> {
        self.send_reply(
            client,
            &GetKeyboardControlReply {
                global_auto_repeat: AutoRepeatMode::ON,
                sequence,
                length: 0,
                led_mask: 0,
                key_click_percent: 0,
                bell_percent: 50,
                bell_pitch: 400,
                bell_duration: 100,
                // 所有 keycode 都报"可自动重复"，xterm 之类的按住重复才正常
                auto_repeats: [0xff; 32],
            },
        );
        Ok(())
    }

    pub(crate) fn get_pointer_control(
        &self,
        client: u32,
        sequence: u16,
    ) -> Result<(), XError> {
        self.send_reply(
            client,
            &GetPointerControlReply {
                sequence,
                length: 0,
                acceleration_numerator: 2,
                acceleration_denominator: 1,
                threshold: 4,
            },
        );
        Ok(())
    }

    pub(crate) fn get_screen_saver(
        &self,
        client: u32,
        sequence: u16,
    ) -> Result<(), XError> {
        self.send_reply(
            client,
            &GetScreenSaverReply {
                sequence,
                length: 0,
                timeout: 600,
                interval: 0,
                prefer_blanking: Blanking::DEFAULT,
                allow_exposures: Exposures::DEFAULT,
            },
        );
        Ok(())
    }

    pub(crate) fn get_pointer_mapping(
        &self,
        client: u32,
        sequence: u16,
    ) -> Result<(), XError> {
        // X 的按钮编号：1=左 2=中 3=右；恒等映射
        let map = vec![1u8, 2, 3];
        let length = map.len().div_ceil(4) as u32;
        self.send_reply(
            client,
            &GetPointerMappingReply {
                sequence,
                length,
                map,
            },
        );
        Ok(())
    }

    pub(crate) fn set_pointer_mapping(
        &self,
        client: u32,
        sequence: u16,
    ) -> Result<(), XError> {
        self.send_reply(
            client,
            &SetPointerMappingReply {
                status: MappingStatus::SUCCESS,
                sequence,
                length: 0,
            },
        );
        Ok(())
    }

    pub(crate) fn get_modifier_mapping(
        &self,
        client: u32,
        sequence: u16,
    ) -> Result<(), XError> {
        let keycodes = super::input::modifier_mapping();
        let length = keycodes.len().div_ceil(4) as u32;
        self.send_reply(
            client,
            &GetModifierMappingReply {
                sequence,
                length,
                keycodes,
            },
        );
        Ok(())
    }

    pub(crate) fn set_modifier_mapping(
        &self,
        client: u32,
        sequence: u16,
    ) -> Result<(), XError> {
        self.send_reply(
            client,
            &SetModifierMappingReply {
                status: MappingStatus::SUCCESS,
                sequence,
                length: 0,
            },
        );
        Ok(())
    }

    pub(crate) fn list_hosts(&self, client: u32, sequence: u16) -> Result<(), XError> {
        self.send_reply(
            client,
            &ListHostsReply {
                mode: AccessControl::DISABLE,
                sequence,
                length: 0,
                hosts: Vec::new(),
            },
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::parse_color;

    #[test]
    fn named_colors_ignore_case_and_spaces() {
        assert_eq!(parse_color("white"), Some((0xffff, 0xffff, 0xffff)));
        assert_eq!(parse_color("BLACK"), Some((0, 0, 0)));
        assert_eq!(parse_color("Light Blue"), parse_color("lightblue"));
        assert_eq!(parse_color("grey"), parse_color("gray"));
    }

    #[test]
    fn hex_forms_scale_to_16_bits() {
        assert_eq!(parse_color("#fff"), Some((0xffff, 0xffff, 0xffff)));
        assert_eq!(parse_color("#f00"), Some((0xffff, 0, 0)));
        assert_eq!(parse_color("#ff0000"), Some((0xffff, 0, 0)));
        assert_eq!(parse_color("#000000"), Some((0, 0, 0)));
        assert_eq!(parse_color("#ffff00000000"), Some((0xffff, 0, 0)));
        // 中间值线性放大：0x80 = 128 → 128*65535/255 = 32896 ≈ 0x8080
        assert_eq!(parse_color("#808080").map(|rgb| rgb.0), Some(0x8080));
    }

    #[test]
    fn unknown_or_malformed_names_are_rejected() {
        assert_eq!(parse_color("definitely-not-a-color"), None);
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("#"), None);
        assert_eq!(parse_color(""), None);
    }
}
