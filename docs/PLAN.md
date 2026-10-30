# Apeiron v2 — GPUI rewrite plan

The shared contract for everyone working on the rewrite. Read all of it before
touching code. If you need to change an interface defined here, keep the change
minimal and say so in your final report.

## Goal

Apeiron, a polished infinite-canvas window manager for Windows 11: the desktop is an
unbounded plane, every app window lives on it, and you pan/zoom with touchpad
gestures like Figma/tldraw. It has to feel instant, never lose a window, and be
easy to use (tray icon, settings window, search). All drawing is GPUI.

The v1 prototype (on `main` and PRs #1/#2, the files currently in `src/`) works
and is the reference for behaviour: `src/canvas.rs`, `src/touchpad.rs`,
`src/wins.rs`, `src/main.rs`. Port its logic; don't redesign what works.

## Proven facts (don't re-litigate)

- gpui `0.2.2` from crates.io builds on this machine (same version as the
  user's `architect-gpui` repo at `C:\Users\hecto\Documents\code\architect-gpui`
  — look there for working GPUI-on-Windows patterns: `apps/gpui/src/platform.rs`
  for getting the HWND via `raw-window-handle`, `apps/gpui/src/tray.rs` for
  `tray-icon` 0.25 wired into GPUI, `Cargo.toml` for profiles). Patterns only;
  don't copy app-specific code.
- DWM live thumbnails (`DwmRegisterThumbnail` with the GPUI window's HWND as
  destination) render ON TOP of GPUI's DirectComposition content. Verified
  with `docs/spike-gpui-dwm-thumbnail.rs`. So GPUI draws grid/labels/minimap
  and DWM composites the live window thumbnails above it.
- GPUI `WindowKind::PopUp` creates a `WS_EX_TOOLWINDOW` window with style 0
  and a visible border; restyle it after creation with `SetWindowLongPtrW`
  (`GWL_STYLE = WS_POPUP`, add `WS_EX_TOPMOST | WS_EX_NOACTIVATE` to the
  existing ex-style, keep `WS_EX_NOREDIRECTIONBITMAP`) + `SetWindowPos(...,
  SWP_FRAMECHANGED)`.
- The precision touchpad on this machine reports 5 contact slots; raw HID
  parsing in `src/touchpad.rs` works.
- `Ctrl+Alt+Space` is taken on this machine. Hotkey registration failures must
  warn, never abort.
- Real windows can't be scaled. All motion happens on the overlay with
  thumbnails; real windows are moved (async `SetWindowPos`) only when motion
  settles at zoom 1. Keep this.

## Threads and data flow

```
 ┌──────────── engine thread (Win32) ────────────┐        ┌──────── GPUI main thread ────────┐
 │ message-only window: WM_INPUT (raw touchpad),  │ UiEvent│ views read Snapshot in render()   │
 │ WM_HOTKEY, WM_TIMER (anim 8ms), WM_APP_CMD     │ ─────► │ overlay window, bg window,        │
 │ Canvas model: cam, zoom, wins, gestures, anim  │        │ settings window, tray icon        │
 │ DWM thumbnails (dest = overlay HWND)           │ ◄───── │ mouse/keyboard on overlay & bg →  │
 │ moves real windows, shows/hides overlay HWND   │ Command│ Command                           │
 └────────────────────────────────────────────────┘        └───────────────────────────────────┘
          shared: Arc<RwLock<Snapshot>>, Arc<RwLock<Settings>>
```

- **Engine thread** owns everything latency-critical: touchpad input, the
  canvas model, DWM thumbnail updates, showing/hiding the overlay HWND
  (`SetWindowPos`/`ShowWindowAsync` cross-thread, async flags), moving real
  windows. Thumbnails update directly from the engine on every input frame —
  never wait for a GPUI frame to move windows.
- **GPUI thread** only draws (grid, labels, minimap, search, settings UI) and
  forwards mouse/keyboard input as `Command`s.
- Engine → UI: after any state change the engine writes `Snapshot` and sends
  `UiEvent::Redraw` on an `async_channel`, coalesced with an `AtomicBool` so at
  most one Redraw is queued. Other events: `FocusSearch`, `OpenSettings`,
  `Quit`.
- UI → engine: `std::sync::mpsc::Sender<Command>` plus
  `PostMessageW(engine_hwnd, WM_APP_CMD)` to wake the engine's message loop.
  Wrap both in a `EngineHandle` with `send(cmd)`.
- Coordinates: the engine works in physical pixels of the primary monitor.
  Snapshot is physical. Views divide by `window.scale_factor()`; mouse
  positions from GPUI are multiplied by it before going into a Command.

## Crate layout

Single binary crate, Windows only, edition 2021. `windows` 0.62 for Win32,
`gpui` 0.2.2, `raw-window-handle` 0.6, `tray-icon` 0.25, `async-channel` 2,
`serde`/`serde_json`, `windows-registry` 0.6 (autostart), `image` 0.25 only if
needed for icons. Release profile: `lto = "fat"`, `codegen-units = 1`,
`opt-level = 3`. Dev profile: deps at `opt-level = 3`.
Use `#![windows_subsystem = "windows"]` in release builds (no console window)
but keep `println!` logging working in debug (`cfg_attr(not(debug_assertions), ...)`).
Log to `%APPDATA%\Apeiron\log.txt` as well (simple append, small).

```
src/
  main.rs              args: `rescue`, `--watchdog <pid>`; single instance
                       (named mutex Local\Apeiron); spawn engine;
                       run GPUI app; open windows; tray
  safety.rs            watchdog spawn, console-close + panic rescue (from v1)
  shared.rs            Snapshot, WinInfo, Mode, Command, UiEvent, EngineHandle
  settings.rs          Settings (serde, defaults, load/save)        [feature B]
  layout.rs            saved canvas layout (load/save/match)          [feature A]
  autostart.rs         HKCU\...\Run on/off                           [feature B]
  tray.rs              tray icon + menu                              [feature B]
  icons.rs             HICON → gpui image cache                       [feature C]
  engine/
    mod.rs             thread, message-only window, msg loop, dispatch
    canvas.rs          model + gestures + anim + commit + sync + free-spot (port of v1)
    touchpad.rs        (v1, unchanged)
    wins.rs            (v1 + exe name / class / title helpers)
    tidy.rs            pure layout fn                                [feature D]
  ui/
    mod.rs             open overlay/bg/settings windows, restyle HWNDs,
                       subclass bg (stay bottom, never activate), event pump
    theme.rs           colours + sizes (dark, calm, one accent)
    canvas_view.rs     CanvasView: grid painter; overlay variant also hosts
                       labels, minimap, search
    labels.rs          window labels + hover ring                    [feature C]
    search.rs          search box + results                           [feature C]
    minimap.rs         minimap                                        [feature D]
    settings_window.rs settings UI                                    [feature B]
```

Feature modules start as **stubs with the final signatures, already called
from the core**, so feature work only fills them in.

## Shared types (`shared.rs`)

```rust
pub struct WinInfo { pub id: isize /* HWND */, pub title: String, pub exe: String,
                     pub x: f64, pub y: f64, pub w: f64, pub h: f64 /* canvas coords */,
                     pub focused: bool }
pub enum Mode { Idle, Moving, Overview, Search }
pub struct Snapshot { pub cam_x: f64, pub cam_y: f64, pub zoom: f64,
                      pub screen_w: f64, pub screen_h: f64, pub mode: Mode,
                      pub overlay_visible: bool, pub windows: Vec<WinInfo> /* topmost first */,
                      pub hovered: Option<isize>, pub search: Option<SearchState>,
                      pub paused: bool, pub generation: u64 }
pub struct SearchState { pub query: String, pub results: Vec<isize>, pub selected: usize }
pub enum Command {
    MouseDown { x: f64, y: f64 }, MouseMove { x: f64, y: f64 }, MouseUp,
    Wheel { dx: f64, dy: f64, pinch: bool, x: f64, y: f64 },
    ToggleOverview, LeaveOverview, Tidy, FocusWindow(isize), FlyTo { x: f64, y: f64 },
    OpenSearch, SearchQuery(String), SearchMove(i32), SearchAccept, CloseSearch,
    SettingsChanged, SetPaused(bool), ResetLayout, Quit,
}
pub enum UiEvent { Redraw, FocusSearch, OpenSettings, Quit }
```
(Exact field sets may grow; keep names.)

## Behaviour spec

Gestures (engine, from v1): exactly 3 fingers pan/pinch anywhere; 2 fingers
only when the cursor is over the overlay or bg window; 4+ fingers belong to
Windows. Pinch lock (zoom engages after |ln(spread/spread0)| > 0.12). Flick
momentum. Snap to zoom 1 when a gesture ends within 0.15 of 1, else stay in
Overview. Ignore wheel events within 1.5 s of a touchpad frame. Pan/pinch/flick
multipliers come from Settings.

Hotkeys (engine, `RegisterHotKey` on the message-only window; failures warn):
`Ctrl+Alt+Z` overview toggle, `Ctrl+Alt+F` find, `Ctrl+Alt+T` tidy,
`Ctrl+Alt+Q` quit, `Esc` (registered only while overlay is up) leaves
overview/search.

Windows:
- **Overlay**: GPUI PopUp, fullscreen primary, restyled borderless topmost,
  `WS_EX_NOACTIVATE` except while searching (it must take keyboard focus then:
  the engine got foreground rights from the hotkey; UI calls
  `window.activate_window()` + `SetForegroundWindow`). Hidden by default; the
  engine shows it at gesture start and hides it ~70 ms after committing.
  Renders: grid, labels (zoom < 0.75), hover ring, minimap, search.
- **Background**: GPUI window, fullscreen, always bottom-most (subclass:
  force `HWND_BOTTOM` in `WM_WINDOWPOSCHANGING`, `MA_NOACTIVATE` on
  `WM_MOUSEACTIVATE`), not in taskbar/alt-tab. Renders grid at the current
  camera + minimap (if enabled). It is the desktop; desktop icons are hidden
  behind it (acceptable).
- **Settings**: normal GPUI window opened from the tray, single instance.

Overview mouse (overlay): click window → fly + focus; drag window → move it on
the canvas; drag empty → pan; wheel → pan; Ctrl+wheel → zoom (mouse users).

Safety (from v1, must keep): restore-on-quit, console-close/panic rescue,
detached watchdog process, `rescue` CLI. Quit always ends at zoom 1 with real
windows placed and the layout saved.

## Features

**A. Remember layout** (`layout.rs`): `%APPDATA%\Apeiron\layout.json`
with camera and per-window canvas positions. Window identity across restarts:
key = lowercase exe file name + class name + title; fall back to exe + class
(first unclaimed). Save debounced (~1 s after a commit) and on quit. On sync, a
fresh window with a saved match takes the saved position (not free-spot).
Settings toggle `remember_layout`. Command::ResetLayout clears it.

**B. Settings + tray + autostart** (`settings.rs`, `settings_window.rs`,
`tray.rs`, `autostart.rs`): settings JSON next to layout.json. Fields (with
defaults): `pan_speed` 1.0, `pinch_speed` 1.0, `flick` 0.25, `two_finger_canvas`
true, `three_finger` true, `show_grid` true, `show_labels` true, `show_minimap`
true, `remember_layout` true, `autostart` false, `gap` 40. Settings window: a
clean single-column list with section headers (Gestures, Canvas, General),
toggles and − value + steppers, live-applied (write file + Command::SettingsChanged),
a hotkey reference section, buttons "Tidy up now" and "Reset saved layout".
Tray: icon drawn in code (no asset files needed); left-click = overview; menu:
Overview, Find window…, Tidy up, Settings…, Pause/Resume, Quit. Pause = engine
ignores gestures and hides bg. Autostart via `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

**C. Labels + icons + search** (`labels.rs`, `icons.rs`, `search.rs`):
when zoom < 0.75 each window gets a label above it: 16px app icon + title
(truncated). Hover ring on the window under the mouse in overview. Icons from
`WM_GETICON`/`GCLP_HICON` → 32×32 BGRA → GPUI image, cached per HWND.
Search (Ctrl+Alt+F or tray): overview + centred search field at the top; type
to filter by title/exe (case-insensitive, prefix matches rank first); matching
windows stay bright, others dim; ↑/↓ select, Enter flies to + focuses, Esc
closes. The camera gently frames the selected result as you move through them.

**D. Tidy + minimap** (`tidy.rs`, `minimap.rs`): `tidy::arrange(&[(w,h)],
gap) -> Vec<(x,y)>` pure function: keep current reading order (sort by y then
x), wrap rows at ≈ sqrt(total area × 1.6), align row tops. Engine animates each
window from old to new position (extend Anim with per-window from/to), then
frames everything in overview. Minimap: bottom-right panel (~220×140 logical,
rounded, translucent) on overlay and bg (setting), window rects + camera
viewport rect; click or drag in it moves the camera.

## Quality bar

- Builds with zero warnings and zero clippy warnings (`cargo clippy --release`).
- No `unwrap()` on Win32 results in runtime paths; log and continue.
- Re-entrancy: Win32 calls can re-enter window procedures; never hold a
  RefCell/Mutex borrow across calls that send messages to our own windows
  (see v1 `with_canvas`).
- Comment density like v1: module docs + a line where something is
  non-obvious. No noise comments.
- Never leave the user's windows off-screen. If you run the app to test it,
  run it briefly, quit with Ctrl+Alt+Q or kill it, then run
  `target\release\apeiron.exe rescue` (or the debug path).
- You can screenshot the composed desktop with
  `ffmpeg -v error -y -f lavfi -i "ddagrab=output_idx=0:framerate=5,hwdownload,format=bgra" -frames:v 1 -vf scale=960:-1 shot.png`
  (ffmpeg is on PATH) and view the PNG to check rendering.
- Commit with clear messages (no Co-Authored-By trailer) and push your branch
  after every commit.
