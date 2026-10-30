//! The press-and-hold gesture, as a pure state machine.
//!
//! The trick is *hold-and-replay*: the mouse-down is intercepted and kept
//! aside. If the user releases or moves before the hold timer fires, the
//! original mouse-down is re-injected followed by the current event, and the
//! system never notices. If the timer fires first, the app owns the pointer
//! until the button is released, and the resulting rectangle is reported.
//!
//! The modifiers held at mouse-down pick the mode: a plain press for a
//! screenshot, with the recording modifier (Shift) for a screen recording.
//!
//! This type performs no I/O and owns no timer. The caller applies the
//! returned [`Verdict`] and [`Effect`]s, which keeps it testable anywhere.
//!
//! Windows note: a low-level mouse hook must not swallow plain mouse moves
//! (that freezes the cursor), so the app always passes `Drag` events through
//! and only honours the verdict for button events.

use crate::geometry::{Point, Rect};

/// Modifier keys as a small, platform-neutral bit set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const NONE: Modifiers = Modifiers(0);
    pub const SHIFT: Modifiers = Modifiers(1 << 0);
    pub const CONTROL: Modifiers = Modifiers(1 << 1);
    pub const ALT: Modifiers = Modifiers(1 << 2);
    pub const WIN: Modifiers = Modifiers(1 << 3);

    pub const fn contains(self, other: Modifiers) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn union(self, other: Modifiers) -> Modifiers {
        Modifiers(self.0 | other.0)
    }
    pub const fn without(self, other: Modifiers) -> Modifiers {
        Modifiers(self.0 & !other.0)
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Modifiers;
    fn bitor(self, rhs: Modifiers) -> Modifiers {
        self.union(rhs)
    }
}

/// What a completed gesture produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureMode {
    /// A still image copied to the clipboard.
    Screenshot,
    /// A video of the dragged region with system audio, saved to disk.
    Recording,
}

/// A pointer event as seen by the recognizer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerInput {
    /// Primary button pressed, with the modifiers held at that moment.
    Down { point: Point, modifiers: Modifiers },
    /// Pointer moved while the primary button is held.
    Drag(Point),
    /// Primary button released.
    Up(Point),
    /// The hold timer, armed by [`Effect::ArmTimer`], fired.
    HoldTimeout,
    /// The user asked to abort (Escape key).
    Cancel,
}

/// What the caller must do with the event that produced a response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Deliver the event untouched.
    Pass,
    /// Drop the event.
    Swallow,
    /// Drop the event, but keep a copy: it may be replayed later.
    Hold,
    /// Re-inject the held event, then re-inject this one, in that order.
    /// The original event must be dropped so ordering is guaranteed.
    Release,
}

/// Side effects the caller must perform, in order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Effect {
    ArmTimer { millis: u32 },
    DisarmTimer,
    BeginSelection { origin: Point, mode: CaptureMode },
    UpdateSelection(Rect),
    CommitSelection { rect: Rect, mode: CaptureMode },
    CancelSelection,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    pub verdict: Verdict,
    pub effects: Vec<Effect>,
}

impl Response {
    pub fn new(verdict: Verdict, effects: Vec<Effect>) -> Self {
        Self { verdict, effects }
    }
    pub fn just(verdict: Verdict) -> Self {
        Self { verdict, effects: Vec::new() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GestureConfig {
    /// How long the button must stay pressed, without moving, to enter selection mode.
    pub hold_millis: u32,
    /// Movement tolerated during the hold before it counts as a normal drag,
    /// in the same units as the points given to the recognizer.
    pub move_tolerance: f64,
    /// Modifiers that must all be held for the gesture to arm. Empty means a plain press.
    pub required_modifiers: Modifiers,
    /// Extra modifier that switches the gesture from screenshot to recording.
    /// Empty disables recording entirely.
    pub recording_modifier: Modifiers,
    /// When false, a press carrying `recording_modifier` is passed through.
    /// Used while a recording is already running.
    pub allows_recording: bool,
}

impl Default for GestureConfig {
    fn default() -> Self {
        Self {
            hold_millis: 350,
            move_tolerance: 4.0,
            required_modifiers: Modifiers::NONE,
            recording_modifier: Modifiers::SHIFT,
            allows_recording: true,
        }
    }
}

impl GestureConfig {
    /// Decides which mode a press starts, or `None` if it is not a gesture candidate.
    pub fn mode_for(&self, modifiers: Modifiers) -> Option<CaptureMode> {
        if !modifiers.contains(self.required_modifiers) {
            return None;
        }
        let wants_recording = !self.recording_modifier.is_empty() && modifiers.contains(self.recording_modifier);
        if !wants_recording {
            return Some(CaptureMode::Screenshot);
        }
        if self.allows_recording {
            Some(CaptureMode::Recording)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum State {
    Idle,
    /// Button is down, timer armed, waiting to see whether this is a hold.
    Pending { origin: Point, mode: CaptureMode },
    /// A normal click or drag is in progress; events flow untouched until mouse-up.
    Passthrough,
    /// The overlay is up and the user is dragging out a rectangle.
    Selecting { origin: Point, mode: CaptureMode },
}

#[derive(Clone, Debug)]
pub struct HoldGestureRecognizer {
    state: State,
    pub config: GestureConfig,
}

impl Default for HoldGestureRecognizer {
    fn default() -> Self {
        Self::new(GestureConfig::default())
    }
}

impl HoldGestureRecognizer {
    pub fn new(config: GestureConfig) -> Self {
        Self { state: State::Idle, config }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn is_selecting(&self) -> bool {
        matches!(self.state, State::Selecting { .. })
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.state, State::Idle)
    }

    pub fn handle(&mut self, input: PointerInput) -> Response {
        use Effect::*;
        use PointerInput::*;
        use Verdict::*;

        match (self.state, input) {
            // idle
            (State::Idle, Down { point, modifiers }) => match self.config.mode_for(modifiers) {
                Some(mode) => {
                    self.state = State::Pending { origin: point, mode };
                    Response::new(Hold, vec![ArmTimer { millis: self.config.hold_millis }])
                }
                None => Response::just(Pass),
            },
            (State::Idle, _) => Response::just(Pass),

            // pending
            (State::Pending { origin, .. }, Drag(point)) => {
                if origin.distance(point) > self.config.move_tolerance {
                    self.state = State::Passthrough;
                    Response::new(Release, vec![DisarmTimer])
                } else {
                    Response::just(Swallow)
                }
            }
            (State::Pending { .. }, Up(_)) => {
                self.state = State::Idle;
                Response::new(Release, vec![DisarmTimer])
            }
            (State::Pending { origin, mode }, HoldTimeout) => {
                self.state = State::Selecting { origin, mode };
                Response::new(Swallow, vec![BeginSelection { origin, mode }])
            }
            // A second press without a release cannot happen; a cancel while
            // pending has nothing to cancel yet. Ignore both.
            (State::Pending { .. }, Down { .. }) | (State::Pending { .. }, Cancel) => Response::just(Swallow),

            // passthrough
            (State::Passthrough, Up(_)) => {
                self.state = State::Idle;
                Response::just(Pass)
            }
            (State::Passthrough, _) => Response::just(Pass),

            // selecting
            (State::Selecting { origin, .. }, Drag(point)) => {
                Response::new(Swallow, vec![UpdateSelection(Rect::from_corners(origin, point))])
            }
            (State::Selecting { origin, mode }, Up(point)) => {
                self.state = State::Idle;
                Response::new(Swallow, vec![CommitSelection { rect: Rect::from_corners(origin, point), mode }])
            }
            (State::Selecting { .. }, Cancel) => {
                self.state = State::Idle;
                Response::new(Swallow, vec![CancelSelection])
            }
            (State::Selecting { .. }, _) => Response::just(Swallow),
        }
    }

    /// Forces the recognizer back to idle, e.g. when the hook is reinstalled.
    pub fn reset(&mut self) {
        self.state = State::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Effect::*;
    use PointerInput::*;
    use Verdict::*;

    const ORIGIN: Point = Point::new(100.0, 100.0);

    fn recognizer(required: Modifiers, allows_recording: bool) -> HoldGestureRecognizer {
        HoldGestureRecognizer::new(GestureConfig {
            hold_millis: 300,
            move_tolerance: 4.0,
            required_modifiers: required,
            recording_modifier: Modifiers::SHIFT,
            allows_recording,
        })
    }

    fn default_recognizer() -> HoldGestureRecognizer {
        recognizer(Modifiers::NONE, true)
    }

    fn down(modifiers: Modifiers) -> PointerInput {
        Down { point: ORIGIN, modifiers }
    }

    #[test]
    fn quick_click_is_held_then_replayed() {
        let mut r = default_recognizer();
        assert_eq!(r.handle(down(Modifiers::NONE)), Response::new(Hold, vec![ArmTimer { millis: 300 }]));
        assert_eq!(r.state(), State::Pending { origin: ORIGIN, mode: CaptureMode::Screenshot });
        assert_eq!(r.handle(Up(ORIGIN)), Response::new(Release, vec![DisarmTimer]));
        assert_eq!(r.state(), State::Idle);
    }

    #[test]
    fn moving_past_tolerance_becomes_a_normal_drag() {
        let mut r = default_recognizer();
        r.handle(down(Modifiers::NONE));
        assert_eq!(r.handle(Drag(Point::new(102.0, 101.0))), Response::just(Swallow));
        assert_eq!(r.state(), State::Pending { origin: ORIGIN, mode: CaptureMode::Screenshot });
        assert_eq!(r.handle(Drag(Point::new(120.0, 100.0))), Response::new(Release, vec![DisarmTimer]));
        assert_eq!(r.state(), State::Passthrough);
        assert_eq!(r.handle(Drag(Point::new(140.0, 100.0))), Response::just(Pass));
        assert_eq!(r.handle(Up(Point::new(140.0, 100.0))), Response::just(Pass));
        assert_eq!(r.state(), State::Idle);
    }

    #[test]
    fn holding_still_starts_a_screenshot_selection() {
        let mut r = default_recognizer();
        r.handle(down(Modifiers::NONE));
        assert_eq!(
            r.handle(HoldTimeout),
            Response::new(Swallow, vec![BeginSelection { origin: ORIGIN, mode: CaptureMode::Screenshot }])
        );
        assert!(r.is_selecting());
        let corner = Point::new(50.0, 180.0);
        let expected = Rect::new(50.0, 100.0, 50.0, 80.0);
        assert_eq!(r.handle(Drag(corner)), Response::new(Swallow, vec![UpdateSelection(expected)]));
        assert_eq!(
            r.handle(Up(corner)),
            Response::new(Swallow, vec![CommitSelection { rect: expected, mode: CaptureMode::Screenshot }])
        );
        assert_eq!(r.state(), State::Idle);
    }

    #[test]
    fn shift_hold_starts_a_recording_selection() {
        let mut r = default_recognizer();
        r.handle(down(Modifiers::SHIFT));
        assert_eq!(r.state(), State::Pending { origin: ORIGIN, mode: CaptureMode::Recording });
        assert_eq!(
            r.handle(HoldTimeout),
            Response::new(Swallow, vec![BeginSelection { origin: ORIGIN, mode: CaptureMode::Recording }])
        );
        let expected = Rect::new(100.0, 100.0, 200.0, 150.0);
        assert_eq!(
            r.handle(Up(Point::new(300.0, 250.0))),
            Response::new(Swallow, vec![CommitSelection { rect: expected, mode: CaptureMode::Recording }])
        );
    }

    #[test]
    fn quick_shift_click_still_reaches_the_app() {
        let mut r = default_recognizer();
        assert_eq!(r.handle(down(Modifiers::SHIFT)), Response::new(Hold, vec![ArmTimer { millis: 300 }]));
        assert_eq!(r.handle(Up(ORIGIN)), Response::new(Release, vec![DisarmTimer]));
    }

    #[test]
    fn shift_hold_passes_through_while_recording_is_not_allowed() {
        let mut r = recognizer(Modifiers::NONE, false);
        assert_eq!(r.handle(down(Modifiers::SHIFT)), Response::just(Pass));
        assert_eq!(r.state(), State::Idle);
        assert_eq!(r.handle(down(Modifiers::NONE)), Response::new(Hold, vec![ArmTimer { millis: 300 }]));
    }

    #[test]
    fn recording_combines_with_a_required_trigger_modifier() {
        let mut r = recognizer(Modifiers::CONTROL, true);
        assert_eq!(r.handle(down(Modifiers::SHIFT)), Response::just(Pass));
        r.handle(down(Modifiers::CONTROL | Modifiers::SHIFT));
        assert_eq!(r.state(), State::Pending { origin: ORIGIN, mode: CaptureMode::Recording });
    }

    #[test]
    fn escape_cancels_an_active_selection() {
        let mut r = default_recognizer();
        r.handle(down(Modifiers::NONE));
        r.handle(HoldTimeout);
        assert_eq!(r.handle(Cancel), Response::new(Swallow, vec![CancelSelection]));
        assert_eq!(r.state(), State::Idle);
        // The eventual mouse-up must not leak a stray click.
        assert_eq!(r.handle(Up(ORIGIN)), Response::just(Pass));
    }

    #[test]
    fn late_timeout_after_release_is_ignored() {
        let mut r = default_recognizer();
        r.handle(down(Modifiers::NONE));
        r.handle(Up(ORIGIN));
        assert_eq!(r.handle(HoldTimeout), Response::just(Pass));
        assert_eq!(r.state(), State::Idle);
    }

    #[test]
    fn required_modifiers_gate_the_gesture() {
        let mut r = recognizer(Modifiers::CONTROL, true);
        assert_eq!(r.handle(down(Modifiers::NONE)), Response::just(Pass));
        assert_eq!(r.state(), State::Idle);
        assert_eq!(r.handle(down(Modifiers::CONTROL)), Response::new(Hold, vec![ArmTimer { millis: 300 }]));
        assert_eq!(r.state(), State::Pending { origin: ORIGIN, mode: CaptureMode::Screenshot });
    }

    #[test]
    fn rectangles_are_normalized_regardless_of_direction() {
        let a = Point::new(10.0, 20.0);
        let b = Point::new(5.0, 40.0);
        assert_eq!(Rect::from_corners(a, b), Rect::new(5.0, 20.0, 5.0, 20.0));
        assert_eq!(Rect::from_corners(b, a), Rect::new(5.0, 20.0, 5.0, 20.0));
    }

    #[test]
    fn clamping_keeps_the_selection_on_screen() {
        let bounds = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert_eq!(Rect::new(-10.0, 90.0, 30.0, 30.0).clamped(&bounds), Rect::new(0.0, 90.0, 20.0, 10.0));
        assert_eq!(Rect::new(200.0, 200.0, 10.0, 10.0).clamped(&bounds), Rect::ZERO);
    }

    #[test]
    fn reset_returns_to_idle_from_any_state() {
        let mut r = default_recognizer();
        r.handle(down(Modifiers::NONE));
        r.handle(HoldTimeout);
        r.reset();
        assert!(r.is_idle());
        assert_eq!(r.handle(Up(ORIGIN)), Response::just(Pass));
    }
}
