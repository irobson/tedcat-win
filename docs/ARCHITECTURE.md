# Architecture

TedCat for Windows is a native Rust port of the macOS app
([irobson/tedcat](https://github.com/irobson/tedcat)). The behaviour is the
same; the platform layer is new.

## Crates

| Crate | What it holds |
|-------|---------------|
| `crates/core` (`tedcat-core`) | Pure logic, no Win32: the hold-gesture state machine, the recording decision (region / too small / audio only), the video encoding plan and file naming. Same test cases as the macOS app. Runs and tests on any OS. |
| `crates/app` (`tedcat`, binary `TedCat.exe`) | Everything Windows. Only compiles for Windows targets. Its Win32-free decision code (for example `input/hook/logic.rs`) has unit tests of its own. |

## Threads

```
 hook thread                        UI thread (main)                            worker / recorder threads
 ───────────                        ────────────────                            ──────────────────────────
 WH_MOUSE_LL / WH_KEYBOARD_LL       TedCat.Msg (message-only): every post       WGC FrameArrived (free-threaded)
 HookLogic + recognizer             TedCat.Main (hidden top-level): tray,       WASAPI loopback loop
 SendInput replays                    broadcasts, end of the session            Media Foundation sink writer
                   ──WM_APP_GESTURE──────▶ overlay, pill, outline, card         Recorder::stop (blocking)
                   ──WM_APP_HOOK_STATUS──▶ screenshot to clipboard
                   ◀─WM_HOOK_SELECTION_REFUSED── (thread message)
                                           ◀──WM_APP_RECORDER── Interrupted / Finished
```

Every window and every UI call lives on the UI thread. Other threads only
post `WM_APP_*` messages with a boxed payload (`messages.rs`).

### Messages between threads

- **Two windows.** `TedCat.Main` is a hidden top-level window: it owns the
  tray icon and the clipboard, and receives broadcasts (`TaskbarCreated`,
  `WM_SETTINGCHANGE`, `WM_DISPLAYCHANGE`) and `WM_QUERYENDSESSION` /
  `WM_ENDSESSION`. Being top-level, it also receives other processes'
  `HWND_BROADCAST` posts, so it never takes a payload: it logs and ignores
  `WM_APP_GESTURE`..`WM_APP_RECORDER`. Payload posts go to the message-only
  window `TedCat.Msg` (parent `HWND_MESSAGE`), which broadcasts never reach.
- **Payload cookie.** `UiHandle::post` boxes the payload into `LPARAM` and puts
  a random per-process cookie (never zero) in `WPARAM`. `take_payload` only
  turns `LPARAM` back into a box when the cookie matches, so a `WM_APP + n`
  that another process posts straight to `TedCat.Msg` is logged and ignored,
  never dereferenced.
- **UI to hook.** The UI makes one request of the hook thread: when it will
  not show a selection (the overlay could not be created, or TedCat is
  quitting or the session is ending while a selection is on screen), it posts
  `WM_HOOK_SELECTION_REFUSED` with the selection's origin packed into
  `WPARAM`/`LPARAM` (`HookHandle::refuse_selection`). The hook then hands the
  held press back to the app (see below). It ignores the request unless that
  selection is still in progress, so a late request never touches a later
  gesture.
- **Reasons are sentences.** Every reason shown to the user
  (`HookStatus::Unavailable` / `Degraded`, `RecorderEvent::Interrupted`, the
  errors of `Recorder::start` and `stop`, shell and Launch at Login errors) is
  a complete sentence with one final period. `messages::sentence` is the one
  place that adds a missing period; each producer passes its reason through it
  where the reason leaves its module. The UI shows reasons as they are and
  only appends whole sentences after them ("The display resolution changed
  during the recording. TedCat saves what was captured."). Every balloon is
  also logged as `notice: <title>: <text>`.

## Coordinates

The process is per-monitor DPI aware (v2, from the manifest), so hook events,
window positions and capture APIs all use physical pixels of the virtual
screen (`geom.rs`). The core works in logical units; the app divides by the
monitor's scale (DPI / 96) when asking it about sizes, so "4 points of
tolerance" or "10 points before it counts as a drag" mean the same on a 100 %
and a 200 % display.

## Hold-and-replay on Windows

A low-level mouse hook swallows the primary button-down and arms a timer. If
the button is released or the pointer moves past the tolerance first, the held
down and the current event are re-injected together with one `SendInput` call
(absolute virtual-desk coordinates, sent from the hook thread's message loop,
never from inside the callback), marked with `dwExtraInfo = "TCAT"` so the hook
lets its own replays through. Mouse moves are never swallowed (that would
freeze the cursor). The decisions live in `input/hook/logic.rs` (`HookLogic`,
no Win32 calls, unit tested); `input/hook.rs` is the thread, the callbacks and
the Win32 calls around it.

- **Swapped buttons.** The hook reports logical buttons ("left" is the
  primary button, also with "primary button = right" for left-handed use);
  `SendInput` takes physical ones and swaps them itself. Replays convert back
  with `SM_SWAPBUTTON`, read for every batch since the setting can change at
  any time, so a quick primary click stays a primary click and a drag never
  leaves a button down.
- **Modifier order.** Shift, Ctrl, Alt and Win pressed or released while the
  press is held, or while replays are queued, are swallowed and replayed in
  the same ordered `SendInput` call, in their physical place: a Ctrl+click
  arrives with `MK_CONTROL` and `GetKeyState` as the user pressed them, and a
  Shift pressed after the press is not part of it. Keys are only queued when
  the foreground window's process can take injected input (ours, or not more
  privileged); otherwise they pass at once, because UIPI would drop a replayed
  key-up and leave a key stuck. Once a key of the user is queued, every later
  one follows it, so a press and its release never swap places.
- **Stream order for everything else.** While a modifier waits behind the
  pending press, or a replay is queued, any other key (not Escape), wheel
  notch or other button is queued too. With modifiers held behind the pending
  press it also ends the hold first, as a drag would: press, modifiers and the
  event go out in physical order, and the release then passes. (Holding those
  events back instead would delay typing until the hold resolved and deliver
  them to the overlay if it became a selection.) Nothing is eaten, nothing
  overtakes what came first. Escape keeps its own handling, below.
- **Menu mask key.** A selection swallows the press for good, so a modifier
  down at it would reach the app as a lone tap: Alt or Win (menu bar, KeyTips,
  Start menu), Shift (IME mode). TedCat injects the unassigned virtual key
  `0xE8` (no character, no default action) while the modifier is down: at the
  hold itself for Alt and Win, when the selection begins for Shift and Ctrl,
  and between a held modifier's press and release.
- **Selection refusal.** If the UI cannot show a selection, or quits or ends
  the session while one is on screen, the hook replays the held press at the
  selection's origin and a move to where the pointer is now; the rest of the
  drag and the release then reach the app.
- **Lost key-ups.** If `SendInput` inserts only part of a batch, the key
  releases among the rest, the mask key's included, are tried again on their
  own, so no key stays logically down.

Lessons from the macOS app, where a revoked permission once made the hook eat
every click, are built in:

- **Fail open.** If `SendInput` refuses a mouse replay, the hook stops holding
  presses immediately and tells the user.
- **UIPI.** Presses over windows of elevated processes are never held, since
  a non-elevated app cannot replay into them.
- **Own windows.** Presses over TedCat's windows (class `TedCat.*`) are never held.
- **Pen and touch.** Mouse events Windows synthesizes from pen or touch are
  never held (press-and-hold is the touch right-click).
- **Emergency switch.** Escape three times within 1.5 s turns the gesture off.
  Escape during a selection cancels it.
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
- **Encoder size cap.** H.264 accepts at most 4096 x 2304 (long side x short
  side, levels 5.1/5.2). A larger region (a full-width selection on a 5K or a
  5120 x 1440 display) is scaled down to fit, keeping its aspect ratio:
  `VideoEncodingPlan` has the cropped `source_*` size and the encoded
  `pixel_*` size. The D3D11 video processor scales on the GPU; where it is
  unavailable each frame is scaled on the CPU instead. The log says which
  ("scaling the WxH region to WxH on the GPU / CPU"). The smoke test forces
  this path with `TEDCAT_TEST_MAX_VIDEO_SIZE`.
- **Media Foundation is delay-loaded.** `mfplat.dll` and `mfreadwrite.dll`
  are missing on Windows N and KN editions until the Media Feature Pack is
  installed. `build.rs` makes them delay-load imports of `TedCat.exe`, so it
  still starts there (the gesture and screenshots work). Before anything calls
  into them, recording loads both from System32 (`record/sys.rs`); if one is
  missing, recording fails with "Recording needs Windows Media Foundation
  (install the Media Feature Pack)." instead of the delay-load helper ending
  the process.
- **Display changes.** If the recorded monitor changes size mid-recording
  (resolution, rotation), frames keep arriving in surfaces of the old size, so
  the crop would be wrong: the recording is interrupted ("The display
  resolution changed during the recording.") and the UI stops it, which keeps
  everything captured before the change. A monitor that goes away interrupts
  it the same way.
- **Capture border.** TedCat asks for borderless capture
  (`SetIsBorderRequired(false)`). Windows honours that only where it supports
  it (Windows 11) and its consent setting lets desktop apps drop the border;
  otherwise it draws its yellow border on screen (not in the video).
- TedCat's own windows use `WDA_EXCLUDEFROMCAPTURE`, so they never appear in
  screenshots or videos.

## Quitting and the end of the session

Recordings are never left unfinalized, and presses never stay held while
TedCat goes away.

- **Quit.** The gesture is switched off first (the flag the hook reads, never
  the saved setting), so nothing is held while TedCat waits; a press already
  pending is handed back when its hold timer fires. A selection on screen is
  closed and its press handed back (selection refusal). Then the recording is
  finalized (up to 30 s, pumping messages meanwhile), the hook stops and
  TedCat exits.
- **`WM_QUERYENDSESSION`** (sign-out, shutdown, restart). TedCat never vetoes,
  but a running recording starts finalizing at once and
  `ShutdownBlockReasonCreate` names it ("Saving a recording") on Windows'
  shutdown screen. The reason is removed once nothing records or saves.
- **`WM_ENDSESSION`** returns only once every save is complete (or after
  30 s), because Windows may end the process right after. With the controller
  free it runs the quit path above. The running recorder and the saves in
  flight live in thread-locals outside the controller, so a quit already in
  progress, or a controller borrowed by a handler further up the stack, is no
  obstacle: the save is finished from there, and the gesture is switched off
  for the wait there too.

## Build

`cargo build --release -p tedcat` with the MSVC toolchain. `build.rs` embeds
the icon, the manifest (per-monitor DPI awareness v2, common controls v6) and
version info, and passes `/DELAYLOAD:mfplat.dll`, `/DELAYLOAD:mfreadwrite.dll`
and `delayimp.lib` to link.exe. Cargo applies those to the bin's unit-test
harness too, which calls no Media Foundation code, so the harness link would
warn LNK4199 ("no imports found"); `/IGNORE:4199` silences it, and the smoke
test checks the release executable's import table instead. CI builds with
`RUSTFLAGS=-D warnings`, linker output included (`linker_messages`), so it
stays warning-free. The GNU target (`x86_64-pc-windows-gnu`, for checking and
cross-compiling) has no delay loading; only MSVC builds are released.

## Tests

- `cargo test --workspace`: the core's gesture, recording and naming tests
  (any OS), and the app's unit tests on Windows: hook decisions (ordering,
  masks, fail-open, refusal), message cookies and sentences, notice texts,
  saves and the end of the session, layout, text and media helpers.
- `scripts/smoke-test.ps1` (CI job "Smoke test" on `windows-latest`) drives
  the release `TedCat.exe` on the runner's real desktop with `SendInput`
  (injected events without the "TCAT" marker count as real input) against
  test windows that tag every message with `GetMessageExtraInfo`, so it can
  tell a held-and-replayed event from one that passed straight through. Each
  step reports evidence; any failure fails the job. It proves:
  - clicks, double clicks and drags still arrive exactly once and in order;
    hold + drag puts an image of the right size and content on the clipboard
    without leaking the press; Escape cancels; Shift + hold records an MP4 of
    the right size and length (ffprobe) without the capture border where
    Windows allows that; Escape x3 turns the gesture off; a single instance;
    clicks work after TedCat is killed, even mid-gesture;
  - Media Foundation is a delay-load import (dumpbin or llvm-readobj); a
    region over the (lowered) encoder limit is scaled and still shows the
    right pixels; a resolution change mid-recording interrupts it with a
    readable notice, keeps what was recorded and restores the display;
  - junk `WM_APP` posts to both windows are ignored; Quit and the end of the
    session save the recording (complete `moov` box) while presses pass
    straight through, set the shutdown block reason, wait in `WM_ENDSESSION`,
    and hand a selection's press back to the app;
  - with swapped buttons a primary click and drag replay the right button;
    modifiers, keys typed and wheel notches keep their physical order around
    a held press; Alt/Win + hold injects the mask key and opens no menu;
  - every balloon of the run reads as whole sentences (no "..").
