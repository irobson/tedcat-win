//! "Launch at Login": the `TedCat` value under
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

use std::path::Path;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ,
};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
/// Where Task Manager and Settings > Apps > Startup record a disabled entry.
const APPROVED_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run");
const VALUE_NAME: PCWSTR = w!("TedCat");

/// The command line stored in the Run key: the quoted executable path.
pub(crate) fn run_command(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// True when a Run value launches `exe` (quotes, case and surrounding spaces ignored).
pub(crate) fn command_matches(value: &str, exe: &Path) -> bool {
    let value = value.trim();
    let path = if let Some(rest) = value.strip_prefix('"') { rest.split('"').next().unwrap_or("") } else { value };
    path.eq_ignore_ascii_case(&exe.display().to_string())
}

/// Startup-approved data: an odd first byte means the user disabled the entry.
pub(crate) fn approved_disables(data: &[u8]) -> bool {
    data.first().is_some_and(|first| first & 1 == 1)
}

fn current_exe() -> Result<std::path::PathBuf, String> {
    std::env::current_exe().map_err(|error| format!("could not find TedCat.exe: {error}"))
}

fn read_run_value() -> Option<String> {
    let mut buffer = [0u16; 1024];
    let mut size = std::mem::size_of_val(&buffer) as u32;
    // SAFETY: the buffer and its size in bytes are passed together.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE_NAME,
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    (status == ERROR_SUCCESS).then(|| crate::util::from_wide(&buffer))
}

fn disabled_by_user() -> bool {
    let mut data = [0u8; 16];
    let mut size = data.len() as u32;
    // SAFETY: as above.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            APPROVED_KEY,
            VALUE_NAME,
            RRF_RT_REG_BINARY,
            None,
            Some(data.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    status == ERROR_SUCCESS && approved_disables(&data[..(size as usize).min(data.len())])
}

/// True when this TedCat.exe starts at login.
pub(crate) fn is_enabled() -> bool {
    let Ok(exe) = current_exe() else { return false };
    read_run_value().is_some_and(|value| command_matches(&value, &exe)) && !disabled_by_user()
}

fn check(status: WIN32_ERROR, what: &str) -> Result<(), String> {
    if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(format!("could not {what} (error {})", status.0))
    }
}

/// Turns launching at login on (for this TedCat.exe) or off. The error is a
/// sentence for the user (`messages::sentence`).
pub(crate) fn set_enabled(enabled: bool) -> Result<(), String> {
    write_run_key(enabled).map_err(|error| crate::messages::sentence(&error))
}

fn write_run_key(enabled: bool) -> Result<(), String> {
    if enabled {
        let command = crate::util::wide(&run_command(&current_exe()?));
        // SAFETY: the data pointer and byte length describe `command`, including its null.
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                VALUE_NAME,
                REG_SZ.0,
                Some(command.as_ptr() as *const _),
                (command.len() * 2) as u32,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(format!("could not write the Run key (error {})", status.0));
        }
        // Clear a "disabled" mark left by Task Manager so the entry really runs.
        // SAFETY: deletes one value under HKCU; a missing value is fine.
        check(unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, APPROVED_KEY, VALUE_NAME) }, "reset the startup approval")
    } else {
        // SAFETY: as above.
        check(unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE_NAME) }, "remove the Run key")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn the_run_command_is_quoted() {
        let exe = PathBuf::from(r"C:\Program Files\TedCat\TedCat.exe");
        assert_eq!(run_command(&exe), r#""C:\Program Files\TedCat\TedCat.exe""#);
        assert!(command_matches(&run_command(&exe), &exe));
    }

    #[test]
    fn matching_ignores_quotes_case_and_spaces() {
        let exe = PathBuf::from(r"C:\Tools\TedCat.exe");
        assert!(command_matches(r"c:\tools\tedcat.exe", &exe));
        assert!(command_matches(r#"  "C:\Tools\TedCat.exe" --flag "#, &exe));
        assert!(!command_matches(r#""D:\Old\TedCat.exe""#, &exe));
        assert!(!command_matches("", &exe));
    }

    #[test]
    fn startup_approval_bytes() {
        assert!(!approved_disables(&[0x02, 0, 0, 0]));
        assert!(!approved_disables(&[0x06, 0, 0, 0]));
        assert!(approved_disables(&[0x03, 0, 0, 0]));
        assert!(!approved_disables(&[]));
    }
}
