//! X11 连接建立（setup）阶段的编解码。
//!
//! 客户端连上后发的第一段字节是 setup request，服务端用 setup reply 回应，
//! 之后才进入正常的请求/事件流（见 `server.rs`）。
//!
//! 字节序：X11 允许客户端指定 LSBFirst / MSBFirst，服务端必须用同样的字节序回复。
//! 本进程跑在小端平台（Windows x64 / arm64），因此只接受 LSBFirst 的客户端；
//! 收到 MSBFirst 就用 setup failed 明确拒绝，而不是发出一段对方读不懂的回复。
//!
//! 这里只做"连接建立"这一段；请求分帧与语义在 `server.rs` 与后续里程碑里实现。

use x11rb_protocol::protocol::xproto::{
    BackingStore, Depth, EventMask, Format, ImageOrder, Screen, Setup, SetupFailed, SetupRequest,
    VisualClass, Visualtype,
};
use x11rb_protocol::x11_utils::{Serialize, TryParse};

/// 我们声明的协议版本。
pub const PROTOCOL_MAJOR: u16 = 11;
pub const PROTOCOL_MINOR: u16 = 0;

/// 服务端 release number：客户端只拿它做显示与兼容判断，跟着 Xorg 的编码方式写。
pub const RELEASE_NUMBER: u32 = 12_101_000;

/// 资源 ID 掩码；每个客户端占 `1 << 21` 个 ID，第 n 个客户端拿到 `(n + 1) << 21`。
pub const RESOURCE_ID_MASK: u32 = 0x001f_ffff;
const RESOURCE_ID_SHIFT: u32 = 21;

/// 单个请求的最大长度，单位是 4 字节（0xffff → 256 KiB，与 Xorg 一致）。
pub const MAX_REQUEST_LENGTH: u16 = 0xffff;

pub const MIN_KEYCODE: u8 = 8;
pub const MAX_KEYCODE: u8 = 255;

/// 服务端预分配的资源 ID：都落在客户端 ID 区间（≥ 0x0020_0000）之外。
pub const ROOT_WINDOW: u32 = 0x0000_012c;
pub const ROOT_VISUAL: u32 = 0x0000_0021;
/// NyaTerm: depth 32 的 ARGB TrueColor 视觉（firefox/GTK 透明窗口用）。
pub const ARGB32_VISUAL: u32 = 0x0000_0022;
pub const ROOT_COLORMAP: u32 = 0x0000_0020;

/// screen 的像素深度：先只提供 24 位真彩（32 bpp）。
pub const SCREEN_DEPTH: u8 = 24;

pub const VENDOR: &str = "RustTerm X Server";

/// setup request 的固定头长度：
/// byte_order(1) + pad(1) + major(2) + minor(2) + name_len(2) + data_len(2) + pad(2)。
pub const SETUP_PREFIX_LEN: usize = 12;

/// 'l' = LSBFirst（本进程支持），'B' = MSBFirst（明确拒绝）。
const BYTE_ORDER_LSB: u8 = 0x6c;
const BYTE_ORDER_MSB: u8 = 0x42;

/// 握手阶段的失败原因。都属于"连接不可用"，调用方记日志后关掉连接即可。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupError {
    /// 头部不足 12 字节
    ShortPrefix,
    /// 客户端要求的字节序本进程不支持
    UnsupportedByteOrder(u8),
    /// x11rb 解析失败（长度字段自相矛盾等）
    Parse(String),
}

/// 把长度补齐到 4 字节边界（X11 的所有字段都按 4 字节对齐）。
fn pad4(len: usize) -> usize {
    (len + 3) & !3
}

/// 客户端 ID 区间起点。
pub fn resource_id_base(client_index: u32) -> u32 {
    (client_index + 1) << RESOURCE_ID_SHIFT
}

/// `setup_prefix()` 的结论：要么给出剩余长度，要么明确拒绝。
///
/// 之所以要先算长度：setup request 是变长的（认证名 / 认证数据），必须读满整段才能解析。
/// 返回 `Err(SetupError::UnsupportedByteOrder)` 时调用方应当回 setup failed 再关连接。
pub fn setup_prefix(prefix: &[u8]) -> Result<usize, SetupError> {
    if prefix.len() < SETUP_PREFIX_LEN {
        return Err(SetupError::ShortPrefix);
    }
    if prefix[0] != BYTE_ORDER_LSB {
        return Err(SetupError::UnsupportedByteOrder(prefix[0]));
    }
    // 头部字段是小端（本进程字节序）——上面已确认客户端也用 LSBFirst
    let name_len = u16::from_ne_bytes([prefix[6], prefix[7]]) as usize;
    let data_len = u16::from_ne_bytes([prefix[8], prefix[9]]) as usize;
    Ok(pad4(name_len) + pad4(data_len))
}

/// 解析完整的 setup request（固定头 + 剩余部分）。
pub fn parse_setup(full: &[u8]) -> Result<SetupRequest, SetupError> {
    // 先过一遍头部校验（字节序 + 长度自洽），再交给 x11rb 解析
    setup_prefix(full)?;
    SetupRequest::try_parse(full)
        .map(|(request, _)| request)
        .map_err(|e| SetupError::Parse(format!("{e:?}")))
}

/// 生成 setup reply（成功）。
pub fn setup_reply(client_index: u32, width: u16, height: u16) -> Vec<u8> {
    let mut setup = Setup {
        status: 1,
        protocol_major_version: PROTOCOL_MAJOR,
        protocol_minor_version: PROTOCOL_MINOR,
        // 长度字段 = 前 8 字节之后的字节数 / 4；先序列化一次拿到总长再回填
        length: 0,
        release_number: RELEASE_NUMBER,
        resource_id_base: resource_id_base(client_index),
        resource_id_mask: RESOURCE_ID_MASK,
        motion_buffer_size: 0,
        maximum_request_length: MAX_REQUEST_LENGTH,
        image_byte_order: ImageOrder::LSB_FIRST,
        bitmap_format_bit_order: ImageOrder::LSB_FIRST,
        bitmap_format_scanline_unit: 32,
        bitmap_format_scanline_pad: 32,
        min_keycode: MIN_KEYCODE,
        max_keycode: MAX_KEYCODE,
        vendor: VENDOR.as_bytes().to_vec(),
        // NyaTerm: 宣告常见的 pixmap 深度（1/4/8/16/24/32）。firefox/GTK 启动
        // 时会探测这些深度来选格式，被拒绝就直接放弃建窗；cairo 画图时按这里
        // 的 bits_per_pixel/scanline_pad 计算行宽——数值必须与真实 Xorg 一致
        // （depth 4 是 8bpp 而不是 16；depth 16 是 16bpp），否则行宽错位触发
        // 客户端断言崩溃。
        pixmap_formats: [
            (1u8, 1u8, 32u16),
            (4, 8, 32),
            (8, 8, 32),
            (16, 16, 32),
            (24, 32, 32),
            (32, 32, 32),
        ]
        .iter()
        .map(|&(depth, bits_per_pixel, scanline_pad)| Format {
            depth,
            bits_per_pixel: bits_per_pixel as u8,
            scanline_pad: scanline_pad as u8,
        })
        .collect(),
        roots: vec![screen(width, height)],
    };
    setup.length = ((setup.serialize().len() - 8) / 4) as u16;
    setup.serialize()
}

/// 生成 setup failed（拒绝连接）。
pub fn setup_failed(reason: &str) -> Vec<u8> {
    // reason 的长度字段只有 1 字节，超长就截断
    let reason = reason.as_bytes();
    let reason = &reason[..reason.len().min(u8::MAX as usize)];
    let mut failed = SetupFailed {
        status: 0,
        protocol_major_version: PROTOCOL_MAJOR,
        protocol_minor_version: PROTOCOL_MINOR,
        length: 0,
        reason: reason.to_vec(),
    };
    failed.length = ((failed.serialize().len() - 8) / 4) as u16;
    failed.serialize()
}

/// 单屏描述。M0 固定一个 24 位真彩视觉；M6 起屏幕尺寸跟窗口走、并支持多视觉。
fn screen(width: u16, height: u16) -> Screen {
    Screen {
        root: ROOT_WINDOW,
        default_colormap: ROOT_COLORMAP,
        white_pixel: 0x00ff_ffff,
        black_pixel: 0x0000_0000,
        current_input_masks: EventMask::NO_EVENT,
        width_in_pixels: width,
        height_in_pixels: height,
        // 毫米尺寸只是给客户端的提示，按 96 DPI 折算（1 px = 254/960 mm）
        width_in_millimeters: (width as u32 * 254 / 960) as u16,
        height_in_millimeters: (height as u32 * 254 / 960) as u16,
        min_installed_maps: 1,
        max_installed_maps: 1,
        root_visual: ROOT_VISUAL,
        backing_stores: BackingStore::NOT_USEFUL,
        save_unders: false,
        root_depth: SCREEN_DEPTH,
        allowed_depths: vec![
            Depth {
                depth: SCREEN_DEPTH,
                visuals: vec![Visualtype {
                    visual_id: ROOT_VISUAL,
                    class: VisualClass::TRUE_COLOR,
                    bits_per_rgb_value: 8,
                    colormap_entries: 256,
                    red_mask: 0x00ff_0000,
                    green_mask: 0x0000_ff00,
                    blue_mask: 0x0000_00ff,
                }],
            },
            // NyaTerm: depth 32 的 ARGB 视觉——firefox/GTK 的标准用法（透明
            // 合成窗口）。没有它，这类客户端创建窗口时直接收到 BadMatch。
            Depth {
                depth: 32,
                visuals: vec![Visualtype {
                    visual_id: ARGB32_VISUAL,
                    class: VisualClass::TRUE_COLOR,
                    bits_per_rgb_value: 8,
                    colormap_entries: 256,
                    red_mask: 0x00ff_0000,
                    green_mask: 0x0000_ff00,
                    blue_mask: 0x0000_00ff,
                }],
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb_protocol::x11_utils::TryParse;

    /// 按协议手写一段 setup request，用来喂给解析器。
    fn setup_request_bytes(byte_order: u8, auth_name: &[u8], auth_data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(byte_order);
        bytes.push(0);
        bytes.extend_from_slice(&PROTOCOL_MAJOR.to_ne_bytes());
        bytes.extend_from_slice(&PROTOCOL_MINOR.to_ne_bytes());
        bytes.extend_from_slice(&(auth_name.len() as u16).to_ne_bytes());
        bytes.extend_from_slice(&(auth_data.len() as u16).to_ne_bytes());
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(auth_name);
        bytes.resize(SETUP_PREFIX_LEN + pad4(auth_name.len()), 0);
        let data_at = bytes.len();
        bytes.extend_from_slice(auth_data);
        bytes.resize(data_at + pad4(auth_data.len()), 0);
        bytes
    }

    #[test]
    fn prefix_length_covers_the_whole_request() {
        // 认证名 18 字节（不是 4 的倍数）+ cookie 16 字节，用来验证补齐
        let bytes = setup_request_bytes(BYTE_ORDER_LSB, b"MIT-MAGIC-COOKIE-1", &[0x11; 16]);
        let rest = setup_prefix(&bytes[..SETUP_PREFIX_LEN]).expect("头部应当可解析");
        assert_eq!(SETUP_PREFIX_LEN + rest, bytes.len());
    }

    #[test]
    fn accepts_little_endian_setup() {
        let bytes = setup_request_bytes(BYTE_ORDER_LSB, b"MIT-MAGIC-COOKIE-1", &[0x22; 16]);
        let request = parse_setup(&bytes).expect("应当解析成功");
        assert_eq!(request.protocol_major_version, PROTOCOL_MAJOR);
        assert_eq!(request.protocol_minor_version, PROTOCOL_MINOR);
        assert_eq!(request.authorization_protocol_name, b"MIT-MAGIC-COOKIE-1");
        assert_eq!(request.authorization_protocol_data, vec![0x22u8; 16]);
    }

    #[test]
    fn rejects_big_endian_setup() {
        let bytes = setup_request_bytes(BYTE_ORDER_MSB, b"MIT-MAGIC-COOKIE-1", &[0x33; 16]);
        assert_eq!(
            setup_prefix(&bytes[..SETUP_PREFIX_LEN]),
            Err(SetupError::UnsupportedByteOrder(BYTE_ORDER_MSB))
        );
        assert!(matches!(
            parse_setup(&bytes),
            Err(SetupError::UnsupportedByteOrder(BYTE_ORDER_MSB))
        ));
    }

    #[test]
    fn setup_reply_round_trips_through_x11rb() {
        let bytes = setup_reply(0, 1280, 800);
        assert_eq!(bytes[0], 1, "status 必须是 1（成功）");
        assert_eq!(bytes.len() % 4, 0, "回复必须 4 字节对齐");

        let (setup, rest) = Setup::try_parse(&bytes).expect("x11rb 应当能解析我们生成的回复");
        assert!(rest.is_empty());
        assert_eq!(setup.length as usize * 4, bytes.len() - 8, "length 字段算错");
        assert_eq!(setup.protocol_major_version, PROTOCOL_MAJOR);
        assert_eq!(setup.resource_id_base, 0x0020_0000);
        assert_eq!(setup.resource_id_mask, RESOURCE_ID_MASK);
        assert_eq!(setup.vendor, VENDOR.as_bytes());
        assert!(setup.pixmap_formats.len() >= 5, "1/4/8/24/32 depths advertised");
        assert_eq!(setup.roots.len(), 1);

        let screen = &setup.roots[0];
        assert_eq!(screen.root, ROOT_WINDOW);
        assert_eq!(screen.root_visual, ROOT_VISUAL);
        assert_eq!(screen.default_colormap, ROOT_COLORMAP);
        assert_eq!(screen.width_in_pixels, 1280);
        assert_eq!(screen.height_in_pixels, 800);
        assert_eq!(screen.root_depth, SCREEN_DEPTH);
        assert_eq!(screen.allowed_depths.len(), 2, "depth 24 + depth 32 (ARGB) both advertised");
        assert_eq!(screen.allowed_depths[0].visuals.len(), 1);
        let visual = &screen.allowed_depths[0].visuals[0];
        assert_eq!(visual.visual_id, ROOT_VISUAL);
        assert_eq!(visual.class, VisualClass::TRUE_COLOR);
        assert_eq!(visual.red_mask, 0x00ff_0000);
    }

    #[test]
    fn second_client_gets_a_distinct_id_range() {
        let first = setup_reply(0, 640, 480);
        let second = setup_reply(1, 640, 480);
        let base = |bytes: &[u8]| Setup::try_parse(bytes).unwrap().0.resource_id_base;
        assert_eq!(base(&first), 0x0020_0000);
        assert_eq!(base(&second), 0x0040_0000);
    }

    #[test]
    fn failed_reply_round_trips() {
        let bytes = setup_failed("byte-order not supported");
        let (failed, rest) = SetupFailed::try_parse(&bytes).expect("x11rb 应当能解析");
        assert!(rest.is_empty());
        assert_eq!(failed.status, 0);
        assert_eq!(failed.reason, b"byte-order not supported");
        assert_eq!(failed.length as usize * 4, bytes.len() - 8);
    }
}
