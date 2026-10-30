//! Pure text helpers for the tray menu, the recording pill and the saved card.

use crate::settings::Trigger;

/// The single-character ellipsis used everywhere.
pub(crate) const ELLIPSIS: char = '…';

/// The recording timer: `"00:07"`, `"12:34"`, and `"1:02:03"` past an hour.
pub(crate) fn format_timer(total_secs: u64) -> String {
    let (hours, minutes, seconds) = (total_secs / 3600, (total_secs / 60) % 60, total_secs % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

/// A finished recording's length: `"0:07"`, `"12:34"`, `"1:02:03"` (rounded to the second).
pub(crate) fn format_duration(secs: f64) -> String {
    let total = if secs.is_finite() && secs > 0.0 { secs.round() as u64 } else { 0 };
    let (hours, minutes, seconds) = (total / 3600, (total / 60) % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// A file size in decimal units, like the macOS app: `"512 bytes"`, `"842 KB"`, `"12.3 MB"`, `"1.25 GB"`.
pub(crate) fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1_000.0;
    const MB: f64 = 1_000_000.0;
    const GB: f64 = 1_000_000_000.0;
    let b = bytes as f64;
    if bytes == 1 {
        "1 byte".to_owned()
    } else if b < KB {
        format!("{bytes} bytes")
    } else if b < MB - 500.0 {
        format!("{} KB", ((b / KB).round() as u64).max(1))
    } else if b < GB - 50_000_000.0 {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.2} GB", b / GB)
    }
}

/// Shortens `text` with an ellipsis in the middle until `measure` says it fits
/// in `max_width`. The end (date and extension of a file name) is kept a
/// little longer than the start. Returns the text unchanged when it fits.
pub(crate) fn middle_ellipsis(text: &str, max_width: i32, measure: impl Fn(&str) -> i32) -> String {
    if measure(text) <= max_width {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let candidate = |keep: usize| -> String {
        let tail = keep.div_ceil(2);
        let head = keep - tail;
        let mut s: String = chars[..head].iter().collect();
        s.push(ELLIPSIS);
        s.extend(chars[chars.len() - tail..].iter());
        s
    };
    // Largest number of kept characters that still fits (binary search; width grows with `keep`).
    let (mut low, mut high) = (0usize, chars.len().saturating_sub(1));
    if measure(&candidate(0)) > max_width {
        return ELLIPSIS.to_string();
    }
    while low < high {
        let mid = (low + high).div_ceil(2);
        if measure(&candidate(mid)) <= max_width {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    candidate(low)
}

/// Replaces the user's profile folder at the start of `path` with `~`
/// (case-insensitive, as Windows paths are): `C:\Users\rob\Videos` -> `~\Videos`.
pub(crate) fn shorten_home(path: &str, home: &str) -> String {
    let home = home.trim_end_matches(['\\', '/']);
    if home.is_empty() || path.len() < home.len() || !path.is_char_boundary(home.len()) {
        return path.to_owned();
    }
    let (start, rest) = path.split_at(home.len());
    if !start.eq_ignore_ascii_case(home) && start.to_lowercase() != home.to_lowercase() {
        return path.to_owned();
    }
    if rest.is_empty() {
        "~".to_owned()
    } else if rest.starts_with(['\\', '/']) {
        format!("~{rest}")
    } else {
        path.to_owned()
    }
}

/// The two gesture hint lines of the tray menu for `trigger`.
pub(crate) fn gesture_hints(trigger: Trigger) -> [String; 2] {
    let prefix = trigger.key_name().map(|key| format!("{key} + ")).unwrap_or_default();
    [
        format!("{prefix}Hold, drag: screenshot to clipboard"),
        format!("{prefix}Shift + Hold, drag: record video of the area"),
    ]
}

/// A null-terminated UTF-16 copy of `s` for a fixed-size Win32 buffer,
/// truncated (with an ellipsis, never splitting a surrogate pair) if needed.
pub(crate) fn fixed_wide<const N: usize>(s: &str) -> [u16; N] {
    let mut out = [0u16; N];
    if N == 0 {
        return out;
    }
    let units: Vec<u16> = s.encode_utf16().collect();
    let capacity = N - 1;
    if units.len() <= capacity {
        out[..units.len()].copy_from_slice(&units);
        return out;
    }
    if capacity == 0 {
        return out;
    }
    let mut keep = capacity - 1;
    // Do not end on a high surrogate.
    if keep > 0 && (0xD800..0xDC00).contains(&units[keep - 1]) {
        keep -= 1;
    }
    out[..keep].copy_from_slice(&units[..keep]);
    out[keep] = ELLIPSIS as u16;
    out
}

/// The first free recording file name: `name(1)`, `name(2)`, ... skipping
/// names for which `exists` returns true. Gives up after 999 attempts and
/// returns the last candidate.
pub(crate) fn unique_name(name: impl Fn(u32) -> String, exists: impl Fn(&str) -> bool) -> String {
    for attempt in 1..999 {
        let candidate = name(attempt);
        if !exists(&candidate) {
            return candidate;
        }
    }
    name(999)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_is_minutes_and_seconds() {
        assert_eq!(format_timer(0), "00:00");
        assert_eq!(format_timer(7), "00:07");
        assert_eq!(format_timer(65), "01:05");
        assert_eq!(format_timer(59 * 60 + 59), "59:59");
        assert_eq!(format_timer(3600), "1:00:00");
        assert_eq!(format_timer(3723), "1:02:03");
    }

    #[test]
    fn durations_round_to_the_second() {
        assert_eq!(format_duration(0.0), "0:00");
        assert_eq!(format_duration(6.6), "0:07");
        assert_eq!(format_duration(754.0), "12:34");
        assert_eq!(format_duration(3723.2), "1:02:03");
        assert_eq!(format_duration(-3.0), "0:00");
        assert_eq!(format_duration(f64::NAN), "0:00");
    }

    #[test]
    fn byte_sizes_use_decimal_units() {
        assert_eq!(format_bytes(0), "0 bytes");
        assert_eq!(format_bytes(1), "1 byte");
        assert_eq!(format_bytes(999), "999 bytes");
        assert_eq!(format_bytes(1_000), "1 KB");
        assert_eq!(format_bytes(842_400), "842 KB");
        assert_eq!(format_bytes(999_400), "999 KB");
        assert_eq!(format_bytes(999_600), "1.0 MB");
        assert_eq!(format_bytes(12_345_678), "12.3 MB");
        assert_eq!(format_bytes(1_250_000_000), "1.25 GB");
    }

    fn width(s: &str) -> i32 {
        s.chars().count() as i32
    }

    #[test]
    fn middle_ellipsis_keeps_short_text() {
        assert_eq!(middle_ellipsis("short.mp4", 20, width), "short.mp4");
        assert_eq!(middle_ellipsis("exactly10!", 10, width), "exactly10!");
    }

    #[test]
    fn middle_ellipsis_cuts_the_middle_and_fits() {
        let name = "TedCat 2026-10-30 at 14.03.22.mp4";
        let short = middle_ellipsis(name, 15, width);
        assert_eq!(width(&short), 15);
        assert!(short.starts_with("TedCat"));
        assert!(short.ends_with("22.mp4"));
        assert_eq!(short.matches(ELLIPSIS).count(), 1);
    }

    #[test]
    fn middle_ellipsis_degrades_to_a_lone_ellipsis() {
        assert_eq!(middle_ellipsis("abcdef", 1, width), "…");
        assert_eq!(middle_ellipsis("abcdef", 0, width), "…");
        assert_eq!(middle_ellipsis("abcdef", 2, width), "…f");
    }

    #[test]
    fn middle_ellipsis_handles_multibyte_text() {
        let short = middle_ellipsis("Gravação de ação.m4a", 9, width);
        assert_eq!(width(&short), 9);
        assert!(short.ends_with(".m4a"));
    }

    #[test]
    fn home_folder_becomes_a_tilde() {
        let home = r"C:\Users\rob";
        assert_eq!(shorten_home(r"C:\Users\rob\Videos\TedCat", home), r"~\Videos\TedCat");
        assert_eq!(shorten_home(r"c:\users\ROB\Videos", home), r"~\Videos");
        assert_eq!(shorten_home(r"C:\Users\rob", home), "~");
        assert_eq!(shorten_home(r"C:\Users\robert\Videos", home), r"C:\Users\robert\Videos");
        assert_eq!(shorten_home(r"D:\Clips", home), r"D:\Clips");
        assert_eq!(shorten_home(r"C:\Users\rob\Videos", r"C:\Users\rob\"), r"~\Videos");
        assert_eq!(shorten_home(r"D:\Clips", ""), r"D:\Clips");
        assert_eq!(shorten_home("C:", home), "C:");
    }

    #[test]
    fn hints_follow_the_trigger() {
        assert_eq!(
            gesture_hints(Trigger::HoldOnly),
            [
                "Hold, drag: screenshot to clipboard".to_owned(),
                "Shift + Hold, drag: record video of the area".to_owned()
            ]
        );
        assert_eq!(
            gesture_hints(Trigger::Control),
            [
                "Ctrl + Hold, drag: screenshot to clipboard".to_owned(),
                "Ctrl + Shift + Hold, drag: record video of the area".to_owned()
            ]
        );
        assert!(gesture_hints(Trigger::Alt)[1].starts_with("Alt + Shift + Hold"));
        assert!(gesture_hints(Trigger::Win)[0].starts_with("Win + Hold"));
    }

    #[test]
    fn fixed_wide_copies_and_terminates() {
        let buf: [u16; 8] = fixed_wide("TedCat");
        assert_eq!(&buf[..7], &[84, 101, 100, 67, 97, 116, 0]);
        let exact: [u16; 4] = fixed_wide("abc");
        assert_eq!(exact, [97, 98, 99, 0]);
    }

    #[test]
    fn fixed_wide_truncates_with_an_ellipsis() {
        let buf: [u16; 5] = fixed_wide("abcdefgh");
        assert_eq!(buf, [97, 98, 99, ELLIPSIS as u16, 0]);
        // "ab😀cd": the emoji is a surrogate pair at units 2-3; it must not be split.
        let emoji: [u16; 5] = fixed_wide("ab\u{1F600}cd");
        assert_eq!(emoji, [97, 98, ELLIPSIS as u16, 0, 0]);
        let tiny: [u16; 1] = fixed_wide("abc");
        assert_eq!(tiny, [0]);
    }

    #[test]
    fn unique_name_skips_existing_files() {
        let name = |attempt: u32| if attempt <= 1 { "a.mp4".to_owned() } else { format!("a ({attempt}).mp4") };
        assert_eq!(unique_name(name, |_| false), "a.mp4");
        assert_eq!(unique_name(name, |n| n == "a.mp4" || n == "a (2).mp4"), "a (3).mp4");
        assert_eq!(unique_name(name, |_| true), "a (999).mp4");
    }
}
