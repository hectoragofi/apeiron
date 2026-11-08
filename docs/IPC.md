# Apeiron IPC (protocol version 1)

Apeiron serves a named pipe that other apps (first of all screenbuddy) use to
read the canvas and move its camera.

- Pipe: `\\.\pipe\apeiron-v1`, byte mode, **newline-delimited JSON** (UTF-8,
  one object per line, `\n` or `\r\n`). Lines over 64 KiB close the connection.
- Any number of clients at once. Each connection is served on its own
  threads with overlapped I/O (separate read and write operations), so the
  server can push events while it reads requests on the same connection;
  nothing ever blocks Apeiron's UI or engine threads. A client may send
  requests on a subscribed connection; replies and events interleave.
- Access: the current user (and SYSTEM) only. The pipe has an explicit DACL
  `D:P(A;;GA;;;<user SID>)(A;;GA;;;SY)` (the default named-pipe DACL would let
  Everyone open it for reading) and rejects remote clients. The first instance
  is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so Apeiron won't serve a
  name another process already holds.
- Started with Apeiron; not available while it isn't running. Clients should
  retry (screenbuddy retries every 5 s).

## Coordinates

- **Screen:** physical pixels of the virtual screen (the primary monitor's
  top-left is `0,0`).
- **Canvas:** physical pixels at zoom 1. A canvas point `(cx, cy)` is on
  screen at `((cx - camera.x) * zoom, (cy - camera.y) * zoom)`.
- Rects are `[x, y, w, h]`. Window rects are the window's *visible frame*
  (without the invisible resize borders Windows adds around most windows).
- `hwnd` is the window handle as a JSON integer.

## Requests and replies

```
→ {"id":1,"op":"hello"}
← {"id":1,"ok":true,"data":{"version":1,"canvas":true}}
← {"id":2,"ok":false,"error":"unknown window: 1234"}
```

`id` is echoed back as given (any JSON value; `null` if missing). Replies to
one connection can come out of order: requests that wait for the camera reply
when it settles, while later requests are answered meanwhile. Unknown ops,
bad JSON and missing/invalid fields get an `ok:false` reply.

| op | fields | reply `data` |
|---|---|---|
| `hello` | | `{version:1, canvas:bool}` |
| `get_layout` | | see below |
| `focus_window` | `hwnd` | `{}` once settled |
| `leave_stage` | | `{}` once settled |
| `fly_to` | `canvas_rect` | `{}` once settled |
| `back` | | `{}` once settled |
| `reserve_region` | `name`, `w`, `h` | `{canvas_rect}` |
| `place_window` | `hwnd`, `region` | `{canvas_rect}` |
| `subscribe` | | `{}`, then events |

"Once settled" means: no camera animation running, no fingers or mouse
steering the canvas, and, if the camera landed at 1:1, the real windows have
been placed and the overlay is gone. If that hasn't happened within 10 s (the
user took over with a gesture and stayed in the overview, say) the reply is
`ok:false`, `"timed out waiting for the canvas to settle"`.

### `hello`

`canvas` is `false` in tabs-only mode (Settings → infinite canvas off). Then
`leave_stage`, `fly_to`, `back`, `reserve_region` and `place_window` reply
`{"ok":false,"error":"the canvas is off (tabs-only mode)"}`; `hello`,
`get_layout`, `focus_window` and `subscribe` still work.

### `get_layout`

```json
{
  "camera": {"x": 0.0, "y": 0.0, "zoom": 1.0},
  "stage": false,
  "moving": false,
  "windows": [
    {"hwnd": 123, "title": "README.md - Code", "exe": "code.exe",
     "canvas_rect": [10, 20, 1200, 800], "screen_rect": [10, 20, 1200, 800],
     "on_screen": true, "tab_hidden": false, "away": false}
  ]
}
```

- `stage`: the overview (zoomed-out view, including the search over it) is open.
- `moving`: the overlay is up and the camera or windows are in motion (a
  gesture, an animation, or real windows landing). `false` while resting in
  the overview.
- `windows`: on-canvas windows first (topmost first), then hidden tabs, then
  minimized/hidden windows that keep their canvas spot.
  - `exe` is the lowercase executable name.
  - `screen_rect` is where the visible frame is on screen right now (rounded
    to whole pixels; while `moving` or zoomed out this is where its live
    thumbnail is drawn). `null` for hidden tabs and `away` windows.
  - `on_screen`: `screen_rect` overlaps a monitor.
  - `tab_hidden`: a tab of a tab group that isn't shown; `canvas_rect` is
    the group's spot.
  - `away`: minimized (or hidden) but keeping its spot on the canvas.

### `focus_window {hwnd}`

Flies the camera to the window, lands at 1:1 and activates it; replies once
settled. A hidden tab is switched to first; a minimized window is restored.
In tabs-only mode it just switches tabs / activates the window and replies at
once. `back` afterwards returns to the camera from before.

### `leave_stage`

Drops the overview back to 1:1 (where it was opened from); replies once
settled. Replies at once if no overview is open.

### `fly_to {canvas_rect}`

If the rect fits on screen at 1:1 the camera lands at 1:1 centred on it
(rects bigger than the screen get their top-left pinned). Otherwise it zooms
out to fit the rect with a margin and stays in the overview. Replies once
settled.

### `back`

Returns the camera to where it was before the last `fly_to` or
`focus_window` (at 1:1 if it was near 1:1, else as an overview). Error
`"nothing to go back to"` if there's none (each fly can be undone once).

### `reserve_region {name, w, h}` → `{canvas_rect}`

A canvas area for the client to put its own windows in. The first request
for a `name` places it in free canvas space: 400 px right of every window
(on the canvas, minimized or tabbed), every other region and the current
view, top-aligned with the highest of them. Later requests for the same name
return the same spot (resized to the new `w`/`h` if they differ). Regions are
saved in `%APPDATA%\Apeiron\layout.json` and survive restarts and "Reset saved
layout".

### `place_window {hwnd, region}` → `{canvas_rect}`

Moves the window's visible frame to the region's top-left corner (shrunk to
fit the region if it's bigger) without moving the camera; `canvas_rect` is
where it went. A maximized window is restored first. Errors for an unknown
region or window, and for a hidden tab or minimized window.

## Events (`subscribe`)

After the `subscribe` reply the connection also receives events, one per
line, starting with the current `camera`, `stage` and `gesture`:

```
{"event":"camera","x":0.0,"y":0.0,"zoom":1.0,"moving":false}
{"event":"stage","open":true}
{"event":"window_moved","hwnd":123,"canvas_rect":[x,y,w,h],"screen_rect":[x,y,w,h]|null}
{"event":"window_closed","hwnd":123}
{"event":"focus","hwnd":123}
{"event":"tab_switch","group":456,"hwnd":123}
{"event":"gesture","active":true}
```

- Events are worked out from Apeiron's state at most 60 times a second, so
  `camera` (and everything else) is coalesced to ≤ 60/s; the last state is
  always sent.
- Every time motion comes to rest the client gets a `camera` event with
  `moving:false`, even if the whole motion fell between two event ticks or it
  is already moving again (then `moving:false` is followed by `moving:true`).
  Refresh the layout on that edge.
- `window_moved` is sent when a window's canvas rect changes, when it
  appears (opened, restored, shown as a tab), and when its screen rect
  changed once motion stops. Screen rects aren't sent every frame while
  `moving` is true: hide drawings on `moving:true`, re-project from the
  `window_moved` events that follow `moving:false`.
- `window_closed`: the window was destroyed.
- `focus`: a window on the canvas got the keyboard (not sent when focus
  goes to something that isn't on the canvas).
- `tab_switch`: a tab group now shows `hwnd`; `group` identifies the group
  (the hwnd of its first tab).
- `gesture`: `active:true` while fingers are on the touchpad, the mouse is
  dragging on the canvas (a window, including Alt free-drags, a tab, a
  window by its title bar, or the canvas itself), or Apeiron's Alt+Tab
  switcher is open (from the Alt+Tab press until Alt is let go or it's
  cancelled). Use it to stand down your own Alt handling.
- A client that stops reading has events dropped once 1024 lines are queued.

## Other apps' windows

Apeiron never puts windows whose class name starts with `screenbuddy.` on
the canvas.

## Trying it

`cargo run --example ipc_client` sends `hello` and `get_layout`; pass requests
as arguments (`-- '{"op":"focus_window","hwnd":1234}'`) or `-- --watch` to
print events.
