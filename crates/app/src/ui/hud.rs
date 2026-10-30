//! The recording pill: a small dark capsule in a corner of the screen with a
//! pulsing red dot (a waveform for audio-only), the elapsed time and a stop
//! square. Clicking it stops the recording. It also shows "Saving…" while the
//! file is finalized and a yellow "Failed" state (with the reason as a
//! tooltip) when something went wrong.

use std::cell::RefCell;
use std::f64::consts::TAU;
use std::rc::Rc;
use std::time::Instant;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DestroyWindow, KillTimer, SetTimer, MA_NOACTIVATE, SPI_SETWORKAREA, WINDOW_EX_STYLE,
    WM_CAPTURECHANGED, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE,
    WM_NCDESTROY, WM_SETCURSOR, WM_SETTINGCHANGE, WM_TIMER,
};

use crate::geom::{PointPx, RectPx};
use crate::ui::canvas::{self, Canvas, Font, WEIGHT_SEMIBOLD};
use crate::ui::layout::px;
use crate::ui::raster::{Area, Color, Pixmap};
use crate::ui::text::format_timer;
use crate::ui::{post_action, window, Placement, UiAction};

const CLASS: PCWSTR = w!("TedCat.Hud");

/// Size in logical px.
const WIDTH: f64 = 132.0;
const HEIGHT: f64 = 34.0;

const TICK_TIMER: usize = 1;
/// About 15 frames per second for the pulse; the time label changes once a second.
const TICK_MILLIS: u32 = 66;
const EXPIRE_TIMER: usize = 2;
/// How long the failed state stays up.
const FAILED_MILLIS: u32 = 4000;
/// One full pulse of the dot.
const PULSE_SECS: f64 = 1.6;

const BACKGROUND: Color = Color::rgb(30, 30, 30);
const BACKGROUND_HOVER: Color = Color::rgb(44, 44, 44);
const BORDER: Color = Color::rgba(255, 255, 255, 28);
const TEXT: Color = Color::rgb(245, 245, 245);
const RED: Color = crate::ui::outline::RECORDING_RED;
const GRAY: Color = Color::rgb(142, 142, 147);
const YELLOW: Color = Color::rgb(255, 214, 10);
const STOP: Color = Color::rgb(240, 240, 240);
const STOP_HOVER_RING: Color = Color::rgba(255, 255, 255, 34);

/// What the pill shows.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum HudMode {
    /// Recording since `started`.
    Recording { started: Instant },
    /// The recording is being finalized.
    Saving,
    /// Starting or finalizing failed; `reason` is shown as a tooltip.
    Failed { reason: String },
}

struct HudState {
    hwnd: HWND,
    /// Receives this window's `WM_APP_UI` actions (TedCat's message-only window).
    notify: HWND,
    generation: u32,
    audio_only: bool,
    mode: HudMode,
    placement: Placement,
    scale: f64,
    position: PointPx,
    hover: bool,
    tracking: bool,
    pressed: bool,
    epoch: Instant,
    font: Font,
    canvas: Option<Canvas>,
    tooltip: Option<window::Tooltip>,
}

/// The pill window; dropping it closes it.
pub(crate) struct Hud {
    hwnd: HWND,
    generation: u32,
    state: Rc<RefCell<HudState>>,
}

fn size(scale: f64) -> (i32, i32) {
    (px(WIDTH, scale), px(HEIGHT, scale))
}

impl Hud {
    /// Shows the pill in `placement`'s corner; its clicks go to `notify` as `WM_APP_UI`.
    pub(crate) fn show(
        notify: HWND,
        generation: u32,
        audio_only: bool,
        placement: Placement,
        mode: HudMode,
    ) -> windows::core::Result<Hud> {
        window::register_class(CLASS, Some(hud_proc), None)?;
        let monitor = placement.monitor();
        let (width, height) = size(monitor.scale);
        let position = placement.position(&monitor, width, height);
        let hwnd = window::create_floating(
            CLASS,
            WINDOW_EX_STYLE(0),
            RectPx::new(position.x, position.y, position.x + width, position.y + height),
        )?;
        let state = Rc::new(RefCell::new(HudState {
            hwnd,
            notify,
            generation,
            audio_only,
            mode: HudMode::Saving,
            placement,
            scale: monitor.scale,
            position,
            hover: false,
            tracking: false,
            pressed: false,
            epoch: Instant::now(),
            font: Font::new(13.0, monitor.scale, WEIGHT_SEMIBOLD),
            canvas: None,
            tooltip: None,
        }));
        window::attach(hwnd, &state);
        let hud = Hud { hwnd, generation, state };
        hud.set_mode(mode);
        window::show_no_activate(hwnd);
        Ok(hud)
    }

    pub(crate) fn generation(&self) -> u32 {
        self.generation
    }

    /// Switches what the pill shows (timers and tooltip follow).
    pub(crate) fn set_mode(&self, mode: HudMode) {
        let Ok(mut st) = self.state.try_borrow_mut() else { return };
        let hwnd = st.hwnd;
        // SAFETY: timers on our own window.
        unsafe {
            let _ = KillTimer(Some(hwnd), TICK_TIMER);
            let _ = KillTimer(Some(hwnd), EXPIRE_TIMER);
            match &mode {
                HudMode::Recording { .. } => {
                    SetTimer(Some(hwnd), TICK_TIMER, TICK_MILLIS, None);
                }
                HudMode::Saving => {}
                HudMode::Failed { .. } => {
                    SetTimer(Some(hwnd), EXPIRE_TIMER, FAILED_MILLIS, None);
                }
            }
        }
        st.tooltip = match &mode {
            HudMode::Failed { reason } => window::Tooltip::new(hwnd, reason),
            _ => None,
        };
        st.mode = mode;
        render(&mut st);
    }
}

impl Drop for Hud {
    fn drop(&mut self) {
        if let Ok(mut st) = self.state.try_borrow_mut() {
            st.tooltip = None;
        }
        // SAFETY: destroying our own window on the UI thread (its timers go with it).
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Five bars of a little sound wave, centered at `(cx, cy)`.
pub(crate) fn draw_waveform(pixmap: &mut Pixmap, cx: f64, cy: f64, height: f64, bar: f64, color: Color) {
    const LEVELS: [f64; 5] = [0.45, 0.85, 1.0, 0.65, 0.4];
    let gap = bar * 0.8;
    let total = LEVELS.len() as f64 * bar + (LEVELS.len() - 1) as f64 * gap;
    let mut x = cx - total / 2.0;
    for level in LEVELS {
        let h = (height * level).max(bar);
        pixmap.fill_rounded_rect(Area::new(x, cy - h / 2.0, bar, h), bar / 2.0, color);
        x += bar + gap;
    }
}

fn render(st: &mut HudState) {
    let s = st.scale;
    let (width, height) = size(s);
    if st.canvas.as_ref().is_none_or(|c| c.width() != width || c.height() != height) {
        st.canvas = match Canvas::new(width, height) {
            Ok(canvas) => Some(canvas),
            Err(error) => {
                crate::log_error!("pill canvas: {error}");
                None
            }
        };
    }
    let label = match &st.mode {
        HudMode::Recording { started } => format_timer(started.elapsed().as_secs()),
        HudMode::Saving => "Saving…".to_owned(),
        HudMode::Failed { .. } => "Failed".to_owned(),
    };
    let pulse = 0.675 + 0.325 * (TAU * st.epoch.elapsed().as_secs_f64() / PULSE_SECS).cos();
    let (mode, audio_only, hover, hwnd, position) = (st.mode.clone(), st.audio_only, st.hover, st.hwnd, st.position);
    let Some(canvas) = st.canvas.as_mut() else { return };
    let (w, h) = (width as f64, height as f64);
    {
        let mut p = canvas.pixmap();
        p.fill(if hover { BACKGROUND_HOVER } else { BACKGROUND });
        p.stroke_rounded_rect(Area::new(0.0, 0.0, w, h), h / 2.0, s.max(1.0), BORDER);
        let (cx, cy) = (19.0 * s, h / 2.0);
        let indicator = match mode {
            HudMode::Recording { .. } => RED.faded(pulse),
            HudMode::Saving => GRAY,
            HudMode::Failed { .. } => YELLOW,
        };
        if audio_only && !matches!(mode, HudMode::Failed { .. }) {
            draw_waveform(&mut p, cx, cy, 13.0 * s, 2.0 * s, indicator);
        } else {
            p.fill_circle(cx, cy, 5.0 * s, indicator);
        }
        if matches!(mode, HudMode::Recording { .. }) {
            let (sx, sy) = (w - 20.0 * s, h / 2.0);
            if hover {
                p.fill_circle(sx, sy, 12.0 * s, STOP_HOVER_RING);
            }
            p.fill_rounded_rect(Area::new(sx - 5.0 * s, sy - 5.0 * s, 10.0 * s, 10.0 * s), 2.5 * s, STOP);
        }
    }
    canvas.text(&label, canvas::rect(33.0, 0.0, 66.0, HEIGHT, s), &st.font, TEXT);
    canvas.pixmap().finish_rounded(h / 2.0);
    if let Err(error) = canvas.present(hwnd, position) {
        crate::log_warn!("could not draw the pill: {error}");
    }
}

/// The monitor or its scale changed: resize and move back into the corner.
fn relayout(st: &mut HudState) {
    let monitor = st.placement.monitor();
    if (monitor.scale - st.scale).abs() > f64::EPSILON {
        st.scale = monitor.scale;
        st.font = Font::new(13.0, monitor.scale, WEIGHT_SEMIBOLD);
    }
    let (width, height) = size(st.scale);
    st.position = st.placement.position(&monitor, width, height);
    render(st);
}

extern "system" fn hud_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_SETCURSOR => {
            window::set_hand_cursor();
            return LRESULT(1);
        }
        WM_NCDESTROY => {
            window::detach::<HudState>(hwnd);
        }
        _ => {
            if let Some(state) = window::state::<HudState>(hwnd) {
                if let Ok(mut st) = state.try_borrow_mut() {
                    if let Some(result) = handle(&mut st, message, wparam) {
                        return result;
                    }
                }
            }
        }
    }
    // SAFETY: default handling for everything else.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn handle(st: &mut HudState, message: u32, wparam: WPARAM) -> Option<LRESULT> {
    let hwnd = st.hwnd;
    match message {
        WM_TIMER if wparam.0 == TICK_TIMER => render(st),
        WM_TIMER if wparam.0 == EXPIRE_TIMER => {
            // SAFETY: our own timer.
            unsafe {
                let _ = KillTimer(Some(hwnd), EXPIRE_TIMER);
            }
            post_action(st.notify, UiAction::HudExpired, st.generation);
        }
        WM_MOUSEMOVE => {
            if !st.tracking {
                st.tracking = window::track_leave(hwnd);
            }
            if !st.hover {
                st.hover = true;
                render(st);
            }
        }
        WM_MOUSELEAVE => {
            st.tracking = false;
            st.hover = false;
            render(st);
        }
        WM_LBUTTONDOWN => {
            st.pressed = true;
            // SAFETY: capturing the mouse for our own window during a click.
            unsafe {
                SetCapture(hwnd);
            }
        }
        WM_LBUTTONUP => {
            let was_pressed = std::mem::replace(&mut st.pressed, false);
            // SAFETY: releases our own capture (WM_CAPTURECHANGED re-enters harmlessly).
            unsafe {
                let _ = ReleaseCapture();
            }
            if was_pressed && window::cursor_inside(hwnd) {
                post_action(st.notify, UiAction::HudClicked, st.generation);
            }
        }
        WM_CAPTURECHANGED => st.pressed = false,
        WM_DPICHANGED | WM_DISPLAYCHANGE => relayout(st),
        WM_SETTINGCHANGE if wparam.0 == SPI_SETWORKAREA.0 as usize => relayout(st),
        _ => return None,
    }
    Some(LRESULT(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_uses_the_tedcat_prefix() {
        // SAFETY: CLASS is a static, null-terminated string.
        let name = unsafe { CLASS.to_string() }.unwrap_or_default();
        assert!(name.starts_with(crate::util::win::CLASS_PREFIX), "{name}");
    }

    #[test]
    fn pill_size_scales() {
        assert_eq!(size(1.0), (132, 34));
        assert_eq!(size(2.0), (264, 68));
        assert_eq!(size(1.5), (198, 51));
    }
}
