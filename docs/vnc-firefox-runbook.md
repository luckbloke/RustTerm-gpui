# Runbook: Firefox on a remote openSUSE server viewed via VNC (replacing X11-over-SSH)

Audience: Rust terminal app (Windows) with a working RFB VNC client tab, connecting over SSH to an openSUSE 15.x server, root, no physical display required. Goal: smooth Firefox UI (menus, scrolling) instead of ~1–2 fps over `ssh -X`.

> ⚠️ **Verification note:** live web verification was not possible while writing this runbook (no network). Commands reflect openSUSE 15.x / TigerVNC / x11vnc conventions as of Firefox-ESR-era packages; items I could not double-check are marked **[unverified]**.

---

## 1. Option comparison

| Option | What it is | GTK/X extension support | Smoothness for Firefox | Setup effort | Verdict |
|---|---|---|---|---|---|
| **(b) TigerVNC `Xvnc` (`vncserver :1`)** | A **full X.org server** with the VNC protocol built in (`libvnc.so` module / Xvnc binary). Firefox runs directly on it via `DISPLAY=:1`. | **Real Xorg semantics**: RANDR (fixed virtual modes), XKB, XInput/XInput2, XFixes, RENDER, GLX (software Mesa). GTK polls happily — the pointer lives *on the server*, so no round-trips. | Excellent — updates are pushed as RFB framebuffer deltas server-side; menus/scrolling run at server render speed (30–60 fps LAN). | Low: one package, one command, one password. | ✅ **Recommended** |
| **(a) x11vnc attached to an existing Xvnc/Xorg display** | Mirrors an *existing* X display into VNC by polling it with XSHM/XDAMAGE. | Same as the display it mirrors. If that display is Xvnc, this adds a redundant copy layer; if it's headless Xorg you need `Xvfb`/dummy driver anyway. | Good but strictly *worse than (b)*: x11vnc polls the X server and re-encodes; adds latency and CPU on a box that could serve RFB natively. | Medium (needs an X display to exist first). | Use only if you already must attach to a real physical/Xvnc display. |
| **(c) x11vnc on physical `:0`** | Mirrors the console display. | Real hardware Xorg. | Fine, but requires an active console session, auto-login, and a monitor (or dummy plug) on the server. Violates "no physical display needed". | Medium + console fiddling. | ❌ Not for this use case. |

**Why (b) wins for GTK/Firefox:** the GTK "pointer polling flood" that kills `ssh -X` disappears because the X server and the app are on the *same machine* — GTK's repeated `XQueryPointer`/motion queries are local memory-speed calls into Xvnc. Only framebuffer deltas cross the network as RFB. Xvnc is genuinely X.org (TigerVNC builds it from the X.Org tree with a VNC encoding extension), so RANDR, XKB and XInput2 all behave as GTK expects. **Confirmed by design; exact extension list per build [unverified].**

---

## 2. Setup on openSUSE 15.x

### 2.1 Packages

```sh
# as root on the openSUSE server
zypper install tigervnc xorg-x11-fonts-core dejavu-fonts
# optional, only if you ever want to mirror an existing display:
# zypper install x11vnc
```

Package names: `tigervnc` provides `Xvnc`, `vncserver`, `vncpasswd`. `x11vnc` is a separate package with the same name. RealVNC exists as a proprietary third-party download — not needed. **[unverified: exact sub-package split on 15.x; `tigervnc` is the standard name on Leap.]**

### 2.2 Password (per user, not root ideally)

```sh
# run as the normal user who will own the Firefox session
vncpasswd            # set a view+full password; store file: ~/.vnc/passwd
chmod 600 ~/.vnc/passwd   # vncpasswd does this automatically
```

If your Rust VNC client supports **no-auth**, you can skip vncpasswd and bind to localhost only (see §5) — but a password costs nothing and is safer.

### 2.3 Quick manual start (recommended first test)

```sh
vncserver :1 -geometry 1280x800 -depth 24 -dpi 96 \
  -localhost no -rfbport 5901 -SecurityTypes VncAuth
# run firefox in it:
DISPLAY=:1 firefox -no-remote -profile /root/.vnc/ffprof &   # see §3.3
```

- `:1` ⇒ RFB port **5901** (display N ⇒ 5900+N).
- `-depth 24` ⇒ default RFB pixel format 32bpp; your client can negotiate TrueColor 24/32.
- `-localhost no` allows direct LAN connections; omit (default `yes` on TigerVNC) if you only use the SSH tunnel — **preferred**, see §5.

**Note on flag style:** TigerVNC ≥1.9 accepts `vncserver :1 -geometry ... -depth ...`; some builds want the legacy `-rfbport 5901` passed as `-rfbport=5901` or via `$HOME/.vnc/config` **[unverified per-version]**. Equivalent permanent config:

```sh
cat > ~/.vnc/config <<'EOF'
geometry=1280x800
depth=24
dpi=96
localhost=yes
# session=... only for the systemd variant with a desktop WM; not needed for manual firefox
EOF
```

### 2.4 Persistent: systemd user service (openSUSE)

openSUSE ships a systemd integration:

```sh
# as root, enable a persistent user session
loginctl enable-linger <username>
# as <username>:
vncpasswd
systemctl --user enable vncserver@:1.service   # or start once
systemctl --user start vncserver@:1.service
```

Check the unit exists: `systemctl --user list-unit-files 'vncserver@*'`. **[unverified: exact unit name on 15.x — some versions use `vncserver@1.service` or `xvnc-server@:1`]**. If systemd plumbing fights you, a plain `vncserver :1 ...` in a `tmux`/`@reboot` cron is perfectly adequate for a headless utility box.

### 2.5 Firewall

```sh
# firewalld (default on Leap):
firewall-cmd --permanent --add-port=5901/tcp
firewall-cmd --reload
# NOT needed at all if you use the SSH tunnel (§5) and bind to localhost.
```

If SuSEfirewall2 (older): `yast firewall` → allowed services → VNC. **[unverified for very old 15.0]**

---

## 3. Tuning for LAN responsiveness

### 3.1 TigerVNC server-side

Xvnc does the encoding; defaults (ZRLE + Tight) are good. Useful knobs:

- `vncserver :1 -Resolution 1280x800` — some TigerVNC versions accept `-geometry`/`-resolution` interchangeably **[unverified which on your build; test `Xvnc --help`]**.
- Log level: add `-log *:syslog:30` or leave default.

### 3.2 RFB encodings — what your simple client should request

| Encoding | Wire behavior | LAN speed | Client complexity | Recommendation |
|---|---|---|---|---|
| **Raw** | No compression, full/partial rects as pixels | Highest bandwidth but zero CPU; on 1 Gbps LAN at 1280×800 easily fine | Trivial (you already do dirty-rect) | ✅ **Best first choice for LAN** — lowest server latency |
| **Hextile** | Tiled, RLE-ish per tile | Very fast, ~5–10× less bandwidth than raw | Simple | ✅ Good fallback if raw saturates Wi-Fi |
| **Tight** | JPEG/zlib gradient compression | Great on slow links, adds server CPU | Moderate (JPEG + zlib decoders) | Only if bandwidth-limited |
| **ZRLE** | zlib per tile, TigerVNC default | Efficient, moderate CPU | zlib inflate | Fine if you already have zlib inflate |

Practical order in your SetEncodings: `Raw, Hextile, ZRLE, Tight`. Over 100 Mbps LAN at 1280×800×24: raw ≈ 4 MB/full frame — scrolling partial rects stay well under wire speed; menus are tiny rects. If you see bandwidth issues (Wi-Fi), prefer Hextile/ZRLE.

Color depth: request **TrueColor, 24 or 32 bpp** (SetPixelFormat). Xvnc runs depth 24, so no palette conversions. Avoid 16 bpp — dithering artifacts on text.

### 3.3 Firefox: separate profile + no first-run noise

```sh
mkdir -p ~/.vnc
# one-time:
DISPLAY=:1 firefox -no-remote -CreateProfile vncprof
# each launch:
DISPLAY=:1 firefox -no-remote -profile ~/.vnc/ffprof vncprof &
```

Simpler modern form: just `-profile /path` (auto-creates). Add to the profile's `user.js` to kill popups:

```js
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("app.update.auto", false);
user_pref("datareporting.policy.firstRunURL", "");
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
```

- `MOZ_DISABLE_CONTENT_SANDBOX`: **not needed** — irrelevant to rendering smoothness; leave the sandbox on.
- `-no-remote` is important: prevents the new VNC-session Firefox from hijacking/joining an existing profile instance.
- Firefox 79 is old; consider `MozillaFirefox` update or ESR from the update repo **[unverified availability on that kernel-5.10 box's repos]**.

### 3.4 x11vnc flags (only if you choose option (a)/(c))

| Flag | One-line justification |
|---|---|
| `-deferupdate 10` | Batch framebuffer changes ~10 ms instead of default 200 ms → visibly snappier; slight CPU cost. |
| `-wait 5` | Poll interval ms (default ~20); lower = faster change detection. |
| `-noxdamage` | Ignore XDAMAGE hints, poll full screen — *slower overall* but fixes stale/garbled regions with buggy compositors; try only if you see artifacts. (So: default **with** XDAMAGE is right for speed.) |
| `-repeat` | Keep key autorepeat enabled server-side so held keys repeat in the app. |
| `-noipv6` | x11vnc's IPv6 listener can confuse simple clients; bind IPv4 only. |
| `-rfbport 5901` | Fix the port instead of first-free. |
| `-localhost` | Only accept local (tunnel) connections — see §5. |
| `-noclipboard` | Disable clipboard sync if the primary-selection chatter causes spurious updates (see §4). |

---

## 4. Known pitfalls

- **Clipboard with TigerVNC Xvnc**: standard CLIPBOARD/PRIMARY sync works via RFB ServerCutText/ClientCutText. Some simple clients only implement ServerCutText — copy/paste Windows→VNC may need you to send ClientCutText. No server flag needed with Xvnc. (x11vnc: `-noclipboard` to disable if noisy.)
- **Keyboard layout**: Xvnc runs real XKB; keysyms your client sends land correctly. Keep your RFB client sending *keysyms* (Latin-1/keysym table), not raw scancodes. Non-US layouts: set `XKB_LAYOUT=de` style vars in `~/.vnc/config` **[unverified exact var name; TigerVNC 1.11+ uses `-XkbLayout` in config]**.
- **CapsLock / NumLock sync**: classic RFB wart — if the client machine's CapsLock state differs from the server's, letters come out inverted. TigerVNC clients do lock-sync; a simple client should track CapsLock and send press/release consistently, or send keysyms with correct case and let XKB fix it. If you see stuck-case text, toggle CapsLock twice in the VNC session to resync.
- **GTK polling is a non-issue on Xvnc** — it only hurts when X is remote. No flags needed.
- **No WM**: if you launch bare Firefox on `:1`, there's no window manager → no decorations, no alt-tab. That's usually fine for a single-app kiosk tab; if you want a WM: `zypper install openbox` and run `DISPLAY=:1 openbox &` before firefox. **[unverified: openbox still packaged on 15.x — yes it is, but confirm]**
- **Firefox content sandbox** needs user namespaces; on hardened kernels with `kernel.unprivileged_userns_clone=0` pages may fail to render — that's a Firefox issue, not VNC. Don't disable the sandbox reflexively.

---

## 5. Security: recommend SSH tunnel (your app already has SSH)

Running VNC open on the LAN (`-localhost no` + firewall port) is acceptable at a stretch with VncAuth, but the password handshake is DES-vulnerable (legacy VNC auth). **Better: bind to localhost and tunnel** — your app already has SSH:

Server: omit `-localhost no` (default `localhost=yes`), open **no** firewall port.

Windows client:

```
ssh -L 5901:localhost:5901 user@server      # in your terminal app or your app's SSH layer
```

Then point the VNC client at **`localhost:5901`** — any standard RFB client works, including yours, because the tunnel is transparent TCP. Confirmed pattern: this is exactly how TigerVNC's own `-via`/`-tunnel` options operate under the hood (they just script `ssh -L`).

In-app flow: open SSH session → `vncserver :1` (first time: `vncpasswd`) → open VNC tab to `127.0.0.1:5901`. With `localhost=yes` you can even skip `vncpasswd` (`-SecurityTypes None -localhost yes` is then reasonably safe) — **[unverified: TigerVNC permits None+localhost without password; yes, but test]**.

---

## 6. Sanity checklist (smoothness)

1. `vncserver :1 -geometry 1280x800 -depth 24 -dpi 96` — check `~/.vnc/hostname:1.log` for "Listening for VNC connections on TCP port 5901".
2. `ss -ltn | grep 5901` — listening (loopback if tunneled).
3. Connect; expect the (mostly black) desktop within <1 s.
4. **Menu test**: right-click / open Firefox hamburger menu — should paint instantly, zero "polling flood" on the SSH side (check with `top`: Xvnc CPU near-idle at rest; your old `ssh -X` path showed constant XQueryPointer traffic — it should be gone).
5. **Scroll test**: scroll a long page (e.g., Wikipedia) — expect continuous 30–60 fps updates over LAN; measure dirty-rect rate in your client (target >20 rects-in-flight bursts without stalls).
6. **Drag test**: drag-select text — selection highlight should track the pointer 1:1 with no rubber-band lag.
7. **Keys**: type, hold a key (autorepeat), toggle CapsLock (case resyncs).
8. **Clipboard**: copy text in VNC Firefox → paste on Windows (via ServerCutText) as a stretch goal.
9. `Xvnc` CPU while idle should be ~0% — if it burns CPU at rest, something is polling (your client requesting full refreshes in a loop — only send FullFramebufferRequest on explicit user action or reconnect).

---

### Sources (to cite/re-verify when network is available)

- TigerVNC wiki & `man vncserver`, `man Xvnc` (TigerVNC project, github.com/TigerVNC/tigervnc) — Xvnc is X.org with VNC extension; `localhost` default; `-via` SSH tunneling.
- `man x11vnc` (LibVNCServer project) — `-deferupdate`, `-wait`, `-noxdamage`, `-repeat`, `-noipv6`, `-rfbport`, `-noclipboard`.
- openSUSE Documentation, "VNC" chapter (doc.opensuse.org) — `tigervnc` package, `vncserver@.service` systemd user units, firewall port 5901.
- RFC 6143 (RFB 3.8) — encodings, cut-text messages.
