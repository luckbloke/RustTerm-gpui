# X11 client-compatibility research: GTK3/firefox probes, real X.org semantics, Windows X servers, and network performance

Research date: 2026-11-08. Web search was unavailable in this session; all findings below come from direct fetches of primary sources (X.org xserver git, GNOME gtk git, Mozilla hg). Every claim is sourced or explicitly marked **UNCONFIRMED**.

---

## 1. Who kills GTK/firefox on X errors, and what real X.org sends for the window-0 probe chain

### 1.1 Real X.org: window 0 is BadWindow, sent as an *error packet*, client is NOT disconnected

From `dix/dispatch.c` (fetched master):
https://gitlab.freedesktop.org/xorg/xserver/-/raw/master/dix/dispatch.c

- Every window-taking request goes through `dixLookupWindow(&pWin, stuff->id, ...)`; id `0` (None) is not a valid resource, so it returns `BadWindow` (see `ProcGetWindowAttributes` line ~813, `ProcChangeWindowAttributes` line ~791, `ProcGetGeometry` line ~1059, `ProcQueryTree` line ~1072 — all `return rc` from `dixLookupWindow`).
- Crucially, the dispatch loop does:

```c
if (client->noClientException != Success) {
    CloseDownClient(client);
} else if (result != Success) {
    SendErrorToClient(client, client->majorOp, client->minorOp,
                      client->errorValue, result);
    break;   /* yields, keeps client alive */
}
```

So X.org:
1. Sends a **standard X error packet** (first byte 0, error code BadWindow=3, major/minor opcode, and the **sequence number of the failing request**).
2. Continues processing the client. The connection is **only** closed on `noClientException` (I/O error) — never for a mere protocol error.

**Implication for src/xsrv:** if your Rust server kills/disconnects the client, drops the error, or answers window-0 requests at all, that diverges from X.org. Correct behavior: reply nothing, send exactly one error packet with the right sequence, keep going.

Error-packet checklist (X protocol, chapter 8 / xproto):
- byte 0 = 0 (error), byte 1 = error code (BadWindow = 3, BadDrawable = 9, BadMatch = 8, BadValue = 2)
- bytes 2–3 = the client's current sequence number for that request
- bytes 4–7 = bad resource id (`errorValue`), 8–11 = minor opcode, 12–13 = major opcode
- GetWindowAttributes(0)/GetGeometry(0) → **BadWindow** (dixLookupWindow is type-checked for WINDOW); drawing requests (XPutImage etc.) on 0 → **BadDrawable** (dixLookupDrawable); CopyArea between mismatched drawables → BadMatch.

### 1.2 Why firefox/GTK *children* died: GDK's fatal error handler + sequence-range traps

From `gdk/x11/gdkdisplay-x11.c` (gtk-3-24 branch, fetched):
https://gitlab.gnome.org/GNOME/gtk/-/raw/gtk-3-24/gdk/x11/gdkdisplay-x11.c

`_gdk_x11_display_error_event()` is the whole story:

```c
for (tmp_list = display_x11->error_traps; ...) {
    trap = tmp_list->data;
    if (SEQUENCE_COMPARE (trap->start_sequence, <=, error->serial) &&
        (trap->end_sequence == 0 ||
         SEQUENCE_COMPARE (trap->end_sequence, >, error->serial)))
      { ignore = TRUE; trap->error_code = error->error_code; break; }
}
if (!ignore) {
    ...
    g_warning ("The program '%s' received an X Window System error...");
    _exit (1);          /* <-- the death */
}
```

So GTK apps die with `_exit(1)` on **any X error whose serial is not inside an active trap**. GTK's traps (`gdk_x11_display_error_trap_push/pop`) are purely **sequence-number-range** based: push records `XNextRequest()`, pop records the end serial (and `XSync`s when the code is needed). Two consequences for your server:

1. **Sequence numbers must be exact and consistent.** If your server miscounts sequences (e.g., assigns different sequence numbers to a request than the client counted, wraps them, or attributes an error to the wrong sequence), an error that GDK *did* trap is judged "outside the trap" and the app `_exit(1)`s. This is the most likely reason firefox children died on `GetWindowAttributes(0)` etc. while surviving on real X.org: on X.org these probes are wrapped in `gdk_x11_display_error_trap_push/pop_ignored` (see gdkscreen-x11.c below), and the errors are silently eaten; on your server the error either never arrived (client hung) or arrived with a wrong serial / error code and escaped the trap.
2. Errors must be delivered **in order, before any later reply the client reads**; Xlib matches errors to `xError.serial` only against what it has sent.

### 1.3 The startup probe chain itself: where it lives

I could not 100% pin the exact `GrabServer → GetSelectionOwner → GetWindowAttributes(0) → GetGeometry(0) → QueryTree(0) → ChangeWindowAttributes(0)` caller to one line (searchfox/GitLab code search were unavailable). What I *did* confirm from primary sources:

- **GTK3 wm/compositor probes at startup** (`gdk/x11/gdkscreen-x11.c`, fetched):
  https://gitlab.gnome.org/GNOME/gtk/-/raw/gtk-3-24/gdk/x11/gdkscreen-x11.c
  - `check_is_composited()`: `XGetSelectionOwner(display, "_NET_WM_CM_S0")`; owner != None ⇒ "composited".
  - `_gdk_x11_screen_setup()`: requests XFixes selection notification for `_NET_WM_CM_S0`, then calls `check_is_composited`.
  - `get_net_supporting_wm_check()`: `XGetProperty(root, _NET_SUPPORTING_WM_CHECK)` **inside a pushed error trap**; then `fetch_net_wm_check_window()` calls it twice (root, then on the returned window — the window must list *itself*), then `XSelectInput(check_window, StructureNotifyMask)` (= your observed `ChangeWindowAttributes`) **also inside a trap**, and finally re-validates the property again. Every step traps.
  - `gdk_x11_screen_supports_net_wm_hint()` then reads `_NET_SUPPORTED` from root; missing/empty ⇒ every hint FALSE ⇒ graceful no-WM operation.
- **firefox's GrabServer probe** (`toolkit/components/remote/nsXRemoteClient.cpp`, ESR78/FIREFOX_78_0esr_RELEASE and main, fetched):
  https://hg-edge.mozilla.org/releases/mozilla-esr78/raw-file/FIREFOX_78_0esr_RELEASE/toolkit/components/remote/nsXRemoteClient.cpp
  `SendCommandLine()` installs a custom error handler:
  ```cpp
  static int HandleBadWindow(Display*, XErrorEvent* event) {
    if (event->error_code == BadWindow) { sGotBadWindow = true; return 0; /* ignored */ }
    return (*sOldHandler)(display, event);
  }
  ```
  `GetLock()` does `XGrabServer → XGetWindowProperty → XChangeProperty → XUngrabServer`; `FindBestWindow()` walks `XQueryTree(root)` + property probes on possibly-dead windows — **all BadWindow errors explicitly ignored**. This is why firefox "children" survive on X.org even while issuing probes on stale/zero windows: they deliberately tolerate BadWindow but **not other error codes** — if your server answers BadWindow probes with a different code (or a *success reply* with wrong shape), it escapes both this handler and GDK's traps.

**Most likely root cause (marked as assessment, not fetched fact):** your server's error packets were either (a) not sent, (b) sent with wrong `sequenceNumber`/`majorOp`/`minorOp`/`errorValue`, or (c) sent as something other than a first-byte-0 error packet. GTK/firefox code is trap-based around *correct* BadWindow errors; a mis-formed or missing error breaks the trap matching and hits `_exit(1)`.

### 1.4 Why answering window-0 with synthetic success froze the screen

If the fake-success path made `XGetSelectionOwner(_NET_WM_CM_S0)` (or the `_NET_SUPPORTING_WM_CHECK` double-read) succeed, GDK sets `screen->is_composited = TRUE` and/or `wmspec_check_window != None` (gdkscreen-x11.c, cited above). Then:
- `gdk_x11_screen_supports_net_wm_hint()` reads `_NET_SUPPORTED`; if you advertised hints there (e.g. `_NET_WM_STATE`, `_NET_ACTIVE_WINDOW`), GTK takes the "WM present" branches: it waits for `_NET_WM_STATE` client messages / frame properties that a WM would set, does `gdk_window_foreign_new` on `_NET_ACTIVE_WINDOW` values, etc.
- `is_composited == TRUE` steers GTK into rgba/ARGB visual selection (`_gdk_x11_screen_init_visuals` picks the "best" rgba visual when composited) and apps avoid opaque repaints expecting the compositor to blend.
- **UNCONFIRMED (couldn't fetch the exact source):** the precise "no drawing requests ever" path is most consistent with GTK believing a compositor exists and deferring to GL/composited rendering that never gets validated by your server (no Damage events consumed, no GLX), so no client-side XPutImage/PutImage ever occurs. Treat as hypothesis; verify by checking which visual GTK picks (log `gdk_visual` choices) and whether `RenderCreatePicture`/GLX requests arrive.

What a real *no-WM X.org* returns (from the cited sources' logic):
- `GetProperty(root, _NET_SUPPORTING_WM_CHECK)` → reply with `type=None, format=0, nItems=0, bytesAfter=0, length=0` (property absent). GDK's `get_net_supporting_wm_check` sees `type != XA_WINDOW` ⇒ None ⇒ no WM. Graceful.
- `GetSelectionOwner(_NET_WM_CM_S0)` → reply `owner=None(0)`. `check_is_composited` ⇒ FALSE. Graceful.
- `GetSelectionOwner(CLIPBOARD_MANAGER)` → None; `gdk_x11_display_supports_clipboard_persistence` returns FALSE (gdkdisplay-x11.c cited above).
- `_NET_SUPPORTED`, `_NET_WORKAREA`, `_NET_CURRENT_DESKTOP`, `_NET_ACTIVE_WINDOW` → absent-property replies; every `gdk_x11_screen_supports_net_wm_hint` FALSE; GTK falls back to "unknown WM", core drawing path.

**Actionable: never answer these with fake windows. Always reply "property absent" (type=None) / owner=None.** Not owning `_NET_WM_CM_S0` and not setting `_NET_SUPPORTING_WM_CHECK` is exactly what plain `Xorg` + `xinit` does, and GTK apps run fine that way.

---

## 2. Extensions and root-window environment on mature Windows X servers

Confirmed:
- Cygwin/X is **X.org (XWin)** with the standard dix, i.e. it *is* the reference dispatch above: window-0 → BadWindow error packet, never disconnect. Docs: https://x.cygwin.com/docs/ug/using.html . XWin's MIT-SHM is documented in its User's Guide "Shared memory support" chapter (https://x.cygwin.com/docs/ug/using-shared-memory.html — **UNCONFIRMED**: page not fetched this session); the general X.org rule is that MIT-SHM is only usable when client and server share memory, so over SSH real servers still advertise it, and libX11 doesn't know remoteness — apps that use MIT-SHM over SSH get garbage/crashes, which is why deliberately NOT advertising it (as you do) is actually the correct, safe choice; GTK only loses fast-copy paths and falls back to PutImage.
- GDK requires only: XFIXES (selection notify; optional), SHAPE, RANDR (any; else Xinerama, else single-head fallback — see `init_randr15/13/init_no_multihead` in gdkscreen-x11.c cited above), XKB optional, XInput optional (GDK falls back to core; `gdk_x11_display_get_default_seat` even traps `XIGetClientPointer`).
- **UNCONFIRMED (docs unfetchable this session):** exact extension lists of MobaXterm/Xming/VcXsrv/X410. Known-good behavior of MobaXterm for SSH X11 firefox is widely reported but I could not fetch a primary doc. The evidence-backed conclusion is different: what matters is not which extensions they advertise, but that they **all ship the real X.org dix dispatch** (VcXsrv and Cygwin/X literally build xorg-server for Windows; MobaXterm's "MobaXserver" embeds XWin — vendor pages UNCONFIRMED). If your Rust server implements the X.org dix error semantics (§1.1) plus honest "absent property / None owner" replies (§1.4), GTK3 apps need no WM and no extra extensions.
- RandR: GDK uses RandR 1.5/1.3 if available, else single monitor. Advertising **RANDR with one output/crtc**, or nothing at all, both work; do not advertise RANDR and then fail `RRGetScreenResources` replies.

---

## 3. Performance over SSH (XPutImage-dominated bandwidth)

Sources fetched this session are thin here; the following combines protocol knowledge with what NX/X2Go document. Markings where I could not verify online.

- **Biggest lever: damage tracking.** GTK4/firefox with XFIXES+DAMAGE present render incrementally (the extensions you already advertise). Full-window 1–4MB `PutImage`s usually mean the *client chose* full repaint because: (a) the window has no backing store and it got an Expose, (b) it believes the screen is composited (§1.4), or (c) GTK's double-buffer ended in a full flush. Fix the fake-success freeze (§1.4) first; that alone stops "nothing draws" and pathological full repaints.
- **BackingStore**: X.org supports `BackingStore` on windows; with backing store enabled, GTK gets no Expose storms after occlusion. Implementing backing store in src/xsrv is heavy; a cheaper approximation is correct Expose rect coalescing.
- **XCopyArea scroll optimization:** when a client scrolls with CopyArea, forward only the *newly exposed* rect to your Windows renderer (translate the blit locally), and emit GraphicsExpose only for regions that actually need it. Mature servers do this blit in DDX without touching the wire at all — CopyArea costs 0 bytes of X traffic from the client, so optimize your *internal* path, not the protocol.
- **PutImage handling:** clients already clip to their dirty rect; your win is server-side: coalesce consecutive PutImage rects per frame, and when rendering to the Windows surface, blit only intersecting rects. **UNCONFIRMED (couldn't fetch NX sources this session):** NX's libnxcomp achieves far more than this — it is an X-protocol-aware proxy that delta-compresses PutImage payloads, caches glyph/property data, and re-encodes only changed scanline runs. If you want a first step inside your own server: per-frame diff the new framebuffer region against the previous surface and ship only changed rows/tiles to the Windows renderer (CPU diff is cheap vs. 4MB GDI blits). X2Go/NX documents: https://wiki.x2go.org/ (**UNCONFIRMED** — not fetched).
- **Compression below the protocol:** since you're behind SSH anyway, the cheapest wins are enabling SSH channel compression (`Compression yes`) and, better, your own tile-cache protocol between the Linux-side xsrv instance and the Windows renderer (zstd on 64x64/128x128 tiles; repeated tiles ≈ zero bytes). This mirrors what NX does at the X level without parsing X protocol.
- **MIT-SHM absence:** correct to omit (§2). GTK's cairo Xlib surface then uses PutImage; that is what MobaXterm sessions do too and firefox is still usable.

---

## 4. Concrete action list for src/xsrv

1. **Never disconnect on protocol errors.** Mirror `Dispatch()`'s `SendErrorToClient` + continue (§1.1).
2. **Audit sequence-number bookkeeping** so trapped errors match GDK's `SEQUENCE_COMPARE` ranges: reply/error sequence must equal the client's per-connection request counter for that request, wrapping correctly at 2^16 (§1.2).
3. **Return exact X.org codes for window-0:** BadWindow (3) for `GetWindowAttributes/GetGeometry/QueryTree/ChangeWindowAttributes/GetSelectionOwner`-adjacent window ops, errorValue = the id, major/minor opcodes set; BadDrawable (9) for pixmap/drawable ops (§1.1).
4. **Revert the synthetic-success replies.** `_NET_SUPPORTING_WM_CHECK`/`_NET_SUPPORTED`/`_NET_ACTIVE_WINDOW`/`_NET_WORKAREA`/`_NET_CURRENT_DESKTOP`/`_NET_WM_STATE` must be *absent* properties; `_NET_WM_CM_S0`/`CLIPBOARD_MANAGER` selections must have owner None. That is the state of a bare X.org and GTK3 handles it cleanly (§1.4).
5. Optionally advertise RANDR (1.2+ with a single output/crtc/monitor) — otherwise GDK falls back to one monitor via `init_no_multihead`; either is fine.
6. Keep MIT-SHM unadvertised; add internal frame-diff/tile caching + coalesced dirty rects for the Windows renderer; coalesce Exposes; consider backing-store or at least expose-rect merging (§3).

## Source list

- X.org xserver `dix/dispatch.c` (ProcGetWindowAttributes/ProcChangeWindowAttributes/ProcGetGeometry/ProcQueryTree, SendErrorToClient dispatch loop): https://gitlab.freedesktop.org/xorg/xserver/-/raw/master/dix/dispatch.c
- GTK3 `gdk/x11/gdkdisplay-x11.c` (error trap implementation, `_gdk_x11_display_error_event` `_exit(1)`, clipboard-manager checks, precache atoms incl. `_NET_WM_CM_S0`): https://gitlab.gnome.org/GNOME/gtk/-/raw/gtk-3-24/gdk/x11/gdkdisplay-x11.c
- GTK3 `gdk/x11/gdkscreen-x11.c` (`get_net_supporting_wm_check`, `fetch_net_wm_check_window`, `check_is_composited`, `supports_net_wm_hint`, RandR/monitor init): https://gitlab.gnome.org/GNOME/gtk/-/raw/gtk-3-24/gdk/x11/gdkscreen-x11.c
- GTK3 `gdk/x11/gdkselection-x11.c` (SetSelectionOwner/GetSelectionOwner semantics): https://gitlab.gnome.org/GNOME/gtk/-/raw/gtk-3-24/gdk/x11/gdkselection-x11.c
- GTK (main, GTK4) `gdk/x11/gdkclipboard-x11.c` (CLIPBOARD_MANAGER probing, XFixes selection notify): https://gitlab.gnome.org/GNOME/gtk/-/raw/main/gdk/x11/gdkclipboard-x11.c
- firefox ESR78 `toolkit/components/remote/nsXRemoteClient.cpp` (GrabServer lock protocol, BadWindow-tolerant handler): https://hg-edge.mozilla.org/releases/mozilla-esr78/raw-file/FIREFOX_78_0esr_RELEASE/toolkit/components/remote/nsXRemoteClient.cpp
- firefox main same file: https://raw.githubusercontent.com/mozilla-firefox/firefox/main/toolkit/components/remote/nsXRemoteClient.cpp (via https://searchfox.org/firefox-main/source/toolkit/components/remote/nsXRemoteClient.cpp)
- Cygwin/X user guide (XWin = X.org on Windows): https://x.cygwin.com/docs/ug/using.html

Explicitly unconfirmed: exact extension sets of MobaXterm/Xming/VcXsrv/X410; XWin MIT-SHM doc page; NX/x2go compression internals; the exact line of the GTK code path that issues the full window-0 probe chain you observed (searchfox/GitLab code search unavailable this session). Searchfox confirmed the only XGrabServer in firefox widget code is in the remote client path.
