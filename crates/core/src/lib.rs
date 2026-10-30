//! Platform-independent logic shared by every part of TedCat for Windows.
//!
//! Nothing in this crate touches Win32. It is a port of `TedCatCore` from the
//! macOS app (github.com/irobson/tedcat), with the same behaviour and the same
//! test cases, so both apps agree on what a gesture means.

pub mod geometry;
pub mod gesture;
pub mod naming;
pub mod recording;

pub use geometry::{Point, Rect, Size};
pub use gesture::{
    CaptureMode, Effect, GestureConfig, HoldGestureRecognizer, Modifiers, PointerInput, Response, State, Verdict,
};
pub use naming::{recording_file_name, Timestamp};
pub use recording::{Codec, RecordingTarget, VideoEncodingPlan};
