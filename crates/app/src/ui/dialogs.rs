//! System dialogs.

use std::path::{Path, PathBuf};

use windows::core::{w, HSTRING};
use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST,
    FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
};

/// Asks the user for a folder, starting in `initial`. Returns `None` when the
/// dialog was cancelled or failed (failures are logged).
///
/// `owner` may be `None`: the dialog then gets its own taskbar button, which
/// keeps it easy to find for an app that has no visible windows.
pub(crate) fn pick_folder(owner: Option<HWND>, initial: &Path, title: &str) -> Option<PathBuf> {
    match pick_folder_inner(owner, initial, title) {
        Ok(path) => path,
        Err(error) if error.code() == ERROR_CANCELLED.to_hresult() => None,
        Err(error) => {
            crate::log_error!("folder picker failed: {error}");
            None
        }
    }
}

fn pick_folder_inner(owner: Option<HWND>, initial: &Path, title: &str) -> windows::core::Result<Option<PathBuf>> {
    // SAFETY: standard IFileOpenDialog usage on an STA thread; the result string is
    // freed with CoTaskMemFree.
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
        let options = dialog.GetOptions()?;
        dialog.SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST)?;
        let _ = dialog.SetTitle(&HSTRING::from(title));
        let _ = dialog.SetOkButtonLabel(w!("Select Folder"));
        let start =
            if initial.is_dir() { Some(initial.to_path_buf()) } else { initial.parent().map(Path::to_path_buf) };
        if let Some(start) = start {
            if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(start.as_os_str()), None) {
                let _ = dialog.SetFolder(&item);
            }
        }
        dialog.Show(owner)?;
        let item = dialog.GetResult()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH)?;
        let path = name.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(name.0 as *const _));
        Ok(path)
    }
}
