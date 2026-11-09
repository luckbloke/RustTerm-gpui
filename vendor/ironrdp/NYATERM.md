# NyaTerm IronRDP snapshot

- Upstream: `https://github.com/Devolutions/IronRDP`
- Commit: `b149f500b85124c513646494335fb6cee525d897`
- Snapshot date: `2026-09-22`

The workspace is kept as one coherent source snapshot. NyaTerm-specific changes must be
documented here and kept narrowly scoped so a future upstream refresh can be rebased as a
single unit.

## Local changes

- `ironrdp-client` accepts an application-provided direct transport stream so NyaTerm proxy and
  jump-host connections do not bypass the shared network stack.
- `ironrdp-client` accepts an asynchronous post-handshake certificate verifier. This keeps
  NyaTerm's known-host prompt and fingerprint policy authoritative with the Windows native-TLS
  backend, whose upstream validation callback is intentionally unavailable.
- `ironrdp-rdpdr-native` accepts an absolute local directory beneath a drive (`D:\dir`) as the
  redirected root, instead of requiring a whole volume (`D:\`). The Windows path conversion is
  shared with the existing volume-root path: it re-emits the `<letter>:\` prefix, collapses
  repeated separators, and still rejects drive-relative, root-relative, UNC, namespace, embedded
  NUL, and `.`/`..` forms. The normalized path keeps a trailing backslash so the root opens as a
  directory. Covered by unit tests in `windows/handles.rs`.
- `ironrdp-rdpdr` logs `[RDPDR] server rejected device announcement: ...` when the server refuses
  a redirected device. Upstream only records rejected IDs, which leaves a missing `\\tsclient`
  share undiagnosable from the client log.
- `ironrdp-rdpdr` serves filesystem IRPs that race against the announcement response instead of
  dropping them. Real Windows servers deliver the first create before
  `DR_CORE_DEVICE_ANNOUNCE_RSP`; leaving that IRP unanswered makes the server wait ~30 s and retry,
  and a later rejection removes the device and cancels its deferred operations. IRPs for unknown or
  rejected devices are still ignored. Same change as upstream #2191, rebased onto this snapshot.
- `ironrdp-svc` no longer sets `CHANNEL_FLAG_SHOW_PROTOCOL` on messages that need more than one
  chunk. Per MS-RDPBCGR 2.2.6.1.1 that flag means the 8-byte Channel PDU Header MUST be visible to
  the application endpoint; Windows then hands the header to `rdpdr.sys` as part of the RDPDR byte
  stream, the parse desynchronizes, and the redirected device is killed. A 2000-byte read response
  was enough to make every later open and copy fail with "0x8007048F 设备没有连接", while messages
  that fit in a single chunk (no flag) kept working. Channels that need the header visible
  (CLIPRDR, RAIL) still request it explicitly through `SvcMessage::with_flags`.
- `ironrdp-rdpdr-native` caps a single read response at 16 KiB of data. The server requests
  `0x8000`-byte reads; answering with the full `0x8000`-byte-plus-header completion made Windows
  silently kill the redirected device after the first large read, so every later open and copy
  reported "0x8007048F 设备没有连接" while earlier reads kept succeeding. A short read is a legal
  partial completion and rdpdr.sys chains reads for the remainder.
- `ironrdp-rdpdr` announces filesystem device names as 8-bit null-terminated characters instead of
  UTF-16. Windows reads the `DeviceData` field as 8-bit characters, so a UTF-16 name surfaced only
  as its first letter (`\\tsclient\t` for `trd rw`; Windows 11 build 26300, Windows Server 2022 and
  2025 behave this way). Non-ASCII and `PreferredDosName`-forbidden characters become `_`, a
  trailing colon is kept. Same change as upstream #2342, rebased onto this snapshot.
- `ironrdp-rdpdr` and `ironrdp-rdpdr-native` print every announced device name, every filesystem
  IRP, and every refused IRP on stderr when the environment variable `NYATERM_RDPDR_TRACE` is set.
  The consuming application installs no `tracing` subscriber, so upstream's `debug!`/`trace!`
  records would never be visible while debugging a "无法访问 \\tsclient\..." report.
- Both crates also append the always-on diagnostics (announced names, accepted/rejected
  announcements, dropped IRPs, and every refused read/write/create/query IRP) to the file named by
  `NYATERM_RDPDR_LOG`, because stderr is invisible for windowed (`windows_subsystem = "windows"`)
  launches, which is how end users run the app.
- `ironrdp-rdpdr-native` drops create-option bits it does not model instead of refusing the whole
  create IRP with `STATUS_NOT_SUPPORTED`. Explorer sends cache/capacity hints (for example
  `FILE_NO_INTERMEDIATE_BUFFERING`) when opening a redirected volume; MSTSC and FreeRDP ignore
  unknown bits, and so refusing them makes a healthy share unopenable on the remote side.
  `FILE_OPEN_BY_FILE_ID` is still refused because it cannot be resolved inside the redirected root.
- `ironrdp-rdpdr-native` no longer sets `OBJ_DONT_REPARSE` when opening the redirected root by its
  absolute NT path. The `\??\<letter>:` prefix of any drive path is a symbolic link by design, so
  that flag made every root open fail with `STATUS_REPARSE_POINT_ENCOUNTERED` (0xC000050B), the
  server kept retrying the create IRP forever, and File Explorer on the remote side reported
  "无法访问 \\tsclient\...". The flag still applies to component names opened relative to the
  redirected root handle, where it keeps server-controlled paths from escaping through reparse
  points. Covered by the `open_object_attributes` unit tests and the real-I/O root tests.


- **Standard RDP Security client path** (legacy RC4 security layer, `PROTOCOL_RDP`): the connector
  now supports real Windows servers whose "RDP security layer" policy forbids TLS/CredSSP
  (`SSL_NOT_ALLOWED_BY_SERVER`). Previously only the `ENCRYPTION_LEVEL_NONE` named-pipe variant
  worked; a negotiated RC4 level was rejected outright.
  - `ironrdp-pdu`: new `rdp::standard_security` module — session key derivation
    ([MS-RDPBCGR] 5.3.4/5.3.5), MAC computation (5.3.6), per-direction continuous RC4 streams with
    the 4096-packet key update, `SecurityExchangePdu`, proprietary server certificate decoding, and
    raw-key RSA PKCS#1 encryption (backed by `num-bigint`; the crate already depended on
    `sha1`/`md-5`). Unit tests use vectors computed from the spec formulas with an independent
    implementation.
  - `ironrdp-connector`: new `RdpSecurityExchange` state sends the CS_SECURITY_EXCHANGE PDU after
    MCS channel join ([MS-RDPBCGR] 1.3.6.1.2); the client GCC security block advertises the
    40/56/128-bit methods; the Client Info PDU, licensing PDUs (both directions), connect-time
    auto-detect responses, the Client Confirm Active, and all connection-finalization PDUs are
    sealed, and the matching inbound PDUs are unsealed, via a shared `RdpSecurity` state that is
    handed to `ConnectionResult::rdp_security` (and carried by the activation factory for
    Deactivation-Reactivation). Framing helpers live in the connector-internal `standard_security`
    module. FIPS remains unsupported and is rejected. The server certificate's embedded RSA
    signature is parsed but not verified (Standard RDP Security provides no server authentication on
    the wire; FreeRDP validates it only against the well-known TSSK).
  - `ironrdp-session`: new `security` module. The X.224 processor and the Fast-Path processor share
    the connector-negotiated `RdpSecurity` state through `ActiveStageBuilder::security`; inbound
    I/O-channel, message-channel and fast-path output frames are unsealed, and all outbound traffic
    (Share Data PDUs, SVC chunks, auto-detect responses, fast-path input, Frame Acknowledge) is
    sealed. MAC mismatches are logged and tolerated, matching FreeRDP's stance on the broken-by-design
    legacy layer.
  - `ironrdp-client`: `ConfigBuilder::with_standard_rdp_security(bool)` overrides the
    transport-derived default (Windows named pipes still opt in automatically).
  - **Proprietary server certificate decoding fixed against real servers** (Server 2008 R2
    verified): the blob has no leading length field and no `dwVersionLen` — it starts directly with
    `dwVersion` (whose low bits select the format, `CERT_CHAIN_VERSION_1` for the proprietary
    layout); `wPublicKeyBlobLen`/`wSignatureBlobLen` cover only the bytes that follow their own
    2-byte type + 2-byte length headers. The previous decoder read a phantom length + version
    length, desynchronized on the very first field, and failed with "received 1 bytes, expected 16
    bytes" before the security exchange could start.
  - **Session key derivation fixed against real servers** (Server 2008 R2 verified): the session key
    blob is `SaltedHash(MasterSecret, 'X'/'YY'/'ZZZ', ClientRandom, ServerRandom)` — FreeRDP's live
    `security_establish_keys` path (`security_A` then `security_X`, no salt-order swap). The previous
    derivation used the `'A'`/`'BB'`/`'CCC'` master hashes with a swapped salt order, i.e. the
    `security_session_key_blob`/`security_master_hash` pair that FreeRDP defines but never calls; it
    produced a wrong MacKey and wrong RC4 keys, so every inbound server PDU decrypted to garbage.
    FIPS keeps the same MacSaltKey (FreeRDP copies `session_key_blob[0..16]` for every mode).
  - **Licensing exchange framing fixed against real servers** (Server 2008 R2
    capture-driven, FreeRDP cross-checked):
    - the `LicensePdu`'s embedded `BasicSecurityHeader` is the packet's *outer*
      framing: it is stripped before sealing and re-assembled from the
      clear-text header after unsealing (previously it was duplicated inside
      the encrypted body inbound and outbound alike);
    - `LicenseHeader::decode` no longer requires `SEC_LICENSE_PKT`: a captured
      Windows Server 2008 R2 sends its licensing PDUs with the `SEC_ENCRYPT`
      flag only (FreeRDP `license_read_preamble` never checks the flags);
    - client licensing PDUs advertise
      `PreambleFlags::EXTENDED_ERROR_MSG_SUPPORTED` (FreeRDP `license_send`);
    - the licensing certificate's RSA modulus excludes the 8 trailing zero
      bytes of the key blob (FreeRDP `ModulusLength = keylen - 8`); feeding the
      padding made the modulus 2^64 times too large so the server could not
      decrypt the premaster secret;
    - the licensing sequence now *moves* the connector's `RdpSecurity` state in
      and back per step instead of cloning it: a stale clone desynchronizes the
      RC4 keystreams after licensing (every post-licensing PDU garbles);
    - non-licensing traffic is told apart by *content* (an inbound PDU that
      does not decode as an MS-RDPELE PDU is ignored with the state kept),
      because Windows sets no distinguishing flag and uses the I/O channel --
      captured: channel 1003, `SEC_ENCRYPT` only.
  - MAC placement follows FreeRDP (`rdp_security_stream_out`/`rdp_decrypt`): the 8-byte MAC travels
    in the clear and only the body is RC4-encrypted.
- Standard RDP Security follow-up (FreeRDP cross-check of
  `libfreerdp/core/security.c` / `rdp.c`):
  - Outbound frames now always select the **salted** MAC variant
    (`SEC_SECURE_CHECKSUM` + use-count appended to the SHA-1 input,
    FreeRDP `security_salted_mac_signature`); inbound frames pick the MAC
    variant from the *sender's* `SEC_SECURE_CHECKSUM` header bit, with the
    decrypt use count minus one (the MAC covers the already-counted
    plaintext). `RdpSecurity::seal/unseal` take the variant explicitly.
  - **FIPS-140-1 mode implemented** (`ENCRYPTION_METHOD_FIPS`, previously
    rejected): X/YY/ZZZ premaster session-key blob, 20-byte HMAC-SHA1 sign
    key (hand-rolled HMAC to avoid a `digest`-version mismatch),
    `fips_expand_key_bits` (reverse + 7-bit expand + odd-parity tables from
    FreeRDP `security.c`), and continuous zero-IV 3DES-CBC via the `des`
    crate (new `ironrdp-pdu` dependency, 0.9). FIPS wire layout per FreeRDP:
    `len(0x10) + version(0x01) + pad + MAC(8) + ciphertext(data || zero-pad)`,
    sender pads to the block size.

- **TLS upgrade floor lowered to TLS 1.0** (native-tls/SChannel backend): a captured Windows
  Server 2008 R2 offers only TLS 1.0 with AES128/SHA1, which modern Windows client policy refuses
  (`SEC_E_ALGORITHM_MISMATCH`). `ironrdp-tls` now sets `min_protocol_version(TLS 1.0)` on the
  connector builder; certificate trust stays with the validation policy.

- **Security Exchange client random: legacy RSA, not PKCS#1** (FreeRDP
  `crypto_rsa_common` cross-check; explains `mac_valid=false`): RDP's legacy RSA
  evaluates modulus, exponent and input in LITTLE-ENDIAN byte order with no
  padding scheme at all (FreeRDP reverses each byte array before
  `BN_mod_exp` and reverses the output back). The previous implementation used
  big-endian PKCS#1 v1.5 type 2, so the server decrypted a different
  "client random", every derived session key differed, and all inbound MACs
  failed. `rsa_public_encrypt_pkcs1` is replaced by
  `rsa_public_encrypt_le` (little-endian, zero-padded ciphertext).

- **Fast-path input under Standard RDP Security: only the event data is
  sealed** (FreeRDP `fastpath_send_multiple_input_pdu` cross-check; explains
  "connect, desktop appears, then the server resets as soon as the mouse
  moves"): the fast-path header (flags byte + PER length [+ numEvents]) stays
  in the clear, then `MAC || RC4 ciphertext` covers the *event list only*, and
  the PER length is rewritten to span the whole PDU. The previous code sealed
  `frame[1..]` — which swallowed the length field into the ciphertext and
  dropped it from the wire — and set `FASTPATH_INPUT_ENCRYPTED` in the wrong
  bit (0x02, inside the `numEvents` field, instead of 0x80).
  `security::seal_fastpath_input` now owns the layout, the salted MAC variant
  matches the `FASTPATH_INPUT_SECURE_CHECKSUM` bit it sets, inbound
  `unseal_fastpath` picks its MAC variant from the received header, and
  `security::tests::fastpath_input_sealing_keeps_the_header_in_the_clear`
  locks the wire format.

- **InputOnly CreateWindow 的深度展开顺序**（firefox `BadMatch (request_code 1)`）：
  `create_window` 先把 `depth == 0` 展开成 `parent.depth`，再做 InputOnly 的
  `depth != 0 → BadMatch` 校验，于是 firefox/GTK 的 1x1 InputOnly 子窗口
  （`depth=0 class=InputOnly visual=0`）必然被拒。现在 InputOnly 在展开之前
  分流：depth/visual 必须为 CopyFromParent、border_width 必须为 0，窗口 depth
  存 0 且不分配像素，合成器跳过这类窗口。

- **InputOnly 的 visual 必须被忽略**（firefox `BadWindow (request_code 18 ChangeProperty)`）：
  firefox/GTK 在创建 10x10 的 InputOnly 窗口时传的是默认视觉（0x21）而不是
  CopyFromParent，Xorg 对此不校验（InputOnly 没有可绘制内容）。上一版把
  `visual != 0` 也当成 BadMatch，于是窗口被整个丢掉，客户端紧接着对它设的十几条
  属性全部 BadWindow，GDK 直接 abort。现在只校验 `depth`（必须为 0），visual 忽略。
  另加：CreateWindow 失败结果与 ChangeProperty BadWindow 都落诊断日志。

- **诊断探针改成按需开启 + X 连接关 Nagle**（修"终端打字半天才回显"）：
  排障期加的逐 PDU/逐请求探针每条都做一次"open+write+close"，在忙碌会话里
  每秒上千次同步文件 I/O，直接把输入回显拖慢。逐 PDU 的 `[session]`、
  `[session-pdu]`、`[svc-out]`、`[reactivation] frame len=`/`step: waiting`、
  fastpath 计数采样全部删除；逐请求的 x11 日志改由 `NYATERM_X11_TRACE=1`
  门控（失败诊断保留为常开）。另外 xsrv 的 accept 循环补上
  `set_nodelay(true)`：X11 全是小包，Nagle 会把按键事件攒着等 ACK。

- **X 帧泵：脏区对比下沉到服务端**（修"终端打命令像慢镜头"）：
  旧实现每 33ms 一次整屏快照（`to_vec`）+ 整屏比对 + 基准帧拷贝，1080p 下
  约 750MB/s 纯内存带宽，仅 X 标签开着就把一个核吃满，整个应用（含终端回显）
  被拖成慢镜头。现在 `ServerState` 维护基准帧（`sent_frame`）与脏区提示
  （compose/映射/销毁/移动时并入窗口矩形），`take_frame_if_changed` 只在
  提示范围内逐像素比对并就地更新基准；空闲时零拷贝，动画客户端只处理小矩形。
  帧泵删掉了自己的整屏缓冲。

- **CapsLock 对齐改用 Input Event Sync**（修"大写传不过去"）：折算大写字母时
  Shift 处于按下状态，Windows 忽略按住 Shift 时的 CapsLock 切换——注入
  CapsLock 扫描码会静默失效而客户端却乐观更新了状态。现在 RDP 路径发送
  Input Event Sync（`pad2Octets + toggleFlags`，绝对设置 scroll/num/caps，
  不受修饰键影响）；VNC 保留 keysym 路径（Caps_Lock keysym 不受 Shift 影响）。

- **CapsLock 对齐最终方案：每次按键绝对同步**（修"CapsLock 按键无效/大小写
  反转"，Win7/2008/Win10 一致）：gpui 不向应用投递 CapsLock 按键事件，"感知
  本地切换再折算"从根上不成立；服务器下发的 Set Keyboard Indicators 又只在
  远端状态变化时出现，无法作为进入状态的依据。最终改为确定性方案——每个
  按键 down 事件前发送 Input Event Sync（绝对设置 scroll/num/caps 为本地
  GetKeyState 状态），远端锁定键永远等于本地 LED，不依赖任何猜测或学习。
  旧的 remote_caps 猜测状态机整体删除；KeyboardIndicators 管线保留（LED
  显示备用）。VNC 用 keysym 输入，字符自带大小写，无需对齐。

- **X server 宣告并接受 1/4/8/24/32 位图深度**（修 firefox "窗口永不出现"）：
  firefox/GTK 启动时创建 1x1 测试窗后用 CreatePixmap 探测 depth 4/8 位图格式，
  服务端只支持 24/32 并回 BadValue，客户端随即销毁测试窗放弃建窗——表现为
  "没有任何报错但窗口永不出现"。现在 setup 宣告全部五种深度（1bpp=1bit/pixel、
  4bpp=16、8bpp=8、24/32=32，均 32-bit 扫描行对齐），CreatePixmap 同样放行；
  像素统一按 u32 存储，浅深度的位图语义由使用方（SHAPE 掩码/贴图）解释。

- **ZPixmap 按**目标深度**解包 + 修正宣告的位图格式**（修 firefox cairo 断言
  崩溃 `cairo-xlib-surface.c:1178 _cairo_xlib_surface_draw_image`）： PutImage
  原来把所有非 1 位 ZPixmap 都当 4 字节/像素解析，cairo 往 8/16 位深的
  pixmap 画图（行按 32 位对齐）时整体错位。现在 `put_zpixmap_depth` 按
  24/32=4B、16=2B（RGB565→8bit 通道展开）、8=1B、4=半字节打包逐深度解包，
  行宽一律补齐到 32 位。setup 的 pixmap_formats 修正为真实 Xorg 数值
  （depth 4 = 8bpp 而非 16，补上漏掉的 depth 16 = 16bpp）——cairo 按这里的
  bpp/pad 计算行宽，数值错会让断言直接失败。

- **RENDER PictFormat 掩码改为位掩码语义**（修 firefox cairo 断言崩溃
  `cairo-xlib-surface.c:1178` 附近的 `assert(ret)`）：cairo 用
  `a_mask = direct.alphaMask << direct.alphaShift` 组合通道掩码，在真 Xorg
  上得到 0xff000000——证明线上语义是**位掩码**。此前把 mask 当"通道位数"（8）
  发送，cairo 组出 0x08000000 这类非法掩码，`_pixman_format_from_masks`
  ret=0 直接 assert 退出。现在 ARGB32/RGB24/A8 全部发 0xff 位掩码。
  （配合前两条：pixmap 多深度宣告 + ZPixmap 按深度解包，firefox 的建窗、
  探测、绘制路径才走通。）

- **RENDER Composite 实现完整 PictOp 混合族**（修 firefox
  `RenderBadPictOp request 140 minor 26`——CreateSolidFill/Composite 的 CSS
  混合模式）：原来只支持 Src/Over/Atop/In，GTK4/firefox 大量使用
  Multiply/Screen/Overlay/Darken/Lighten/ColorDodge/ColorBurn/HardLight/
  SoftLight/Difference/Exclusion（0x30-0x3a）与 HSL Hue/Saturation/Color/
  Luminosity（0x48-0x4b），遇到就回 BadPictOp 让客户端 abort。现在：
  Porter-Duff 全家（目标不透明简化）、可分离混合按 CSS/PDF 公式逐通道计算
  （源先反预乘），HSL 混合按 SetSat/SetLum/ClipColor 标准函数实现，输出
  `(1−αs)·Cd + αs·B` 复合回不透明目标。

- **X 回复的 length 字段必须回填**（修 firefox "无报错但主窗口永不出现"）：
  `send_reply` 把 bytes[4..8]（length，单位 4 字节、不含 32 字节固定头）恒设为 0，
  构造 reply 时该字段是 0 且从未改写。32 字节的回复（QueryExtension/InternAtom/
  GetGeometry）恰好正确——这就是 xeyes 一直能跑的原因；但 44 字节的
  GetWindowAttributesReply 少报 12 字节，客户端只消费 32 字节，余下字节被当成
  下一条回复的开头，**整个协议流错位**：后续出现 id=0 的窗口查询、firefox 建完
  探针窗口就静默卡死。现在统一按 `(总长/4) - 8` 回填，并加回归测试
  `reply_length_field_matches_the_frame_size` 锁定"length 与总长自洽"不变式。

- **RENDER 采样按源深度取 alpha + VNC CapsLock 透传恢复**：
  1. `sample_picture` 对 pixmap 源强制 alpha=255，而 firefox 的 UI 基底是
     ARGB32、透明区域 alpha=0 → 渲染成全黑窗口（只有一条进度线可见）。
     `put_zpixmap_depth` 在 depth 32 时曾抹掉 alpha 字节——现在保留完整
     0xAARRGGBB，`BorrowedPixels` 带上源深度，采样按深度取 alpha（32 位取
     高字节，其余按不透明约定）。Composite 后firefox 的透明基底与内容叠加
     正确（配 X 输出的黑背景即正常深色主题底色）。
  2. CapsLock 重构时误删了 VNC 的透传路径（keysym 输入必须以远端锁定键为准）
     ——恢复 VNC 协议下 Caps_Lock keysym 按下+抬起透传；RDP 仍走绝对 Sync。

- **RENDER 三种渐变实现**（修 firefox `BadImplementation request 140 minor 27`
  = CreateLinearGradient）：CreateLinear/Radial/ConicalGradient 落地为
  PictureSource 渐变源（停靠点 offset 用 x11rb 的 i32 Fixed(16.16)/65536 换算），
  采样时按 RENDER 规范的几何参数现算颜色：线性用轴向投影 t；径向用双圆二次
  方程取 [0,1] 根（a≈0 时退化为点圆公式）；锥形用方位角差归一化。停靠点间
  线性插值（未预乘），输出预乘后走通用混合。Composite 组装处渐变源与 Solid
  同等处理（无像素缓冲，采样现算）；作为绘制目标时按规范回 BadMatch。

- **RENDER CreateCursor 实现**（修 firefox `BadImplementation request 140
  minor 27`）：x11rb 的官方编号 minor 27 = CreateCursor（渐变是 34/35/36，
  已实现）。CreateCursor 从 picture 创建图标光标，画面不渲染光标内容——
  登记 ID 与属主、校验 source picture 存在即可，此前直接回 BadImplementation
  让 firefox abort。
- **VNC 全按键失效修复**：CapsLock 状态检测分支里误加了 `return`，VNC 的
  每个按键 down 都被吞掉。现在检测到本地 CapsLock 状态变化时透传一次
  Caps_Lock keysym（按下+抬起），**随后继续走普通按键路径**。

- **VNC keysym 按本地实际输出构造 + SPICE CapsLock 补检测**（修 VNC CapsLock
  无效）：VNC 的 keysym 自带大小写——探针显示本地 caps ON 时（key_char="A"）
  发出去的 keysym 是 0x61（小写），导致远端打出小写。现在 ASCII 字符的 keysym
  直接用码点（'A'=0x41、'a'=0x61、'!'=0x21）——按本地键盘输出的键为准，远端
  就是显示输出。SPICE 此前完全没有 caps 同步（死路），现在与 VNC 共用同一个
  CapsLock 透传 KeyInput：VNC 取 keysym 0xffe5（XK_Caps_Lock），SPICE 取
  scancode 0x3a（Linux KEY_CAPSLOCK = 58，set-1 扫描码 = Linux keycode 映射
  已验证正确）。字段 vnc_caps_sent 改名 caps_sent（VNC/SPICE 共用）。
- **光标持有 picture 引用（X11 引用计数语义）**（修 firefox `RenderBadPicture
  minor 27`）：firefox 的 cursor 流程是「建 picture → CreateCursor → 立刻
  FreePicture（指望光标持有引用）→ 再建第二个 cursor 引用同一 picture」。
  此前 FreePicture 直接删 picture，第二次 CreateCursor 就 BadPicture。现在：
  cursors 映射改为 CursorRef { owner, picture }，RENDER CreateCursor 记录
  picture 引用，FreePicture 检查光标引用（有引用则保留 picture），FreeCursor
  释放引用并销毁 picture。新增回归测试 `cursor_holds_picture_reference`。
- **Xfixes SetCursorName 处理器**：此前落默认 BadRequest（日志
  `XfixesSetCursorName -> BadRequest`），GTK/firefox 设置光标名时会收到错误。
  现在校验光标存在即可（画面不渲染光标名）。

- **picture 引用 pixmap 的引用计数语义**（修 firefox `RenderBadPicture
  minor 27` 第二层）：GTK 的光标流程是「CreatePixmap → CreatePicture →
  DestroyPixmap（指望 picture 持有引用）→ CreateCursor → FreePicture」。
  此前 FreePixmap 直接删 pixmap 并连带删掉包装它的 picture，CreateCursor
  就 BadPicture。现在 FreePixmap 检查 picture 引用（有引用则 pixmap 与
  picture 都保留）。新增回归测试 `picture_holds_pixmap_reference`。

- **合成批处理**（修 firefox "慢镜头" 4fps）：compose() 是全屏重建
  （填 1280x800 背景 + 重画所有映射窗口），此前**每个绘制请求后都调用**
  （15 个调用点：draw_with_gc / finish_draw / shape 系列 / map/unmap 等）。
  firefox 一次重绘发几百个绘制请求 = 几百次全屏合成/帧。现在：绘制与
  结构变化只 `mark_compose_dirty()`，帧泵 take_frame_if_changed 入口
  `ensure_composed()` 统一合成（30fps 上限），GetImage(root) 前也
  ensure_composed 保证读到最新合成结果。DamageNotify 跟随合成节奏批量发出
  （≤33ms 延迟，语义可接受）。composite_and_damage 测试改为手动
  ensure_composed 后收通知。

- **请求直方图 + 连接起止探针**（诊断 firefox 卡顿/子进程崩溃）：心跳新增
  `[hb] reqs-top: {major<<8|minor ×次数}` top-6（5s 窗口增量）与
  `conns=+握手数/-断开数`；serve_stream 拆出 serve_stream_inner，连接结束
  （错误）写日志。数据背景：firefox 启动后 reqs=+1300~1600/5s、
  composes=+0 frames=+0、cpu=+3s/5s 且伴随 6 次 "[Child][RunMessage]
  Channel closing"——内容子进程崩溃循环，刷的是未知请求。这版跑一次
  firefox，直方图会直接给出它刷的是什么。

- **修复直方图不显示的 bug**：note_request 与 take_histo 函数内各自声明了
  `static HISTO`（两个独立的 Mutex<Option<HashMap>>），写和读不是同一张表，
  reqs-top 永远为空。提升为模块级共享 static。顺带把心跳的 conns 改为
  5 秒增量（此前打的是累计值）。

- **停止宣告 MIT-SHM**（修 firefox "只有鼠标动才刷新"）：请求直方图显示
  浏览期间 firefox 只发 QueryPointer（~300/s，GTK4 无 XInput2 时的帧时钟
  指针轮询），**零绘制请求**——WebRender 用 XShmPutImage 发帧，我们的
  ShmPutImage 回 BadMatch，帧被静默丢弃（X 错误还会杀死内容子进程 = 启动时
  的 "Channel closing" 消息）；鼠标移动时看到的更新是 GTK/cairo 用普通
  XPutImage 重绘 chrome。宣告 SHM 却全部失败是最坏组合——现在从扩展注册表
  移除 MIT-SHM（QueryExtension 应答 absent），客户端从起点走非 SHM 路径
  （OpenSSH 远程 X 转发同理）。SHM 处理器保留（handler 级测试仍有效）。

- **SSH 传输启用 zlib 压缩**（修远程 X 应用"慢镜头"的带宽瓶颈）：X11 转发的
  PutImage 是大块 RGB 像素（整窗重绘 1~4MB/次），SSH 隧道吞吐直接决定远程
  X 应用流畅度。hardened_config 的 preferred.compression 头部插入 ZLIB（立即
  生效）与 ZLIB_LEGACY（zlib@openssh.com，认证后启用），UI 像素内容压缩比
  2~5x；服务器不支持时协商回落 none 无害。（russh 的 flate2 feature 本就启用。）

- **window 0 = None 的查询优雅应答**（修 firefox 内容子进程启动即死）：
  请求直方图 + 前序探针确认每个客户端在 seq60-61 查
  `GetWindowAttributes(0)` / `GetGeometry(0)`（GTK 启动期 WM/合成器探测：
  GrabServer → GetSelectionOwner → 拿到 0 就直接查，无空值检查），BadWindow
  错误未捕获 → Xlib 默认自杀 → "Channel closing" → 内容进程死了页面全黑。
  现在 GetWindowAttributes(0) 回根窗口风格应答（VIEWABLE/INPUT_OUTPUT），
  GetGeometry(0) 回根窗口几何，GetProperty(0) 回无属性——探测安全通过。
  err->c 探针（错误发送 + 前序 5 条请求）保留。

- **根治 GTK3 xsettings 探测缺陷（"Channel closing" 终极修复）**：源码级定位
  ——gtk-3-24 xsettings-client.c `check_manager_window()` 对 XGetSelectionOwner
  结果不判 None，直接 `gdk_x11_window_foreign_new_for_display(display, 0)`，
  随后 `XSelectInput(0, PropertyChangeMask|StructureNotifyMask)` 无陷阱保护，
  任何错误 → `_gdk_x11_display_error_event` → `_exit(1)`（子进程成批死亡、
  内容黑屏）。真实桌面从不触发因为 gnome-settings-daemon 持有 `_XSETTINGS_S0`。
  根治：ServerState::setup_xsettings_manager() 开机自建 1x1 InputOnly 窗口
  （id=ROOT-16，compose 跳过 InputOnly 不会上屏）并持有 `_XSETTINGS_S0` 所有权；
  同时把 window-0 的全部 fake-success 回滚为 X.org 原生语义（attrs→BadWindow、
  geom→BadDrawable、tree→BadWindow、CWA→BadWindow、property→BadWindow）。
  探针链证据：InternAtom("_XSETTINGS_S0")=13字节 + GetSelectionOwner→0 +
  XSelectInput 参数 2048/4325376 逐字匹配。

- **修复"画面只在顶部一条条慢慢展开"（脏矩形高度被截断）**：
  ServerState::take_frame_if_changed 的内层比较循环里，当**某一行整行都不同**
  时执行了 `break ''rows`——跳出的是**所有行**的扫描，脏矩形 dy1 被截断到那一
  行（实际高度 1 像素）；而 sent_seq 已经推进，剩余行**永远不会补发**。
  firefox/WebRender 的整窗 PutImage 正是"整行都不同"，所以每帧只发一条 1 像素
  高的细缝，鼠标移动触发新绘制才再展开一条——与用户描述完全一致。
  修复：改为 `break`（只结束本行扫描，继续扫后续行以取得正确的 dy 范围）。
  回归测试 `dirty_rect_covers_rows_after_a_full_width_row`：把 bug 版本改回去
  实测失败并报 "实际高度 1"，证明测试确实抓得住这个 bug。

- **修复"页面元素点不动"（指针事件投递语义）**，依据 X.org dix/events.c 源码：
  1. **传播规则**：`DeliverDeviceEvents` 在第一个成功投递的窗口处 `break`，
     祖先不重复收。原实现把祖先链上**每个**选择者都投一遍，firefox 的内容
     子窗口与顶层窗口同属一个客户端 → 一次点击收到两份 press/release → GDK
     点击状态机被破坏。
  2. **隐式抓取**：`DeliverEventsToWindow` 投递 ButtonPress 时调用
     `ActivateImplicitGrab()`，此后指针事件投给按下时的窗口直到全部抬起。
     新增 `InputState::implicit_grab`：首次按键建立、全部抬起后释放；移动与
     释放都优先投给抓取窗口（拖拽、选中文字、点击后窗口树变化都依赖它）。
  3. **按住键时的运动事件过滤**：X.org `SetMaskForEvent` 每次按下都会把
     MotionNotify 的过滤器加上对应 ButtonNMotionMask；现在 motion_mask 在
     按住期间并入 `button_mask`（其位就是 1<<(7+n) = ButtonNMotionMask）。
  回归测试：`button_press_is_delivered_once_not_to_every_selecting_ancestor`、
  `implicit_grab_keeps_release_on_the_press_window`（都实测过 bug 版会失败）。

- **修复鼠标光标闪烁/飘忽（X11 会话）**：`pointer_at`（所有后端共用）在**每次
  鼠标移动**时无条件调用 `win32::hide_cursor()`（`SetCursor(NULL)`）。内置 X
  server 的 X11 后端**不下发任何 Cursor 事件**，所以远端光标根本不存在；本地
  光标被藏掉后画面上没有光标，而 Windows 每次 WM_SETCURSOR 又让 gpui 把它恢复
  成默认箭头——两者来回拉锯 → 光标持续闪烁。改为仅在 `self.cursor.is_some()`
  （确实要自己绘制远端光标，如 RDP/VNC）时才隐藏本地光标。

- **修复 firefox 运行中崩溃（BadMatch, request_code 78）**：CreateColormap 只接受
  ROOT_VISUAL，但握手时我们通告了**两个**视觉（depth24 的 ROOT_VISUAL 与 depth32
  的 ARGB32_VISUAL）。GTK 为弹窗/半透明 surface 用 depth32 建 colormap 时收到
  BadMatch → GDK 直接退出（日志实测 seq4526 major=78 与 GDK 报的 serial 4526
  完全一致；x11rb 确认 CREATE_COLORMAP_REQUEST=78）。现在两个视觉都接受，并按
  协议补上 alloc 校验（仅 AllocNone/AllocAll，否则 BadValue）。
  回归测试 `create_colormap_accepts_both_advertised_visuals`（bug 版实测报
  code 8 = BadMatch）。
  附带确认：GTK 只用 XCompositeRedirectWindow/UnredirectWindow
  （CompositeRedirectManual），不调用 NameWindowPixmap —— 那两个请求我们已接受。

- **修复"过一会界面就操作不了"（主线程被无条件重绘占满）**：
  心跳 `cpu` 是**本进程** CPU 时间，实测 `cpu=+4~5s/5s`（几乎占满一个核），
  而同一时刻 `composes=+0 frames=+0`——说明瓶颈不在 X 路径，而在 UI 主线程。
  根因：app.rs 渲染尾部只要**存在**终端/远程桌面/连接中标签就无条件
  `request_animation_frame()`，即以刷新率持续全 UI 重绘；主线程被占满后输入
  事件排队，用户感觉"过一会界面就操作不了"。
  改为**按需续帧**：`counters::UI_DIRTY` 由"有新内容"的三处置位——终端
  `pane.pump()` 返回有新输出、远程面板 `poll()` 收到新画面、端口扫描结果变化；
  render 尾部只有 `take_ui_dirty()` 为真或空闲兜底 100ms 到期才续帧。
  效果：有数据全速流畅，空闲降到 ~10fps，主线程不再被空转吃掉。
- **指针按键自愈对账**：新增 `XInputEvent::PointerWithButtons{x,y,buttons}`，
  服务端每次移动都与 UI 的权威按键掩码对账，补发丢失的 Release 并释放隐式
  抓取（鼠标在画面外松开/窗口失焦会导致抓取永远卡住，之后所有指针事件都投给
  旧窗口）。回归测试 `pointer_move_reconciles_a_lost_button_release`（实测
  bug 版会失败）。同时把高频 `[ptr]` 探针换成低频 `[btn]` 边沿日志。

- **回滚"按需续帧"（我自己引入的死锁式卡顿）**：GPUI 只在被 notify 时重绘，而
  远程画面的 RdEvent 只在 render() 里被 poll() 消费——没有任何后台任务会在新帧
  到达时唤醒 UI。停掉续帧后新帧唤不醒界面，画面卡到下一次偶发 notify 才动，
  终端也跟着卡。已恢复每帧续帧，并写清原因防止再犯。
  （附带收获：该实验证明 CPU 诊断正确——空闲 CPU 从 4~5s/5s 降到 0.05~1.5s/5s。）
- **终端快照按需重建**：原来每帧无条件重建整屏快照。现在只有新输出/标题变化/
  焦点变化时立刻重建，否则每 250ms 兜底一次。空闲 CPU 4~5s/5s → 1.45s/5s。
- **RENDER 热路径快路径**（实测请求直方图：FillRectangles 147~360/5s 第一、
  Trapezoids 104~188/5s、CreatePicture/FreePicture ~180/5s、CreatePixmap 90/5s，
  即 cairo 走的是 FillRectangles+Trapezoids 重活路径）：
  · 纯色源 picture 的采样提到循环外（原来逐像素调 sample_picture）；
  · 结果为 dst 的 op（DST/AtopReverse）与"透明源 + Over/Atop"整段跳过，
    且不再标记重绘；
  · 结果为纯色的情形（Clear、不透明源 + Src/Over/Atop/In）整段直接写色，
    不走 blend_pixel。
  同时确认：CompositeGlyphs 从未被调用（文字不是字形路径），RENDER 全部请求
  都有实现（err->c 为空）。

- **修复"滚动一下界面就卡死"（严重：源像素借走不归还）**：
  `BorrowedPixels` 借走 drawable 的像素缓冲用的是 `std::mem::take`，而归还靠
  **显式调用 `restore()`**（没有 Drop 实现）。`blend_spans`
  （Trapezoids/Triangles/TriStrip/TriFan 的核心）**从未归还**：任何"以某个
  drawable 作为源"的梯形绘制都会把那块 drawable 的 pixels 抽空且不还，之后
  compose() 读到空缓冲 → 画面冻结，或线程 panic 毒化状态锁 → 整个界面再也画不
  出来。用户实测：滚动一下就卡死（滚动恰好会触发这类绘制）。
  修复：绘制体收进块内让像素借用先结束，**成功/失败/空操作三条路径统一归还**；
  空操作（DST/AtopReverse）不再标记重绘。
  回归测试 `trapezoids_restores_the_borrowed_source_pixels`——实测不归还时
  断言失败（left: 0, right: 64，即源缓冲被抽空），证明测试抓得住。

- **修复"操作一会就卡死（约 1 分钟）"的真正根因：抓取是假的**。
  `grab_pointer`/`grab_keyboard` 以前只回 `Success` 而**不重定向事件**（注释里
  写着"不真正冻结事件流"）。GTK 拿到成功状态后进入拖动/菜单状态机，期待后续
  指针事件都投到 grab_window；我们仍按位置投递，它的状态机永远等不到事件，
  于是退化成 QueryPointer 轮询（实测 350 次/秒）并**停止绘制**——心跳还在、
  日志里 `composes=+0 frames=+0` 持续 50 秒以上，就是用户看到的"界面卡死但
  进程还活着"。点开菜单/下拉框/拖动条就会触发，所以"操作一会就卡死"。
  修复：真正实现抓取语义
  · `InputState.pointer_grab: Option<PointerGrab{window,owner_events,event_mask,client}>`
    与 `keyboard_grab`；移动/按键优先投给抓取窗口（优先级：显式抓取 > 隐式抓取
    > 位置命中）；
  · `focus_target()` 认键盘抓取（菜单依赖）；
  · `UngrabPointer`/`UngrabKeyboard`/`ChangeActivePointerGrab` 不再是空操作；
  · 客户端断开时自动解除它持有的抓取（否则会把指针永久钉在消失的窗口上）。
  回归测试 `explicit_pointer_grab_redirects_events_to_the_grab_window`——实测
  不重定向时事件投给了指针下的窗口（2097154）而不是抓取窗口（2097153）。

- **XInput2 最小实现（XI2 Phase 1，消灭 GTK 指针轮询的第一块）**：
  调研文档 docs/research/minimal-xi2-for-gtk3.md + gtk3-poll-freeze-report.md
  交叉验证后实施：
  · 注册 XInputExtension（opcode 131，MIT-SHM 弃用后的空位）；
  · xinput.rs：XIQueryVersion（回 min(请求,2.0)）、XIQueryDevice（VCP=2/
    VCK=3 两个主设备，零从设备）、XISelectEvents（按窗口×客户端×deviceid
    存储掩码）、XIQueryPointer（FP1616）、XIGrabDevice/XIUngrabDevice（映射
    到已有抓取状态）、XIAllowEvents（no-op，从不冻结）、XIGetClientPointer；
  · **XI2 GenericEvent 投递**：Motion(6)/ButtonPress(4)/ButtonRelease(5)/
    KeyPress(2)/KeyRelease(3)，信封 type=35 + extension=131 + evtype/
    deviceid/time/detail/root/event/child + FP1616 坐标 + mods/group/按钮掩码
    （XI_Motion=6，注意不是 15——15 是 XI_RawButtonPress）；
  · **核心抑制**（XI2 规范：XI 投递后核心投递终止）：core_suppressed(client)
    ——XI2 协商成功且做过对应选择的客户端不再收核心输入事件。双投会毁掉
    GDK 点击状态机（点不动回归）。
  已知缺口（下阶段）：XI_Enter/Leave/FocusIn/Out 的 XI2 投递（悬停光标更新
  仍走核心路径给非抑制客户端）、XIQueryDevice 的 classes 为空（GDK 容忍）、
  层级事件（HierarchyChanged）。
  回归测试 `xi2_selection_switches_delivery_to_generic_events`：协商+选择后，
  运动事件必须以 GenericEvent(35) 到达且核心 MotionNotify=0（双投即失败）。
- **修复 QueryPointer 的 child 字段（"操作一会就卡死"的第二根因）**：
  GTK3 `gdk_x11_device_core_window_at_position()` 用 XQueryPointer 的
  child_return 做**下降迭代**（child==None 才停）。我们此前返回全局最深窗口
  ——被查询窗口包含指针时 child=自身/深层窗口 → 主线程**卡死在单个 GDK
  调用里**（QueryPointer 330 次/秒 + 零绘制，心跳仍在）。现改为
  `child_at(查询窗口)`（直接子窗口且含指针，否则 None）——与 X.org 一致。
  回归测试 `query_pointer_child_is_none_when_pointer_is_not_in_a_direct_child`。

- **真实事件时间戳**（第三个智能体排位的第 5 项）：此前所有输入事件的
  time 字段恒为 0（= GDK_CURRENT_TIME）。GTK 用事件时间做**双击判定、菜单
  超时、set_user_time**——恒 0 会让每次点击被判定为"连击"，表现不稳定。
  ServerState 增加 `start_time: Instant`，`event_time()` 返回启动后毫秒数
  （截 u32，与 X 时间戳循环语义一致）；Motion/Enter/Leave/Button/Key 全部
  事件与 XI2 GenericEvent 的 time 字段已填真实时间戳。

- **XI2 crossing + focus 投递补全（Phase 1 收尾）**：GDK XI2 模式只读 XI2
  版本的 Enter/Leave/FocusIn/FocusOut——核心版本对做了 XI2 选择的客户端被
  抑制后，这些事件若没有 XI2 版本，悬停/焦点更新会失效。补齐：
  pointer_motion 在窗口变化时投 XI_Enter(7)/XI_Leave(8)（旧窗口 Leave、
  新窗口 Enter）；set_input_focus 投 XI_FocusOut(10)/XI_FocusIn(9)。
  回归测试 `xi2_enter_leave_delivered_to_selecting_clients`。

- **XI2 wire round-trip 测试**（守门员）：
  `query_device_reply_round_trips_through_x11rb`（ALL → VCP/VCK 两主设备、
  x11rb TryParse 完整解析、无残留字节）、
  `query_pointer_reply_round_trips_through_x11rb`（FP1616 坐标 (100,60) 正确）。
  firefox 的 Xlib 用同一套解析规则——这些字节过不了 x11rb 就过不了 Xlib。

- **运行时实证（20:55:55 构建日志）**：卡死（"50 秒 composes=+0"的冻结）
  已消失——BorrowedPixels 归还修复后每个心跳窗口稳定 `composes≈145
  frames≈143`（~29fps 连续绘制，含滚动）。剩余问题：QueryPointer ~180/s
  轮询 + CPU 4~5s/5s（由 22:20:48 的 XI2 解决）+ GetInputFocus ~15/s。

- **XIQueryDevice 补设备类信息**（消除"0 按钮/0 轴设备可能丢 XI2 按钮"风险）：
  VCP = Button(5 键) + Valuator×2（X/Y 相对模式，范围 ±32768）；VCK = Key
  （keys 数组 248 字 → len=250，与真实 X.org 主键盘基准一致）。len 公式
  = (6 + 类数据字节 + 3) / 4（含 4 字节 type+len 头）。
  round-trip 测试用 x11rb TryParse 验证（含 FP3232 与 992B keys 的完整解析、
  无残留字节）。
- **22:31:15 构建**：XIQueryDevice 类信息补全（Button+2 Valuator / Key）。
  104/104 测试。等待运行时验证（用户跑 22:31:15 或 22:20:34，观察
  reqs-top 里 0x2602 频率是否从 ~180/s 大幅下降）。

- **XI2 wire 守门员测试抓到两个真 bug**（GenericEvent 线格式）：
  1. `pad_event` 无条件 resize 到 32 字节——88 字节的 xXIDeviceEvent 被静默
     截断（release 下 debug_assert 不触发）。XI2 GenericEvent 是完整格式
     （自带尾部填充），投递时直接 clone + 盖写 sequence，不过 pad_event。
  2. FP1616 字节序写成大端——X11 流对客户端是**本机字节序**（其它字段都用
     to_ne_bytes）。改 to_ne_bytes。
  守门员测试 `xi2_motion_generic_event_matches_the_wire_layout` 逐字段断言
  xXIDeviceEvent 布局（信封/extension/length/evtype/deviceid/detail/root/
  event/FP1616 坐标/buttons_len/mods.effective）。

- **修复 XI2 后 firefox 启动即段错误（GetExtensionVersion 缺实现）**：
  GDK3 初始化同时探测 XI2（XIQueryVersion ✓）与旧版 XInput
  （GetExtensionVersion，major=131 minor=1）——后者没实现，回 BadRequest，
  未捕获错误杀死 firefox（用户实测"段错误（核心已转储）"，err->c 日志
  seq69/72 精确指认）。实现：present=true + server 2.0。
  回归测试 `get_extension_version_must_succeed_for_gdk_probe`。

- **XInput 请求全量兜底**（系统性收口"下一个崩溃"）：枚举 x11rb 全部 61 个
  Xinput 请求变体，为 47 个未逐一实现的请求加优雅处理——需要应答的
  （GetDevice*/GetFeedbackControl/GetSelectedExtensionEvents/QueryDeviceState/
  ListInputDevices/OpenDevice/XIGetFocus/XIGetSelectedEvents/XIPassiveGrabDevice
  等 17 个）回 32 字节空应答（length=0 = 空列表/零字段，语义可降级）；无应答
  的（Change*/Set*/Ungrab*/Close*/DeviceBell/XIBarrierReleasePointer/XIChangeHierarchy/
  XIChangeProperty/XIDeleteProperty/XISet* 等 30 个）回 Ok。任何 XInput 请求
  都不再产生 BadRequest——GDK 启动探测与运行时调用全覆盖。
- **修复 XIGetProperty 应答 length 错误（"Extra reply data still left in
  queue" → firefox SIGSEGV）**：x11rb 的 XIGetPropertyReply::serialize_into
  写出 44 字节（32 头 + type/bytes_after/num_items 共 12B），而 length 字段
  留 0 → Xlib 只消费 32 字节，剩余 12 字节滞留队列 → 下一应答读取时
  "[xcb] Extra reply data still left in queue" → abort → 段错误级联
  （用户实测：启动后 GenerateDump + xcb_io.c:664 断言失败）。修复：
  length=3（12/4）。全量审计其余手工应答（QueryPointer=14/88B、
  QueryDevice=按实际字节数、兜底臂=0/32B）均自洽。

- **决策：撤下 XI2 宣告（回滚到稳定的核心设备路径）**。
  原因：XI2 探测链修一个冒一个（GetExtensionVersion → XIGetProperty/XIChangeCursor
  → "Extra reply data"），把此前"基本可用"的 X11 转发拖进崩溃循环。不宣告
  XInputExtension 后 GDK 走核心路径——那条路的全部已知 bug 都已修复并有回归
  测试（单次投递、隐式抓取+对账自愈、真实时间戳、QueryPointer child 下降循环、
  真抓取）。XI2 实现代码保留在 xinput.rs + 47 个兜底臂，后续作为正式项目重启。
- **XIQueryPointer 应答帧长错误（"鼠标一点就崩"的直接根因，已修）**：
  实测 x11rb 对无按钮 XIQueryPointerReply 的序列化是 **56 字节**，而我此前
  `bytes.resize(32 + 56, 0)` 硬编码发 **88 字节**、length 写死 14 → 每次点击
  （GDK 的 window_at_position 必调 XIQueryPointer）多发 32 字节 → 客户端按
  length 消费后残留 → `[xcb] Extra reply data still left in queue` → abort →
  GenerateDump。修复：改用框架的 `send_reply`（x11rb 序列化 + 按实际字节回填
  length），XIQueryDevice 同样从手工 truncate/append 改为 send_reply。
  测试收紧：round-trip 断言**零残留** + `length*4+32 == 帧长` + XIQueryPointer
  固定 56 字节（此前的 "≤3 字节残留" 容差正是放走这个 bug 的原因）。
