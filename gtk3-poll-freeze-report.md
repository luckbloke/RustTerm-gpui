# GTK3 (gtk-3-24) XTranslateCoordinates/XQueryPointer storm + painting stop — code-archaeology report

Scope: firefox-79 ESR on GTK 3.24.x, X11 core protocol only (no XI2 advertised, no WM, no compositor, no XSync/RANDR), against a from-scratch Rust X server.
Method: sources read from `GNOME/gtk` branch `gtk-3-24` (cloned tarball), plus X.org dix knowledge. Every claim marked [CONFIRMED] (code read) or [UNCONFIRMED] (inference).

---

## (a) The polling loop: functions that emit XQueryPointer + XTranslateCoordinates

### XQueryPointer (all core-fallback pointer state goes through these)
1. **`gdk_x11_device_core_query_state()`** — gdk/x11/gdkdevice-core-x11.c:251-322. One `XQueryPointer` per call. Reached from:
   - `gdk_device_get_state` / `gdk_device_get_position(_double)` — gdk/gdkdevice.c:581-640, `_gdk_device_query_state` at 1866-1880.
2. **`gdk_x11_device_core_window_at_position()`** — gdk/x11/gdkdevice-core-x11.c:423-565. This is the dangerous one: it contains a **`while (xwindow)` descent loop** that issues one `XQueryPointer` per window-tree level, descending via `child_return`:
   ```c
   while (xwindow)
     {
       last = xwindow;
       ...
       XQueryPointer (xdisplay, xwindow,
                      &root, &xwindow,      /* <-- child_return becomes the next iteration target */
                      &xroot_x, &xroot_y, &xwin_x, &xwin_y, &xmask);
       ...
       if (get_toplevel && last != root &&
           (window = gdk_x11_window_lookup_for_display (display, last)) != NULL &&
           window->window_type != GDK_WINDOW_FOREIGN)
         { xwindow = last; break; }         /* only breaks early when get_toplevel=TRUE */
     }
   ```
   **With `get_toplevel=FALSE` (i.e. `gdk_device_get_window_at_position()`, gdk/gdkdevice.c:647-690), the loop exits only when the server's `child_return` is `None`.** Callers:
   - `get_current_toplevel()` → gdk/gdkdisplay.c:991-1009 (`get_toplevel=TRUE`).
   - `gdk_device_get_window_at_position(_double)` → gtk tooltips (gtk/gtktooltip.c:948,1016,1197), combobox, menu, drag, etc. (`get_toplevel=FALSE` → descends to the leaf).
   - `synthesize_crossing_events()` → gdk/gdkdisplay.c:903-989, 1-2 `gdk_window_get_device_position_double()` (= 1 `XQueryPointer` each, gdk/gdkwindow.c:5032-5050 → gdkwindow-x11.c:3411-3424) per crossing synthesis.

### XTranslateCoordinates
1. **`gdk_window_x11_get_root_coords()`** — gdk/x11/gdkwindow-x11.c:3210-3232, reached via `gdk_window_get_origin()` (gdk/gdkwindow.c:6830-6844). GTK-side drivers:
   - **`rewrite_events_translate()`** — gtk/gtkmain.c:1469-1483: **2 `gdk_window_get_origin()` = 2 XTranslateCoordinates per rewritten pointer event**, called from `rewrite_event_for_window()` (gtkmain.c:1485-1545) whenever a button/motion/scroll event is re-targeted because a **GTK grab with `owner_events=TRUE`** is active (`rewrite_event_for_grabs`, gtkmain.c:1547+).
   - `gdk_x11_window_get_frame_extents()` — gdkwindow-x11.c:3313 (only when `_NET_FRAME_EXTENTS` is advertised — [UNCONFIRMED] whether your server advertises it; do not).
2. **ConfigureNotify handling** — gdk/x11/gdkdisplay-x11.c:996-1006: one XTranslateCoordinates per non-synthetic ConfigureNotify. Not a poll loop, but contributes during event storms.

### The loop that can run forever
**Primary suspect (fits the frozen-phase histogram exactly — 330-350 QP/s, zero other requests): an infinite/very long descent in `gdk_x11_device_core_window_at_position()`'s `while (xwindow)` loop** caused by your server's `XQueryPointer` never returning `child_return == None` (e.g. returning the queried window itself, or a stale child id, when the pointer is over the window). Symptoms match perfectly: the **main thread is stuck inside a single GDK call**, so:
- it emits only XQueryPointer round-trips at Xlib-socket round-trip speed (~3 ms locally ≈ your 330/s) — [UNCONFIRMED until you check your `child_return` semantics; see §5 fix 1];
- the frame clock paint idle never runs again → zero PutImage;
- the process is alive and the server answers every request "correctly";
- trigger = the first event that resolves "which window is under the pointer" after a grab/menu state (enter/leave synthesis, tooltip timer, popup positioning — all fire during ~1 min of link/scrollbar clicking).
**Marked [UNCONFIRMED — highest-value verification]**: log per-window `child_return` of your `XQueryPointer` implementation during the frozen phase; if it is ever non-zero for a window that has no child under the pointer, this is your bug. A second, related stuck-loop exists in the same file for untrusted clients (lines ~470-535), same root cause.

Secondary suspects (need inbound event flow, cannot explain a request-only frozen phase alone):
- **Motion-hint poll chain**: a window that selected `GDK_POINTER_MOTION_HINT_MASK` gets `MotionNotify is_hint=1`; GDK then demands a fresh poll per motion: `gdk_event_request_motions()` — gdk/gdkevents.c:1844-1858 calls `gdk_device_get_state()` (→ XQueryPointer) per hint motion; re-armed in gdk/gdkwindow.c:9377-9396 (`is_hint=TRUE` when evmask has the hint bit) and gtk/gtkwidget.c:7531-7536. Note `gdk_x11_device_core_grab()` deliberately strips `PointerMotionHintMask` from grabs (gdkdevice-core-x11.c:355-357) but plain `XSelectInput` does not. Rate limit is the X motion rate — cannot explain 330/s without input, but explains elevated QP during interaction.
- **Menu/tooltip timers** (gtkmenu.c:2428,4772,4963,5267; gtktooltip.c) poll at 50-300 ms — an order of magnitude too slow; and Firefox draws its own menus, so gtkmenu is very unlikely to be involved [UNCONFIRMED — depends on what GTK widgets Firefox instantiates].

---

## (b) Entry condition: how GTK gets into a stuck grab / pointer-machine state

Key bookkeeping in GTK 3.24 core path:

1. **Every core-protocol ButtonPress creates a GDK-internal "implicit grab"** — gdk/gdkwindow.c:9501-9558 (`proxy_button_event` → `_gdk_display_add_device_grab(..., implicit=TRUE)` at 9542-9556). No `XGrabPointer` needed; this is pure client-side state.
2. **The implicit grab ends only when a GDK_BUTTON_RELEASE arrives and passes all of these checks** — gdk/gdkwindow.c:10077-10105:
   - `!event->any.send_event` (line 10080) — a release with the synthetic bit set is ignored;
   - `_gdk_display_has_device_grab(...)` finds the grab (serial window — gdk/gdkdisplay.c:1296-1320);
   - `event->button.state` must not contain any *other* button mask (line 10098);
   - the release must first **survive translation**: `gdk_x11_device_manager_core_translate_event()` drops the X event silently when `window == NULL`, i.e. when `xevent->xany.window` does not resolve to a known GdkWindow (gdk/x11/gdkdevicemanager-core-x11.c, `ButtonRelease:` branch — "if (window == NULL) return_val = FALSE"). During a real X grab with `owner_events=FALSE`, the server rewrites `event` to the grab window — if your grab-window choice or field packing is off, the release is **dropped** and GTK's implicit grab lives forever.
3. Once stuck, `get_pointer_window()` (gdk/gdkwindow.c:9099+, grab gating) routes all subsequent pointer events to the (dead) grab window → **mouse no longer affects the page**; keyboard stops if a keyboard grab is similarly stuck — note GTK seat grabs (gdk/gdkseatdefault.c:118-172) grab pointer **and** keyboard, and core keyboard events are re-targeted by `get_event_window()` in gdkdevicemanager-core-x11.c when a keyboard grab is registered.
4. If your server ends a real grab without the client seeing it, GTK expects a **LeaveNotify(mode=NotifyUngrab)** (translated to GDK_CROSSING_UNGRAB) or uses `GDK_CROSSING_UNGRAB` bookkeeping at gdkwindow.c:9981-10019; native grab crossing events from *its own* grabs are deliberately ignored (they are synthesized in-process). So missing crossing events alone do not break GTK — the **ButtonRelease** is the load-bearing event.

**Best-fit trigger chain**: ~1 min of clicks/scrollbar drags → per-click implicit GDK grabs + real X grabs (Firefox popup/seat grabs, scrollbar) → one ButtonRelease is lost/misdelivered/mis-stated → grab never ends → all pointer (and keyboard) routing dies; UI freezes while the process stays in the GTK event loop polling pointer state.

---

## (c) Why painting stops: frame-clock freeze analysis

**Confirmed mechanism** — gdk/gdkframeclockidle.c:
- `RUN_PAINT_IDLE(priv) = (freeze_count == 0 && (requested & ~FLUSH_EVENTS) || updating_count > 0)` (macros near top of file); `gdk_frame_clock_idle_freeze()` increments `freeze_count` and calls `maybe_stop_idle()`; while `freeze_count > 0` **the paint idle is removed and never re-added**, but the main loop stays fully alive (timers, X event source, pointer polls) — exactly your symptom.
- Paint cadence is self-ticking (`min_next_frame_time` + `maybe_start_idle` in `gdk_frame_clock_paint_idle` tail); **without `_NET_WM_FRAME_DRAWN` there is no external dependency — the clock cannot stall on its own.**

**Freeze sources on X11 (complete list from grep of `_gdk_frame_clock_freeze` callers):**
1. **Toplevel creation** — gdk/x11/gdkwindow-x11.c:1198-1199: every non-child window is created frozen (`gdk_window_freeze_toplevel_updates` → `_gdk_frame_clock_freeze`, gdk/gdkwindow.c:4774-4782). The **only** thaw is the **MapNotify** handler — gdk/x11/gdkdisplay-x11.c:917-919. Thaw is skipped entirely if the event does not resolve to a GdkWindow (`window == NULL` → `event->any.window = NULL` in gdkdevicemanager-core-x11.c translate) or if `is_substructure` is wrongly computed (depends on `xevent->xmap.window` vs `xevent->xany.window`).
2. **UnmapNotify** — gdk/x11/gdkdisplay-x11.c:893-894 freezes the toplevel again; any UnmapNotify delivered to a toplevel that is never followed by a correctly-resolvable MapNotify leaves `freeze_count > 0` **forever**.
3. **Frame sync** — gdk/x11/gdkwindow-x11.c:454-461 freezes after `begin_frame` iff `impl->frame_sync_enabled && gdk_x11_screen_supports_net_wm_hint(_NET_WM_FRAME_DRAWN)`; thaw only by the `_NET_WM_FRAME_DRAWN` ClientMessage (gdk/x11/gdkdisplay-x11.c:1382-1386, filter `_gdk_wm_protocols_filter`) or by UnmapNotify (line 886-891). Requires both XSync advertised (`extended_update_counter`) and `_NET_SUPPORTED` advertising the hint. You advertise neither → path disabled. **Conversely: never advertise `_NET_SUPPORTED`/XSync partially — a half-implemented `_NET_WM_FRAME_DRAWN` is a guaranteed permanent freeze.**

**Why no PutImage**: firefox-79 software rendering paints only when GTK's frame clock runs the paint/expose cycle (Firefox widget/gtk `WindowSurfaceX11Image` XPutImage on GTK expose events [UNCONFIRMED detail — Firefox-side, from product knowledge]). `freeze_count > 0` ⇒ no paint idle ⇒ no expose ⇒ no PutImage, while timers and pointer polling continue.

**Most probable concrete server bug**: a spurious or late-handled Unmap/Map sequence on a toplevel (popup windows are also non-child GdkWindows and each carries its own frozen-at-creation clock). Check that, after every `XMapWindow`, you emit `MapNotify` with `xany.window == xmap.window == the mapped window` (it selects StructureNotifyMask on itself), `override_redirect` set correctly, and that `UnmapNotify` is never emitted without a later MapNotify. This explains "paints normally for a minute, then stops" as soon as some popup/temp window lifecycle goes wrong. [Partially UNCONFIRMED — verify from your server's event-send log which MapNotify/UnmapNotify you delivered in the transition window.]

---

## (d) Exit conditions on real X.org

- Core `XGrabPointer` is **GrabModeAsync**; there is no frozen-event queue and `XAllowEvents` is **not** part of GTK's state machine (dix/events.c `AllowSomeClient` only services sync-mode grabs / `SyncPointer`; GTK never uses it). So "missing AllowEvents replay" is **not** the cause here.
- What X.org delivers that you must match (dix/events.c `DeactivatePointerGrab`, `ActivatePointerGrab`; dix/enterleave.c `EnterLeaveEvents`/`DeviceEnterLeaveEvents`):
  1. **ButtonRelease during a grab is delivered to the grab window** (event fields: `event` = grab window, `window` = grab window, `child` = actual subwindow), governed by the grab's `event_mask`; the grab **auto-terminates when the last button is released** (`DeactivatePointerGrab`) — no client action needed.
  2. On grab activation, a **LeaveNotify chain with `mode=NotifyGrab`** and correct detail hierarchy (NotifyAncestor/Virtual/Inferior/Nonlinear...); on deactivation, an **EnterNotify chain with `mode=NotifyUngrab`** (enterleave.c). GDK tolerates missing ones for its own grabs (it synthesizes its own, gdkwindow.c:9981-10019) but uses `NotifyUngrab` crossings to recover from *other clients'* grabs and to emit GdkEventGrabBroken.
  3. `FocusIn/FocusOut` with `mode=NotifyGrab/NotifyUngrab` — gdkdevicemanager-core-x11.c `_gdk_device_manager_core_handle_focus` explicitly filters those modes; wrong modes corrupt `has_focus` and can freeze keyboard handling (`HAS_FOCUS` macro).
  4. **Motion `detail`**: core MotionNotify has no detail; `is_hint` is what matters — real X.org sets `is_hint=0` unless the client selected `PointerMotionHintMask`.
- The single event whose absence keeps GTK stuck is the **correct ButtonRelease** (see §b.2 for the four conditions it must satisfy).

---

## (e) Ranked server-side fixes (wire-level)

1. **Fix `XQueryPointer` `child_return` semantics** (highest impact; likely your frozen-phase root cause): return `child_return = None` whenever no child window of the queried window contains the pointer; only return a real child when the pointer is inside a descendant. Also honor: same_screen=True, correct root/root_x/root_y, and a real mask. Verify by logging child_return during the frozen phase. (Protocol xorgproto `XQueryPointer` reply spec; GTK consumer: gdkdevice-core-x11.c:447-460 descent loop.)
2. **Guarantee ButtonRelease delivery & correctness**: every ButtonPress must be paired with a release; deliver it to the grab window during grabs; `send_event=0`; `state` = modifiers + *other still-held* buttons (never the released button's mask); correct `event`/`window` fields so `xany.window` resolves to a GdkWindow your client created. This alone ends stuck implicit grabs.
3. **Never partially advertise**: do not advertise `_NET_SUPPORTED` at all (no WM), and do not advertise XSync unless you implement counter increments + `_NET_WM_FRAME_DRAWN`/`_NET_WM_FRAME_TIMINGS` ClientMessages. Half-support = permanent frame-clock freeze (§c.3).
4. **Map/Unmap Notify correctness**: after XMapWindow emit MapNotify to the window itself (it selects StructureNotifyMask); keep Unmap/Map strictly paired; fill `override_redirect` and `xmap.window`/`xany.window` per core spec (event field = selecting window). This removes the frame-clock freeze leak (§c.1-2).
5. **Real timestamps**: event timestamps must be a monotonic ms clock (root-window uptime), never 0. `0 == GDK_CURRENT_TIME`, so `set_user_time()` skips every event (gdkdevicemanager-core-x11.c `set_user_time` — "If an event doesn't have a valid timestamp, we shouldn't use it") and GTK multi-click detection (time-delta comparisons, e.g. gtkgesturemultipress) degenerates: `0 - 0 = 0` is always within the double-click interval → every click becomes a double/triple click. Legal on the wire (no protocol error — matches your "no X errors" log) but behaviorally harmful.
6. **Always `is_hint=0`** on MotionNotify; do not set `PointerMotionHintMask` bits on behalf of clients and do not compress motion into hint mode. GDK's hint path forces one XQueryPointer round-trip per motion (gdkevents.c:1844-1858), which massively amplifies your request storm during interaction.
7. **Enter/LeaveNotify during grabs**: emit the enterleave.c-style NotifyGrab/NotifyUngrab crossing chains with correct `detail` and valid root coords, `same_screen`, and `focus` flags; make sure `translate_notify_type()`'s `g_assert_not_reached()` set (Ancestor/Virtual/Inferior/Nonlinear/NonlinearVirtual) is never violated — a detail outside that set aborts the GTK process (devicemanager-core-x11.c `translate_notify_type`).
8. **ConfigureNotify**: for toplevels, non-synthetic ConfigureNotify must carry correct `width/height/border_width` and the event-vs-window field distinction (gdkdisplay-x11.c:975-1012 discards child/root forms; non-synthetic ones trigger the XTranslateCoordinates origin refresh and un-stick `_gdk_x11_moveresize_configure_done`, gdkdisplay-x11.c:1030-1036).

## Verification checklist for your server (fastest path to root cause)
1. During frozen phase, log `XQueryPointer` (window, child_return) — look for non-None child forever (§a) → fix #1.
2. Log every event you send to the client in the 10 s before PutImage collapses — look for an UnmapNotify without a following resolvable MapNotify, or a ButtonRelease (check state mask, send_event bit, xany.window) (§b, §c).
3. Grep your `XGrabPointer` handler: confirm you auto-deactivate the grab when all buttons are released, and that grab `event_mask` includes ButtonRelease delivery to grab windows.

## Source index
- gdk/x11/gdkdevice-core-x11.c:251-322 (query_state), 423-565 (window_at_position descent loop), 355-357 (hint-strip in grab)
- gdk/x11/gdkdevicemanager-core-x11.c: translate_event (ButtonRelease drop on window==NULL), translate_notify_type assert, set_user_time, `_gdk_device_manager_core_handle_focus`
- gdk/x11/gdkwindow-x11.c:1198-1199 (creation freeze), 3210-3232 (get_root_coords/XTranslateCoordinates), 454-461 (frame-sync freeze)
- gdk/x11/gdkdisplay-x11.c:857-899 (UnmapNotify freeze), 901-921 (MapNotify thaw), 996-1006 (ConfigureNotify translate), 1030-1036 (moveresize), 1341-1397 (_NET_WM_FRAME_DRAWN filter thaw), 1693-1708 (trusted-client XQueryPointer probe)
- gdk/gdkwindow.c:4710-4782 (freeze/thaw updates), 903-989 (synthesize_crossing_events), 991-1009 (get_current_toplevel), 9501-9558 (implicit grab on ButtonPress), 9377-9396 (motion-hint arming), 10077-10105 (implicit grab end conditions), 6830-6884 (get_origin → get_root_coords)
- gdk/gdkframeclockidle.c: RUN_PAINT_IDLE/RUN_FLUSH_IDLE macros, freeze/thaw, paint_idle
- gdk/gdkevents.c:1844-1858 (gdk_event_request_motions); gdk/gdkdisplay.c:903-1009; gtk/gtkmain.c:1469-1560 (rewrite_events_translate / grab rewriting); gtk/gtkwidget.c:7531-7536; gtk/gdkseatdefault.c grab pair
- X.org dix/events.c (`DeactivatePointerGrab`, `AllowSomeClient` — core grabs are Async; AllowEvents not used by GTK), dix/enterleave.c (NotifyGrab/NotifyUngrab crossing chains). Repository: https://gitlab.freedesktop.org/xorg/xserver (exact line numbers not re-verified — line numbers [UNCONFIRMED], function names confirmed).
