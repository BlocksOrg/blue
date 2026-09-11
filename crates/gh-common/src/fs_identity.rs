//! Filesystem identity for a path — the pair that answers "is this still the
//! same object?" independently of its name.
//!
//! Unix reads `(st_dev, st_ino)` straight off `MetadataExt`. Windows exposes
//! the same pair (volume serial number and file index), but `std`'s accessors
//! for it sit behind the permanently unstable `windows_by_handle` feature, so
//! we query `GetFileInformationByHandle` ourselves and stay on stable.

use std::path::Path;

/// `(volume serial number, file index)` for `path`, or `None` when the path
/// cannot be opened for a metadata query.
///
/// Opens the link itself rather than following it, matching the
/// `symlink_metadata` semantics callers use for the rest of a fingerprint.
pub fn file_identity(path: &Path) -> Option<(u64, u64)> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide = crate::paths::wide_path(path).ok()?;
    // SAFETY: `wide` is NUL-terminated and outlives the call. Zero desired
    // access asks for metadata only; BACKUP_SEMANTICS lets directories open and
    // OPEN_REPARSE_POINT keeps us on the link instead of its target.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return None;
    }
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `handle` is open and `info` is a live buffer of the right type.
    let queried = unsafe { GetFileInformationByHandle(handle, &mut info) };
    // SAFETY: `handle` came from CreateFileW above and is not used again.
    unsafe { CloseHandle(handle) };
    if queried == 0 {
        return None;
    }
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Some((u64::from(info.dwVolumeSerialNumber), index))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_object_reached_two_ways_has_one_identity() {
        let dir = std::env::temp_dir().join(format!("blue-identity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("managed.txt");
        std::fs::write(&file, "body").unwrap();

        let direct = file_identity(&file).expect("file identity");
        let indirect = file_identity(&dir.join(".").join("managed.txt")).expect("file identity");
        assert_eq!(direct, indirect);
        assert_ne!(file_identity(&dir).expect("directory identity"), direct);
        assert!(file_identity(&dir.join("absent.txt")).is_none());

        let _ = std::fs::remove_dir_all(dir);
    }
}
