//! 键盘映射与输入注入（M5 的核心部分）。
//!
//! 输入有两条独立的路径，必须都成立才算"能操作"：
//!
//! 1. **客户端握手的 keymap**：X 客户端（尤其是 toolkit）启动时会要
//!    `GetKeyboardMapping`，把 keysym 反查成 keycode；`XLookupString` 也靠它把
//!    KeyPress 的 keycode + state 还原成字符。表里没有的 keysym，应用就"打不出字"。
//! 2. **事件投递**：真实 X server 由设备产生事件，我们是把前端画布上的键鼠翻译成
//!    事件帧，按协议的传播规则（子窗口 → 祖先，受 `do_not_propagate` 截断）
//!    投给选择了对应事件掩码的客户端。
//!
//! 键位表用 **Xorg 标准 keycode**（不是 PC scancode）：前端发的是 keysym，
//! 由这里反查 keycode 与是否需要 Shift。这样扩展键（Home/End/方向键）
//! 不会和数字键盘撞车——scancode 近似做法在这里必然踩坑（Home 与 KP_7 同为 0x47）。

use x11rb_protocol::protocol::xproto::*;
use x11rb_protocol::x11_utils::Serialize;

use super::handshake::ROOT_WINDOW;
use super::xinput::device_id;
use super::state::ServerState;

/// 每个 keycode 的 keysym 个数：0 = 不按 Shift，1 = 按住 Shift。
/// 真实的 XKB 是 2~4 组乘 2~8 层，这里只做 US 布局的最小可用集。
pub const KEYSYMS_PER_KEYCODE: u8 = 2;

/// 焦点语义里的 `PointerRoot`：焦点跟随指针位置（X 协议的默认行为）。
pub const POINTER_ROOT: Window = 1;
/// 焦点语义里的 `None`：没有窗口有焦点，键盘事件丢弃。
pub const FOCUS_NONE: Window = 0;

// ---- 修饰键 keycode（Xorg 默认映射）----
const KC_SHIFT_L: u8 = 50;
const KC_SHIFT_R: u8 = 62;
const KC_CONTROL_L: u8 = 37;
const KC_CONTROL_R: u8 = 105;
const KC_ALT_L: u8 = 64;
const KC_ALT_R: u8 = 108;
const KC_CAPS_LOCK: u8 = 66;
const KC_NUM_LOCK: u8 = 77;
const KC_SUPER_L: u8 = 133;
const KC_SUPER_R: u8 = 134;

// ---- KeyButMask 的位（x11rb 里是 newtype，这里直接用数值做位运算）----
pub const MASK_SHIFT: u16 = 1 << 0;
pub const MASK_LOCK: u16 = 1 << 1;
pub const MASK_CONTROL: u16 = 1 << 2;
pub const MASK_MOD1: u16 = 1 << 3;
pub const MASK_MOD4: u16 = 1 << 6;

/// 键位表：`(keycode, 未按 Shift 的 keysym, 按住 Shift 的 keysym)`。
///
/// 按 keycode 升序排列（`keysyms_for` 用二分查找）。未列出的 keycode 一律
/// 回 `NoSymbol`，客户端拿到全 0 只会认为这个键"没有符号"，不会报错。
const KEYMAP: &[(u8, u32, u32)] = &[
    // 主键区第一排
    (9, 0xff1b, 0xff1b),  // Escape
    (10, 0x31, 0x21),     // 1 !
    (11, 0x32, 0x40),     // 2 @
    (12, 0x33, 0x23),     // 3 #
    (13, 0x34, 0x24),     // 4 $
    (14, 0x35, 0x25),     // 5 %
    (15, 0x36, 0x5e),     // 6 ^
    (16, 0x37, 0x26),     // 7 &
    (17, 0x38, 0x2a),     // 8 *
    (18, 0x39, 0x28),     // 9 (
    (19, 0x30, 0x29),     // 0 )
    (20, 0x2d, 0x5f),     // - _
    (21, 0x3d, 0x2b),     // = +
    (22, 0xff08, 0xff08), // BackSpace
    (23, 0xff09, 0xff09), // Tab
    // QWERTY 行
    (24, 0x71, 0x51), // q
    (25, 0x77, 0x57), // w
    (26, 0x65, 0x45), // e
    (27, 0x72, 0x52), // r
    (28, 0x74, 0x54), // t
    (29, 0x79, 0x59), // y
    (30, 0x75, 0x55), // u
    (31, 0x69, 0x49), // i
    (32, 0x6f, 0x4f), // o
    (33, 0x70, 0x50), // p
    (34, 0x5b, 0x7b), // [ {
    (35, 0x5d, 0x7d), // ] }
    (36, 0xff0d, 0xff0d), // Return
    (37, 0xffe3, 0xffe3), // Control_L
    // ASDF 行
    (38, 0x61, 0x41),     // a
    (39, 0x73, 0x53),     // s
    (40, 0x64, 0x44),     // d
    (41, 0x66, 0x46),     // f
    (42, 0x67, 0x47),     // g
    (43, 0x68, 0x48),     // h
    (44, 0x6a, 0x4a),     // j
    (45, 0x6b, 0x4b),     // k
    (46, 0x6c, 0x4c),     // l
    (47, 0x3b, 0x3a),     // ; :
    (48, 0x27, 0x22),     // ' "
    (49, 0x60, 0x7e),     // ` ~
    (50, 0xffe1, 0xffe1), // Shift_L
    (51, 0x5c, 0x7c),     // \ |
    // ZXCV 行
    (52, 0x7a, 0x5a),     // z
    (53, 0x78, 0x58),     // x
    (54, 0x63, 0x43),     // c
    (55, 0x76, 0x56),     // v
    (56, 0x62, 0x42),     // b
    (57, 0x6e, 0x4e),     // n
    (58, 0x6d, 0x4d),     // m
    (59, 0x2c, 0x3c),     // , <
    (60, 0x2e, 0x3e),     // . >
    (61, 0x2f, 0x3f),     // / ?
    (62, 0xffe2, 0xffe2), // Shift_R
    (63, 0xffaa, 0xffaa), // KP_Multiply
    (64, 0xffe9, 0xffe9), // Alt_L
    (65, 0x20, 0x20),     // space
    (66, 0xffe5, 0xffe5), // Caps_Lock
    // F1..F10
    (67, 0xffbe, 0xffbe),
    (68, 0xffbf, 0xffbf),
    (69, 0xffc0, 0xffc0),
    (70, 0xffc1, 0xffc1),
    (71, 0xffc2, 0xffc2),
    (72, 0xffc3, 0xffc3),
    (73, 0xffc4, 0xffc4),
    (74, 0xffc5, 0xffc5),
    (75, 0xffc6, 0xffc6),
    (76, 0xffc7, 0xffc7),
    (77, 0xff7f, 0xff7f), // Num_Lock
    (78, 0xff14, 0xff14), // Scroll_Lock
    // 数字键盘（NumLock 打开时的符号；NumLock 关闭的导航符号不做二次映射）
    (79, 0xffb7, 0xff95), // KP_7 / KP_Home
    (80, 0xffb8, 0xff97), // KP_8 / KP_Up
    (81, 0xffb9, 0xff9a), // KP_9 / KP_Prior
    (82, 0xffad, 0xffad), // KP_Subtract
    (83, 0xffb4, 0xff96), // KP_4 / KP_Left
    (84, 0xffb5, 0xffb5), // KP_5
    (85, 0xffb6, 0xffb6), // KP_6
    (86, 0xffab, 0xffab), // KP_Add
    (87, 0xffb1, 0xff9c), // KP_1 / KP_End
    (88, 0xffb2, 0xff99), // KP_2 / KP_Down
    (89, 0xffb3, 0xff9b), // KP_3 / KP_Next
    (90, 0xffb0, 0xff9e), // KP_0 / KP_Insert
    (91, 0xffae, 0xff9f), // KP_Decimal / KP_Delete
    (92, 0xffe8, 0xffe8), // Meta_L
    (93, 0xffeb, 0xffeb), // Super_L（左侧 Win 键）
    (95, 0xffc8, 0xffc8), // F11
    (96, 0xffc9, 0xffc9), // F12
    // 编辑与导航键
    (104, 0xfffa, 0xfffa), // 未定义键（留空避免越界）
    (105, 0xffe4, 0xffe4), // Control_R
    (106, 0xffe7, 0xffe7), // Meta_R
    (107, 0xffec, 0xffec), // Super_R
    (108, 0xffea, 0xffea), // Alt_R
    (110, 0xff50, 0xff50), // Home
    (111, 0xff52, 0xff52), // Up
    (112, 0xff55, 0xff55), // Prior (Page Up)
    (113, 0xff51, 0xff51), // Left
    (114, 0xff53, 0xff53), // Right
    (115, 0xff57, 0xff57), // End
    (116, 0xff54, 0xff54), // Down
    (117, 0xff56, 0xff56), // Next (Page Down)
    (118, 0xff63, 0xff63), // Insert
    (119, 0xffff, 0xffff), // Delete
    (127, 0xff13, 0xff13), // Pause
];

/// 某个 keycode 的两个 keysym。
pub fn keysyms_for(keycode: u8) -> [u32; 2] {
    match KEYMAP.binary_search_by_key(&keycode, |entry| entry.0) {
        Ok(index) => [KEYMAP[index].1, KEYMAP[index].2],
        Err(_) => [0, 0],
    }
}

/// 从 `first_keycode` 开始取 `count` 个 keycode 的 keysym 平铺表，
/// 供 `GetKeyboardMapping` 回复使用。
pub fn keysym_table(first_keycode: u8, count: u8) -> Vec<u32> {
    let mut table = Vec::with_capacity(usize::from(count) * usize::from(KEYSYMS_PER_KEYCODE));
    for offset in 0..count {
        let keycode = first_keycode.saturating_add(offset);
        table.extend_from_slice(&keysyms_for(keycode));
    }
    table
}

/// `GetModifierMapping` 的回复内容：8 个修饰键行 × 每行 8 个 keycode。
///
/// Xlib/Xt 用它把 `KeyButMask` 的 ShiftMask/ControlMask/Mod1Mask… 对应到具体
/// keycode；回全 0 会让 toolkit 认不出修饰键（快捷键、Shift+方向键选择之类失效）。
/// 行序按协议固定：Shift、Lock、Control、Mod1…Mod5。
pub fn modifier_mapping() -> Vec<Keycode> {
    let mut map = vec![0u8; 8 * 8];
    // 每行一个 keycode：不足 8 个的行其余位置保持 0（表示"没有绑定"）
    map[0] = KC_SHIFT_L;
    map[8] = KC_CAPS_LOCK;
    map[16] = KC_CONTROL_L;
    map[24] = KC_ALT_L; // Mod1
    map[48] = KC_SUPER_L; // Mod4
    map
}

/// keysym → `(keycode, 是否需要 Shift)`。
///
/// 前端发的是 keysym（浏览器已经按系统布局算好了字符），这里反查 keycode；
/// 命中的是第二个 keysym 就要求 Shift 位置位，应用才会按 `XLookupString`
/// 拿到正确的字符。
pub fn keycode_for_keysym(keysym: u32) -> Option<(u8, bool)> {
    if keysym == 0 {
        return None;
    }
    for entry in KEYMAP {
        if entry.1 == keysym {
            return Some((entry.0, false));
        }
        if entry.2 == keysym {
            return Some((entry.0, true));
        }
    }
    None
}

/// keycode 对应的修饰位（没有修饰作用返回 0）。
fn modifier_mask(keycode: u8) -> u16 {
    match keycode {
        KC_SHIFT_L | KC_SHIFT_R => MASK_SHIFT,
        KC_CAPS_LOCK => MASK_LOCK,
        KC_CONTROL_L | KC_CONTROL_R => MASK_CONTROL,
        KC_ALT_L | KC_ALT_R => MASK_MOD1,
        KC_SUPER_L | KC_SUPER_R => MASK_MOD4,
        _ => 0,
    }
}

/// 指针与焦点的当前状态。
///
/// 注意 X11 里焦点和指针是**两件独立的事**：键盘事件去焦点窗口，
/// 指针事件去指针下的窗口。没有窗口管理器时默认焦点是 `PointerRoot`，
/// 我们把"跟随指针"实现成按当时指针位置解析，等价于 X 的 PointerRoot 语义。
/// 一次活动的指针抓取（`XGrabPointer` 成功后必须真正生效）。
///
/// NyaTerm: 以前只回 `Success` 而不重定向事件，导致 GTK 拿到"成功"后进入
/// 拖动/菜单状态机、期待后续指针事件都投到 grab_window，却永远等不到 →
/// 退化成 QueryPointer 轮询（实测 350 次/秒）且不再绘制，表现为"界面卡死但
/// 进程还活着、心跳仍在"。
#[derive(Debug, Clone, Copy)]
pub struct PointerGrab {
    /// 抓取窗口：期间所有指针事件从它开始投递。
    pub window: Window,
    /// 属主事件：指针在属主自己的窗口里时按常规投递。
    pub owner_events: bool,
    /// 抓取期间的事件掩码（限制投递的事件类型）。
    pub event_mask: u32,
    /// 拥有这次抓取的客户端（客户端断开时自动解除）。
    pub client: u32,
}

pub struct InputState {
    pub pointer_x: i16,
    pub pointer_y: i16,
    pub button_mask: u16,
    /// 指针当前所在的最深窗口（用于 Enter/Leave 转换判断）。
    pub pointer_window: Option<Window>,
    /// 隐式抓取窗口：按下鼠标键期间，指针事件（移动/释放）都投给按下时的
    /// 那个窗口，直到所有键抬起。对应 X.org `ActivateImplicitGrab()`
    /// （dix/events.c：`DeliverEventsToWindow` 在投递 ButtonPress 时激活）。
    /// 没有它，拖拽和"按下后窗口树变化"的点击都会丢失——GTK 的点击状态机
    /// 拿不到配对的 Release，页面元素就"点不动"。
    pub implicit_grab: Option<Window>,
    /// 活动的显式指针抓取（优先级高于隐式抓取与位置命中）。
    pub pointer_grab: Option<PointerGrab>,
    /// 活动的键盘抓取窗口（`XGrabKeyboard`）：期间键盘事件都投给它。
    pub keyboard_grab: Option<Window>,
    /// 焦点窗口：`POINTER_ROOT` / `FOCUS_NONE` / 具体窗口 ID。
    pub focus: Window,
    pub revert_to: u8,
    pub modifiers: u16,
    pub keys_down: [bool; 256],
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            pointer_x: 0,
            pointer_y: 0,
            button_mask: 0,
            pointer_window: None,
            implicit_grab: None,
            pointer_grab: None,
            keyboard_grab: None,
            focus: POINTER_ROOT,
            revert_to: u8::from(InputFocus::POINTER_ROOT),
            modifiers: 0,
            keys_down: [false; 256],
        }
    }
}

impl InputState {
    /// 事件帧里 `state` 字段的值：修饰键 + 按住的鼠标键。
    fn event_state(&self) -> u16 {
        self.modifiers | self.button_mask
    }
}

impl ServerState {
    // ---- 坐标 ----

    /// 窗口内容区左上角在屏幕上的绝对坐标（累加祖先的位置与边框）。
    pub fn window_origin(&self, id: Window) -> (i32, i32) {
        let (mut x, mut y) = (0, 0);
        let mut current = id;
        while current != ROOT_WINDOW {
            let Some(window) = self.windows.get(&current) else {
                break;
            };
            x += i32::from(window.x) + i32::from(window.border_width);
            y += i32::from(window.y) + i32::from(window.border_width);
            current = window.parent;
        }
        (x, y)
    }

    /// 屏幕坐标处最深的**可见**窗口（自根窗口按叠放顺序从上往下找）。
    pub fn window_at(&self, x: i16, y: i16) -> Window {
        let mut current = ROOT_WINDOW;
        'descend: loop {
            let Some(window) = self.windows.get(&current) else {
                break;
            };
            // children 的顺序就是叠放顺序，最后一个在最上层，所以要反向遍历
            for child_id in window.children.iter().rev() {
                let Some(child) = self.windows.get(child_id) else {
                    continue;
                };
                if self.map_state(*child_id) != MapState::VIEWABLE {
                    continue;
                }
                let (content_x, content_y) = self.window_origin(*child_id);
                let border = i32::from(child.border_width);
                let left = content_x - border;
                let top = content_y - border;
                let right = content_x + i32::from(child.width) + border;
                let bottom = content_y + i32::from(child.height) + border;
                if i32::from(x) >= left
                    && i32::from(x) < right
                    && i32::from(y) >= top
                    && i32::from(y) < bottom
                    // SHAPE：矩形命中之后还要过 bounding shape（L 形/圆角窗口
                    // 的透明区域不接收输入）
                    && self.shape_contains(*child_id, i32::from(x), i32::from(y))
                {
                    current = *child_id;
                    continue 'descend;
                }
            }
            break;
        }
        current
    }

    /// `parent` 的直接子窗口里，包含屏幕坐标 `(x, y)` 的那个（没有则 0）。
    pub fn child_at(&self, parent: Window, x: i16, y: i16) -> Window {
        let Some(window) = self.windows.get(&parent) else {
            return 0;
        };
        for child_id in window.children.iter().rev() {
            let Some(child) = self.windows.get(child_id) else {
                continue;
            };
            if self.map_state(*child_id) != MapState::VIEWABLE {
                continue;
            }
            let (content_x, content_y) = self.window_origin(*child_id);
            let border = i32::from(child.border_width);
            if i32::from(x) >= content_x - border
                && i32::from(x) < content_x + i32::from(child.width) + border
                && i32::from(y) >= content_y - border
                && i32::from(y) < content_y + i32::from(child.height) + border
                // SHAPE：同 `window_at`，bounding shape 优先于外接矩形
                && self.shape_contains(*child_id, i32::from(x), i32::from(y))
            {
                return *child_id;
            }
        }
        0
    }

    // ---- 事件投递 ----

    /// 把指针事件投给 `start` 及其祖先，直到某个窗口的 `do_not_propagate` 命中。
    ///
    /// `build` 的入参是 `(接收窗口, 该窗口的 child 字段, event_x, event_y)`：
    /// 同一个物理事件对每个接收窗口的坐标都不一样，不能算一次复用。
    ///
    /// NyaTerm（X11 规范 §10.2 传播规则）：事件只投给**从最深窗口往上第一个
    /// 选择该事件的窗口**，传播到此为止——更高层的祖先即使也选了同一事件也
    /// **不再重复收**。此前实现把链上每个选择者都投一遍，firefox 的内容子窗口
    /// 和顶层窗口同属一个客户端，一次物理点击收到两份 ButtonPress/Release，
    /// GDK 的点击状态机被破坏，表现为"点击页面元素没反应"。
    fn propagate_pointer(
        &self,
        start: Window,
        x: i16,
        y: i16,
        mask: u32,
        evtype: u16,
        detail: u32,
        buttons: u16,
        state_mask: u16,
        build: impl Fn(Window, Window, i16, i16) -> Vec<u8>,
    ) {
        // XI2 先行：第一个有 XI2 选择的窗口收到 GenericEvent（此后核心抑制
        // 只对做了 XI2 选择的客户端生效，其余客户端仍走核心）
        let xi2_delivered = self.deliver_xi2_input(start, evtype, detail, x, y, buttons, state_mask, 2);
        let mut current = start;
        loop {
            let Some(window) = self.windows.get(&current) else {
                break;
            };
            let stop = window.do_not_propagate & mask != 0;
            // 有**非 XI2 抑制**的客户端选择该事件 → 投递并停止传播
            let has_receiver = window.event_masks.iter().any(|(&client, &selected)| {
                selected & mask != 0 && !self.core_suppressed(client, evtype, true)
            });
            if has_receiver {
                self.deliver_input_suppressed(current, mask, evtype, |event| {
                    let child = self.child_at(event, x, y);
                    let (origin_x, origin_y) = self.window_origin(event);
                    build(
                        event,
                        child,
                        (i32::from(x) - origin_x) as i16,
                        (i32::from(y) - origin_y) as i16,
                    )
                });
                let _ = xi2_delivered;
                break;
            }
            if stop || current == ROOT_WINDOW {
                break;
            }
            current = window.parent;
        }
    }

    // ---- 指针 ----

    // ---- XI2（XInput2）事件投递 ----
    //
    // XI2 事件是 GenericEvent（type=35）：32 字节信封 + 可变部分。
    // 信封：type(1)=35, extension(1)=XI 操作码, sequence(2), length(4)=信封
    // 之后全部字节/4, evtype(2), deviceid(2), time(4), detail(4), root(4),
    // event(4), child(4)。
    // 之后（xXIDeviceEvent）：root_x/root_y/event_x/event_y 各 FP1616(4B)，
    // sourceid(2)+pad(2), flags(4), buttons_len(2), valuators_len(2),
    // mods(16), group(4), 按钮掩码(buttons_len*4)，valuator 掩码+值。
    //
    // 关键规则（XI2 规范）：事件作为 XI 投递给某客户端后，该事件对它的
    // **核心投递终止**。GDK 选 XI2 时不撤核心掩码——服务端必须抑制，否则
    // 一次点击收两份。

    /// 构造一个 XI2 GenericEvent（输入类：Motion/Buttons/Keys/Crossing/Focus）。
    #[allow(clippy::too_many_arguments)]
    fn build_xi2_input_event(
        &self,
        evtype: u16,
        deviceid: u16,
        sourceid: u16,
        detail: u32,
        event_window: Window,
        child: Window,
        x: i16,
        y: i16,
        state_mask: u16,
        buttons: u16,
    ) -> Vec<u8> {
        let opcode = super::extensions::OPCODE_XINPUT;
        let fp = |v: i16| -> [u8; 4] {
            // FP1616：整数部分 * 65536
            let raw = (i32::from(v) * 65536).to_ne_bytes();
            raw
        };
        let (origin_x, origin_y) = self.window_origin(event_window);
        let mut e = Vec::with_capacity(32 + 48);
        e.push(35); // GenericEvent
        e.push(opcode);
        e.extend_from_slice(&0u16.to_ne_bytes()); // sequence 占位，发送前盖写
        e.extend_from_slice(&0u32.to_ne_bytes()); // length 占位
        e.extend_from_slice(&evtype.to_ne_bytes());
        e.extend_from_slice(&deviceid.to_ne_bytes());
        e.extend_from_slice(&0u32.to_ne_bytes()); // time（见核心路径的同款取舍）
        // 偏移 16..20 的语义**按事件族不同**（XI2proto.h 权威布局）：
        //  - 设备事件（xXIDeviceEvent）：detail 是 u32
        //  - Enter/Leave/FocusIn/FocusOut（xXIEnterEvent，Focus 是同结构别名）：
        //    sourceid(2) + mode(1) + detail(1)
        // 这里曾经一律写 detail(u32) → Enter/Focus 帧里 **sourceid 从未写入**，
        // GDK 在偏移 16 读到 0 → 设备表查 0 号 → NULL → GDK_IS_DEVICE
        // criticals（firefox 启动即崩，实测三连 critical 正是 XI_Enter 分支）。
        let enter_family = matches!(evtype, 7 | 8 | 9 | 10);
        if enter_family {
            e.extend_from_slice(&sourceid.to_ne_bytes()); // 16: sourceid
            e.push(0); // 18: mode = XINotifyNormal
            e.push(3); // 19: detail = XINotifyNonlinear（指针跨窗口，与真实 X 一致）
        } else {
            e.extend_from_slice(&detail.to_ne_bytes()); // 16: detail
        }
        e.extend_from_slice(&ROOT_WINDOW.to_ne_bytes()); // 20 root
        e.extend_from_slice(&event_window.to_ne_bytes()); // 24 event
        e.extend_from_slice(&child.to_ne_bytes()); // 28 child
        // mods（xXIModifierInfo：base/latched/locked/effective 各 u32 = 16B）。
        let write_mods = |e: &mut Vec<u8>| {
            e.extend_from_slice(&u32::from(state_mask).to_ne_bytes()); // base
            e.extend_from_slice(&0u32.to_ne_bytes()); // latched
            e.extend_from_slice(&0u32.to_ne_bytes()); // locked
            e.extend_from_slice(&u32::from(state_mask).to_ne_bytes()); // effective
        };
        // group（xXIGroupInfo：base/latched/locked/effective 各 u8 = 4B）
        let write_group = |e: &mut Vec<u8>| {
            e.extend_from_slice(&[0u8; 4]);
        };
        // XI2 事件布局按 evtype 分两族（XI2proto.h）：
        // - 设备事件（Motion/Key/Button）：xXIDeviceEvent，88B：坐标(32..48)、
        //   buttons_len(48)、valuators_len(50)、sourceid(52)、pad(54)、
        //   flags(56)、mods(60)、group(76)、按钮掩码(80)、valuator 掩码(84)
        // - Enter/Leave/FocusIn/FocusOut：xXIEnterEvent（Focus 是它的 typedef
        //   别名）：坐标(32..48)、same_screen(48 BOOL u32)、focus(52 BOOL u32)、
        //   buttons_len(56)+pad、mods(60)、group(76)、按钮掩码(80)
        //   = 84B。**固定部分必须 ≥ sizeof(xXIEnterEvent)=80B**——不足的话
        //   libXi wireToEnterLeave 判 event_bytes < sizeof(*in) 直接整帧丢弃
        //   （焦点/跨越事件静默消失）。
        match evtype {
            7 | 8 | 9 | 10 => {
                e.extend_from_slice(&fp(x)); // 32 root_x
                e.extend_from_slice(&fp(y)); // 36 root_y
                e.extend_from_slice(&fp((x as i32 - origin_x) as i16)); // 40 event_x
                e.extend_from_slice(&fp((y as i32 - origin_y) as i16)); // 44 event_y
                e.push(1); // 48 same_screen = TRUE（BOOL 在 X11 线格式里是 **1 字节**）
                let is_focus = matches!(evtype, 9 | 10);
                e.push(u8::from(is_focus)); // 49 focus
                e.extend_from_slice(&0u16.to_ne_bytes()); // 50 buttons_len = 0（无尾随掩码）
                write_mods(&mut e); // 52..68
                write_group(&mut e); // 68..72
                // 到此 72 字节 = sizeof(xXIEnterEvent)（BOOL 1 字节版）。
                // 再补 16 字节零填充，让 libXi 的 `event_bytes < sizeof(*in)`
                // 下限校验在两种 BOOL 解释下都通过（BOOL=4B 时 sizeof=80）。
                // 多余尾字节会被按偏移读取的实现忽略；length 字段随实际长度
                // 自洽（send 前由 deliver_xi2_input 回填），不会造成流错位。
                e.extend_from_slice(&[0u8; 16]); // 72..88
            }
            _ => {
                e.extend_from_slice(&fp(x)); // 32 root_x
                e.extend_from_slice(&fp(y)); // 36 root_y
                e.extend_from_slice(&fp((x as i32 - origin_x) as i16)); // 40 event_x
                e.extend_from_slice(&fp((y as i32 - origin_y) as i16)); // 44 event_y
                e.extend_from_slice(&1u16.to_ne_bytes()); // 48 buttons_len（字数）
                // valuators_len = 1：**必须留出这一个字**——GDK 的 translate_axes
                // 会按设备轴数读掩码字节，valuators_len=0 时它仍可能读到帧尾之外
                // （越界读）。发一个全零掩码字最安全（没有置位 → 不读值）。
                e.extend_from_slice(&1u16.to_ne_bytes()); // 50 valuators_len
                e.extend_from_slice(&sourceid.to_ne_bytes()); // 52 sourceid
                e.extend_from_slice(&0u16.to_ne_bytes()); // 54 pad0
                e.extend_from_slice(&0u32.to_ne_bytes()); // 56 flags
                write_mods(&mut e); // 60..76
                write_group(&mut e); // 76..80
                // 80：按钮掩码（buttons_len * 4 字节）；bit (n+7) = 按钮 n
                let mut bm = 0u32;
                for b in 1..=5u16 {
                    if buttons & (1u16 << (7 + b)) != 0 {
                        bm |= 1u32 << (b - 1 + 8);
                    }
                }
                e.extend_from_slice(&bm.to_ne_bytes());
                // 84：valuators 掩码 1 字（全零）→ 共 88 字节
                e.extend_from_slice(&[0u8; 4]);
            }
        }
        e
    }

    /// 把 XI2 事件投给"从 `start` 沿祖先链**第一个**选择了该事件的窗口"的
    /// 全部选择者。返回是否有投递（用于核心抑制判断）。
    fn deliver_xi2_input(
        &self,
        start: Window,
        evtype: u16,
        detail: u32,
        x: i16,
        y: i16,
        buttons: u16,
        state_mask: u16,
        sourceid: u16,
    ) -> bool {
        use super::handshake::ROOT_WINDOW as ROOT;
        // deviceid 语义：指针类事件用 VCP/ALL_MASTER 的选择，键盘类用 VCK/ALL
        let key_like = matches!(evtype, 2 | 3 | 9 | 10);
        let deviceid = if key_like { device_id::VCK } else { device_id::VCP };
        // sourceid 同族：键盘/焦点事件由主键盘"产生"（真实 X.org 里是物理从
        // 键盘；我们无从设备，用 VCK 保证 GDK 查表非 NULL 且语义一致）
        let sourceid = if key_like { device_id::VCK } else { sourceid };

        // 1) 链上第一个有 XI2 选择的窗口
        let mut current = start;
        let mut deliver_window: Option<Window> = None;
        loop {
            if let Some(sel) = self.xi2_selections.get(&current) {
                if super::xinput::xi2_any_selects(sel, evtype) {
                    deliver_window = Some(current);
                    break;
                }
            }
            if current == ROOT {
                break;
            }
            match self.windows.get(&current) {
                Some(w) => current = w.parent,
                None => break,
            }
        }
        let Some(win_id) = deliver_window else {
            return false;
        };

        // 2) 事件窗口 = 投递窗口；child = 该窗口的直接子窗口中含指针者
        let child = self.child_at(win_id, x, y);
        let frame = self.build_xi2_input_event(
            evtype, deviceid, sourceid, detail, win_id, child, x, y, state_mask, buttons,
        );
        // length = 信封之后全部字节 / 4
        let len_units = ((frame.len() - 32) / 4) as u32;
        let mut frame = frame;
        frame[4..8].copy_from_slice(&len_units.to_ne_bytes());

        // 3) 发给该窗口上选择了该事件的全部 XI2 客户端（按各自序列号盖写）
        // 白屏/criticals 排查：限频（≥1s）记录每个 XI2 事件的投递要素。
        {
            use std::sync::atomic::{AtomicU64, Ordering};
            static LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
            let now_ms = {
                use std::sync::OnceLock;
                static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
                EPOCH.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
            };
            let last = LAST_LOG_MS.load(Ordering::Relaxed);
            if now_ms.saturating_sub(last) >= 1000
                && LAST_LOG_MS
                    .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                crate::xsrv::log_line(format_args!(
                    "[xi2-deliver] evtype={} deviceid={} sourceid={} win={:#x} start={:#x} detail={} xy=({},{}) buttons={:#x} state={:#x}",
                    evtype, deviceid, sourceid, win_id, start, detail, x, y, buttons, state_mask,
                ));
            }
        }
        let Some(selectors) = self.xi2_selections.get(&win_id) else {
            return true;
        };
        for (&client, sel) in selectors {
            if !self.xi2_enabled(client) {
                continue;
            }
            if !super::xinput::xi2_client_selects(selectors, client, evtype) {
                continue;
            }
            let _ = sel;
            if let Some(handle) = self.clients.get(&client) {
                // XI2 GenericEvent 是完整格式（自带尾部填充），**不能**过
                // pad_event——它会无条件 resize 到 32 字节，把 88 字节的
                // xXIDeviceEvent 截断（wire 测试 xi2_motion_generic 抓到）。
                let mut f = frame.clone();
                self.stamp_sequence(client, &mut f);
                let _ = handle.sender.send(f);
            }
        }
        true
    }

    /// 客户端是否应抑制核心输入事件（XI2 已协商且在指针下窗口链上做过
    /// XI2 选择）。XI2 规范：XI 投递后核心投递终止——双投会毁掉点击状态机。
    /// 当前 X 时间戳（服务端启动后毫秒数，截断为 u32）。
    pub fn event_time(&self) -> u32 {
        self.start_time.elapsed().as_millis() as u32
    }

    pub fn core_suppressed(&self, client: u32, event_type: u16, pointer: bool) -> bool {
        if !self.xi2_enabled(client) {
            return false;
        }
        // 该客户端是否在任何窗口选择了对应 XI2 事件（全窗口表扫描，客户端
        // 数量小、事件率可接受；如成为热点可改为按客户端索引）
        let deviceids: [u16; 3] = if pointer {
            [device_id::ALL, device_id::ALL_MASTER, device_id::VCP]
        } else {
            [device_id::ALL, device_id::ALL_MASTER, device_id::VCK]
        };
        let bit = 1u32 << event_type;
        for sel in self.xi2_selections.values() {
            if super::xinput::xi2_client_selects(sel, client, event_type) {
                return true;
            }
            // ALL/ALL_MASTER 语义：客户端以这些 deviceid 选择时按事件类型等价
            for id in deviceids {
                if let Some(Some(sel_one)) = sel.get(&client).map(Some) {
                    if let Some(m) = sel_one.masks.get(&id) {
                        if m & bit != 0 {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// 指针移动。返回是否有窗口需要重绘（本实现恒 false，帧由合成序号驱动）。
    pub fn pointer_motion(&mut self, x: i16, y: i16) {
        let (width, height) = match self.windows.get(&ROOT_WINDOW) {
            Some(root) => (root.width as i16, root.height as i16),
            None => return,
        };
        let x = x.clamp(0, width.saturating_sub(1));
        let y = y.clamp(0, height.saturating_sub(1));

        // 显式抓取优先：期间所有指针事件从抓取窗口开始投递（XGrabPointer 语义）
        let target = match self.input.pointer_grab {
            Some(grab) => grab.window,
            None => self
                .input
                .implicit_grab
                .unwrap_or_else(|| self.window_at(x, y)),
        };
        let previous = self.input.pointer_window;
        self.input.pointer_x = x;
        self.input.pointer_y = y;
        self.input.pointer_window = Some(target);

        // Enter/Leave：只在"最深窗口"发生变化时发，够 toolkit 更新光标与 hover 状态。
        if previous != Some(target) {
            let state = self.input.event_state();
            if let Some(old) = previous {
                self.deliver_input_suppressed(old, u32::from(EventMask::LEAVE_WINDOW), 8, |event| {
                    let (origin_x, origin_y) = self.window_origin(event);
                    LeaveNotifyEvent {
                        response_type: LEAVE_NOTIFY_EVENT,
                        detail: NotifyDetail::ANCESTOR,
                        sequence: 0,
                        time: self.event_time(),
                        root: ROOT_WINDOW,
                        event,
                        child: 0,
                        root_x: x,
                        root_y: y,
                        event_x: (i32::from(x) - origin_x) as i16,
                        event_y: (i32::from(y) - origin_y) as i16,
                        state: KeyButMask::from(state),
                        mode: NotifyMode::NORMAL,
                        same_screen_focus: 0,
                    }
                    .serialize()
                    .to_vec()
                });
            }
            if target != ROOT_WINDOW {
                self.deliver_input_suppressed(target, u32::from(EventMask::ENTER_WINDOW), 7, |event| {
                    let (origin_x, origin_y) = self.window_origin(event);
                    EnterNotifyEvent {
                        response_type: ENTER_NOTIFY_EVENT,
                        detail: NotifyDetail::ANCESTOR,
                        sequence: 0,
                        time: self.event_time(),
                        root: ROOT_WINDOW,
                        event,
                        child: 0,
                        root_x: x,
                        root_y: y,
                        event_x: (i32::from(x) - origin_x) as i16,
                        event_y: (i32::from(y) - origin_y) as i16,
                        state: KeyButMask::from(state),
                        mode: NotifyMode::NORMAL,
                        same_screen_focus: 0,
                    }
                    .serialize()
                    .to_vec()
                });
            }
        }

        let state = self.input.event_state();
        // 按住鼠标键时，MotionNotify 的过滤器还要包含对应 ButtonNMotionMask
        // （X.org `SetMaskForEvent`：每次按下都会改写 MotionNotify 的过滤器），
        // 否则拖拽（选中文字、拖滚动条）收不到运动事件。
        // `button_mask` 的位就是 ButtonNMotionMask：1<<(7+n)。
                // NyaTerm: XI2 crossing（Enter/Leave）——GDK 在 XI2 模式下只读 XI2
        // 版本的 crossing 事件；核心版本对做了 XI2 选择的客户端被抑制。
        if let Some(old) = previous {
            self.deliver_xi2_input(old, 8 /* Leave */, 0, x, y, self.input.button_mask, state, 2);
        }
        if let Some(new_target) = self.input.pointer_window {
            if new_target != ROOT_WINDOW {
                self.deliver_xi2_input(new_target, 7 /* Enter */, 0, x, y, self.input.button_mask, state, 2);
            }
        }
let motion_mask = if self.input.button_mask == 0 {
            u32::from(EventMask::POINTER_MOTION)
        } else {
            u32::from(EventMask::POINTER_MOTION) | u32::from(self.input.button_mask)
        };
        self.propagate_pointer(
            target,
            x,
            y,
            motion_mask,
            6, // XI_Motion
            0,
            self.input.button_mask,
            state,
            |event, child, event_x, event_y| {
                MotionNotifyEvent {
                    response_type: MOTION_NOTIFY_EVENT,
                    detail: Motion::NORMAL,
                    sequence: 0,
                    time: self.event_time(),
                    root: ROOT_WINDOW,
                    event,
                    child,
                    root_x: x,
                    root_y: y,
                    event_x,
                    event_y,
                    state: KeyButMask::from(state),
                    same_screen: true,
                }
                .serialize()
                .to_vec()
            },
        );
    }

    /// 指针移动 + 以 UI 的按键掩码为准对账服务端的按键状态。
    ///
    /// `buttons` 位：1=左 2=中 4=右（与 UI 侧同一个位掩码）。
    ///
    /// 为什么需要：UI 只在"指针在画面内移动"时上报按键状态。如果按下之后
    /// 在画面外松开（或窗口失焦、滚轮拖滚动条时移出区域），我们收不到抬起
    /// 事件，`button_mask` 与隐式抓取就永远停在按下状态——此后所有指针事件
    /// 都投给旧窗口，用户感觉"界面突然操作不了了"。每次移动都对账即可自愈。
    pub fn pointer_move_with_buttons(&mut self, x: i16, y: i16, buttons: u8) {
        // 先移动：坐标要先更新，pointer_button 用的是 self.input.pointer_x/y
        self.pointer_motion(x, y);
        for (bit, button) in [(1u8, 1u8), (2u8, 2u8), (4u8, 3u8)] {
            let server_down = self.input.button_mask & (1u16 << (7 + button)) != 0;
            let ui_down = buttons & bit != 0;
            if server_down != ui_down {
                crate::xsrv::log_line(format_args!(
                    "[btn] 对账修正 button={button} 服务端按下={server_down} UI按下={ui_down} \
                     mask={:#06x} grab={:?} at=({x},{y})",
                    self.input.button_mask, self.input.implicit_grab
                ));
                self.pointer_button(button, ui_down);
            }
        }
    }

    /// 鼠标键按下/抬起。按下时顺带把焦点给指针下的窗口：
    /// 我们没有窗口管理器，不这么做的话应用永远收不到键盘。
    pub fn pointer_button(&mut self, button: u8, pressed: bool) {
        let (x, y) = (self.input.pointer_x, self.input.pointer_y);
        // 显式抓取优先，其次隐式抓取，最后按位置命中（X.org ActivateImplicitGrab
        // / XGrabPointer 的合成语义）
        let target = match self.input.pointer_grab {
            Some(grab) => grab.window,
            None => self
                .input
                .implicit_grab
                .unwrap_or_else(|| self.window_at(x, y)),
        };
        if target != ROOT_WINDOW {
            self.input.pointer_window = Some(target);
        }

        if pressed && button <= 5 {
            let bit = 1u16 << (7 + button);
            // 第一个按下的键建立隐式抓取：之后直到全部抬起，指针事件都投给
            // 这个窗口（拖拽，以及"按下后窗口树变化"的点击都依赖它）。
            if self.input.button_mask == 0 {
                self.input.implicit_grab = Some(target);
            }
            if self.input.focus != target {
                self.set_input_focus(target, u8::from(InputFocus::POINTER_ROOT));
            }
            // X 协议里 ButtonPress 的 state 不含本次按下的键
            let state = self.input.event_state();
            self.propagate_pointer(
                target,
                x,
                y,
                u32::from(EventMask::BUTTON_PRESS),
                4, // XI_ButtonPress
                u32::from(button),
                // XI2 帧的按钮掩码反映**按下之后**的状态（核心 state 字段仍不含
                // 本按钮，保持 X 核心协议语义不变）
                self.input.button_mask | bit,
                state,
                |event, child, event_x, event_y| {
                    ButtonPressEvent {
                        response_type: BUTTON_PRESS_EVENT,
                        detail: button,
                        sequence: 0,
                        time: self.event_time(),
                        root: ROOT_WINDOW,
                        event,
                        child,
                        root_x: x,
                        root_y: y,
                        event_x,
                        event_y,
                        state: KeyButMask::from(state),
                        same_screen: true,
                    }
                    .serialize()
                    .to_vec()
                },
            );
            self.input.button_mask |= bit;
            // NyaTerm 探针（低频）：按键边沿 + 抓取状态。卡死问题就靠这几行定位。
            crate::xsrv::log_line(format_args!(
                "[btn] button={button} down mask={:#06x} grab={:?} target={target:#x} at=({x},{y})",
                self.input.button_mask, self.input.implicit_grab
            ));
        } else {
            self.input.button_mask &= !(1u16 << (7 + button.min(5)));
            crate::xsrv::log_line(format_args!(
                "[btn] button={button} up mask={:#06x} grab={:?} target={target:#x} at=({x},{y})",
                self.input.button_mask, self.input.implicit_grab
            ));
            let state = self.input.event_state();
            self.propagate_pointer(
                target,
                x,
                y,
                u32::from(EventMask::BUTTON_RELEASE),
                5, // XI_ButtonRelease
                u32::from(button),
                self.input.button_mask,
                state,
                |event, child, event_x, event_y| {
                    ButtonReleaseEvent {
                        response_type: BUTTON_RELEASE_EVENT,
                        detail: button,
                        sequence: 0,
                        time: self.event_time(),
                        root: ROOT_WINDOW,
                        event,
                        child,
                        root_x: x,
                        root_y: y,
                        event_x,
                        event_y,
                        state: KeyButMask::from(state),
                        same_screen: true,
                    }
                    .serialize()
                    .to_vec()
                },
            );
            // 所有键都抬起 → 释放隐式抓取（必须在投递 Release 之后）
            if self.input.button_mask == 0 {
                self.input.implicit_grab = None;
            }
        }
    }

    /// 滚轮：X 核心协议用 4/5 号键表示上下滚动（横向用 6/7）。
    pub fn pointer_scroll(&mut self, up: bool) {
        let button = if up { 4 } else { 5 };
        self.pointer_button(button, true);
        self.pointer_button(button, false);
    }

    // ---- 键盘 ----

    /// 键盘事件。`keycode` 由 `keycode_for_keysym` 反查得到；
    /// `forced_shift` 表示这个 keysym 只有按住 Shift 才打得出来。
    pub fn key_event(&mut self, keycode: u8, pressed: bool, forced_shift: bool) {
        if keycode == 0 {
            return;
        }
        let modifier = modifier_mask(keycode);

        // X 的 state 字段是"事件发生后的修饰状态"，但 KeyPress 自己的修饰位
        // 要在按下之后才算——顺序反了 Shift+A 会被应用当成 'a'。
        let before = self.input.event_state();
        let state = match (pressed, forced_shift) {
            (true, true) => before | MASK_SHIFT,
            _ => before,
        };

        let target = self.focus_target();
        if target != ROOT_WINDOW && target != FOCUS_NONE {
            let (x, y) = (self.input.pointer_x, self.input.pointer_y);
            let event_type = if pressed {
                KEY_PRESS_EVENT
            } else {
                KEY_RELEASE_EVENT
            };
            let mask = if pressed {
                u32::from(EventMask::KEY_PRESS)
            } else {
                u32::from(EventMask::KEY_RELEASE)
            };
            let evtype_xi = if pressed { 2u16 } else { 3u16 };
            self.propagate_pointer(
                target,
                x,
                y,
                mask,
                evtype_xi,
                u32::from(keycode),
                0,
                state,
                |event, child, event_x, event_y| {
                    KeyPressEvent {
                        response_type: event_type,
                        detail: keycode,
                        sequence: 0,
                        time: self.event_time(),
                        root: ROOT_WINDOW,
                        event,
                        child,
                        root_x: x,
                        root_y: y,
                        event_x,
                        event_y,
                        state: KeyButMask::from(state),
                        same_screen: true,
                    }
                    .serialize()
                    .to_vec()
                },
            );
        }

        // 修饰状态在事件之后更新：上面算的 state 已是"按下后"的值
        if pressed {
            self.input.modifiers |= modifier;
            self.input.keys_down[usize::from(keycode)] = true;
        } else {
            self.input.modifiers &= !modifier;
            self.input.keys_down[usize::from(keycode)] = false;
        }
    }

    /// 焦点落在哪个窗口上：`PointerRoot` 解析成指针下的窗口。
    fn focus_target(&self) -> Window {
        // 键盘抓取期间键盘事件投给抓取窗口（菜单/快捷键依赖，
        // XGrabKeyboard 的语义；否则 GTK 的菜单状态机同样会空转）
        if let Some(window) = self.input.keyboard_grab {
            return window;
        }
        match self.input.focus {
            POINTER_ROOT => self
                .input
                .pointer_window
                .unwrap_or_else(|| self.window_at(self.input.pointer_x, self.input.pointer_y)),
            FOCUS_NONE => FOCUS_NONE,
            window => window,
        }
    }

    /// 设置输入焦点，并按协议补发 FocusOut / FocusIn。
    pub fn set_input_focus(&mut self, focus: Window, revert_to: u8) {
        let old = self.focus_target();
        if focus != POINTER_ROOT && focus != FOCUS_NONE && !self.windows.contains_key(&focus) {
            return;
        }
        self.input.focus = focus;
        self.input.revert_to = revert_to;
        let new = self.focus_target();
        if old == new {
            return;
        }
        if old != FOCUS_NONE && old != ROOT_WINDOW {
            self.deliver_input_suppressed(old, u32::from(EventMask::FOCUS_CHANGE), 10, |event| {
                FocusOutEvent {
                    response_type: FOCUS_OUT_EVENT,
                    detail: NotifyDetail::ANCESTOR,
                    sequence: 0,
                    event,
                    mode: NotifyMode::NORMAL,
                }
                .serialize()
                .to_vec()
            });
            // NyaTerm: XI2 版本的 FocusOut（GDK XI2 模式只读 XI2）
            self.deliver_xi2_input(old, 10 /* FocusOut */, 0, self.input.pointer_x, self.input.pointer_y, 0, 0, 2);
        }
        if new != FOCUS_NONE && new != ROOT_WINDOW {
            self.deliver_input_suppressed(new, u32::from(EventMask::FOCUS_CHANGE), 9, |event| {
                FocusInEvent {
                    response_type: FOCUS_IN_EVENT,
                    detail: NotifyDetail::ANCESTOR,
                    sequence: 0,
                    event,
                    mode: NotifyMode::NORMAL,
                }
                .serialize()
                .to_vec()
            });
            // NyaTerm: XI2 版本的 FocusIn
            self.deliver_xi2_input(new, 9 /* FocusIn */, 0, self.input.pointer_x, self.input.pointer_y, 0, 0, 2);
        }
    }

    /// `QueryPointer` 需要的当前指针信息：`(root_x, root_y, child, mask)`。
    pub fn pointer_snapshot(&self) -> (i16, i16, Window, u16) {
        (
            self.input.pointer_x,
            self.input.pointer_y,
            self.input.pointer_window.unwrap_or(0),
            self.input.event_state(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use x11rb_protocol::protocol::xinput;
    use x11rb_protocol::protocol::xproto;
    use x11rb_protocol::protocol::Request;

    use super::*;

    fn fixture() -> (ServerState, u32, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
        let mut state = ServerState::new(Default::default());
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, sender);
        (state, 0, receiver)
    }

    fn ok(state: &mut ServerState, client: u32, sequence: u16, request: Request<'static>) {
        state
            .handle(client, sequence, &request)
            .unwrap_or_else(|e| panic!("请求失败：{e:?}"));
    }

    /// 建窗口 → 选 ButtonPress/ButtonRelease/PointerMotion → 映射。
    fn make_window(state: &mut ServerState, client: u32, seq: &mut u16, wid: u32) {
        ok(state, client, *seq, Request::CreateWindow(xproto::CreateWindowRequest {
            depth: 0,
            wid,
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: WindowClass::INPUT_OUTPUT,
            visual: 0,
            value_list: Cow::Owned(xproto::CreateWindowAux::default()),
        }));
        *seq += 1;
        ok(state, client, *seq, Request::ChangeWindowAttributes(xproto::ChangeWindowAttributesRequest {
            window: wid,
            value_list: Cow::Owned(xproto::ChangeWindowAttributesAux {
                event_mask: Some(EventMask::from(
                    u32::from(EventMask::BUTTON_PRESS)
                        | u32::from(EventMask::BUTTON_RELEASE)
                        | u32::from(EventMask::POINTER_MOTION),
                )),
                ..Default::default()
            }),
        }));
        *seq += 1;
        ok(state, client, *seq, Request::MapWindow(xproto::MapWindowRequest { window: wid }));
        *seq += 1;
    }

    /// 数出通道里指定响应类型的事件条数（顺带清空通道）。
    fn count_type(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        response_type: u8,
    ) -> usize {
        let mut n = 0;
        while let Ok(frame) = rx.try_recv() {
            if frame.first() == Some(&response_type) {
                n += 1;
            }
        }
        n
    }

    /// X.org `DeliverDeviceEvents` 的传播规则：事件投给祖先链上**第一个**
    /// 成功投递的窗口，祖先不再重复收。此前实现对链上每个选择者都投一遍，
    /// firefox 的内容窗口与顶层窗口同属一个客户端，一次点击收到两份
    /// press/release —— GDK 点击状态机被破坏，页面元素"点不动"。
    #[test]
    fn button_press_is_delivered_once_not_to_every_selecting_ancestor() {
        let (mut state, client, mut rx) = fixture();
        let child = 0x0020_0001u32;
        let mut seq = 1;
        make_window(&mut state, client, &mut seq, child);

        // 根窗口也选择 ButtonPress：真实 X 里祖先不会因此重复收到
        ok(&mut state, client, seq, Request::ChangeWindowAttributes(xproto::ChangeWindowAttributesRequest {
            window: ROOT_WINDOW,
            value_list: Cow::Owned(xproto::ChangeWindowAttributesAux {
                event_mask: Some(EventMask::BUTTON_PRESS),
                ..Default::default()
            }),
        }));

        state.input.pointer_x = 50;
        state.input.pointer_y = 50;
        state.pointer_button(1, true);

        assert_eq!(
            count_type(&mut rx, BUTTON_PRESS_EVENT),
            1,
            "一次点击只能投递一份 ButtonPress"
        );
    }

    /// 隐式抓取（X.org `ActivateImplicitGrab`）：按下后指针移出窗口，
    /// Release 仍投给按下时的窗口；全部抬起后抓取释放。
    #[test]
    fn implicit_grab_keeps_release_on_the_press_window() {
        let (mut state, client, mut rx) = fixture();
        let child = 0x0020_0001u32;
        let mut seq = 1;
        make_window(&mut state, client, &mut seq, child);

        state.input.pointer_x = 50;
        state.input.pointer_y = 50;
        state.pointer_button(1, true);
        assert_eq!(count_type(&mut rx, BUTTON_PRESS_EVENT), 1);
        assert_eq!(state.input.implicit_grab, Some(child), "按下应建立隐式抓取");

        // 指针移出窗口（屏幕另一角）再抬起
        state.pointer_motion(400, 400);
        state.pointer_button(1, false);

        assert_eq!(
            count_type(&mut rx, BUTTON_RELEASE_EVENT),
            1,
            "Release 必须投给按下时的窗口"
        );
        assert_eq!(state.input.implicit_grab, None, "全部抬起后抓取应释放");
    }

    /// 自愈：UI 丢掉抬起事件时（鼠标在画面外松开、窗口失焦），下一次指针
    /// 移动对账要补发 Release 并释放隐式抓取。否则此后所有指针事件都投给
    /// 那个旧窗口，用户感觉"过一会界面就操作不了"。
    #[test]
    fn pointer_move_reconciles_a_lost_button_release() {
        let (mut state, client, mut rx) = fixture();
        let child = 0x0020_0001u32;
        let mut seq = 1;
        make_window(&mut state, client, &mut seq, child);

        state.input.pointer_x = 50;
        state.input.pointer_y = 50;
        state.pointer_button(1, true);
        assert_eq!(count_type(&mut rx, BUTTON_PRESS_EVENT), 1);
        assert_eq!(state.input.implicit_grab, Some(child), "按下应建立抓取");
        assert_ne!(state.input.button_mask, 0);

        // UI 之后只报移动、不再报抬起（buttons = 0）
        state.pointer_move_with_buttons(60, 60, 0);

        assert_eq!(state.input.button_mask, 0, "对账应清掉服务端仍按着的键");
        assert_eq!(state.input.implicit_grab, None, "对账应释放隐式抓取");
        assert_eq!(
            count_type(&mut rx, BUTTON_RELEASE_EVENT),
            1,
            "对账应补发一份 ButtonRelease"
        );
    }

    /// 取出事件帧里的 `event`（接收窗口）字段：32 字节事件里偏移 4..8。
    fn take_event_windows(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        response_type: u8,
    ) -> Vec<u32> {
        let mut out = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            if frame.len() >= 8 && frame[0] == response_type {
                out.push(u32::from_ne_bytes(frame[12..16].try_into().unwrap()));
            }
        }
        out
    }

    /// 回归：QueryPointer 的 `child` 必须是"查询窗口的**直接**子窗口且包含
    /// 指针"，否则 None。X.org 语义；GDK 的 window_at_position() 用 child 做
    /// 下降迭代（child==None 才停）——返回非直系子窗口会让主线程卡死在单个
    /// GDK 调用里（QueryPointer 330 次/秒 + 停止绘制 = 用户实测"操作一会
    /// 就卡死"）。
    #[test]
    fn query_pointer_child_is_none_when_pointer_is_not_in_a_direct_child() {
        let (mut state, client, mut rx) = fixture();
        let win = 0x0020_0001u32;
        let mut seq = 1;
        make_window(&mut state, client, &mut seq, win);

        // 指针在窗口内
        state.input.pointer_x = 50;
        state.input.pointer_y = 50;

        state
            .handle(0, seq, &Request::QueryPointer(xproto::QueryPointerRequest { window: win }))
            .expect("QueryPointer 应当成功");

        // 收应答：XError 之外的第一条 = QueryPointerReply；child 字段在偏移
        // 12..16（8 头 + root 4，然后才是 child）
        let reply = loop {
            match rx.try_recv() {
                Ok(frame) if frame.len() >= 32 && frame[0] == 1 => break frame,
                Ok(_) => continue,
                Err(_) => panic!("没有收到 QueryPointer 应答"),
            }
        };
        let child = u32::from_ne_bytes(reply[12..16].try_into().unwrap());
        assert_eq!(
            child, 0,
            "指针不在任何直接子窗口里时 child 必须为 None——\
             返回非 None 会让 GDK 的下降循环卡死"
        );

        // 指针移出窗口后再查根窗口：child 依然不应是"查询窗口自己"
        state.input.pointer_x = 900;
        state.input.pointer_y = 900;
        seq += 1;
        state
            .handle(0, seq, &Request::QueryPointer(xproto::QueryPointerRequest { window: win }))
            .expect("QueryPointer 应当成功");
        // 静默消费（不做二次断言，覆盖越界路径即可）
    }

    /// 回归：`XGrabPointer` 必须**真正重定向**指针事件。
    ///
    /// 历史 bug：只回 `Success` 却不重定向 → GTK 的拖动/菜单状态机永远等不到
    /// 事件，退化成 QueryPointer 轮询（实测 350 次/秒）并停止绘制，界面看起来
    /// "卡死"但进程还活着（心跳仍在）。
    #[test]
    fn explicit_pointer_grab_redirects_events_to_the_grab_window() {
        let (mut state, client, mut rx) = fixture();
        let lower = 0x0020_0001u32;
        let upper = 0x0020_0002u32;
        let mut seq = 1;
        make_window(&mut state, client, &mut seq, lower);
        make_window(&mut state, client, &mut seq, upper); // 后建的叠在上面

        // 未抓取时：指针在 (50,50) 命中顶层窗口 upper
        state.input.pointer_x = 50;
        state.input.pointer_y = 50;
        state.pointer_motion(50, 50);
        assert_eq!(
            take_event_windows(&mut rx, MOTION_NOTIFY_EVENT),
            vec![upper],
            "未抓取时事件应投给指针下的顶层窗口"
        );

        // 抓取 lower（owner_events=false）
        state
            .handle(0, seq, &Request::GrabPointer(xproto::GrabPointerRequest {
                owner_events: false,
                grab_window: lower,
                event_mask: EventMask::POINTER_MOTION,
                pointer_mode: xproto::GrabMode::ASYNC,
                keyboard_mode: xproto::GrabMode::ASYNC,
                confine_to: 0,
                cursor: 0,
                time: 0,
            }))
            .expect("GrabPointer 应当成功");
        seq += 1;

        state.pointer_motion(60, 60);
        assert_eq!(
            take_event_windows(&mut rx, MOTION_NOTIFY_EVENT),
            vec![lower],
            "抓取期间指针事件必须投给抓取窗口（而不是指针下的窗口）"
        );

        // 解除抓取后恢复按位置投递
        state
            .handle(0, seq, &Request::UngrabPointer(xproto::UngrabPointerRequest { time: 0 }))
            .expect("UngrabPointer 应当成功");
        state.pointer_motion(70, 70);
        assert_eq!(
            take_event_windows(&mut rx, MOTION_NOTIFY_EVENT),
            vec![upper],
            "解除抓取后应恢复按位置投递"
        );
    }
    /// XI2 版本协商成功 + XISelectEvents 之后：运动事件必须以 **XI2
    /// GenericEvent**（type=35）投递，且该客户端**不再收到**核心 MotionNotify
    /// （XI2 规范：XI 投递后核心投递终止；双投会让 GDK 的点击状态机坏掉）。
    #[test]
    fn xi2_selection_switches_delivery_to_generic_events() {
        let (mut state, client, mut rx) = fixture();
        let win = 0x0020_0001u32;
        let mut seq = 1;
        make_window(&mut state, client, &mut seq, win);

        // XIQueryVersion：应答 (2,0)
        state
            .handle(0, seq, &Request::XinputXIQueryVersion(xinput::XIQueryVersionRequest {
                major_version: 2,
                minor_version: 4,
            }))
            .expect("XIQueryVersion 应当成功");
        seq += 1;
        assert!(state.xi2_enabled(client), "协商后 XI2 应当可用");

        // XISelectEvents：root 窗口、AllMasterDevices、Motion 掩码
        state
            .handle(0, seq, &Request::XinputXISelectEvents(xinput::XISelectEventsRequest {
                window: ROOT_WINDOW,
                masks: Cow::Owned(vec![xinput::EventMask {
                    deviceid: xinput::DeviceId::from(device_id::ALL_MASTER),
                    mask: vec![xinput::XIEventMask::MOTION],
                }]),
            }))
            .expect("XISelectEvents 应当成功");
        seq += 1;

        state.input.pointer_x = 50;
        state.input.pointer_y = 50;
        state.pointer_motion(50, 50);

        // XI2 GenericEvent：type=35
        let mut generic = 0;
        let mut core_motion = 0;
        while let Ok(frame) = rx.try_recv() {
            if frame.len() >= 32 {
                if frame[0] == 35 && u16::from_ne_bytes(frame[8..10].try_into().unwrap()) == 6 {
                    generic += 1;
                }
                if frame[0] == MOTION_NOTIFY_EVENT {
                    core_motion += 1;
                }
            }
        }
        assert!(generic >= 1, "必须收到 XI2 GenericEvent(Motion)");
        assert_eq!(
            core_motion, 0,
            "XI2 客户端不得再收到核心 MotionNotify（双投毁点击）"
        );
    }
    /// XI2 crossing：客户端在窗口上选择了 XI_Enter/Leave 后，指针进出该窗口
    /// 必须收到 XI2 GenericEvent 版本的 Enter/Leave（GDK XI2 模式只读 XI2）。
    #[test]
    fn xi2_enter_leave_delivered_to_selecting_clients() {
        let (mut state, client, mut rx) = fixture();
        let win = 0x0020_0001u32;
        let mut seq = 1;
        make_window(&mut state, client, &mut seq, win);

        // XIQueryVersion + 在窗口上选 Enter+Leave
        state
            .handle(0, seq, &Request::XinputXIQueryVersion(xinput::XIQueryVersionRequest {
                major_version: 2,
                minor_version: 4,
            }))
            .expect("XIQueryVersion 应当成功");
        seq += 1;
        state
            .handle(0, seq, &Request::XinputXISelectEvents(xinput::XISelectEventsRequest {
                window: win,
                masks: Cow::Owned(vec![xinput::EventMask {
                    deviceid: xinput::DeviceId::from(device_id::ALL_MASTER),
                    mask: vec![
                        xinput::XIEventMask::ENTER,
                        xinput::XIEventMask::LEAVE,
                    ],
                }]),
            }))
            .expect("XISelectEvents 应当成功");
        seq += 1;

        // 同时也选择**核心** Enter（GDK XI2 模式不撤核心掩码——这正是双投
        // 风险所在，服务端必须抑制）
        state
            .handle(0, seq, &Request::ChangeWindowAttributes(xproto::ChangeWindowAttributesRequest {
                window: win,
                value_list: Cow::Owned(xproto::ChangeWindowAttributesAux {
                    event_mask: Some(EventMask::ENTER_WINDOW),
                    ..Default::default()
                }),
            }))
            .expect("核心 ENTER 选择应当成功");
        seq += 1;

        // 指针先在外面（400,400），再进入窗口（50,50）
        state.pointer_motion(400, 400);
        state.pointer_motion(50, 50);

        // 数事件：XI2 GenericEvent(Enter,7) 至少一份；**核心 EnterNotify
        // （响应类型也是 7，但 frame[0]=7 而非 35）必须是 0（被抑制）**
        let mut enters = 0;
        let mut leaves = 0;
        let mut core_enter = 0;
        while let Ok(frame) = rx.try_recv() {
            if frame.len() >= 32 && frame[0] == 35 {
                match u16::from_ne_bytes(frame[8..10].try_into().unwrap()) {
                    7 => enters += 1,
                    8 => leaves += 1,
                    _ => {}
                }
            } else if frame.len() >= 32 && frame[0] == 7 {
                core_enter += 1;
            }
        }
        assert!(enters >= 1, "必须收到 XI2 GenericEvent(Enter)");
        assert_eq!(
            core_enter, 0,
            "XI2 客户端不得再收到核心 EnterNotify（双投毁悬停状态机）"
        );
    }
    /// XI2 GenericEvent 线格式守门员：逐字段断言 xXIDeviceEvent 布局
    /// （XI2proto.h：信封 32B + root_x/root_y/event_x/event_y 各 FP1616 +
    /// sourceid/pad/flags + buttons_len/valuators_len + mods(16) + group(4)
    /// + 按钮掩码(4) + valuator 掩码(4)）。firefox 的 Xlib 手工解析同样的
    /// 字节——任何一个字段错位它都会丢事件。
    #[test]
    fn xi2_motion_generic_event_matches_the_wire_layout() {
        let (mut state, client, mut rx) = fixture();
        let win = 0x0020_0001u32;
        let mut seq = 1;
        make_window(&mut state, client, &mut seq, win);

        state
            .handle(0, seq, &Request::XinputXIQueryVersion(xinput::XIQueryVersionRequest {
                major_version: 2,
                minor_version: 4,
            }))
            .expect("XIQueryVersion 应当成功");
        seq += 1;
        state
            .handle(0, seq, &Request::XinputXISelectEvents(xinput::XISelectEventsRequest {
                window: ROOT_WINDOW,
                masks: Cow::Owned(vec![xinput::EventMask {
                    deviceid: xinput::DeviceId::from(device_id::ALL_MASTER),
                    mask: vec![xinput::XIEventMask::MOTION],
                }]),
            }))
            .expect("XISelectEvents 应当成功");
        seq += 1;

        state.pointer_motion(100, 60);

        // 收集 XI2 GenericEvent（Motion）
        let mut ge = None;
        while let Ok(frame) = rx.try_recv() {
            if frame.len() >= 32 && frame[0] == 35 {
                if u16::from_ne_bytes(frame[8..10].try_into().unwrap()) == 6 {
                    ge = Some(frame);
                }
            }
        }
        let frame = ge.expect("必须收到 XI2 Motion GenericEvent");

        // 信封
        let ext = frame[1];
        assert_eq!(ext, crate::xsrv::extensions::OPCODE_XINPUT, "extension 操作码");
        let len_units = u32::from_ne_bytes(frame[4..8].try_into().unwrap());
        assert_eq!(
            frame.len() as u32,
            32 + len_units * 4,
            "length 必须等于信封之后的字节数/4"
        );
        let evtype = u16::from_ne_bytes(frame[8..10].try_into().unwrap());
        assert_eq!(evtype, 6, "XI_Motion");
        let deviceid = u16::from_ne_bytes(frame[10..12].try_into().unwrap());
        assert_eq!(deviceid, device_id::VCP, "指针事件 deviceid = VCP(2)");

        // xXIDeviceEvent：detail(16..20)
        let detail = u32::from_ne_bytes(frame[16..20].try_into().unwrap());
        assert_eq!(detail, 0, "Motion 的 detail 为 0");
        // root(20..24)
        let root = u32::from_ne_bytes(frame[20..24].try_into().unwrap());
        assert_eq!(root, ROOT_WINDOW);
        // event(24..28)：投递窗口（root 上选择了 XI2 → 事件窗口是 root）
        let event = u32::from_ne_bytes(frame[24..28].try_into().unwrap());
        assert_eq!(event, ROOT_WINDOW);

        // FP1616 root_x/root_y（32..40）：100 / 60
        let root_x = i32::from_ne_bytes(frame[32..36].try_into().unwrap());
        let root_y = i32::from_ne_bytes(frame[36..40].try_into().unwrap());
        assert_eq!((root_x / 65536, root_y / 65536), (100, 60));

        // xXIDeviceEvent：buttons_len 在 48..50（valuators_len 50..52、sourceid 52..54）
        let buttons_len = u16::from_ne_bytes(frame[48..50].try_into().unwrap());
        assert_eq!(buttons_len, 1, "按钮掩码 1 个 u32 字（偏移 48）");
        let valuators_len = u16::from_ne_bytes(frame[50..52].try_into().unwrap());
        assert_eq!(valuators_len, 1, "valuator 掩码 1 个 u32 字（偏移 50）");
        assert_eq!(
            u32::from_ne_bytes(frame[84..88].try_into().unwrap()),
            0,
            "valuator 掩码字本身在 84..88（全零；缺了它 GDK translate_axes 会越界读）"
        );
        let sourceid = u16::from_ne_bytes(frame[52..54].try_into().unwrap());
        assert_eq!(sourceid, device_id::VCP, "sourceid 在偏移 52（不是 16）");

        // mods.effective（mods 起点 60：base 60..64, latched 64..68, locked
        // 68..72, effective 72..76 的低 16 位）= 无按键按下 = 0
        let effective = u16::from_ne_bytes(frame[72..74].try_into().unwrap());
        assert_eq!(effective, 0, "无修饰键按下");
    }

    // ==== XI2 全事件类型 wire 测试（任务 XI2 二期-B）====
    //
    // 共用小工具：额外客户端、XI2 选择、按 GenericEvent(35)/核心事件分流。

    /// 注册第 `id` 号客户端并返回其接收通道。
    fn extra_client(
        state: &mut ServerState,
        id: u32,
    ) -> tokio::sync::mpsc::UnboundedReceiver<Vec<u8>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(id, tx);
        rx
    }

    /// XIQueryVersion + 在 `window` 上以 AllMasterDevices 选择一组 XI2 掩码。
    fn xi2_select(
        state: &mut ServerState,
        client: u32,
        seq: &mut u16,
        window: u32,
        masks: &[xinput::XIEventMask],
    ) {
        ok(state, client, *seq, Request::XinputXIQueryVersion(xinput::XIQueryVersionRequest {
            major_version: 2,
            minor_version: 4,
        }));
        *seq += 1;
        ok(state, client, *seq, Request::XinputXISelectEvents(xinput::XISelectEventsRequest {
            window,
            masks: Cow::Owned(vec![xinput::EventMask {
                deviceid: xinput::DeviceId::from(device_id::ALL_MASTER),
                mask: masks.to_vec(),
            }]),
        }));
        *seq += 1;
    }

    /// 清空通道并分流：GenericEvent（type=35）按 evtype 收集，核心事件按
    /// response_type 计数。返回 (generic 帧, 核心 response_type → 次数)。
    fn drain_events(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    ) -> (
        Vec<(u16, Vec<u8>)>,
        std::collections::HashMap<u8, usize>,
    ) {
        let mut generic = Vec::new();
        let mut core: std::collections::HashMap<u8, usize> = std::collections::HashMap::new();
        while let Ok(frame) = rx.try_recv() {
            if frame.len() < 32 {
                continue;
            }
            if frame[0] == 35 {
                let evtype = u16::from_ne_bytes(frame[8..10].try_into().unwrap());
                generic.push((evtype, frame));
            } else {
                *core.entry(frame[0]).or_insert(0) += 1;
            }
        }
        (generic, core)
    }

    /// 取某 evtype 的第一个 GenericEvent 帧。
    fn take_generic(generic: &[(u16, Vec<u8>)], evtype: u16) -> Vec<u8> {
        generic
            .iter()
            .find(|(t, _)| *t == evtype)
            .map(|(_, f)| f.clone())
            .unwrap_or_else(|| panic!("必须收到 XI2 GenericEvent(evtype={evtype})"))
    }

    /// 断言 GenericEvent 信封自洽：extension 操作码 + length 字段 = 尾部字节数。
    fn assert_envelope(frame: &[u8]) {
        assert_eq!(frame[1], crate::xsrv::extensions::OPCODE_XINPUT, "extension 操作码");
        let len_units = u32::from_ne_bytes(frame[4..8].try_into().unwrap());
        assert_eq!(
            frame.len() as u32,
            32 + len_units * 4,
            "length 必须等于信封之后的字节数/4"
        );
    }

    /// XI2 KeyPress(2)/KeyRelease(3) wire 布局：detail=keycode、deviceid=VCK(3)、
    /// mods.effective 反映当前修饰键状态；核心 KeyPress/Release 被抑制。
    #[test]
    fn xi2_key_press_release_wire_layout() {
        let (mut state, client, mut rx) = fixture();
        let win = 0x0020_0001u32;
        let mut seq = 1;
        make_window(&mut state, client, &mut seq, win);
        xi2_select(
            &mut state,
            client,
            &mut seq,
            win,
            &[xinput::XIEventMask::KEY_PRESS, xinput::XIEventMask::KEY_RELEASE],
        );

        // 焦点默认 PointerRoot，指针 (0,0) 命中 win → 键盘事件目标是 win。
        // Shift_L 按下 → q 按下（state 含 Shift）→ Shift 抬起 → q 抬起（state 干净）。
        state.key_event(50, true, false);
        state.key_event(24, true, false);
        state.key_event(50, false, false);
        state.key_event(24, false, false);

        let (generic, core) = drain_events(&mut rx);
        assert_eq!(core.get(&2), None, "XI2 客户端不得收到核心 KeyPress");
        assert_eq!(core.get(&3), None, "XI2 客户端不得收到核心 KeyRelease");

        // 取 keycode=24（q）的 KeyPress：Shift_L(50) 的按下事件先到，按 detail 过滤
        let press = generic
            .iter()
            .find(|(t, f)| {
                *t == 2 && u32::from_ne_bytes(f[16..20].try_into().unwrap()) == 24
            })
            .map(|(_, f)| f.clone())
            .expect("必须收到 keycode=24 的 XI2 KeyPress");
        assert_envelope(&press);
        assert_eq!(
            press.len(),
            32 + 56,
            "xXIDeviceEvent 88 字节（32 头 + 16 坐标 + buttons/valuators/sourceid/pad/flags 12 + 16 mods + 4 group + 4 按钮掩码 + 4 valuator 掩码）"
        );
        let evtype = u16::from_ne_bytes(press[8..10].try_into().unwrap());
        assert_eq!(evtype, 2, "XI_KeyPress");
        let deviceid = u16::from_ne_bytes(press[10..12].try_into().unwrap());
        assert_eq!(deviceid, device_id::VCK, "键盘事件 deviceid = VCK(3)");
        let detail = u32::from_ne_bytes(press[16..20].try_into().unwrap());
        assert_eq!(detail, 24, "detail = keycode");
        let event = u32::from_ne_bytes(press[24..28].try_into().unwrap());
        assert_eq!(event, win, "事件窗口 = 选择 XI2 的窗口");
        let sourceid = u16::from_ne_bytes(press[52..54].try_into().unwrap());
        assert_eq!(sourceid, device_id::VCK, "sourceid 在协议偏移 52；键盘事件源 = VCK");
        let buttons_len = u16::from_ne_bytes(press[48..50].try_into().unwrap());
        assert_eq!(buttons_len, 1);
        let base = u16::from_ne_bytes(press[60..62].try_into().unwrap());
        assert_eq!(base, MASK_SHIFT, "mods.base 含 Shift");
        let effective = u16::from_ne_bytes(press[72..74].try_into().unwrap());
        assert_eq!(effective, MASK_SHIFT, "mods.effective 反映按住的 Shift");
        let button_mask_word = u32::from_ne_bytes(press[80..84].try_into().unwrap());
        assert_eq!(button_mask_word, 0, "键盘事件无按钮按下");

        let release = generic
            .iter()
            .find(|(t, f)| {
                *t == 3 && u32::from_ne_bytes(f[16..20].try_into().unwrap()) == 24
            })
            .map(|(_, f)| f.clone())
            .expect("必须收到 keycode=24 的 XI2 KeyRelease");
        let detail = u32::from_ne_bytes(release[16..20].try_into().unwrap());
        assert_eq!(detail, 24, "Release detail = keycode");
        let effective = u16::from_ne_bytes(release[72..74].try_into().unwrap());
        assert_eq!(effective, 0, "修饰键全部抬起后 effective = 0");
    }

    /// XI2 ButtonPress(4)/ButtonRelease(5) wire 布局：detail=按钮号、
    /// buttons 掩码字反映按下状态（按钮 n → bit 8+n-1）；press 的 state 不含
    /// 本次按下的键。核心 ButtonPress/Release 被抑制。
    #[test]
    fn xi2_button_press_release_wire_layout() {
        let (mut state, client, mut rx) = fixture();
        let win = 0x0020_0001u32;
        let mut seq = 1;
        make_window(&mut state, client, &mut seq, win);
        xi2_select(
            &mut state,
            client,
            &mut seq,
            win,
            &[xinput::XIEventMask::BUTTON_PRESS, xinput::XIEventMask::BUTTON_RELEASE],
        );

        state.input.pointer_x = 50;
        state.input.pointer_y = 50;
        state.pointer_button(1, true);
        let (generic, core) = drain_events(&mut rx);
        assert_eq!(core.get(&4), None, "不得收到核心 ButtonPress");
        let press = take_generic(&generic, 4);
        assert_envelope(&press);
        assert_eq!(press.len(), 32 + 56, "xXIDeviceEvent 88 字节（含 valuator 掩码字）");
        let evtype = u16::from_ne_bytes(press[8..10].try_into().unwrap());
        assert_eq!(evtype, 4, "XI_ButtonPress");
        assert_eq!(u16::from_ne_bytes(press[10..12].try_into().unwrap()), device_id::VCP);
        assert_eq!(u32::from_ne_bytes(press[16..20].try_into().unwrap()), 1, "detail = 按钮号");
        assert_eq!(u32::from_ne_bytes(press[24..28].try_into().unwrap()), win);
        // X 语义：ButtonPress 的 state 不含本次按下的键
        assert_eq!(u32::from_ne_bytes(press[72..76].try_into().unwrap()), 0, "press 的 effective（mods 60..76 末字段）不含本按钮");
        let buttons = u32::from_ne_bytes(press[80..84].try_into().unwrap());
        assert_eq!(buttons, 1 << 8, "按钮 1 按下 → 掩码 bit 8");

        state.pointer_button(1, false);
        let (generic, core) = drain_events(&mut rx);
        assert_eq!(core.get(&5), None, "不得收到核心 ButtonRelease");
        let release = take_generic(&generic, 5);
        assert_eq!(
            u32::from_ne_bytes(release[80..84].try_into().unwrap()),
            0,
            "抬起后按钮掩码清零"
        );
        assert_eq!(
            u16::from_ne_bytes(release[72..74].try_into().unwrap()),
            0,
            "Release 的 state 同样不含该按钮"
        );
    }

    /// XI2 Enter(7)/Leave(8) 逐字段：xXIEnterEvent 是 72 字节布局（mode 在
    /// 70..72 的 u16），不是设备事件的 88 字节——GDK 按 Crossing 布局解析。
    #[test]
    fn xi2_enter_leave_wire_layout() {
        let (mut state, client, mut rx) = fixture();
        let win = 0x0020_0001u32;
        let mut seq = 1;
        make_window(&mut state, client, &mut seq, win);
        xi2_select(
            &mut state,
            client,
            &mut seq,
            win,
            &[xinput::XIEventMask::ENTER, xinput::XIEventMask::LEAVE],
        );

        // 指针在窗外 → 进入 → 离开
        state.pointer_motion(400, 400);
        state.pointer_motion(50, 50);
        let (generic, core) = drain_events(&mut rx);
        assert_eq!(core.get(&7), None, "不得收到核心 EnterNotify");
        let enter = take_generic(&generic, 7);
        assert_envelope(&enter);
        assert_eq!(
            enter.len(),
            88,
            "xXIEnterEvent 本体 72B（BOOL 1B：same_screen@48 focus@49 buttons_len@50 mods@52 group@68）+ 16B 稳健填充"
        );
        let evtype = u16::from_ne_bytes(enter[8..10].try_into().unwrap());
        assert_eq!(evtype, 7, "XI_Enter");
        assert_eq!(u16::from_ne_bytes(enter[10..12].try_into().unwrap()), device_id::VCP);
        // xXIEnterEvent：sourceid 在 16..18、mode 在 18、detail 在 19
        assert_eq!(
            u16::from_ne_bytes(enter[16..18].try_into().unwrap()),
            device_id::VCP,
            "sourceid 在偏移 16（此前漏写 → GDK 读到 0 → 设备表查 0 → NULL criticals）"
        );
        assert_eq!(enter[18], 0, "mode = XINotifyNormal");
        assert_eq!(enter[19], 3, "detail = XINotifyNonlinear");
        // 尾部布局（BOOL 1 字节）：same_screen@48、focus@49、buttons_len@50、
        // mods@52..68、group@68..72。整帧必须正好 72 字节。
        assert_eq!(enter[48], 1, "same_screen@48（BOOL 1 字节，不是 u32）");
        assert_eq!(enter[49], 0, "focus@49（Enter 时无焦点）");
        assert_eq!(
            u16::from_ne_bytes(enter[50..52].try_into().unwrap()),
            0,
            "buttons_len@50 = 0（无尾随按钮掩码）"
        );
        assert_eq!(u32::from_ne_bytes(enter[24..28].try_into().unwrap()), win, "event 在 24");
        assert_eq!(u32::from_ne_bytes(enter[64..68].try_into().unwrap()), 0, "effective = 无修饰键（mods @52..68）");

        state.pointer_motion(400, 400);
        let (generic, core) = drain_events(&mut rx);
        assert_eq!(core.get(&8), None, "不得收到核心 LeaveNotify");
        let leave = take_generic(&generic, 8);
        assert_eq!(leave.len(), 88, "Leave 同布局（72B 本体 + 16B 填充）");
        assert_eq!(u32::from_ne_bytes(leave[24..28].try_into().unwrap()), win);
        assert_eq!(
            u16::from_ne_bytes(leave[16..18].try_into().unwrap()),
            device_id::VCP,
            "Leave 同样在 16 写 sourceid"
        );
        assert_eq!(leave[18], 0, "mode = NORMAL");
    }

    /// XI2 FocusIn(9)/FocusOut(10)：协议里是 `typedef xXIEnterEvent
    /// xXIFocusInEvent;` —— 与 Enter **同布局 72 字节**（mode 在偏移 18 的 u8，
    /// 不是 56 字节的独立结构）；set_input_focus 路径投递 XI2 版并抑制核心版。
    #[test]
    fn xi2_focus_in_out_wire_layout_on_focus_change() {
        let (mut state, client, mut rx) = fixture();
        let win1 = 0x0020_0001u32;
        let win2 = 0x0020_0002u32;
        let mut seq = 1;
        make_window(&mut state, client, &mut seq, win1);
        make_window(&mut state, client, &mut seq, win2);
        xi2_select(
            &mut state,
            client,
            &mut seq,
            win2,
            &[xinput::XIEventMask::FOCUS_IN, xinput::XIEventMask::FOCUS_OUT],
        );
        // 同时选择核心 FOCUS_CHANGE（GDK XI2 模式不撤核心掩码 → 必须被抑制）
        ok(&mut state, client, seq, Request::ChangeWindowAttributes(xproto::ChangeWindowAttributesRequest {
            window: win2,
            value_list: Cow::Owned(xproto::ChangeWindowAttributesAux {
                event_mask: Some(EventMask::FOCUS_CHANGE),
                ..Default::default()
            }),
        }));
        seq += 1;

        state.set_input_focus(win1, u8::from(InputFocus::POINTER_ROOT));
        drain_events(&mut rx); // win1 上无 XI2 焦点选择，丢弃
        state.set_input_focus(win2, u8::from(InputFocus::POINTER_ROOT));
        let (generic, core) = drain_events(&mut rx);
        assert_eq!(core.get(&9), None, "不得收到核心 FocusIn");
        assert_eq!(core.get(&10), None, "不得收到核心 FocusOut");
        let focus_in = take_generic(&generic, 9);
        assert_envelope(&focus_in);
        assert_eq!(focus_in.len(), 88, "FocusIn 是 xXIEnterEvent 别名（72B 本体 + 16B 填充）");
        assert_eq!(u16::from_ne_bytes(focus_in[10..12].try_into().unwrap()), device_id::VCK, "焦点事件 deviceid = VCK");
        assert_eq!(u32::from_ne_bytes(focus_in[24..28].try_into().unwrap()), win2);
        assert_eq!(
            u16::from_ne_bytes(focus_in[16..18].try_into().unwrap()),
            device_id::VCK,
            "焦点事件的 sourceid 也在偏移 16"
        );
        assert_eq!(focus_in[18], 0, "mode = XINotifyNormal");
        assert_eq!(focus_in[48], 1, "same_screen@48");
        assert_eq!(focus_in[49], 1, "FocusIn 的 focus@49 = TRUE");

        state.set_input_focus(win1, u8::from(InputFocus::POINTER_ROOT));
        let (generic, _) = drain_events(&mut rx);
        let focus_out = take_generic(&generic, 10);
        assert_eq!(focus_out.len(), 88, "FocusOut 同布局（72B + 16B 填充）");
        assert_eq!(u32::from_ne_bytes(focus_out[24..28].try_into().unwrap()), win2, "FocusOut 投给失焦窗口");
        assert_eq!(
            u16::from_ne_bytes(focus_out[16..18].try_into().unwrap()),
            device_id::VCK,
            "FocusOut 同样在 16 写 sourceid"
        );
        assert_eq!(focus_out[18], 0, "mode = NORMAL");
    }

    /// 抑制矩阵完整性：客户端同时选了核心掩码和 XI2 掩码时，全部六类
    /// 事件（KeyPress=2/KeyRelease=3/ButtonPress=4/ButtonRelease=5/Motion=6
    /// 各一对）只收 XI2 GenericEvent，核心版本计数为 0。
    #[test]
    fn xi2_suppression_matrix_all_six_event_classes() {
        let (mut state, client, mut rx) = fixture();
        let win = 0x0020_0001u32;
        let mut seq = 1;
        make_window(&mut state, client, &mut seq, win); // 已含核心 Button/Motion
        // 补上核心 KeyPress/KeyRelease
        ok(&mut state, client, seq, Request::ChangeWindowAttributes(xproto::ChangeWindowAttributesRequest {
            window: win,
            value_list: Cow::Owned(xproto::ChangeWindowAttributesAux {
                event_mask: Some(EventMask::from(
                    u32::from(EventMask::KEY_PRESS) | u32::from(EventMask::KEY_RELEASE),
                )),
                ..Default::default()
            }),
        }));
        seq += 1;
        // XI2 六类掩码选在根窗口（真实 GDK 就这么干）
        xi2_select(
            &mut state,
            client,
            &mut seq,
            ROOT_WINDOW,
            &[
                xinput::XIEventMask::KEY_PRESS,
                xinput::XIEventMask::KEY_RELEASE,
                xinput::XIEventMask::BUTTON_PRESS,
                xinput::XIEventMask::BUTTON_RELEASE,
                xinput::XIEventMask::MOTION,
            ],
        );

        state.input.pointer_x = 50;
        state.input.pointer_y = 50;
        state.pointer_motion(50, 50);
        state.pointer_button(1, true);
        state.pointer_button(1, false);
        state.key_event(24, true, false);
        state.key_event(24, false, false);

        let (generic, core) = drain_events(&mut rx);
        for evtype in [2u16, 3, 4, 5, 6] {
            assert!(
                generic.iter().any(|(t, _)| *t == evtype),
                "evtype={evtype} 必须收到 XI2 GenericEvent"
            );
            // 核心 response_type 与 XI2 evtype 数值相同，但已按 frame[0] 分流
            assert_eq!(
                core.get(&(evtype as u8)),
                None,
                "evtype={evtype} 的核心版本必须被抑制（计数 0）"
            );
        }
    }

    /// 多客户端：client0 选 XI2、client1 纯核心——前者收 GenericEvent、
    /// 后者收核心事件，互不干扰。
    #[test]
    fn xi2_multiclient_xi2_client_and_core_client_do_not_interfere() {
        let (mut state, client0, mut rx0) = fixture();
        let mut rx1 = extra_client(&mut state, 1);
        let win = 0x0020_0001u32;
        let mut seq = 1;
        make_window(&mut state, client0, &mut seq, win);
        xi2_select(
            &mut state,
            client0,
            &mut seq,
            win,
            &[
                xinput::XIEventMask::MOTION,
                xinput::XIEventMask::BUTTON_PRESS,
                xinput::XIEventMask::BUTTON_RELEASE,
            ],
        );
        // client1 在根窗口上选核心事件（client0 的 XI2 抑制不影响它）
        ok(&mut state, 1, 1, Request::ChangeWindowAttributes(xproto::ChangeWindowAttributesRequest {
            window: ROOT_WINDOW,
            value_list: Cow::Owned(xproto::ChangeWindowAttributesAux {
                event_mask: Some(EventMask::from(
                    u32::from(EventMask::POINTER_MOTION)
                        | u32::from(EventMask::BUTTON_PRESS)
                        | u32::from(EventMask::BUTTON_RELEASE),
                )),
                ..Default::default()
            }),
        }));

        state.input.pointer_x = 50;
        state.input.pointer_y = 50;
        state.pointer_motion(50, 50);
        state.pointer_button(1, true);
        state.pointer_button(1, false);

        let (generic0, core0) = drain_events(&mut rx0);
        assert!(generic0.iter().any(|(t, _)| *t == 6), "XI2 客户端收 Motion GenericEvent");
        assert!(generic0.iter().any(|(t, _)| *t == 4), "XI2 客户端收 ButtonPress GenericEvent");
        assert!(generic0.iter().any(|(t, _)| *t == 5), "XI2 客户端收 ButtonRelease GenericEvent");
        assert_eq!(core0.get(&6), None, "XI2 客户端不收核心 Motion");
        assert_eq!(core0.get(&4), None, "XI2 客户端不收核心 ButtonPress");

        let (generic1, core1) = drain_events(&mut rx1);
        assert!(generic1.is_empty(), "纯核心客户端不得收到 GenericEvent");
        assert!(core1.get(&6).copied().unwrap_or(0) >= 1, "核心客户端收 MotionNotify");
        assert!(core1.get(&4).copied().unwrap_or(0) >= 1, "核心客户端收 ButtonPress");
        assert!(core1.get(&5).copied().unwrap_or(0) >= 1, "核心客户端收 ButtonRelease");
    }
}
