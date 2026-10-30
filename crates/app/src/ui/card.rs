//! The "recording saved" card: thumbnail, title, file name, details and a
//! "Show in Explorer" link. Clicking the card opens the file, dragging it
//! drags the file into other apps, and it hides itself 8 s after the pointer
//! last left it.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DestroyWindow, GetSystemMetrics, KillTimer, SetTimer, MA_NOACTIVATE, SM_CXDRAG, SM_CYDRAG,
    SPI_SETWORKAREA, WINDOW_EX_STYLE, WM_CAPTURECHANGED, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCDESTROY, WM_SETCURSOR, WM_SETTINGCHANGE, WM_TIMER,
};

use crate::geom::{PointPx, RectPx};
use crate::messages::Thumbnail;
use crate::ui::canvas::{self, Canvas, Font, WEIGHT_REGULAR, WEIGHT_SEMIBOLD};
use crate::ui::layout::px;
use crate::ui::raster::{fit_inside, Area, Color};
use crate::ui::text::{format_bytes, format_duration, middle_ellipsis, shorten_home};
use crate::ui::{dragdrop, hud, post_action, window, Placement, UiAction};

const CLASS: PCWSTR = w!("TedCat.Card");

/// Layout in logical px.
const WIDTH: f64 = 384.0;
const HEIGHT: f64 = 88.0;
const RADIUS: f64 = 10.0;
const THUMB: Area = Area::new(12.0, 12.0, 92.0, 64.0);
const TEXT_X: f64 = 116.0;
const TITLE: Area = Area::new(TEXT_X, 10.0, 232.0, 20.0);
const NAME: Area = Area::new(TEXT_X, 30.0, 232.0, 18.0);
const META: Area = Area::new(TEXT_X, 48.0, 256.0, 16.0);
const LINK: Area = Area::new(TEXT_X, 64.0, 256.0, 16.0);
const CLOSE: Area = Area::new(WIDTH - 8.0 - 24.0, 8.0, 24.0, 24.0);
const LINK_TEXT: &str = "Show in Explorer";
// The title and the file name never run under the close button.
const _: () = assert!(TITLE.x + TITLE.w <= CLOSE.x && NAME.x + NAME.w <= CLOSE.x);

const COUNTDOWN_TIMER: usize = 1;
/// The card hides this long after the pointer left it (or after it appeared away from the pointer).
const COUNTDOWN_MILLIS: u32 = 8000;

const BACKGROUND: Color = Color::rgb(32, 32, 32);
const BACKGROUND_HOVER: Color = Color::rgb(40, 40, 40);
const BORDER: Color = Color::rgba(255, 255, 255, 26);
const THUMB_BACKGROUND: Color = Color::rgb(50, 50, 50);
const TITLE_COLOR: Color = Color::rgb(255, 255, 255);
const NAME_COLOR: Color = Color::rgb(228, 228, 228);
const META_COLOR: Color = Color::rgb(160, 160, 160);
const LINK_COLOR: Color = Color::rgb(96, 205, 255);
const CLOSE_COLOR: Color = Color::rgb(200, 200, 200);
const CLOSE_HOVER: Color = Color::rgba(255, 255, 255, 30);
const WAVE_COLOR: Color = crate::ui::outline::RECORDING_RED;

/// What the card describes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CardInfo {
    pub path: PathBuf,
    pub audio_only: bool,
    pub bytes: u64,
    pub duration_secs: f64,
    pub thumbnail: Option<Thumbnail>,
}

/// The part of the card under the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hit {
    Close,
    Link,
    Thumbnail,
    Body,
}

struct Fonts {
    title: Font,
    name: Font,
    small: Font,
}

impl Fonts {
    fn new(scale: f64) -> Fonts {
        Fonts {
            title: Font::new(13.0, scale, WEIGHT_SEMIBOLD),
            name: Font::new(12.0, scale, WEIGHT_REGULAR),
            small: Font::new(11.0, scale, WEIGHT_REGULAR),
        }
    }
}

struct CardState {
    hwnd: HWND,
    main: HWND,
    generation: u32,
    info: CardInfo,
    placement: Placement,
    scale: f64,
    position: PointPx,
    hover: Option<Hit>,
    tracking: bool,
    pressed: Option<(Hit, i32, i32)>,
    dragging: bool,
    /// Width of the "Show in Explorer" text in physical px (measured when drawn).
    link_width: i32,
    fonts: Fonts,
    canvas: Option<Canvas>,
}

/// The card window; dropping it closes it.
pub(crate) struct SavedCard {
    hwnd: HWND,
    generation: u32,
    path: PathBuf,
    /// Keeps the state alive as long as the card (the window holds another reference).
    _state: Rc<RefCell<CardState>>,
}

fn size(scale: f64) -> (i32, i32) {
    (px(WIDTH, scale), px(HEIGHT, scale))
}

fn start_countdown(hwnd: HWND) {
    // SAFETY: a timer on our own window (re-arming replaces it).
    unsafe {
        SetTimer(Some(hwnd), COUNTDOWN_TIMER, COUNTDOWN_MILLIS, None);
    }
}

fn stop_countdown(hwnd: HWND) {
    // SAFETY: as above.
    unsafe {
        let _ = KillTimer(Some(hwnd), COUNTDOWN_TIMER);
    }
}

impl SavedCard {
    /// Shows the card in `placement`'s corner.
    pub(crate) fn show(
        main: HWND,
        generation: u32,
        info: CardInfo,
        placement: Placement,
    ) -> windows::core::Result<SavedCard> {
        window::register_class(CLASS, Some(card_proc), None)?;
        let monitor = placement.monitor();
        let (width, height) = size(monitor.scale);
        let position = placement.position(&monitor, width, height);
        let hwnd = window::create_floating(
            CLASS,
            WINDOW_EX_STYLE(0),
            RectPx::new(position.x, position.y, position.x + width, position.y + height),
        )?;
        let path = info.path.clone();
        let state = Rc::new(RefCell::new(CardState {
            hwnd,
            main,
            generation,
            info,
            placement,
            scale: monitor.scale,
            position,
            hover: None,
            tracking: false,
            pressed: None,
            dragging: false,
            link_width: 0,
            fonts: Fonts::new(monitor.scale),
            canvas: None,
        }));
        window::attach(hwnd, &state);
        {
            let mut st = state.borrow_mut();
            render(&mut st);
        }
        window::show_no_activate(hwnd);
        refresh_pointer(&state);
        Ok(SavedCard { hwnd, generation, path, _state: state })
    }

    pub(crate) fn generation(&self) -> u32 {
        self.generation
    }

    pub(crate) fn path(&self) -> PathBuf {
        self.path.clone()
    }
}

impl Drop for SavedCard {
    fn drop(&mut self) {
        // SAFETY: destroying our own window on the UI thread (its timer goes with it).
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Re-checks whether the pointer is over the card: while it is, the
/// countdown is paused; once it is not, the countdown runs.
fn refresh_pointer(state: &Rc<RefCell<CardState>>) {
    let Ok(mut st) = state.try_borrow_mut() else { return };
    let hwnd = st.hwnd;
    if window::cursor_inside(hwnd) {
        stop_countdown(hwnd);
        st.tracking = window::track_leave(hwnd);
    } else {
        st.tracking = false;
        if st.hover.take().is_some() {
            render(&mut st);
        }
        start_countdown(hwnd);
    }
}

fn hit_test(st: &CardState, x: i32, y: i32) -> Hit {
    let (lx, ly) = (x as f64 / st.scale, y as f64 / st.scale);
    let link = Area::new(LINK.x, LINK.y, st.link_width as f64 / st.scale, LINK.h);
    if CLOSE.contains(lx, ly) {
        Hit::Close
    } else if link.contains(lx, ly) {
        Hit::Link
    } else if THUMB.contains(lx, ly) {
        Hit::Thumbnail
    } else {
        Hit::Body
    }
}

/// `C:\Users\me\Videos\TedCat` as `~\Videos\TedCat`.
fn folder_label(info: &CardInfo) -> String {
    let folder = info.path.parent().map(|p| p.display().to_string()).unwrap_or_default();
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    shorten_home(&folder, &home)
}

fn render(st: &mut CardState) {
    let s = st.scale;
    let (width, height) = size(s);
    if st.canvas.as_ref().is_none_or(|c| c.width() != width || c.height() != height) {
        st.canvas = match Canvas::new(width, height) {
            Ok(canvas) => Some(canvas),
            Err(error) => {
                crate::log_error!("card canvas: {error}");
                None
            }
        };
    }
    let Some(canvas) = st.canvas.as_mut() else { return };
    let fonts = &st.fonts;
    let info = &st.info;
    let hover = st.hover;
    let scaled = |a: Area| Area::new(a.x * s, a.y * s, a.w * s, a.h * s);

    // Texts, fitted with real measurements.
    let title = if info.audio_only { "Audio recording saved" } else { "Screen recording saved" };
    let file_name = info.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let name = middle_ellipsis(&file_name, px(NAME.w, s), |t| canvas.measure(t, &fonts.name));
    let suffix = format!(" · {} · {}", format_bytes(info.bytes), format_duration(info.duration_secs));
    let folder_room = px(META.w, s) - canvas.measure(&suffix, &fonts.small);
    let folder = middle_ellipsis(&folder_label(info), folder_room, |t| canvas.measure(t, &fonts.small));
    let meta = format!("{folder}{suffix}");
    let link_width = canvas.measure(LINK_TEXT, &fonts.small).min(px(LINK.w, s));

    let (w, h) = (width as f64, height as f64);
    {
        let mut p = canvas.pixmap();
        p.fill(if hover.is_some() { BACKGROUND_HOVER } else { BACKGROUND });
        p.stroke_rounded_rect(Area::new(0.0, 0.0, w, h), RADIUS * s, s.max(1.0), BORDER);
        let thumb = scaled(THUMB);
        p.fill_rounded_rect(thumb, 6.0 * s, THUMB_BACKGROUND);
        match &info.thumbnail {
            Some(t) if !info.audio_only => {
                let (fw, fh) = fit_inside(t.width, t.height, thumb.w, thumb.h);
                let image = Area::new(thumb.x + (thumb.w - fw) / 2.0, thumb.y + (thumb.h - fh) / 2.0, fw, fh);
                p.draw_image(image, 6.0 * s, &t.bgra, t.width as usize, t.height as usize);
            }
            _ if info.audio_only => {
                hud::draw_waveform(
                    &mut p,
                    thumb.x + thumb.w / 2.0,
                    thumb.y + thumb.h / 2.0,
                    34.0 * s,
                    5.0 * s,
                    WAVE_COLOR,
                );
            }
            _ => {
                // No frame was captured: a simple "play" glyph.
                let (cx, cy, r) = (thumb.x + thumb.w / 2.0, thumb.y + thumb.h / 2.0, 9.0 * s);
                let (a, b, c) = ((cx - r * 0.6, cy - r), (cx - r * 0.6, cy + r), (cx + r, cy));
                for (from, to) in [(a, b), (b, c), (c, a)] {
                    p.stroke_line(from, to, 2.0 * s, META_COLOR);
                }
            }
        }
        let close = scaled(CLOSE);
        let (cx, cy) = (close.x + close.w / 2.0, close.y + close.h / 2.0);
        if hover == Some(Hit::Close) {
            p.fill_circle(cx, cy, close.w / 2.0, CLOSE_HOVER);
        }
        let arm = 4.0 * s;
        p.stroke_line((cx - arm, cy - arm), (cx + arm, cy + arm), 1.5 * s, CLOSE_COLOR);
        p.stroke_line((cx - arm, cy + arm), (cx + arm, cy - arm), 1.5 * s, CLOSE_COLOR);
    }
    let rect = |a: Area| canvas::rect(a.x, a.y, a.w, a.h, s);
    canvas.text(title, rect(TITLE), &fonts.title, TITLE_COLOR);
    canvas.text(&name, rect(NAME), &fonts.name, NAME_COLOR);
    canvas.text(&meta, rect(META), &fonts.small, META_COLOR);
    canvas.text(LINK_TEXT, rect(LINK), &fonts.small, LINK_COLOR);
    {
        let mut p = canvas.pixmap();
        if hover == Some(Hit::Link) {
            let y = (LINK.y + LINK.h - 2.0) * s;
            let x = LINK.x * s;
            p.stroke_line((x, y), (x + link_width as f64, y), s.max(1.0), LINK_COLOR);
        }
        p.finish_rounded(RADIUS * s);
    }
    if let Err(error) = canvas.present(st.hwnd, st.position) {
        crate::log_warn!("could not draw the card: {error}");
    }
    st.link_width = link_width;
}

fn relayout(st: &mut CardState) {
    let monitor = st.placement.monitor();
    if (monitor.scale - st.scale).abs() > f64::EPSILON {
        st.scale = monitor.scale;
        st.fonts = Fonts::new(monitor.scale);
    }
    let (width, height) = size(st.scale);
    st.position = st.placement.position(&monitor, width, height);
    render(st);
}

/// Runs an OLE drag of the file; the card stays up and its countdown is paused meanwhile.
fn drag(state: &Rc<RefCell<CardState>>) {
    let (hwnd, path) = {
        let Ok(mut st) = state.try_borrow_mut() else { return };
        st.dragging = true;
        st.pressed = None;
        (st.hwnd, st.info.path.clone())
    };
    stop_countdown(hwnd);
    // SAFETY: gives up our click capture; OLE takes the mouse for the drag.
    unsafe {
        let _ = ReleaseCapture();
    }
    // No borrow is held here: the drag runs a modal loop that dispatches messages to this window.
    match dragdrop::drag_file(hwnd, &path) {
        Ok(effect) => crate::log_info!("drag of {} ended (effect {})", path.display(), effect.0),
        Err(error) => crate::log_warn!("could not drag {}: {error}", path.display()),
    }
    if let Ok(mut st) = state.try_borrow_mut() {
        st.dragging = false;
    }
    refresh_pointer(state);
}

extern "system" fn card_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_SETCURSOR => {
            window::set_hand_cursor();
            return LRESULT(1);
        }
        WM_NCDESTROY => {
            window::detach::<CardState>(hwnd);
        }
        _ => {
            if let Some(state) = window::state::<CardState>(hwnd) {
                if let Some(result) = handle(&state, message, wparam, lparam) {
                    return result;
                }
            }
        }
    }
    // SAFETY: default handling for everything else.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn handle(state: &Rc<RefCell<CardState>>, message: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    let Ok(mut st) = state.try_borrow_mut() else { return None };
    let hwnd = st.hwnd;
    match message {
        WM_TIMER if wparam.0 == COUNTDOWN_TIMER => {
            stop_countdown(hwnd);
            if !st.dragging {
                post_action(st.main, UiAction::CardExpired, st.generation);
            }
        }
        WM_MOUSEMOVE => {
            let (x, y) = window::lparam_point(lparam);
            if !st.tracking && !st.dragging {
                stop_countdown(hwnd);
                st.tracking = window::track_leave(hwnd);
            }
            if let Some((hit, x0, y0)) = st.pressed {
                // SAFETY: plain metrics queries.
                let (dx, dy) = unsafe { (GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG)) };
                if matches!(hit, Hit::Thumbnail | Hit::Body) && ((x - x0).abs() > dx || (y - y0).abs() > dy) {
                    drop(st);
                    drag(state);
                    return Some(LRESULT(0));
                }
            }
            let hit = Some(hit_test(&st, x, y));
            if st.hover != hit {
                st.hover = hit;
                render(&mut st);
            }
        }
        WM_MOUSELEAVE => {
            st.tracking = false;
            if st.pressed.is_none() && !st.dragging {
                st.hover = None;
                render(&mut st);
                start_countdown(hwnd);
            }
        }
        WM_LBUTTONDOWN => {
            let (x, y) = window::lparam_point(lparam);
            st.pressed = Some((hit_test(&st, x, y), x, y));
            // SAFETY: capturing the mouse for our own window during a click.
            unsafe {
                SetCapture(hwnd);
            }
        }
        WM_LBUTTONUP => {
            let (x, y) = window::lparam_point(lparam);
            let pressed = st.pressed.take();
            let release_hit = hit_test(&st, x, y);
            let inside = window::cursor_inside(hwnd);
            let (main, generation) = (st.main, st.generation);
            drop(st);
            // SAFETY: releases our own capture (WM_CAPTURECHANGED re-enters harmlessly).
            unsafe {
                let _ = ReleaseCapture();
            }
            if let Some((hit, _, _)) = pressed {
                let same = hit == release_hit
                    || (matches!(hit, Hit::Thumbnail | Hit::Body) && matches!(release_hit, Hit::Thumbnail | Hit::Body));
                if inside && same {
                    let action = match hit {
                        Hit::Close => UiAction::CardClose,
                        Hit::Link => UiAction::CardReveal,
                        Hit::Thumbnail | Hit::Body => UiAction::CardOpen,
                    };
                    post_action(main, action, generation);
                }
            }
            refresh_pointer(state);
        }
        WM_CAPTURECHANGED => st.pressed = None,
        WM_DPICHANGED | WM_DISPLAYCHANGE => relayout(&mut st),
        WM_SETTINGCHANGE if wparam.0 == SPI_SETWORKAREA.0 as usize => relayout(&mut st),
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
    fn layout_fits_inside_the_card() {
        for area in [THUMB, TITLE, NAME, META, LINK, CLOSE] {
            assert!(area.x >= 0.0 && area.y >= 0.0, "{area:?}");
            assert!(area.x + area.w <= WIDTH && area.y + area.h <= HEIGHT, "{area:?}");
        }
    }

    #[test]
    fn card_size_scales() {
        assert_eq!(size(1.0), (384, 88));
        assert_eq!(size(1.25), (480, 110));
    }
}
