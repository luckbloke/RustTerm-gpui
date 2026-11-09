//! X11 错误帧的编码。
//!
//! 线上错误帧固定 32 字节：`response_type=0` + `error_code` + `sequence` +
//! `bad_value` + `minor_opcode` + `major_opcode` + padding。客户端靠
//! `sequence` 把错误对上具体请求，所以错误必须在同一个连接、按请求顺序发出。
//!
//! 这里不追求覆盖全部错误码：只定义 M1 会产生的那些，其余请求失败时统一
//! 回 BadRequest（表示"这个请求本身没实现"）或 BadImplementation。

/// 错误码（X11 协议固定值）。
pub const BAD_REQUEST: u8 = 1;
pub const BAD_VALUE: u8 = 2;
pub const BAD_WINDOW: u8 = 3;
pub const BAD_PIXMAP: u8 = 4;
pub const BAD_ATOM: u8 = 5;
pub const BAD_CURSOR: u8 = 6;
pub const BAD_FONT: u8 = 7;
pub const BAD_MATCH: u8 = 8;
pub const BAD_DRAWABLE: u8 = 9;
pub const BAD_ACCESS: u8 = 10;
pub const BAD_ALLOC: u8 = 11;
pub const BAD_COLORMAP: u8 = 12;
pub const BAD_G_CONTEXT: u8 = 13;
pub const BAD_ID_CHOICE: u8 = 14;
pub const BAD_NAME: u8 = 15;
pub const BAD_LENGTH: u8 = 16;
pub const BAD_IMPLEMENTATION: u8 = 17;

/// 一个待发送的错误：错误码 + 出错的具体值。
///
/// 请求的 major/minor opcode 由连接循环补上（它在解析请求头时就知道），
/// 这样分发层只关心语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XError {
    pub code: u8,
    pub bad_value: u32,
}

impl XError {
    pub const fn new(code: u8, bad_value: u32) -> Self {
        Self { code, bad_value }
    }

    pub const fn bad_request() -> Self {
        Self::new(BAD_REQUEST, 0)
    }

    pub const fn bad_value(value: u32) -> Self {
        Self::new(BAD_VALUE, value)
    }

    pub const fn bad_window(id: u32) -> Self {
        Self::new(BAD_WINDOW, id)
    }

    pub const fn bad_pixmap(id: u32) -> Self {
        Self::new(BAD_PIXMAP, id)
    }

    pub const fn bad_atom(id: u32) -> Self {
        Self::new(BAD_ATOM, id)
    }

    pub const fn bad_cursor(id: u32) -> Self {
        Self::new(BAD_CURSOR, id)
    }

    pub const fn bad_name() -> Self {
        Self::new(BAD_NAME, 0)
    }

    pub const fn bad_drawable(id: u32) -> Self {
        Self::new(BAD_DRAWABLE, id)
    }

    pub const fn bad_font(id: u32) -> Self {
        Self::new(BAD_FONT, id)
    }

    pub const fn bad_match() -> Self {
        Self::new(BAD_MATCH, 0)
    }

    pub const fn bad_access() -> Self {
        Self::new(BAD_ACCESS, 0)
    }

    pub const fn bad_colormap(id: u32) -> Self {
        Self::new(BAD_COLORMAP, id)
    }

    pub const fn bad_gc(id: u32) -> Self {
        Self::new(BAD_G_CONTEXT, id)
    }

    pub const fn bad_id_choice(id: u32) -> Self {
        Self::new(BAD_ID_CHOICE, id)
    }

    pub const fn bad_length() -> Self {
        Self::new(BAD_LENGTH, 0)
    }

    pub const fn bad_implementation() -> Self {
        Self::new(BAD_IMPLEMENTATION, 0)
    }
}

/// 编码一个 32 字节的错误帧。
pub fn encode(error: XError, sequence: u16, minor_opcode: u8, major_opcode: u8) -> Vec<u8> {
    let mut frame = [0u8; 32];
    frame[0] = 0; // 0 = Error
    frame[1] = error.code;
    frame[2..4].copy_from_slice(&sequence.to_ne_bytes());
    frame[4..8].copy_from_slice(&error.bad_value.to_ne_bytes());
    frame[8..10].copy_from_slice(&(minor_opcode as u16).to_ne_bytes());
    frame[10] = major_opcode;
    // frame[11] 是 pad，后面 20 字节也保持 0
    frame.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_frame_layout_matches_the_protocol() {
        let frame = encode(XError::bad_window(0x1234), 7, 3, 12);
        assert_eq!(frame.len(), 32);
        assert_eq!(frame[0], 0);
        assert_eq!(frame[1], BAD_WINDOW);
        assert_eq!(u16::from_ne_bytes([frame[2], frame[3]]), 7);
        assert_eq!(u32::from_ne_bytes(frame[4..8].try_into().unwrap()), 0x1234);
        assert_eq!(u16::from_ne_bytes([frame[8], frame[9]]), 3);
        assert_eq!(frame[10], 12);
    }
}
