//! Minimal append-only file logger.
//!
//! Writes to `%LOCALAPPDATA%\TedCat\tedcat.log`; the file is truncated when it
//! grows past 1 MB. Use the `log_info!`, `log_warn!` and `log_error!` macros.
//! Logging never panics and never blocks for long, so it is safe to call from
//! the hook thread (but keep it out of the hot path of every mouse event).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());
const MAX_BYTES: u64 = 1_000_000;

pub fn log_file() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("TedCat").join("tedcat.log"))
}

pub fn write(level: &str, message: &str) {
    let Some(path) = log_file() else { return };
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if fs::metadata(&path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        let _ = fs::remove_file(&path);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let t = super::paths::local_time();
        let _ = writeln!(
            file,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {level:<5} {message}",
            t.year, t.month, t.day, t.hour, t.minute, t.second
        );
    }
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::util::log::write("INFO", &format!($($arg)*)) };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::util::log::write("WARN", &format!($($arg)*)) };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::util::log::write("ERROR", &format!($($arg)*)) };
}
