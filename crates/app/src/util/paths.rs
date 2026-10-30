//! Well-known folders and the local clock.

use std::path::PathBuf;

use tedcat_core::Timestamp;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Shell::{FOLDERID_Videos, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

/// `%APPDATA%\TedCat`, where settings live.
pub fn app_data_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("TedCat")
}

/// The user's Videos folder (it may be redirected, e.g. to OneDrive).
pub fn videos_dir() -> PathBuf {
    unsafe {
        if let Ok(path) = SHGetKnownFolderPath(&FOLDERID_Videos, KF_FLAG_DEFAULT, None) {
            let result = path.to_string().ok().map(PathBuf::from);
            CoTaskMemFree(Some(path.0 as *const _));
            if let Some(result) = result {
                return result;
            }
        }
    }
    std::env::var_os("USERPROFILE")
        .map(|p| PathBuf::from(p).join("Videos"))
        .unwrap_or_else(std::env::temp_dir)
}

/// Default recordings folder: `Videos\TedCat`.
pub fn default_recordings_dir() -> PathBuf {
    videos_dir().join("TedCat")
}

/// The local wall-clock time.
pub fn local_time() -> Timestamp {
    let t = unsafe { GetLocalTime() };
    Timestamp {
        year: t.wYear,
        month: t.wMonth as u8,
        day: t.wDay as u8,
        hour: t.wHour as u8,
        minute: t.wMinute as u8,
        second: t.wSecond as u8,
    }
}
