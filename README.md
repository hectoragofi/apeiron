# infinite-canvas

A window manager for Windows where your desktop is an infinite canvas. Every app window has a position on an unbounded plane, and you pan and zoom around it with touchpad gestures, the way you would in Figma or tldraw.

## How it works

Windows can't scale real windows, and apps repaint late when they're moved, so motion never touches the real windows directly:

1. When a gesture starts, a fullscreen overlay appears with a live DWM thumbnail of every window at exactly 1:1, so it looks identical to the desktop.
2. Panning and zooming move the thumbnails, which DWM composites on the GPU.
3. When the gesture ends near 100% zoom, the real windows are moved to where their thumbnails are and the overlay goes away.

Canvas positions are stored as `f64`, so the plane is effectively unbounded. Windows far off-camera are parked just off-screen.

## Controls

| Input | Action |
|---|---|
| Three-finger drag | Pan the canvas (works over any app) |
| Three-finger pinch | Zoom around the cursor |
| Ctrl+Alt+Z | Zoom out to fit everything, or back in |
| Click a window (zoomed out) | Fly to it and focus it |
| Drag a window (zoomed out) | Move it on the canvas |
| Drag empty space / two-finger scroll / pinch (zoomed out) | Pan / zoom |
| Esc (zoomed out) | Back to 1:1 where you started |
| Ctrl+Alt+Q | Quit |

Three-finger gestures need Windows' own three-finger gestures switched off: **Settings > Bluetooth & devices > Touchpad > Three-finger gestures > Nothing**. Gestures are read straight from the precision touchpad via Raw Input.

## Safety

The app only moves windows. It never modifies the system. Windows parked off-screen are brought back when:

- you quit, close the console, or the app panics;
- the app is killed: a detached watchdog process waits for it to exit and then rescues stranded windows;
- you run `infinite-canvas rescue` manually.

## Build

```
cargo run --release
```

## Known limits (prototype)

- Primary monitor only.
- Minimized windows aren't on the canvas. Maximized windows become normal windows of the same size when the canvas first moves.
- Admin (elevated) windows can't be moved unless the app also runs as admin.
- Canvas layout isn't saved between runs yet.
