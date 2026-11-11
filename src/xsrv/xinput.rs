//! XInput2（XI2）扩展的最小可用实现。
//!
//! 目的（详见 NYATERM.md）：GTK3 没有 XI2 时会退化为"定时器轮询指针位置"
//! （实测 XTranslateCoordinates ~80/s + XQueryPointer ~330/s），把主线程和
//! 服务端都拖进空转；有 XI2 时 GDK 改用服务器推送的运动事件，一次轮询都没有。
//!
//! 最小集（调研文档 docs/research/minimal-xi2-for-gtk3.md，逐条对照过 GDK
//! gtk-3-24 源码）：XIQueryVersion / XIQueryDevice / XISelectEvents /
//! XIQueryPointer / XIGrabDevice / XIUngrabDevice / XIAllowEvents /
//! XIGetClientPointer / XIChangeCursor。其余（XIWarpPointer/XIListProperties/
//! XIGetProperty 等）要么有 error-trap 降级、要么没有请求方。
//!
//! 设备层级（GDK3 硬编码约定，gdkscreen/gdkdevicemanager-x11.c）：
//! - 虚拟核心指针 VCP：deviceid=2（MASTER_POINTER，attachment=VCK）
//! - 虚拟核心键盘 VCK：deviceid=3（MASTER_KEYBOARD，attachment=VCP）
//! - XIAllMasterDevices=1、XIAllDevices=0；零从设备即可。
//!
//! **投递规则（点不动的关键）**：XI2 规范——事件一旦作为 XI 事件投递给某
//! 客户端，该事件对它的核心投递即告终止。GDK 选了 XI2 后**不会撤销核心
//! 掩码**，所以服务端必须做"按客户端抑制核心输入事件"，否则一次点击会被
//! 投两份（核心一份 + XI2 一份），点击状态机直接坏掉。
//!
//! 版本策略：客户端请求 (X,Y)，我们回 min(X,2) / min(Y,0)——2.0。
//! 回 2.0 让 GDK3 不选择 touch/gesture 掩码，天然缩小实现面。

use x11rb_protocol::protocol::xinput;
use x11rb_protocol::protocol::xproto;
use x11rb_protocol::x11_utils::Serialize;

use super::handshake::ROOT_WINDOW;
use super::input::{PointerGrab, MASK_CONTROL, MASK_SHIFT};
use super::state::ServerState;

/// XI2 事件类型码（XI2.h；GenericEvent 包裹，type=35）。
pub mod xi_event_type {
    pub const KEY_PRESS: u16 = 2;
    pub const KEY_RELEASE: u16 = 3;
    pub const BUTTON_PRESS: u16 = 4;
    pub const BUTTON_RELEASE: u16 = 5;
    pub const MOTION: u16 = 6;
    pub const ENTER: u16 = 7;
    pub const LEAVE: u16 = 8;
    pub const FOCUS_IN: u16 = 9;
    pub const FOCUS_OUT: u16 = 10;
    pub const HIERARCHY: u16 = 11;
    pub const PROPERTY: u16 = 12;
}

/// 虚拟设备 ID（GDK3 硬编码）。
pub mod device_id {
    pub const ALL: u16 = 0;
    pub const ALL_MASTER: u16 = 1;
    pub const VCP: u16 = 2;
    pub const VCK: u16 = 3;
}

/// 一个客户端的 XI2 会话状态（版本协商 + 窗口事件选择）。
#[derive(Debug, Default, Clone)]
pub struct Xi2Client {
    /// XIQueryVersion 协商成功后的版本。
    pub version: Option<(u16, u16)>,
}

/// 窗口上的 XI2 事件选择：`mask[deviceid]` = XIEventMask 位或。
///
/// deviceid 用客户端传入的原值（XIAllMasterDevices / 具体主设备 id 都保留，
/// 匹配时按"语义等价"处理：ALL_MASTER/具体主设备 id 都视作"两个主设备"）。
#[derive(Debug, Default, Clone)]
pub struct Xi2Selection {
    pub masks: std::collections::HashMap<u16, u32>,
}

impl Xi2Selection {
    /// 该窗口是否选择了某个 XI2 事件（对任一 deviceid，语义等价展开）。
    pub fn selects(&self, event_type: u16) -> bool {
        let bit = event_bit(event_type);
        self.masks.values().any(|m| m & bit != 0)
    }
}

/// XI2 事件类型 → XIEventMask 位。
fn event_bit(event_type: u16) -> u32 {
    1u32 << event_type
}

/// 任意客户端在该窗口选择了某 XI2 事件？
pub fn xi2_any_selects(
    map: &std::collections::HashMap<u32, Xi2Selection>,
    event_type: u16,
) -> bool {
    let bit = 1u32 << event_type;
    map.values()
        .any(|sel| sel.masks.values().any(|m| m & bit != 0))
}

/// 指定客户端在该窗口选择了某 XI2 事件？
pub fn xi2_client_selects(
    map: &std::collections::HashMap<u32, Xi2Selection>,
    client: u32,
    event_type: u16,
) -> bool {
    let bit = 1u32 << event_type;
    map.get(&client)
        .map(|sel| sel.masks.values().any(|m| m & bit != 0))
        .unwrap_or(false)
}

/// 输入协议开关（config.rs `xinput_mode`）守卫：Core 模式下 XInputExtension
/// 未宣告，带应答的 XI/XInput 请求统一回 BadRequest（客户端不应发来；void
/// 请求在 request.rs 分发臂里静默忽略——都不会造成流错位）。
fn gate() -> Result<(), super::error::XError> {
    if crate::config::xinput_enabled() {
        Ok(())
    } else {
        Err(super::error::XError::bad_request())
    }
}

impl ServerState {
    /// 客户端可用的 XI2 版本：min(客户端请求, 2.0)。
    pub fn xi2_query_version(
        &mut self,
        client: u32,
        sequence: u16,
        request: &xinput::XIQueryVersionRequest,
    ) -> Result<(), super::error::XError> {
        gate()?;
        let major = request.major_version.min(2);
        let minor = if request.major_version >= 2 {
            request.minor_version.min(0)
        } else {
            0
        };
        let entry = self.xi2_clients.entry(client).or_default();
        entry.version = Some((major, minor));
        self.send_reply(
            client,
            &xinput::XIQueryVersionReply {
                sequence,
                length: 0,
                major_version: major,
                minor_version: minor,
            },
        );
        Ok(())
    }

    /// 客户端是否完成了 XI2 版本协商（决定核心事件抑制是否生效）。
    pub fn xi2_enabled(&self, client: u32) -> bool {
        self.xi2_clients
            .get(&client)
            .map(|c| c.version.is_some())
            .unwrap_or(false)
    }

    /// XIQueryDevice：只报告两个主设备（VCP/VCK），带最小类信息。
    pub fn xi2_query_device(
        &mut self,
        client: u32,
        sequence: u16,
        request: &xinput::XIQueryDeviceRequest,
    ) -> Result<(), super::error::XError> {
        gate()?;
        // 只接受 ALL / 具体主设备；从设备我们根本不存在
        let infos = match u16::from(request.deviceid) {
            device_id::ALL | device_id::ALL_MASTER => vec![self.xi2_device_info_vcp(), self.xi2_device_info_vck()],
            device_id::VCP => vec![self.xi2_device_info_vcp()],
            device_id::VCK => vec![self.xi2_device_info_vck()],
            _ => return Err(super::error::XError::new(super::error::BAD_VALUE, u32::from(request.deviceid))),
        };

        // NyaTerm: 同样交给 `send_reply`——x11rb 会把 infos 一起序列化并按实际
        // 字节数回填 length。此前手工 truncate(32) + 追加自拼数据的写法既绕又
        // 容易与真实长度错位（同一类 "extra reply data" 崩溃的来源）。
        self.send_reply(
            client,
            &xinput::XIQueryDeviceReply {
                sequence,
                length: 0,
                infos,
            },
        );
        Ok(())
    }

    fn xi2_device_info_vcp(&self) -> xinput::XIDeviceInfo {
        xinput::XIDeviceInfo {
            deviceid: xinput::DeviceId::from(device_id::VCP),
            // type_/attachment：MASTER_POINTER，配对 VCK
            type_: xinput::DeviceType::from(1u16), // XIMasterPointer
            attachment: xinput::DeviceId::from(device_id::VCK),
            enabled: true,
            name: b"Virtual core pointer".to_vec(),
            // NyaTerm: 最小类信息——真实 X.org 的主指针 = Button(5 键) +
            // 2×Valuator(X/Y, 相对模式)。零类的设备在 GDK3 里没有按钮/轴
            // 定义，XI2 按钮事件可能被丢弃。
            classes: vec![
                xinput::DeviceClass {
                    len: (6 + 4 + 20 + 3) / 4, // 头(4) + sourceid(2) + state(4) + 5 labels(20)
                    sourceid: xinput::DeviceId::from(device_id::VCP),
                    data: xinput::DeviceClassData::Button(xinput::DeviceClassDataButton {
                        state: vec![0u32],
                        labels: vec![0u32.into(); 5],
                    }),
                },
                xinput::DeviceClass {
                    len: (6 + 36 + 3) / 4, // 头(4) + sourceid(2) + number/label(6) + 3×FP3232(24) + resolution(4) + mode(2)
                    sourceid: xinput::DeviceId::from(device_id::VCP),
                    data: xinput::DeviceClassData::Valuator(xinput::DeviceClassDataValuator {
                        number: 0,
                        label: 0,
                        min: xinput::Fp3232 { integral: -32768, frac: 0 },
                        max: xinput::Fp3232 { integral: 32767, frac: 0 },
                        value: xinput::Fp3232 { integral: 0, frac: 0 },
                        resolution: 0,
                        mode: xinput::ValuatorMode::RELATIVE,
                    }),
                },
                xinput::DeviceClass {
                    len: (6 + 36 + 3) / 4,
                    sourceid: xinput::DeviceId::from(device_id::VCP),
                    data: xinput::DeviceClassData::Valuator(xinput::DeviceClassDataValuator {
                        number: 1,
                        label: 0,
                        min: xinput::Fp3232 { integral: -32768, frac: 0 },
                        max: xinput::Fp3232 { integral: 32767, frac: 0 },
                        value: xinput::Fp3232 { integral: 0, frac: 0 },
                        resolution: 0,
                        mode: xinput::ValuatorMode::RELATIVE,
                    }),
                },
            ],
        }
    }

    fn xi2_device_info_vck(&self) -> xinput::XIDeviceInfo {
        xinput::XIDeviceInfo {
            deviceid: xinput::DeviceId::from(device_id::VCK),
            // NyaTerm: use=XIMasterKeyboard=**2**（XI2.h Device types：1=Master
            // Pointer、2=MasterKeyboard、3=SlavePointer、4=SlaveKeyboard）。
            // 曾写 3 → GDK create_device 走 SlavePointer 分支 → VCK 变成
            // "从指针"，master 配对表破坏、键盘事件设备类型错误。
            type_: xinput::DeviceType::from(2u16), // XIMasterKeyboard
            attachment: xinput::DeviceId::from(device_id::VCP),
            enabled: true,
            name: b"Virtual core keyboard".to_vec(),
            // NyaTerm: Key 类——keys 数组覆盖 keycode 8..255（248 字），与真实
            // X.org 主键盘一致（len=250 是已知基准值，可交叉验证公式）。
            classes: vec![xinput::DeviceClass {
                len: (6 + 2 + 992 + 3) / 4, // = 250
                sourceid: xinput::DeviceId::from(device_id::VCK),
                data: xinput::DeviceClassData::Key(xinput::DeviceClassDataKey {
                    keys: vec![0u32; 248],
                }),
            }],
        }
    }

    /// XISelectEvents：把窗口上的 XI2 掩码**替换**（协议语义：整体替换该
    /// deviceid 在该窗口的选择，不是叠加）。
    pub fn xi2_select_events(
        &mut self,
        client: u32,
        request: &xinput::XISelectEventsRequest<'_>,
    ) -> Result<(), super::error::XError> {
        self.window(request.window)?;
        let per_client = self
            .xi2_selections
            .entry(request.window)
            .or_default()
            .entry(client)
            .or_default();
        for mask in &*request.masks {
            let bits: u32 = mask.mask.iter().fold(0u32, |acc, m| acc | u32::from(*m));
            if bits == 0 {
                // 清除该 deviceid 的选择
                per_client.masks.remove(&u16::from(mask.deviceid));
            } else {
                per_client.masks.insert(u16::from(mask.deviceid), bits);
            }
        }
        Ok(())
    }

    /// 窗口链上（含窗口本身）是否有任何客户端选择了该 XI2 事件。
    pub fn xi2_any_selection_on_chain(&self, mut window: u32, event_type: u16) -> bool {
        use super::handshake::ROOT_WINDOW as ROOT;
        loop {
            if let Some(sel) = self.xi2_selections.get(&window) {
                if xi2_any_selects(sel, event_type) {
                    return true;
                }
            }
            if window == ROOT {
                return false;
            }
            match self.windows.get(&window) {
                Some(w) => window = w.parent,
                None => return false,
            }
        }
    }

    /// 客户端在投递链上是否选择了该 XI2 事件（按 client 过滤）。
    pub fn xi2_client_selects_on_chain(
        &self,
        client: u32,
        mut window: xproto::Window,
        event_type: u16,
    ) -> bool {
        use super::handshake::ROOT_WINDOW as ROOT;
        loop {
            // XI2 的窗口选择不区分 owner/others——选择即接收（简化：所有
            // 选择者都收；不同客户端各自收到，与核心协议的 others 语义近似）
            if let Some(sel) = self.xi2_selections.get(&window) {
                if xi2_any_selects(sel, event_type) {
                    return true;
                }
            }
            if window == ROOT {
                return false;
            }
            match self.windows.get(&window) {
                Some(w) => window = w.parent,
                None => return false,
            }
        }
    }

    /// XIQueryPointer：返回当前指针位置（相对 root 与 window，FP1616）。
    pub fn xi2_query_pointer(
        &mut self,
        client: u32,
        sequence: u16,
        request: &xinput::XIQueryPointerRequest,
    ) -> Result<(), super::error::XError> {
        gate()?;
        let (root_x, root_y) = (self.input.pointer_x, self.input.pointer_y);
        self.window(request.window)?;
        let (origin_x, origin_y) = self.window_origin(request.window);
        // NyaTerm: 用 `send_reply` 让 x11rb 自己序列化并按**实际字节数**回填
        // length。此前手工 `resize(32+56)` 把硬编码的 88 字节强加给 x11rb 的
        // 输出、length 也写死 14——一旦真实固定长度不是 88，就会多发若干字节，
        // 客户端按 length 消费后残留 → "[xcb] Extra reply data still left in
        // queue" → abort（实测：鼠标一点就崩，因为每次点击都走 XIQueryPointer）。
        self.send_reply(
            client,
            &xinput::XIQueryPointerReply {
                sequence,
                length: 0, // send_reply 会按实际字节数覆盖
                root: ROOT_WINDOW,
                child: self.child_at(request.window, root_x, root_y),
                root_x: i32::from(root_x) * 65536,
                root_y: i32::from(root_y) * 65536,
                win_x: (i32::from(root_x) - origin_x) * 65536,
                win_y: (i32::from(root_y) - origin_y) * 65536,
                same_screen: true,
                mods: Default::default(),
                group: Default::default(),
                buttons: Vec::new(),
            },
        );
        Ok(())
    }

    /// XIGrabDevice：把 XI2 抓取映射到已有的核心抓取状态（菜单/拖拽依赖）。
    pub fn xi2_grab_device(
        &mut self,
        client: u32,
        sequence: u16,
        request: &xinput::XIGrabDeviceRequest<'_>,
    ) -> Result<(), super::error::XError> {
        gate()?;
        self.window(request.window)?;
        let is_pointer = u16::from(request.deviceid) != device_id::VCK;
        if is_pointer {
            self.input.pointer_grab = Some(PointerGrab {
                window: request.window,
                owner_events: bool::from(request.owner_events),
                event_mask: 0,
                client,
            });
        } else {
            self.input.keyboard_grab = Some(request.window);
        }
        self.send_reply(
            client,
            &xinput::XIGrabDeviceReply {
                sequence,
                length: 0,
                status: xproto::GrabStatus::SUCCESS,
            },
        );
        Ok(())
    }

    /// XIUngrabDevice：解除（指针/键盘按 deviceid 区分）。
    pub fn xi2_ungrab_device(
        &mut self,
        request: &xinput::XIUngrabDeviceRequest,
    ) -> Result<(), super::error::XError> {
        if u16::from(request.deviceid) == device_id::VCK {
            self.input.keyboard_grab = None;
        } else {
            self.input.pointer_grab = None;
        }
        Ok(())
    }

    /// XIAllowEvents：抓取期间事件不被冻结（我们从不冻结），空操作。
    pub fn xi2_allow_events(&mut self, _request: &xinput::XIAllowEventsRequest) {}

    /// XIGetClientPointer：我们没有"每客户端指针"概念，恒真。
    pub fn xi2_get_client_pointer(
        &mut self,
        client: u32,
        sequence: u16,
        _request: &xinput::XIGetClientPointerRequest,
    ) -> Result<(), super::error::XError> {
        gate()?;
        self.send_reply(
            client,
            &xinput::XIGetClientPointerReply {
                sequence,
                length: 0,
                set: true,
                deviceid: xinput::DeviceId::from(device_id::VCP),
            },
        );
        Ok(())
    }

    /// XInput 1.x 的 `GetExtensionVersion`：GDK3 初始化时会**同时**探测
    /// XI2（XIQueryVersion）与旧版 XInput（GetExtensionVersion，minor=1）——
    /// 后者缺实现会回 BadRequest，未捕获错误直接杀死 firefox。
    /// 回"支持"：present=true + 版本号。
    pub fn xinput_get_extension_version(
        &mut self,
        client: u32,
        sequence: u16,
        _request: &xinput::GetExtensionVersionRequest,
    ) -> Result<(), super::error::XError> {
        gate()?;
        self.send_reply(
            client,
            &xinput::GetExtensionVersionReply {
                xi_reply_type: 0,
                sequence,
                length: 0,
                server_major: 2,
                server_minor: 0,
                present: true,
            },
        );
        Ok(())
    }
    /// XIChangeCursor：给窗口设置 XI2 光标（GDK `gdk_device_xi2_set_cursor`
    /// 调用，**无错误陷阱**——缺实现会 XI_BadEvent 直接杀死 firefox，
    /// request_code=131 minor=42，用户实测崩溃）。我们自绘光标，这里
    /// 记录到窗口供未来使用，语义上恒成功。
    pub fn xi2_change_cursor(
        &mut self,
        request: &xinput::XIChangeCursorRequest,
    ) -> Result<(), super::error::XError> {
        self.window(request.window)?;
        Ok(())
    }

    /// XIWarpPointer：指针移动到目标位置（简化：dst_window=None 时为根坐标；
    /// 有 dst_window 时取窗口原点偏移）。
    pub fn xi2_warp_pointer(
        &mut self,
        request: &xinput::XIWarpPointerRequest,
    ) -> Result<(), super::error::XError> {
        let (x, y) = if u32::from(request.dst_win) == 0 {
            (
                (request.dst_x / 65536) as i16,
                (request.dst_y / 65536) as i16,
            )
        } else {
            let (origin_x, origin_y) = self.window_origin(request.dst_win);
            (
                (origin_x + (request.dst_x / 65536)) as i16,
                (origin_y + (request.dst_y / 65536)) as i16,
            )
        };
        self.pointer_motion(x, y);
        Ok(())
    }

    /// XIListProperties：设备属性列表（空——我们没有设备属性）。
    pub fn xi2_list_properties(
        &mut self,
        client: u32,
        sequence: u16,
        _request: &xinput::XIListPropertiesRequest,
    ) -> Result<(), super::error::XError> {
        gate()?;
        self.send_reply(
            client,
            &xinput::XIListPropertiesReply {
                sequence,
                length: 0,
                properties: Vec::new(),
            },
        );
        Ok(())
    }

    /// XIGetProperty：设备属性读取（GDK 查 "Device Product ID" 等）——
    /// 返回"属性不存在"（type=None），与真实 X.org 一致。
    pub fn xi2_get_property(
        &mut self,
        client: u32,
        sequence: u16,
        _request: &xinput::XIGetPropertyRequest,
    ) -> Result<(), super::error::XError> {
        gate()?;
        self.send_reply(
            client,
            &xinput::XIGetPropertyReply {
                sequence,
                // NyaTerm: type/bytes_after/num_items/format 四个字段在 32 字节
                // 固定头**内**（偏移 8..20，x11rb 布局与 XI2 规范一致），空项时
                // 应答 = 32 字节。length 在此写什么都会被 `send_reply` 按实际
                // 字节数回填——这就是禁止手工拼字节的原因：一期手工帧 44 字节
                // 配 length=0 → Xlib 只消费 32 → 残留 12 字节 → "[xcb] Extra
                // reply data still left in queue" → abort（firefox 级联崩溃）。
                length: 0,
                type_: 0,
                bytes_after: 0,
                num_items: 0,
                items: xinput::XIGetPropertyItems::Data8(Vec::new()),
            },
        );
        Ok(())
    }

    /// 客户端断开：清理 XI2 状态。
    pub fn remove_xi2_client(&mut self, client: u32) {
        self.xi2_clients.remove(&client);
        // 抓取也可能属于它
        if self.input.pointer_grab.map(|g| g.client) == Some(client) {
            self.input.pointer_grab = None;
        }
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use x11rb_protocol::protocol::xinput;
    use x11rb_protocol::protocol::Request;
    use x11rb_protocol::x11_utils::TryParse;

    /// 输入协议开关（task-8）：Core 模式下带应答的 XInput 请求必须回
    /// BadRequest（而非任何 XI2 应答帧）；Xi2 模式恢复后一切照旧。
    #[test]
    fn core_mode_guards_xinput_replies() {
        crate::config::set_xinput_mode(crate::config::XInputMode::Core);
        let mut state = fixture();
        let err = state
            .handle(0, 1, &Request::XinputXIQueryVersion(xinput::XIQueryVersionRequest {
                major_version: 2,
                minor_version: 2,
            }))
            .expect_err("Core 模式下 XIQueryVersion 必须回 BadRequest");
        assert_eq!(err.code, super::super::error::BAD_REQUEST);

        crate::config::set_xinput_mode(crate::config::XInputMode::Xi2);
        state
            .handle(0, 2, &Request::XinputXIQueryVersion(xinput::XIQueryVersionRequest {
                major_version: 2,
                minor_version: 2,
            }))
            .expect("Xi2 模式恢复后必须照常成功");
    }

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

    /// XIQueryDevice 应答必须能被 x11rb（= firefox Xlib 同一套解析规则）
    /// 完整解析：固定头 + 设备信息，length 字段与字节数自洽。
    /// 这里的手工拼装曾经极易出错——此测试是线格式的守门员。
    #[test]
    fn query_device_reply_round_trips_through_x11rb() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XinputXIQueryDevice(xinput::XIQueryDeviceRequest {
                deviceid: xinput::DeviceId::from(device_id::ALL),
            }))
            .expect("XIQueryDevice(ALL) 应当成功");

        let frame = recv_reply(&mut rx);
        let (reply, rest) = xinput::XIQueryDeviceReply::try_parse(&frame)
            .expect("应答必须能被 x11rb 解析");
        // 固定 32 字节 + infos 数据；rest 应该已被 infos 消耗（长度自洽）
        assert_eq!(
            reply.infos.len(),
            2,
            "ALL 应当报告两个主设备（VCP/VCK）"
        );
        let ids: Vec<u16> = reply
            .infos
            .iter()
            .map(|i| u16::from(i.deviceid))
            .collect();
        assert!(ids.contains(&device_id::VCP), "必须含虚拟核心指针");
        assert!(ids.contains(&device_id::VCK), "必须含虚拟核心键盘");
        // 严格：解析后必须**零残留**（容差曾放过 "extra reply data" 崩溃）
        assert!(rest.is_empty(), "解析后必须无残留字节，实际 {} 字节", rest.len());
        // length 字段必须与实际字节数自洽
        let declared = u32::from_ne_bytes(frame[4..8].try_into().unwrap()) as usize;
        assert_eq!(declared * 4 + 32, frame.len(), "length 字段与实际帧长必须一致");
    }

    /// XIQueryPointer 应答同样必须能被 x11rb 解析，且 FP1616 坐标与
    /// 当前指针位置一致。
    #[test]
    fn query_pointer_reply_round_trips_through_x11rb() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state.input.pointer_x = 100;
        state.input.pointer_y = 60;

        state
            .handle(0, 1, &Request::XinputXIQueryPointer(xinput::XIQueryPointerRequest {
                window: super::super::handshake::ROOT_WINDOW,
                deviceid: xinput::DeviceId::from(device_id::ALL_MASTER),
            }))
            .expect("XIQueryPointer 应当成功");

        let frame = recv_reply(&mut rx);
        let (reply, rest) = xinput::XIQueryPointerReply::try_parse(&frame)
            .expect("应答必须能被 x11rb 解析");
        assert!(rest.is_empty(), "解析后必须无残留字节，实际 {} 字节", rest.len());
        let declared = u32::from_ne_bytes(frame[4..8].try_into().unwrap()) as usize;
        assert_eq!(declared * 4 + 32, frame.len(), "length 字段与实际帧长必须一致");
        // FP1616：整数部分 * 65536
        // 协议事实：无按钮时 XIQueryPointer 应答固定 56 字节（32 头 + 24）。
        // 曾经硬编码发 88 字节 → 每次点击多发 32 字节 → xcb "Extra reply data"
        // → abort（用户实测鼠标一点就崩）。此断言防止再犯。
        assert_eq!(frame.len(), 56, "XIQueryPointer 无按钮时固定 56 字节");
        let root_x = reply.root_x / 65536;
        let root_y = reply.root_y / 65536;
        assert_eq!((root_x, root_y), (100, 60), "FP1616 坐标必须与指针位置一致");
    }
    /// 回归：GDK3 初始化会同时探测 XI2（XIQueryVersion）与旧版 XInput
    /// （GetExtensionVersion，minor=1）。后者缺实现回 BadRequest → 未捕获
    /// 错误直接杀死 firefox（用户实测启动即段错误）。
    #[test]
    fn get_extension_version_must_succeed_for_gdk_probe() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XinputGetExtensionVersion(xinput::GetExtensionVersionRequest {
                name: std::borrow::Cow::Owned(b"XInputExtension".to_vec()),
            }))
            .expect("GetExtensionVersion 必须成功（GDK 启动探测）");

        // 应答：present=true（偏移 12..13）
        let reply = loop {
            match rx.try_recv() {
                Ok(frame) if frame.len() >= 32 && frame[0] == 1 => break frame,
                Ok(_) => continue,
                Err(_) => panic!("没有收到应答"),
            }
        };
        let present = reply[12];
        assert_eq!(present, 1, "present 必须为 true");
    }

    // ---- 线格式 round-trip 守门员（二期-A 全量审计补充）----
    // 每个应答类型三件套：x11rb TryParse 零残留 + length 自洽 + 关键字段。

    /// length 字段（偏移 4..8，单位 4 字节）必须与实际帧长自洽。
    fn assert_length_self_consistent(frame: &[u8]) -> u32 {
        let declared = u32::from_ne_bytes(frame[4..8].try_into().unwrap());
        assert_eq!(
            usize::try_from(declared).unwrap() * 4 + 32,
            frame.len(),
            "length={} 与实际帧长 {} 字节不一致",
            declared,
            frame.len()
        );
        declared
    }

    /// XInput 1.x GetExtensionVersion 应答：present=true 且版本号字段位置正确。
    /// （旧版字段布局：server_major/server_minor 在 8..12，present 在 12。）
    #[test]
    fn get_extension_version_reply_round_trips_through_x11rb() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XinputGetExtensionVersion(xinput::GetExtensionVersionRequest {
                name: std::borrow::Cow::Owned(b"XInputExtension".to_vec()),
            }))
            .expect("GetExtensionVersion 应当成功");

        let frame = recv_reply(&mut rx);
        let (reply, rest) = xinput::GetExtensionVersionReply::try_parse(&frame)
            .expect("应答必须能被 x11rb 解析");
        assert!(rest.is_empty(), "解析后必须无残留字节，实际 {} 字节", rest.len());
        assert_length_self_consistent(&frame);
        assert_eq!(frame.len(), 32, "GetExtensionVersion 固定 32 字节");
        assert!(reply.present, "present 必须为 true");
        assert_eq!((reply.server_major, reply.server_minor), (2, 0), "版本号必须为 2.0");
    }

    /// XIListProperties 空列表应答：32 字节 + length=0，零残留。
    #[test]
    fn list_properties_empty_reply_round_trips_through_x11rb() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XinputXIListProperties(xinput::XIListPropertiesRequest {
                deviceid: xinput::DeviceId::from(device_id::VCP),
            }))
            .expect("XIListProperties 应当成功");

        let frame = recv_reply(&mut rx);
        let (reply, rest) = xinput::XIListPropertiesReply::try_parse(&frame)
            .expect("应答必须能被 x11rb 解析");
        assert!(rest.is_empty(), "解析后必须无残留字节，实际 {} 字节", rest.len());
        assert_length_self_consistent(&frame);
        assert_eq!(frame.len(), 32, "空属性列表应答固定 32 字节");
        assert!(reply.properties.is_empty(), "属性列表必须为空");
    }

    /// XIGetProperty"属性不存在"应答线格式守门员——XI2 一期崩 4 轮的原点
    /// （"[xcb] Extra reply data" → SIGSEGV）。
    ///
    /// 协议勘误：按 XI2 规范与 x11rb 布局，type/bytes_after/num_items/
    /// format 四个字段**就在 32 字节固定头内**（偏移 8..20），空项应答 =
    /// 32 字节、length=0。一期审计口径"44 字节、length=3"对应的是当初
    /// 手工拼帧把头内字段又追加在 32 字节之后的错误布局——真正的崩溃
    /// 根因是 length 与实际字节数不自洽。此测试锁死 send_reply 路径的
    /// 自洽形态：帧长 == length*4+32，解析零残留。
    #[test]
    fn get_property_reply_wire_guard_44_bytes_length3() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XinputXIGetProperty(xinput::XIGetPropertyRequest {
                deviceid: xinput::DeviceId::from(device_id::VCP),
                delete: false,
                property: 0u32.into(),
                type_: 0u32.into(),
                offset: 0,
                len: 0,
            }))
            .expect("XIGetProperty 应当成功");

        let frame = recv_reply(&mut rx);
        let (reply, rest) = xinput::XIGetPropertyReply::try_parse(&frame)
            .expect("应答必须能被 x11rb 解析");
        assert!(rest.is_empty(), "解析后必须无残留字节，实际 {} 字节", rest.len());
        // 一期崩溃点的不变量：length 必须与实际字节数自洽（bug 版留 0 发
        // 44 字节、或声明与字节不匹配 → Xlib 只消费 length*4+32，残留即崩）。
        let declared = assert_length_self_consistent(&frame);
        assert_eq!(frame.len(), 32, "空项应答 = 32 字节固定头（尾字段在头内），无变长部分");
        assert_eq!(declared, 0, "空项时 length 必须为 0（send_reply 按实际字节数回填）");
        assert_eq!(reply.type_, 0, "属性不存在时 type 必须为 None(0)");
        assert_eq!(reply.bytes_after, 0);
        assert_eq!(reply.num_items, 0);
        assert_eq!(
            frame[20], 8,
            "format 必须在偏移 20（头内）；Data8 项的 format 编码为 8"
        );
    }

    /// XIGetClientPointer 应答：set=true + deviceid=VCP，32 字节零残留。
    #[test]
    fn get_client_pointer_reply_round_trips_through_x11rb() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XinputXIGetClientPointer(xinput::XIGetClientPointerRequest {
                window: super::super::handshake::ROOT_WINDOW,
            }))
            .expect("XIGetClientPointer 应当成功");

        let frame = recv_reply(&mut rx);
        let (reply, rest) = xinput::XIGetClientPointerReply::try_parse(&frame)
            .expect("应答必须能被 x11rb 解析");
        assert!(rest.is_empty(), "解析后必须无残留字节，实际 {} 字节", rest.len());
        assert_length_self_consistent(&frame);
        assert_eq!(frame.len(), 32, "XIGetClientPointer 固定 32 字节");
        assert!(reply.set, "set 必须为 true（恒真语义）");
        assert_eq!(u16::from(reply.deviceid), device_id::VCP, "客户端指针必须是 VCP");
    }

    /// XIQueryDevice 单设备分支：VCP-only——只有虚拟核心指针，零残留。
    #[test]
    fn query_device_vcp_only_round_trips_through_x11rb() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XinputXIQueryDevice(xinput::XIQueryDeviceRequest {
                deviceid: xinput::DeviceId::from(device_id::VCP),
            }))
            .expect("XIQueryDevice(VCP) 应当成功");

        let frame = recv_reply(&mut rx);
        let (reply, rest) = xinput::XIQueryDeviceReply::try_parse(&frame)
            .expect("应答必须能被 x11rb 解析");
        assert!(rest.is_empty(), "解析后必须无残留字节，实际 {} 字节", rest.len());
        assert_length_self_consistent(&frame);
        assert_eq!(reply.infos.len(), 1, "VCP-only 必须恰好报告 1 个设备");
        assert_eq!(u16::from(reply.infos[0].deviceid), device_id::VCP);
    }

    /// XIQueryDevice 单设备分支：VCK-only——只有虚拟核心键盘，零残留。
    /// （VCK 带 Key 类 = 250 words 的最深层变长结构，是 infos 序列化的
    /// 长度公式最强交叉验证。）
    #[test]
    fn query_device_vck_only_round_trips_through_x11rb() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XinputXIQueryDevice(xinput::XIQueryDeviceRequest {
                deviceid: xinput::DeviceId::from(device_id::VCK),
            }))
            .expect("XIQueryDevice(VCK) 应当成功");

        let frame = recv_reply(&mut rx);
        let (reply, rest) = xinput::XIQueryDeviceReply::try_parse(&frame)
            .expect("应答必须能被 x11rb 解析");
        assert!(rest.is_empty(), "解析后必须无残留字节，实际 {} 字节", rest.len());
        assert_length_self_consistent(&frame);
        assert_eq!(reply.infos.len(), 1, "VCK-only 必须恰好报告 1 个设备");
        assert_eq!(u16::from(reply.infos[0].deviceid), device_id::VCK);
    }

    /// 兜底空应答代表 1：GetDeviceButtonMapping（request.rs 手工 32 字节）。
    /// 该 Reply 类型变长部分为空列表，32 + length=0 恰好合法。
    #[test]
    fn fallback_get_device_button_mapping_reply_wire_guard() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XinputGetDeviceButtonMapping(xinput::GetDeviceButtonMappingRequest {
                device_id: device_id::VCP as u8,
            }))
            .expect("GetDeviceButtonMapping 应当成功");

        let frame = recv_reply(&mut rx);
        let (reply, rest) = xinput::GetDeviceButtonMappingReply::try_parse(&frame)
            .expect("应答必须能被 x11rb 解析");
        assert!(rest.is_empty(), "解析后必须无残留字节，实际 {} 字节", rest.len());
        assert_length_self_consistent(&frame);
        assert_eq!(frame.len(), 32, "兜底空应答固定 32 字节");
        assert!(reply.map.is_empty(), "映射表必须为空");
    }

    /// 兜底空应答代表 2：XIGetFocus（request.rs 手工 32 字节）。
    /// 该 Reply 类型固定字段全部落在 32 字节头内，32 + length=0 合法。
    #[test]
    fn fallback_xi_get_focus_reply_wire_guard() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XinputXIGetFocus(xinput::XIGetFocusRequest {
                deviceid: xinput::DeviceId::from(device_id::VCP),
            }))
            .expect("XIGetFocus 应当成功");

        let frame = recv_reply(&mut rx);
        let (_reply, rest) = xinput::XIGetFocusReply::try_parse(&frame)
            .expect("应答必须能被 x11rb 解析");
        assert!(rest.is_empty(), "解析后必须无残留字节，实际 {} 字节", rest.len());
        assert_length_self_consistent(&frame);
        assert_eq!(frame.len(), 32, "兜底空应答固定 32 字节");
    }

    // ---- 模拟 libXi 解析器（XIQueryDevice 崩溃守门员）----
    //
    // 背景：firefox（libXi → GDK3）曾把我们的 XIQueryDevice 应答解析成
    // 0 个设备（GDK 设备哈希空表 → 所有 XI2 事件 source_device=NULL →
    // criticals 崩溃），而 x11rb TryParse 宽容通过——两者校验强度不同。
    // 以下解析器逐行对照 libXi src/XIQueryDevice.c 与 src/XExtInt.c 的
    // 读取/校验逻辑（wire 结构取自 xorgproto XI2proto.h），任何一步
    // 失败都等价于 libXi 的 error_loop → 返回 NULL/ndevices=-1。

    /// libXi XIQueryDevice.c：`name_bytes = ((size_t)wire->name_len + 3) / 4 * 4`
    fn libxi_name_bytes(name_len: usize) -> usize {
        (name_len + 3) / 4 * 4
    }

    /// XExtInt.c validate_class_header：len>0 且 len*4 不越过 end。
    /// 返回 (type, len_words)。
    fn libxi_validate_class_header(buf: &[u8], off: usize) -> Option<(u16, u16)> {
        if off + 8 > buf.len() {
            // ptr + sizeof(xXIAnyInfo) > end
            return None;
        }
        let type_ = u16::from_ne_bytes(buf[off..off + 2].try_into().unwrap());
        let len = u16::from_ne_bytes(buf[off + 2..off + 4].try_into().unwrap());
        if len == 0 {
            return None;
        }
        let remaining = buf.len() - off;
        if usize::from(len) > remaining / 4 {
            return None;
        }
        Some((type_, len))
    }

    /// XExtInt.c validate_class_payload：按类型的最小载荷校验。
    /// wire 结构大小（XI2proto.h）：sizeof(xXIButtonInfo)=8、
    /// sizeof(xXIKeyInfo)=8、sizeof(xXIValuatorInfo)=44。
    /// 类类型常量（XI2.h）：XIKeyClass=0、XIButtonClass=1、
    /// XIValuatorClass=2、XIScrollClass=3、XITouchClass=8。
    fn libxi_validate_class_payload(buf: &[u8], off: usize, type_: u16, len_words: u16) -> bool {
        let cls_len = usize::from(len_words) * 4;
        match type_ {
            1 => {
                // XIButtonClass
                if cls_len < 8 {
                    return false;
                }
                let num_buttons = u16::from_ne_bytes(buf[off + 6..off + 8].try_into().unwrap());
                let mask_size = (usize::from(num_buttons + 7) / 8 + 3) / 4 * 4;
                let labels_size = usize::from(num_buttons) * 4;
                8 + mask_size + labels_size <= cls_len
            }
            0 => {
                // XIKeyClass
                if cls_len < 8 {
                    return false;
                }
                let num_keycodes = u16::from_ne_bytes(buf[off + 6..off + 8].try_into().unwrap());
                usize::from(num_keycodes) * 4 <= cls_len - 8
            }
            2 => {
                // XIValuatorClass：sizeof(xXIValuatorInfo)=44
                cls_len >= 44
            }
            _ => true, // 未知类型 libXi 跳过（copy_classes 不消耗 len? 见 walk）
        }
    }

    /// 按libXi XIQueryDevice.c 主循环整体解析（不含 32 字节应答头）。
    /// 成功返回设备摘要 (deviceid, use, attachment, enabled, name, num_classes)。
    #[allow(clippy::type_complexity)]
    fn libxi_parse_query_device(buf: &[u8]) -> Result<Vec<(u16, u16, u16, bool, String, usize)>, String> {
        let end = buf.len();
        let mut ptr = 0usize;
        // reply.num_devices 已在外层断言，这里按 libXi 循环推进直到耗尽
        let mut devices = Vec::new();
        while ptr < end {
            // ptr + sizeof(xXIDeviceInfo)=12 > end → error_loop
            //（xXIDeviceInfo = deviceid2+use2+attachment2+num_classes2+name_len2
            //  +enabled1+pad1 = 12 字节，见 XI2proto.h）
            if ptr + 12 > end {
                return Err(format!("设备头越界 @{}", ptr));
            }
            let deviceid = u16::from_ne_bytes(buf[ptr..ptr + 2].try_into().unwrap());
            let use_ = u16::from_ne_bytes(buf[ptr + 2..ptr + 4].try_into().unwrap());
            let attachment = u16::from_ne_bytes(buf[ptr + 4..ptr + 6].try_into().unwrap());
            let num_classes = u16::from_ne_bytes(buf[ptr + 6..ptr + 8].try_into().unwrap()) as usize;
            let name_len = u16::from_ne_bytes(buf[ptr + 8..ptr + 10].try_into().unwrap()) as usize;
            let enabled = buf[ptr + 10] != 0;
            ptr += 12;

            let name_bytes = libxi_name_bytes(name_len);
            if ptr + name_bytes > end {
                return Err(format!("name 越界 @{}", ptr));
            }
            let name = String::from_utf8_lossy(&buf[ptr..ptr + name_len]).into_owned();
            ptr += name_bytes;

            // size_classes + copy_classes 都走同一套 header/payload 校验并按
            // len*4 步进；这里合并为一次 walk（两遍结果一致）。
            let mut nclasses = 0usize;
            for _ in 0..num_classes {
                let (type_, len_words) = match libxi_validate_class_header(buf, ptr) {
                    Some(v) => v,
                    None => return Err(format!("class 头校验失败 @{}", ptr)),
                };
                if !libxi_validate_class_payload(buf, ptr, type_, len_words) {
                    return Err(format!("class 载荷校验失败 type={} len={} @{}", type_, len_words, ptr));
                }
                ptr += usize::from(len_words) * 4;
                nclasses += 1;
            }
            devices.push((deviceid, use_, attachment, enabled, name, nclasses));
        }
        // libXi 本身容忍尾部残余，但残余即意味着布局偏差——这里比 libXi
        // 更严格，顺带检测任何多余字节。
        if ptr != end {
            return Err(format!("解析后残留 {} 字节（布局偏差）", end - ptr));
        }
        Ok(devices)
    }

    /// **决定性测试**：模拟 libXi（= firefox 的 Xlib）读取规则解析
    /// XIQueryDevice(XIAllDevices) 应答——GDK3 constructed() 正是拿这个
    /// 建设备哈希表。任何偏差在此暴露（x11rb TryParse 会宽容通过）。
    #[test]
    fn query_device_reply_parses_with_libxi_rules() {
        let mut state = fixture();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);

        state
            .handle(0, 1, &Request::XinputXIQueryDevice(xinput::XIQueryDeviceRequest {
                deviceid: xinput::DeviceId::from(device_id::ALL),
            }))
            .expect("XIQueryDevice(ALL) 应当成功");

        let frame = recv_reply(&mut rx);
        // libXi：num_devices @8..10，length*4 = 变长部分字节数
        let num_devices = u16::from_ne_bytes(frame[8..10].try_into().unwrap()) as usize;
        let length = u32::from_ne_bytes(frame[4..8].try_into().unwrap()) as usize;
        assert_eq!(num_devices, 2, "num_devices 必须为 2（GDK 按它循环建设备）");
        assert_eq!(
            frame.len() - 32,
            length * 4,
            "变长部分字节数必须 == length*4（libXi 按 _XRead(length*4) 读取）"
        );

        let devices = libxi_parse_query_device(&frame[32..])
            .expect("libXi 规则解析必须成功——失败即 GDK 得到空设备表（firefox 崩溃根因）");
        assert_eq!(devices.len(), 2, "必须解析出 2 个设备");

        let vcp = devices.iter().find(|d| d.0 == device_id::VCP).expect("必须有 VCP");
        let vck = devices.iter().find(|d| d.0 == device_id::VCK).expect("必须有 VCK");
        // XIMasterPointer=1 / XIMasterKeyboard=2（XI2.h Device types；
        // 3=XISlavePointer——曾经写错导致 GDK 把 VCK 当从设备）
        assert_eq!(vcp.1, 1, "VCP use 必须 = XIMasterPointer");
        assert_eq!(vck.1, 2, "VCK use 必须 = XIMasterKeyboard");
        assert_eq!(vcp.2, device_id::VCK, "VCP attachment 必须是 VCK");
        assert_eq!(vck.2, device_id::VCP, "VCK attachment 必须是 VCP");
        // GDK constructed()：`if (!dev->enabled) continue;`——禁用设备不进哈希表
        assert!(vcp.3 && vck.3, "两个主设备必须 enabled=true");
        assert_eq!(vcp.4, "Virtual core pointer");
        assert_eq!(vck.4, "Virtual core keyboard");
        // 字节恰好耗尽（residual == "extra reply data" 级错位）
        assert_eq!(vcp.5 + vck.5, 4, "VCP 3 类（Button+2 Valuator）+ VCK 1 类（Key）");
    }

    /// 同上，覆盖 VCP-only / VCK-only 分支（GDK 只在构造时用 ALL，
    /// 但 XIChangeHierarchy/HIP 等路径会单查设备）。
    #[test]
    fn query_device_single_device_parses_with_libxi_rules() {
        for (deviceid, want_name, want_use, want_classes) in [
            (device_id::VCP, "Virtual core pointer", 1u16, 3usize),
            (device_id::VCK, "Virtual core keyboard", 2u16, 1usize),
        ] {
            let mut state = fixture();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            state.register_client(0, tx);

            state
                .handle(0, 1, &Request::XinputXIQueryDevice(xinput::XIQueryDeviceRequest {
                    deviceid: xinput::DeviceId::from(deviceid),
                }))
                .expect("XIQueryDevice 应当成功");

            let frame = recv_reply(&mut rx);
            let num_devices = u16::from_ne_bytes(frame[8..10].try_into().unwrap()) as usize;
            assert_eq!(num_devices, 1);
            let length = u32::from_ne_bytes(frame[4..8].try_into().unwrap()) as usize;
            assert_eq!(frame.len() - 32, length * 4);

            let devices = libxi_parse_query_device(&frame[32..])
                .expect("libXi 规则解析必须成功");
            assert_eq!(devices.len(), 1);
            let dev = &devices[0];
            assert_eq!(dev.0, deviceid);
            assert_eq!(dev.1, want_use);
            assert_eq!(dev.4, want_name);
            assert_eq!(dev.5, want_classes);
        }
    }

    // ---- GDK3 消费端完整 mock（gdkdevicemanager-xi2.c，gtk-3-24）----
    //
    // 移植 GDK 的设备哈希表模型：constructed() 逐设备（enabled 才收）建
    // id_table/masters/slaves 三张表 → relate_masters 配对；translate_events
    // 各分支按 deviceid/sourceid 查 **同一张** id_table。对我们真实的
    // XIQueryDevice 应答帧与真实 XI2 事件帧跑这套逻辑，断言查表永不 NULL。

    /// GDK id_table 条目：(use, attachment)。
    #[allow(clippy::type_complexity)]
    fn gdk3_mock_consume_query_device(
        reply_frame: &[u8],
    ) -> std::collections::HashMap<u16, (u16, u16)> {
        use std::collections::HashMap;
        // constructed: XIQueryDevice(XIAllDevices) — libXi 失败返回 NULL+(-1)
        // → GDK 3.24 无任何检查直接跳过循环 → 空表（症状根因路径）。
        let num_devices = u16::from_ne_bytes(reply_frame[8..10].try_into().unwrap()) as usize;
        let length = u32::from_ne_bytes(reply_frame[4..8].try_into().unwrap()) as usize;
        assert_eq!(reply_frame.len() - 32, length * 4, "reply.length 必须自洽");
        let devices = libxi_parse_query_device(&reply_frame[32..])
            .expect("XIQueryDevice 必须能被 libXi 解析（失败 = GDK 空表）");
        assert_eq!(devices.len(), num_devices, "解析设备数必须 == num_devices");

        let mut id_table = HashMap::new();
        let mut masters = Vec::new();
        for (deviceid, use_, attachment, enabled, _name, _nclasses) in &devices {
            // `if (!dev->enabled) continue;` —— 禁用设备不进任何表
            if !enabled {
                continue;
            }
            id_table.insert(*deviceid, (*use_, *attachment));
            // XIMasterPointer=1 / XIMasterKeyboard=2；3/4=Slave、5=Floating
            if *use_ == 1 || *use_ == 2 {
                masters.push((*deviceid, *attachment));
            } else {
                // Slave 不参与 master 配对——GDK 只 relate masters
                assert!(
                    !(*use_ == 3 || *use_ == 4),
                    "意外：我们把设备报告成 Slave(use={use_})——GDK 会按从设备处理主设备"
                );
            }
        }
        // relate_masters：双向关联必须能查到双方
        for (id, att) in &masters {
            assert!(
                id_table.contains_key(id) && id_table.contains_key(att),
                "master 配对断裂：{id}↔{att} 有一方不在 id_table"
            );
        }
        id_table
    }

    /// GDK translate_events：从 GenericEvent 帧取 (deviceid, sourceid) 并查表。
    /// xXIDeviceEvent：deviceid@10、sourceid@52；
    /// xXIEnterEvent（Focus/Enter/Leave）：deviceid@10、sourceid@16。
    fn gdk3_mock_translate_lookup(
        id_table: &std::collections::HashMap<u16, (u16, u16)>,
        frame: &[u8],
    ) -> (u16, u16) {
        assert_eq!(frame[0], 35, "必须是 GenericEvent");
        let evtype = u16::from_ne_bytes(frame[8..10].try_into().unwrap());
        let deviceid = u16::from_ne_bytes(frame[10..12].try_into().unwrap());
        let sourceid = match evtype {
            // Key/Button/Motion/Touch（xXIDeviceEvent 布局）
            2 | 3 | 4 | 5 | 6 | 18 | 19 => {
                u16::from_ne_bytes(frame[52..54].try_into().unwrap())
            }
            // Enter/Leave/FocusIn/FocusOut（xXIEnterEvent 布局）
            7 | 8 | 9 | 10 => u16::from_ne_bytes(frame[16..18].try_into().unwrap()),
            _ => panic!("未覆盖的 evtype={evtype}"),
        };
        assert!(
            id_table.contains_key(&deviceid),
            "GDK 查表 device={deviceid} → NULL（criticals 根因）"
        );
        assert!(
            id_table.contains_key(&sourceid),
            "GDK 查表 source_device={sourceid} → NULL（criticals 根因）"
        );
        (deviceid, sourceid)
    }

    /// **端到端 GDK mock**：XIQueryDevice 应答建表（GDK constructed），
    /// 再对我们真实发出的 Motion / KeyPress / XIGetClientPointer 应答跑
    /// translate_events 的查表逻辑——任何一步查表 NULL 都会复现 firefox
    /// 的 GDK_IS_DEVICE criticals。
    #[test]
    fn gdk3_consumer_mock_never_returns_null_device() {
        use std::collections::HashMap;

        let mut state = ServerState::new(Default::default());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_client(0, tx);
        let mut seq: u16 = 1;
        let mut ok = |state: &mut ServerState, s: u16, req: Request<'static>| {
            state
                .handle(0, s, &req)
                .unwrap_or_else(|e| panic!("请求失败：{e:?}"));
        };

        // ---- GDK constructed：XIQueryDevice(XIAllDevices) 建表 ----
        ok(&mut state, seq, Request::XinputXIQueryDevice(xinput::XIQueryDeviceRequest {
            deviceid: xinput::DeviceId::from(device_id::ALL),
        }));
        seq += 1;
        let query_frame = {
            loop {
                match rx.try_recv() {
                    Ok(f) if f.len() >= 32 && f[0] == 1 => break f,
                    Ok(_) => continue,
                    Err(_) => panic!("没有收到 XIQueryDevice 应答"),
                }
            }
        };
        let id_table = gdk3_mock_consume_query_device(&query_frame);
        assert!(id_table.contains_key(&device_id::VCP) && id_table.contains_key(&device_id::VCK));

        // ---- firefox 后续：XIGetClientPointer → get_client_pointer 查表 ----
        ok(&mut state, seq, Request::XinputXIGetClientPointer(
            xinput::XIGetClientPointerRequest { window: 0 },
        ));
        seq += 1;
        let gcp = loop {
            match rx.try_recv() {
                Ok(f) if f.len() >= 32 && f[0] == 1 => break f,
                Ok(_) => continue,
                Err(_) => panic!("没有收到 XIGetClientPointer 应答"),
            }
        };
        // xXIGetClientPointerReply：set@8、deviceid@10（libXi XIGetCPtr.c 直接
        // 取 reply.deviceid 回填 GDK 的出参——不管 set）
        let gcp_id = u16::from_ne_bytes(gcp[10..12].try_into().unwrap());
        assert!(
            id_table.contains_key(&gcp_id),
            "get_client_pointer 查表 device={gcp_id} → NULL"
        );

        // ---- 真实事件帧 → translate_events 查表 ----
        // 建窗口 + XIQueryVersion + XISelectEvents（GDK get_handled_events 的
        // 输入/指针/键盘掩码）
        let win = 0x0020_0001u32;
        ok(&mut state, seq, Request::CreateWindow(xproto::CreateWindowRequest {
            depth: 0,
            wid: win,
            parent: super::super::handshake::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 200,
            height: 200,
            border_width: 0,
            class: xproto::WindowClass::INPUT_OUTPUT,
            visual: 0,
            value_list: std::borrow::Cow::Owned(xproto::CreateWindowAux::default()),
        }));
        seq += 1;
        ok(&mut state, seq, Request::ChangeWindowAttributes(
            xproto::ChangeWindowAttributesRequest {
                window: win,
                value_list: std::borrow::Cow::Owned(xproto::ChangeWindowAttributesAux {
                    event_mask: Some(xproto::EventMask::from(
                        u32::from(xproto::EventMask::BUTTON_PRESS)
                            | u32::from(xproto::EventMask::BUTTON_RELEASE)
                            | u32::from(xproto::EventMask::POINTER_MOTION),
                    )),
                    ..Default::default()
                }),
            },
        ));
        seq += 1;
        ok(&mut state, seq, Request::MapWindow(xproto::MapWindowRequest { window: win }));
        seq += 1;
        ok(&mut state, seq, Request::XinputXIQueryVersion(
            xinput::XIQueryVersionRequest { major_version: 2, minor_version: 2 },
        ));
        seq += 1;
        ok(&mut state, seq, Request::XinputXISelectEvents(
            xinput::XISelectEventsRequest {
                window: win,
                masks: std::borrow::Cow::Owned(vec![xinput::EventMask {
                    deviceid: xinput::DeviceId::from(device_id::ALL_MASTER),
                    mask: vec![
                        xinput::XIEventMask::MOTION,
                        xinput::XIEventMask::BUTTON_PRESS,
                        xinput::XIEventMask::KEY_PRESS,
                    ],
                }]),
            },
        ));
        seq += 1;
        while rx.try_recv().is_ok() {} // 清空通道

        // Motion（真实指针移动）
        state.pointer_motion(10, 10);
        // ButtonPress（真实点击）+ KeyPress（真实按键）
        state.pointer_button(1, true);
        state.key_event(24, true, false);

        // 逐帧跑 GDK translate_events 查表
        let mut checked = 0;
        while let Ok(frame) = rx.try_recv() {
            if frame.len() < 32 || frame[0] != 35 {
                continue; // 非 XI2 GenericEvent
            }
            let (deviceid, sourceid) = gdk3_mock_translate_lookup(&id_table, &frame);
            assert_eq!(
                u16::from_ne_bytes(frame[10..12].try_into().unwrap()),
                deviceid
            );
            checked += 1;
        }
        assert!(checked >= 3, "至少应验证 Motion/Button/Key 三类事件帧，实际 {checked}");
    }
}
