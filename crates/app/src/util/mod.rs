//! Small Win32 helpers shared by every module.

pub mod log;
pub mod paths;
pub mod wide;
pub mod win;

pub use wide::{from_wide, wide};
