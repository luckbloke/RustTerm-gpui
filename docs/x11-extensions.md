# xsrv 扩展支持（X11 extensions）

本文记录 `src/xsrv/` 进程内 X11 服务端的扩展实现状态。注册表（opcode、
first_event/first_error 基址）在 `src/xsrv/extensions.rs`，请求语义同文件，
区域（region）运算在 `src/xsrv/region.rs`，BIG-REQUESTS 分帧在
`src/xsrv/server.rs` 的 `request_loop`。

opcode 采用固定值（参考 Xorg 常见分配）：

| 扩展 | opcode | first_event | first_error |
| --- | --- | --- | --- |
| XC-MISC | 128 | — | — |
| XTEST | 129 | — | — |
| MIT-SHM | 131 | 91 | — |
| SHAPE | 132 | 90 | — |
| BIG-REQUESTS | 135 | — | — |
| XFIXES | 138 | 92 | 128（BadRegion） |
| Composite | 139 | — | — |
| RENDER | 140 | — | 150（PictFormat/Picture/PictOp/GlyphSet/Glyph） |
| DAMAGE | 143 | 94 | 129（BadDamage） |

> 注：SHAPE 的 opcode 用 132 而不是某些资料里写的 87——扩展 opcode 必须落在
> 128..=255（87 是核心协议的 AllocColorPlanes，`x11rb-protocol` 的
> `Request::parse` 会先按核心分发）。客户端从 `QueryExtension` 拿 opcode，
> 只要服务端两侧一致即可。

## 实现状态

### BIG-REQUESTS — 完整支持
- `QueryExtension("BIG-REQUESTS")` 报告存在（opcode 135）；
- `Enable`（XCB 总是用 32 位长度形式发送它）回复 `maximum_request_length`
  = 16 MiB / 4 单位；
- 连接循环：启用后，16 位长度字段为 0 的请求改读随后的 32 位真实长度
  （含 8 字节头的 4 字节单位数），上限 16 MiB，逐连接生效；
- 测试：`server.rs::query_extension_reports_the_registry`（含 >256KiB 的
  PutImage 大请求往返）。

### XC-MISC — 完整支持
- `GetVersion` → 1.1；`GetXIDRange` → 本客户端 `(index+1)<<21` 区间，
  count 扣除已占用 ID；`GetXIDList` → 区间内未占用的 ID（XCB
  `xcb_generate_id` 依赖）。
- 测试：`extensions.rs::xc_misc_returns_the_client_id_range`。

### XTEST — 完整支持（fake input 即真实注入）
- `GetVersion` → 2.2；`FakeInput` 直接转成 `input.rs` 的注入入口：
  KeyPress/KeyRelease 走 keycode（`key_event`），ButtonPress/Release 走
  `pointer_button`（4/5 映射为滚轮 `pointer_scroll`），MotionNotify 走
  `pointer_motion`（绝对坐标）；`CompareCursor` / `GrabControl` 回 BadMatch。

### MIT-SHM — 最小支持（走 PutImage 回退）
- `QueryVersion` → 1.2 但 `shared_pixmaps=false`；`Attach` 按协议回
  BadAccess——over SSH 没有共享内存，客户端（GTK/Qt）会退回普通
  `PutImage` 路径，这是标准做法；`Detach` 忽略；shm 绘制变体
  （PutImage/GetImage/CreatePixmap/AttachFd/CreateSegment）回 BadMatch。
- 测试：`extensions.rs::shm_reports_version_but_attach_fails`。

### SHAPE — 支持核心路径
- `QueryVersion` → 1.1；`Rectangles`（Set/Union/Intersect/Subtract/Invert，
  bounding 与 clip 两种 kind，矩形列表经 `region.rs` 规范化存储）；
- 合成：`compose()` 对有 bounding shape 的窗口逐行绘制（与父窗口裁剪及
  clip shape 求交），见 `state.rs::draw_shaped_window`；
- 输入：`window_at` / `child_at` 的矩形命中之后还要过 `shape_contains`
  （bounding shape 之外的透明区域不接收输入）；
- `QueryExtents` 返回记录的包围盒；`GetRectangles` 往返回记录的矩形；
  `Combine` / `Offset` 支持；`Mask`（位图形状）与 `SelectInput`（形状变化
  通知）不支持（Mask 回 BadMatch，SelectInput 接受但无事件）。
- 测试：`extensions.rs::shape_rectangles_affect_extents_and_hit_testing`。

### XFIXES — Region 类支持，其余最小化
- `QueryVersion` → 6.0；
- **Region 类完整**：CreateRegion / SetRegion / DestroyRegion / CopyRegion /
  UnionRegion / IntersectRegion / SubtractRegion / InvertRegion /
  TranslateRegion / RegionExtents / FetchRegion（即 RegionRectangles 查询），
  内部用 `region.rs` 的逐行规范形；不存在的 region 回自定义错误 BadRegion
  （错误码 128）；
- SaveSet / SelectSelectionInput / SelectCursorInput / HideCursor /
  ShowCursor：接受但无副作用（不产生 CursorNotify 事件）；
- CursorImage / SetCursorName / ExpandRegion / SetGCClipRegion 等回
  BadMatch。
- 测试：`extensions.rs::xfixes_region_roundtrip_and_binary_ops`。

### Composite — 最小支持
- `QueryVersion` → 0.4（多数 toolkit 到此为止）；RedirectWindow /
  RedirectSubwindows / Unredirect* 校验窗口后返回成功但不改变合成行为；
  `CreateRegionFromBorderClip` 把窗口内容矩形转成 region；
  GetOverlayWindow / NameWindowPixmap 回 BadMatch。
- 测试：`extensions.rs::composite_and_damage_basic_roundtrip`。

### DAMAGE — 支持（含 DamageNotify）
- `QueryVersion` → 1.1；`DamageCreate` 记录（damage ID、drawable、
  report level、属主）并发送初始空矩形 DamageNotify；`DamageDestroy` /
  `DamageSubtract` / `DamageAdd`；
- 通知：每次绘制（`draw_with_gc`）把绑定在该 drawable 上的 damage 标脏，
  `compose()` 收尾时对脏 damage 发 DamageNotify（事件码 = first_event 94，
  `area` 为 drawable 整体，`geometry` 为屏幕位置），sequence 按接收方盖写。
  通知粒度是"每轮合成一个包围矩形"而非逐矩形增量（RAW_RECTANGLES 级别的
  精确增量区域未做）。
- 测试：`extensions.rs::composite_and_damage_basic_roundtrip`。

### RENDER — 核心子集（版本 0.11，`render.rs`）
请求语义在 `src/xsrv/render.rs`（注册表在 `extensions.rs`，opcode 140）。
像素模型沿用 drawable 的 `0x00RRGGBB` 缓冲：目标按不透明处理，源端按预乘
alpha 混合（`out = src_premul + dst*(1-a)`）。
- `QueryVersion` → 0.11；
- `QueryPictFormats` → ARGB32 / RGB24 / A8 三种直接格式（ID 1/2/3），
  screen 结构：depth 24 → RGB24 绑定根 visual，depth 32 → ARGB32
  （visualid 0，与 Xorg 的报告方式一致），fallback = RGB24；
- Picture：CreatePicture（记录 format / repeat / clip origin）、
  ChangePicture（repeat / clip origin）、SetPictureClipRectangles
  （经 `region.rs` 规范化存储，绘制时逐像素裁剪）、FreePicture、
  SetPictureFilter（Nearest/Fast/Good/Best/Bilinear 接受并忽略，真滤波
  回 BadMatch）、SetPictureTransform（接受并忽略，只支持单位变换）；
  窗口 / pixmap 销毁时其 picture 一并清理；
- `CreateSolidFill` → 纯色源 picture（含 alpha，预乘参与混合）；
- `FillRectangles` → Over（alpha 混合）/ Source（直接替换），目标像素
  `0x00RRGGBB`，窗口填充走 `mark_damage` + `compose()` 标脏；
- GlyphSet：CreateGlyphSet（A8 / ARGB32）、ReferenceGlyphSet（别名指向
  规范表）、AddGlyphs（位图每行 4 字节对齐切分，A8 / ARGB32）、
  FreeGlyphSet / FreeGlyphs；
- `Composite(Glyphs8/16/32)` → 文字主通道：解析 xGlyphElt 命令流
  （8 字节头 len/dx/dy；len=0 定位元素；len=0xff 切 glyphset；字形紧密
  排列补齐 4 字节），笔画从 (0,0) 起步，字形画在 pen-(x,y)、按 (x_off,
  y_off) 推进；A8 字形提供 alpha（颜色取源 picture），ARGB32 字形自带
  预乘色；缺失字形回 BadGlyph；
- `Composite` → Over / Source / Atop / In（目标不透明时 Atop=Over、
  In=预乘源色），src/mask/dst 取三者交集，mask 只取 alpha 通道，
  repeat=true 时源越界按尺寸回卷；
- `Trapezoids`（左右边插值逐行扫描）/ `Triangles`（复用 `raster.rs`
  多边形扫描）/ `TriStrip` / `TriFan`（拆三角形）→ span 栅格化 + 按 op
  混合，源为 solid 或 picture；
- `QueryPictIndexValues` → 只报 direct 格式，按规范回 BadMatch；
  `QueryFilters` → 回空列表（toolkit 走默认滤波）。
- 测试：`render.rs`（QueryPictFormats 含 ARGB32、FillRectangles 的 Over
  像素断言、solid→窗口 Composite 断言、Glyphs 混合断言、形状栅格化、
  clip 与生命周期、blend 原语、glyphcmds 解析）。

### Gradients（RENDER）— 不支持
Linear / Radial / Conical 渐变与 AddTraps、RENDER CreateCursor、
CreateAnimCursor 回 BadImplementation（toolkit 有回退路径）。

## 已知取舍

- DamageNotify 只上报 drawable 整体矩形，不维护逐矩形增量区域；
- `ShapeMask`（1 位位图形状）回 BadMatch，圆角窗口常用的 Rectangles 路径
  不受影响；
- Composite 不做重定向合成，Overlay 窗口不可用；
- MIT-SHM 依赖 fd 传递的变体（AttachFd/CreateSegment）直接拒绝，不尝试
  解析 fd；
- RENDER：transform 只支持单位变换（非单位矩阵被忽略，会画错位置）；
  repeat 是最近邻整数回卷（无平滑滤波）；picture 的 alphamap / clipmask
  pixmap / polyedge / polymode / dither / componentalpha 接受并忽略；
  A1 字形格式、Indexed 格式、Gradients、AddTraps 不支持（明确报错）；
  Composite 只支持 Over / Source / Atop / In（其余 op 回 BadPictOp）。
