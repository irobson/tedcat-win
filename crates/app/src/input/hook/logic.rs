//! The decision layer of the input hook, free of Win32 calls so it can be unit tested.
//!
//! [`HookLogic`] wraps the core `HoldGestureRecognizer` with what the Windows
//! hook adds on top of it: the held press and the replay queue, the ordering of
//! events while a replay is still queued, Escape handling and the emergency
//! switch, and the fail-open rules. The hook thread (the parent module) feeds
//! it classified events, sends the queued replays with `SendInput`, and
//! executes the [`Command`]s it emits.
//!
//! Modifier keys (Shift, Ctrl, Alt, Win) pressed or released while a press is
//! held, or while replays are still queued, are swallowed and queued behind
//! the press, so the app sees them where they physically happened: a click's
//! `MK_CONTROL` / `MK_SHIFT` and `GetKeyState` come from the key state at that
//! point of the input stream. A selection swallows the press for good, so
//! TedCat then injects the unassigned key [`MENU_MASK_VK`] while the modifier
//! is down; otherwise the app would see a lone Alt or Win tap (menu bar,
//! KeyTips, Start menu) or a lone Shift tap (IME input mode).
//!
//! Every method that can be reached from a hook callback is allocation-light
//! and never blocks.

use tedcat_core::{Effect, GestureConfig, HoldGestureRecognizer, Modifiers, Point, PointerInput, State, Verdict};

use crate::geom::{PointPx, RectPx};
use crate::messages::{GestureEvent, HookStatus};

/// Escape presses that trigger the emergency switch...
pub(super) const KILL_SWITCH_PRESSES: usize = 3;
/// ...within this many milliseconds.
pub(super) const KILL_SWITCH_WINDOW_MS: u32 = 1_500;
/// The watchdog reinstalls the hooks when input arrived this long after the last callback.
pub(super) const WATCHDOG_STALL_MS: u32 = 2_000;
/// Movement tolerated while a press is pending, in logical units (multiplied by the monitor scale).
pub(super) const MOVE_TOLERANCE_POINTS: f64 = 4.0;
/// Shortest and longest hold accepted from the settings.
pub(super) const HOLD_MILLIS_RANGE: (u32, u32) = (50, 10_000);
/// Minimum spacing between two `GestureEvent::Update` posts (about 144 Hz).
pub(super) const UPDATE_INTERVAL_US: u64 = 7_000;
/// How long a looked-up integrity level stays valid.
pub(super) const INTEGRITY_CACHE_TTL_MS: u64 = 2_000;
const INTEGRITY_CACHE_CAPACITY: usize = 8;

/// A mouse action TedCat may re-inject.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReplayAction {
    Move,
    LeftDown,
    LeftUp,
    RightDown,
    RightUp,
    MiddleDown,
    MiddleUp,
    /// `XBUTTON1` or `XBUTTON2`.
    XDown(u16),
    XUp(u16),
    /// Wheel delta, a multiple of `WHEEL_DELTA` (120) for notched wheels.
    Wheel(i16),
    HWheel(i16),
}

/// One mouse event to re-inject, at a point in physical pixels of the virtual screen.
///
/// Buttons are logical (primary / secondary, as the hook reports them); the
/// hook thread converts them to the physical buttons `SendInput` expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ReplayEvent {
    pub point: PointPx,
    pub action: ReplayAction,
}

impl ReplayEvent {
    pub fn new(point: PointPx, action: ReplayAction) -> Self {
        Self { point, action }
    }
}

/// The unassigned virtual key TedCat injects so the release of a modifier
/// whose click was swallowed is not a lone tap (the "menu mask" key; 0xE8 is
/// unassigned, produces no character and has no default action).
pub(super) const MENU_MASK_VK: u16 = 0xE8;

/// One keyboard event to re-inject.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct KeyStroke {
    /// Virtual key as the hook reported it (`VK_LCONTROL`, `VK_SHIFT`, ...).
    pub vk: u16,
    pub scan: u16,
    /// `LLKHF_EXTENDED` was set (right Ctrl / Alt, the Win keys).
    pub extended: bool,
    pub up: bool,
}

impl KeyStroke {
    /// The [`MENU_MASK_VK`] key going down (`up` false) or up.
    pub const fn mask(up: bool) -> Self {
        Self { vk: MENU_MASK_VK, scan: 0, extended: false, up }
    }

    /// True for the menu mask key TedCat adds itself (not one of the user's keys).
    pub const fn is_mask(&self) -> bool {
        self.vk == MENU_MASK_VK
    }
}

/// One event of the replay queue, injected in queue order with one `SendInput` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Replay {
    Mouse(ReplayEvent),
    Key(KeyStroke),
}

impl Replay {
    /// A key of the user (not the mask key) that must keep its place in the stream.
    fn is_user_key(&self) -> bool {
        matches!(self, Replay::Key(key) if !key.is_mask())
    }
}

/// What the hook thread knows about the world when the left button goes down.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PressContext {
    /// `GestureSettings::enabled` at this press.
    pub enabled: bool,
    /// False when the press must never be held: no window under the pointer,
    /// one of TedCat's own windows, or a window of a process with a higher
    /// (or unknown) integrity level.
    pub eligible: bool,
    /// Scale of the monitor under the pointer.
    pub scale: f64,
    /// Modifiers held at this press.
    pub modifiers: Modifiers,
    pub hold_millis: u32,
    pub required: Modifiers,
    pub allows_recording: bool,
}

impl PressContext {
    /// The recognizer configuration for this press (physical pixels).
    pub fn config(&self) -> GestureConfig {
        let scale = if self.scale.is_finite() && self.scale > 0.0 { self.scale } else { 1.0 };
        GestureConfig {
            hold_millis: self.hold_millis.clamp(HOLD_MILLIS_RANGE.0, HOLD_MILLIS_RANGE.1),
            move_tolerance: MOVE_TOLERANCE_POINTS * scale,
            // Shift is reserved for recording, whatever the settings say.
            required_modifiers: self.required.without(Modifiers::SHIFT),
            recording_modifier: Modifiers::SHIFT,
            allows_recording: self.allows_recording,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LogLevel {
    Info,
    Warn,
    Error,
}

/// Side effects the hook thread performs for [`HookLogic`], in order.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Command {
    /// Post to the UI thread as `WM_APP_GESTURE`.
    Gesture(GestureEvent),
    ArmHoldTimer(u32),
    DisarmHoldTimer,
    /// Replays were queued: wake the hook thread's message loop so it sends them.
    Wake,
    /// Set `GestureSettings::enabled` to false. `status` is posted when the
    /// gesture was enabled, or in any case when `always_report` is set.
    Disable { status: HookStatus, always_report: bool },
    Log(LogLevel, String),
}

/// Hold-and-replay on top of the core recognizer. See the module docs.
#[derive(Debug)]
pub(super) struct HookLogic {
    recognizer: HoldGestureRecognizer,
    /// The swallowed left-down, kept from `Hold` until it is replayed, consumed
    /// by a commit or cancel, or dropped.
    held: Option<PointPx>,
    /// Modifier keys swallowed while the press is pending, in order. They go
    /// out right behind the held press, or on their own when the press turns
    /// into a selection or is dropped.
    held_keys: Vec<KeyStroke>,
    /// A modifier was down at the held press and no mask key has been queued
    /// since (Alt and Win get one at the hold itself).
    mask_owed: bool,
    hold_timer_armed: bool,
    replays: Vec<Replay>,
    escape: EscapeTracker,
    commands: Vec<Command>,
}

impl Default for HookLogic {
    fn default() -> Self {
        Self::new()
    }
}

impl HookLogic {
    pub fn new() -> Self {
        Self {
            recognizer: HoldGestureRecognizer::default(),
            held: None,
            held_keys: Vec::new(),
            mask_owed: false,
            hold_timer_armed: false,
            replays: Vec::new(),
            escape: EscapeTracker::default(),
            commands: Vec::new(),
        }
    }

    #[cfg(test)]
    fn is_idle(&self) -> bool {
        self.recognizer.is_idle()
    }

    #[cfg(test)]
    fn held(&self) -> Option<PointPx> {
        self.held
    }

    #[cfg(test)]
    fn has_queued_replays(&self) -> bool {
        !self.replays.is_empty()
    }

    /// Takes the queued replays, in the order they must be injected.
    pub fn take_replays(&mut self) -> Vec<Replay> {
        std::mem::take(&mut self.replays)
    }

    /// Moves the pending commands to the end of `out`.
    pub fn drain_commands_into(&mut self, out: &mut Vec<Command>) {
        out.append(&mut self.commands);
    }

    /// Left button pressed. Returns true when the event must be swallowed.
    pub fn left_down(&mut self, point: PointPx, ctx: &PressContext) -> bool {
        if !self.recognizer.is_idle() {
            // The hook sees every release, so a press while a gesture is still in
            // progress means its release was lost (secure desktop, removed hook).
            // The app never saw that held press either: drop it.
            self.abandon(false);
        }
        let verdict = if ctx.enabled && ctx.eligible {
            self.recognizer.config = ctx.config();
            let response =
                self.recognizer.handle(PointerInput::Down { point: point.to_core(), modifiers: ctx.modifiers });
            self.apply(&response.effects);
            response.verdict
        } else {
            Verdict::Pass
        };
        let swallow = self.finish(verdict, ReplayEvent::new(point, ReplayAction::LeftDown));
        if verdict == Verdict::Hold {
            // Alt or Win down at a press TedCat may swallow for good: mask them
            // right away, while they are certainly still down. Masking a press
            // that is later replayed as a normal click is harmless. Shift and
            // Ctrl only get their mask once a selection begins.
            let menu_keys = ctx.modifiers.contains(Modifiers::ALT) || ctx.modifiers.contains(Modifiers::WIN);
            if menu_keys {
                self.enqueue_mask();
            }
            self.mask_owed = !menu_keys && !ctx.modifiers.is_empty();
        }
        swallow
    }

    /// Left button released. Returns true when the event must be swallowed.
    pub fn left_up(&mut self, point: PointPx) -> bool {
        let current = ReplayEvent::new(point, ReplayAction::LeftUp);
        if self.recognizer.is_idle() {
            return self.defer_while_replaying(current);
        }
        let response = self.recognizer.handle(PointerInput::Up(point.to_core()));
        self.apply(&response.effects);
        self.finish(response.verdict, current)
    }

    /// Pointer moved. Moves are never swallowed (that would freeze the cursor),
    /// whatever the recognizer says.
    pub fn moved(&mut self, point: PointPx) {
        if self.recognizer.is_idle() {
            return;
        }
        let response = self.recognizer.handle(PointerInput::Drag(point.to_core()));
        self.apply(&response.effects);
        if response.verdict == Verdict::Release {
            // A normal drag: give the app the press at its original point, then
            // bring the pointer back to where it is now.
            self.release_held();
            self.enqueue(Replay::Mouse(ReplayEvent::new(point, ReplayAction::Move)));
        }
    }

    /// Any other button or wheel event. Passed through, unless a replay is still
    /// queued: then it is swallowed and queued behind it, so the app sees every
    /// event in the order it happened.
    pub fn other_button(&mut self, event: ReplayEvent) -> bool {
        self.defer_while_replaying(event)
    }

    /// A modifier key (Shift, Ctrl, Alt or Win, see [`is_modifier_vk`]) went
    /// down or up. Returns true when the event must be swallowed: it is then
    /// queued and replayed in its physical place, behind the held press (or
    /// behind the replays still queued).
    ///
    /// `foreground_accepts` says whether replayed keys can reach the
    /// foreground window (UIPI drops input injected into a more privileged
    /// process, which could leave a modifier stuck down). It is asked only
    /// when the key would be queued and no key of the user is queued yet;
    /// once one is, every later modifier is queued too, so a press and its
    /// release can never swap places.
    pub fn modifier_key(&mut self, key: KeyStroke, foreground_accepts: impl FnOnce() -> bool) -> bool {
        let pending = self.held.is_some() && matches!(self.recognizer.state(), State::Pending { .. });
        if !pending && self.replays.is_empty() {
            return false;
        }
        let keys_queued = !self.held_keys.is_empty() || self.replays.iter().any(Replay::is_user_key);
        if !keys_queued && !foreground_accepts() {
            return false;
        }
        if pending {
            self.held_keys.push(key);
        } else {
            self.enqueue(Replay::Key(key));
        }
        true
    }

    /// Escape key down (`down`) or up. Returns true when the key event must be swallowed.
    pub fn escape(&mut self, down: bool, now_ms: u32) -> bool {
        if !down {
            self.escape.down = false;
            return std::mem::take(&mut self.escape.swallowing);
        }
        if std::mem::replace(&mut self.escape.down, true) {
            // Auto-repeat: neither a new press nor a new cancel.
            return self.escape.swallowing;
        }
        let mut swallow = false;
        if self.recognizer.is_selecting() {
            let response = self.recognizer.handle(PointerInput::Cancel);
            self.apply(&response.effects);
            swallow = true;
        }
        if self.escape.press(now_ms) {
            self.kill_switch();
        }
        self.escape.swallowing = swallow;
        swallow
    }

    /// The hold timer fired. `enabled` is the current `GestureSettings::enabled`.
    pub fn hold_timeout(&mut self, enabled: bool) {
        self.hold_timer_armed = false;
        if !matches!(self.recognizer.state(), State::Pending { .. }) {
            return;
        }
        if !enabled {
            // Turned off while the button was down: give the press back.
            self.abandon(true);
            return;
        }
        let response = self.recognizer.handle(PointerInput::HoldTimeout);
        self.apply(&response.effects);
    }

    /// `SetTimer` failed for an `ArmHoldTimer`: the hold can never fire, so give
    /// the press back to the app right away.
    pub fn hold_timer_failed(&mut self) {
        self.hold_timer_armed = false;
        if matches!(self.recognizer.state(), State::Pending { .. }) {
            self.abandon(true);
        }
        self.log(LogLevel::Warn, "The hold timer could not be armed; the press was passed through.".to_owned());
    }

    /// Posting `GestureEvent::Begin` to the UI failed: the UI never showed the
    /// overlay, so give the press back to the app instead of eating the click.
    pub fn begin_failed(&mut self) {
        if self.recognizer.is_selecting() {
            self.release_held();
            self.held = None;
            self.recognizer.reset();
        }
        self.log(LogLevel::Warn, "The UI did not accept the start of a selection; the press was passed through.".to_owned());
    }

    /// The UI will not show the selection that began at `origin` (its overlay
    /// could not be created, or TedCat is quitting): hand the press back to the
    /// app, as the held press at `origin` followed by a move to `current`, so a
    /// press-and-hold or a slow drag still reaches it and the release passes.
    /// Ignored unless that selection is still in progress, so a late request
    /// never touches a later gesture.
    pub fn selection_refused(&mut self, origin: PointPx, current: PointPx) {
        let ours = matches!(
            self.recognizer.state(),
            State::Selecting { origin: began, .. } if point_px(began) == origin
        );
        if !ours {
            return;
        }
        self.release_held();
        if current != origin {
            self.enqueue(Replay::Mouse(ReplayEvent::new(current, ReplayAction::Move)));
        }
        self.held = None;
        self.recognizer.reset();
        self.log(LogLevel::Warn, "The UI did not show the selection; the press was passed through.".to_owned());
    }

    /// `SendInput` refused (part of) a replay. Fail open: stop holding presses
    /// at once and tell the user.
    pub fn replay_failed(&mut self, reason: String) {
        self.replays.clear();
        self.abandon(false);
        self.log(LogLevel::Error, format!("Click replay failed, so TedCat stopped holding clicks: {reason}"));
        self.commands.push(Command::Disable {
            status: HookStatus::Degraded(format!(
                "Windows refused to replay a click ({reason}). TedCat stopped holding clicks; \
                 turn the gesture back on from the tray menu."
            )),
            always_report: true,
        });
    }

    /// The hooks were reinstalled by the watchdog: events were missed, so drop
    /// any held press and cancel any selection.
    pub fn hooks_reset(&mut self) {
        self.abandon(false);
    }

    /// The hook thread is stopping: drop every mouse replay and the held press,
    /// but keep the swallowed keys (the caller sends them one last time), so
    /// no modifier is left down.
    pub fn shutdown(&mut self) {
        self.replays.retain(|replay| matches!(replay, Replay::Key(_)));
        self.abandon(false);
    }

    /// A bug (panic) inside the hook logic: stop holding presses. Swallowed
    /// keys are still delivered, so no modifier is left down.
    pub fn internal_failure(&mut self) {
        self.replays.retain(|replay| matches!(replay, Replay::Key(_)));
        for key in std::mem::take(&mut self.held_keys) {
            self.replays.push(Replay::Key(key));
        }
        if !self.replays.is_empty() {
            self.commands.push(Command::Wake);
        }
        self.held = None;
        self.mask_owed = false;
        self.recognizer.reset();
        self.hold_timer_armed = false;
        self.commands.push(Command::DisarmHoldTimer);
        self.commands.push(Command::Gesture(GestureEvent::Cancel));
        self.log(LogLevel::Error, "Internal error in the input hook; click holding was turned off.".to_owned());
        self.commands.push(Command::Disable {
            status: HookStatus::Degraded(
                "TedCat hit an internal error in its input hook and stopped holding clicks.".to_owned(),
            ),
            always_report: true,
        });
    }

    fn kill_switch(&mut self) {
        self.escape.clear();
        // A pending press is still physically down: replay it so the release
        // completes a normal click. A selection is cancelled.
        self.abandon(true);
        self.commands.push(Command::Disable { status: HookStatus::KillSwitch, always_report: false });
        self.log(
            LogLevel::Warn,
            "Emergency switch: Escape pressed 3 times within 1.5 s; the hold gesture is off.".to_owned(),
        );
    }

    /// Ends the current gesture outside the recognizer's normal flow.
    /// `replay_pending_press`: replay a pending (not yet selecting) press
    /// instead of dropping it.
    fn abandon(&mut self, replay_pending_press: bool) {
        match self.recognizer.state() {
            State::Pending { .. } if replay_pending_press => self.release_held(),
            State::Selecting { .. } => self.commands.push(Command::Gesture(GestureEvent::Cancel)),
            _ => {}
        }
        self.held = None;
        // The keys are real input even when the press is dropped.
        self.flush_held_keys_without_press();
        self.disarm_hold_timer();
        self.recognizer.reset();
    }

    fn finish(&mut self, verdict: Verdict, current: ReplayEvent) -> bool {
        match verdict {
            Verdict::Pass => self.defer_while_replaying(current),
            Verdict::Swallow => true,
            Verdict::Hold => {
                self.held = Some(current.point);
                true
            }
            Verdict::Release => {
                self.release_held();
                self.enqueue(Replay::Mouse(current));
                true
            }
        }
    }

    fn defer_while_replaying(&mut self, event: ReplayEvent) -> bool {
        if self.replays.is_empty() {
            false
        } else {
            self.enqueue(Replay::Mouse(event));
            true
        }
    }

    /// Queues the held press, then the keys swallowed while it was held.
    fn release_held(&mut self) {
        if let Some(point) = self.held.take() {
            self.enqueue(Replay::Mouse(ReplayEvent::new(point, ReplayAction::LeftDown)));
        }
        self.mask_owed = false;
        let mut keys = std::mem::take(&mut self.held_keys);
        for key in keys.drain(..) {
            self.enqueue(Replay::Key(key));
        }
        self.held_keys = keys;
    }

    /// Queues the keys swallowed while the press was held when the press
    /// itself never reaches the app (a selection began, or it was dropped).
    /// No click separates those modifiers' presses from their releases any
    /// more, so a mask key goes between each press and the release after it,
    /// and at the end while one is still down.
    fn flush_held_keys_without_press(&mut self) {
        let mut owed = std::mem::take(&mut self.mask_owed);
        let mut keys = std::mem::take(&mut self.held_keys);
        for key in keys.drain(..) {
            if key.up && owed {
                self.enqueue_mask();
                owed = false;
            }
            self.enqueue(Replay::Key(key));
            owed |= !key.up;
        }
        self.held_keys = keys;
        if owed {
            self.enqueue_mask();
        }
    }

    /// Queues the menu mask key, down then up.
    fn enqueue_mask(&mut self) {
        self.enqueue(Replay::Key(KeyStroke::mask(false)));
        self.enqueue(Replay::Key(KeyStroke::mask(true)));
    }

    fn enqueue(&mut self, replay: Replay) {
        if self.replays.is_empty() {
            self.commands.push(Command::Wake);
        }
        self.replays.push(replay);
    }

    fn disarm_hold_timer(&mut self) {
        if self.hold_timer_armed {
            self.hold_timer_armed = false;
            self.commands.push(Command::DisarmHoldTimer);
        }
    }

    fn log(&mut self, level: LogLevel, message: String) {
        self.commands.push(Command::Log(level, message));
    }

    fn apply(&mut self, effects: &[Effect]) {
        for effect in effects {
            match *effect {
                Effect::ArmTimer { millis } => {
                    self.hold_timer_armed = true;
                    self.commands.push(Command::ArmHoldTimer(millis));
                }
                Effect::DisarmTimer => self.disarm_hold_timer(),
                Effect::BeginSelection { origin, mode } => {
                    self.commands.push(Command::Gesture(GestureEvent::Begin { origin: point_px(origin), mode }));
                    // The press stays held (for `begin_failed`) but will not be
                    // replayed: deliver the keys swallowed so far now.
                    self.flush_held_keys_without_press();
                }
                Effect::UpdateSelection(rect) => {
                    self.commands.push(Command::Gesture(GestureEvent::Update(RectPx::from_core(rect))));
                }
                Effect::CommitSelection { rect, mode } => {
                    self.held = None;
                    self.commands.push(Command::Gesture(GestureEvent::Commit { rect: RectPx::from_core(rect), mode }));
                }
                Effect::CancelSelection => {
                    self.held = None;
                    self.commands.push(Command::Gesture(GestureEvent::Cancel));
                }
            }
        }
    }
}

/// A core point (physical pixels as `f64`) back to pixels.
fn point_px(point: Point) -> PointPx {
    PointPx::new(point.x.round() as i32, point.y.round() as i32)
}

/// Recent Escape presses, for the emergency switch.
#[derive(Debug, Default)]
struct EscapeTracker {
    recent: [u32; KILL_SWITCH_PRESSES],
    count: usize,
    /// Escape is currently down (to recognise auto-repeat).
    down: bool,
    /// The current press was swallowed, so its repeats and release are too.
    swallowing: bool,
}

impl EscapeTracker {
    /// Records a press at `now_ms` (a wrapping millisecond tick). True when it
    /// completes 3 presses within the kill-switch window.
    fn press(&mut self, now_ms: u32) -> bool {
        self.recent.rotate_left(1);
        self.recent[KILL_SWITCH_PRESSES - 1] = now_ms;
        self.count = (self.count + 1).min(KILL_SWITCH_PRESSES);
        self.count == KILL_SWITCH_PRESSES && now_ms.wrapping_sub(self.recent[0]) <= KILL_SWITCH_WINDOW_MS
    }

    fn clear(&mut self) {
        self.count = 0;
    }
}

/// Whether the watchdog should consider the hooks dead: input was seen by the
/// system (`last_input`, from `GetLastInputInfo`) well after the last hook
/// callback (`last_callback`). Both are wrapping `GetTickCount` values.
pub(super) fn hooks_look_dead(last_input: u32, last_callback: u32) -> bool {
    (last_input.wrapping_sub(last_callback) as i32) > WATCHDOG_STALL_MS as i32
}

/// Coalesces selection updates so the UI is not flooded by high-rate mice.
#[derive(Debug, Default)]
pub(super) struct UpdateThrottle {
    last_post_us: Option<u64>,
    last_sent: Option<RectPx>,
    pending: Option<RectPx>,
}

/// What to do with an offered update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Throttled {
    /// Post this update now.
    Send(RectPx),
    /// Kept as pending; make sure a flush timer is armed.
    Deferred,
    /// Same as the last update sent; nothing to do.
    Unchanged,
}

impl UpdateThrottle {
    pub fn offer(&mut self, rect: RectPx, now_us: u64) -> Throttled {
        if self.pending.is_none() && self.last_sent == Some(rect) {
            return Throttled::Unchanged;
        }
        let due = self.last_post_us.is_none_or(|last| now_us.saturating_sub(last) >= UPDATE_INTERVAL_US);
        if due {
            self.pending = None;
            self.last_sent = Some(rect);
            self.last_post_us = Some(now_us);
            Throttled::Send(rect)
        } else {
            self.pending = Some(rect);
            Throttled::Deferred
        }
    }

    /// The pending update, if any, to post now (from the flush timer).
    pub fn flush(&mut self, now_us: u64) -> Option<RectPx> {
        let rect = self.pending.take()?;
        self.last_sent = Some(rect);
        self.last_post_us = Some(now_us);
        Some(rect)
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Short-lived cache of process integrity levels (`None` = could not be read).
#[derive(Debug, Default)]
pub(super) struct IntegrityCache {
    entries: Vec<(u32, Option<u32>, u64)>,
}

impl IntegrityCache {
    /// `Some(level)` when `pid` was looked up less than the TTL ago.
    pub fn get(&self, pid: u32, now_ms: u64) -> Option<Option<u32>> {
        self.entries
            .iter()
            .find(|(p, _, at)| *p == pid && now_ms.saturating_sub(*at) < INTEGRITY_CACHE_TTL_MS)
            .map(|(_, level, _)| *level)
    }

    pub fn insert(&mut self, pid: u32, level: Option<u32>, now_ms: u64) {
        self.entries.retain(|(p, _, at)| *p != pid && now_ms.saturating_sub(*at) < INTEGRITY_CACHE_TTL_MS);
        if self.entries.len() >= INTEGRITY_CACHE_CAPACITY {
            if let Some(oldest) = self.entries.iter().enumerate().min_by_key(|(_, e)| e.2).map(|(i, _)| i) {
                self.entries.swap_remove(oldest);
            }
        }
        self.entries.push((pid, level, now_ms));
    }
}

/// Whether a press over a window of process `pid` may be held.
///
/// Never over TedCat itself (`own_pid`, or a `TedCat.*` window class), never
/// without a process, and never over a process whose integrity level is higher
/// than ours or unknown, because UIPI would block the replay.
pub(super) fn may_hold_over(
    pid: u32,
    tedcat_class: bool,
    integrity: Option<u32>,
    own_pid: u32,
    own_integrity: u32,
) -> bool {
    if pid == 0 || pid == own_pid || tedcat_class {
        return false;
    }
    matches!(integrity, Some(level) if level <= own_integrity)
}

/// Whether keys swallowed while a press is held may be queued for replay,
/// given the process `pid` owning the foreground window.
///
/// Our own process always can; any other must have a known integrity level
/// no higher than ours, because UIPI silently drops input injected into a
/// more privileged process and a dropped key-up leaves a modifier stuck.
pub(super) fn may_queue_keys_for(pid: u32, integrity: Option<u32>, own_pid: u32, own_integrity: u32) -> bool {
    if pid == 0 {
        return false;
    }
    pid == own_pid || matches!(integrity, Some(level) if level <= own_integrity)
}

/// True for the virtual keys of Shift, Ctrl, Alt and Win, generic or
/// left/right (`VK_SHIFT`..`VK_MENU`, `VK_LWIN`, `VK_RWIN`, `VK_LSHIFT`..`VK_RMENU`).
pub(super) fn is_modifier_vk(vk: u32) -> bool {
    matches!(vk, 0x10..=0x12 | 0x5B | 0x5C | 0xA0..=0xA5)
}

/// True when a UTF-16 window class name starts with `prefix`.
pub(super) fn class_has_prefix(class_name: &[u16], prefix: &str) -> bool {
    let mut name = class_name.iter().copied().take_while(|&c| c != 0);
    prefix.encode_utf16().all(|expected| name.next() == Some(expected))
}

/// Modifier key states to core modifiers.
pub(super) fn modifiers_from_keys(shift: bool, control: bool, alt: bool, win: bool) -> Modifiers {
    let mut modifiers = Modifiers::NONE;
    for (down, bit) in [(shift, Modifiers::SHIFT), (control, Modifiers::CONTROL), (alt, Modifiers::ALT), (win, Modifiers::WIN)] {
        if down {
            modifiers = modifiers | bit;
        }
    }
    modifiers
}

/// Signature Windows puts in `dwExtraInfo` of mouse events promoted from pen
/// and touch input (`MI_WP_SIGNATURE`, compared under `SIGNATURE_MASK`).
const PEN_OR_TOUCH_SIGNATURE: usize = 0xFF51_5700;
const PEN_OR_TOUCH_MASK: usize = 0xFFFF_FF00;

/// True for mouse events Windows synthesized from pen or touch input. Those are
/// never held: replaying them as mouse input would lose their pen/touch nature
/// (and press-and-hold is the touch right-click gesture).
pub(super) fn is_pen_or_touch(extra_info: usize) -> bool {
    extra_info & PEN_OR_TOUCH_MASK == PEN_OR_TOUCH_SIGNATURE
}

/// Maps a pixel coordinate to the 0..=65535 range `SendInput` expects with
/// `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`, aiming at the pixel's
/// centre so rounding in either direction lands on the same pixel.
/// `origin` and `extent` describe the virtual screen on that axis.
pub(super) fn normalize_absolute(value: i32, origin: i32, extent: i32) -> i32 {
    if extent <= 1 {
        return 0;
    }
    let offset = (i64::from(value) - i64::from(origin)).clamp(0, i64::from(extent) - 1);
    let normalized = (offset * 65_536 + 32_768) / i64::from(extent);
    normalized.clamp(0, 65_535) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use tedcat_core::CaptureMode;

    const ORIGIN: PointPx = PointPx::new(100, 100);

    fn ctx() -> PressContext {
        PressContext {
            enabled: true,
            eligible: true,
            scale: 1.0,
            modifiers: Modifiers::NONE,
            hold_millis: 350,
            required: Modifiers::NONE,
            allows_recording: true,
        }
    }

    fn commands(logic: &mut HookLogic) -> Vec<Command> {
        let mut out = Vec::new();
        logic.drain_commands_into(&mut out);
        out
    }

    fn gestures(commands: &[Command]) -> Vec<GestureEvent> {
        commands
            .iter()
            .filter_map(|c| match c {
                Command::Gesture(g) => Some(*g),
                _ => None,
            })
            .collect()
    }

    fn down(point: PointPx) -> Replay {
        Replay::Mouse(ReplayEvent::new(point, ReplayAction::LeftDown))
    }

    fn up(point: PointPx) -> Replay {
        Replay::Mouse(ReplayEvent::new(point, ReplayAction::LeftUp))
    }

    /// Left Ctrl as the low-level hook reports it.
    fn ctrl(up: bool) -> KeyStroke {
        KeyStroke { vk: 0xA2, scan: 0x1D, extended: false, up }
    }

    fn shift(up: bool) -> KeyStroke {
        KeyStroke { vk: 0xA0, scan: 0x2A, extended: false, up }
    }

    fn key(stroke: KeyStroke) -> Replay {
        Replay::Key(stroke)
    }

    fn mask() -> [Replay; 2] {
        [Replay::Key(KeyStroke::mask(false)), Replay::Key(KeyStroke::mask(true))]
    }

    fn accepts() -> bool {
        true
    }

    fn refuses() -> bool {
        false
    }

    #[test]
    fn quick_click_is_held_then_replayed_in_order() {
        let mut logic = HookLogic::new();
        assert!(logic.left_down(ORIGIN, &ctx()));
        assert_eq!(commands(&mut logic), vec![Command::ArmHoldTimer(350)]);
        let release = PointPx::new(101, 100);
        assert!(logic.left_up(release));
        assert_eq!(commands(&mut logic), vec![Command::DisarmHoldTimer, Command::Wake]);
        assert_eq!(logic.take_replays(), vec![down(ORIGIN), up(release)]);
        assert!(logic.is_idle());
    }

    #[test]
    fn moves_are_never_swallowed_and_a_drag_replays_the_press() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        commands(&mut logic);
        logic.moved(PointPx::new(102, 101));
        assert!(!logic.has_queued_replays());
        let far = PointPx::new(120, 100);
        logic.moved(far);
        assert_eq!(logic.take_replays(), vec![down(ORIGIN), Replay::Mouse(ReplayEvent::new(far, ReplayAction::Move))]);
        assert_eq!(commands(&mut logic), vec![Command::DisarmHoldTimer, Command::Wake]);
        // The rest of the drag flows untouched.
        logic.moved(PointPx::new(150, 100));
        assert!(!logic.left_up(PointPx::new(150, 100)));
        assert!(logic.take_replays().is_empty());
        assert!(logic.is_idle());
    }

    #[test]
    fn tolerance_follows_the_monitor_scale() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &PressContext { scale: 2.0, ..ctx() });
        // 7 px is within 4 points at 200 %.
        logic.moved(PointPx::new(107, 100));
        assert!(!logic.has_queued_replays());
        logic.moved(PointPx::new(109, 100));
        assert!(logic.has_queued_replays());
    }

    #[test]
    fn holding_still_selects_and_commits_without_replaying() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        commands(&mut logic);
        logic.hold_timeout(true);
        assert_eq!(
            gestures(&commands(&mut logic)),
            vec![GestureEvent::Begin { origin: ORIGIN, mode: CaptureMode::Screenshot }]
        );
        logic.moved(PointPx::new(300, 250));
        assert_eq!(gestures(&commands(&mut logic)), vec![GestureEvent::Update(RectPx::new(100, 100, 300, 250))]);
        assert!(logic.left_up(PointPx::new(300, 250)));
        assert_eq!(
            gestures(&commands(&mut logic)),
            vec![GestureEvent::Commit { rect: RectPx::new(100, 100, 300, 250), mode: CaptureMode::Screenshot }]
        );
        assert!(logic.take_replays().is_empty());
        assert_eq!(logic.held(), None);
        assert!(logic.is_idle());
    }

    #[test]
    fn shift_selects_a_recording_unless_recording_is_not_allowed() {
        let mut logic = HookLogic::new();
        let shift = PressContext { modifiers: Modifiers::SHIFT, ..ctx() };
        logic.left_down(ORIGIN, &shift);
        commands(&mut logic);
        logic.hold_timeout(true);
        assert_eq!(
            gestures(&commands(&mut logic)),
            vec![GestureEvent::Begin { origin: ORIGIN, mode: CaptureMode::Recording }]
        );

        let mut logic = HookLogic::new();
        assert!(!logic.left_down(ORIGIN, &PressContext { allows_recording: false, ..shift }));
        assert!(logic.is_idle());
    }

    #[test]
    fn presses_are_passed_when_disabled_or_not_eligible() {
        let mut logic = HookLogic::new();
        assert!(!logic.left_down(ORIGIN, &PressContext { enabled: false, ..ctx() }));
        assert!(!logic.left_up(ORIGIN));
        assert!(!logic.left_down(ORIGIN, &PressContext { eligible: false, ..ctx() }));
        assert!(!logic.left_up(ORIGIN));
        assert!(commands(&mut logic).is_empty());
        assert!(logic.take_replays().is_empty());
    }

    #[test]
    fn required_modifiers_never_include_shift() {
        let config = PressContext { required: Modifiers::CONTROL | Modifiers::SHIFT, ..ctx() }.config();
        assert_eq!(config.required_modifiers, Modifiers::CONTROL);
        assert_eq!(config.recording_modifier, Modifiers::SHIFT);
        assert_eq!(PressContext { hold_millis: 0, ..ctx() }.config().hold_millis, HOLD_MILLIS_RANGE.0);
    }

    #[test]
    fn events_arriving_before_a_queued_replay_is_sent_are_queued_behind_it() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        logic.left_up(ORIGIN);
        // A right click and a disabled left press arrive before the loop sends the replay.
        let right = ReplayEvent::new(ORIGIN, ReplayAction::RightDown);
        assert!(logic.other_button(right));
        let second = PointPx::new(200, 200);
        assert!(logic.left_down(second, &PressContext { enabled: false, ..ctx() }));
        assert!(logic.left_up(second));
        assert_eq!(
            logic.take_replays(),
            vec![down(ORIGIN), up(ORIGIN), Replay::Mouse(right), down(second), up(second)]
        );
        // Once sent, other buttons pass straight through again.
        assert!(!logic.other_button(right));
    }

    #[test]
    fn a_double_click_is_replayed_as_two_ordered_clicks() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        logic.left_up(ORIGIN);
        assert!(logic.left_down(ORIGIN, &ctx()));
        assert!(logic.left_up(ORIGIN));
        assert_eq!(logic.take_replays(), vec![down(ORIGIN), up(ORIGIN), down(ORIGIN), up(ORIGIN)]);
    }

    #[test]
    fn a_modifier_released_while_the_press_is_held_follows_the_press() {
        let mut logic = HookLogic::new();
        // Ctrl went down before the press: nothing is held, so it passes.
        assert!(!logic.modifier_key(ctrl(false), || panic!("nothing is held: no need to ask")));
        assert!(logic.left_down(ORIGIN, &PressContext { modifiers: Modifiers::CONTROL, ..ctx() }));
        assert!(logic.modifier_key(ctrl(true), accepts), "a Ctrl-up while the press is held is swallowed");
        assert!(!logic.has_queued_replays(), "nothing is sent before the press is decided");
        assert!(logic.left_up(ORIGIN));
        assert_eq!(logic.take_replays(), vec![down(ORIGIN), key(ctrl(true)), up(ORIGIN)]);
        // Once sent, modifiers pass straight through again.
        assert!(!logic.modifier_key(ctrl(false), || panic!("nothing is queued: no need to ask")));
    }

    #[test]
    fn a_modifier_pressed_while_the_press_is_held_comes_after_it() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        assert!(logic.modifier_key(shift(false), accepts));
        assert!(logic.left_up(ORIGIN));
        // The Shift-up arrives before the loop sent the replay: it waits behind it.
        assert!(logic.modifier_key(shift(true), || panic!("a key of the user is already queued")));
        assert_eq!(logic.take_replays(), vec![down(ORIGIN), key(shift(false)), up(ORIGIN), key(shift(true))]);
    }

    #[test]
    fn a_drag_replays_the_swallowed_keys_behind_the_press() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        assert!(logic.modifier_key(ctrl(false), accepts));
        let far = PointPx::new(120, 100);
        logic.moved(far);
        assert_eq!(
            logic.take_replays(),
            vec![down(ORIGIN), key(ctrl(false)), Replay::Mouse(ReplayEvent::new(far, ReplayAction::Move))]
        );
    }

    #[test]
    fn modifiers_pass_when_the_foreground_cannot_take_replayed_keys() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        assert!(!logic.modifier_key(ctrl(false), refuses));
        assert!(!logic.modifier_key(ctrl(true), refuses));
        logic.left_up(ORIGIN);
        assert_eq!(logic.take_replays(), vec![down(ORIGIN), up(ORIGIN)]);
    }

    #[test]
    fn once_a_key_is_queued_the_next_ones_follow_it_without_asking() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        assert!(logic.modifier_key(ctrl(false), accepts));
        // Even if the foreground changed meanwhile, the release must not overtake the press.
        assert!(logic.modifier_key(ctrl(true), refuses));
        logic.left_up(ORIGIN);
        assert_eq!(logic.take_replays(), vec![down(ORIGIN), key(ctrl(false)), key(ctrl(true)), up(ORIGIN)]);
    }

    #[test]
    fn modifiers_behind_a_queued_replay_are_queued() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        logic.left_up(ORIGIN);
        assert!(logic.modifier_key(ctrl(false), accepts));
        assert_eq!(logic.take_replays(), vec![down(ORIGIN), up(ORIGIN), key(ctrl(false))]);
    }

    #[test]
    fn modifiers_are_not_queued_during_a_selection() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        logic.hold_timeout(true);
        assert!(logic.take_replays().is_empty());
        assert!(!logic.modifier_key(ctrl(false), || panic!("nothing to queue behind: no need to ask")));
    }

    #[test]
    fn keys_swallowed_before_a_selection_are_delivered_when_it_begins() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        assert!(logic.modifier_key(ctrl(false), accepts));
        commands(&mut logic);
        logic.hold_timeout(true);
        assert!(commands(&mut logic).contains(&Command::Wake));
        // Ctrl is down and the press never reaches the app: masked after the Ctrl-down.
        let [mask_down, mask_up] = mask();
        assert_eq!(logic.take_replays(), vec![key(ctrl(false)), mask_down, mask_up]);
        assert!(logic.left_up(PointPx::new(300, 300)));
        assert!(logic.take_replays().is_empty(), "the committed press is not replayed");
    }

    #[test]
    fn a_tap_while_the_press_is_held_gets_a_mask_between_press_and_release() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        logic.modifier_key(ctrl(false), accepts);
        logic.modifier_key(ctrl(true), accepts);
        logic.hold_timeout(true);
        let [mask_down, mask_up] = mask();
        assert_eq!(logic.take_replays(), vec![key(ctrl(false)), mask_down, mask_up, key(ctrl(true))]);
    }

    #[test]
    fn shift_or_ctrl_at_the_press_are_masked_once_a_selection_begins() {
        for modifiers in [Modifiers::SHIFT, Modifiers::CONTROL] {
            let mut logic = HookLogic::new();
            assert!(logic.left_down(ORIGIN, &PressContext { modifiers, ..ctx() }));
            assert!(!logic.has_queued_replays(), "no mask while the press may still be a click");
            logic.hold_timeout(true);
            assert_eq!(logic.take_replays(), mask().to_vec(), "{modifiers:?}");
        }
        // Released before the selection began: the mask goes before the release.
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &PressContext { modifiers: Modifiers::CONTROL, ..ctx() });
        logic.modifier_key(ctrl(true), accepts);
        logic.hold_timeout(true);
        let [mask_down, mask_up] = mask();
        assert_eq!(logic.take_replays(), vec![mask_down, mask_up, key(ctrl(true))]);
    }

    #[test]
    fn alt_and_win_are_masked_at_the_hold() {
        for modifiers in [Modifiers::ALT, Modifiers::WIN, Modifiers::ALT | Modifiers::SHIFT] {
            let mut logic = HookLogic::new();
            assert!(logic.left_down(ORIGIN, &PressContext { modifiers, ..ctx() }));
            assert!(commands(&mut logic).contains(&Command::Wake));
            assert_eq!(logic.take_replays(), mask().to_vec(), "{modifiers:?}");
            // The selection adds no second mask, and the commit replays nothing.
            logic.hold_timeout(true);
            assert!(logic.take_replays().is_empty());
            assert!(logic.left_up(PointPx::new(300, 300)));
            assert!(logic.take_replays().is_empty());
        }
        // A press without modifiers is never masked.
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        logic.hold_timeout(true);
        logic.left_up(PointPx::new(300, 300));
        assert!(logic.take_replays().is_empty());
    }

    #[test]
    fn a_quick_click_after_the_mask_is_replayed_behind_it() {
        let mut logic = HookLogic::new();
        let win = PressContext { modifiers: Modifiers::WIN, required: Modifiers::WIN, ..ctx() };
        assert!(logic.left_down(ORIGIN, &win));
        assert!(logic.left_up(ORIGIN));
        let [mask_down, mask_up] = mask();
        assert_eq!(logic.take_replays(), vec![mask_down, mask_up, down(ORIGIN), up(ORIGIN)]);
    }

    #[test]
    fn the_mask_does_not_pull_later_keys_into_the_queue() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &PressContext { modifiers: Modifiers::ALT, ..ctx() });
        assert!(logic.has_queued_replays());
        // An elevated foreground: the Alt-up passes instead of waiting behind the mask.
        let alt_up = KeyStroke { vk: 0xA4, scan: 0x38, extended: false, up: true };
        assert!(!logic.modifier_key(alt_up, refuses));
    }

    #[test]
    fn a_dropped_press_still_delivers_the_swallowed_keys() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &PressContext { modifiers: Modifiers::CONTROL, ..ctx() });
        logic.modifier_key(ctrl(true), accepts);
        logic.hooks_reset();
        let [mask_down, mask_up] = mask();
        assert_eq!(logic.take_replays(), vec![mask_down, mask_up, key(ctrl(true))]);
        assert!(logic.is_idle());
    }

    #[test]
    fn the_kill_switch_gives_back_the_press_with_its_keys() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        logic.modifier_key(ctrl(false), accepts);
        for t in [0u32, 100, 200] {
            logic.escape(true, t);
            logic.escape(false, t + 10);
        }
        assert_eq!(logic.take_replays(), vec![down(ORIGIN), key(ctrl(false))]);
        assert!(logic.is_idle());
    }

    #[test]
    fn shutdown_and_internal_failures_keep_the_swallowed_keys() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        logic.left_up(ORIGIN);
        // A second press is held behind the queued click, with a key behind it.
        assert!(logic.left_down(ORIGIN, &ctx()));
        assert!(logic.modifier_key(ctrl(true), accepts));
        logic.shutdown();
        assert_eq!(logic.take_replays(), vec![key(ctrl(true))], "mouse replays are dropped, keys are not");

        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        logic.modifier_key(ctrl(false), accepts);
        commands(&mut logic);
        logic.internal_failure();
        assert!(commands(&mut logic).contains(&Command::Wake));
        assert_eq!(logic.take_replays(), vec![key(ctrl(false))]);
        assert!(logic.is_idle());
    }

    #[test]
    fn modifier_virtual_keys() {
        for vk in [0x10, 0x11, 0x12, 0x5B, 0x5C, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5] {
            assert!(is_modifier_vk(vk), "{vk:#x}");
        }
        for vk in [0x1B, 0x14, 0x5D, 0x41, 0xE8, 0x09] {
            assert!(!is_modifier_vk(vk), "{vk:#x}");
        }
        assert!(KeyStroke::mask(false).is_mask() && !ctrl(false).is_mask());
    }

    #[test]
    fn keys_are_queued_only_for_a_foreground_that_can_take_them() {
        const MEDIUM: u32 = 0x2000;
        const HIGH: u32 = 0x3000;
        assert!(may_queue_keys_for(10, Some(MEDIUM), 1, MEDIUM));
        assert!(!may_queue_keys_for(10, Some(HIGH), 1, MEDIUM), "UIPI would drop the replayed keys");
        assert!(!may_queue_keys_for(10, None, 1, MEDIUM), "unknown integrity counts as elevated");
        assert!(!may_queue_keys_for(0, Some(MEDIUM), 1, MEDIUM), "no foreground window");
        assert!(may_queue_keys_for(1, None, 1, MEDIUM), "our own windows always can");
        assert!(may_queue_keys_for(10, Some(HIGH), 1, HIGH));
    }

    #[test]
    fn a_press_while_a_gesture_is_in_progress_starts_over() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        logic.hold_timeout(true);
        commands(&mut logic);
        // The release was lost; a new press must not be eaten by the stale selection.
        let point = PointPx::new(500, 500);
        assert!(!logic.left_down(point, &PressContext { eligible: false, ..ctx() }));
        assert_eq!(gestures(&commands(&mut logic)), vec![GestureEvent::Cancel]);
        assert!(logic.take_replays().is_empty());
        assert!(logic.is_idle());
    }

    #[test]
    fn escape_cancels_a_selection_and_is_swallowed() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        logic.hold_timeout(true);
        commands(&mut logic);
        assert!(logic.escape(true, 1_000));
        assert_eq!(gestures(&commands(&mut logic)), vec![GestureEvent::Cancel]);
        assert!(logic.escape(true, 1_030), "auto-repeat of a swallowed press stays swallowed");
        assert!(logic.escape(false, 1_050), "the matching release is swallowed too");
        assert!(!logic.escape(true, 5_000), "outside a selection Escape passes");
        assert!(!logic.escape(false, 5_010));
        // The release after a cancel passes untouched and replays nothing.
        assert!(!logic.left_up(ORIGIN));
        assert!(logic.take_replays().is_empty());
    }

    #[test]
    fn three_escapes_within_the_window_trip_the_kill_switch() {
        let mut logic = HookLogic::new();
        for (i, t) in [10_000u32, 10_400, 10_900].into_iter().enumerate() {
            assert!(!logic.escape(true, t), "kill-switch presses are not swallowed");
            logic.escape(false, t + 50);
            let cmds = commands(&mut logic);
            let tripped = cmds.iter().any(|c| matches!(c, Command::Disable { status: HookStatus::KillSwitch, .. }));
            assert_eq!(tripped, i == 2);
        }
    }

    #[test]
    fn slow_escapes_and_auto_repeat_do_not_trip_the_kill_switch() {
        let mut logic = HookLogic::new();
        for t in [0u32, 1_000, 2_000, 3_000] {
            logic.escape(true, t);
            logic.escape(false, t + 50);
        }
        // One long press: a single press however long the auto-repeat runs.
        logic.escape(true, 10_000);
        logic.escape(true, 10_030);
        logic.escape(true, 10_060);
        logic.escape(false, 10_090);
        let cmds = commands(&mut logic);
        assert!(!cmds.iter().any(|c| matches!(c, Command::Disable { .. })));
    }

    #[test]
    fn kill_switch_gives_back_a_pending_press() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        commands(&mut logic);
        for t in [0u32, 100, 200] {
            logic.escape(true, t);
            logic.escape(false, t + 10);
        }
        assert_eq!(logic.take_replays(), vec![down(ORIGIN)]);
        assert!(logic.is_idle());
        // The real release then passes untouched.
        assert!(!logic.left_up(ORIGIN));
    }

    #[test]
    fn kill_switch_tolerates_tick_wraparound() {
        let mut logic = HookLogic::new();
        for t in [u32::MAX - 400, u32::MAX - 10, 300] {
            logic.escape(true, t);
            logic.escape(false, t.wrapping_add(10));
        }
        assert!(commands(&mut logic).iter().any(|c| matches!(c, Command::Disable { status: HookStatus::KillSwitch, .. })));
    }

    #[test]
    fn a_failed_replay_fails_open() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        logic.left_up(ORIGIN);
        logic.take_replays();
        commands(&mut logic);
        logic.replay_failed("1 of 2".to_owned());
        let cmds = commands(&mut logic);
        assert!(cmds
            .iter()
            .any(|c| matches!(c, Command::Disable { status: HookStatus::Degraded(_), always_report: true })));
        assert!(logic.is_idle());
        assert!(!logic.has_queued_replays());
    }

    #[test]
    fn disabled_at_timeout_gives_the_press_back() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        commands(&mut logic);
        logic.hold_timeout(false);
        assert!(gestures(&commands(&mut logic)).is_empty());
        assert_eq!(logic.take_replays(), vec![down(ORIGIN)]);
        assert!(logic.is_idle());
    }

    #[test]
    fn a_failed_begin_gives_the_press_back() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        logic.hold_timeout(true);
        logic.begin_failed();
        assert_eq!(logic.take_replays(), vec![down(ORIGIN)]);
        assert!(logic.is_idle());
    }

    #[test]
    fn a_selection_the_ui_refused_gives_the_press_back() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        logic.hold_timeout(true);
        let dragged = PointPx::new(180, 140);
        logic.moved(dragged);
        commands(&mut logic);
        // A request for another origin belongs to an earlier gesture: ignored.
        logic.selection_refused(PointPx::new(5, 5), dragged);
        assert!(!logic.has_queued_replays());
        assert!(!logic.is_idle());

        logic.selection_refused(ORIGIN, dragged);
        assert_eq!(logic.take_replays(), vec![down(ORIGIN), Replay::Mouse(ReplayEvent::new(dragged, ReplayAction::Move))]);
        assert!(commands(&mut logic).contains(&Command::Wake));
        assert!(logic.is_idle());
        assert_eq!(logic.held(), None);
        // The rest of the drag and the release reach the app.
        logic.moved(PointPx::new(200, 150));
        assert!(!logic.left_up(PointPx::new(200, 150)));
        assert!(!logic.has_queued_replays());
        // A duplicate request changes nothing.
        logic.selection_refused(ORIGIN, dragged);
        assert!(!logic.has_queued_replays());
    }

    #[test]
    fn refusing_a_selection_never_touches_a_pending_press() {
        let mut logic = HookLogic::new();
        assert!(logic.left_down(ORIGIN, &ctx()));
        logic.selection_refused(ORIGIN, ORIGIN);
        assert!(!logic.has_queued_replays());
        assert_eq!(logic.held(), Some(ORIGIN));
        // Refused at the press point itself: no extra move is replayed.
        logic.hold_timeout(true);
        logic.selection_refused(ORIGIN, ORIGIN);
        assert_eq!(logic.take_replays(), vec![down(ORIGIN)]);
    }

    #[test]
    fn a_failed_timer_gives_the_press_back() {
        let mut logic = HookLogic::new();
        assert!(logic.left_down(ORIGIN, &ctx()));
        logic.hold_timer_failed();
        assert_eq!(logic.take_replays(), vec![down(ORIGIN)]);
        assert!(logic.is_idle());
    }

    #[test]
    fn late_or_stale_timeouts_are_ignored() {
        let mut logic = HookLogic::new();
        logic.hold_timeout(true);
        logic.left_down(ORIGIN, &ctx());
        logic.left_up(ORIGIN);
        commands(&mut logic);
        logic.hold_timeout(true);
        assert!(gestures(&commands(&mut logic)).is_empty());
    }

    #[test]
    fn watchdog_reset_drops_the_held_press_and_cancels_the_selection() {
        let mut logic = HookLogic::new();
        logic.left_down(ORIGIN, &ctx());
        commands(&mut logic);
        logic.hooks_reset();
        assert!(logic.take_replays().is_empty(), "the release was missed, replaying would leave the button stuck");
        assert_eq!(commands(&mut logic), vec![Command::DisarmHoldTimer]);

        logic.left_down(ORIGIN, &ctx());
        logic.hold_timeout(true);
        commands(&mut logic);
        logic.hooks_reset();
        assert_eq!(gestures(&commands(&mut logic)), vec![GestureEvent::Cancel]);
        assert!(logic.is_idle());
    }

    #[test]
    fn watchdog_detects_input_the_hooks_missed() {
        assert!(!hooks_look_dead(10_000, 9_000));
        assert!(!hooks_look_dead(10_000, 8_000));
        assert!(hooks_look_dead(10_001, 8_000));
        // Callbacks newer than the last input (normal) and tick wraparound.
        assert!(!hooks_look_dead(5_000, 6_000));
        assert!(hooks_look_dead(2_500, u32::MAX - 100));
        assert!(!hooks_look_dead(500, u32::MAX - 100));
    }

    #[test]
    fn updates_are_coalesced() {
        let mut throttle = UpdateThrottle::default();
        let a = RectPx::new(0, 0, 10, 10);
        let b = RectPx::new(0, 0, 20, 20);
        let c = RectPx::new(0, 0, 30, 30);
        assert_eq!(throttle.offer(a, 0), Throttled::Send(a));
        assert_eq!(throttle.offer(a, 1_000), Throttled::Unchanged);
        assert_eq!(throttle.offer(b, 2_000), Throttled::Deferred);
        assert_eq!(throttle.offer(c, 3_000), Throttled::Deferred);
        assert_eq!(throttle.flush(9_000), Some(c));
        assert_eq!(throttle.flush(9_500), None);
        assert_eq!(throttle.offer(c, 9_600), Throttled::Unchanged);
        assert_eq!(throttle.offer(a, 20_000), Throttled::Send(a));
        throttle.reset();
        assert_eq!(throttle.offer(a, 20_001), Throttled::Send(a));
    }

    #[test]
    fn integrity_cache_expires_and_stays_small() {
        let mut cache = IntegrityCache::default();
        cache.insert(42, Some(8192), 0);
        cache.insert(7, None, 0);
        assert_eq!(cache.get(42, 1_000), Some(Some(8192)));
        assert_eq!(cache.get(7, 1_000), Some(None));
        assert_eq!(cache.get(42, INTEGRITY_CACHE_TTL_MS), None);
        assert_eq!(cache.get(9, 0), None);
        for pid in 100..120 {
            cache.insert(pid, Some(8192), 10);
        }
        assert!(cache.entries.len() <= INTEGRITY_CACHE_CAPACITY);
        assert_eq!(cache.get(119, 11), Some(Some(8192)));
    }

    #[test]
    fn holds_are_refused_over_tedcat_and_more_privileged_processes() {
        const MEDIUM: u32 = 0x2000;
        const HIGH: u32 = 0x3000;
        const LOW: u32 = 0x1000;
        assert!(may_hold_over(10, false, Some(MEDIUM), 1, MEDIUM));
        assert!(may_hold_over(10, false, Some(LOW), 1, MEDIUM));
        assert!(!may_hold_over(10, false, Some(HIGH), 1, MEDIUM));
        assert!(!may_hold_over(10, false, None, 1, MEDIUM), "unknown integrity counts as elevated");
        assert!(!may_hold_over(1, false, Some(MEDIUM), 1, MEDIUM), "our own process");
        assert!(!may_hold_over(10, true, Some(MEDIUM), 1, MEDIUM), "a TedCat.* window class");
        assert!(!may_hold_over(0, false, Some(MEDIUM), 1, MEDIUM));
        assert!(may_hold_over(10, false, Some(HIGH), 1, HIGH), "an elevated TedCat may hold over elevated apps");
    }

    #[test]
    fn class_prefix_matching() {
        let name: Vec<u16> = "TedCat.Overlay.Dim\0junk".encode_utf16().collect();
        assert!(class_has_prefix(&name, "TedCat."));
        let other: Vec<u16> = "Chrome_WidgetWin_1".encode_utf16().collect();
        assert!(!class_has_prefix(&other, "TedCat."));
        let short: Vec<u16> = "TedCat\0".encode_utf16().collect();
        assert!(!class_has_prefix(&short, "TedCat."));
    }

    #[test]
    fn pen_and_touch_events_are_recognised() {
        assert!(is_pen_or_touch(0xFF51_5700));
        assert!(is_pen_or_touch(0xFF51_5780));
        assert!(!is_pen_or_touch(0));
        assert!(!is_pen_or_touch(0x5443_4154));
    }

    #[test]
    fn modifier_keys_map_to_core_modifiers() {
        assert_eq!(modifiers_from_keys(false, false, false, false), Modifiers::NONE);
        assert_eq!(modifiers_from_keys(true, false, false, false), Modifiers::SHIFT);
        assert_eq!(
            modifiers_from_keys(true, true, true, true),
            Modifiers::SHIFT | Modifiers::CONTROL | Modifiers::ALT | Modifiers::WIN
        );
    }

    /// Both ways Windows is commonly described to map normalized coordinates back
    /// to pixels must land on the original pixel.
    #[test]
    fn absolute_coordinates_land_on_the_original_pixel() {
        for (origin, extent) in [(0, 1920), (-1920, 3840), (-2560, 6400), (0, 7680), (-1000, 1001)] {
            for offset in [0, 1, extent / 3, extent / 2, extent - 2, extent - 1] {
                let n = normalize_absolute(origin + offset, origin, extent) as i64;
                assert!((0..=65_535).contains(&n));
                let truncated = n * extent as i64 / 65_536;
                assert_eq!(truncated, offset as i64, "origin {origin} extent {extent} offset {offset}");
                let rounded = ((n * (extent as i64 - 1)) as f64 / 65_535.0).round() as i64;
                assert!((rounded - offset as i64).abs() <= 1);
            }
        }
        assert_eq!(normalize_absolute(-50, 0, 1920), normalize_absolute(0, 0, 1920));
        assert_eq!(normalize_absolute(5_000, 0, 1920), normalize_absolute(1919, 0, 1920));
        assert_eq!(normalize_absolute(10, 0, 1), 0);
    }
}
