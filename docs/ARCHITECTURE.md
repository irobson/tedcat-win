# Architecture

TedCat for Windows is a native Rust port of the macOS app
([irobson/tedcat](https://github.com/irobson/tedcat)). The behaviour is the
same; the platform layer is new.

## Crates

| Crate | What it holds |
|-------|---------------|
| `crates/core` (`tedcat-core`) | Pure logic, no Win32: the hold-gesture state machine, the recording decision (region / too small / audio only), the video encoding plan and file naming. Same test cases as the macOS app. Runs and tests on any OS. |
| `crates/app` (`tedcat`, binary `TedCat.exe`) | Everything Windows. Only compiles for Windows targets. |

## Threads

```
 hook thread                         UI thread (main)                      worker / recorder threads
 ───────────                         ────────────────                      ──────────────────────────
 WH_MOUSE_LL / WH_KEYBOARD_LL        hidden window TedCat.Main             WGC FrameArrived (free-threaded)
 HoldGestureRecognizer               tray, overlay, pill, outline, card    WASAPI loopback loop
 SendInput replays        ──WM_APP_GESTURE──▶  screenshot to clipboard     Media Foundation sink writer
                          ──WM_APP_HOOK_STATUS─▶  Recorder::start          Recorder::stop (blocking)
                                     ◀──WM_APP_RECORDER── Interrupted / Finished
```

Every window and every UI call lives on the UI thread. Other threads only post
`WM_APP_*` messages with a boxed payload (`messages.rs`).

## Coordinates

The process is per-monitor DPI aware (v2, from the manifest), so hook events,
window positions and capture APIs all use physical pixels of the virtual
screen (`geom.rs`). The core works in logical units; the app divides by the
monitor's scale (DPI / 96) when asking it about sizes, so "4 points of
tolerance" or "10 points before it counts as a drag" mean the same on a 100 %
and a 200 % display.

## Hold-and-replay on Windows

A low-level mouse hook swallows the left button-down and arms a timer. If the
button is released or the pointer moves past the tolerance first, the held
down and the current event are re-injected together with `SendInput`, marked
with `dwExtraInfo = "TCAT"` so the hook lets its own replays through. Mouse
moves are never swallowed (that would freeze the cursor).

Lessons from the macOS app, where a revoked permission once made the hook eat
every click, are built in:

- **Fail open.** If `SendInput` refuses a replay, the hook stops holding
  presses immediately and tells the user.
- **UIPI.** Presses over windows of elevated processes are never held, since
  a non-elevated app cannot replay into them.
- **Own windows.** Presses over TedCat's windows (class `TedCat.*`) are never held.
- **Emergency switch.** Escape three times within 1.5 s turns the gesture off.
- **Watchdog.** Windows silently drops hooks whose callback times out; the hook
  thread detects that and reinstalls them.
- **Single instance.** A second TedCat exits immediately, so two hooks can
  never hold the same click.

## Capture

- Screenshots: `BitBlt` of the region from the screen DC, placed on the
  clipboard as `CF_DIBV5` and PNG.
- Recordings: Windows.Graphics.Capture on the region's monitor, cropped on the
  GPU, H.264 via the Media Foundation sink writer, 30 fps, size and bitrate from
  `VideoEncodingPlan`. System audio from WASAPI loopback (plus a silent render
  stream so loopback never stalls), AAC 48 kHz stereo, same file.
- TedCat's own windows use `WDA_EXCLUDEFROMCAPTURE`, so they never appear in
  screenshots or videos.
