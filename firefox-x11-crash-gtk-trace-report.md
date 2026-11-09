# Code-archaeology report: GTK3 request sequence during firefox startup on a custom X11 server

Trace under analysis (SSH X11 forwarding, custom Rust X server, no WM/compositor):

```
seq56 QueryFont
seq57 GrabServer          (36)
seq58 InternAtom
seq59 GetSelectionOwner   (23) → owner = 0
seq60 GetWindowAttributes(window=0)   → BadWindow
seq61 GetGeometry(0)                  → BadDrawable
seq62 QueryTree(0)                    → BadWindow
seq63 ChangeWindowAttributes(0)       → BadWindow
```

All GTK code below was read from the **gtk-3-24** branch raw files (the GTK3 version firefox 79 links):
<https://gitlab.gnome.org/GNOME/gtk/-/raw/gtk-3-24/...>

---

## (a) Identified function(s)

### 1. `InternAtom` + `GetSelectionOwner` → owner=0 — **HIGH confidence: the compositing-manager check**

`gdk/x11/gdkscreen-x11.c`, function **`_gdk_x11_screen_setup()` → `check_is_composited()` → `get_cm_atom()`**
(raw: <https://gitlab.gnome.org/GNOME/gtk/-/raw/gtk-3-24/gdk/x11/gdkscreen-x11.c>)

```c
static Atom
get_cm_atom (GdkX11Screen *x11_screen)
{
  return _gdk_x11_get_xatom_for_display_printf (x11_screen->display, "_NET_WM_CM_S%d", x11_screen->screen_num);
}   // ← XInternAtom("_NET_WM_CM_S0") on first use  →  seq58

static gboolean
check_is_composited (GdkDisplay *display, GdkX11Screen *x11_screen)
{
  Window xwindow;
  xwindow = XGetSelectionOwner (GDK_DISPLAY_XDISPLAY (display), get_cm_atom (x11_screen));
  return xwindow != None;   // ← seq59, owner==0 → is_composited = FALSE
}

void
_gdk_x11_screen_setup (GdkScreen *screen)
{
  gdk_display_request_selection_notification (x11_screen->display, ... get_cm_atom (x11_screen));  // XFixes, if present
  x11_screen->is_composited = check_is_composited (x11_screen->display, x11_screen);
}
```

`_gdk_x11_screen_setup` runs **exactly once at display/screen startup** — matches "during display startup".
`owner == 0` is the *expected* value here; the function's entire purpose is to detect "no compositor". It
cannot fail on the reply itself (GetSelectionOwner never errors) and it does **not** use the returned window.

Equivalent alternative for seq58/59 (same request pattern, later in time): **`gdk_x11_display_supports_clipboard_persistence()`**
in `gdk/x11/gdkdisplay-x11.c` — `XInternAtom("CLIPBOARD_MANAGER")` + `XGetSelectionOwner`, called from
`gtk_clipboard_real_set_can_store` (gtk/gtkclipboard.c) when firefox claims CLIPBOARD and offers SAVE_TARGETS:

```c
static gboolean
gdk_x11_display_supports_clipboard_persistence (GdkDisplay *display)
{
  Atom clipboard_manager;
  clipboard_manager = gdk_x11_get_xatom_by_name_for_display (display, "CLIPBOARD_MANAGER"); // InternAtom 1st time
  return XGetSelectionOwner (GDK_X11_DISPLAY (display)->xdisplay, clipboard_manager) != None;
}
```

Both candidates tolerate `owner == 0` perfectly — **neither queries window 0**.

### 2. `GrabServer` (seq57) — `gdk_x11_display_grab()`

`gdk/x11/gdkdisplay-x11.c` (~line 2019–2031 in the file):

```c
void
gdk_x11_display_grab (GdkDisplay *display)
{
  ...
  if (display_x11->grab_count == 0)
    XGrabServer (display_x11->xdisplay);
  display_x11->grab_count++;
}
```

Important negative result: in GTK 3.24, **`gdk/x11/gdkselection-x11.c` no longer contains any `XGrabServer`**
(verified; `_gdk_x11_display_set_selection_owner` is just `XSetSelectionOwner` + `XGetSelectionOwner`). So the
"GrabServer around clipboard ownership" folklore from GTK2 does **not** apply to firefox-79-era GTK3.
Remaining producers of GrabServer (ranked):

* `gdk_x11_display_grab()` public API (above). Nested-count based.
* Firefox's own X remote client, `widget/xremoteclient/XRemoteClient.cpp` (`FindBestWindow` grabs the server
  while scanning the window tree for `_MOZILLA_VERSION`-carrying windows). Relevant only if the session
  attempted a remote command handoff (`firefox <url>` while another instance seemed present). I could not
  fetch this file for the exact FIREFOX_79_0_BUILD1 tag (path had moved by that release); verify locally.
* An **older GTK 3.24.x patch level is NOT a candidate** — the selection rewrite predates 3.24.0.

### 3. The four `window=0` queries (seq60–63) — the actual crash source. Candidates, ranked:

The entire gdkwindow-x11.c contains only two `XGetWindowAttributes` call sites, and only these GTK spots
issue Attrs/Geom/Tree/ChangeWindowAttributes. **No single GTK function issues all four in that order**, and
every candidate that can see `window=0` is error-trapped. Ranked candidates:

**(1) `gdk_x11_window_foreign_new_for_display(display, 0)` — most likely origin of Attrs+Tree, ~medium confidence**

`gdk/x11/gdkwindow-x11.c` lines 1240–1328:

```c
gdk_x11_display_error_trap_push (display);
result = XGetWindowAttributes (display_x11->xdisplay, window, &attrs);      // ← seq60
if (gdk_x11_display_error_trap_pop (display) || !result) return NULL;
...
gdk_x11_display_error_trap_push (display);
result = XQueryTree (display_x11->xdisplay, window, &root, &parent, &children, &nchildren);  // ← seq62
if (gdk_x11_display_error_trap_pop (display) || !result) return NULL;
```

Callers that could pass 0: `gdk_x11_display_translate_event()` in gdkdisplay-x11.c wraps
`xevent->xselectionrequest.requestor` and XFixes `selection_notify->owner` into foreign windows — both are
guarded by `!= None`, so window 0 only reaches it if **your event payloads carry owner/requestor = 0 with a
None-vs-zero inconsistency**, or if some firefox/GTK code passed a stored `Window` that is 0. Note the trap
pattern here: `pop()` (with code) → **XSync** → this path is designed to survive BadWindow.

**(2) `gdk_x11_window_get_frame_extents()` fallback parent-walk — Geom+Tree (and repeated)**

`gdk/x11/gdkwindow-x11.c` (~line 3240–3372). After `_NET_FRAME_EXTENTS`/`_NET_VIRTUAL_ROOTS` fail (they
won't even be tried without a WM, since `gdk_x11_screen_supports_net_wm_hint` returns FALSE when
`wmspec_check_window == None`), it walks up the tree with `XGetGeometry` + `XQueryTree`. The **whole function
runs inside a single `gdk_x11_display_error_trap_push/pop_ignored`**, so errors here are swallowed.
`gtk_window_get_frame_extents`/firefox `nsWindow::GetScreenBounds` go through this.

**(3) Untrapped per-window helpers invoked on a GdkWindow whose `impl->xid == 0` — the killer**

* `gdk_window_x11_get_geometry()` (gdkwindow-x11.c ~line 3218): bare `XGetGeometry(..., GDK_WINDOW_XID(window), ...)` — **no error trap**. ← matches seq61 BadDrawable.
* `gdk_window_set_events()` → X11 impl → `XSelectInput(...)` — **no error trap**. ← matches seq63 ChangeWindowAttributes.
* `gdk_x11_screen_setup`/`init_randr_support` style `XSelectInput` on a stored window id.

If a GdkWindow was constructed with XID 0 (e.g., a foreign wrapper created from a `Window` that your server
reported as 0, or a failed-window path in firefox's `nsWindow`), each of these issues a raw request on 0 and
**none of them is trapped**. On X.org this branch is not taken because the preceding data differs (see (c));
on your server it is taken, and the untrapped BadDrawable/BadWindow kills the process (mechanism in (e)).

**(4) `fetch_net_wm_check_window()` (your question 3) — only if `_NET_SUPPORTING_WM_CHECK` misbehaves**

`gdk/x11/gdkscreen-x11.c`:

```c
static Window
get_net_supporting_wm_check (GdkX11Screen *screen, Window window)
{
  gdk_x11_display_error_trap_push (display);
  XGetWindowProperty (screen->xdisplay, window,
                      gdk_x11_get_xatom_by_name_for_display (display, "_NET_SUPPORTING_WM_CHECK"),
                      0, G_MAXLONG, False, XA_WINDOW, &type, &format, &n_items, &bytes_after, &data);
  gdk_x11_display_error_trap_pop_ignored (display);

  if (type == XA_WINDOW)
    value = *(Window *)data;   // ⚠ dereferences data even when n_items == 0 (data == NULL)!
  ...
}

static void
fetch_net_wm_check_window (GdkScreen *screen)
{
  window = get_net_supporting_wm_check (x11_screen, x11_screen->xroot_window);
  if (window == None) return;                       // absent property → "no WM", clean exit
  if (window != get_net_supporting_wm_check (x11_screen, window)) return;
  gdk_x11_display_error_trap_push (display);
  XSelectInput (x11_screen->xdisplay, window, StructureNotifyMask);   // ← a ChangeWindowAttributes candidate
  error = gdk_x11_display_error_trap_pop (display);
  ...
}
```

Answers to your Q3 directly: **if the root property is absent (type=None), GTK3 queries nothing further and
concludes "no WM"** — it does *not* query window 0. It queries `wm_window` only if your GetProperty returned
`type == XA_WINDOW` with a payload; and then the XSelectInput on that window **is trapped**. Two concrete
server-bug hazards to check:

* **Never return `type=XA_WINDOW` with `n_items=0`** — `*(Window*)data` then dereferences NULL → SIGSEGV, not an X error.
* If you return `type=XA_WINDOW` with value 0, `value=0` → `window==None` → clean "no WM". OK.

### 4. `QueryFont` (seq56)

No GTK3 code in the files examined issues core `QueryFont` (GTK3/cairo/pango use XRender/Xft/cairo text).
Most plausible sources: a legacy `XLoadQueryFont/XQueryFont` user such as libX11's XIM/Xmb layer
(<https://gitlab.freedesktop.org/xorg/lib/libx11/-/raw/master/modules/im/ximcp/imDefIm.c> — note
`_XimPreConnect()` also does `XInternAtom(XIM_SERVERS)` + `XGetWindowProperty(root)` + `XGetSelectionOwner`,
an alternative producer of seq58/59 if GTK_IM_MODULE resolves to XIM), or firefox's own font code. Low
priority: QueryFont on a valid font never errors.

---

## (b) Key code excerpts

Already inline above. Additional trap machinery, `gdk/x11/gdkdisplay-x11.c`:

```c
struct _GdkErrorTrap {
  gulong start_sequence;   /* first sequence to ignore        */
  gulong end_sequence;     /* first sequence NOT to ignore; 0 = still active */
  int error_code;
};

void
gdk_x11_display_error_trap_push (GdkDisplay *display)
{
  _gdk_x11_error_handler_push ();                    /* install swallowing handler */
  trap->start_sequence = XNextRequest (display_x11->xdisplay);
  ...
}

static gint
gdk_x11_display_error_trap_pop_internal (GdkDisplay *display, gboolean need_code)
{
  ...
  if (need_code) {
      next_sequence = XNextRequest (...); processed_sequence = XLastKnownRequestProcessed (...);
      if ((next_sequence - 1) != processed_sequence)
          XSync (display_x11->xdisplay, False);      /* only when needed */
      result = trap->error_code;
  } else result = Success;                            /* pop_ignored: NO XSync at all */
  trap->end_sequence = XNextRequest (display_x11->xdisplay);
  _gdk_x11_error_handler_pop ();
  ...
}

void
_gdk_x11_display_error_event (GdkDisplay *display, XErrorEvent *error)
{
  for (trap in display_x11->error_traps)
      if (SEQUENCE_COMPARE (trap->start_sequence, <=, error->serial) &&
          (trap->end_sequence == 0 ||
           SEQUENCE_COMPARE (trap->end_sequence, >, error->serial)))
        { ignore = TRUE; trap->error_code = error->error_code; break; }

  if (!ignore) {
      msg = g_strdup_printf ("The program '%s' received an X Window System error...serial %ld error_code %d request_code %d...", ...);
      g_warning ("%s", msg);
      _exit (1);            /* ← the firefox "crash": GTK hard-exits on untrapped X errors */
  }
}
```

## (c) Semantic contract — what replies/errors this code tolerates

* `GetSelectionOwner(_NET_WM_CM_S0)` → **owner must be None/0** → `is_composited = FALSE`, continue. Any
  non-zero owner flips GDK into composited mode (rgba visual activation, `gdk_screen_is_composited()==TRUE`,
  compositor-dependent painting).
* `GetSelectionOwner(CLIPBOARD_MANAGER)` → None → `gtk_clipboard_set_can_store` becomes a no-op; non-None
  → firefox expects SAVE_TARGETS round-trips to work.
* Root `GetProperty(_NET_SUPPORTING_WM_CHECK)` → **type must be None** (or XA_WINDOW value None) → "no WM".
  A valid WM is concluded only when the root value is a window W *and* the same property on W returns W.
* GetProperty contract: absent property ⇒ `Success`, `type=None`, `n_items=0`, no data. Never
  `type=XA_WINDOW` with `n_items=0` (NULL deref in `get_net_supporting_wm_check`).
* XIM (if in play): root `GetProperty(XIM_SERVERS)` absent/type-mismatch → `_XimPreConnect` returns False,
  no XIM server, GTK falls back to simple IM. Tolerant.
* `XGetWindowAttributes/XGetGeometry/XQueryTree` on nonexistent windows must return **BadWindow /
  BadDrawable / BadWindow (error codes 3/9/3)** — GTK traps tolerate them, but only if attributed correctly (e).

## (d) What your server must return for graceful "no WM, no compositor"

1. `GetSelectionOwner` for `_NET_WM_CM_S0`, `CLIPBOARD_MANAGER`, `XIM_SERVERS`, and any selection → reply
   `owner = 0 (None)` unless a client actually owns it. All three startup checks then conclude "absent" and
   continue normally.
2. Root-window GetProperty for `_NET_SUPPORTING_WM_CHECK`, `_NET_SUPPORTED`, `_NET_VIRTUAL_ROOTS`,
   `_NET_FRAME_EXTENTS`, `_NET_WORKAREA`, XSETTINGS etc. → property **absent** (type None). GTK then uses
   the no-WM paths: `XSetInputFocus` (trapped) instead of `_NET_ACTIVE_WINDOW` client messages, frame-extents
   tree-walk fallback, frame-clock not throttled by `_NET_WM_FRAME_DRAWN`.
3. Never fabricate window 0: never return 0 as a "Window" payload (owner fields in events, property values,
   requestor in SelectionRequest events you synthesize). If you synthesize events (XFixes SelectionNotify,
   PropertyNotify), owner/requestor must be a real XID or None — and None must be *exactly* 0.
4. X errors: exact error code (BadWindow=3, BadDrawable=9, BadValue=2, BadMatch=8), `error.sequence` = the
   exact wire sequence number of the failing request, delivered **before the reply of any later request**.

## (e) Trap mechanism analysis (your question 5)

Mechanism (GTK3, `gdk/x11/gdkdisplay-x11.c`):
`gdk_x11_display_error_trap_push` records `start_sequence = XNextRequest(dpy)` and installs an Xlib error
handler that swallows all errors. `..._pop_ignored` records `end_sequence = XNextRequest(dpy)` **without any
XSync**; `..._pop` (the code-returning variant) `XSync(dpy, False)` **only if** `XNextRequest-1 !=
XLastKnownRequestProcessed` (i.e., it believes there are unacknowledged requests). Errors are attributed in
`_gdk_x11_display_error_event` **purely by the `serial` field of the XErrorEvent**, matched against
`start_sequence <= serial < end_sequence` for any live trap (traps stay in the list until their
end_sequence is ≤ `XLastKnownRequestProcessed`, so late-arriving in-range errors are still swallowed). An
error that matches no trap prints the classic message and calls **`_exit(1)`** (non-debug builds) — that is
your process death, not a signal crash.

Escape scenarios — how an error on your server can escape a trap that would have caught it on X.org:

1. **Wrong sequence numbers in the error packet (the decisive one).** Attribution uses only
   `error->serial`. If your Rust server assigns the error a sequence that is not exactly the failing
   request's sequence (off-by-one, server-global instead of per-client counter, sequence reuse after
   wrap, or replaying an old serial), the error falls outside every trap range → `_exit(1)`. A real X.org
   server guarantees `serial == request sequence`, so the same GTK code never mis-attributes.
2. **Stale/replayed errors.** An error delivered for a request that was issued *before* any trap was pushed
   carries a serial < `start_sequence` of the current trap → escapes. If your server queues/duplicates
   BadWindow errors (e.g., re-sends an error after re-reading its request queue), this kills the client.
3. **Timing does NOT escape the trap.** Because attribution is serial-based and popped traps are retained
   until provably outdated, even an error arriving after `pop_ignored` (no XSync!) is ignored if its serial
   is in range. Conversely, `pop_ignored` never syncing means GTK *relies* on the server delivering errors
   before replies of later requests — if your server sends the seq61 BadDrawable reply-or-error out of order
   relative to later replies, Xlib's ordering assumptions break and errors can be processed at surprising
   points, but they still only kill the process if their serial is wrong or no trap is active.
4. **Truly untrapped paths die immediately on X.org too.** The difference is *which branch runs*: on X.org
   the no-WM checks above return None and the `window=0` requests are never generated at all. On your
   server something handed the client a window id of 0 (or an event with owner/requestor 0-with-nonzero
   semantics), routing execution into raw, untrapped helpers like `gdk_window_x11_get_geometry` (XGetGeometry,
   no trap) — that's the "on real X.org it does not crash" asymmetry, not a trap failure.

**Debugging tip:** set `GDK_SYNCHRONIZE=1` for firefox on your server; GDK then syncs after every request and
the failing request appears with a meaningful backtrace, which will pinpoint which of the candidates in (a).3
is actually executing.

## Sources read (all fetched verbatim)

* GTK gtk-3-24 `gdk/x11/gdkscreen-x11.c` — <https://gitlab.gnome.org/GNOME/gtk/-/raw/gtk-3-24/gdk/x11/gdkscreen-x11.c>
* GTK gtk-3-24 `gdk/x11/gdkdisplay-x11.c` — <https://gitlab.gnome.org/GNOME/gtk/-/raw/gtk-3-24/gdk/x11/gdkdisplay-x11.c>
* GTK gtk-3-24 `gdk/x11/gdkwindow-x11.c` — <https://gitlab.gnome.org/GNOME/gtk/-/raw/gtk-3-24/gdk/x11/gdkwindow-x11.c>
* GTK gtk-3-24 `gdk/x11/gdkselection-x11.c` — <https://gitlab.gnome.org/GNOME/gtk/-/raw/gtk-3-24/gdk/x11/gdkselection-x11.c>
* GTK gtk-3-24 `gtk/gtkclipboard.c` — <https://gitlab.gnome.org/GNOME/gtk/-/raw/gtk-3-24/gtk/gtkclipboard.c>
* GTK gtk-3-24 `gtk/gtkselection.c` — <https://gitlab.gnome.org/GNOME/gtk/-/raw/gtk-3-24/gtk/gtkselection.c>
* libX11 ximcp `imDefIm.c`, `imInsClbk.c` — <https://gitlab.freedesktop.org/xorg/lib/libx11/-/raw/master/modules/im/ximcp/imDefIm.c>,
  <https://gitlab.freedesktop.org/xorg/lib/libx11/-/raw/master/modules/im/ximcp/imInsClbk.c>
* Firefox 79 `widget/xremoteclient/XRemoteClient.cpp` — could not be fetched at tag FIREFOX_79_0_BUILD1
  (path removed/moved); check your unpacked source locally before ruling the remote-client GrabServer in/out.

## Uncertainty statement

The `InternAtom + GetSelectionOwner` pair and the trap mechanism are pinned down with high confidence.
The **`GrabServer` and the four window-0 requests are not all from one identified function**; the report
lists every plausible producer with the reasoning above. Highest-value next steps: (1) log the atom name
your server interns at seq58 (tells you whether it is `_NET_WM_CM_S0`, `CLIPBOARD_MANAGER`, or
`XIM_SERVERS`, disambiguating candidates 1/2/XIM instantly); (2) run firefox with `GDK_SYNCHRONIZE=1`;
(3) grep your firefox source tree (`widget/xremoteclient/XRemoteClient.cpp`) for `XGrabServer` and
`FindBestWindow` to check the remote-client GrabServer path.
