//! Opening files and folders with the shell. Errors are sentences for the
//! user (`messages::sentence`).

use std::path::Path;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::UI::Shell::{ILCreateFromPathW, ILFree, SHOpenFolderAndSelectItems, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Opens `path` (a file or a folder) with its default handler.
pub(crate) fn open(path: &Path) -> Result<(), String> {
    let target = HSTRING::from(path.as_os_str());
    // SAFETY: plain ShellExecuteW with null-terminated strings that outlive the call.
    let result = unsafe { ShellExecuteW(None, w!("open"), &target, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    // ShellExecuteW returns a value greater than 32 on success.
    if result.0 as isize > 32 {
        Ok(())
    } else {
        Err(format!("Windows could not open {} (error {}).", path.display(), result.0 as isize))
    }
}

/// Opens an Explorer window on the folder of `path` with the file selected.
pub(crate) fn reveal(path: &Path) -> Result<(), String> {
    let target = HSTRING::from(path.as_os_str());
    // SAFETY: the PIDL comes from ILCreateFromPathW and is freed with ILFree after use.
    unsafe {
        let pidl = ILCreateFromPathW(&target);
        if !pidl.is_null() {
            let result = SHOpenFolderAndSelectItems(pidl, None, 0);
            ILFree(Some(pidl));
            if result.is_ok() {
                return Ok(());
            }
        }
    }
    // Fallback: explorer.exe /select,"path".
    let arguments = HSTRING::from(format!("/select,\"{}\"", path.display()));
    // SAFETY: as in `open`.
    let result =
        unsafe { ShellExecuteW(None, w!("open"), w!("explorer.exe"), &arguments, PCWSTR::null(), SW_SHOWNORMAL) };
    if result.0 as isize > 32 {
        Ok(())
    } else {
        Err(format!("Windows could not show {} in Explorer.", path.display()))
    }
}

/// Opens a text file (the log) with its default app, falling back to Notepad
/// when `.log` has no association.
pub(crate) fn open_text(path: &Path) -> Result<(), String> {
    if open(path).is_ok() {
        return Ok(());
    }
    let arguments = HSTRING::from(format!("\"{}\"", path.display()));
    // SAFETY: as in `open`.
    let result =
        unsafe { ShellExecuteW(None, w!("open"), w!("notepad.exe"), &arguments, PCWSTR::null(), SW_SHOWNORMAL) };
    if result.0 as isize > 32 {
        Ok(())
    } else {
        Err(format!("Windows could not open {}.", path.display()))
    }
}
