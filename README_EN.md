# RustTerm GPUI

**A pure-Rust, all-in-one terminal client** — local / SSH / Telnet terminals, SFTP, VNC / RDP / SPICE remote desktop, an embedded browser and an AI assistant in a single window.

> 中文版：[README.md](README.md)

- UI framework: **GPUI Kit 0.8** (Zed's gpui plus longbridge's gpui-component — GPU-accelerated, declarative element tree rebuilt every frame), replacing the earlier `rustterm-egui` (eframe/egui) prototype.
- Goal: prove that a pure-Rust desktop client can replace the main project's Tauri + xterm.js frontend. It is built and verified on **Windows only** for now: ConPTY, WebView2, GDI capture and the DirectWrite font stack are all Windows implementations, and a few modules carry `cfg` fallbacks for other platforms that have not been verified.
- License: MIT (see [LICENSE](LICENSE)).

## Stack

| Area | Choice |
|---|---|
| UI framework | gpui-kit 0.8 (gpui + gpui-base + gpui-component + Lucide icon assets) |
| Terminal core | alacritty_terminal 0.26 (VT state machine) + vte 0.15 |
| Local PTY | portable-pty (ConPTY on Windows) |
| SSH | vendored russh (the same 0.62 fork as the main project, with legacy-algorithm fallbacks) + russh-sftp |
| Remote desktop | vendored vnc-rs (VNC) / vendored IronRDP (RDP) / vendored capsaicin (SPICE) |
| Browser | wry 0.47 (WebView2), `browser` feature, on by default |
| AI | ureq (blocking HTTP on worker threads): OpenAI-compatible / Ollama / Anthropic |
| Other | tokio, serde / serde_json, image, rfd, base64, uuid, chrono, windows-sys |

---

## Features

### Terminal & Sessions

- **Local terminal** (portable-pty + ConPTY), **SSH terminal** (password / private key + passphrase), **Telnet** (raw TCP + minimal IAC negotiation), **Ping** (runs `ping -t` in a PTY so it reuses the full terminal renderer)
- **ProxyJump**: a real `direct-tcpip` tunnel (not local port forwarding), with the jump host's handle kept alive; `user@host:port` parsing, credentials follow the target host's auth method
- **SSH host keys**: accept-new (TOFU) written into `~/.ssh/known_hosts`; a changed key is always refused. Falls back to accept-all when the home directory is unavailable (same as the P0 behaviour)
- **Auth**: password auth falls back to keyboard-interactive (PAM setups); the algorithm list appends SHA-1 / CBC / des after the secure defaults, so Jumpserver and older devices work
- **Session management**: `sessions.json` persistence for SSH / VNC / RDP / SPICE sessions (host, port, user, password, key path, domain); the sidebar list supports click-to-select, **double-click to connect**, inline connect / edit / delete, search filtering, 8 recent sessions and seeded examples on first run
- **Quick-connect bar**: address | jump host | password / key | credentials | connect
- **Unified tab strip**: terminals, browser, remote desktop, local files, macros and port scanning live side by side with click-to-switch, active highlight and a close button (**no drag reordering or double-click rename yet**)
- **Batch execution** (one command to every terminal), **synchronized input** (mirror the active terminal's keystrokes to the others), **find** (Ctrl+F highlight with next/previous), **key macros** (manual / hotkey / on-connect / on-disconnect triggers)
- **Terminal rendering**: each frame builds a `TermSnapshot` (visible rows with text + colour runs, plus selection / search / cursor overlays) that a canvas paints; 10k-line scrollback, selection copy (Ctrl+C smart copy), bracketed paste, mouse reporting (click / drag / motion), OSC 52 clipboard *writes* (read requests answer with an empty string so remote programs never hang)
- Terminal device queries (DA, colour requests, …) are answered through `PtyProxy` instead of leaking into the log

### SFTP

- A **docked panel** on the right that binds to the active SSH session and reuses its authenticated connection — no re-handshake, no re-auth, and it still works through jump hosts
- Remote browsing (path bar + home button), upload / download (chunked, with progress and cancel), rename / mkdir / delete, hidden-file filtering (flagged by the backend, filtered by the UI, so toggling never re-lists)
- **Drag-and-drop upload**: files go straight up; directories are created remotely first and their contents uploaded one by one (with a per-drop count limit)
- The list **auto-refreshes** after upload / delete / mkdir / rename

### Remote Desktop (VNC / RDP / SPICE)

- All three protocols genuinely implemented behind one interface: down `RdCommand` (keyboard / pointer / wheel / close), up `RdEvent` (state / resolution / RGBA8 dirty-rect frames / errors)
- The frame buffer is its own gpui-free module (`framebuf.rs`, offline-regressable): dirty rects merge into an RGBA master buffer → converted to BGRA → uploaded as a `RenderImage` texture and painted by a canvas
- RDP goes through CredSSP / NLA (a username is mandatory — without it the failure is an opaque timeout), VNC uses the Auto security policy (credentials first, then a credential-less retry), SPICE composites the primary surface only
- Raw errors are translated by `friendly_error()` into actionable hints (missing password / missing username / odd resolution, …)
- `fit` / `zoom` scaling, a keyboard-capture toggle, a cached remote-cursor sprite, and a clipboard-sync toggle (UI only — the backend is not wired up)
- Key mapping: PC/AT set-1 scan codes + X11 keysyms, with Shift-resolved printable characters; unmapped keys fall back to a Unicode channel
- Opening a saved remote-desktop session connects automatically

### Embedded Browser

- A wry / WebView2 **native child window**, one WebView per browser tab, living on a **dedicated thread** with its own message pump (which keeps gpui's rendering from re-entering)
- Toolbar: back / forward / reload / address bar / bookmarks / history; page title and URL callbacks keep the address bar in sync; up to 1000 history entries
- When the AI panel is open the WebView's right edge is clamped to the panel's left edge; inactive tabs are hidden so they never cover a terminal
- If WebView2 is missing or fails to initialise, a readable error is shown instead of a black rectangle or a crash

### AI Assistant

- Three providers: **OpenAI-compatible** (relays / GLM / Kilo), **local Ollama** and **Anthropic**; provider, API key, base URL, model, max_tokens, temperature, read-only mode, history limit, context line count and per-line truncation are all configurable and persisted
- **Automatic terminal context**: every send prepends a terminal-info header (SSH user@host / Telnet / local) plus the last N lines of output (over-long lines truncated)
- **Attachments**: 📷 main-window screenshots (GDI; `PrintWindow` first, `BitBlt` fallback; downscaled to 1280 px on the long edge; at most 3), 📎 files (PNG inlined directly, JPEG converted to PNG, text inlined as a fenced block and truncated, other binaries rejected); screenshots + files capped at 6
- Commands are extracted from `bash` code blocks and offered as a "send to terminal" action; dangerous commands are blocked, read-only mode blocks writes, and Agent mode switches the system prompt

### Operations Tools

- **Port scanner**: IP ranges / CIDR + port lists (including ranges) scanned by a worker pool where concurrency equals the number of open connections; cancellable, with a hard limit of 65536 IP×port probes per run
- **Tunnel**: a local TCP listener forwarding to a target host/port (plain `std::net`, independent of any SSH session) with start/stop, connection counts and errors
- **Macro manager**: list (enable toggle / run / edit / duplicate / delete) plus an editor (name / description / enabled, and action rows with a type badge, value input and move-up / move-down / delete)
- **Local file browser**: a wide main tab for the local filesystem

### UI / Localization / Configuration

- Live **Chinese ⇄ English** switching (menus, toolbar, sidebar, welcome page, status bar, quick-connect bar — a static string table whose unknown keys fall through verbatim) and dark / light themes (gpui-component Theme)
- Layout: menu bar + toolbar + quick-connect bar + left sidebar (sessions / tools / macros) + central tab area + right SFTP / AI panels + status bar + overlay dialogs
- Status bar: connection state, tab kind, tab count, session count, terminal size (cols×rows) and toast messages
- Every password field has a "reveal" eye button; the window sizes itself to the work area (`SPI_GETWORKAREA` + DPI) with a 900×560 minimum
- Configuration persists to `%APPDATA%\rustterm-gpui\config.json`, migrating the old egui directory on first run; if the primary path is not writable it falls back to `data\` next to the executable
- Crashes are appended to `%APPDATA%\rustterm-gpui\crash.log`, so panics still leave a trace in a GUI-subsystem build

---

## Keyboard shortcuts

| Shortcut | Action |
|---|---|
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |
| `Ctrl+1` … `Ctrl+9` | Jump to tab N |
| `Ctrl+Shift+←` / `Ctrl+Shift+→` | Move the current tab left / right |
| `Ctrl+N` | New local terminal |
| `Ctrl+T` | New browser tab |
| `Ctrl+Shift+S` | Toggle the SFTP panel |
| `Ctrl+Shift+E` | Local file browser |
| `Ctrl+Shift+R` | Remote desktop |
| `Ctrl+Shift+B` | Embedded browser |
| `Ctrl+Shift+A` | AI assistant |
| `Ctrl+W` | Close the current tab |
| `Ctrl+F` | Find in terminal (terminal tabs only) |
| `Ctrl+L` | Focus the browser address bar |
| `F11` | Fullscreen |

Macros can bind their own shortcuts (e.g. `Ctrl+M`); when one matches, the keystroke is consumed and the macro runs instead.

---

## Build / Run

```powershell
cargo run --release          # run (recommended)
cargo build --release        # produce the executable
cargo test                   # pure-logic unit tests
```

`cargo test` covers everything that needs no window: i18n strings and language fallback, macro hotkey normalisation and trigger round-trips, Telnet IAC negotiation, port-scan parsing and SFTP path utilities (17 cases in total, written with plain `#[test]`).

- `browser` (wry / WebView2) is on by default. To build without the embedded browser: `cargo build --release --no-default-features`
- The devtools binaries are not part of the default build; enable the `devtools` feature explicitly:

```powershell
cargo run --features devtools --bin snap     -- shot.png [window]     # capture full screen / main window client area
cargo run --features devtools --bin ui       -- click 400 300         # drive mouse/keyboard (window must be foreground)
cargo run --features devtools --bin post     -- postclick 400 300     # PostMessage variant (no foreground needed)
cargo run --features devtools --bin rdprobe                           # frame-buffer regression + failure paths for all three protocols
cargo run --features devtools --bin vncprobe -- <host> <port> [password]
cargo run --features devtools --bin rdpprobe -- <host> <port> [user] [password] [domain]
cargo run --features devtools --bin rfbhello -- <host> <port>
cargo run --features devtools --bin ptytest  -- ping 127.0.0.1 -n 2
```

Build notes:

- **MSVC toolchain**: on the dev machine `check-msvc.bat` / `run-msvc.bat` wrap `vcvars64` and nasm. ⚠️ Both scripts still `cd` into the old egui directory — repoint them at this one before use
- `profile.dev.debug = 0`: with MSVC, `debug=true` makes cc pass `/Z7`, which trips cl D8050 when spawning Rust; dev debug info is therefore off (dependencies still compile at `opt-level = 2`)
- The app icon is embedded from `icons/icon.ico` by `build.rs` via winresource (Windows only)
- `russh` builds with default features off (avoiding the aws-lc-rs C build) and keeps only the ring backend plus hand-picked features; `[patch.crates-io]` points `picky` and `sspi` at the vendored patched copies so their ecdsa version matches russh

---

## Architecture

```text
src/
  main.rs         entry: 32MB stack thread + crash log + DComp off + window geometry + icon assets
  app.rs          shell: menus / toolbar / quick connect / sidebar / tab strip / central area / status bar / overlay dialogs
  term.rs         terminal core: Term state machine, TermSnapshot build & paint, keyboard/mouse → VT encoding, selection / paste / find
  tabs.rs         tab model and kinds (Terminal / Browser / RemoteDesktop / LocalFiles / Macro / PortScan)
  config.rs       AppConfig + AiSettings (serde defaults, migration, fallback path)
  sessions.rs     session model + sessions.json (recent list, first-run examples)
  backends/
    mod.rs        shared backend IO: output_rx / input_tx / resize_tx
    ssh.rs        russh: auth, ProxyJump, known_hosts checking, SshHandle::open_sftp
    local.rs      portable-pty local shell / arbitrary commands
    tcp.rs        raw TCP (Telnet, minimal IAC negotiation)
  remote/
    mod.rs        VNC / RDP / SPICE behind RdCommand / RdEvent + friendly_error + key maps
    vnc.rs        VNC (vendored vnc-rs, BGRA pixel format forced)
    rdp.rs        RDP (vendored IronRDP, CredSSP + TLS + accept-any-certificate)
    spice.rs      SPICE (vendored capsaicin, primary surface only)
  framebuf.rs     remote-desktop frame buffer: dirty rects → RGBA master → BGRA (pure logic, offline-regressable)
  rdp.rs          remote-desktop tab: scaled painting, keyboard capture, remote cursor, controls
  sftp.rs         right-docked SFTP panel (drag-drop upload, progress, inline actions)
  sftp_client.rs  SFTP wrapper: list / transfer / cancel / progress + path utilities
  filebrowser.rs  local file browser tab
  browser.rs      WebView2 child window (dedicated thread + message pump + event callbacks)
  ai.rs           multi-provider request building / response parsing / command extraction / danger & write checks
  ai_assistant.rs AI chat panel (bubbles / attachments / terminal context / Agent toggle)
  ai_config_panel.rs  AI settings panel
  config_panel.rs     general settings panel
  screenshot.rs   GDI capture of the main window client area (PrintWindow → BitBlt fallback)
  tunnel.rs       local TCP forwarder
  portscan.rs     port scanner (bounded worker pool + stop flag)
  macros.rs       macro model / byte encoding / trigger and hotkey dispatch
  i18n.rs         Chinese/English static string table
  ui_icons.rs     shared icons (bypassing the component layer's default-subset Icon)
  widgets.rs      shared widgets (password field with eye toggle)
  win32.rs        main-window HWND registry (browser child window / capture / drop-point math)
```

**Data flow**

- Terminal: `pty/ssh → mpsc → vte Processor.advance(Term) → renderable_content() → TermSnapshot → canvas`
- Terminal input: `GPUI events → VT sequences → mpsc → pty/ssh` (device-query replies loop back through `PtyProxy`)
- Remote desktop: `backend thread → RdEvent{Frame dirty rect} → FrameBuffer → BGRA → RenderImage → canvas`
- Browser: `gpui main thread ⇄ channel ⇄ dedicated WebView thread (own message pump) ⇄ WebView2`

---

## Development phases

The phases follow `main.rs`'s own module documentation:

| Phase | Scope | Status |
|---|---|---|
| P1 | Product skeleton: unified tabs / SFTP dock / status bar / settings panel / i18n | Done |
| P2 | Remote desktop: VNC / RDP / SPICE frame rendering, backends untouched | Done |
| P3 | Compatibility layer: wry browser tab + multi-provider AI assistant + screenshot analysis | Done |
| P4 | Polish: macros / keep-alive / packaging / performance & memory tuning | In progress (macros done; keep-alive and packaging not started) |

## Engineering notes (GPUI / wry pitfalls)

1. **DirectComposition must be off.** gpui renders through `CreateSwapChainForComposition` by default, and a DComp visual tree does not contain plain HWND child windows — the embedded browser's WebView2 gets completely covered (the page really did load). `main.rs` sets `GPUI_DISABLE_DIRECT_COMPOSITION=true` so gpui uses `CreateSwapChainForHwnd` instead; an explicit user setting wins.
2. **The WebView needs its own thread.** wry calls `wait_with_pump` when creating WebView2, pumping messages on the *calling* thread. From inside a gpui render frame that dispatches gpui's own `WM_PAINT` too, so rendering re-enters and the frame scheduler panics on a second `AppCell` borrow ("click the browser and it dies"). Creation and every call therefore live on a dedicated thread, with the main thread talking to it over channels.
3. **Big stack required.** gpui lays out and paints recursively and this project's element tree is deep; the Windows default 1MB main-thread stack overflows occasionally, and a stack overflow never reaches the panic hook, leaving no log at all. The whole app runs on a 32MB stack thread (the standard practice for Zed/gpui apps).
4. **Icon assets.** `gpui_kit::application().with_assets(AllAssets)` must register the full icon set, or every icon in the component library paints blank; the component layer's `Icon` only accepts its default subset, so `Icon::default().path(...)` is used to reach the rest (see `ui_icons.rs`).
5. **Test-macro collision.** `use gpui_kit::*` brings the `gpui_kit::test` *module* into scope; a test module must not `use super::*`, or `#[test]` resolves to it and expands forever.
6. **Compute the terminal origin once.** A canvas prepaint writes the text-grid origin into the global `TermArea`, and both painting and mouse column/row math use that same origin, so selection can never drift.
7. **Native child windows always paint above gpui.** Whenever an overlay (dialog / menu / context menu) opens, the WebView area must be hidden or clamped or it covers the UI.
8. **Dialogs are same-frame overlays.** egui's separate viewport windows became absolutely positioned overlay layers that stay out of the main layout; `overlay_open()` is the single check for "an overlay is up", used for hiding the WebView and gating shortcuts.

## Known gaps

- **Keep-alive is not implemented**: `config.keep_alive_interval` and its settings field exist, but `macros::apply_keep_alive()` is an empty stub and no SSH heartbeat task is spawned
- **X11 forwarding is not implemented**: the `menu.x11` / `item.x11_forward` strings exist, but neither the menu nor any backend is wired up
- **"Restore last session" is not wired**: the `welcome.restore` string exists but the welcome page never renders that button
- **No tab drag-reordering or double-click rename**: only click-to-switch and a close button (the egui version had both)
- **Remote-desktop clipboard sync**: only the `clipboard_sync` toggle exists; nothing is connected behind it
- **SPICE** composites the primary surface only (no multi-surface or cursor compositing); **VNC** does not sync CapsLock; **Telnet** ignores NAWS window negotiation
- **Terminal performance**: one `shape_line` per row per frame with no row-level shaping cache, and the whole-screen snapshot is rebuilt every frame — row caching or a GPU vertex cache are the obvious next steps
- **Split panes are not implemented**: gpui-kit ships `resizable` (`h_resizable` / `v_resizable`) and a full `dock` (recursive PaneTree splitting + drag & drop + layout dump/load), but this project still assumes a single terminal for sizing, focus and input (`fit_to` estimates from the viewport, one `term_focus` / `term_snap`, a global `TermArea`), so that layer has to be taken apart first
- **Packaging**: no installer or distribution script
- **Leftovers**: `src/status.rs` is dead code (the status bar moved into `app.rs`), and `check-msvc.bat` / `run-msvc.bat` still point at the old egui directory

## Relationship with the main project

- A separate directory (`rustterm-gpui`); no file in the main project is touched. What is shared is **vendored, patched dependencies**: `vendor/russh` (legacy-algorithm fallbacks, used exactly like the main project's `src-tauri/src/ssh.rs`), `vendor/picky` and `vendor/sspi` (via `[patch.crates-io]`), plus `vendor/ironrdp`, `vendor/vnc-rs` and `vendor/capsaicin` for the three remote-desktop protocols
- SSH algorithm negotiation and host-key policy, and the port scanner's concurrency model, both follow the main project (`src-tauri/src/ssh.rs`, `src-tauri/src/portscan.rs`)
- Once this round of validation passes (rendering performance / CJK / keyboard-mouse / SFTP / remote desktop / browser / AI), `term.rs`'s paint and input-encoding layers can move into the main project to replace the xterm.js frontend
