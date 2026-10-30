//! Pure timing logic for the recorder: clock conversions, the 30 fps frame
//! throttle, the end-of-recording tail and the audio timeline.
//!
//! Every time value here is in 100-nanosecond units ("hns"), the time base of
//! Media Foundation, WinRT `TimeSpan` and the QPC positions WASAPI reports.
//! Nothing in this file touches Win32, so it is unit tested on any OS.

/// 100-nanosecond units per second.
pub(crate) const HNS_PER_SECOND: i64 = 10_000_000;

/// Converts a `QueryPerformanceCounter` reading to 100 ns units.
///
/// Uses 128-bit intermediate math: `counter * 10^7` overflows `i64` after a
/// few days of uptime with a 10 MHz counter.
pub(crate) fn qpc_to_hns(counter: i64, frequency: i64) -> i64 {
    if frequency <= 0 {
        return 0;
    }
    ((counter as i128 * HNS_PER_SECOND as i128) / frequency as i128) as i64
}

/// The index of the frame (at `rate` frames per second) that contains `hns`.
/// Rounds toward negative infinity, so times before zero map to negative frames.
pub(crate) fn hns_to_frames(hns: i64, rate: u32) -> i64 {
    (hns as i128 * rate as i128).div_euclid(HNS_PER_SECOND as i128) as i64
}

/// The start time of frame `frame` at `rate` frames per second, in 100 ns.
pub(crate) fn frames_to_hns(frame: i64, rate: u32) -> i64 {
    (frame as i128 * HNS_PER_SECOND as i128).div_euclid(rate.max(1) as i128) as i64
}

/// Start time and duration of `count` frames starting at frame `start`.
/// Computed from absolute positions so rounding never accumulates.
pub(crate) fn frame_span(start: i64, count: u64, rate: u32) -> (i64, i64) {
    let begin = frames_to_hns(start, rate);
    let end = frames_to_hns(start + count as i64, rate);
    (begin, end - begin)
}

/// Picks which captured frames get encoded so the video never exceeds `fps`.
///
/// Windows.Graphics.Capture delivers a frame per composition (up to the
/// monitor refresh rate) and only when something changed. The throttle keeps
/// a schedule of due times one interval apart: a frame is kept once the next
/// due time is reached (with 1 ms of jitter tolerance, so a 60.001 Hz display
/// still yields 30 fps rather than 20), and each kept frame moves the due time
/// one interval further. That caps the average at `fps` on any refresh rate
/// (75 Hz and 144 Hz displays still average close to 30 fps instead of 25 or
/// 28), while two kept frames are never closer than about 3/4 of an interval.
/// After a pause (static screen) the schedule restarts from the new frame
/// instead of bursting to catch up.
#[derive(Clone, Debug)]
pub(crate) struct FrameThrottle {
    interval: i64,
    min_gap: i64,
    due: Option<i64>,
}

impl FrameThrottle {
    const JITTER: i64 = 10_000; // 1 ms

    pub(crate) fn new(fps: u32) -> Self {
        let interval = HNS_PER_SECOND / fps.max(1) as i64;
        Self { interval, min_gap: interval * 3 / 4, due: None }
    }

    /// The nominal frame interval in 100 ns.
    pub(crate) fn interval(&self) -> i64 {
        self.interval
    }

    /// Returns true when the frame at `time` (100 ns, relative to the start)
    /// should be encoded, and advances the schedule if so.
    pub(crate) fn accept(&mut self, time: i64) -> bool {
        match self.due {
            Some(due) if time < due - Self::JITTER => false,
            Some(due) => {
                self.due = Some((due + self.interval).max(time + self.min_gap));
                true
            }
            None => {
                self.due = Some(time + self.interval);
                true
            }
        }
    }
}

/// How to end the video stream at stop time.
///
/// Capture only delivers frames when pixels change, so the last real frame may
/// be seconds old when the user stops. It is written with a duration that
/// reaches the stop time and, when it is at least one interval old, repeated at
/// the stop time so the video lasts as long as the recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tail {
    /// Duration to give the last real frame.
    pub last_duration: i64,
    /// Timestamp of the repeated frame, if one is needed (its duration is one interval).
    pub repeat_at: Option<i64>,
}

pub(crate) fn tail(last_time: i64, stop_time: i64, interval: i64) -> Tail {
    let elapsed = stop_time - last_time;
    if elapsed >= interval {
        Tail { last_duration: elapsed, repeat_at: Some(stop_time) }
    } else {
        Tail { last_duration: interval, repeat_at: None }
    }
}

/// What to do with one captured audio packet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Placement {
    /// Input frames to drop from the start of the packet (before the recording
    /// started, or because the written audio ran ahead of the clock).
    pub skip_input_frames: usize,
    /// Output frames of silence to write before the packet (a gap in capture).
    pub silence_frames: u64,
}

/// Places captured audio packets on the recording timeline.
///
/// Output is written back to back at `rate` (48 kHz), so the AAC stream has no
/// holes. Each packet's QPC timestamp is compared with where the written audio
/// has reached: a gap larger than `tolerance` (a capture glitch, a device
/// switch) is filled with silence, and audio running ahead of the clock by more
/// than `tolerance` is trimmed, so audio and video stay in sync even over long
/// recordings. Smaller differences are ignored to avoid clicks.
#[derive(Clone, Debug)]
pub(crate) struct AudioTimeline {
    rate: u32,
    tolerance_frames: i64,
    next: Option<i64>,
}

impl AudioTimeline {
    pub(crate) fn new(rate: u32, tolerance_hns: i64) -> Self {
        Self { rate, tolerance_frames: hns_to_frames(tolerance_hns, rate), next: None }
    }

    /// Decides how to place a packet of `input_frames` frames at `input_rate`
    /// captured at `time` (100 ns relative to the recording start; `None` when
    /// the device flagged its timestamp as unreliable). Returns `None` when the
    /// whole packet must be dropped. After writing, call [`advance`](Self::advance)
    /// with the number of output frames actually written (silence included).
    pub(crate) fn place(&mut self, time: Option<i64>, input_frames: usize, input_rate: u32) -> Option<Placement> {
        if input_frames == 0 {
            return None;
        }
        let Some(next) = self.next else {
            let start = time.unwrap_or(0);
            if start < 0 {
                // Captured before the recording started: keep only the part after zero.
                let skip = ceil_div((-start) as i128 * input_rate as i128, HNS_PER_SECOND as i128) as usize;
                if skip >= input_frames {
                    return None;
                }
                self.next = Some(0);
                return Some(Placement { skip_input_frames: skip, silence_frames: 0 });
            }
            self.next = Some(hns_to_frames(start, self.rate));
            return Some(Placement::default());
        };
        let Some(time) = time else {
            return Some(Placement::default());
        };
        let drift = hns_to_frames(time, self.rate) - next;
        if drift > self.tolerance_frames {
            return Some(Placement { skip_input_frames: 0, silence_frames: drift as u64 });
        }
        if drift < -self.tolerance_frames {
            let skip = ((-drift) as i128 * input_rate as i128 / self.rate.max(1) as i128) as usize;
            if skip >= input_frames {
                return None;
            }
            return Some(Placement { skip_input_frames: skip, silence_frames: 0 });
        }
        Some(Placement::default())
    }

    /// The next output frame to write (frames since the recording start).
    pub(crate) fn position(&self) -> i64 {
        self.next.unwrap_or(0)
    }

    /// Moves the write position after `frames` output frames were written.
    pub(crate) fn advance(&mut self, frames: u64) {
        self.next = Some(self.position() + frames as i64);
    }
}

fn ceil_div(numerator: i128, denominator: i128) -> i128 {
    (numerator + denominator - 1).div_euclid(denominator)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 10_000;

    #[test]
    fn qpc_conversion_is_exact_and_does_not_overflow() {
        assert_eq!(qpc_to_hns(10_000_000, 10_000_000), 10_000_000);
        assert_eq!(qpc_to_hns(3_000_000, 3_000_000), HNS_PER_SECOND);
        assert_eq!(qpc_to_hns(1_500_000, 3_000_000), HNS_PER_SECOND / 2);
        // 300 days of uptime with a 10 MHz counter.
        let counter = 300 * 86_400 * 10_000_000i64;
        assert_eq!(qpc_to_hns(counter, 10_000_000), counter);
        assert_eq!(qpc_to_hns(5, 0), 0);
    }

    #[test]
    fn frame_conversions_round_down_and_spans_do_not_drift() {
        assert_eq!(hns_to_frames(HNS_PER_SECOND, 48_000), 48_000);
        assert_eq!(hns_to_frames(-1, 48_000), -1);
        assert_eq!(hns_to_frames(208, 48_000), 0);
        assert_eq!(hns_to_frames(209, 48_000), 1);
        assert_eq!(frames_to_hns(48_000, 48_000), HNS_PER_SECOND);
        let mut total = 0;
        let mut start = 0;
        for _ in 0..1000 {
            let (time, duration) = frame_span(start, 441, 48_000);
            assert_eq!(time, total);
            total += duration;
            start += 441;
        }
        assert_eq!(total, frames_to_hns(441_000, 48_000));
    }

    fn kept(throttle: &mut FrameThrottle, refresh_hz: f64, seconds: f64) -> usize {
        let period = HNS_PER_SECOND as f64 / refresh_hz;
        let count = (seconds * refresh_hz) as usize;
        (0..count).filter(|&k| throttle.accept((k as f64 * period).round() as i64)).count()
    }

    #[test]
    fn throttle_keeps_every_other_frame_at_60_hz() {
        let mut throttle = FrameThrottle::new(30);
        assert_eq!(kept(&mut throttle, 60.0, 10.0), 300);
    }

    #[test]
    fn throttle_tolerates_displays_slightly_faster_than_60_hz() {
        for hz in [60.001, 59.94, 60.05] {
            let mut throttle = FrameThrottle::new(30);
            let n = kept(&mut throttle, hz, 10.0);
            assert!((295..=301).contains(&n), "{hz} Hz kept {n}");
        }
    }

    #[test]
    fn throttle_stays_near_30_fps_on_high_refresh_displays() {
        for hz in [75.0, 90.0, 120.0, 144.0, 165.0, 240.0] {
            let mut throttle = FrameThrottle::new(30);
            let n = kept(&mut throttle, hz, 10.0);
            assert!((270..=310).contains(&n), "{hz} Hz kept {n}");
        }
    }

    #[test]
    fn throttle_keeps_30_fps_content_even_with_jitter() {
        let mut throttle = FrameThrottle::new(30);
        let period = HNS_PER_SECOND / 30;
        let jitter = [0, -3_000, 2_000, -5_000, 4_000];
        let n = (0..300).filter(|&k| throttle.accept(k * period + jitter[k as usize % jitter.len()])).count();
        assert_eq!(n, 300);
    }

    #[test]
    fn throttle_never_keeps_frames_much_closer_than_three_quarters_of_an_interval() {
        let mut throttle = FrameThrottle::new(30);
        let mut last = None;
        for k in 0..2000 {
            let t = k * 7 * MS / 3; // 428 Hz
            if throttle.accept(t) {
                if let Some(previous) = last {
                    assert!(t - previous >= throttle.interval() * 3 / 4 - 10_000);
                }
                last = Some(t);
            }
        }
    }

    #[test]
    fn first_frame_is_always_kept_and_static_screens_keep_late_frames() {
        let mut throttle = FrameThrottle::new(30);
        assert!(throttle.accept(0));
        assert!(!throttle.accept(10 * MS));
        assert!(throttle.accept(5 * HNS_PER_SECOND));
    }

    #[test]
    fn tail_repeats_an_old_last_frame_at_stop_time() {
        let interval = HNS_PER_SECOND / 30;
        assert_eq!(
            tail(HNS_PER_SECOND, 4 * HNS_PER_SECOND, interval),
            Tail { last_duration: 3 * HNS_PER_SECOND, repeat_at: Some(4 * HNS_PER_SECOND) }
        );
        assert_eq!(tail(HNS_PER_SECOND, HNS_PER_SECOND + 10 * MS, interval), Tail { last_duration: interval, repeat_at: None });
        assert_eq!(tail(HNS_PER_SECOND, HNS_PER_SECOND - MS, interval), Tail { last_duration: interval, repeat_at: None });
    }

    #[test]
    fn first_audio_packet_sets_the_start_position() {
        let mut timeline = AudioTimeline::new(48_000, 40 * MS);
        assert_eq!(timeline.place(Some(20 * MS), 480, 48_000), Some(Placement::default()));
        assert_eq!(timeline.position(), 960);
    }

    #[test]
    fn audio_before_the_start_is_trimmed_or_dropped() {
        let mut timeline = AudioTimeline::new(48_000, 40 * MS);
        assert_eq!(timeline.place(Some(-20 * MS), 480, 48_000), None);
        assert_eq!(
            timeline.place(Some(-5 * MS), 480, 44_100),
            Some(Placement { skip_input_frames: 221, silence_frames: 0 })
        );
        assert_eq!(timeline.position(), 0);
    }

    #[test]
    fn continuous_audio_needs_no_correction_and_jitter_is_ignored() {
        let mut timeline = AudioTimeline::new(48_000, 40 * MS);
        timeline.place(Some(0), 480, 48_000);
        timeline.advance(480);
        for k in 1..100 {
            let jitter = if k % 2 == 0 { 3 * MS } else { -2 * MS };
            assert_eq!(timeline.place(Some(k * 10 * MS + jitter), 480, 48_000), Some(Placement::default()));
            timeline.advance(480);
        }
    }

    #[test]
    fn gaps_are_filled_with_silence() {
        let mut timeline = AudioTimeline::new(48_000, 40 * MS);
        timeline.place(Some(0), 480, 48_000);
        timeline.advance(480);
        // Next packet arrives 500 ms later instead of 10 ms.
        let placement = timeline.place(Some(510 * MS), 480, 48_000).unwrap();
        assert_eq!(placement, Placement { skip_input_frames: 0, silence_frames: 24_000 });
        timeline.advance(placement.silence_frames + 480);
        assert_eq!(timeline.position(), 24_960);
    }

    #[test]
    fn audio_running_ahead_of_the_clock_is_trimmed() {
        let mut timeline = AudioTimeline::new(48_000, 40 * MS);
        timeline.place(Some(0), 480, 48_000);
        timeline.advance(48_000); // one second written
        assert_eq!(
            timeline.place(Some(900 * MS), 9_600, 48_000),
            Some(Placement { skip_input_frames: 4_800, silence_frames: 0 })
        );
        assert_eq!(timeline.place(Some(900 * MS), 480, 48_000), None);
    }

    #[test]
    fn unreliable_timestamps_are_placed_back_to_back() {
        let mut timeline = AudioTimeline::new(48_000, 40 * MS);
        assert_eq!(timeline.place(None, 480, 48_000), Some(Placement::default()));
        assert_eq!(timeline.position(), 0);
        timeline.advance(480);
        assert_eq!(timeline.place(None, 480, 48_000), Some(Placement::default()));
        assert_eq!(timeline.place(Some(0), 0, 48_000), None);
    }
}
