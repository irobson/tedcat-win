//! Dragging a saved recording out of the card (OLE drag and drop).

use std::path::Path;

use windows::core::{BOOL, HRESULT, HSTRING};
use windows::Win32::Foundation::{DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, HWND, S_OK};
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::Ole::{IDropSource, IDropSource_Impl, DROPEFFECT, DROPEFFECT_COPY};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::{BHID_DataObject, IShellItem, SHCreateItemFromParsingName, SHDoDragDrop};
use windows_core::implement;

/// The drag source: drop when the left button is released, cancel on Escape.
#[implement(IDropSource)]
struct DropSource;

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, escape_pressed: BOOL, key_state: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape_pressed.as_bool() {
            DRAGDROP_S_CANCEL
        } else if (key_state.0 & MK_LBUTTON.0) == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Drags the file at `path` (as a copy) from window `source` until the user
/// drops or cancels. Blocking: runs a modal loop, like every OLE drag.
///
/// The data object comes from the shell, so Explorer, browsers, mail and chat
/// apps all accept it (it carries `CF_HDROP` and the shell ID list), and the
/// shell supplies the drag image. Only copying is offered, so dropping on a
/// folder never moves the recording away.
pub(crate) fn drag_file(source: HWND, path: &Path) -> windows::core::Result<DROPEFFECT> {
    // SAFETY: standard shell drag and drop on the STA UI thread (OLE is initialized in app::run).
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None)?;
        let data: IDataObject = item.BindToHandler(None, &BHID_DataObject)?;
        let drop_source: IDropSource = DropSource.into();
        SHDoDragDrop(Some(source), &data, &drop_source, DROPEFFECT_COPY)
    }
}
