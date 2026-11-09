# Minimal viable XInput2 for a from-scratch Rust X server (GTK3 polling elimination)

Research deliverable. All GDK quotes are from the `gtk-3-24` branch (fetched 2026-11-08). Unverified items are marked **[unconfirmed]**.

## 1. What GDK3 actually calls (gtk-3-24, gdk/x11)

### 1.1 Display init (decision to use XI2 at all)

`gdkdevicemanager-x11.c: _gdk_x11_device_manager_new()`
<https://gitlab.gnome.org/GNOME/gtk/-/raw/gtk-3-24/gdk/x11/gdkdevicemanager-x11.c>

- Skipped entirely if `GDK_CORE_DEVICE_EVENTS` is set in the env.
- `XQueryExtension("XInputExtension", &opcode, &firstevent, &firsterror)` — client just needs the major opcode (over the wire this was already part of the client's initial connection setup; GDK uses Xlib which queries extensions via `XQueryExtension`/`QueryExtension` requests — our server must already list the extension).
- `XIQueryVersion(dpy, major=2, minor=4)` — **if this returns BadRequest, GDK falls back to the core device manager → polling resumes**. This is the single switch. GDK accepts whatever (major, minor) the server returns; server can reply 2.0–2.4.

### 1.2 Every XI2 request GDK3 sends, with exact call site

From `gdkdevicemanager-xi2.c`, `gdkdevice-xi2.c`, `gdkdisplay-x11.c`:

| Request (XI opcode) | libXi call | Where / when | Mandatory? |
|---|---|---|---|
| 47 XIQueryVersion | `XIQueryVersion` | `_gdk_x11_device_manager_new` | **Yes** — failure ⇒ core fallback |
| 48 XIQueryDevice | `XIQueryDevice(XIAllDevices)` | `gdk_x11_device_manager_xi2_constructed()` — enumerate hierarchy at init | **Yes** (untrapped; failure breaks construction) |
| 48 XIQueryDevice | `XIQueryDevice(device_id)` | `gdk_x11_device_manager_xi2_constructed` HierarchyChanged/DeviceChanged handlers; `gdk_x11_device_xi2_get_state()` | Must implement |
| 46 XISelectEvents | `XISelectEvents` | (a) `constructed()`: root window, `XIAllDevices`, mask {HierarchyChanged, DeviceChanged, PropertyEvent}; (b) `gdk_x11_device_manager_xi2_select_window_events()`: every window whose GDK mask includes input events, deviceid `XIAllMasterDevices`; (c) `gdk_x11_device_xi2_select_window_events()` | **Yes** (a) is untrapped at init |
| 40 XIQueryPointer | `XIQueryPointer` | `gdk_x11_device_xi2_query_state()`, `window_at_position()` (initial cursor pos, DND, `get_state`) | Should implement; call sites are error-trapped and have fallbacks, but ignoring them permanently degrades DND/positioning |
| 51 XIGrabDevice | `XIGrabDevice` | `gdk_x11_device_xi2_grab()` — GTK popup/menubar/DND grabs | **Yes** if you want clicks on menus to behave; maps onto your existing grab machinery |
| 52 XIUngrabDevice | `XIUngrabDevice` | `gdk_x11_device_xi2_ungrab()` | Yes (paired) |
| 45 XIGetClientPointer | `XIGetClientPointer(None, &id)` | `gdk_x11_display_get_default_seat()` (gdkdisplay-x11.c) — only reached when >1 seat exists; normally short-circuits. Error-trapped. | Cheap to implement; optional in practice |
| 41 XIWarpPointer | `XIWarpPointer` | `gdk_x11_device_xi2_warp()` (gdk_device_warp) | Optional; error-trapped via generic X error path |
| 42 XIChangeCursor / 43? | `XIDefineCursor`/`XIUndefineCursor` (X_XIChangeCursor=42) | `gdk_x11_device_xi2_set_window_cursor()` — per-window cursor changes on the **master pointer id** | Needed for normal cursor feedback |
| 56 XIListProperties | — | **not called** by GDK core paths | Not needed |
| 53 XIAllowEvents | — | GDK uses `GrabModeAsync`; no XIAllowEvents calls in these files | Not needed (implement as success/no-op) |
| 59 XIGetProperty | `XIGetProperty` for atoms `"Device Product ID"`, `"libinput Tapping Enabled"` | `get_device_ids()`, `is_touchpad_device()` during device creation — both fully error-trapped and treat failure as "not a touchpad / no ids" | Return "not found" (type=None) is fine |
| 54/55 XIPassiveGrab/Ungrab | — | not called by GDK core X11 backend (GTK4 uses it; GTK3 not in these files) | Not needed for firefox-79 |

**No `xi_queryDevicePointer`** — that request does not exist; the pointer-position request is `XIQueryPointer` (opcode 40).

### 1.3 Graceful degradation if XIQueryVersion succeeds but sub-requests fail

- Everything *after* version negotiation that GDK wraps in `gdk_x11_display_error_trap_push/pop_ignored` degrades silently (XIGetProperty, XIGetClientPointer, XIQueryPointer in some paths).
- `XIQueryDevice(XIAllDevices)` and the init-time `XISelectEvents` on the root window are **not** error-trapped; an X error there is fatal for the app (gdk error handler `_exit(1)`). These two must be correct.
- No request GDK sends is optional in the sense of "may return X error at init": so implement XIQueryVersion, XIQueryDevice, XISelectEvents properly, and make everything else either work or fail safely in trapped paths.

### 1.4 Device hierarchy GDK expects

`gdkdevicemanager-xi2.c: constructed()`:
- `XIQueryDevice(XIAllDevices)` — GDK iterates all devices, skips `enabled == FALSE`, classifies by `use` (XIMasterPointer/XIMasterKeyboard/XISlavePointer/XISlaveKeyboard/XIFloatingSlave) and pairs masters via `attachment`.
- `VIRTUAL_CORE_POINTER_ID = 2`, `VIRTUAL_CORE_KEYBOARD_ID = 3` — hardcoded in gdkdevicemanager-x11.c, confirming VCP deviceid **2**, VCK deviceid **3**.
- Event selection always uses pseudo-devices: `XIAllDevices = 0` (root-window hierarchy masks), `XIAllMasterDevices = 1` (all per-window input masks, incl. grabs in `select_window_events`). Events themselves must arrive with `deviceid = 2` (pointer) / `3` (keyboard) and `sourceid` = the generating device; GDK looks devices up by **both** ids in its table. **Minimum viable hierarchy: exactly 2 enabled devices** — master pointer (id 2, attachment 3, ButtonClass + 2 relative ValuatorClasses) and master keyboard (id 3, attachment 2, KeyClass with keycode list). Zero slave devices works; GDK tolerates it (sourceid == deviceid lookup misses are tolerated: `gdk_event_set_source_device(event, NULL)` is harmless). Optionally add one fake slave pointer (e.g. "Virtual X11 pointer", attached to 2) to satisfy any XI1-era expectations — not required.

Practical class layout for VCP:
- ButtonClass: `num_buttons = 5..7` (4/5 = wheel, 6/7 = horiz wheel) + button-name atoms (can be None/0).
- 2 ValuatorClasses: relative mode, numbers 0/1, label atoms "Rel X"/"Rel Y" (**or no labels** — labels absent ⇒ GDK_AXIS_IGNORE, fine for a mouse). Real X.Org gives relative axes min=-1/max=-1 **[unconfirmed — verify against a real Xorg with x11rb client]**; GDK only uses min/max for absolute axes.
- No ScrollClass ⇒ GDK gets scroll via button 4/5 XI_ButtonPress events (classic wheel), and smooth-scroll code is simply never triggered. Recommended for a minimal server.

VCK class: KeyClass listing all keycodes in your keymap (e.g. 8..255) — GDK uses it for `gdk_device_set_key` bookkeeping only.

### 1.5 Runtime event types GDK translates (devicemanager `translate_event`)

XI_Motion, XI_ButtonPress, XI_ButtonRelease, XI_KeyPress, XI_KeyRelease, XI_Enter, XI_Leave, XI_FocusIn, XI_FocusOut, XI_HierarchyChanged, XI_DeviceChanged, XI_PropertyEvent; touch/gesture only if server minor ≥2/≥4 (runtime-checked). **Replying version 2.0 is the easiest way to shrink the surface** — GDK then never selects Touch*/Gesture* masks.

## 2. Wire format (from xorgproto)

Sources (raw, current master):
- Request/opcode numbers, req/reply/event structs: <https://gitlab.freedesktop.org/xorg/proto/xorgproto/-/raw/master/include/X11/extensions/XI2proto.h>
- Event-type codes, device-type/class codes, mask macros: <https://gitlab.freedesktop.org/xorg/proto/xorgproto/-/raw/master/include/X11/extensions/XI2.h>
- Full prose spec incl. delivery rules: <https://gitlab.freedesktop.org/xorg/proto/xorgproto/-/raw/master/specs/XI2proto.txt>

### 2.1 Correction to the question's premise: event-type codes

**XI_Motion = 6, not 15.** Verified from XI2.h:

```
XI_DeviceChanged 1   XI_KeyPress 2    XI_KeyRelease 3   XI_ButtonPress 4
XI_ButtonRelease 5   XI_Motion 6      XI_Enter 7        XI_Leave 8
XI_FocusIn 9         XI_FocusOut 10   XI_HierarchyChanged 11  XI_PropertyEvent 12
XI_RawKeyPress 13    XI_RawKeyRelease 14  XI_RawButtonPress 15  XI_RawButtonRelease 16
XI_RawMotion 17      XI_TouchBegin 18 ... XI_GestureSwipeEnd 32  XI_LASTEVENT=32
```
(15 is XI_RawButtonPress.) Masks: `XI_MotionMask = 1 << 6`, etc.; `XIMaskLen(e) = (e>>3)+1`.

Other ids: `XIAllDevices=0`, `XIAllMasterDevices=1`; `XIMasterPointer=1, XIMasterKeyboard=2, XISlavePointer=3, XISlaveKeyboard=4, XIFloatingSlave=5`; classes `XIKeyClass=0, XIButtonClass=1, XIValuatorClass=2, XIScrollClass=3`; request opcodes `X_XIQueryPointer=40, X_XISelectEvents=46, X_XIQueryVersion=47, X_XIQueryDevice=48, X_XIGrabDevice=51, X_XIUngrabDevice=52, X_XIAllowEvents=53, X_XIGetClientPointer=45, X_XIChangeCursor=42, X_XIWarpPointer=41, X_XIListProperties=56, X_XIGetProperty=59`.

### 2.2 Requests (reqType = your extension major opcode, ReqType = minor opcode above)

```
xXIQueryVersionReq (8B):   reqType u8, ReqType u8(47), length u16=2, major u16, minor u16
xXIQueryVersionReply (32B): repType u8=1(X_Reply), RepType u8(47), seq u16, length u32=0,
                            major u16, minor u16, 5×pad u32
xXIQueryDeviceReq (8B):     reqType, ReqType(48), length=2, deviceid u16, pad u16
xXIQueryDeviceReply:        repType=1, RepType=48, seq, length = trailing words,
                            num_devices u16, pad u16, 5×pad u32
                            then num_devices × xXIDeviceInfo:
  xXIDeviceInfo (12B+name+classes): deviceid u16, use u16, attachment u16, num_classes u16,
                            name_len u16, enabled u8, pad u8; name (padded to 4);
                            then num_classes × class infos:
  xXIAnyInfo hdr (8B): type u16, length u16 (words incl. hdr), sourceid u16, pad u16
  xXIButtonInfo: hdr + num_buttons, then button bitmask (padded to 4) + num_buttons × Atom
  xXIKeyInfo: hdr(type=0) with num_keycodes, then num_keycodes × CARD32 keycodes
  xXIValuatorInfo (32B+hdr=8 → total 40? struct is 8-byte hdr + 32B body): hdr(type=2,
                            length in words), number u16, label Atom, min FP3232(8B),
                            max FP3232, value FP3232, resolution u32, mode u8, pad1 u8, pad2 u16
xXISelectEventsReq (12B):   reqType, ReqType(46), length, win u32, num_masks u16, pad u16;
                            then num_masks × { xXIEventMask{deviceid u16, mask_len u16(words)}, mask bytes (padded to 4) }
xXIQueryPointerReq (12B):   reqType, ReqType(40), length=3, win u32, deviceid u16, pad u16
xXIQueryPointerReply (56B): repType=1, RepType=40, seq, length=13, root u32, child u32,
                            root_x FP1616, root_y, win_x, win_y, same_screen u8, pad u8,
                            buttons_len u16, mods{xXIModifierInfo 16B}, group{xXIGroupInfo 4B}
                            + trailing button-mask bytes (buttons_len words)
xXIGrabDeviceReq (24B):     reqType, ReqType(51), length=6, grab_window u32, time u32, cursor u32,
                            deviceid u16, grab_mode u8, paired_device_mode u8,
                            owner_events u8, pad u8, mask_len u16 (+ mask bytes)
xXIGrabDeviceReply (32B):   repType=1, RepType=51, seq, length=0, status u8, pad, u16 pad, 6×pad u32
xXIUngrabDeviceReq (12B):   reqType, ReqType(52), length=3, time u32, deviceid u16, pad u16
xXIAllowEventsReq (12B):    reqType, ReqType(53), length=3, time u32, deviceid u16, mode u8, pad u8
xXIGetClientPointerReq(8B): reqType, ReqType(45), length=2, win u32
xXIGetClientPointerReply:   set u8, pad, deviceid u16, pads
xXIChangeCursorReq (16B):   reqType, ReqType(42), length=4, win u32, cursor u32, deviceid u16, pad u16
xXIWarpPointerReq (36B):    reqType, ReqType(41), length=9, src_win, dst_win, src_x FP1616, src_y,
                            src_width u16, src_height u16, dst_x FP1616, dst_y, deviceid u16, pad u16
xXIListPropertiesReq (8B):  reqType, ReqType(56), length=2, deviceid u16, pad u16
```

FP1616 = i32 16.16 fixed point; FP3232 = {i32 integral, u32 frac}.

### 2.3 XI2 events: GenericEvent envelope + per-type body

All XI2 events are XGE GenericEvents: `response_type = 35 (GenericEvent)`, `extension` = XI major opcode, then:

```
xXIGenericDeviceEvent header (32B total):
  type u8=35, extension u8, sequenceNumber u16, length u32 (words AFTER the 32 bytes),
  evtype u16, deviceid u16, time u32
```

`xXIDeviceEvent` (used for XI_KeyPress/KeyRelease/ButtonPress/ButtonRelease/Motion, code 2–5, 6):

```
offset 32: detail u32            # keycode / button number; 0 for Motion
  36: root u32, 40: event u32, 44: child u32
  48: root_x FP1616, 52: root_y, 56: event_x, 60: event_y   # 16.16 fixed point
  64: buttons_len u16 (words), 66: valuators_len u16 (words), 68: sourceid u16, 70: pad u16
  72: flags u32                   # 0; XIPointerEmulated=1<<16
  76: mods: base/latched/locked/effective u32 ×4
  92: group: base/latched/locked/effective u8 ×4
  96: buttons bitmask (buttons_len × 4B)   # bit 1 set while button 1 held, etc.
      valuators bitmask (valuators_len × 4B)
      axisvalues: one FP3232 per set valuator bit, in valuator order
length = (everything after byte 32)/4.
```

`xXIEnterEvent` (XI_Enter=7, XI_Leave=8, XI_FocusIn=9, XI_FocusOut=10) — same layout as DeviceEvent through `child`, then at 32: `sourceid u16, mode u8, detail u8, root u32, event u32, child u32` (offset 36/40/44), then 48: root_x, root_y, event_x, event_y FP1616 ×4, 64: `same_screen u8 (BOOL), focus u8 (BOOL), buttons_len u16`, 68: mods 16B, group 4B, then trailing button-mask words. Note root/event/child positions deliberately match xXIDeviceEvent.

`xXIHierarchyEvent` (11): after header: flags u32, num_info u16, pad u16, pad u32 ×2 (event ends at 32B if you never emit it); `xXIHierarchyInfo` = deviceid u16, attachment u16, use u8, enabled u8, pad u16, flags u32 (16B each).

`xXIDeviceChangedEvent` (12+1): num_classes u16, sourceid u16, reason u8, pad…; `xXIPropertyEvent` (12): property Atom, what u8… Only needed if you actually mutate devices — for a static 2-device hierarchy you never send them, but GDK *selected* for them, so absence is fine.

## 3. Dual delivery / core-event suppression (the click-critical answer)

**The server must suppress core events when the client's XI2 selection covers that event on that window.** Evidence and mechanics:

- GDK does **not** remove core event masks when XI2 is active. The XI2 device manager *subclasses* `GDK_TYPE_X11_DEVICE_MANAGER_CORE` (`G_DEFINE_TYPE_WITH_CODE (GdkX11DeviceManagerXI2, gdk_x11_device_manager_xi2, GDK_TYPE_X11_DEVICE_MANAGER_CORE, …)`), and the core manager's `select_window_events`/`XSelectInput` path keeps setting the ordinary core event mask (ButtonPressMask | PointerMotionMask | …) on every window. `gdk_x11_device_manager_xi2_select_window_events()` additionally sends `XISelectEvents` with `deviceid = XIAllMasterDevices` for the same windows (gdkdevicemanager-xi2.c, end of file); `gdk_x11_device_xi2_select_window_events()` does the same for a specific master device id.
- The XI2 spec (§ "Event processing for attached slave devices", XI2proto.txt) defines the rule: event goes out as an XI event from the MD; "**If the event has been delivered, event processing stops. Otherwise, the event is delivered as a core event**". So a client that selected XI2 events on the event window gets **only XI2 events**; a client that didn't (xi型/old clients, xeyes, your test clients) still gets core events. GDK relies on this: if the server delivered *both*, GTK would see duplicate button/motion events (double clicks, double key handling) and break.
- Practical server rule: when delivering input events generated by a device attached to a master, for each (client, event-window) pair: if the client has any XI2 mask selection on the *delivery chain* that covers this event type (for deviceid XIAllMasterDevices/XIAllDevices or the actual master id), deliver the XI2 DeviceEvent and do **not** deliver the corresponding core event to that client. Otherwise deliver the core event as before. Enter/Leave/Focus follow the same rule (GDK handles XI_Enter/Leave/FocusIn/FocusOut and *also* still handles core-focus edge cases via `_gdk_device_manager_core_handle_focus` — the suppression rule prevents double delivery).
- Grabs: GDK grabs with `XIGrabDevice` (async, owner_events per grab) for popups/DND. While a GDK grab is active, XI events go to the grabbing client per the grab mask, and core events to that same client are suppressed per the same selection logic; your existing implicit-grab propagation should be reused for computing the XI event window (`event`, `child`) and coordinates. GDK also uses classic core `GrabPointer` in some legacy paths — both must coexist (XI2 spec: XI grabs and core grabs don't block each other except for modifier conflicts).

So: **deliver only XI2 to XI2-selecting clients, only core to others. Do not dual-deliver.**

## 4. VcXsrv / Xming / MobaXterm behavior

- MobaXterm embeds the Cygwin/X (XWin) X.Org server; VcXsrv is a standalone build of the same XWin codebase; Xming is built from an older X.Org snapshot. Upstream X.Org has shipped a complete XInput2 (dix + Xi + Xext/XGE) since server 1.7 (2009), so all three expose the full XI2 request/event surface to any client, including remote ones.
- **XI2 is not affected by SSH X11 forwarding at the protocol level**: the extension opcode negotiation, XIQueryVersion, XISelectEvents and GenericEvents all travel over the normal connection stream, so a client forwarded to a remote XWin gets normal pushed XI2 motion events. There is no known "XI2 disabled over forwarding" switch. **[unconfirmed — could not reach the VcXsrv/Xming issue trackers or changelogs; web search was unavailable in this session. Verify on their GitHub (VcXsrv issue search "XInput2") if it matters.]**
- Relevant quirk that *does* exist in GDK (and your server should handle): untrusted clients (`XSECURITY`-restricted) get `XIQueryPointer` denied; GDK has an explicit `!trusted_client` fallback in `gdk_x11_device_xi2_query_state()` / `window_at_position()` that creates 1×1 probe windows (gdkdevice-xi2.c). Normal SSH-forwarded clients are trusted, so this path is inactive — but if your server ever denies XIQueryPointer, GDK silently degrades rather than crashing. No `XGETCLIENTDATA`-style mechanism applies to XI2. **[Xming free-version XI2 status unconfirmed; paid current Xming claims XInput support in its changelog — verify manually.]**

## 5. Implementation plan for the Rust server

Scope: reply version **2.0** (minimal: no touch/gesture masks, GDK never selects them), 2 devices, no raw events, no scroll classes.

### (a) Exact minimal request/event set

Requests to implement (XI minor opcodes): **47 QueryVersion, 48 QueryDevice, 46 SelectEvents, 40 QueryPointer, 51 GrabDevice, 52 UngrabDevice, 42 ChangeCursor**, plus trivially **53 AllowEvents** (no-op success), **45 GetClientPointer** (return set=1, deviceid=2), **56 ListProperties** (empty), **59 GetProperty** (type=None, empty). All others (WarpPointer 41 optional, ChangeHierarchy 43, SetFocus 49, GetFocus 50, PassiveGrab 54/55, Change/DeleteProperty 57/58, GetSelectedEvents 60, Barrier 61) may return `Implementation` error or proper responses; GDK does not call them in the firefox-79 path.

Events to *send*: XI_Motion(6), XI_ButtonPress(4), XI_ButtonRelease(5), XI_KeyPress(2), XI_KeyRelease(3), XI_Enter(7), XI_Leave(8), XI_FocusIn(9), XI_FocusOut(10), and — never, but selectable — XI_HierarchyChanged(11)/DeviceChanged(1)/PropertyEvent(12) on the root window.

### (b) Steps

1. Register extension "XInputExtension" in the connection-setup extension list (major opcode X, first event = unused since all events are GenericEvents routed by opcode, first error = your XI error range for BadDevice/BadEvent/BadMode/BadClass/Invalid… — XI defines error codes 0..5 in XIproto; GDK only reacts to generic X errors).
2. XI2 wire structs in Rust: copy field tables from §2.2/§2.3; x11rb's `xinput` module already encodes exactly this wire format (client side) — use its definitions as the source of truth for offsets, don't hand-count.
3. Device model: `Device { id, use, attachment, enabled, classes }`; seed VCP(2)/VCK(3). Route your existing motion/button/key event generator through it: every generated event now carries (masterid, sourceid=masterid is acceptable — but see pitfall below).
4. Event selection state: per window, map deviceid-selector (0/1/actual ids) → byte mask; XISelectEvents *replaces* the mask for that deviceid+window (spec semantics).
5. Delivery: in your existing propagation pipeline, compute the event window/child as today; for each candidate client, if it has a covering XI2 selection (or an active XI grab) → serialize xXIDeviceEvent / xXIEnterEvent and skip core delivery; else core path as today.
6. Grabs: XIGrabDevice = your existing explicit grab with XI event mask; XIGrabDevice reply status codes 0=success,1=AlreadyGrabbed,2=InvalidTime,3=NotViewable,4=Frozen.
7. XIQueryPointer: your existing TranslateCoordinates+QueryPointer logic with FP1616 and mods/group/button state.

### (c) Pitfalls that break clicks/keyboard

1. **Forgetting core-event suppression** (§3) ⇒ every click counts twice. Biggest risk.
2. **FP1616 everywhere**: GDK divides event_x/root_x by window_scale as `gdouble` — forgetting the 16.16 shift gives garbage coordinates. `(v as f64) / 65536.0`.
3. **`length` is words-after-32** in GenericEvents, not total words, and the *client must be able to trust it*; libXi parses exactly `length` bytes. Off-by-one = truncated valuator/button lists.
4. **deviceid vs sourceid**: GDK looks up both in its device table and dereferences `source_device->last_tool` — if sourceid names a device not returned by XIQueryDevice, GTK may crash (`source_device` NULL deref in translate_event). Either set sourceid=2/3, or add the device to the hierarchy.
5. **Suppress core KeyPress too**: GDK translates XI_KeyPress via its keymap; double delivery = double characters.
6. Focus: after XI2, GDK expects XI_FocusIn/Out (it ignores core FocusIn once XI2 selected because of suppression). If you keep sending core FocusIn only, GTK's focus/user-time logic stalls (no `_NET_WM_USER_TIME` updates).
7. Enter/Leave: send XI_Enter on real crossing changes with correct `mode` (0 normal, 1 grab, 2 ungrab) and `detail` (XINotifyInferior=2 etc.) — GTK uses `detail != XINotifyInferior` on toplevels to reset scroll state and `set_user_time`.
8. XISelectEvents semantics: replace, not OR, for the given deviceid+window; num_masks may select several deviceids in one request (GDK sends 1 mask per request, but honor the general form).
9. Reply version: return `min(requested, 2.0)` (or 2.2); never a *higher* minor than requested; never 1.x. GDK asserts major==2.
10. Sequence numbers in GenericEvents: must be the sequence of the *last processed request*; GDK uses them for error-trap correlation.
11. Don't forget XIGetClientPointer is called with `win=None` — returning failure is tolerated (error-trapped), success with deviceid=2 is cleaner.

### (d) Effort & risk

| Item | Estimate |
|---|---|
| Wire structs + parse/reply for the 11 requests | 2–3 days (x11rb definitions as reference) |
| Device hierarchy + selection state | 1–2 days |
| Delivery routing + core suppression | 3–5 days (touches your hottest path; the riskiest part) |
| XIQueryPointer / grabs / cursor | 2–3 days |
| Firefox/GTK integration testing & quirks | 2–4 days |
| **Total** | **~1.5–3 weeks** for one engineer already owning the event pipeline |

Risks: (1) suppression logic regressions for non-XI2 clients; (2) grab interplay (core GrabPointer from legacy paths vs XIGrabDevice popups); (3) unconfirmed details marked above (valuator min/max for relative axes, VcXsrv/Xming SSH claims).

## 6. Test plan

**x11rb clients:**
1. `QueryVersion(2,4)` → expect (2,0); repeat with (2,0) → (2,0).
2. `QueryDevice(XIAllDevices)` → exactly 2 devices, ids 2/3, correct use/attachment; decode classes with the x11rb xinput module.
3. `SelectEvents(root, XIAllDevices, [HierarchyChanged|DeviceChanged|PropertyEvent])` and `SelectEvents(win, XIAllMasterDevices, [Motion|ButtonPress|ButtonRelease|Enter|Leave|KeyPress|KeyRelease|FocusIn|FocusOut])` → Success.
4. Move pointer / click / type over the window: assert you receive **XI events only** (no core ButtonPress/Motion on that window), correct `deviceid=2`, `sourceid`, FP1616 coords, button state bits, seq numbers monotonic.
5. Same test with a *second* client that only selects core masks → it must still receive core events (suppression is per-client).
6. `XIQueryPointer` round trip vs XTranslateCoordinates result; grab: `XIGrabDevice` then verify event routing during grab + `XIUngrabDevice`.
7. Negative: `QueryDevice(42)` → BadDevice error with your XI error code.

**firefox-79 on openSUSE over SSH:**
1. Repeat your measurement: XTranslateCoordinates/XQueryPointer counts must collapse to ~0/s steady state (expect a handful at startup only, from `window_at_position`).
2. Clicks: single click = single GDK_BUTTON_PRESS (check with `GDK_DEBUG=events firefox`); menus open on first click, no double-fire.
3. Keyboard: no doubled characters; focus rings follow clicks (XI_Enter + XI_FocusIn working).
4. Hover: tooltips appear promptly without polling lag; wheel scrolls (button 4/5 path since no ScrollClass).
5. DND (drag a link to the bookmark bar): XIGrabDevice path exercises popup grabs.
6. Cursor changes over links (XIChangeCursor on master pointer).
