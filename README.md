# RustTerm GPUI

**纯 Rust 全功能终端客户端** — 把本地 / SSH / Telnet 终端、SFTP、VNC / RDP / SPICE 远程桌面、内置浏览器与 AI 助手收进同一个窗口。

> English version: [README_EN.md](README_EN.md)

- UI 框架：**GPUI Kit 0.8**（Zed 的 gpui + longbridge gpui-component，GPU 加速、声明式元素树每帧重建），取代早先的 `rustterm-egui`（eframe/egui）原型。
- 目标：验证「纯 Rust 桌面客户端」能否整体替换主工程的 Tauri + xterm.js 前端。当前只在 **Windows** 上构建与验证：ConPTY、WebView2、GDI 截屏、DirectWrite 字体栈都按 Windows 实现，非 Windows 平台只在少数模块留了 `cfg` 兜底分支，未经验证。
- 许可：MIT（见 [LICENSE](LICENSE)）。

## 技术栈

| 领域 | 选型 |
|---|---|
| UI 框架 | gpui-kit 0.8（gpui + gpui-base + gpui-component + Lucide 图标资源） |
| 终端内核 | alacritty_terminal 0.26（VT 状态机）+ vte 0.15 |
| 本地 PTY | portable-pty（Windows 走 ConPTY） |
| SSH | vendor russh（与主工程同源的 0.62 fork，含旧算法兜底）+ russh-sftp |
| 远程桌面 | vendor vnc-rs（VNC）/ vendor IronRDP（RDP）/ vendor capsaicin（SPICE） |
| 浏览器 | wry 0.47（WebView2），`browser` feature，默认开启 |
| AI | ureq（阻塞 HTTP 放后台线程）：OpenAI 兼容 / Ollama / Anthropic |
| 其他 | tokio、serde / serde_json、image、rfd、base64、uuid、chrono、windows-sys |

---

## 功能特性

### 终端与会话

- **本地终端**（portable-pty + ConPTY）、**SSH 终端**（密码 / 私钥 + passphrase）、**Telnet**（原始 TCP + 最小 IAC 协商）、**Ping**（PTY 里跑 `ping -t`，复用终端渲染与交互）
- **ProxyJump 跳板机**：真实 `direct-tcpip` 通道（不是本地端口转发），跳板机 Handle 一并保活；`user@host:port` 解析，凭据沿用目标机认证方式
- **SSH 主机密钥**：accept-new（TOFU）写入 `~/.ssh/known_hosts`，密钥变更一律拒绝；取不到主目录时退化为全放行（与 P0 行为一致）
- **认证**：password 失败自动 fallback 到 keyboard-interactive（PAM 场景）；算法协商在默认安全算法之后追加 SHA-1 / CBC / des 兜底，兼容 Jumpserver 与老设备
- **Keep-Alive**：配置面板里的间隔（秒，0 = 关闭）真实生效 —— 会话任务每秒读一次全局间隔，只在**确实空闲**够久时发 `keepalive@openssh.com`（want_reply 顺带探活）；改间隔不用重连
- **会话管理**：`sessions.json` 持久化 SSH / VNC / RDP / SPICE 四类会话（主机、端口、用户、密码、私钥路径、域、**RDP 共享目录**），侧栏列表支持单击选中、**双击连接**、行内连接 / 编辑 / 删除、搜索过滤、最近会话（8 条）、首次运行写入示例会话
- **快速连接栏**：地址 | 跳板机 | 密码 / 密钥 | 凭据 | 连接
- **统一标签栏**：终端 / 浏览器 / 远程桌面 / 本地文件 / 键宏 / 端口扫描平级共存，点击切换 + 活动高亮 + 关闭按钮；**拖拽标签可重排**（拖动中源标签半透明，松开落在哪一格就插到哪一格），**双击标签就地重命名**（回车或失焦提交，清空则恢复默认标题）
- **分屏（dock）**：`TermPane` 提升为 dock 面板实体（`impl Panel`），由 `gpui-base` 的 `DockArea` + `gpui-component` 的 `DockSkin` 承载 —— 任意嵌套分屏、拖到边缘分裂、标签组合并/拆分、单格缩放、整树 dump/load（重启还原布局）都交给 dock。`Ctrl+Shift+D` 向右、`Ctrl+Shift+U` 向下仍可用，新窗格**复制当前会话的启动配置**（SSH 会另起一条连接，不共享通道）；每格各自测量尺寸、各自一个 pty、各自一份选区与查找状态
- **恢复上次会话**：空标签页的欢迎页上有按钮，按 `last_tabs.json` 重开上次退出时的标签（本地 / 命令 / Telnet / SSH / 浏览器 / 远程桌面 / 本地文件）；**快照不含密码**，SSH 与远程桌面按会话表取凭据，取不到就跳过并提示
- **批量执行**（同一条命令发到所有终端）、**同步输入**（活动终端按键镜像到其它终端）、**查找**（Ctrl+F 高亮 + 上/下跳转）、**键宏**（手动 / 快捷键 / 连接时 / 断开时四种触发）
- **终端绘制**：每帧生成 `TermSnapshot`（可见行文本 + 颜色 run + 选区 / 查找 / 光标覆盖层），由 canvas 绘制；10k 行 scrollback、选区复制（Ctrl+C 智能复制）、bracketed paste、鼠标上报（click / drag / motion）、OSC 52 写系统剪贴板（读请求回空串，避免远端程序挂起）
- 终端设备查询（DA / 颜色请求等）由 `PtyProxy` 应答回写，不外泄到日志

### SFTP

- 右侧**停靠面板**，绑定当前活动 SSH 会话，复用已认证连接（不重新建链、不重复认证，跳板机场景同样成立）
- 远端目录浏览（路径栏 + 家目录）、上传 / 下载（分块 + 进度 + 可取消）、重命名 / 新建目录 / 删除、隐藏文件过滤（后端标记、前端过滤，切换不重新列目录）
- **拖拽上传**：文件直接上传，目录先建远端目录再逐个上传内部文件（单次拖拽数量有上限保护）
- 上传 / 删除 / 新建 / 重命名完成后**自动刷新**列表

### 远程桌面（VNC / RDP / SPICE）

- 三协议真实实现，统一接口：下行 `RdCommand`（键盘 / 指针 / 滚轮 / 关闭），上行 `RdEvent`（状态 / 分辨率 / RGBA8 脏矩形帧 / 错误）
- 帧缓冲独立成模块（`framebuf.rs`，不依赖 gpui、可离线回归）：脏矩形合并 → RGBA 主缓冲 → BGRA → `RenderImage` 纹理上传（canvas 绘制）
- RDP 走 CredSSP / NLA（用户名必填，缺了会很难懂地超时）、VNC 走 Auto 安全策略（覆盖带凭据 / 无凭据）、SPICE 把主 surface 作为桌面输出、子 surface 单独维护缓冲（合成要等 capsaicin 解析 draw 通道）
- 错误经 `friendly_error()` 翻译成可操作提示（缺密码 / 缺用户名 / 分辨率异常等）
- `fit` / `zoom` 缩放、键盘捕获开关、远端光标贴图（带缓存）
- **剪贴板双向同步**：RDP 走自定义 CLIPRDR 后端（文本 + 文件，见下）、VNC 走 `ServerCutText` / `ClientCutText`（Latin-1）、SPICE 暂无剪贴板通道
- **CapsLock 状态同步**：RFB / RDP 都没有远端 LED 回传，客户端只在本地锁定状态变化时补发一次 `Caps_Lock`，让远端跟上（否则会出现「本地大写、远端小写」）
- **传文件**：RDP 有两条通道，按有没有配共享目录二选一 —— 配了共享目录就走 **RDPDR 驱动器重定向**：连接表单（或会话对话框）里指定一个本机目录（整盘 `D:\` 或某个文件夹 `D:\rustterm` 都行，**只暴露这个目录**），**随会话存进 `sessions.json`**，连上后远端「我的电脑」里多一个 `RustTerm` 盘符，直接拖拽即可（大文件、目录、批量都走这条通道）；**没配共享目录就走剪贴板**：本机复制文件 → 远端粘贴，远端复制 → 本机缓存目录并写回剪贴板（`src/remote/rdp_clipboard.rs`，进度显示在标签页工具条右侧）。VNC / SPICE 协议本身没有文件通道（RFB 只有剪贴板），需要时用右侧 SFTP 面板
- 键映射：PC/AT set-1 扫描码 + X11 keysym，Shift 下的可打印字符单独换算，无映射键走 Unicode 通道
- 从已保存的远程桌面会话打开即自动连接

### 内置浏览器

- wry / WebView2 **原生子窗口**，每个浏览器标签独占一个 WebView，且跑在**专属线程**上（自带消息泵，避免 gpui 渲染重入）
- 工具栏：后退 / 前进 / 刷新 / 地址栏 / 书签栏 / 历史记录；页面标题与 URL 经回调同步到地址栏；历史上限 1000 条
- 与 AI 面板并排时把 WebView 右缘钳制到面板左缘；非活动标签自动隐藏，不会盖在终端上
- WebView2 未安装 / 初始化失败时显示可读错误，而不是黑屏或崩溃

### AI 助手

- 三提供方：**OpenAI 兼容**（含中转网关 / GLM / Kilo）、**本地 Ollama**、**Anthropic**；提供方、API Key、Base URL、模型、max_tokens、temperature、只读模式、历史保留条数、上下文行数与单行截断长度全部可配并持久化
- **自动附带终端上下文**：发送时拼接【终端信息】头（SSH user@host / Telnet / 本地）+ 最近 N 行终端输出（单行超长截断）
- **附件**：📷 主窗口截图（GDI，`PrintWindow` 优先 / `BitBlt` 兜底，压到最长边 1280，最多 3 张）、📎 文件（PNG 直接内联；JPEG 转 PNG；文本按代码块内联并截断；其它二进制拒绝），截图 + 文件总数上限 6
- 回复中的 `bash` 代码块会自动提取出命令并附「发送到终端」按钮；危险命令拦截、只读模式禁写、Agent 模式切换系统提示词

### 运维工具

- **端口扫描**：IP 范围 / CIDR + 端口列表（支持范围段）多线程扫描，并发数 = 同时打开的连接数，可停止；单次 IP×端口上限 65536
- **隧道**：本地 TCP 监听 → 目标主机端口转发（纯 `std::net`，不依赖 SSH 会话），可启停、查看连接数与错误
- **键宏管理器**：列表（启用开关 / 执行 / 编辑 / 复制 / 删除）+ 编辑器（名称 / 描述 / 启用 + 动作行：类型徽标 + 值输入 + 上移 / 下移 / 删除）
- **本地文件浏览器**：主标签页宽栏浏览本地文件系统

### 界面 / 本地化 / 配置

- 中英文**实时切换**（菜单、工具栏、侧栏、欢迎页、状态栏、快速连接栏；静态词条表，未知 key 原样返回），深色 / 浅色主题（gpui-component Theme）
- 布局：菜单栏 + 工具栏 + 快速连接栏 + 左侧栏（会话 / 工具 / 宏）+ 中央标签区 + 右侧 SFTP / AI 面板 + 底部状态栏 + 覆盖层对话框
- 状态栏：连接状态、标签类型、标签数、会话数、终端尺寸（cols×rows）、toast 提示
- 密码框统一带「查看密码」眼睛按钮；窗口尺寸按工作区自适应（`SPI_GETWORKAREA` + DPI），最小 900×560
- 配置持久化到 `%APPDATA%\rustterm-gpui\config.json`，首次运行自动迁移旧 egui 目录配置；主路径写失败回退到 exe 旁 `data\`
- 崩溃日志写到 `%APPDATA%\rustterm-gpui\crash.log`（GUI 子系统下 panic 也能留痕）

---

## 快捷键

| 快捷键 | 作用 |
|---|---|
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | 下一个 / 上一个标签 |
| `Ctrl+1` … `Ctrl+9` | 跳到第 N 个标签 |
| `Ctrl+Shift+←` / `Ctrl+Shift+→` | 当前标签左移 / 右移 |
| `Ctrl+N` | 新建本地终端 |
| `Ctrl+T` | 新建浏览器标签 |
| `Ctrl+Shift+S` | 显示 / 隐藏 SFTP 面板 |
| `Ctrl+Shift+E` | 本地文件浏览器 |
| `Ctrl+Shift+R` | 远程桌面 |
| `Ctrl+Shift+B` | 内置浏览器 |
| `Ctrl+Shift+A` | AI 助手 |
| `Ctrl+W` | 关闭当前标签 |
| `Ctrl+Shift+D` | 当前窗格向右分屏 |
| `Ctrl+Shift+U` | 当前窗格向下分屏 |
| `Ctrl+Shift+P` | 切换到下一个窗格 |
| `Ctrl+Shift+W` | 关闭当前窗格（只剩一格时不动作） |
| `Ctrl+F` | 终端查找（仅终端标签可用） |
| `Ctrl+L` | 聚焦浏览器地址栏 |
| `F11` | 全屏 |

键宏还可以绑定自定义快捷键（`Ctrl+M` 这类组合），命中时按键被宏吃掉并改为执行宏。

---

## 构建 / 运行

```powershell
cargo run --release          # 运行（推荐）
cargo build --release        # 产出可执行文件
cargo test                   # 纯逻辑单测
```

`cargo test` 覆盖的是不依赖窗口的部分：i18n 词条与语言回退、键宏快捷键归一化与触发类型往返、Telnet IAC 协商与 NAWS 应答、端口扫描解析、SFTP 路径工具与大小格式化（共 22 个用例，用的是标准 `#[test]`；含会话存盘的向后兼容：老 `sessions.json` 缺 `share_dir` 仍能解析）。

- 默认启用 `browser`（wry / WebView2）。不需要内置浏览器时：`cargo build --release --no-default-features`
- 开发工具二进制不在默认构建里，需要显式开 `devtools` feature：

```powershell
cargo run --features devtools --bin snap     -- shot.png [window]     # 截图（全屏 / 主窗口客户区）
cargo run --features devtools --bin ui       -- click 400 300         # 驱动鼠标键盘（需窗口前台）
cargo run --features devtools --bin post     -- postclick 400 300     # PostMessage 版（无需前台）
cargo run --features devtools --bin rdprobe                           # 帧缓冲回归 + 三协议失败路径
cargo run --features devtools --bin vncprobe -- <host> <port> [password]
cargo run --features devtools --bin rdpprobe -- <host> <port> [user] [password] [domain]
cargo run --features devtools --bin rfbhello -- <host> <port>
cargo run --features devtools --bin ptytest  -- ping 127.0.0.1 -n 2
```

构建注意事项：

- **MSVC 工具链**：开发机上的 `check-msvc.bat` / `run-msvc.bat` 负责切 `vcvars64` 与 nasm。⚠️ 两个脚本里的工程路径仍指向旧的 egui 目录，使用前要先改成当前目录
- `profile.dev.debug = 0`：MSVC 下 `debug=true` 会让 cc 传 `/Z7`，随 Rust spawn 触发 cl D8050，因此 dev 档关闭调试信息（依赖仍按 `opt-level = 2` 编译）
- 应用图标由 `build.rs` 通过 winresource 嵌入 `icons/icon.ico`（仅 Windows）
- `russh` 关闭 default-features（避开 aws-lc-rs 的 C 构建），只保留 ring 后端 + 手动开启的特性；`[patch.crates-io]` 把 `picky` / `sspi` 指向 vendor 修补版，与 russh 的 ecdsa 版本对齐

---

## 架构

```text
src/
  main.rs         入口：32MB 栈线程 + 崩溃日志 + 关闭 DComp + 窗口几何 + 图标资源注册
  app.rs          外壳：菜单 / 工具栏 / 快速连接 / 侧栏 / 标签栏 / 中央区 / 状态栏 / 覆盖层对话框
  term.rs         终端核心：Term 状态机、TermSnapshot 生成与绘制、键鼠 → VT 编码、选区 / 粘贴 / 查找
  split.rs        分屏布局树：叶子 / 分割节点 + 稳定节点 id（比例靠它挂在 ResizableState 上）
  tabs.rs         标签模型 + 终端标签页（多个窗格 + 分屏树 + 活动窗格 id）
  config.rs       AppConfig + AiSettings（serde 默认值迁移 + 回退落盘）
  sessions.rs     会话模型 + sessions.json（最近会话、首次运行示例）
  backends/
    mod.rs        后端统一 IO：output_rx / input_tx / resize_tx
    ssh.rs        russh：认证、ProxyJump、known_hosts 校验、SshHandle::open_sftp
    local.rs      portable-pty 本地 shell / 任意命令
    tcp.rs        原始 TCP（Telnet 最小 IAC 协商）
  remote/
    mod.rs        VNC / RDP / SPICE 统一 RdCommand / RdEvent + friendly_error + 键映射
    vnc.rs        VNC（vendor vnc-rs，强制 BGRA 像素格式）
    rdp.rs        RDP（vendor IronRDP，CredSSP + TLS + 接受任意证书）
    spice.rs      SPICE（vendor capsaicin，主 surface + 光标通道 + 子 surface 缓冲）
  framebuf.rs     远程桌面帧缓冲：脏矩形 → RGBA 主缓冲 → BGRA（纯逻辑，可离线回归）
  rdp.rs          远程桌面标签页：等比缩放绘制、键盘捕获、远端光标、控制项
  sftp.rs         右侧 SFTP 停靠面板（拖拽上传、进度、行内操作）
  sftp_client.rs  SFTP 封装：列表 / 传输 / 取消 / 进度 + 路径工具
  filebrowser.rs  本地文件浏览器标签页
  browser.rs      WebView2 子窗口（专属线程 + 消息泵 + 事件回传）
  ai.rs           多提供方请求构造 / 响应解析 / 命令提取 / 危险与写命令判定
  ai_assistant.rs AI 对话面板（气泡 / 附件 / 终端上下文 / Agent 开关）
  ai_config_panel.rs  AI 配置面板
  config_panel.rs     通用配置面板
  screenshot.rs   GDI 抓主窗口客户区（PrintWindow → BitBlt 兜底）
  tunnel.rs       本地 TCP 隧道中转
  portscan.rs     端口扫描（worker 池限流 + 停止标志）
  macros.rs       键宏模型 / 编码 / 触发与快捷键派发
  i18n.rs         中英文静态词条表
  ui_icons.rs     统一图标（绕过组件层 Icon 只认默认子集的限制）
  widgets.rs      通用部件（带眼睛按钮的密码框）
  win32.rs        主窗口 HWND 注册表（浏览器子窗口 / 截图 / 拖拽落点共用）
```

**数据流**

- 终端：`pty/ssh → mpsc → vte Processor.advance(Term) → renderable_content() → TermSnapshot → canvas`
- 终端输入：`GPUI 事件 → VT 序列 → mpsc → pty/ssh`（终端查询响应经 `PtyProxy` 回写）
- 分屏尺寸：`canvas prepaint 记录本格 bounds → pane_areas → 下一帧 fit_to(该格)`，
  每格独立算列数/行数；鼠标先按 `pane_areas` 命中判定，再换算列/行
- 远程桌面：`后端线程 → RdEvent{Frame 脏矩形} → FrameBuffer → BGRA → RenderImage → canvas`
- 浏览器：`gpui 主线程 ⇄ channel ⇄ WebView 专属线程（自带消息泵）⇄ WebView2`

---

## 开发阶段

阶段划分沿用 `main.rs` 的模块文档：

| 阶段 | 内容 | 状态 |
|---|---|---|
| P1 | 产品骨架：统一标签页 / SFTP 双栏 / 状态栏 / 配置面板 / i18n | 已落地 |
| P2 | 远程桌面：VNC / RDP / SPICE 帧渲染，后端零改动 | 已落地 |
| P3 | 兼容层：wry 浏览器标签 + AI 助手多提供方 + AI 截屏分析 | 已落地 |
| P4 | 打磨：键宏 / keep-alive / 装配打包 / 性能与内存调优 | 进行中（键宏、keep-alive、行级 shaping 缓存已完成；打包未做） |

## 工程要点（GPUI / wry 踩坑）

1. **关闭 DirectComposition**：gpui 默认用 `CreateSwapChainForComposition` 渲染，而 DComp 视觉树不包含普通 HWND 子窗口 —— 内置浏览器的 WebView2 会被整块盖住（页面其实已经加载成功）。`main.rs` 默认设 `GPUI_DISABLE_DIRECT_COMPOSITION=true` 改走 `CreateSwapChainForHwnd`；用户显式设置则以用户为准。
2. **WebView 必须跑在专属线程**：wry 创建 WebView2 时会调用 `wait_with_pump` 在**本线程**泵消息，若发生在 gpui 渲染帧里，会把 gpui 自己的 `WM_PAINT` 一并派发出去造成渲染重入，帧调度器二次借用 `AppCell` 直接 panic（表现为「点一下浏览器就崩」）。因此创建与全部调用都放到一个专属线程，主线程只通过 channel 收发。
3. **大栈线程**：GPUI 递归布局 / 递归绘制，加上本工程很深的元素树，Windows 默认 1MB 主线程栈会偶发溢出；栈溢出不走 panic hook，日志什么都留不下。整个应用因此跑在 32MB 栈的线程上（Zed / gpui 应用的标准做法）。
4. **图标资源**：必须在 `gpui_kit::application().with_assets(AllAssets)` 注册完整图标集，否则组件里的图标全部渲染为空白；组件层 `Icon` 只接受默认子集，用 `Icon::default().path(...)` 绕过（见 `ui_icons.rs`）。
5. **测试宏冲突**：`use gpui_kit::*` 会把 `gpui_kit::test` 模块带进作用域，测试模块里不要 `use super::*`，否则 `#[test]` 会解析到它并无限递归展开。
6. **终端原点只算一次**：canvas 的 prepaint 把「文本网格」原点写进全局 `TermArea`，绘制与鼠标列 / 行换算共用同一原点，避免两处各算一套导致选中错位。
7. **原生子窗口恒在 gpui 之上**：任何浮层（对话框 / 菜单 / 右键菜单）打开时都要隐藏或钳制 WebView 区域，否则会被盖住。
8. **对话框 = 同帧覆盖层**：egui 的独立视口窗口在 GPUI 里改为绝对定位覆盖层，不参与主布局；`overlay_open()` 统一判定「当前是否有浮层」，供 WebView 隐藏与快捷键屏蔽使用。
9. **dock 面板的两处约束**：
   - **尺寸只能取自己上一帧的 bounds**：分屏后每格大小不同，用窗口视口去估算列数/行数会让第二格按整屏算。每格在 canvas prepaint 里把自己的 bounds 存下来，下一帧据此 `fit_to`；鼠标列/行换算也用同一份 bounds；
   - **面板实体创建需要 `&mut Window`**：而「新建终端」常从菜单 / 快捷键发起（那时没有 window），所以新面板先以 `PendingPane`（意图）排进工作区，首帧渲染时再变成 `TerminalView` 实体装进 dock。

10. **`div()` 默认是 block，不是 flex**（`Style::default().display = Display::Block`）：只有 `.flex()` / `.h_flex()` / `.v_flex()` 才是 flex 容器。中间层漏了这一步，子元素的 `flex_1` 直接不生效，而 `height:100%` 又解析到 auto 高度上 —— 高度塌成 0，**终端 canvas 一片空白的真实原因**。分屏尤其容易踩：每多包一层容器就要确认它是 flex；叶子里的 canvas 用 `flex_1` 拿高度，不要依赖百分比。

## 已知缺口

- **Keep-Alive 只覆盖 SSH**：`config.keep_alive_interval`（0 = 关闭）已经是真实现 —— SSH 会话任务每秒醒一次读全局间隔，攒够且这段时间确实没数据才发一个 `keepalive@openssh.com`（want_reply，顺带探活），改配置不用重连。Telnet / 远程桌面没有心跳：Telnet 的 `IAC NOP` 只在少数老设备上有意义，RFB 本身没有 idle 超时概念
- **X11 转发未实现**：`menu.x11` / `item.x11_forward` 词条在表里，菜单与后端都没接
- **RDP 剪贴板传文件**：走 CLIPRDR 的 `FileGroupDescriptorW` + `FileContentsRequest`（延迟拉取），实现在 `src/remote/rdp_clipboard.rs`（参考 rustterm-tauri 的同名实现）。**只在本会话没配共享目录时启用**，配了共享目录则文件统一走 RDPDR 盘符，避免远端同时看到盘符和剪贴板文件、粘贴时撞锁。远端→本机的文件落在 `%LOCALAPPDATA%\rustterm-gpui\cache\rdp-clipboard\<uuid>`，全部下完才写回剪贴板（写太早资源管理器会拿到半个文件），旧缓存 24 小时后自动清理
- **VNC / SPICE 没有文件通道**：RFB 标准协议没有文件传输（只有剪贴板），SPICE 的文件通道（`port`/`usbredir`）capsaicin 也未暴露。需要时用右侧 SFTP
- **SPICE 子 surface 不合成**：子 surface 的像素已落到独立缓冲，但「贴到主面哪个位置」在 draw 通道的 `DRAW_COPY_BITS` 里，capsaicin 目前归到 `UnhandledDraw`
- **终端性能**：已做行级 shaping 缓存（key = 文本 + 样式 + 字号 + 字体族，命中直接复用 `ShapedLine`）；整屏快照仍每帧重建（`renderable_content` 遍历全可见区），后续可考虑按行版本号增量生成
- **恢复上次会话的边界**：`last_tabs.json` 只记「哪些标签开着」，分屏内部结构与标签自定义名不恢复（那是 dock 自己的 dump/load 该管的）；快照**不含密码**，SSH 恢复时按 `host/port/user` 回 `sessions.json` 取凭据，取不到（临时连的、没存密码的）就跳过并在提示里报个数，不会拿空密码去撞
- **打包**：没有安装包或分发脚本
- **残留物**：`src/status.rs` 已废弃（状态栏并入 `app.rs`）；`check-msvc.bat` / `run-msvc.bat` 仍指向旧的 egui 目录

## 与主工程关系

- 独立目录（`rustterm-gpui`），不改动主工程任何文件。共享的核心是 **vendor 修补版依赖**：`vendor/russh`（旧算法兜底，用法与主工程 `src-tauri/src/ssh.rs` 一致）、`vendor/picky` 与 `vendor/sspi`（经 `[patch.crates-io]`），以及 `vendor/ironrdp`、`vendor/vnc-rs`、`vendor/capsaicin` 三套远程桌面实现
- SSH 算法协商与主机密钥策略、端口扫描的并发模型均对齐主工程（`src-tauri/src/ssh.rs`、`src-tauri/src/portscan.rs`）
- 若本轮验证达标（渲染性能 / CJK / 键鼠输入 / SFTP / 远程桌面 / 浏览器 / AI），可把 `term.rs` 的绘制与输入编码层迁回主工程，替换 xterm.js 前端
