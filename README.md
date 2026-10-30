# Apeiron

Apeiron (Greek for "the boundless") is a window manager for Windows where your desktop is an infinite canvas. Every app window has a position on an unbounded plane, and you pan and zoom around it with touchpad gestures, the way you would in Figma or tldraw.

## How it works

Windows can't scale real windows, and apps repaint late when they're moved, so motion never touches the real windows directly:

1. When a gesture starts, a fullscreen overlay appears with a live DWM thumbnail of every window at exactly 1:1, so it looks identical to the desktop.
2. Panning and zooming move the thumbnails, which DWM composites on the GPU.
3. When the gesture ends near 100% zoom, the real windows are moved to where their thumbnails are and the overlay goes away.

Canvas positions are stored as `f64`, so the plane is effectively unbounded. Windows far off-camera are parked just off-screen.

## Architecture

Two threads (details in [`docs/PLAN.md`](docs/PLAN.md)):

- **Engine thread** (`src/engine/`): a message-only Win32 window that reads raw touchpad input, hotkeys and the animation timer, owns the canvas model, updates the DWM thumbnails and moves real windows. It never waits for the UI.
- **GPUI thread** (`src/ui/`): draws the backdrop (and labels, minimap, search) on two fullscreen windows, an overlay and a background, and forwards mouse input to the engine as `Command`s.

The engine publishes a `Snapshot` (`src/shared.rs`) after each state change and sends a coalesced `UiEvent::Redraw`; DWM thumbnails composite above GPUI's content.

## Controls

| Input | Action |
|---|---|
| Three-finger drag | Pan the canvas (works over any app) |
| Three-finger pinch | Zoom around the cursor |
| Two-finger scroll / pinch on empty canvas | Pan / zoom |
| Ctrl+Alt+Z | Zoom out to fit everything, or back in |
| Ctrl+Alt+F | Find a window |
| Ctrl+Alt+T | Tidy up |
| Click a window (zoomed out) | Fly to it and focus it |
| Drag a window (zoomed out) | Move it on the canvas |
| Drag empty space (zoomed out) | Pan |
| Esc (zoomed out) | Back to 1:1 where you started |
| Ctrl+Alt+Q | Quit |

Four-finger gestures are left to Windows, so desktop switching still works.

The canvas becomes one continuous world: your wallpaper, mirror-tiled so it never ends, drifts past behind the windows more slowly than they move (parallax, adjustable in the settings), and shrinks a little as you zoom out. Overlay and desktop draw it identically, so ending a gesture never jumps. Prefer something calmer? Switch the background to a dot grid. A small minimap in the top-right corner shows every window and where you are, and follows along live, including when you drag windows around by hand; click or drag in it to fly somewhere. New windows (and ones that were maximized) are slid sideways into free space instead of piling up.

Three-finger gestures need Windows' own three-finger gestures switched off: **Settings > Bluetooth & devices > Touchpad > Three-finger gestures > Nothing**. Gestures are read straight from the precision touchpad via Raw Input.

## Safety

The app only moves windows. It never modifies the system. Windows parked off-screen are brought back when:

- you quit (the canvas returns to 1:1 first), or the app panics;
- the app is killed: a detached watchdog process waits for it to exit and then rescues stranded windows;
- you run `apeiron rescue` manually (e.g. `target\release\apeiron.exe rescue`).

Only one instance runs at a time. Logs go to `%APPDATA%\Apeiron\log.txt`, next to `settings.json` and `layout.json` (carried over from `%APPDATA%\infinite-canvas` the first time Apeiron runs).

## Build

```
cargo build --release
target\release\apeiron.exe
```

Right-click the tray icon for settings, pause and quit. "Start with Windows" in the settings adds an `Apeiron` entry to `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

## Status

The core (gestures, overview, overlay and background windows, safety) plus remembered layout, settings window, tray icon, autostart, labels, search, tidy and the minimap are in place.

## Known limits

- Primary monitor only.
- Minimized windows aren't on the canvas. Maximized windows become normal windows of the same size when the canvas first moves.
- Admin (elevated) windows can't be moved unless the app also runs as admin.
