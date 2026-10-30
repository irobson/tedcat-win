//! Shows a [`MenuEntry`] list as a Win32 popup menu at the tray icon.

use windows::core::PWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, InsertMenuItemW, PostMessageW, SetForegroundWindow, TrackPopupMenuEx, HMENU,
    MENUITEMINFOW, MENU_ITEM_STATE, MENU_ITEM_TYPE, MFS_CHECKED, MFS_DISABLED, MFS_ENABLED, MFT_RADIOCHECK,
    MFT_SEPARATOR, MFT_STRING, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING, MIIM_SUBMENU, TPM_BOTTOMALIGN,
    TPM_LEFTALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTALIGN, TPM_RIGHTBUTTON, TPM_TOPALIGN, WM_NULL,
};

use crate::geom::{MonitorInfo, PointPx};
use crate::ui::layout::menu_alignment;
use crate::ui::menu_model::{Check, MenuEntry};

/// Owns an `HMENU` (and, through it, its submenus) until drop.
struct Menu(HMENU);

impl Drop for Menu {
    fn drop(&mut self) {
        // SAFETY: we created the menu; DestroyMenu also destroys attached submenus.
        unsafe {
            let _ = DestroyMenu(self.0);
        }
    }
}

/// Menu text with `&` escaped (a single `&` would underline the next letter).
fn menu_text(text: &str) -> Vec<u16> {
    text.replace('&', "&&").encode_utf16().chain(std::iter::once(0)).collect()
}

fn build(entries: &[MenuEntry]) -> windows::core::Result<Menu> {
    // SAFETY: plain menu creation; the menu is owned by the returned guard.
    let menu = Menu(unsafe { CreatePopupMenu() }?);
    for (position, entry) in entries.iter().enumerate() {
        let mut info = MENUITEMINFOW { cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32, ..Default::default() };
        let mut text: Vec<u16> = Vec::new();
        let mut submenu: Option<Menu> = None;
        match entry {
            MenuEntry::Separator => {
                info.fMask = MIIM_FTYPE;
                info.fType = MFT_SEPARATOR;
            }
            MenuEntry::Item(item) => {
                text = menu_text(&item.text);
                info.fMask = MIIM_ID | MIIM_STRING | MIIM_FTYPE | MIIM_STATE;
                info.wID = item.id;
                info.dwTypeData = PWSTR(text.as_mut_ptr());
                let (kind, checked) = match item.check {
                    Check::None => (MFT_STRING, false),
                    Check::Toggle(on) => (MFT_STRING, on),
                    Check::Radio(on) => (MENU_ITEM_TYPE(MFT_STRING.0 | MFT_RADIOCHECK.0), on),
                };
                info.fType = kind;
                let enabled = if item.enabled { MFS_ENABLED } else { MFS_DISABLED };
                info.fState = if checked { MENU_ITEM_STATE(enabled.0 | MFS_CHECKED.0) } else { enabled };
            }
            MenuEntry::Submenu { text: label, entries } => {
                let child = build(entries)?;
                text = menu_text(label);
                info.fMask = MIIM_STRING | MIIM_FTYPE | MIIM_SUBMENU;
                info.fType = MFT_STRING;
                info.dwTypeData = PWSTR(text.as_mut_ptr());
                info.hSubMenu = child.0;
                submenu = Some(child);
            }
        }
        // SAFETY: `info` and the `text` it points to outlive the call, which copies the text.
        unsafe { InsertMenuItemW(menu.0, position as u32, true, &info) }?;
        // On success the parent owns the submenu and destroys it with itself.
        if let Some(child) = submenu {
            std::mem::forget(child);
        }
        drop(text);
    }
    Ok(menu)
}

/// Shows the menu at `at` (screen pixels) for the tray icon of `owner` and
/// returns the chosen item id, if any. Follows the documented tray-menu
/// pattern: the owner becomes the foreground window first so the menu closes
/// when the user clicks elsewhere, and a `WM_NULL` is posted afterwards.
pub(crate) fn show_popup(owner: HWND, entries: &[MenuEntry], at: PointPx) -> Option<u32> {
    let menu = match build(entries) {
        Ok(menu) => menu,
        Err(error) => {
            crate::log_error!("could not build the tray menu: {error}");
            return None;
        }
    };
    let monitor = MonitorInfo::from_point(at);
    let (right, bottom) = menu_alignment(at, monitor.rect);
    let horizontal = if right { TPM_RIGHTALIGN } else { TPM_LEFTALIGN };
    let vertical = if bottom { TPM_BOTTOMALIGN } else { TPM_TOPALIGN };
    let flags = TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | horizontal | vertical;
    // SAFETY: documented TrackPopupMenu usage on the UI thread with a menu we own.
    let chosen = unsafe {
        let _ = SetForegroundWindow(owner);
        let chosen = TrackPopupMenuEx(menu.0, flags.0, at.x, at.y, owner, None);
        let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
        chosen
    };
    drop(menu);
    u32::try_from(chosen.0).ok().filter(|&id| id != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ampersands_are_escaped_and_terminated() {
        assert_eq!(menu_text("A&B"), "A&&B\0".encode_utf16().collect::<Vec<u16>>());
    }
}
