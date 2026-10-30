//! Recording file names: `TedCat 2026-10-30 at 14.03.22.mp4`, matching the
//! macOS app (and macOS screenshot naming).

/// A local wall-clock time, supplied by the caller so this stays pure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timestamp {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// The file name for a recording.
///
/// `attempt` starts at 1; when a file with that name already exists the
/// caller retries with 2, 3, ... which appends ` (2)`, ` (3)`, ...
/// `extension` is given without the dot (`"mp4"`, `"m4a"`).
pub fn recording_file_name(stamp: Timestamp, extension: &str, attempt: u32) -> String {
    let base = format!(
        "TedCat {:04}-{:02}-{:02} at {:02}.{:02}.{:02}",
        stamp.year, stamp.month, stamp.day, stamp.hour, stamp.minute, stamp.second
    );
    if attempt <= 1 {
        format!("{base}.{extension}")
    } else {
        format!("{base} ({attempt}).{extension}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAMP: Timestamp = Timestamp { year: 2026, month: 10, day: 4, hour: 9, minute: 3, second: 7 };

    #[test]
    fn matches_the_macos_naming() {
        assert_eq!(recording_file_name(STAMP, "mp4", 1), "TedCat 2026-10-04 at 09.03.07.mp4");
        assert_eq!(recording_file_name(STAMP, "m4a", 1), "TedCat 2026-10-04 at 09.03.07.m4a");
    }

    #[test]
    fn collisions_get_a_counter() {
        assert_eq!(recording_file_name(STAMP, "mp4", 2), "TedCat 2026-10-04 at 09.03.07 (2).mp4");
    }
}
