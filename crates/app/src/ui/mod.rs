//! Tray icon and menu, recording pill, region outline and saved card.
//!
//! CONTRACT (owned by the UI agent): everything here runs on the UI thread.
//! Window class names start with `util::win::CLASS_PREFIX`; every visible
//! window calls `util::win::exclude_from_capture`; none of them takes focus
//! except dialogs. Submodules are free to change.
//!
//! Module map:
//! - `tray`, `icons`, `menu`, `menu_model`: notification-area icon, balloons and the popup menu.
//! - `hud`: the recording pill; `outline`: the red ring around a recorded region;
//!   `card`: the "recording saved" card, with `dragdrop` for dragging the file out.
//! - `canvas`, `raster`, `window`: drawing and window plumbing shared by the above.
//! - `dialogs`, `shell`, `autostart`, `sound`: small wrappers around system services.
//! - `layout`, `text`: pure placement and formatting helpers (unit tested).

pub(crate) mod autostart;
pub(crate) mod canvas;
pub(crate) mod card;
pub(crate) mod dialogs;
pub(crate) mod dragdrop;
pub(crate) mod hud;
pub(crate) mod icons;
pub(crate) mod layout;
pub(crate) mod menu;
pub(crate) mod menu_model;
pub(crate) mod outline;
pub(crate) mod raster;
pub(crate) mod shell;
pub(crate) mod sound;
pub(crate) mod text;
pub(crate) mod tray;
pub(crate) mod window;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::geom::{MonitorInfo, PointPx};
use layout::Corner;

/// Private notification from TedCat's own windows (pill, card) to TedCat's
/// message-only window (`TedCat.Msg`, which broadcasts never reach).
/// `WPARAM`: a [`UiAction`] code; `LPARAM`: the sender's generation number, so
/// actions from a window that has since been replaced are ignored. No payload
/// is boxed, so nothing leaks if the message is dropped.
pub(crate) const WM_APP_UI: u32 = WM_APP + 0x40;

/// What the user did in one of TedCat's windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UiAction {
    /// The recording pill was clicked.
    HudClicked,
    /// The failed pill has been visible long enough.
    HudExpired,
    /// The saved card was clicked: open the file.
    CardOpen,
    /// "Show in Explorer" was clicked.
    CardReveal,
    /// The close button was clicked.
    CardClose,
    /// The card's auto-hide countdown ran out.
    CardExpired,
}

impl UiAction {
    const ALL: [UiAction; 6] = [
        UiAction::HudClicked,
        UiAction::HudExpired,
        UiAction::CardOpen,
        UiAction::CardReveal,
        UiAction::CardClose,
        UiAction::CardExpired,
    ];

    fn code(self) -> usize {
        Self::ALL.iter().position(|&a| a == self).map_or(0, |i| i + 1)
    }

    /// The action for a `WPARAM` code from [`post_action`].
    pub(crate) fn from_code(code: usize) -> Option<UiAction> {
        code.checked_sub(1).and_then(|i| Self::ALL.get(i)).copied()
    }
}

/// Posts `action` from the window with `generation` to `notify` (the window
/// the pill or card was shown for: TedCat's message-only window).
pub(crate) fn post_action(notify: HWND, action: UiAction, generation: u32) {
    // SAFETY: plain PostMessageW with integer parameters.
    let posted = unsafe { PostMessageW(Some(notify), WM_APP_UI, WPARAM(action.code()), LPARAM(generation as isize)) };
    if let Err(error) = posted {
        crate::log_warn!("could not post {action:?}: {error}");
    }
}

/// Where the pill and the card sit: `corner` of the work area of the monitor
/// nearest `anchor` (the center of the recorded region, or the primary
/// monitor's origin for audio-only recordings). The monitor is looked up again
/// every time, so a display that was removed or rescaled is handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Placement {
    pub anchor: PointPx,
    pub corner: Corner,
}

impl Placement {
    /// Bottom-right of the primary monitor (whose top-left is the virtual-screen origin).
    pub(crate) fn primary_bottom_right() -> Placement {
        Placement { anchor: PointPx::new(0, 0), corner: Corner::BottomRight }
    }

    pub(crate) fn monitor(&self) -> MonitorInfo {
        MonitorInfo::from_point(self.anchor)
    }

    /// Top-left position for a window of `width` x `height` physical pixels on `monitor`.
    pub(crate) fn position(&self, monitor: &MonitorInfo, width: i32, height: i32) -> PointPx {
        let margin = layout::px(layout::CORNER_MARGIN, monitor.scale);
        layout::place_in_corner(monitor.work_area, self.corner, width, height, margin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_codes_round_trip() {
        for action in UiAction::ALL {
            assert_ne!(action.code(), 0);
            assert_eq!(UiAction::from_code(action.code()), Some(action));
        }
        assert_eq!(UiAction::from_code(0), None);
        assert_eq!(UiAction::from_code(99), None);
    }
}
