//! XKEYBOARD（XKB）最小集：让 GTK3（gdkkeys-x11.c）走 XKB 路径。
//!
//! ## 为什么必须做（已证实，不要重新论证）
//! 没有 XKB 时 GDK 走核心键表路径，`gdk_x11_keymap_key_is_modifier` 的
//! else 分支解引用 `mod_keymap`（只在更晚的 translate 时才由
//! XGetModifierMapping 填充）→ **第一次按键必然 NULL 解引用段错误**
//! （用户实测 zenity 打字即崩，栈：key_is_modifier+0x9b ← XI_KeyPress）。
//! `use_xkb` 由 `XkbQueryExtension` 决定——宣告并实现 XKB 后 GDK 全程走
//! XKB 分支，只解引用 `xkb->map->modmap`（由我们的 GetMap 应答建出）。
//!
//! ## 应答按 libX11 消费规则构造（逐行对照真实源码，task-9 复核）
//! - `_XkbReadGetMapReply`（XKBGetMap.c）：应答头 40 字节（比普通应答多
//!   8 字节，libX11 以 `_XReply(extra=8)` 读取），`extraData = length*4-8`
//!   按**固定顺序**消费 8 个 section（types→syms→actions→behaviors→
//!   vmods→explicit→modmap→vmodmap），最后 `left != 0 → BadLength`——
//!   **零残留是硬校验**。
//! - `_XkbReadGetNamesReply`（XKBNames.c）：`length==0` 时在任何 buffer
//!   使用之前直接 `return Success`；`names->vmods`/`names->groups` 是
//!   **XkbNamesRec 里的内联数组**（XKBstr.h：`Atom vmods[XkbNumVirtualMods]`、
//!   `Atom groups[XkbNumKbdGroups]`，非指针！）——`xkb->names` 由
//!   XkbGetNames 调用者 calloc，**结构上不可能 NULL**，全零 = NoSymbol。
//!   （task-9 曾怀疑空应答导致 `names->vmods` NULL 解引用——源码排除。）
//!   注：`XkbAllocNames`（XKBAlloc.c）确实不分配 vmods/groups——因为它们
//!   根本不需要分配；`_XkbReadGetNamesReply` 里的 `XkbAllocNames` 调用被
//!   `(xkb->names == NULL) &&` 短路（调用者已 calloc），仅服务 keys/aliases。
//! - `_XkbReadVirtualMods`：`if (rep->virtualMods)` 位掩码非零才读
//!   `XkbPaddedSize(popcount)` 字节——我们回 0 位 = 0 字节，恰好自洽；
//!   `XkbServerMapRec.vmods[16]` 同样是**内联数组**，只要 server 结构被
//!   分配（present 含任何 ServerInfo 位 → 我们含 VirtualMods ∈
//!   XkbAllServerInfoMask → `XkbAllocServerMap`）就非 NULL。
//! - `_XkbReadModifierMap`：`totalModMapKeys > 0` 时经
//!   `XkbAllocClientMap(xkb, XkbModifierMapMask, 0)` 分配
//!   `map->modmap`（max_key_code+1 字节），读 `XkbPaddedSize(2*total)`
//!   字节并只循环 total 次（尾部 pad 对不进循环）。
//! - `XkbGetControls`（92 字节应答）：GDK get_num_groups 读
//!   `ctrls->num_groups`——必须 ≥1，否则 have_bidi_layouts 路径异常。
//!
//! ## 四个 GDK 解引用指针的真实来源（task-9 硬断言覆盖）
//! | 指针 | 种类 | 非空条件 | 我们的应答 |
//! |---|---|---|---|
//! | names->vmods/groups | 内联数组 | 永真（calloc） | ✅ |
//! | server->vmods | 内联数组 | present 含 ServerInfo 位 | ✅（0x47 含 0x40）|
//! | map->modmap | 指针 | totalModMapKeys>0 | ✅（=5）|
//! | map->types/syms/key_sym_map | 指针 | nTypes/nKeySyms>0 | ✅ |
//!
//! ## 与核心键表的一致性
//! XKB 表数据全部派生自 `input.rs` 的 `KEYMAP`/`modifier_mapping()`——
//! GDK 在 XKB 路径用 XKB 表翻译字符，语义不一致会导致按键字符错乱。

use x11rb_protocol::protocol::xkb;
use x11rb_protocol::protocol::xproto;

use super::handshake::{MAX_KEYCODE, MIN_KEYCODE};
use super::input::{keysyms_for, modifier_mapping};
use super::state::ServerState;

/// GetNames 只承诺 groups/vmods 两个名字段（空数组，见模块注释）。
const SUPPORTED_NAME_PARTS: u32 = (1 << 12) | (1 << 11); // GROUP_NAMES | VIRTUAL_MOD_NAMES

impl ServerState {
    /// XkbUseExtension（GDK XkbQueryExtension）→ supported=1, 1.0。
    pub fn xkb_use_extension(
        &mut self,
        client: u32,
        sequence: u16,
        _request: &xkb::UseExtensionRequest,
    ) -> Result<(), super::error::XError> {
        self.send_reply(
            client,
            &xkb::UseExtensionReply {
                supported: true,
                sequence,
                length: 0,
                server_major: 1,
                server_minor: 0,
            },
        );
        Ok(())
    }

    /// XkbGetState：全零状态（我们不在 XKB 层维护锁态，核心路径自管理）。
    pub fn xkb_get_state(
        &mut self,
        client: u32,
        sequence: u16,
        _request: &xkb::GetStateRequest,
    ) -> Result<(), super::error::XError> {
        self.send_reply(
            client,
            &xkb::GetStateReply {
                device_id: 0,
                sequence,
                length: 0,
                mods: 0u8.into(),
                base_mods: 0u8.into(),
                latched_mods: 0u8.into(),
                locked_mods: 0u8.into(),
                group: 0u8.into(),
                locked_group: 0u8.into(),
                base_group: 0,
                latched_group: 0,
                compat_state: 0u8.into(),
                grab_mods: 0u8.into(),
                compat_grab_mods: 0u8.into(),
                lookup_mods: 0u8.into(),
                compat_lookup_mods: 0u8.into(),
                ptr_btn_state: 0u16.into(),
            },
        );
        Ok(())
    }

    /// XkbGetControls：92 字节应答。**num_groups 必须 ≥1**（GDK
    /// get_num_groups 直接返回 `ctrls->num_groups`）。
    pub fn xkb_get_controls(
        &mut self,
        client: u32,
        sequence: u16,
        _request: &xkb::GetControlsRequest,
    ) -> Result<(), super::error::XError> {
        self.send_reply(
            client,
            &xkb::GetControlsReply {
                device_id: 0,
                sequence,
                length: 0, // send_reply 按 (92-32)/4=15 回填
                mouse_keys_dflt_btn: 0,
                num_groups: 1,
                groups_wrap: 0, // XkbWrapIntoRange
                internal_mods_mask: 0u8.into(),
                ignore_lock_mods_mask: 0u8.into(),
                internal_mods_real_mods: 0u8.into(),
                ignore_lock_mods_real_mods: 0u8.into(),
                internal_mods_vmods: 0u16.into(),
                ignore_lock_mods_vmods: 0u16.into(),
                repeat_delay: 660,
                repeat_interval: 40,
                slow_keys_delay: 0,
                debounce_delay: 0,
                mouse_keys_delay: 0,
                mouse_keys_interval: 0,
                mouse_keys_time_to_max: 0,
                mouse_keys_max_speed: 0,
                mouse_keys_curve: 0,
                access_x_option: 0u16.into(),
                access_x_timeout: 0,
                access_x_timeout_mask: 0u32.into(),
                access_x_timeout_values: 0u32.into(),
                access_x_timeout_options_mask: 0u16.into(),
                access_x_timeout_options_values: 0u16.into(),
                enabled_controls: xkb::BoolCtrl::REPEAT_KEYS | xkb::BoolCtrl::AUDIBLE_BELL_MASK,
                per_key_repeat: [0u8; 32],
            },
        );
        Ok(())
    }

    /// XkbGetMap：按 libX11 `_XkbReadGetMapReply` 的固定消费顺序构造
    /// types → syms → modmap（vmods 置零位 = 0 字节 section）。
    pub fn xkb_get_map(
        &mut self,
        client: u32,
        sequence: u16,
        request: &xkb::GetMapRequest,
    ) -> Result<(), super::error::XError> {
        // full（XkbGetMap/XkbGetUpdatedMap）或 partial（XkbGetKeySyms 等）
        // 都回同一份全量数据——数据一致、只是字节多一点，libX11 按
        // 应答头里的 first/count 消费，不会错位。
        let which = u16::from(request.full).max(u16::from(request.partial));
        const KEY_TYPES: u16 = 1 << 0;
        const KEY_SYMS: u16 = 1 << 1;
        const MODIFIER_MAP: u16 = 1 << 2;
        const VIRTUAL_MODS: u16 = 1 << 6;
        let present = which & (KEY_TYPES | KEY_SYMS | MODIFIER_MAP | VIRTUAL_MODS);
        let want_types = present & KEY_TYPES != 0;
        let want_syms = present & KEY_SYMS != 0;
        let want_modmap = present & MODIFIER_MAP != 0;
        let want_vmods = present & VIRTUAL_MODS != 0;

        // ---- types：4 个标准 key type（XKB.h 的 required types）----
        // ONE_LEVEL / TWO_LEVEL / ALPHABETIC / KEYPAD；修饰语义必须与核心
        // GetModifierMapping 一致：Shift=bit0、Lock=bit1、Mod2(NumLock)=bit4。
        const SHIFT: u8 = 1;
        const LOCK: u8 = 2;
        const MOD2: u8 = 0x10;
        let key_types = if want_types {
            vec![
                // ONE_LEVEL
                xkb::KeyType {
                    mods_mask: 0u8.into(),
                    mods_mods: 0u8.into(),
                    mods_vmods: 0u16.into(),
                    num_levels: 1,
                    has_preserve: false,
                    map: Vec::new(),
                    preserve: Vec::new(),
                },
                // TWO_LEVEL：Shift → level 1
                xkb::KeyType {
                    mods_mask: SHIFT.into(),
                    mods_mods: SHIFT.into(),
                    mods_vmods: 0u16.into(),
                    num_levels: 2,
                    has_preserve: false,
                    map: vec![xkb::KTMapEntry {
                        active: true,
                        mods_mask: SHIFT.into(),
                        level: 1,
                        mods_mods: SHIFT.into(),
                        mods_vmods: 0u16.into(),
                    }],
                    preserve: Vec::new(),
                },
                // ALPHABETIC：Shift → level 1；Lock → level 0 且 preserve Lock
                // （GDK 翻译时 preserve 会把 Lock 从 consumed_mods 里豁免，
                // CapsLock 的大小写由 GDK 自己按 LockMask 处理——与真实
                // X.org 默认表语义一致）
                xkb::KeyType {
                    mods_mask: (SHIFT | LOCK).into(),
                    mods_mods: (SHIFT | LOCK).into(),
                    mods_vmods: 0u16.into(),
                    num_levels: 2,
                    has_preserve: true,
                    map: vec![
                        xkb::KTMapEntry {
                            active: true,
                            mods_mask: SHIFT.into(),
                            level: 1,
                            mods_mods: SHIFT.into(),
                            mods_vmods: 0u16.into(),
                        },
                        xkb::KTMapEntry {
                            active: true,
                            mods_mask: LOCK.into(),
                            level: 0,
                            mods_mods: LOCK.into(),
                            mods_vmods: 0u16.into(),
                        },
                    ],
                    preserve: vec![
                        xkb::ModDef {
                            mask: 0u8.into(),
                            real_mods: 0u8.into(),
                            vmods: 0u16.into(),
                        },
                        xkb::ModDef {
                            mask: 0u8.into(),
                            real_mods: LOCK.into(),
                            vmods: 0u16.into(),
                        },
                    ],
                },
                // KEYPAD：NumLock(Mod2) → level 1
                xkb::KeyType {
                    mods_mask: MOD2.into(),
                    mods_mods: MOD2.into(),
                    mods_vmods: 0u16.into(),
                    num_levels: 2,
                    has_preserve: false,
                    map: vec![xkb::KTMapEntry {
                        active: true,
                        mods_mask: MOD2.into(),
                        level: 1,
                        mods_mods: MOD2.into(),
                        mods_vmods: 0u16.into(),
                    }],
                    preserve: Vec::new(),
                },
            ]
        } else {
            Vec::new()
        };

        // ---- syms：MIN..=MAX 每 keycode 一条 SymMapDesc + 平铺 keysym ----
        let n_keys = usize::from(MAX_KEYCODE - MIN_KEYCODE) + 1;
        let mut key_syms = Vec::with_capacity(if want_syms { n_keys } else { 0 });
        let mut syms_data: Vec<xproto::Keysym> = Vec::new();
        if want_syms {
            for offset in 0..n_keys {
                let keycode = u8::try_from(usize::from(MIN_KEYCODE) + offset).unwrap();
                let [low, high] = keysyms_for(keycode);
                if low == 0 && high == 0 {
                    key_syms.push(xkb::KeySymMap {
                        kt_index: [0, 0, 0, 0],
                        group_info: 0,
                        width: 0,
                        syms: Vec::new(),
                    });
                } else {
                    // 1 个 group、宽 2（TWO_LEVEL = type index 1）
                    syms_data.push(low);
                    syms_data.push(high);
                    key_syms.push(xkb::KeySymMap {
                        kt_index: [1, 0, 0, 0],
                        group_info: 1,
                        width: 2,
                        syms: vec![low, high],
                    });
                }
            }
        }

        // ---- modmap：与 modifier_mapping() 同源（Shift/Lock/Ctrl/Mod1/Mod4）----
        let modmap: Vec<xkb::KeyModMap> = if want_modmap {
            modifier_mapping()
                .chunks_exact(8)
                .enumerate()
                .flat_map(|(row, keycodes)| {
                    keycodes.iter().filter_map(move |kc| {
                        let keycode = u8::from(*kc);
                        (keycode != 0).then(|| xkb::KeyModMap {
                            keycode,
                            mods: (1u8 << row).into(),
                        })
                    })
                })
                .collect()
        } else {
            Vec::new()
        };

        let total_syms = u16::try_from(syms_data.len()).expect("keysym 总数远小于 u16 上限");
        let reply = xkb::GetMapReply {
            device_id: 0,
            sequence,
            length: 0, // send_reply 按 (40+var-32)/4 回填
            min_key_code: MIN_KEYCODE.into(),
            max_key_code: MAX_KEYCODE.into(),
            first_type: 0,
            n_types: u8::try_from(key_types.len()).unwrap_or(0),
            total_types: u8::try_from(key_types.len()).unwrap_or(0),
            first_key_sym: if want_syms { MIN_KEYCODE.into() } else { 0u8.into() },
            total_syms,
            n_key_syms: if want_syms { u8::try_from(n_keys).unwrap_or(0) } else { 0 },
            first_key_action: 0u8.into(),
            total_actions: 0,
            n_key_actions: 0,
            first_key_behavior: 0u8.into(),
            n_key_behaviors: 0,
            total_key_behaviors: 0,
            first_key_explicit: 0u8.into(),
            n_key_explicit: 0,
            total_key_explicit: 0,
            first_mod_map_key: if want_modmap { MIN_KEYCODE.into() } else { 0u8.into() },
            n_mod_map_keys: if want_modmap { u8::try_from(n_keys).unwrap_or(0) } else { 0 },
            total_mod_map_keys: u8::try_from(modmap.len()).unwrap_or(0),
            first_v_mod_map_key: 0u8.into(),
            n_v_mod_map_keys: 0,
            total_v_mod_map_keys: 0,
            // 0 位 = libX11 _XkbReadVirtualMods 消费 XkbPaddedSize(0)=0 字节；
            // 但 present 含 VIRTUAL_MODS → XkbAllocServerMap 仍会建
            // server->vmods[16] 全零数组（GDK update_modmap 解引用目标）
            virtual_mods: 0u16.into(),
            map: xkb::GetMapMap {
                types_rtrn: if want_types { Some(key_types) } else { None },
                syms_rtrn: if want_syms { Some(key_syms) } else { None },
                key_actions: None,
                behaviors_rtrn: None,
                vmods_rtrn: if want_vmods { Some(Vec::new()) } else { None },
                explicit_rtrn: None,
                modmap_rtrn: if want_modmap { Some(modmap) } else { None },
                vmodmap_rtrn: None,
            },
        };
        self.send_reply(client, &reply);
        Ok(())
    }

    /// XkbGetNames：回空 value list（length=0）——libX11 仍会按 which 位
    /// 建出 `names->vmods[16]`/`names->groups[4]` 全零数组（GDK 解引用目标），
    /// 0 = NoSymbol，GDK 的 vmod 比对自然不命中，行为等同核心表。
    /// （x11rb 的 which 由 value_list 的 Some 字段推导。）
    pub fn xkb_get_names(
        &mut self,
        client: u32,
        sequence: u16,
        _request: &xkb::GetNamesRequest,
    ) -> Result<(), super::error::XError> {
        let _ = SUPPORTED_NAME_PARTS; // GDK 请求的正是这两个名字段
        self.send_reply(
            client,
            &xkb::GetNamesReply {
                device_id: 0,
                sequence,
                length: 0,
                min_key_code: MIN_KEYCODE.into(),
                max_key_code: MAX_KEYCODE.into(),
                n_types: 0,
                group_names: 0u8.into(),
                virtual_mods: 0u16.into(),
                first_key: 0u8.into(),
                n_keys: 0,
                indicators: 0,
                n_radio_groups: 0,
                n_key_aliases: 0,
                n_kt_levels: 0,
                value_list: xkb::GetNamesValueList {
                    keycodes_name: None,
                    geometry_name: None,
                    symbols_name: None,
                    phys_symbols_name: None,
                    types_name: None,
                    compat_name: None,
                    type_names: None,
                    kt_level_names: None,
                    indicator_names: None,
                    virtual_mod_names: Some(Vec::new()),
                    groups: Some(Vec::new()),
                    key_names: None,
                    key_aliases: None,
                    radio_group_names: None,
                },
            },
        );
        Ok(())
    }

    /// XkbGetIndicatorState：无 LED，全零。
    pub fn xkb_get_indicator_state(
        &mut self,
        client: u32,
        sequence: u16,
        _request: &xkb::GetIndicatorStateRequest,
    ) -> Result<(), super::error::XError> {
        self.send_reply(
            client,
            &xkb::GetIndicatorStateReply {
                device_id: 0,
                sequence,
                length: 0,
                state: 0,
            },
        );
        Ok(())
    }

    /// XkbGetCompatMap：空 compat（0 条解释）。
    pub fn xkb_get_compat_map(
        &mut self,
        client: u32,
        sequence: u16,
        _request: &xkb::GetCompatMapRequest,
    ) -> Result<(), super::error::XError> {
        self.send_reply(
            client,
            &xkb::GetCompatMapReply {
                device_id: 0,
                sequence,
                length: 0,
                groups_rtrn: 0u8.into(),
                first_si_rtrn: 0,
                n_total_si: 0,
                si_rtrn: Vec::new(),
                group_rtrn: Vec::new(),
            },
        );
        Ok(())
    }

    /// XkbGetIndicatorMap：无 indicator。
    pub fn xkb_get_indicator_map(
        &mut self,
        client: u32,
        sequence: u16,
        _request: &xkb::GetIndicatorMapRequest,
    ) -> Result<(), super::error::XError> {
        self.send_reply(
            client,
            &xkb::GetIndicatorMapReply {
                device_id: 0,
                sequence,
                length: 0,
                which: 0,
                real_indicators: 0,
                n_indicators: 0,
                maps: Vec::new(),
            },
        );
        Ok(())
    }

    /// XkbGetNamedIndicator：found=0（没有命名 LED）。
    pub fn xkb_get_named_indicator(
        &mut self,
        client: u32,
        sequence: u16,
        request: &xkb::GetNamedIndicatorRequest,
    ) -> Result<(), super::error::XError> {
        self.send_reply(
            client,
            &xkb::GetNamedIndicatorReply {
                device_id: 0,
                sequence,
                length: 0,
                indicator: request.indicator,
                found: false,
                on: false,
                real_indicator: false,
                ndx: 0,
                map_flags: 0u8.into(),
                map_which_groups: 0u8.into(),
                map_groups: 0u8.into(),
                map_which_mods: 0u8.into(),
                map_mods: 0u8.into(),
                map_real_mods: 0u8.into(),
                map_vmod: 0u16.into(),
                map_ctrls: 0u32.into(),
                supported: false,
            },
        );
        Ok(())
    }

    /// XkbPerClientFlags：只支持 DetectableAutoRepeat（GDK
    /// XkbSetDetectableAutoRepeat 走这里）。
    pub fn xkb_per_client_flags(
        &mut self,
        client: u32,
        sequence: u16,
        request: &xkb::PerClientFlagsRequest,
    ) -> Result<(), super::error::XError> {
        self.send_reply(
            client,
            &xkb::PerClientFlagsReply {
                device_id: 0,
                sequence,
                length: 0,
                supported: xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT,
                value: request.value & xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT,
                auto_ctrls: 0u32.into(),
                auto_ctrls_values: 0u32.into(),
            },
        );
        Ok(())
    }

    /// XkbListComponents：空清单。
    pub fn xkb_list_components(
        &mut self,
        client: u32,
        sequence: u16,
        _request: &xkb::ListComponentsRequest,
    ) -> Result<(), super::error::XError> {
        self.send_reply(
            client,
            &xkb::ListComponentsReply {
                device_id: 0,
                sequence,
                length: 0,
                extra: 0,
                keymaps: Vec::new(),
                keycodes: Vec::new(),
                types: Vec::new(),
                compat_maps: Vec::new(),
                symbols: Vec::new(),
                geometries: Vec::new(),
            },
        );
        Ok(())
    }

    /// XkbGetKbdByName：found=0（没有具名键盘组件可加载）。
    pub fn xkb_get_kbd_by_name(
        &mut self,
        client: u32,
        sequence: u16,
        _request: &xkb::GetKbdByNameRequest,
    ) -> Result<(), super::error::XError> {
        self.send_reply(
            client,
            &xkb::GetKbdByNameReply {
                device_id: 0,
                sequence,
                length: 0,
                min_key_code: MIN_KEYCODE.into(),
                max_key_code: MAX_KEYCODE.into(),
                loaded: false,
                new_keyboard: false,
                found: 0u16.into(),
                reported: 0u16.into(),
                replies: xkb::GetKbdByNameReplies {
                    types: None,
                    compat_map: None,
                    indicator_maps: None,
                    key_names: None,
                    geometry: None,
                },
            },
        );
        Ok(())
    }

    /// XkbGetDeviceInfo：空设备信息。
    pub fn xkb_get_device_info(
        &mut self,
        client: u32,
        sequence: u16,
        _request: &xkb::GetDeviceInfoRequest,
    ) -> Result<(), super::error::XError> {
        self.send_reply(
            client,
            &xkb::GetDeviceInfoReply {
                device_id: 0,
                sequence,
                length: 0,
                present: 0u16.into(),
                supported: 0u16.into(),
                unsupported: 0u16.into(),
                first_btn_wanted: 0,
                n_btns_wanted: 0,
                first_btn_rtrn: 0,
                total_btns: 0,
                has_own_state: false,
                dflt_kbd_fb: 0,
                dflt_led_fb: 0,
                dev_type: 0,
                name: Vec::new(),
                btn_actions: Vec::new(),
                leds: Vec::new(),
            },
        );
        Ok(())
    }

    /// XkbSetDebuggingFlags：有应答的请求（32B 全零）。
    pub fn xkb_set_debugging_flags(
        &mut self,
        client: u32,
        sequence: u16,
        _request: &xkb::SetDebuggingFlagsRequest<'_>,
    ) -> Result<(), super::error::XError> {
        self.send_reply(
            client,
            &xkb::SetDebuggingFlagsReply {
                sequence,
                length: 0,
                current_flags: 0,
                current_ctrls: 0,
                supported_flags: 0,
                supported_ctrls: 0,
            },
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb_protocol::protocol::Request;
    use x11rb_protocol::x11_utils::TryParse;
    use std::borrow::Cow;

    fn fixture() -> ServerState {
        let mut state = ServerState::new(Default::default());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);
        state
    }

    fn recv_reply(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) -> Vec<u8> {
        loop {
            match rx.try_recv() {
                Ok(frame) if frame.len() >= 32 && frame[0] == 1 => return frame,
                Ok(_) => continue,
                Err(_) => panic!("没有收到应答"),
            }
        }
    }

    fn assert_length_self_consistent(frame: &[u8]) -> u32 {
        let declared = u32::from_ne_bytes(frame[4..8].try_into().unwrap());
        assert_eq!(
            usize::try_from(declared).unwrap() * 4 + 32,
            frame.len(),
            "length={} 与实际帧长 {} 不自洽（length 不含 32 字节头）",
            declared,
            frame.len()
        );
        declared
    }

    const USE_CORE_KBD: u16 = 0x0100; // XkbUseCoreKbd

    // ---- 扩展注册 ----

    /// QueryExtension("XKEYBOARD") 必须返回 opcode 与非零 event base
    /// （GDK 的 xkb_event_type = event_base，填 0 会与核心事件冲突）。
    #[test]
    fn xkb_extension_is_registered() {
        let (opcode, first_event, _first_error) =
            super::super::extensions::query_extension("XKEYBOARD")
                .expect("XKEYBOARD 必须已注册");
        assert_eq!(opcode, super::super::extensions::OPCODE_XKEYBOARD);
        assert_ne!(first_event, 0, "XKB 的 event base 不能为 0");
    }

    // ---- XkbUseExtension ----

    /// XkbQueryExtension 要求 serverMajor/serverMinor ≥ 请求版本；GDK 请求
    /// 1.0，我们回 supported=1 + 1.0。
    #[test]
    fn use_extension_reply_round_trips() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XkbUseExtension(xkb::UseExtensionRequest {
                wanted_major: 1,
                wanted_minor: 0,
            }))
            .expect("XkbUseExtension 应当成功");

        let frame = recv_reply(&mut rx);
        let (reply, rest) = xkb::UseExtensionReply::try_parse(&frame).expect("应能解析");
        assert!(rest.is_empty(), "零残留，实际 {} 字节", rest.len());
        assert_length_self_consistent(&frame);
        assert_eq!(frame.len(), 32);
        assert!(reply.supported);
        assert_eq!((reply.server_major, reply.server_minor), (1, 0));
    }

    // ---- void 请求：绝不能回应答 ----

    /// XkbSelectEvents（含 details 块）是 void 请求——多回 32 字节会让
    /// 后续所有应答错位（比崩溃更难查）。
    #[test]
    fn select_events_is_void_no_reply_frame() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XkbSelectEvents(xkb::SelectEventsRequest {
                device_spec: USE_CORE_KBD.into(),
                clear: 0u16.into(),
                select_all: 0u16.into(),
                affect_map: 0u16.into(),
                map: 0u16.into(),
                details: Cow::Owned(xkb::SelectEventsAux::default()),
            }))
            .expect("XkbSelectEvents 应当成功");

        assert!(
            rx.try_recv().is_err(),
            "void 请求严禁回应答——任何字节都会造成流错位"
        );
    }

    /// XkbSetControls 同为 void（XkbSetDetectableAutoRepeat 的后半段）。
    #[test]
    fn set_controls_is_void_no_reply_frame() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XkbSetControls(xkb::SetControlsRequest {
                device_spec: USE_CORE_KBD.into(),
                affect_internal_real_mods: 0u8.into(),
                internal_real_mods: 0u8.into(),
                affect_ignore_lock_real_mods: 0u8.into(),
                ignore_lock_real_mods: 0u8.into(),
                affect_internal_virtual_mods: 0u16.into(),
                internal_virtual_mods: 0u16.into(),
                affect_ignore_lock_virtual_mods: 0u16.into(),
                ignore_lock_virtual_mods: 0u16.into(),
                mouse_keys_dflt_btn: 0,
                groups_wrap: 0,
                access_x_options: 0u16.into(),
                affect_enabled_controls: 0u32.into(),
                enabled_controls: 0u32.into(),
                change_controls: 0u32.into(),
                repeat_delay: 0,
                repeat_interval: 0,
                slow_keys_delay: 0,
                debounce_delay: 0,
                mouse_keys_delay: 0,
                mouse_keys_interval: 0,
                mouse_keys_time_to_max: 0,
                mouse_keys_max_speed: 0,
                mouse_keys_curve: 0,
                access_x_timeout: 0,
                access_x_timeout_mask: 0u32.into(),
                access_x_timeout_values: 0u32.into(),
                access_x_timeout_options_mask: 0u16.into(),
                access_x_timeout_options_values: 0u16.into(),
                per_key_repeat: Cow::Owned([0u8; 32]),
            }))
            .expect("XkbSetControls 应当成功");

        assert!(rx.try_recv().is_err(), "void 请求严禁回应答");
    }

    // ---- XkbGetControls ----

    /// 按 libX11 XKBGetControls 的消费规则 mock：应答固定 92 字节
    /// （length=15），num_groups ≥ 1，enabled_controls 含 RepeatKeys。
    #[test]
    fn get_controls_reply_parses_libx11_style() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XkbGetControls(xkb::GetControlsRequest {
                device_spec: USE_CORE_KBD.into(),
            }))
            .expect("XkbGetControls 应当成功");

        let frame = recv_reply(&mut rx);
        let (reply, rest) = xkb::GetControlsReply::try_parse(&frame).expect("应能解析");
        assert!(rest.is_empty(), "零残留，实际 {} 字节", rest.len());
        assert_eq!(frame.len(), 92, "xkbGetControlsReply 固定 92 字节");
        assert_length_self_consistent(&frame);
        assert!(u8::from(reply.num_groups) >= 1, "GDK get_num_groups 直接用 num_groups");
        assert!(
            u32::from(reply.enabled_controls) & u32::from(xkb::BoolCtrl::REPEAT_KEYS) != 0,
            "全局自动重复应开启"
        );
    }

    // ---- XkbGetState ----

    #[test]
    fn get_state_reply_round_trips() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XkbGetState(xkb::GetStateRequest {
                device_spec: USE_CORE_KBD.into(),
            }))
            .expect("XkbGetState 应当成功");

        let frame = recv_reply(&mut rx);
        let (reply, rest) = xkb::GetStateReply::try_parse(&frame).expect("应能解析");
        assert!(rest.is_empty(), "零残留");
        assert_length_self_consistent(&frame);
        assert_eq!(frame.len(), 32);
        assert_eq!(u16::from(reply.mods), 0);
    }

    // ---- XkbGetMap：libX11 _XkbReadGetMapReply 逐段 mock ----

    /// 按 `_XkbReadGetMapReply` 的真实规则（**不依赖 x11rb TryParse**）解析：
    /// 40 字节头 + 8 个固定顺序 section，最后必须恰好耗尽（left!=0 →
    /// libX11 返回 BadLength → XkbGetMap 返回 NULL → GDK g_error 崩溃）。
    struct LibX11GetMapMock {
        min_key_code: u8,
        max_key_code: u8,
        present: u16,
        n_types: u8,
        types: Vec<(u8, u8, u8, Vec<(bool, u8)>)>, // (mask, num_levels, nMap, entries(active, level))
        first_key_sym: u8,
        n_key_syms: u8,
        total_syms: u16,
        syms: Vec<(u8, [u8; 4], u8, u8, Vec<u32>)>, // (序号, ktIndex, groupInfo, width, syms)
        virtual_mods: u16,
        vmods_bytes: usize,
        modmap: Vec<(u8, u8)>,
        total_mod_map_keys: u8,
    }

    fn libx11_read_get_map_reply(frame: &[u8]) -> Result<LibX11GetMapMock, String> {
        // ---- 40 字节应答头（libX11 _XReply(extra=8)）----
        if frame.len() < 40 {
            return Err("帧不足 40 字节".into());
        }
        if frame[0] != 1 {
            return Err("response_type != X_Reply".into());
        }
        let declared = u32::from_ne_bytes(frame[4..8].try_into().unwrap()) as usize;
        if declared * 4 + 32 != frame.len() {
            return Err(format!("length {} 与帧长 {} 不自洽", declared, frame.len()));
        }
        let min_key_code = frame[10];
        let max_key_code = frame[11];
        if max_key_code < min_key_code {
            return Err("maxKeyCode < minKeyCode → libX11 BadImplementation".into());
        }
        let present = u16::from_ne_bytes(frame[12..14].try_into().unwrap());
        let n_types = frame[15];
        let first_key_sym = frame[17];
        let total_syms = u16::from_ne_bytes(frame[18..20].try_into().unwrap());
        let n_key_syms = frame[20];
        let total_mod_map_keys = frame[33];
        let virtual_mods = u16::from_ne_bytes(frame[38..40].try_into().unwrap());

        // extraData = length*4 - 8（libX11 扣掉 40 字节头里的 8 字节额外部分）
        let mut buf = &frame[40..];
        let expected_extra = declared * 4 - 8;
        if buf.len() != expected_extra {
            return Err(format!(
                "变长部分 {} 字节 ≠ length*4-8={}",
                buf.len(),
                expected_extra
            ));
        }
        let mut take = |n: usize| -> Result<&[u8], String> {
            if buf.len() < n {
                return Err("BadLength（缓冲不足）".into());
            }
            let (head, tail) = buf.split_at(n);
            buf = tail;
            Ok(head)
        };

        // 1. KeyTypes：nTypes × 8B 头 + 每 type nMapEntries×8B + preserve×4B
        let mut types = Vec::new();
        if n_types > 0 {
            for _ in 0..n_types {
                let desc = take(8)?;
                let num_levels = desc[4];
                let n_map = desc[5];
                let has_preserve = desc[6] != 0;
                let mut entries = Vec::new();
                if n_map > 0 {
                    for e in take(usize::from(n_map) * 8)?.chunks_exact(8) {
                        entries.push((e[0] != 0, e[2])); // active, level
                    }
                    if has_preserve {
                        take(usize::from(n_map) * 4)?; // xkbModsWireDesc
                    }
                }
                types.push((desc[0], num_levels, n_map, entries));
            }
        }

        // 2. KeySyms：nKeySyms × 8B SymMapDesc + totalSyms 个 keysym
        let mut syms = Vec::new();
        let mut syms_flat_len = 0usize;
        if n_key_syms > 0 {
            for i in 0..n_key_syms {
                let desc = take(8)?;
                let kt_index = [desc[0], desc[1], desc[2], desc[3]];
                let group_info = desc[4];
                let width = desc[5];
                let n_syms = u16::from_ne_bytes([desc[6], desc[7]]);
                let mut key_syms_i = Vec::new();
                for _ in 0..n_syms {
                    let w = take(4)?;
                    key_syms_i.push(u32::from_ne_bytes(w.try_into().unwrap()));
                }
                syms_flat_len += usize::try_from(n_syms).unwrap();
                syms.push((i, kt_index, group_info, width, key_syms_i));
            }
            // libX11 用 totalSyms 预分配 syms 数组；服务端必须保证
            // sum(nSyms) == totalSyms
            if u16::try_from(syms_flat_len).map(|n| n != total_syms).unwrap_or(true) {
                return Err(format!(
                    "sum(nSyms)={} != totalSyms={}",
                    syms_flat_len,
                    total_syms
                ));
            }
        }

        // 3. KeyActions（present 不含 → nKeyActs=0 跳过）
        // 4. KeyBehaviors（totalKeyBehaviors=0 跳过）

        // 5. VirtualMods：popcount(virtualMods) 字节，pad 到 4
        let vmods_bytes = usize::try_from(virtual_mods.count_ones()).unwrap();
        let padded = (vmods_bytes + 3) / 4 * 4;
        take(padded)?;

        // 6. Explicit（totalKeyExplicit=0 跳过）

        // 7. ModMap：totalModMapKeys 对 (key, mods) 对，pad 到 4
        let mut modmap = Vec::new();
        if total_mod_map_keys > 0 {
            let bytes = (usize::from(total_mod_map_keys) * 2 + 3) / 4 * 4;
            let wire = take(bytes)?;
            // libX11 只循环 totalModMapKeys 次——尾部填充对不算条目
            for pair in wire.chunks_exact(2).take(usize::from(total_mod_map_keys)) {
                if pair[0] > max_key_code {
                    return Err(format!("modmap keycode {} > maxKeyCode", pair[0]));
                }
                modmap.push((pair[0], pair[1]));
            }
        }

        // 8. VModMap（totalVModMapKeys=0 跳过）

        // **零残留**（libX11：left != 0 → BadLength）
        if !buf.is_empty() {
            return Err(format!("残留 {} 字节 → libX11 BadLength", buf.len()));
        }

        Ok(LibX11GetMapMock {
            min_key_code,
            max_key_code,
            present,
            n_types,
            types,
            first_key_sym,
            n_key_syms,
            total_syms,
            syms,
            virtual_mods,
            vmods_bytes,
            modmap,
            total_mod_map_keys,
        })
    }

    /// GetMap 应答必须让 libX11 建出**完整非 NULL** 的结构：types/syms/
    /// modmap/vmods（GDK get_xkb 与 key_is_modifier 的全部解引用点），
    /// 且数据与核心键表语义一致。
    #[test]
    fn get_map_reply_parses_libx11_style() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        let map_part = xkb::MapPart::KEY_TYPES
            | xkb::MapPart::KEY_SYMS
            | xkb::MapPart::MODIFIER_MAP
            | xkb::MapPart::VIRTUAL_MODS;
        state
            .handle(0, 1, &Request::XkbGetMap(xkb::GetMapRequest {
                device_spec: USE_CORE_KBD.into(),
                full: map_part,
                partial: xkb::MapPart::from(0u16),
                first_type: 0,
                n_types: 0,
                first_key_sym: 0u8.into(),
                n_key_syms: 0,
                first_key_action: 0u8.into(),
                n_key_actions: 0,
                first_key_behavior: 0u8.into(),
                n_key_behaviors: 0,
                virtual_mods: 0u16.into(),
                first_key_explicit: 0u8.into(),
                n_key_explicit: 0,
                first_mod_map_key: 0u8.into(),
                n_mod_map_keys: 0,
                first_v_mod_map_key: 0u8.into(),
                n_v_mod_map_keys: 0,
            }))
            .expect("XkbGetMap 应当成功");

        let frame = recv_reply(&mut rx);
        let mock = libx11_read_get_map_reply(&frame).unwrap_or_else(|e| {
            panic!("libX11 规则解析失败：{e}（XkbGetMap 会返回 NULL → GDK g_error）")
        });

        // 键码范围必须覆盖核心声明与修饰键表（Mod4=133！）
        assert_eq!(mock.min_key_code, super::super::handshake::MIN_KEYCODE);
        assert_eq!(mock.max_key_code, super::super::handshake::MAX_KEYCODE);

        // types：4 个标准类型、每个至少 1 level
        assert_eq!(mock.n_types, 4, "4 个标准 key type");
        assert_eq!(mock.types.len(), 4);

        // modmap：与 modifier_mapping() 语义一致
        let expect_modmap = [(50u8, 1u8), (66, 2), (37, 4), (64, 8), (133, 64)];
        assert_eq!(mock.total_mod_map_keys, expect_modmap.len() as u8);
        for (kc, mask) in &expect_modmap {
            assert!(
                mock.modmap.contains(&(*kc, *mask)),
                "modmap 缺 ({kc},{mask})：GDK key_is_modifier 会把修饰键当普通键"
            );
        }
        assert_eq!(mock.modmap.len(), expect_modmap.len());

        // syms：keycode 25 → 'w'(0x77)/'W'(0x57)（与核心 KEYMAP 一致）
        assert_eq!(
            usize::from(mock.n_key_syms),
            usize::from(super::super::handshake::MAX_KEYCODE)
                - usize::from(super::super::handshake::MIN_KEYCODE)
                + 1
        );
        let offset = usize::from(25 - mock.first_key_sym);
        let (_, kt_index, group_info, width, w_syms) = &mock.syms[offset];
        assert_eq!(*w_syms, vec![0x77, 0x57], "keycode 25 必须 = w/W");
        assert_eq!(kt_index[0], 1, "字母键用 TWO_LEVEL 类型");
        assert_eq!(*width, 2);
        assert_eq!(*group_info, 1, "1 个 group");

        // vmods：present 含 VIRTUAL_MODS（→ libX11 分配 server->vmods[16]），
        // 位全零 → 0 字节 section，不破坏零残留
        assert!(
            mock.present & u16::from(xkb::MapPart::VIRTUAL_MODS) != 0,
            "present 必须含 VIRTUAL_MODS——否则 libX11 不分配 server->vmods，GDK update_modmap 解引用 NULL"
        );
        assert_eq!(mock.virtual_mods, 0);
        assert_eq!(mock.vmods_bytes, 0);
    }

    // ---- XkbGetNames ----

    /// GetNames 用 length=0 空应答：真实 libX11 行为（XKBNames.c
    /// `_XkbReadGetNamesReply`）：`names` 结构由调用者 XkbGetNames 先
    /// calloc；`xkb->names == NULL` 恒假 → 内部那次 XkbAllocNames 被短路；
    /// `rep->length == 0` 在任何 buffer 使用之前 `return Success`。
    /// `names->vmods[16]`/`names->groups[4]` 是 XkbNamesRec **内联数组**
    /// （XKBstr.h），calloc 后全零（= NoSymbol）、**结构上不可能 NULL**。
    #[test]
    fn get_names_reply_is_empty_but_allocates_names() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XkbGetNames(xkb::GetNamesRequest {
                device_spec: USE_CORE_KBD.into(),
                which: xkb::NameDetail::GROUP_NAMES | xkb::NameDetail::VIRTUAL_MOD_NAMES,
            }))
            .expect("XkbGetNames 应当成功");

        let frame = recv_reply(&mut rx);
        let (reply, rest) = xkb::GetNamesReply::try_parse(&frame).expect("应能解析");
        assert!(rest.is_empty(), "零残留");
        assert_length_self_consistent(&frame);
        assert_eq!(frame.len(), 32, "空 names 应答固定 32 字节（length=0）");
        // which 写在偏移 8..12（由 value_list 的 Some 字段推导）——必须回显
        // GDK 请求的两个位：libX11 靠它分配 names->groups / names->vmods
        let which = u32::from_ne_bytes(frame[8..12].try_into().unwrap());
        assert!(
            which & (1 << 12) != 0,
            "which 必须含 GROUP_NAMES(bit12)——libX11 的 _XkbReadAtoms 按它决定读取量"
        );
        assert!(
            which & (1 << 11) != 0,
            "which 必须含 VIRTUAL_MOD_NAMES(bit11)"
        );
        assert_eq!(u32::from(reply.virtual_mods), 0, "vmods 位全零 → 0 个名字");
        assert_eq!(u8::from(reply.group_names), 0, "groups 位全零 → 0 个名字");
        assert_eq!(frame[13], 255, "maxKeyCode = 255");
    }

    // ---- task-9：真实 libX11 结构模型 + 四指针非 NULL 硬断言 ----
    //
    // 严格按 XKBstr.h 的结构定义建模（GDK 解引用的全部四个指针）：
    // - XkbClientMapRec.modmap：指针，`_XkbReadModifierMap` 在
    //   totalModMapKeys>0 时经 XkbAllocClientMap 分配（max_key_code+1 字节）
    // - XkbServerMapRec.vmods：**内联数组** [u8;16]，server 结构在
    //   present 含任何 ServerInfo 位（0xF8）时由 XkbAllocServerMap 分配
    // - XkbNamesRec.vmods/groups：**内联数组** [Atom;16]/[Atom;4]，
    //   names 结构由 XkbGetNames 调用者 calloc——永非 NULL
    #[derive(Default)]
    struct XkbDescModel {
        // XkbClientMapRec（XkbGetMap 入口 calloc，modmap 指针按需分配）
        map: Option<MapModel>,
        // XkbServerMapRec（present & 0xF8 != 0 时分配；vmods 内联）
        server: Option<ServerModel>,
        // XkbNamesRec（XkbGetNames 调用者 calloc；vmods/groups 内联）
        names: Option<NamesModel>,
    }
    #[derive(Default)]
    struct MapModel {
        modmap: Option<Vec<u8>>,
    }
    #[derive(Default)]
    struct ServerModel {
        vmods: [u8; 16], // 内联
    }
    #[derive(Default)]
    struct NamesModel {
        vmods: [u32; 16], // 内联
        groups: [u32; 4], // 内联
    }

    /// 按 GDK get_xkb 的真实调用序列跑一遍 libX11 行为模型：
    /// XkbGetMap(0x47) → XkbGetNames(Group|VModNames) → XkbGetControls，
    /// 然后硬断言四个指针全部非 NULL。
    #[test]
    fn gdk_xkb_path_leaves_no_null_pointer() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        let map_part = xkb::MapPart::KEY_TYPES
            | xkb::MapPart::KEY_SYMS
            | xkb::MapPart::MODIFIER_MAP
            | xkb::MapPart::VIRTUAL_MODS;

        // 1. XkbGetMap —— XkbGetMap 入口：map = calloc（有）；
        //    _XkbReadGetMapReply：present & 0xF8 != 0 → server = calloc
        state
            .handle(0, 1, &Request::XkbGetMap(xkb::GetMapRequest {
                device_spec: USE_CORE_KBD.into(),
                full: map_part,
                partial: xkb::MapPart::from(0u16),
                first_type: 0,
                n_types: 0,
                first_key_sym: 0u8.into(),
                n_key_syms: 0,
                first_key_action: 0u8.into(),
                n_key_actions: 0,
                first_key_behavior: 0u8.into(),
                n_key_behaviors: 0,
                virtual_mods: 0u16.into(),
                first_key_explicit: 0u8.into(),
                n_key_explicit: 0,
                first_mod_map_key: 0u8.into(),
                n_mod_map_keys: 0,
                first_v_mod_map_key: 0u8.into(),
                n_v_mod_map_keys: 0,
            }))
            .expect("XkbGetMap 应当成功");
        let frame = recv_reply(&mut rx);
        let mock = libx11_read_get_map_reply(&frame).expect("GetMap 必须通过真实规则解析");

        let mut desc = XkbDescModel::default();
        desc.map = Some(MapModel::default()); // XkbGetMap 入口 calloc
        // _XkbReadGetMapReply：if (!xkb->server) { mask = present & 0xF8; ... }
        const ALL_SERVER_INFO_MASK: u16 = 0xF8;
        if u16::from(mock.present) & ALL_SERVER_INFO_MASK != 0 {
            desc.server = Some(ServerModel::default()); // vmods[16] 内联零值
        }
        // _XkbReadModifierMap：totalModMapKeys > 0 → modmap = calloc(256)，
        // bzero(firstModMapKey..) 后按 wire 对写入
        if mock.total_mod_map_keys > 0 {
            let mut modmap = vec![0u8; 256];
            for (keycode, mods) in &mock.modmap {
                modmap[usize::from(*keycode)] = *mods;
            }
            desc.map.as_mut().unwrap().modmap = Some(modmap);
        }

        // 2. XkbGetNames —— 调用者先 calloc names（vmods/groups 内联全零）
        state
            .handle(0, 2, &Request::XkbGetNames(xkb::GetNamesRequest {
                device_spec: USE_CORE_KBD.into(),
                which: xkb::NameDetail::GROUP_NAMES | xkb::NameDetail::VIRTUAL_MOD_NAMES,
            }))
            .expect("XkbGetNames 应当成功");
        let frame = recv_reply(&mut rx);
        let (names_reply, rest) = xkb::GetNamesReply::try_parse(&frame).expect("应能解析");
        assert!(rest.is_empty());
        // 真实行为：length==0 → 直接 Success，names->vmods/groups 保持 calloc 零值
        assert_eq!(u32::try_from(names_reply.length).unwrap(), 0);
        desc.names = Some(NamesModel::default());

        // 3. XkbGetControls —— ctrls = calloc（num_groups=1）
        state
            .handle(0, 3, &Request::XkbGetControls(xkb::GetControlsRequest {
                device_spec: USE_CORE_KBD.into(),
            }))
            .expect("XkbGetControls 应当成功");
        recv_reply(&mut rx);

        // ---- 四指针硬断言（GDK update_modmap / key_is_modifier 的解引用点）----
        let map = desc.map.as_ref().expect("map 结构必须存在");
        let server = desc.server.as_ref().expect(
            "server 结构必须存在——present 含 VIRTUAL_MODS(ServerInfo 位) → XkbAllocServerMap",
        );
        let names = desc.names.as_ref().expect("names 结构必须存在");
        assert!(
            map.modmap.is_some(),
            "map->modmap 必须非 NULL——GDK key_is_modifier 的 XKB 分支解引用它"
        );
        // server->vmods 与 names->vmods/groups 是内联数组：结构存在即可解引用
        let _deref_server_vmods: &[u8; 16] = &server.vmods;
        let _deref_names_vmods: &[u32; 16] = &names.vmods;
        let _deref_names_groups: &[u32; 4] = &names.groups;
        // GDK update_modmap 语义：vmods 名字全零 → 不命中 Meta/Super/Hyper
        // → modmap[k] 保持真实修饰位（与我们核心表一致）
        assert!(names.vmods.iter().all(|a| *a == 0));
        assert!(names.groups.iter().all(|a| *a == 0));
        let modmap = map.modmap.as_ref().unwrap();
        assert_eq!(u8::from(modmap[50]), 1, "Shift");
        assert_eq!(u8::from(modmap[66]), 2, "Lock");
        assert_eq!(u8::from(modmap[37]), 4, "Control");
        assert_eq!(u8::from(modmap[64]), 8, "Mod1");
        assert_eq!(u8::from(modmap[133]), 64, "Mod4");
    }

    // ---- 其余有应答请求的最小合法应答 ----

    #[test]
    fn indicator_state_reply_round_trips() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XkbGetIndicatorState(xkb::GetIndicatorStateRequest {
                device_spec: USE_CORE_KBD.into(),
            }))
            .expect("应当成功");

        let frame = recv_reply(&mut rx);
        let (reply, rest) = xkb::GetIndicatorStateReply::try_parse(&frame).expect("应能解析");
        assert!(rest.is_empty());
        assert_length_self_consistent(&frame);
        assert_eq!(reply.state, 0, "无 LED");
    }

    #[test]
    fn per_client_flags_reply_advertises_detectable_auto_repeat() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XkbPerClientFlags(xkb::PerClientFlagsRequest {
                device_spec: USE_CORE_KBD.into(),
                change: xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT,
                value: xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT,
                ctrls_to_change: 0u32.into(),
                auto_ctrls: 0u32.into(),
                auto_ctrls_values: 0u32.into(),
            }))
            .expect("应当成功");

        let frame = recv_reply(&mut rx);
        let (reply, rest) = xkb::PerClientFlagsReply::try_parse(&frame).expect("应能解析");
        assert!(rest.is_empty());
        assert_length_self_consistent(&frame);
        assert!(
            u32::from(reply.supported) & u32::from(xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT) != 0,
            "detectable auto repeat 被支持（GDK 据此决定是否 XPending 兜底）"
        );
        assert!(
            u32::from(reply.value) & u32::from(xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT) != 0
        );
    }

    #[test]
    fn compat_map_and_indicator_map_and_named_indicator_are_minimal() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XkbGetCompatMap(xkb::GetCompatMapRequest {
                device_spec: USE_CORE_KBD.into(),
                groups: 0u8.into(),
                get_all_si: false,
                first_si: 0,
                n_si: 0,
            }))
            .expect("应当成功");
        let frame = recv_reply(&mut rx);
        let (reply, rest) = xkb::GetCompatMapReply::try_parse(&frame).expect("应能解析");
        assert!(rest.is_empty());
        assert_length_self_consistent(&frame);
        assert!(reply.si_rtrn.is_empty());

        state
            .handle(0, 2, &Request::XkbGetIndicatorMap(xkb::GetIndicatorMapRequest {
                device_spec: USE_CORE_KBD.into(),
                which: 0,
            }))
            .expect("应当成功");
        let frame = recv_reply(&mut rx);
        let (reply, rest) = xkb::GetIndicatorMapReply::try_parse(&frame).expect("应能解析");
        assert!(rest.is_empty());
        assert_length_self_consistent(&frame);
        assert_eq!(reply.n_indicators, 0);

        state
            .handle(0, 3, &Request::XkbGetNamedIndicator(xkb::GetNamedIndicatorRequest {
                device_spec: USE_CORE_KBD.into(),
                led_class: 0x0300u16.into(), // XkbDfltXIClass
                led_id: 0x0400u16.into(),    // XkbDfltXIId
                indicator: 0,
            }))
            .expect("应当成功");
        let frame = recv_reply(&mut rx);
        let (reply, rest) = xkb::GetNamedIndicatorReply::try_parse(&frame).expect("应能解析");
        assert!(rest.is_empty());
        assert_length_self_consistent(&frame);
        assert!(!reply.found);
    }
}
