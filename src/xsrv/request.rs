//! 核心协议请求的分发与语义。
//!
//! 线格式解析在 `server.rs`（用 `Request::parse`），这里只处理"解析之后做什么"：
//! 资源校验、窗口树维护、事件投递、像素绘制。未实现的请求统一返回
//! BadRequest/BadImplementation，而不是静默丢弃——客户端在等回复，丢弃会表现为
//! "服务端无响应"，很难定位。
//!
//! M1 覆盖：窗口生命周期、atom/property、pixmap/GC/colormap、点线矩形等实心绘制、
//! PutImage/GetImage。字体、RENDER、XTEST 等见后续里程碑。

use x11rb_protocol::protocol::xproto::Property as PropertyState;
use x11rb_protocol::protocol::xproto::*;
use x11rb_protocol::protocol::{bigreq, xc_misc};
use x11rb_protocol::protocol::Request;
use x11rb_protocol::x11_utils::Serialize;

use super::draw;
use super::error::XError;
use super::extensions;
use super::font;
use super::handshake::{ARGB32_VISUAL, ROOT_COLORMAP, ROOT_VISUAL, ROOT_WINDOW, SCREEN_DEPTH};
use super::state::{ColormapData, FontData, GcData, PixmapData, Property, ServerState};

/// 裁剪坐标到 u16（宽高字段用）。
fn clamp_u16(value: i32) -> u16 {
    value.clamp(0, u16::MAX as i32) as u16
}

/// 4 字节对齐后的长度（X11 的变长字段都要补齐）。
fn padded4(len: usize) -> usize {
    (len + 3) & !3
}

impl ServerState {
    pub fn handle(
        &mut self,
        client: u32,
        sequence: u16,
        request: &Request<'_>,
    ) -> Result<(), XError> {
        // 记录客户端推进到的序列号：注入输入产生的事件要按它盖写事件帧
        // （见 `ServerState::stamp_sequence`），否则客户端的 XCB 会序号错乱崩溃。
        self.sequences.insert(client, sequence);

        // NyaTerm diagnostic (opt-in)：逐请求追踪——客户端停在某个等待点
        // （如 firefox 卡在探针后不建主窗）时，这份序列能显示它最后发了什么。
        if std::env::var_os("NYATERM_X11_TRACE").is_some() {
            let debug = format!("{request:?}");
            let debug: String = debug.chars().take(180).collect();
            crate::xsrv::log_line(format_args!("[x11-req] {debug}"));
        }

        let result = match request {
            Request::CreateWindow(request) => {
                let result = self.create_window(client, sequence, request);
                // NyaTerm diagnostic: without a tracing subscriber a rejected
                // CreateWindow is invisible, and the client only reports the
                // *first* error it reads (which may be a later request).
                if let Err(error) = &result {
                    crate::xsrv::log_line(format_args!(
                        "[x11] CreateWindow FAILED wid={:#x} parent={:#x} depth={} class={:?} visual={:#x} {}x{} -> {error:?}",
                        request.wid,
                        request.parent,
                        request.depth,
                        request.class,
                        request.visual,
                        request.width,
                        request.height
                    ));
                }
                result
            }
            Request::ChangeWindowAttributes(request) => {
                self.change_window_attributes(client, request)
            }
            Request::GetWindowAttributes(request) => {
                self.get_window_attributes(client, sequence, request.window)
            }
            Request::DestroyWindow(request) => self.destroy_window(request.window, sequence),
            Request::DestroySubwindows(request) => {
                let children = self.window(request.window)?.children.clone();
                for child in children {
                    self.destroy_window(child, sequence)?;
                }
                Ok(())
            }
            Request::MapWindow(request) => self.map_window(request.window, sequence),
            Request::MapSubwindows(request) => {
                let children = self.window(request.window)?.children.clone();
                for child in children {
                    self.map_window(child, sequence)?;
                }
                Ok(())
            }
            Request::UnmapWindow(request) => self.unmap_window(request.window, sequence),
            Request::UnmapSubwindows(request) => {
                let children = self.window(request.window)?.children.clone();
                for child in children {
                    self.unmap_window(child, sequence)?;
                }
                Ok(())
            }
            Request::ConfigureWindow(request) => {
                self.configure_window(request.window, sequence, request)
            }
            Request::GetGeometry(request) => {
                self.get_geometry(client, sequence, request.drawable)
            }
            Request::QueryTree(request) => self.query_tree(client, sequence, request.window),
            Request::InternAtom(request) => {
                // NyaTerm 探针：记录客户端探测的 atom 名（前 200 条），用于
                // 归因 GTK 启动探测链（window-0 查询的来源）。
                if crate::xsrv::counters::ATOM_LOGS.load(std::sync::atomic::Ordering::Relaxed) < 200 {
                    crate::xsrv::counters::ATOM_LOGS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let name = String::from_utf8_lossy(&request.name).to_string();
                    crate::xsrv::log_line(format_args!(
                        "[x11] c{client} InternAtom({name:?}) only_if_exists={}",
                        u8::from(request.only_if_exists)
                    ));
                }
                let atom = self.intern_atom(&request.name, request.only_if_exists)?;
                self.send_reply(
                    client,
                    &InternAtomReply {
                        sequence,
                        length: 0,
                        atom,
                    },
                );
                Ok(())
            }
            Request::GetAtomName(request) => {
                let name = self.atom_name(request.atom)?.as_bytes().to_vec();
                self.send_reply(
                    client,
                    &GetAtomNameReply {
                        sequence,
                        length: name.len().div_ceil(4) as u32,
                        name,
                    },
                );
                Ok(())
            }
            Request::ChangeProperty(request) => self.change_property(client, sequence, request),
            Request::DeleteProperty(request) => {
                self.delete_property(sequence, request.window, request.property)
            }
            Request::GetProperty(request) => self.get_property(client, sequence, request),
            Request::ListProperties(request) => {
                let atoms: Vec<Atom> = self
                    .window(request.window)?
                    .properties
                    .keys()
                    .copied()
                    .collect();
                self.send_reply(
                    client,
                    &ListPropertiesReply {
                        sequence,
                        length: atoms.len() as u32,
                        atoms,
                    },
                );
                Ok(())
            }
            Request::SendEvent(request) => self.send_event(sequence, request),
            Request::CreatePixmap(request) => self.create_pixmap(client, request),
            Request::FreePixmap(request) => {
                // NyaTerm: X11 引用计数语义——picture 引用 drawable。
                // GTK 的光标流程：CreatePixmap → CreatePicture → DestroyPixmap
                //（指望 picture 持有引用）→ CreateCursor → FreePicture。
                // 若 DestroyPixmap 把 picture 删了，CreateCursor 就 BadPicture。
                let referenced = self.pictures.values().any(|p| {
                    matches!(&p.source, super::state::PictureSource::Drawable(d) if *d == request.pixmap)
                });
                if referenced {
                    // picture 引用：pixmap 保持存活，picture 不删
                    return Ok(());
                }
                if self.pixmaps.remove(&request.pixmap).is_none() {
                    return Err(XError::bad_pixmap(request.pixmap));
                }
                // 没有 picture 引用：正常销毁
                self.pictures.retain(|_, picture| match picture.source {
                    super::state::PictureSource::Drawable(drawable) => drawable != request.pixmap,
                    super::state::PictureSource::Solid { .. } => true,
                    super::state::PictureSource::LinearGradient { .. }
                    | super::state::PictureSource::RadialGradient { .. }
                    | super::state::PictureSource::ConicalGradient { .. } => true,
                });
                Ok(())
            }
            Request::CreateGC(request) => self.create_gc(client, request),
            Request::ChangeGC(request) => {
                let aux = &request.value_list;
                let gc = self
                    .gcs
                    .get_mut(&request.gc)
                    .ok_or_else(|| XError::bad_gc(request.gc))?;
                if let Some(value) = aux.function {
                    gc.function = u32::from(value);
                }
                if let Some(value) = aux.plane_mask {
                    gc.plane_mask = value;
                }
                if let Some(value) = aux.foreground {
                    gc.foreground = value;
                }
                if let Some(value) = aux.background {
                    gc.background = Some(value);
                }
                if let Some(value) = aux.line_width {
                    gc.line_width = value;
                }
                if let Some(value) = aux.line_style {
                    gc.line_style = u32::from(value);
                }
                if let Some(value) = aux.fill_style {
                    gc.fill_style = u32::from(value);
                }
                if let Some(value) = aux.tile {
                    gc.tile = Some(value).filter(|id| *id != 0);
                }
                if let Some(value) = aux.stipple {
                    gc.stipple = Some(value).filter(|id| *id != 0);
                }
                if let Some(value) = aux.font {
                    gc.font = Some(value).filter(|id| *id != 0);
                }
                if let Some(value) = aux.graphics_exposures {
                    gc.graphics_exposures = value != 0;
                }
                if let Some(value) = aux.clip_x_origin {
                    gc.clip_x_origin = value;
                }
                if let Some(value) = aux.clip_y_origin {
                    gc.clip_y_origin = value;
                }
                if let Some(value) = aux.clip_mask {
                    // clip-mask 与裁剪矩形互斥：设置 mask 就清掉矩形列表
                    gc.clip_mask = Some(value).filter(|id| *id != 0);
                    gc.clip_rectangles.clear();
                }
                if let Some(value) = aux.dash_offset {
                    gc.dash_offset = value;
                }
                if let Some(value) = aux.arc_mode {
                    gc.arc_mode = u32::from(value);
                }
                Ok(())
            }
            Request::CopyGC(request) => {
                let source = self.gc(request.src_gc)?.clone();
                self.check_new_id(client, request.dst_gc)?;
                let mask = u32::from(request.value_mask);
                let mut gc = GcData::new(request.dst_gc, client, source.depth);
                if mask & u32::from(GC::FUNCTION) != 0 {
                    gc.function = source.function;
                }
                if mask & u32::from(GC::PLANE_MASK) != 0 {
                    gc.plane_mask = source.plane_mask;
                }
                if mask & u32::from(GC::FOREGROUND) != 0 {
                    gc.foreground = source.foreground;
                }
                if mask & u32::from(GC::BACKGROUND) != 0 {
                    gc.background = source.background;
                }
                if mask & u32::from(GC::LINE_WIDTH) != 0 {
                    gc.line_width = source.line_width;
                }
                if mask & u32::from(GC::LINE_STYLE) != 0 {
                    gc.line_style = source.line_style;
                }
                if mask & u32::from(GC::FILL_STYLE) != 0 {
                    gc.fill_style = source.fill_style;
                }
                if mask & u32::from(GC::TILE) != 0 {
                    gc.tile = source.tile;
                }
                if mask & u32::from(GC::STIPPLE) != 0 {
                    gc.stipple = source.stipple;
                }
                if mask & u32::from(GC::FONT) != 0 {
                    gc.font = source.font;
                }
                if mask & u32::from(GC::GRAPHICS_EXPOSURES) != 0 {
                    gc.graphics_exposures = source.graphics_exposures;
                }
                if mask & u32::from(GC::CLIP_ORIGIN_X) != 0 {
                    gc.clip_x_origin = source.clip_x_origin;
                }
                if mask & u32::from(GC::CLIP_ORIGIN_Y) != 0 {
                    gc.clip_y_origin = source.clip_y_origin;
                }
                if mask & u32::from(GC::CLIP_MASK) != 0 {
                    gc.clip_mask = source.clip_mask;
                    gc.clip_rectangles.clear();
                }
                if mask & u32::from(GC::DASH_OFFSET) != 0 {
                    gc.dash_offset = source.dash_offset;
                }
                if mask & u32::from(GC::ARC_MODE) != 0 {
                    gc.arc_mode = source.arc_mode;
                }
                self.gcs.insert(request.dst_gc, gc);
                Ok(())
            }
            Request::SetDashes(request) => {
                let gc = self
                    .gcs
                    .get_mut(&request.gc)
                    .ok_or_else(|| XError::bad_gc(request.gc))?;
                gc.dash_offset = u32::from(request.dash_offset);
                gc.dashes = request.dashes.to_vec();
                Ok(())
            }
            Request::SetClipRectangles(request) => {
                let gc = self
                    .gcs
                    .get_mut(&request.gc)
                    .ok_or_else(|| XError::bad_gc(request.gc))?;
                gc.clip_x_origin = i32::from(request.clip_x_origin);
                gc.clip_y_origin = i32::from(request.clip_y_origin);
                gc.clip_rectangles = request.rectangles.to_vec();
                // 设置裁剪矩形会让 clip-mask 失效
                gc.clip_mask = None;
                Ok(())
            }
            Request::FreeGC(request) => {
                if self.gcs.remove(&request.gc).is_none() {
                    return Err(XError::bad_gc(request.gc));
                }
                Ok(())
            }
            Request::ClearArea(request) => self.clear_area(sequence, request),
            Request::CopyArea(request) => self.copy_area(request),
            Request::PolyPoint(request) => self.poly_point(request),
            Request::PolyLine(request) => self.poly_line(request),
            Request::PolySegment(request) => self.poly_segment(request),
            Request::PolyRectangle(request) => self.poly_rectangle(request),
            Request::PolyFillRectangle(request) => self.poly_fill_rectangle(request),
            Request::PolyArc(request) => self.poly_arc(request),
            Request::PolyFillArc(request) => self.poly_fill_arc(request),
            Request::FillPoly(request) => self.fill_poly(request),
            Request::PutImage(request) => self.put_image(request),
            Request::ImageText8(request) => self.image_text8(request),
            Request::ImageText16(request) => self.image_text16(request),
            Request::PolyText8(request) => self.poly_text8(request),
            Request::PolyText16(request) => self.poly_text16(request),
            Request::OpenFont(request) => self.open_font(client, request),
            Request::CloseFont(request) => {
                if self.fonts.remove(&request.font).is_none() {
                    return Err(XError::bad_font(request.font));
                }
                Ok(())
            }
            Request::QueryFont(request) => self.query_font(client, sequence, request.font),
            Request::QueryTextExtents(request) => {
                self.query_text_extents(client, sequence, request)
            }
            Request::ListFonts(request) => self.list_fonts(client, sequence, request),
            Request::ListFontsWithInfo(request) => {
                self.list_fonts_with_info(client, sequence, request)
            }
            Request::SetFontPath(_) => Ok(()),
            Request::GetFontPath(_) => {
                self.send_reply(
                    client,
                    &GetFontPathReply {
                        sequence,
                        length: 0,
                        path: Vec::new(),
                    },
                );
                Ok(())
            }
            Request::GetImage(request) => self.get_image(client, sequence, request),
            Request::CreateColormap(request) => {
                self.check_new_id(client, request.mid)?;
                // NyaTerm: 握手时我们通告了**两个**视觉——depth24 的 ROOT_VISUAL
                // 与 depth32 的 ARGB32_VISUAL。GTK/firefox 为弹窗、半透明 surface
                // 建 colormap 时用的就是后者；只认 ROOT_VISUAL 会让它拿到
                // BadMatch 并直接退出（GDK 报 "BadMatch, request_code 78"，
                // 78 就是 CreateColormap）。CreateWindow 早就两个都认，这里漏了。
                if request.visual != ROOT_VISUAL && request.visual != ARGB32_VISUAL {
                    return Err(XError::bad_match());
                }
                // alloc 只允许 AllocNone / AllocAll（协议规定，否则 BadValue）
                if request.alloc != ColormapAlloc::NONE && request.alloc != ColormapAlloc::ALL {
                    return Err(XError::bad_value(u32::from(request.alloc)));
                }
                self.window(request.window)?;
                self.colormaps.insert(
                    request.mid,
                    ColormapData {
                        id: request.mid,
                        visual: request.visual,
                        owner: client,
                    },
                );
                Ok(())
            }
            Request::FreeColormap(request) => {
                if request.cmap == ROOT_COLORMAP || self.colormaps.remove(&request.cmap).is_none()
                {
                    return Err(XError::bad_colormap(request.cmap));
                }
                Ok(())
            }
            Request::AllocColor(request) => {
                if !self.colormaps.contains_key(&request.cmap) {
                    return Err(XError::bad_colormap(request.cmap));
                }
                let pixel = (u32::from(request.red >> 8) << 16)
                    | (u32::from(request.green >> 8) << 8)
                    | u32::from(request.blue >> 8);
                self.send_reply(
                    client,
                    &AllocColorReply {
                        sequence,
                        length: 0,
                        red: request.red,
                        green: request.green,
                        blue: request.blue,
                        pixel,
                    },
                );
                Ok(())
            }
            Request::QueryExtension(request) => {
                // 扩展注册表见 `extensions.rs`：按名字匹配固定 opcode；
                // 不认识的扩展（比如 RENDER）回 present=false，客户端走核心协议。
                let name = String::from_utf8_lossy(&request.name).into_owned();
                let (present, major_opcode, first_event, first_error) =
                    match extensions::query_extension(&name) {
                        Some((opcode, first_event, first_error)) => (true, opcode, first_event, first_error),
                        None => (false, 0, 0, 0),
                    };
                self.send_reply(
                    client,
                    &QueryExtensionReply {
                        sequence,
                        length: 0,
                        present,
                        major_opcode,
                        first_event,
                        first_error,
                    },
                );
                Ok(())
            }
            Request::ListExtensions(_) => {
                let names = extensions::extension_names();
                // length 按"超出 32 字节通用回复头"的 4 字节单位计。线上每个
                // STRING8 是 1 字节长度前缀 + 名字，**逐项不补齐**，整条回复
                // 只在末尾补齐一次（XCB 的 xcb_str_next 与 x11rb 的解析都是
                // 顺序前进，不做对齐——参照 Xorg 的实现）。
                let total: usize = names.iter().map(|name| name.len() + 1).sum();
                let names = names.into_iter().map(|name| Str { name }).collect();
                self.send_reply(
                    client,
                    &ListExtensionsReply {
                        sequence,
                        length: total.div_ceil(4) as u32,
                        names,
                    },
                );
                Ok(())
            }
            Request::GetInputFocus(_) => {
                // 焦点可能是具体窗口、PointerRoot(1) 或 None(0)，全部从状态里取。
                self.send_reply(
                    client,
                    &GetInputFocusReply {
                        revert_to: InputFocus::from(self.input.revert_to),
                        sequence,
                        length: 0,
                        focus: self.input.focus,
                    },
                );
                Ok(())
            }
            Request::SetInputFocus(request) => {
                // 应用（尤其 toolkit）在窗口被点下后会自己抢焦点；之前这里落到
                // 兜底的 BadRequest，GTK 打一行警告后键盘就彻底没反应。
                self.set_input_focus(request.focus, u8::from(request.revert_to));
                Ok(())
            }
            Request::GetKeyboardMapping(request) => {
                // keymap 必须和前端注入的 keysym 对应（见 `input.rs` 的 KEYMAP）：
                // 应用靠它把 keycode 反查成字符，全 0 的回复会让 xterm 之类
                // "能连上但一个字都打不出来"。
                let keysyms = super::input::keysym_table(request.first_keycode, request.count);
                self.send_reply(
                    client,
                    &GetKeyboardMappingReply {
                        keysyms_per_keycode: super::input::KEYSYMS_PER_KEYCODE,
                        sequence,
                        keysyms,
                    },
                );
                Ok(())
            }
            Request::QueryKeymap(_) => {
                let mut keys = [0u8; 32];
                for (keycode, &down) in self.input.keys_down.iter().enumerate() {
                    if down {
                        keys[keycode / 8] |= 1 << (keycode % 8);
                    }
                }
                self.send_reply(
                    client,
                    &QueryKeymapReply {
                        sequence,
                        length: 0,
                        keys,
                    },
                );
                Ok(())
            }
            Request::QueryPointer(request) => {
                let (root_x, root_y, _pw, mask) = self.pointer_snapshot();
                let (origin_x, origin_y) = self.window_origin(request.window);
                // NyaTerm: child 必须是"request.window 的**直接**子窗口且包含
                // 指针"，否则必须为 None(0)。X.org 语义（dix/events.c
                // ProcQueryPointer → QueryTree 语义）。
                //
                // 此前返回 `pointer_window`（全局最深窗口）：当被查询的窗口
                // 本身就包含指针时，child 会等于查询窗口自己或其祖先链上的
                // 深层窗口——GDK 的 window_at_position() 用 child 做**下降
                // 迭代**（child==None 才停），收到"自己"就永远下降不到底 →
                // 主线程卡死在单个 GDK 调用里（QueryPointer 330 次/秒的
                // 轮询洪水 + 停止绘制 = "操作一会就卡死"）。
                let child = self.child_at(request.window, root_x, root_y);
                self.send_reply(
                    client,
                    &QueryPointerReply {
                        same_screen: true,
                        sequence,
                        length: 0,
                        root: ROOT_WINDOW,
                        child,
                        root_x,
                        root_y,
                        win_x: (i32::from(root_x) - origin_x) as i16,
                        win_y: (i32::from(root_y) - origin_y) as i16,
                        mask: KeyButMask::from(mask),
                    },
                );
                Ok(())
            }
            Request::NoOperation(_) => Ok(()),

            // ---- XInput2（XI2）：让 GDK3 用事件推送而非指针轮询 ----
            Request::XinputXIQueryVersion(request) => self.xi2_query_version(client, sequence, request),
            Request::XinputGetExtensionVersion(request) => self.xinput_get_extension_version(client, sequence, request),
            Request::XinputXIQueryDevice(request) => self.xi2_query_device(client, sequence, request),
            Request::XinputXISelectEvents(request) => self.xi2_select_events(client, request),
            Request::XinputXIQueryPointer(request) => self.xi2_query_pointer(client, sequence, request),
            Request::XinputXIGrabDevice(request) => self.xi2_grab_device(client, sequence, request),
            Request::XinputXIUngrabDevice(request) => self.xi2_ungrab_device(request),
            Request::XinputXIAllowEvents(request) => {
                self.xi2_allow_events(request);
                Ok(())
            }
            Request::XinputXIGetClientPointer(request) => {
                self.xi2_get_client_pointer(client, sequence, request)
            }
            Request::XinputXIChangeCursor(request) => self.xi2_change_cursor(request),
            Request::XinputXIWarpPointer(request) => self.xi2_warp_pointer(request),
            Request::XinputXIListProperties(request) => {
                self.xi2_list_properties(client, sequence, request)
            }
            Request::XinputXIGetProperty(request) => {
                self.xi2_get_property(client, sequence, request)
            }

            // ---- 颜色（Xt/Xaw 解析颜色名会走这两条）----
            Request::LookupColor(request) => self.lookup_color(client, sequence, request),
            Request::AllocNamedColor(request) => self.alloc_named_color(client, sequence, request),
            Request::QueryColors(request) => self.query_colors(client, sequence, request),
            Request::StoreColors(request) => self.touch_colormap(request.cmap),
            Request::FreeColors(request) => self.touch_colormap(request.cmap),
            Request::StoreNamedColor(request) => {
                self.touch_colormap(request.cmap)?;
                self.named_color_known(&request.name)
            }
            // TrueColor 上没有可分配的颜色单元：按协议回 BadMatch 让客户端走
            // "直接用像素值"的降级路径（Xlib 对这两条请求的失败是静默返回 0）。
            Request::AllocColorCells(_) | Request::AllocColorPlanes(_) => Err(XError::bad_match()),
            Request::InstallColormap(request) => self.install_colormap(request.cmap),
            Request::UninstallColormap(request) => self.install_colormap(request.cmap),
            Request::ListInstalledColormaps(request) => {
                self.list_installed_colormaps(client, sequence, request)
            }
            Request::CopyColormapAndFree(request) => self.copy_colormap_and_free(client, request),

            // ---- 光标（`XCreateFontCursor` = OpenFont("cursor") + CreateGlyphCursor）----
            Request::CreateCursor(request) => self.create_cursor(client, request),
            Request::CreateGlyphCursor(request) => self.create_glyph_cursor(client, request),
            Request::FreeCursor(request) => self.free_cursor(request.cursor),
            Request::RecolorCursor(request) => self.recolor_cursor(request.cursor),
            Request::QueryBestSize(request) => self.query_best_size(client, sequence, request),

            // ---- 抓取：接受，但不真正冻结事件流（见 misc.rs 的说明）----
            Request::GrabServer(_) | Request::UngrabServer(_) => Ok(()),
            Request::GrabPointer(request) => self.grab_pointer(client, sequence, request),
            Request::GrabKeyboard(request) => self.grab_keyboard(client, sequence, request),
            Request::UngrabPointer(_) => {
                self.ungrab_pointer();
                Ok(())
            }
            Request::UngrabKeyboard(_) => {
                self.ungrab_keyboard();
                Ok(())
            }
            Request::ChangeActivePointerGrab(request) => {
                self.change_active_pointer_grab(request);
                Ok(())
            }
            Request::GrabButton(_)
            | Request::UngrabButton(_)
            | Request::GrabKey(_)
            | Request::UngrabKey(_)
            | Request::AllowEvents(_) => Ok(()),

            // ---- 选择区（剪贴板）：先只维护属主与"拿不到数据"的通知 ----
            Request::SetSelectionOwner(request) => self.set_selection_owner(request),
            Request::GetSelectionOwner(request) => {
                self.get_selection_owner(client, sequence, request)
            }
            Request::ConvertSelection(request) => self.convert_selection(request),

            // ---- 窗口 / 坐标 / 指针 ----
            Request::ChangeSaveSet(request) => self.window(request.window).map(|_| ()),
            Request::CirculateWindow(_) => Ok(()),
            Request::TranslateCoordinates(request) => {
                self.translate_coordinates(client, sequence, request)
            }
            Request::WarpPointer(request) => self.warp_pointer(request),
            Request::GetMotionEvents(request) => self.get_motion_events(client, sequence, request),

            // ---- 键盘 / 指针 / 屏保参数 ----
            Request::Bell(_) | Request::SetScreenSaver(_) | Request::ForceScreenSaver(_) => Ok(()),
            Request::ChangeKeyboardMapping(_)
            | Request::ChangeKeyboardControl(_)
            | Request::ChangePointerControl(_)
            | Request::SetAccessControl(_)
            | Request::SetCloseDownMode(_)
            | Request::ChangeHosts(_)
            | Request::RotateProperties(_)
            | Request::KillClient(_) => Ok(()),
            Request::GetKeyboardControl(_) => self.get_keyboard_control(client, sequence),
            Request::GetPointerControl(_) => self.get_pointer_control(client, sequence),
            Request::GetScreenSaver(_) => self.get_screen_saver(client, sequence),
            Request::GetPointerMapping(_) => self.get_pointer_mapping(client, sequence),
            Request::SetPointerMapping(_) => self.set_pointer_mapping(client, sequence),
            Request::GetModifierMapping(_) => self.get_modifier_mapping(client, sequence),
            Request::SetModifierMapping(_) => self.set_modifier_mapping(client, sequence),
            Request::ListHosts(_) => self.list_hosts(client, sequence),

            // ---- 扩展请求：语义在 `extensions.rs`，这里是纯分发 ----
            Request::BigreqEnable(_) => {
                // BIG-REQUESTS：回复最大请求长度（16 MiB，4 字节单位），
                // 连接循环看到这条请求成功后开始接受 32 位长度的分帧
                self.send_reply(
                    client,
                    &bigreq::EnableReply {
                        sequence,
                        length: 0,
                        maximum_request_length: extensions::MAX_BIG_REQUEST_UNITS,
                    },
                );
                Ok(())
            }
            Request::XcMiscGetVersion(_) => {
                self.send_reply(
                    client,
                    &xc_misc::GetVersionReply {
                        sequence,
                        length: 0,
                        server_major_version: 1,
                        server_minor_version: 1,
                    },
                );
                Ok(())
            }
            Request::XcMiscGetXIDRange(_) => self.xc_misc_get_xid_range(client, sequence),
            Request::XcMiscGetXIDList(request) => {
                self.xc_misc_get_xid_list(client, sequence, request.count)
            }
            Request::XtestGetVersion(_) => self.xtest_get_version(client, sequence),
            Request::XtestFakeInput(request) => self.xtest_fake_input(request),
            // CompareCursor / GrabControl：罕见，先明确拒绝（不做假成功，客户端会当真）
            Request::XtestCompareCursor(_) | Request::XtestGrabControl(_) => {
                Err(XError::bad_match())
            }
            Request::ShmQueryVersion(_) => self.shm_query_version(client, sequence),
            Request::ShmAttach(request) => self.shm_attach(request),
            Request::ShmDetach(request) => self.shm_detach(request),
            // Attach 已经失败，这些 shm 绘制请求正常流程不会来；来了就按未实现拒绝
            Request::ShmPutImage(_)
            | Request::ShmGetImage(_)
            | Request::ShmCreatePixmap(_)
            | Request::ShmAttachFd(_)
            | Request::ShmCreateSegment(_) => Err(XError::bad_match()),
            Request::ShapeQueryVersion(_) => self.shape_query_version(client, sequence),
            Request::ShapeRectangles(request) => self.shape_rectangles(request),
            Request::ShapeMask(request) => self.shape_mask(request),
            Request::ShapeCombine(request) => self.shape_combine(request),
            Request::ShapeOffset(request) => self.shape_offset(request),
            Request::ShapeQueryExtents(request) => {
                self.shape_query_extents(client, sequence, request.destination_window)
            }
            Request::ShapeSelectInput(request) => self.shape_select_input(client, request),
            Request::ShapeInputSelected(_) => self.shape_input_selected(client, sequence),
            Request::ShapeGetRectangles(request) => self.shape_get_rectangles(client, sequence, request),
            Request::XfixesQueryVersion(request) => {
                self.xfixes_query_version(client, sequence, request)
            }
            Request::XfixesChangeSaveSet(request) => self.xfixes_change_save_set(request),
            // SaveSet / 选光标与选选择区事件：接受但不产生事件（CursorImage 相关
            // 的查询真做不了，回 BadMatch，客户端有回退路径）
            Request::XfixesSelectSelectionInput(_)
            | Request::XfixesSelectCursorInput(_)
            | Request::XfixesHideCursor(_)
            | Request::XfixesShowCursor(_) => Ok(()),
            Request::XfixesCreateRegion(request) => self.xfixes_create_region(client, request),
            Request::XfixesSetRegion(request) => self.xfixes_set_region(client, request),
            Request::XfixesDestroyRegion(request) => self.xfixes_destroy_region(request),
            Request::XfixesCopyRegion(request) => self.xfixes_copy_region(client, request),
            Request::XfixesUnionRegion(request) => self.xfixes_union_region(client, request),
            Request::XfixesIntersectRegion(request) => {
                self.xfixes_intersect_region(client, request)
            }
                        Request::XfixesSubtractRegion(request) => self.xfixes_subtract_region(client, request),
            // NyaTerm: Xfixes SetCursorName（GTK/firefox 设置光标名/tooltip）。
            // 校验光标存在即可——画面不渲染光标，名字不需要存储。
            Request::XfixesSetCursorName(request) => self.cursor(u32::from(request.cursor)),
            Request::XfixesInvertRegion(request) => self.xfixes_invert_region(client, request),
            Request::XfixesTranslateRegion(request) => self.xfixes_translate_region(request),
            Request::XfixesRegionExtents(request) => self.xfixes_region_extents(client, request),
            Request::XfixesFetchRegion(request) => self.xfixes_fetch_region(client, sequence, request),
            Request::CompositeQueryVersion(_) => self.composite_query_version(client, sequence),
            Request::CompositeRedirectWindow(request) => self.composite_redirect(request),
            Request::CompositeRedirectSubwindows(request) => {
                self.window(request.window).map(|_| ())
            }
            Request::CompositeUnredirectWindow(request) => self.composite_unredirect(request),
            Request::CompositeUnredirectSubwindows(request) => {
                self.window(request.window).map(|_| ())
            }
            Request::CompositeCreateRegionFromBorderClip(request) => {
                self.composite_create_region_from_border_clip(client, request)
            }
            Request::CompositeReleaseOverlayWindow(_) => Ok(()),
            // Overlay 窗口 / NameWindowPixmap 需要真实的重定向合成，最小化阶段拒绝
            Request::CompositeGetOverlayWindow(_) | Request::CompositeNameWindowPixmap(_) => {
                Err(XError::bad_match())
            }
            Request::DamageQueryVersion(_) => self.damage_query_version(client, sequence),
            Request::DamageCreate(request) => self.damage_create(client, request),
            Request::DamageDestroy(request) => self.damage_destroy(request),
            Request::DamageSubtract(request) => self.damage_subtract(request),
            Request::DamageAdd(request) => self.damage_add(request),

            // ---- RENDER（语义在 render.rs）----
            Request::RenderQueryVersion(_) => self.render_query_version(client, sequence),
            Request::RenderQueryPictFormats(_) => {
                self.render_query_pict_formats(client, sequence)
            }
            Request::RenderQueryPictIndexValues(request) => {
                self.render_query_pict_index_values(client, sequence, request)
            }
            Request::RenderQueryFilters(_) => self.render_query_filters(client, sequence),
            Request::RenderCreatePicture(request) => self.render_create_picture(client, request),
            Request::RenderChangePicture(request) => self.render_change_picture(request),
            Request::RenderSetPictureClipRectangles(request) => {
                self.render_set_picture_clip_rectangles(request)
            }
            Request::RenderFreePicture(request) => self.render_free_picture(request),
            Request::RenderSetPictureTransform(request) => self.render_set_picture_transform(request),
            Request::RenderSetPictureFilter(request) => self.render_set_picture_filter(request),
            Request::RenderCreateSolidFill(request) => self.render_create_solid_fill(client, request),
            Request::RenderFillRectangles(request) => self.render_fill_rectangles(request),
            Request::RenderComposite(request) => self.render_composite(request),
            Request::RenderCreateGlyphSet(request) => self.render_create_glyph_set(client, request),
            Request::RenderReferenceGlyphSet(request) => {
                self.render_reference_glyph_set(client, request)
            }
            Request::RenderFreeGlyphSet(request) => self.render_free_glyph_set(request),
            Request::RenderAddGlyphs(request) => self.render_add_glyphs(request),
            Request::RenderFreeGlyphs(request) => self.render_free_glyphs(request),
            Request::RenderCompositeGlyphs8(request) => self.render_composite_glyphs8(request),
            Request::RenderCompositeGlyphs16(request) => self.render_composite_glyphs16(request),
            Request::RenderCompositeGlyphs32(request) => self.render_composite_glyphs32(request),
            Request::RenderTrapezoids(request) => self.render_trapezoids(request),
            Request::RenderTriangles(request) => self.render_triangles(request),
            Request::RenderTriStrip(request) => self.render_tri_strip(request),
            Request::RenderTriFan(request) => self.render_tri_fan(request),
            // Gradients / AddTraps / CreateCursor / CreateAnimCursor 未实现，
            // 明确拒绝（toolkit 有回退路径）
            // NyaTerm: 渐变是 firefox/GTK 渲染的主路径之一，BadImplementation
            // 会让客户端直接 abort——三种渐变都已按 RENDER 语义实现。
            Request::RenderCreateLinearGradient(request) => {
                self.render_create_linear_gradient(client, request)
            }
            Request::RenderCreateRadialGradient(request) => {
                self.render_create_radial_gradient(client, request)
            }
            Request::RenderCreateConicalGradient(request) => {
                self.render_create_conical_gradient(client, request)
            }
            Request::RenderAddTraps(_) | Request::RenderCreateAnimCursor(_) => {
                self.render_gradient_rejected()
            }
            // NyaTerm: RENDER CreateCursor（minor 27）必须实现——firefox 的
            // 光标管理用它创建图标光标，BadImplementation 会让客户端 abort。
            // 画面不渲染光标内容：登记 ID 与属主即可，source picture 校验存在。
            Request::RenderCreateCursor(request) => {
                self.check_new_id(client, u32::from(request.cid))?;
                let source = u32::from(request.source);
                let _ = self.picture(source)?;
                // 光标持有 picture 引用（X11 引用计数语义）：FreePicture
                // 不能删掉被光标引用的 picture，否则 firefox 建第二个
                // cursor 时会拿到 BadPicture。
                self.cursors.insert(
                    u32::from(request.cid),
                    super::state::CursorRef { owner: client, picture: Some(source) },
                );
                Ok(())
            }

            // ---- XInput 兜底：未逐一实现的 XI/XI2 请求回最小合法应答 ----
            // （GDK3 启动探测会覆盖大量请求；任何 BadRequest 都是未捕获
            //   错误直接杀 firefox。空应答 = 空列表/零字段，语义可降级。）
            Request::XinputGetDeviceButtonMapping(request) => {
                // 需要应答的请求：32 字节空应答（length=0 = 空列表/零字段）
                let mut bytes = Vec::with_capacity(32);
                bytes.push(1); // Reply
                bytes.extend_from_slice(&[0u8; 31]);
                bytes[2..4].copy_from_slice(&sequence.to_ne_bytes());
                self.send(client, &bytes);
                Ok(())
            }
            Request::XinputGetDeviceControl(request) => {
                // 需要应答的请求：32 字节空应答（length=0 = 空列表/零字段）
                let mut bytes = Vec::with_capacity(32);
                bytes.push(1); // Reply
                bytes.extend_from_slice(&[0u8; 31]);
                bytes[2..4].copy_from_slice(&sequence.to_ne_bytes());
                self.send(client, &bytes);
                Ok(())
            }
            Request::XinputGetDeviceDontPropagateList(request) => {
                // 需要应答的请求：32 字节空应答（length=0 = 空列表/零字段）
                let mut bytes = Vec::with_capacity(32);
                bytes.push(1); // Reply
                bytes.extend_from_slice(&[0u8; 31]);
                bytes[2..4].copy_from_slice(&sequence.to_ne_bytes());
                self.send(client, &bytes);
                Ok(())
            }
            Request::XinputGetDeviceFocus(request) => {
                // 需要应答的请求：32 字节空应答（length=0 = 空列表/零字段）
                let mut bytes = Vec::with_capacity(32);
                bytes.push(1); // Reply
                bytes.extend_from_slice(&[0u8; 31]);
                bytes[2..4].copy_from_slice(&sequence.to_ne_bytes());
                self.send(client, &bytes);
                Ok(())
            }
            Request::XinputGetDeviceKeyMapping(request) => {
                // 需要应答的请求：32 字节空应答（length=0 = 空列表/零字段）
                let mut bytes = Vec::with_capacity(32);
                bytes.push(1); // Reply
                bytes.extend_from_slice(&[0u8; 31]);
                bytes[2..4].copy_from_slice(&sequence.to_ne_bytes());
                self.send(client, &bytes);
                Ok(())
            }
            Request::XinputGetDeviceModifierMapping(request) => {
                // 需要应答的请求：32 字节空应答（length=0 = 空列表/零字段）
                let mut bytes = Vec::with_capacity(32);
                bytes.push(1); // Reply
                bytes.extend_from_slice(&[0u8; 31]);
                bytes[2..4].copy_from_slice(&sequence.to_ne_bytes());
                self.send(client, &bytes);
                Ok(())
            }
            Request::XinputGetDeviceMotionEvents(request) => {
                // 需要应答的请求：32 字节空应答（length=0 = 空列表/零字段）
                let mut bytes = Vec::with_capacity(32);
                bytes.push(1); // Reply
                bytes.extend_from_slice(&[0u8; 31]);
                bytes[2..4].copy_from_slice(&sequence.to_ne_bytes());
                self.send(client, &bytes);
                Ok(())
            }
            Request::XinputGetDeviceProperty(request) => {
                // 需要应答的请求：32 字节空应答（length=0 = 空列表/零字段）
                let mut bytes = Vec::with_capacity(32);
                bytes.push(1); // Reply
                bytes.extend_from_slice(&[0u8; 31]);
                bytes[2..4].copy_from_slice(&sequence.to_ne_bytes());
                self.send(client, &bytes);
                Ok(())
            }
            Request::XinputGetFeedbackControl(request) => {
                // 需要应答的请求：32 字节空应答（length=0 = 空列表/零字段）
                let mut bytes = Vec::with_capacity(32);
                bytes.push(1); // Reply
                bytes.extend_from_slice(&[0u8; 31]);
                bytes[2..4].copy_from_slice(&sequence.to_ne_bytes());
                self.send(client, &bytes);
                Ok(())
            }
            Request::XinputGetSelectedExtensionEvents(request) => {
                // 需要应答的请求：32 字节空应答（length=0 = 空列表/零字段）
                let mut bytes = Vec::with_capacity(32);
                bytes.push(1); // Reply
                bytes.extend_from_slice(&[0u8; 31]);
                bytes[2..4].copy_from_slice(&sequence.to_ne_bytes());
                self.send(client, &bytes);
                Ok(())
            }
            Request::XinputQueryDeviceState(request) => {
                // 需要应答的请求：32 字节空应答（length=0 = 空列表/零字段）
                let mut bytes = Vec::with_capacity(32);
                bytes.push(1); // Reply
                bytes.extend_from_slice(&[0u8; 31]);
                bytes[2..4].copy_from_slice(&sequence.to_ne_bytes());
                self.send(client, &bytes);
                Ok(())
            }
            Request::XinputListInputDevices(request) => {
                // 需要应答的请求：32 字节空应答（length=0 = 空列表/零字段）
                let mut bytes = Vec::with_capacity(32);
                bytes.push(1); // Reply
                bytes.extend_from_slice(&[0u8; 31]);
                bytes[2..4].copy_from_slice(&sequence.to_ne_bytes());
                self.send(client, &bytes);
                Ok(())
            }
            Request::XinputListDeviceProperties(request) => {
                // 需要应答的请求：32 字节空应答（length=0 = 空列表/零字段）
                let mut bytes = Vec::with_capacity(32);
                bytes.push(1); // Reply
                bytes.extend_from_slice(&[0u8; 31]);
                bytes[2..4].copy_from_slice(&sequence.to_ne_bytes());
                self.send(client, &bytes);
                Ok(())
            }
            Request::XinputOpenDevice(request) => {
                // 需要应答的请求：32 字节空应答（length=0 = 空列表/零字段）
                let mut bytes = Vec::with_capacity(32);
                bytes.push(1); // Reply
                bytes.extend_from_slice(&[0u8; 31]);
                bytes[2..4].copy_from_slice(&sequence.to_ne_bytes());
                self.send(client, &bytes);
                Ok(())
            }
            Request::XinputXIGetFocus(request) => {
                // 需要应答的请求：32 字节空应答（length=0 = 空列表/零字段）
                let mut bytes = Vec::with_capacity(32);
                bytes.push(1); // Reply
                bytes.extend_from_slice(&[0u8; 31]);
                bytes[2..4].copy_from_slice(&sequence.to_ne_bytes());
                self.send(client, &bytes);
                Ok(())
            }
            Request::XinputXIGetSelectedEvents(request) => {
                // 需要应答的请求：32 字节空应答（length=0 = 空列表/零字段）
                let mut bytes = Vec::with_capacity(32);
                bytes.push(1); // Reply
                bytes.extend_from_slice(&[0u8; 31]);
                bytes[2..4].copy_from_slice(&sequence.to_ne_bytes());
                self.send(client, &bytes);
                Ok(())
            }
            Request::XinputXIPassiveGrabDevice(request) => {
                // 需要应答的请求：32 字节空应答（length=0 = 空列表/零字段）
                let mut bytes = Vec::with_capacity(32);
                bytes.push(1); // Reply
                bytes.extend_from_slice(&[0u8; 31]);
                bytes[2..4].copy_from_slice(&sequence.to_ne_bytes());
                self.send(client, &bytes);
                Ok(())
            }
            Request::XinputAllowDeviceEvents(_) => Ok(()),
            Request::XinputChangeDeviceControl(_) => Ok(()),
            Request::XinputChangeDeviceDontPropagateList(_) => Ok(()),
            Request::XinputChangeDeviceKeyMapping(_) => Ok(()),
            Request::XinputChangeDeviceProperty(_) => Ok(()),
            Request::XinputChangeFeedbackControl(_) => Ok(()),
            Request::XinputChangeKeyboardDevice(_) => Ok(()),
            Request::XinputChangePointerDevice(_) => Ok(()),
            Request::XinputCloseDevice(_) => Ok(()),
            Request::XinputDeviceBell(_) => Ok(()),
            Request::XinputDeleteDeviceProperty(_) => Ok(()),
            Request::XinputGrabDeviceButton(_) => Ok(()),
            Request::XinputGrabDeviceKey(_) => Ok(()),
            Request::XinputSelectExtensionEvent(_) => Ok(()),
            Request::XinputSendExtensionEvent(_) => Ok(()),
            Request::XinputSetDeviceButtonMapping(_) => Ok(()),
            Request::XinputSetDeviceFocus(_) => Ok(()),
            Request::XinputSetDeviceMode(_) => Ok(()),
            Request::XinputSetDeviceModifierMapping(_) => Ok(()),
            Request::XinputSetDeviceValuators(_) => Ok(()),
            Request::XinputUngrabDeviceButton(_) => Ok(()),
            Request::XinputUngrabDeviceKey(_) => Ok(()),
            Request::XinputXIBarrierReleasePointer(_) => Ok(()),
            Request::XinputXIChangeHierarchy(_) => Ok(()),
            Request::XinputXIChangeProperty(_) => Ok(()),
            Request::XinputXIDeleteProperty(_) => Ok(()),
            Request::XinputXIPassiveUngrabDevice(_) => Ok(()),
            Request::XinputXISetClientPointer(_) => Ok(()),
            Request::XinputXISetFocus(_) => Ok(()),
            Request::XinputUngrabDevice(_) => Ok(()),

            // ---- XKEYBOARD：语义在 `xkb.rs`（GTK3 use_xkb 开关 + 键表）----
            // 有应答的给最小合法应答（走 send_reply，length 自动回填）；
            // **void 的一律静默**——多回 32 字节会造成应答流错位。
            Request::XkbUseExtension(request) => self.xkb_use_extension(client, sequence, request),
            Request::XkbGetState(request) => self.xkb_get_state(client, sequence, request),
            Request::XkbGetControls(request) => self.xkb_get_controls(client, sequence, request),
            Request::XkbGetMap(request) => self.xkb_get_map(client, sequence, request),
            Request::XkbGetNames(request) => self.xkb_get_names(client, sequence, request),
            Request::XkbGetIndicatorState(request) => {
                self.xkb_get_indicator_state(client, sequence, request)
            }
            Request::XkbGetCompatMap(request) => self.xkb_get_compat_map(client, sequence, request),
            Request::XkbGetIndicatorMap(request) => {
                self.xkb_get_indicator_map(client, sequence, request)
            }
            Request::XkbGetNamedIndicator(request) => {
                self.xkb_get_named_indicator(client, sequence, request)
            }
            Request::XkbPerClientFlags(request) => {
                self.xkb_per_client_flags(client, sequence, request)
            }
            Request::XkbListComponents(request) => {
                self.xkb_list_components(client, sequence, request)
            }
            Request::XkbGetKbdByName(request) => self.xkb_get_kbd_by_name(client, sequence, request),
            Request::XkbGetDeviceInfo(request) => self.xkb_get_device_info(client, sequence, request),
            Request::XkbSetDebuggingFlags(request) => {
                self.xkb_set_debugging_flags(client, sequence, request)
            }
            // void：SelectEvents / Bell / LatchLockState / SetControls / SetMap /
            // SetCompatMap / SetIndicatorMap / SetNamedIndicator / SetNames /
            // SetGeometry / SetDeviceInfo
            Request::XkbSelectEvents(_) => Ok(()),
            Request::XkbBell(_) => Ok(()),
            Request::XkbLatchLockState(_) => Ok(()),
            Request::XkbSetControls(_) => Ok(()),
            Request::XkbSetMap(_) => Ok(()),
            Request::XkbSetCompatMap(_) => Ok(()),
            Request::XkbSetIndicatorMap(_) => Ok(()),
            Request::XkbSetNamedIndicator(_) => Ok(()),
            Request::XkbSetNames(_) => Ok(()),
            Request::XkbSetDeviceInfo(_) => Ok(()),
            _ => Err(XError::bad_request()),
        };

        // NyaTerm diagnostic (廉价、只在失败时触发)：把请求本身记下来。
        // 未实现的请求被客户端循环重试时，这里是唯一的线索——错误码本身
        // 只说明"失败了"，看不出是哪个请求。
        if let Err(error) = &result {
            // NyaTerm: 带客户端 ID——子进程崩溃归因（"Channel closing" 时
            // 看死掉的连接最后收到了什么错误）。
            let debug = format!("{request:?}");
            let debug: String = debug.chars().take(200).collect();
            crate::xsrv::log_line(format_args!(
                "[x11] c{client} request failed: {debug} -> {error:?}"
            ));
        }

        result
    }

    fn get_window_attributes(
        &self,
        client: u32,
        sequence: u16,
        window: Window,
    ) -> Result<(), XError> {
        // NyaTerm: window 0 = X 的 None。GTK3 的 xsettings 探测缺陷
        // (check_manager_window 不判 None) 曾让 win0 查询链炸掉子进程——根治
        // 方案是服务端持有 _XSETTINGS_S0（见 state.rs setup_xsettings_manager），
        // GTK 不再查 win0。这里回归 X.org 真实语义：BadWindow。
        if window == 0 {
            return Err(XError::bad_window(0));
        }
        let data = self.window(window)?;
        let all_event_masks = data
            .event_masks
            .values()
            .fold(0u32, |all, mask| all | *mask);
        let reply = GetWindowAttributesReply {
            backing_store: BackingStore::NOT_USEFUL,
            sequence,
            length: 0,
            visual: data.visual,
            class: data.class,
            bit_gravity: data.bit_gravity,
            win_gravity: data.win_gravity,
            backing_planes: 0xffff_ffff,
            backing_pixel: 0,
            save_under: false,
            map_is_installed: true,
            map_state: self.map_state(window),
            override_redirect: data.override_redirect,
            colormap: data.colormap,
            all_event_masks: EventMask::from(all_event_masks),
            your_event_mask: EventMask::from(data.event_mask(client)),
            do_not_propagate_mask: EventMask::from(data.do_not_propagate),
        };
        self.send_reply(client, &reply);
        Ok(())
    }

    fn change_window_attributes(
        &mut self,
        client: u32,
        request: &ChangeWindowAttributesRequest<'_>,
    ) -> Result<(), XError> {
        // NyaTerm: window 0 = None。注意：这里必须回 BadWindow——15:14 版曾改为
        // Ok(())，结果 GTK 认定探测目标真实存在、走进了不受支持的渲染分支，
        // 所有客户端停止绘制（画面冻结）。回错误虽然有子进程死掉，但其余
        // 客户端走普通路径，绘制正常。
        if request.window == 0 {
            return Err(XError::bad_window(0));
        }
        let aux = &request.value_list;
        if let Some(colormap) = aux.colormap {
            if colormap != ROOT_COLORMAP && !self.colormaps.contains_key(&colormap) {
                return Err(XError::bad_colormap(colormap));
            }
        }
        {
            let window = self.window_mut(request.window)?;
            if let Some(mask) = aux.event_mask {
                window.event_masks.insert(client, u32::from(mask));
            }
            if let Some(mask) = aux.do_not_propogate_mask {
                window.do_not_propagate = u32::from(mask);
            }
            if let Some(pixel) = aux.background_pixel {
                window.background_pixel = pixel;
            }
            if let Some(pixel) = aux.border_pixel {
                window.border_pixel = pixel;
            }
            if let Some(value) = aux.override_redirect {
                window.override_redirect = value != 0;
            }
            if let Some(value) = aux.bit_gravity {
                window.bit_gravity = value;
            }
            if let Some(value) = aux.win_gravity {
                window.win_gravity = value;
            }
            if let Some(value) = aux.colormap {
                window.colormap = value;
            }
        }
        self.mark_compose_dirty();
        Ok(())
    }

    fn get_geometry(
        &self,
        client: u32,
        sequence: u16,
        drawable: Drawable,
    ) -> Result<(), XError> {
        // NyaTerm: drawable 0 = None → BadDrawable（X.org 语义，见
        // get_window_attributes 注释的根治说明）。
        let reply = if drawable == 0 {
            return Err(XError::bad_drawable(0));
        } else if let Some(window) = self.windows.get(&drawable) {
            GetGeometryReply {
                depth: window.depth,
                sequence,
                length: 0,
                root: ROOT_WINDOW,
                x: if drawable == ROOT_WINDOW { 0 } else { window.x },
                y: if drawable == ROOT_WINDOW { 0 } else { window.y },
                width: window.width,
                height: window.height,
                border_width: window.border_width,
            }
        } else if let Some(pixmap) = self.pixmaps.get(&drawable) {
            GetGeometryReply {
                depth: pixmap.depth,
                sequence,
                length: 0,
                root: 0,
                x: 0,
                y: 0,
                width: pixmap.width,
                height: pixmap.height,
                border_width: 0,
            }
        } else {
            return Err(XError::bad_drawable(drawable));
        };
        self.send_reply(client, &reply);
        Ok(())
    }

    fn query_tree(&self, client: u32, sequence: u16, window: Window) -> Result<(), XError> {
        // NyaTerm: window 0 = None → BadWindow（X.org 语义，见
        // get_window_attributes 注释的根治说明）。
        if window == 0 {
            return Err(XError::bad_window(0));
        }
        let data = self.window(window)?;
        let children = data.children.clone();
        let reply = QueryTreeReply {
            sequence,
            length: children.len() as u32,
            root: ROOT_WINDOW,
            parent: data.parent,
            children,
        };
        self.send_reply(client, &reply);
        Ok(())
    }

    fn change_property(
        &mut self,
        client: u32,
        sequence: u16,
        request: &ChangePropertyRequest<'_>,
    ) -> Result<(), XError> {
        if !matches!(request.format, 8 | 16 | 32) {
            return Err(XError::bad_value(u32::from(request.format)));
        }
        let unit = usize::from(request.format / 8);
        let expected = request.data_len as usize * unit;
        if request.data.len() < expected {
            return Err(XError::bad_length());
        }
        let data = request.data[..expected].to_vec();
        let mode = u8::from(request.mode);
        // NyaTerm 探针：谁在设置 root 属性（_NET_SUPPORTING_WM_CHECK 污染源定位）
        if request.window == ROOT_WINDOW {
            let name = self
                .atoms
                .get(request.property as usize)
                .and_then(|o| o.as_deref())
                .unwrap_or("?");
            crate::xsrv::log_line(format_args!(
                "[x11] c{client} seq{sequence} ChangeProperty(root, {name}) mode={mode} first={:?}",
                &data[..data.len().min(8)]
            ));
        }

        {
            // NyaTerm diagnostic: a BadWindow on ChangeProperty (firefox
            // startup) needs the requested id and the live window set to be
            // actionable — the wire parser is generated code, so the id is
            // trustworthy and the interesting part is *why* it is unknown.
            if self.windows.get(&request.window).is_none() {
                let mut known = Vec::new();
                for (id, data) in &self.windows {
                    known.push(format!("{id:#x}(owner={})", data.owner));
                }
                known.sort();
                known.truncate(24);
                crate::xsrv::log_line(format_args!(
                    "[x11] ChangeProperty BadWindow win={:#x} prop={:#x} type={:#x} fmt={} mode={} len={} known=[{}]",
                    request.window,
                    request.property,
                    u32::from(request.type_),
                    request.format,
                    mode,
                    request.data_len,
                    known.join(" ")
                ));
                return Err(XError::bad_window(request.window));
            }

            let window = self.window_mut(request.window)?;
            let existing = window.properties.get_mut(&request.property);
            match mode {
                0 => {
                    // Replace：直接覆盖
                    window.properties.insert(
                        request.property,
                        Property {
                            type_: request.type_,
                            format: request.format,
                            data,
                        },
                    );
                }
                1 | 2 => {
                    // Prepend / Append：类型与格式必须与已有属性一致
                    if let Some(existing) = existing {
                        if existing.format != request.format || existing.type_ != request.type_ {
                            return Err(XError::bad_match());
                        }
                        if mode == 1 {
                            let mut combined = data;
                            combined.extend_from_slice(&existing.data);
                            existing.data = combined;
                        } else {
                            existing.data.extend_from_slice(&data);
                        }
                    } else {
                        window.properties.insert(
                            request.property,
                            Property {
                                type_: request.type_,
                                format: request.format,
                                data,
                            },
                        );
                    }
                }
                other => return Err(XError::bad_value(u32::from(other))),
            }
        }

        let _ = client;
        self.notify_property(
            request.window,
            sequence,
            request.property,
            PropertyState::NEW_VALUE,
        );
        Ok(())
    }

    fn delete_property(
        &mut self,
        sequence: u16,
        window: Window,
        property: Atom,
    ) -> Result<(), XError> {
        let removed = self.window_mut(window)?.properties.remove(&property);
        if removed.is_some() {
            self.notify_property(window, sequence, property, PropertyState::DELETE);
        }
        Ok(())
    }

    fn notify_property(
        &self,
        window: Window,
        sequence: u16,
        property: Atom,
        state: PropertyState,
    ) {
        self.deliver(window, u32::from(EventMask::PROPERTY_CHANGE), |_| {
            PropertyNotifyEvent {
                response_type: PROPERTY_NOTIFY_EVENT,
                sequence,
                window,
                atom: property,
                time: 0, // CurrentTime
                state,
            }
            .serialize()
            .to_vec()
        });
    }

    fn get_property(
        &mut self,
        client: u32,
        sequence: u16,
        request: &GetPropertyRequest,
    ) -> Result<(), XError> {
        // NyaTerm: window 0 = None → BadWindow（X.org 语义）
        let (type_, format, value, bytes_after, existed) = if request.window == 0 {
            return Err(XError::bad_window(0));
        } else {
            let window = self.window(request.window)?;
            match window.properties.get(&request.property) {
                None => (0, 0, Vec::new(), 0, false),
                Some(property) => {
                    if request.type_ != 0 && request.type_ != property.type_ {
                        (0, 0, Vec::new(), 0, true)
                    } else {
                        let unit = usize::from(property.format / 8).max(1);
                        let total = property.data.len();
                        let start = ((request.long_offset as usize) * 4).min(total);
                        let max_bytes = if request.long_length == 0 {
                            usize::MAX
                        } else {
                            request.long_length as usize * 4
                        };
                        let end = start.saturating_add(max_bytes).min(total);
                        // 返回的数据必须是完整元素
                        let end = start + (end - start) / unit * unit;
                        let value = property.data[start..end].to_vec();
                        (property.type_, property.format, value, total - end, true)
                    }
                }
            }
        };

        if existed && request.delete {
            self.window_mut(request.window)?
                .properties
                .remove(&request.property);
            self.notify_property(
                request.window,
                sequence,
                request.property,
                PropertyState::DELETE,
            );
        }

        let value_len = if format == 0 {
            0
        } else {
            (value.len() / usize::from(format / 8)) as u32
        };
        // NyaTerm 探针：root 的属性读取记录 atom 名 + 应答内容（WM check 归因）
        if request.window == ROOT_WINDOW {
            let name = self
                .atoms
                .get(request.property as usize)
                .and_then(|o| o.as_deref())
                .unwrap_or("?");
            let tname = self
                .atoms
                .get(type_ as usize)
                .and_then(|o| o.as_deref())
                .unwrap_or("?");
            crate::xsrv::log_line(format_args!(
                "[x11] c{client} seq{sequence} GetProperty(root, {name}) -> type={tname}({type_}) format={format} n={value_len} existed={existed} val={:?}",
                &value[..value.len().min(16)]
            ));
        }
        self.send_reply(
            client,
            &GetPropertyReply {
                format,
                sequence,
                length: value.len().div_ceil(4) as u32,
                type_,
                bytes_after: bytes_after as u32,
                value_len,
                value,
            },
        );
        Ok(())
    }

    fn send_event(&self, sequence: u16, request: &SendEventRequest<'_>) -> Result<(), XError> {
        let mut event = request.event.to_vec();
        // 发送方填的序列号是它自己的；服务端要换成当前请求的序列号
        event[2..4].copy_from_slice(&sequence.to_ne_bytes());
        let mask = u32::from(request.event_mask);
        if mask != 0 {
            self.deliver(request.destination, mask, |_| event.clone());
        } else if let Ok(window) = self.window(request.destination) {
            // event_mask 为 0：发给创建该窗口的客户端（ICCCM 的"定向事件"）
            self.send(window.owner, &event);
        }
        Ok(())
    }

    fn create_pixmap(
        &mut self,
        client: u32,
        request: &CreatePixmapRequest,
    ) -> Result<(), XError> {
        self.check_new_id(client, request.pid)?;
        if request.width == 0 || request.height == 0 {
            return Err(XError::bad_value(0));
        }
        // NyaTerm: 接受所有常见深度（1/4/8/24/32）。firefox/GTK 启动时探测
        // depth 4/8 的位图格式，拒绝会让它放弃建窗（窗口"永不出现"）。
        // 像素统一按 u32 存储即可——浅深度的位图语义由使用方（掩码/贴图）解释。
        if request.depth != 1
            && request.depth != 4
            && request.depth != 8
            && request.depth != SCREEN_DEPTH
            && request.depth != 32
        {
            return Err(XError::bad_value(u32::from(request.depth)));
        }
        self.pixmaps.insert(
            request.pid,
            PixmapData {
                id: request.pid,
                depth: request.depth,
                width: request.width,
                height: request.height,
                pixels: vec![0; request.width as usize * request.height as usize],
                owner: client,
            },
        );
        Ok(())
    }

    fn create_gc(&mut self, client: u32, request: &CreateGCRequest<'_>) -> Result<(), XError> {
        self.check_new_id(client, request.cid)?;
        let (_, _, depth) = self.drawable_geometry(request.drawable)?;
        let mut gc = GcData::new(request.cid, client, depth);
        let aux = &request.value_list;
        if let Some(value) = aux.function {
            gc.function = u32::from(value);
        }
        if let Some(value) = aux.plane_mask {
            gc.plane_mask = value;
        }
        if let Some(value) = aux.foreground {
            gc.foreground = value;
        }
        if let Some(value) = aux.background {
            gc.background = Some(value);
        }
        if let Some(value) = aux.line_width {
            gc.line_width = value;
        }
        if let Some(value) = aux.line_style {
            gc.line_style = u32::from(value);
        }
        if let Some(value) = aux.fill_style {
            gc.fill_style = u32::from(value);
        }
        if let Some(value) = aux.tile {
            gc.tile = Some(value).filter(|id| *id != 0);
        }
        if let Some(value) = aux.stipple {
            gc.stipple = Some(value).filter(|id| *id != 0);
        }
        if let Some(value) = aux.font {
            gc.font = Some(value).filter(|id| *id != 0);
        }
        if let Some(value) = aux.graphics_exposures {
            gc.graphics_exposures = value != 0;
        }
        if let Some(value) = aux.clip_x_origin {
            gc.clip_x_origin = value;
        }
        if let Some(value) = aux.clip_y_origin {
            gc.clip_y_origin = value;
        }
        if let Some(value) = aux.clip_mask {
            // clip-mask 与裁剪矩形互斥：设置 mask 就清掉矩形列表
            gc.clip_mask = Some(value).filter(|id| *id != 0);
            gc.clip_rectangles.clear();
        }
        if let Some(value) = aux.dash_offset {
            gc.dash_offset = value;
        }
        if let Some(value) = aux.arc_mode {
            gc.arc_mode = u32::from(value);
        }
        self.gcs.insert(request.cid, gc);
        Ok(())
    }

    fn clear_area(&mut self, sequence: u16, request: &ClearAreaRequest) -> Result<(), XError> {
        let (background, width, height, mapped) = {
            let window = self.window(request.window)?;
            (
                window.background_pixel,
                window.width,
                window.height,
                window.mapped,
            )
        };
        let x = i32::from(request.x);
        let y = i32::from(request.y);
        // 协议规定宽/高为 0 表示"到窗口边缘"
        let clear_width = if request.width == 0 {
            i32::from(width) - x
        } else {
            i32::from(request.width)
        };
        let clear_height = if request.height == 0 {
            i32::from(height) - y
        } else {
            i32::from(request.height)
        };

        self.draw_on(request.window, |canvas| {
            canvas.fill_solid(x, y, clear_width, clear_height, background);
        })?;

        if request.exposures && mapped {
            self.deliver(request.window, u32::from(EventMask::EXPOSURE), |_| {
                ExposeEvent {
                    response_type: EXPOSE_EVENT,
                    sequence,
                    window: request.window,
                    x: clamp_u16(x),
                    y: clamp_u16(y),
                    width: clamp_u16(clear_width),
                    height: clamp_u16(clear_height),
                    count: 0,
                }
                .serialize()
                .to_vec()
            });
        }
        Ok(())
    }

    fn copy_area(&mut self, request: &CopyAreaRequest) -> Result<(), XError> {
        let gc = self.gc_for(request.gc, request.dst_drawable)?.clone();
        let (source, source_width, source_height, _) =
            self.drawable_pixels(request.src_drawable)?;
        let source = source.to_vec();

        // 源坐标可能为负/越界；只处理两边都有效的区域
        let width = (i32::from(request.width))
            .min(source_width as i32 - i32::from(request.src_x))
            .max(0) as u16;
        let height = (i32::from(request.height))
            .min(source_height as i32 - i32::from(request.src_y))
            .max(0) as u16;
        if width == 0 || height == 0 {
            return Ok(());
        }

        self.draw_with_gc(request.dst_drawable, &gc, |canvas| {
            draw::copy_area(
                canvas,
                &source,
                source_width,
                i32::from(request.src_x),
                i32::from(request.src_y),
                i32::from(request.dst_x),
                i32::from(request.dst_y),
                width,
                height,
            );
        })?;
        Ok(())
    }

    fn poly_point(&mut self, request: &PolyPointRequest<'_>) -> Result<(), XError> {
        let gc = self.gc_for(request.gc, request.drawable)?.clone();
        let points = absolute_points(&request.points, u8::from(request.coordinate_mode) == 1);
        self.draw_with_gc(request.drawable, &gc, move |canvas| {
            for (x, y) in points {
                canvas.paint(x, y, gc.foreground);
            }
        })?;
        Ok(())
    }

    fn poly_line(&mut self, request: &PolyLineRequest<'_>) -> Result<(), XError> {
        let gc = self.gc_for(request.gc, request.drawable)?.clone();
        let points = absolute_points(&request.points, u8::from(request.coordinate_mode) == 1);
        self.draw_with_gc(request.drawable, &gc, move |canvas| {
            canvas.polyline(&points);
        })?;
        Ok(())
    }

    fn poly_segment(&mut self, request: &PolySegmentRequest<'_>) -> Result<(), XError> {
        let gc = self.gc_for(request.gc, request.drawable)?.clone();
        let segments: Vec<(i32, i32, i32, i32)> = request
            .segments
            .iter()
            .map(|segment| {
                (
                    i32::from(segment.x1),
                    i32::from(segment.y1),
                    i32::from(segment.x2),
                    i32::from(segment.y2),
                )
            })
            .collect();
        self.draw_with_gc(request.drawable, &gc, move |canvas| {
            for (x1, y1, x2, y2) in segments {
                canvas.line(x1, y1, x2, y2);
            }
        })?;
        Ok(())
    }

    fn poly_rectangle(&mut self, request: &PolyRectangleRequest<'_>) -> Result<(), XError> {
        let gc = self.gc_for(request.gc, request.drawable)?.clone();
        let rectangles = request.rectangles.to_vec();
        self.draw_with_gc(request.drawable, &gc, move |canvas| {
            for rectangle in rectangles {
                canvas.stroke_rect(
                    i32::from(rectangle.x),
                    i32::from(rectangle.y),
                    i32::from(rectangle.width),
                    i32::from(rectangle.height),
                );
            }
        })?;
        Ok(())
    }

    fn poly_fill_rectangle(
        &mut self,
        request: &PolyFillRectangleRequest<'_>,
    ) -> Result<(), XError> {
        let gc = self.gc_for(request.gc, request.drawable)?.clone();
        let rectangles = request.rectangles.to_vec();
        self.draw_with_gc(request.drawable, &gc, move |canvas| {
            for rectangle in rectangles {
                canvas.fill_rect(
                    i32::from(rectangle.x),
                    i32::from(rectangle.y),
                    i32::from(rectangle.width),
                    i32::from(rectangle.height),
                );
            }
        })?;
        Ok(())
    }

    fn poly_arc(&mut self, request: &PolyArcRequest<'_>) -> Result<(), XError> {
        let gc = self.gc_for(request.gc, request.drawable)?.clone();
        let arcs = request.arcs.to_vec();
        self.draw_with_gc(request.drawable, &gc, move |canvas| {
            for arc in &arcs {
                canvas.arc(arc);
            }
        })?;
        Ok(())
    }

    fn poly_fill_arc(&mut self, request: &PolyFillArcRequest<'_>) -> Result<(), XError> {
        let gc = self.gc_for(request.gc, request.drawable)?.clone();
        let arcs = request.arcs.to_vec();
        self.draw_with_gc(request.drawable, &gc, move |canvas| {
            for arc in &arcs {
                canvas.fill_arc(arc);
            }
        })?;
        Ok(())
    }

    fn fill_poly(&mut self, request: &FillPolyRequest<'_>) -> Result<(), XError> {
        let gc = self.gc_for(request.gc, request.drawable)?.clone();
        let points = absolute_points(&request.points, u8::from(request.coordinate_mode) == 1);
        let shape = request.shape;
        self.draw_with_gc(request.drawable, &gc, move |canvas| {
            canvas.fill_polygon(&points, shape);
        })?;
        Ok(())
    }

    fn put_image(&mut self, request: &PutImageRequest<'_>) -> Result<(), XError> {
        let gc = self.gc_for(request.gc, request.drawable)?.clone();
        let (_, _, depth) = self.drawable_geometry(request.drawable)?;
        let format = u8::from(request.format);
        let data = request.data.to_vec();
        let width = request.width;
        let height = request.height;
        let dst_x = i32::from(request.dst_x);
        let dst_y = i32::from(request.dst_y);
        let mut bad_length = false;

        self.draw_with_gc(request.drawable, &gc, |canvas| {
            match format {
                // ZPixmap：按**目标深度**的打包格式解包（cairo 会往 8/16 位深
                // 的 pixmap 里画图，行按 32 位对齐补齐——按 4 字节/像素统一
                // 解析会整体错位，直接触发客户端的绘制断言崩溃）。
                2 if depth != 1 => {
                    if !draw::put_zpixmap_depth(canvas, dst_x, dst_y, width, height, depth, &data) {
                        bad_length = true;
                    }
                }
                // XYBitmap / XYPixmap：每像素 1 位。写进 1 位深的 pixmap（stipple、
                // tile、cursor 位图都是这条路），或用 GC 的前景/背景色画到彩色目标上。
                _ => {
                    // 位图一行按 32 位补齐（setup 里声明的 bitmap_format_scanline_pad）
                    let stride_bits = usize::from(width).div_ceil(32) * 32;
                    let (foreground, background) = if depth == 1 {
                        (1, None)
                    } else {
                        (gc.foreground, gc.background)
                    };
                    if !draw::put_bitmap(
                        canvas,
                        dst_x,
                        dst_y,
                        width,
                        height,
                        stride_bits,
                        &data,
                        foreground,
                        background,
                    ) {
                        bad_length = true;
                    }
                }
            }
        })?;
        if bad_length {
            return Err(XError::bad_length());
        }
        Ok(())
    }

    fn image_text8(&mut self, request: &ImageText8Request<'_>) -> Result<(), XError> {
        let gc = self.gc_for(request.gc, request.drawable)?.clone();
        let string = request.string.to_vec();
        self.draw_with_gc(request.drawable, &gc, move |canvas| {
            canvas.text8(i32::from(request.x), i32::from(request.y), &string);
        })?;
        Ok(())
    }

    fn image_text16(&mut self, request: &ImageText16Request<'_>) -> Result<(), XError> {
        let gc = self.gc_for(request.gc, request.drawable)?.clone();
        let chars: Vec<u16> = request
            .string
            .iter()
            .map(|char2b| char2b_to_u16(*char2b))
            .collect();
        self.draw_with_gc(request.drawable, &gc, move |canvas| {
            canvas.text16(i32::from(request.x), i32::from(request.y), &chars);
        })?;
        Ok(())
    }

    fn poly_text8(&mut self, request: &PolyText8Request<'_>) -> Result<(), XError> {
        let gc = self.gc_for(request.gc, request.drawable)?.clone();
        let items = parse_text_items(&request.items, false);
        self.draw_with_gc(request.drawable, &gc, move |canvas| {
            let mut pen = i32::from(request.x);
            for item in items {
                let chars: Vec<u16> = item.chars.iter().map(|byte| u16::from(*byte)).collect();
                pen = canvas.text16(pen, i32::from(request.y), &chars);
                pen += item.delta;
            }
        })?;
        Ok(())
    }

    fn poly_text16(&mut self, request: &PolyText16Request<'_>) -> Result<(), XError> {
        let gc = self.gc_for(request.gc, request.drawable)?.clone();
        let items = parse_text_items(&request.items, true);
        self.draw_with_gc(request.drawable, &gc, move |canvas| {
            let mut pen = i32::from(request.x);
            for item in items {
                pen = canvas.text16(pen, i32::from(request.y), &item.chars);
                pen += item.delta;
            }
        })?;
        Ok(())
    }

    // ---- 字体 ----

    fn open_font(&mut self, client: u32, request: &OpenFontRequest<'_>) -> Result<(), XError> {
        self.check_new_id(client, request.fid)?;
        // 任何名字都接受并映射到内置字体：客户端拿到 BadName 往往直接退出，
        // 而"字体度量一致"比"名字精确"对可用性更重要。
        let name = String::from_utf8_lossy(&request.name).into_owned();
        self.fonts.insert(
            request.fid,
            FontData {
                id: request.fid,
                name,
                owner: client,
            },
        );
        Ok(())
    }

    /// `QueryFont` / `QueryTextExtents` 的 font 参数可以是字体 ID，也可以是 GC。
    fn resolve_font(&self, fontable: Fontable) -> Result<Font, XError> {
        if self.fonts.contains_key(&fontable) {
            return Ok(fontable);
        }
        if let Some(gc) = self.gcs.get(&fontable) {
            return gc.font.ok_or_else(|| XError::bad_font(fontable));
        }
        Err(XError::bad_font(fontable))
    }

    fn query_font(
        &self,
        client: u32,
        sequence: u16,
        fontable: Fontable,
    ) -> Result<(), XError> {
        let font_id = self.resolve_font(fontable)?;
        // FONT 属性的值要放"解析出来的完整字体名"对应的 atom（见 font_properties）；
        // 所有别名都映射到同一份内置字体，所以统一上报规范名
        let properties = self.font_properties(font::CANONICAL_NAME);
        let char_infos: Vec<Charinfo> = (0u16..=255).map(font::charinfo).collect();
        // QueryFont 的固定部分是 60 字节（比通用的 32 字节多 28），而 length 字段按
        // 协议数的是"32 字节之后"的全部 4 字节单元——**必须把那 28 字节算进去**。
        // 少算会让严格按 length 定位的客户端（x11rb 等）把变长部分读歪。
        let length = (28 + properties.len() * 8 + char_infos.len() * 12) / 4;
        self.send_reply(
            client,
            &QueryFontReply {
                sequence,
                length: length as u32,
                min_bounds: font::min_bounds(),
                max_bounds: font::max_bounds(),
                min_char_or_byte2: u16::from(font::MIN_CHAR as u8),
                max_char_or_byte2: u16::from(font::MAX_CHAR as u8),
                default_char: font::DEFAULT_CHAR,
                draw_direction: FontDraw::LEFT_TO_RIGHT,
                min_byte1: 0,
                max_byte1: 0,
                all_chars_exist: true,
                font_ascent: font::ASCENT,
                font_descent: font::DESCENT,
                properties,
                char_infos,
            },
        );
        Ok(())
    }

    /// 字体的属性表。客户端靠它判断字体是否为等宽、字号多大（xterm 用
    /// `MIN_SPACE` / `MAX_SPACE` 决定字符单元宽度）。
    ///
    /// `font_name` 是解析出来的完整字体名：`FONT` / `CHARSET_REGISTRY` /
    /// `CHARSET_ENCODING` 这三个属性的**值本身也是 atom**，Xlib 建 FontSet 时拿
    /// `XGetAtomName` 还原出字符串来判断字符集——缺了后两个属性，Xlib 会报
    /// "Missing charsets …" / "Unable to load any usable fontset"，xclock 崩在这之前
    /// 就先死在它自己划的这条线上。
    fn font_properties(&self, font_name: &str) -> Vec<Fontprop> {
        let atom = |name: &str| self.atom_ids.get(name).copied().unwrap_or(0);
        let width = u32::from(font::CHAR_WIDTH.unsigned_abs());
        let height = u32::from(font::CELL_HEIGHT.unsigned_abs());
        let mut properties = vec![
            // 值不是字体资源 ID，而是完整 XLFD 名对应的 atom（真实服务端也这么做）
            Fontprop { name: atom("FONT"), value: atom(font_name) },
            Fontprop { name: atom("DEFAULT_CHAR"), value: u32::from(font::DEFAULT_CHAR) },
            Fontprop { name: atom("MIN_SPACE"), value: width },
            Fontprop { name: atom("NORM_SPACE"), value: width },
            Fontprop { name: atom("MAX_SPACE"), value: width },
            Fontprop { name: atom("END_SPACE"), value: width },
            Fontprop { name: atom("PIXEL_SIZE"), value: height },
            Fontprop { name: atom("POINT_SIZE"), value: height * 10 },
            Fontprop { name: atom("RESOLUTION_X"), value: 96 },
            Fontprop { name: atom("RESOLUTION_Y"), value: 96 },
            Fontprop { name: atom("SPACING"), value: 1 }, // 1 = monospace
            Fontprop { name: atom("X_HEIGHT"), value: u32::from(font::ASCENT.unsigned_abs()) - 3 },
            Fontprop { name: atom("QUAD_WIDTH"), value: width },
            Fontprop { name: atom("WEIGHT"), value: 100 }, // 100 = medium
            // 字符集声明：Xlib 的 FontSet 靠这两个属性判断"这个字体是什么字符集"。
            // 我们只有一份 ASCII 字形，所以只声明 iso8859-1，不冒充 CJK 字符集
            // （声明了也只是让客户端拿方块字，还不如让它走降级路径）。
            Fontprop { name: atom("CHARSET_REGISTRY"), value: atom("iso8859") },
            Fontprop { name: atom("CHARSET_ENCODING"), value: atom("1") },
        ];
        // atom 缺失（理论上不会）时不要发 name=0 的属性，客户端会当成 None
        properties.retain(|property| property.name != 0);
        properties
    }

    fn query_text_extents(
        &self,
        client: u32,
        sequence: u16,
        request: &QueryTextExtentsRequest<'_>,
    ) -> Result<(), XError> {
        self.resolve_font(request.font)?;
        // `TextWidth` 只算到最后一个字符的起点，所以要少一个字符宽
        let count = request.string.len().saturating_sub(1);
        let width = font::text_width(count);
        self.send_reply(
            client,
            &QueryTextExtentsReply {
                draw_direction: FontDraw::LEFT_TO_RIGHT,
                sequence,
                length: 0,
                font_ascent: font::ASCENT,
                font_descent: font::DESCENT,
                overall_ascent: font::ASCENT,
                overall_descent: font::DESCENT,
                overall_width: width,
                overall_left: 0,
                overall_right: width,
            },
        );
        Ok(())
    }

    fn list_fonts(
        &self,
        client: u32,
        sequence: u16,
        request: &ListFontsRequest<'_>,
    ) -> Result<(), XError> {
        let pattern = String::from_utf8_lossy(&request.pattern).into_owned();
        let names: Vec<Str> = matching_font_names(&pattern, request.max_names)
            .into_iter()
            .map(|name| Str { name: name.into_bytes() })
            .collect();
        let length: usize = names.iter().map(|name| padded4(name.name.len())).sum();
        self.send_reply(
            client,
            &ListFontsReply {
                sequence,
                length: (length / 4) as u32,
                names,
            },
        );
        Ok(())
    }

    /// `ListFontsWithInfo` 是"一个请求、多个回复"：每个匹配的字体回一条 `FontInfo`，
    /// 最后用一条名字为空的回复收尾。客户端（比如 Xlib 的 `XListFontsWithInfo`）
    /// 靠这个空名字判断列表结束，少发会导致它一直等下去。
    fn list_fonts_with_info(
        &self,
        client: u32,
        sequence: u16,
        request: &ListFontsWithInfoRequest<'_>,
    ) -> Result<(), XError> {
        let pattern = String::from_utf8_lossy(&request.pattern).into_owned();
        let names = matching_font_names(&pattern, request.max_names);
        // 属性表对每个字体都一样（都是同一份内置字形），FONT 上报规范名
        let properties = self.font_properties(font::CANONICAL_NAME);
        for name in &names {
            // FontInfo 的固定部分同样是 60 字节：length 要算上多出来的 28 字节
            let length = 28 + properties.len() * 8 + padded4(name.len());
            self.send_reply(
                client,
                &ListFontsWithInfoReply {
                    sequence,
                    length: (length / 4) as u32,
                    min_bounds: font::min_bounds(),
                    max_bounds: font::max_bounds(),
                    min_char_or_byte2: u16::from(font::MIN_CHAR as u8),
                    max_char_or_byte2: u16::from(font::MAX_CHAR as u8),
                    default_char: font::DEFAULT_CHAR,
                    draw_direction: FontDraw::LEFT_TO_RIGHT,
                    min_byte1: 0,
                    max_byte1: 0,
                    all_chars_exist: true,
                    font_ascent: font::ASCENT,
                    font_descent: font::DESCENT,
                    replies_hint: names.len() as u32,
                    properties: properties.clone(),
                    name: name.as_bytes().to_vec(),
                },
            );
        }
        // 结束标记：名字为空、没有属性。
        //
        // length 不能写 0：FontInfo 的固定部分是 60 字节，按协议 length 数的是
        // "超出 32 字节通用回复头"的部分，所以光是固定部分就有 7 个 4 字节单元。
        // 写 0 会让按 length 定位的客户端以为回复只有 32 字节，把后面的流读歪。
        self.send_reply(
            client,
            &ListFontsWithInfoReply {
                sequence,
                length: (60 - 32) / 4,
                min_bounds: font::min_bounds(),
                max_bounds: font::max_bounds(),
                min_char_or_byte2: 0,
                max_char_or_byte2: 0,
                default_char: 0,
                draw_direction: FontDraw::LEFT_TO_RIGHT,
                min_byte1: 0,
                max_byte1: 0,
                all_chars_exist: false,
                font_ascent: 0,
                font_descent: 0,
                replies_hint: 0,
                properties: Vec::new(),
                name: Vec::new(),
            },
        );
        Ok(())
    }

    fn get_image(
        &mut self,
        client: u32,
        sequence: u16,
        request: &GetImageRequest,
    ) -> Result<(), XError> {
        if u8::from(request.format) != u8::from(ImageFormat::Z_PIXMAP) {
            return Err(XError::bad_match());
        }
        // 读根窗口像素前先把待合成的绘制落地（GetImage(root) 读的是合成结果）
        self.ensure_composed();
        let (pixels, width, height, depth) = self.drawable_pixels(request.drawable)?;
        // 1 位深的可绘制对象（位图）按位打包返回，其余按 4 字节像素
        let data = if depth == 1 {
            draw::get_bitmap(
                pixels,
                width,
                height,
                i32::from(request.x),
                i32::from(request.y),
                request.width,
                request.height,
            )
        } else {
            draw::get_zpixmap(
                pixels,
                width,
                height,
                i32::from(request.x),
                i32::from(request.y),
                request.width,
                request.height,
            )
        }
        .ok_or_else(XError::bad_match)?;
        let visual = if self.windows.contains_key(&request.drawable) {
            ROOT_VISUAL
        } else {
            0
        };
        self.send_reply(
            client,
            &GetImageReply {
                depth,
                sequence,
                visual,
                data,
            },
        );
        Ok(())
    }
}

/// 把相对坐标的点列表按 `CoordinateMode` 展开成绝对坐标。
fn absolute_points(points: &[Point], previous: bool) -> Vec<(i32, i32)> {
    if !previous {
        return points
            .iter()
            .map(|point| (i32::from(point.x), i32::from(point.y)))
            .collect();
    }
    let mut last = (0i32, 0i32);
    let mut absolute = Vec::with_capacity(points.len());
    for point in points {
        last = (last.0 + i32::from(point.x), last.1 + i32::from(point.y));
        absolute.push(last);
    }
    absolute
}

/// `PolyText` 里的一个文字项。
struct TextItem {
    chars: Vec<u16>,
    /// 画完这段文字后笔位置的额外位移。
    delta: i32,
}

/// 解析 `PolyText8` / `PolyText16` 的 item 列表。
///
/// 线格式（X11 协议）：每项第一个字节是长度 `n`。
/// - `n == 255`：字体切换项，第二个字节是字体号，整项 4 字节；客户端在换字体，
///   我们只有一套字体，忽略但必须跳过，否则后面的偏移会全错；
/// - 否则：`n` 个字符 + 1 字节 `delta`，整项补齐到 4 字节。
fn parse_text_items(bytes: &[u8], wide: bool) -> Vec<TextItem> {
    let mut items = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let length = usize::from(bytes[offset]);
        if length == 255 {
            offset += 4;
            continue;
        }
        let char_bytes = if wide { length * 2 } else { length };
        let start = offset + 1;
        let end = start + char_bytes;
        if end + 1 > bytes.len() {
            break;
        }
        let chars = if wide {
            bytes[start..end]
                .chunks_exact(2)
                .map(|pair| u16::from(pair[0]) | (u16::from(pair[1]) << 8))
                .collect()
        } else {
            bytes[start..end].iter().map(|byte| u16::from(*byte)).collect()
        };
        // delta 是 i8：负数表示向左移，常见于连字/回退
        let delta = i32::from(bytes[end] as i8);
        items.push(TextItem { chars, delta });
        offset += padded4(char_bytes + 2);
    }
    items
}

/// `Char2b` 是一个 16 位字符，两个字节分开存放（byte1 高位）。
fn char2b_to_u16(char2b: Char2b) -> u16 {
    (u16::from(char2b.byte1) << 8) | u16::from(char2b.byte2)
}

/// 按 X11 的通配模式从内置字体名表里挑出最多 `max_names` 个名字。
fn matching_font_names(pattern: &str, max_names: u16) -> Vec<String> {
    let limit = if max_names == 0 { usize::MAX } else { usize::from(max_names) };
    font::FONT_NAMES
        .iter()
        .filter(|name| font::matches_pattern(name, pattern))
        .take(limit)
        .map(|name| (*name).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use x11rb_protocol::protocol::xproto;
    use x11rb_protocol::protocol::Request;

    use super::*;

    fn fixture() -> ServerState {
        let mut state = ServerState::new(Default::default());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);
        state
    }

    /// 回归：握手通告了两个视觉（depth24 的 ROOT_VISUAL、depth32 的
    /// ARGB32_VISUAL），CreateColormap 必须两个都接受。
    ///
    /// 历史 bug：只认 ROOT_VISUAL，GTK 为弹窗/半透明 surface 用 depth32 建
    /// colormap 时收到 BadMatch，GDK 直接退出——用户实测崩溃信息正是
    /// "BadMatch (invalid parameter attributes), request_code 78"，
    /// 78 就是 CreateColormap。
    #[test]
    fn create_colormap_accepts_both_advertised_visuals() {
        let mut state = fixture();

        for (seq, visual) in [(1u16, ROOT_VISUAL), (2, ARGB32_VISUAL)] {
            let request = Request::CreateColormap(xproto::CreateColormapRequest {
                alloc: xproto::ColormapAlloc::NONE,
                mid: 0x0020_0000 + visual,
                window: ROOT_WINDOW,
                visual,
            });
            state
                .handle(0, seq, &request)
                .unwrap_or_else(|e| panic!("视觉 {visual:#x} 应当被接受，实际 {e:?}"));
        }

        // 没通告过的视觉仍然要 BadMatch
        let request = Request::CreateColormap(xproto::CreateColormapRequest {
            alloc: xproto::ColormapAlloc::NONE,
            mid: 0x0020_00ff,
            window: ROOT_WINDOW,
            visual: 0xdead,
        });
        let err = state
            .handle(0, 3, &request)
            .expect_err("未通告的视觉必须报 BadMatch");
        assert_eq!(err.code, crate::xsrv::error::BAD_MATCH);
    }
}
