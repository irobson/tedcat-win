//! Thread setup (COM multithreaded apartment, Media Foundation) and small
//! Win32 helpers shared by the recorder threads.

use std::marker::PhantomData;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::{LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use super::timing;
use crate::{log_error, log_warn};

/// What recording reports when Media Foundation is missing: Windows N and KN
/// editions ship without it until the Media Feature Pack is installed.
const MEDIA_FOUNDATION_MISSING: &str = "Recording needs Windows Media Foundation (install the Media Feature Pack).";

/// The Media Foundation DLLs TedCat calls. TedCat.exe delay-loads them (see
/// `build.rs`), so it starts without them; nothing may call into them before
/// `media_foundation_available` succeeded, or the delay-load helper would
/// raise an exception that ends the process.
const MEDIA_FOUNDATION_DLLS: [&str; 2] = ["mfplat.dll", "mfreadwrite.dll"];

/// Loads the Media Foundation DLLs from System32 and keeps them loaded for the
/// life of the process, so the delay-load helper binds to these copies (it
/// finds a module that is already loaded by name before it searches any
/// folder). Returns a message for the user when one is missing.
pub(super) fn media_foundation_available() -> Result<(), String> {
    static LOADED: AtomicBool = AtomicBool::new(false);
    if LOADED.load(Ordering::Acquire) {
        return Ok(());
    }
    for dll in MEDIA_FOUNDATION_DLLS {
        let name: Vec<u16> = dll.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `name` is NUL-terminated and outlives the call. The module
        // handle is deliberately never freed (see above).
        if let Err(e) = unsafe { LoadLibraryExW(PCWSTR(name.as_ptr()), None, LOAD_LIBRARY_SEARCH_SYSTEM32) } {
            log_error!("{dll} could not be loaded from System32: {}", describe(&e));
            return Err(MEDIA_FOUNDATION_MISSING.into());
        }
    }
    LOADED.store(true, Ordering::Release);
    Ok(())
}

/// Initializes COM (multithreaded apartment, which is also what WinRT needs)
/// and optionally Media Foundation for the current thread, and undoes both on
/// drop. Every COM object the thread created must be released before this
/// guard is dropped. Not `Send`: it belongs to the thread that created it.
pub(super) struct MtaScope {
    media_foundation: bool,
    _thread_bound: PhantomData<*const ()>,
}

impl MtaScope {
    pub(super) fn enter(media_foundation: bool) -> Result<MtaScope, String> {
        if media_foundation {
            media_foundation_available()?;
        }
        // SAFETY: called at the start of a thread the recorder owns, which has
        // not initialized COM yet; balanced by CoUninitialize in Drop.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() {
            return Err(format!("COM could not be initialized (0x{:08X})", hr.0 as u32));
        }
        if media_foundation {
            // SAFETY: MFStartup is reference counted per process; balanced by
            // MFShutdown in Drop. mfplat.dll is loaded (checked above).
            if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
                // SAFETY: balances the successful CoInitializeEx above.
                unsafe { CoUninitialize() };
                return Err(format!("Media Foundation could not be started: {}", describe(&e)));
            }
        }
        Ok(MtaScope { media_foundation, _thread_bound: PhantomData })
    }
}

impl Drop for MtaScope {
    fn drop(&mut self) {
        // SAFETY: balances the successful calls made in `enter` on this thread.
        unsafe {
            if self.media_foundation {
                let _ = MFShutdown();
            }
            CoUninitialize();
        }
    }
}

/// Runs `f` on a short-lived thread inside its own multithreaded apartment,
/// so callers do not depend on the apartment (or lack of one) of their thread.
pub(super) fn on_mta_thread<R, F>(f: F) -> Option<R>
where
    R: Send + 'static,
    F: FnOnce() -> R + Send + 'static,
{
    let worker = std::thread::Builder::new()
        .name("TedCat audio probe".into())
        .spawn(move || {
            let scope = MtaScope::enter(false).ok()?;
            let result = f();
            drop(scope);
            Some(result)
        })
        .ok()?;
    worker.join().ok().flatten()
}

/// The QPC clock in 100 ns units: the same time base as WGC's
/// `SystemRelativeTime` and WASAPI's QPC buffer positions.
pub(super) fn now_hns() -> i64 {
    let (mut counter, mut frequency) = (0i64, 0i64);
    // SAFETY: plain out-parameters; both calls cannot fail on Windows XP and later.
    unsafe {
        let _ = QueryPerformanceCounter(&mut counter);
        let _ = QueryPerformanceFrequency(&mut frequency);
    }
    timing::qpc_to_hns(counter, frequency)
}

/// A readable description of a Windows error: the system message and the HRESULT.
pub(super) fn describe(error: &windows::core::Error) -> String {
    let message = error.message();
    let message = message.trim();
    if message.is_empty() {
        format!("0x{:08X}", error.code().0 as u32)
    } else {
        format!("{message} (0x{:08X})", error.code().0 as u32)
    }
}

/// Deletes an unusable output file. Media Foundation may release its file
/// handle a moment after the sink writer goes away, so this retries briefly.
pub(super) fn delete_output(path: &Path) {
    for attempt in 0..10 {
        match std::fs::remove_file(path) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) if attempt == 9 => log_warn!("could not delete {}: {e}", path.display()),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_windows_n_message_is_a_sentence() {
        assert_eq!(crate::messages::sentence(MEDIA_FOUNDATION_MISSING), MEDIA_FOUNDATION_MISSING);
    }
}
