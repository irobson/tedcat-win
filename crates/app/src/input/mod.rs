//! Mouse and keyboard interception, and the on-screen selection.

pub mod hook;
pub mod overlay;

pub use hook::{GestureSettings, InputHook};
pub use overlay::SelectionOverlay;
