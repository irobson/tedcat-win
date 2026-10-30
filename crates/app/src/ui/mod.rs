//! Tray icon and menu, recording pill, region outline and saved card.
//!
//! CONTRACT (owned by the UI agent): everything here runs on the UI thread.
//! Window class names start with `util::win::CLASS_PREFIX`; every visible
//! window calls `util::win::exclude_from_capture`; none of them takes focus
//! except dialogs. Submodules are free to change.
