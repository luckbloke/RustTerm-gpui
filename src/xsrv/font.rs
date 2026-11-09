//! 内置字体。
//!
//! 自研 X server 没有 Xft / fontconfig / X 字体路径，核心协议里的 `OpenFont`、
//! `ImageText8`、`QueryFont` 需要一份**自带的**字形数据。这里直接内嵌一套等宽点阵
//! 字体（`font_data.rs`，由 `scripts/gen-xfont.py` 从系统等宽字体栅格化而来），
//! 所有字体名都统一映射到它。
//!
//! 为什么不按名字挑不同字体：真实应用请求的字体名五花八门（`fixed`、
//! `-misc-fixed-*`、`6x13`、`9x15`……），一一实现不现实，而"名字都能打开、
//! 度量一致"比"部分名字打不开"对客户端友好得多——客户端拿到 `BadName` 往往会
//! 直接放弃启动。度量的一致性也保证布局计算不会错位（字宽固定 7 像素）。
//!
//! M2 只做单字节（Latin-1）字符：`PolyText16` / `ImageText16` 会把 16 位字符
//! 截到低字节，超出范围的字符按 `?` 处理。

use x11rb_protocol::protocol::xproto::Charinfo;

use super::font_data as data;

/// 字宽（像素）。等宽字体，每个字符前进量相同。
pub const CHAR_WIDTH: i16 = data::GLYPH_WIDTH as i16;
/// 基线到字形顶部的距离。
pub const ASCENT: i16 = data::ASCENT;
/// 基线到字形底部的距离。
pub const DESCENT: i16 = data::DESCENT;
/// 单元格总高：`ASCENT + DESCENT`。
pub const CELL_HEIGHT: i16 = data::GLYPH_HEIGHT as i16;
/// 字符集里最小的可用字符。
pub const MIN_CHAR: u16 = data::FIRST_CHAR;
/// 字符集里最大的可用字符。
pub const MAX_CHAR: u16 = data::LAST_CHAR;
/// 缺字时的替代字符。
pub const DEFAULT_CHAR: u16 = b'?' as u16;

/// 可用字体名的候选表。
///
/// `ListFonts` 用通配模式匹配这张表；表里既有 X11 标准的 `-misc-fixed-*` 全名，
/// 也有常见的短别名。客户端为了"挑一个等宽字体"往往只匹配模式，不关心具体名字。
pub const FONT_NAMES: &[&str] = &[
    "fixed",
    "6x13",
    "7x14",
    "9x15",
    "monospace",
    "courier",
    "-misc-fixed-medium-r-normal--13-120-75-75-c-70-iso8859-1",
    "-misc-fixed-medium-r-normal--14-130-75-75-c-70-iso8859-1",
    "-misc-fixed-medium-r-normal-*-*-*-*-*-c-*-iso8859-1",
    "-misc-fixed-*-*-*-*-*-*-*-*-*-*-*-*",
    "-adobe-courier-medium-r-normal--14-140-75-75-m-90-iso8859-1",
];

/// 所有字体名（含别名）都映射到同一份内置字形，`QueryFont` 的 FONT 属性上报的
/// 就是这个"解析后的名字"。必须带完整的 XLFD 字段（尤其 `iso8859-1` 字符集后缀）：
/// Xlib 的 FontSet 拿它和 CHARSET_REGISTRY/ENCODING 属性判断字符集，
/// 缺了会报 "Missing charsets…" / "Unable to load any usable fontset"。
pub const CANONICAL_NAME: &str = "-misc-fixed-medium-r-normal--14-130-75-75-c-70-iso8859-1";

/// 单个字符的度量。等宽字体所有字符的度量相同，所以不区分字符。
pub fn charinfo(ch: u16) -> Charinfo {
    let exists = (MIN_CHAR..=MAX_CHAR).contains(&ch);
    Charinfo {
        left_side_bearing: 0,
        right_side_bearing: if exists { CHAR_WIDTH } else { 0 },
        character_width: if exists { CHAR_WIDTH } else { 0 },
        ascent: if exists { ASCENT } else { 0 },
        descent: if exists { DESCENT } else { 0 },
        attributes: 0,
    }
}

/// `min_bounds`：最小字符的边界（我们所有字符一样大）。
pub fn min_bounds() -> Charinfo {
    charinfo(MIN_CHAR)
}

/// `max_bounds`：最大字符的边界。
pub fn max_bounds() -> Charinfo {
    charinfo(MAX_CHAR)
}

/// 取一个字符的点阵。超出字符集时返回缺字字形（`?`）。
pub fn glyph(ch: u16) -> &'static [u8; data::GLYPH_HEIGHT as usize] {
    let code = if (MIN_CHAR..=MAX_CHAR).contains(&ch) {
        ch
    } else {
        DEFAULT_CHAR
    };
    &data::GLYPHS[(code - MIN_CHAR) as usize]
}

/// 一段文本的推进宽度（像素）。
pub fn text_width(len: usize) -> i32 {
    len as i32 * i32::from(CHAR_WIDTH)
}

/// X11 的字体名匹配：`*` 匹配任意长度、`?` 匹配单个字符，其余按字面比较。
///
/// 只做两段式回溯匹配（贪心 + 回退），足够覆盖客户端常用的模式；
/// 正规的 X 实现会把名字拆成 14 个字段再逐字段匹配，这里不需要那么严格。
pub fn matches_pattern(name: &str, pattern: &str) -> bool {
    let name: Vec<char> = name.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let (mut n, mut p) = (0usize, 0usize);
    // 记住最近一个 `*` 的位置，匹配失败时回退
    let mut star: Option<(usize, usize)> = None;

    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            n += 1;
            p += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, n));
            p += 1;
        } else if let Some((star_p, star_n)) = star {
            // 让 `*` 多吃一个字符再试
            p = star_p + 1;
            n = star_n + 1;
            star = Some((star_p, star_n + 1));
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_glyph_has_the_expected_size() {
        assert_eq!(data::GLYPHS.len() as u16, MAX_CHAR - MIN_CHAR + 1);
        assert_eq!(CELL_HEIGHT, ASCENT + DESCENT);
        // 空格不该有任何墨迹
        assert!(glyph(b' ' as u16).iter().all(|row| *row == 0));
        // 常见字符必须有墨迹，否则说明字体表生成有问题
        for ch in [b'A', b'g', b'0', b'_'] {
            assert!(
                glyph(ch as u16).iter().any(|row| *row != 0),
                "{ch} 的字形是空的"
            );
        }
    }

    #[test]
    fn glyph_rows_draw_the_expected_pixels() {
        // 断言"墨迹都在基线以上、基线以下为空"，而不是写死某一行的下标：
        // 字形在格子里从第几行起笔由字体表决定（生成脚本的 padding）。
        let rows = glyph(b'A' as u16);
        assert!(
            rows[..ASCENT as usize].iter().any(|row| *row != 0),
            "基线以上必须有笔画"
        );
        assert!(
            rows[ASCENT as usize..].iter().all(|row| *row == 0),
            "基线以下不该有笔画"
        );
    }

    #[test]
    fn unknown_characters_fall_back_to_question_mark() {
        assert_eq!(glyph(0x0100), glyph(DEFAULT_CHAR));
        assert_eq!(charinfo(0x0100).character_width, 0, "字符集外不该报告宽度");
    }

    #[test]
    fn text_width_is_monospace() {
        assert_eq!(text_width(0), 0);
        assert_eq!(text_width(5), 5 * i32::from(CHAR_WIDTH));
    }

    #[test]
    fn pattern_matching_follows_x11_wildcards() {
        assert!(matches_pattern("fixed", "fixed"));
        assert!(matches_pattern("fixed", "*"));
        assert!(matches_pattern("6x13", "6x13"));
        assert!(matches_pattern("6x13", "?x13"));
        assert!(matches_pattern(
            "-misc-fixed-medium-r-normal--13-120-75-75-c-70-iso8859-1",
            "-misc-fixed-*"
        ));
        assert!(matches_pattern("fixed", "*fix*"));
        assert!(!matches_pattern("fixed", "6x13"));
        // `?` 匹配任意单个字符：这条原来写成 `!matches_pattern`，与上面的
        // `?x13` 用例自相矛盾，测试自身是错的。
        assert!(matches_pattern("fixed", "fixe?"));
        assert!(!matches_pattern("fixe", "fixe?"));
    }

    #[test]
    fn all_candidate_names_match_the_wildcard_listing() {
        for name in FONT_NAMES {
            assert!(matches_pattern(name, "*"), "{name} 应当匹配 *");
        }
    }
}
