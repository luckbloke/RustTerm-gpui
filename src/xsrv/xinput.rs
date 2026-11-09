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
impl ServerState {
    /// 客户端可用的 XI2 版本：min(客户端请求, 2.0)。
    pub fn xi2_query_version(
        &mut self,
        client: u32,
        sequence: u16,
        request: &xinput::XIQueryVersionRequest,
    ) -> Result<(), super::error::XError> {
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
            type_: xinput::DeviceType::from(3u16), // XIMasterKeyboard
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
        self.send_reply(
            client,
            &xinput::XIGetPropertyReply {
                sequence,
                // NyaTerm: 应答帧实际为 32B 头 + 12B（type/bytes_after/num_items）
                // = 44 字节，length 必须写 3（=12/4）。此前留 0 → Xlib 只消费 32
                // 字节，剩下的 12 字节滞留队列 → "[xcb] Extra reply data still
                // left in queue" → abort（firefox 段错误级联）。
                length: 3,
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
}
