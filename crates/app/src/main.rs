//! TedCat for Windows.
//!
//! - Hold the left mouse button still for a moment, drag, release: screenshot
//!   of the area to the clipboard.
//! - Shift + hold, drag, release: video of the area with system audio (MP4).
//! - Tray menu > Record Audio Only: the computer's audio (M4A).
//!
//! Module map (see docs/ARCHITECTURE.md):
//! - `app`      UI-thread controller: owns every window and reacts to messages.
//! - `input`    low-level hooks (hold-and-replay) and the selection overlay.
//! - `capture`  screenshots to the clipboard.
//! - `record`   Windows.Graphics.Capture + WASAPI loopback + Media Foundation.
//! - `ui`       tray icon and menu, recording pill, region outline, saved card.
//! - `settings` persisted preferences.
//! - `geom`, `messages`, `util`  shared plumbing.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod geom;
#[cfg(windows)]
mod input;
#[cfg(windows)]
mod messages;
#[cfg(windows)]
mod record;
#[cfg(windows)]
mod settings;
#[cfg(windows)]
mod ui;
#[cfg(windows)]
mod util;

#[cfg(windows)]
fn main() {
    std::process::exit(app::run());
}

#[cfg(not(windows))]
fn main() {
    eprintln!("TedCat for Windows only runs on Windows. Build with --target x86_64-pc-windows-gnu or on Windows.");
    std::process::exit(1);
}
