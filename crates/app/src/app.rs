//! The UI-thread controller.
//!
//! CONTRACT (owned by the UI agent): `run` initializes COM (apartment threaded)
//! and Media Foundation users may assume COM is initialized on the UI thread;
//! enforces a single instance (named mutex `Local\TedCat.SingleInstance`; a
//! second launch exits quietly); creates the hidden main window (class
//! `TedCat.Main`) whose window procedure handles `WM_APP_GESTURE`,
//! `WM_APP_HOOK_STATUS`, `WM_APP_RECORDER` and `WM_APP_TRAY`; starts the input
//! hook; and runs the message loop until Quit. It returns the process exit code.
//!
//! On quit while recording, it stops and finalizes the recording before exiting.

/// Runs TedCat until the user quits. Returns the process exit code.
pub fn run() -> i32 {
    todo!("UI agent")
}
