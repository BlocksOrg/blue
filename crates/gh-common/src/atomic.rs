//! Atomic file writes. Every managed file the harness owns is written via
//! tempfile-in-same-dir + `rename`, so a reader (or a desktop app polling the
//! file) never observes a half-written config.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rand::RngCore;

use crate::error::GhError;

const TEMP_NAME_ATTEMPTS: usize = 16;

/// Create a directory whose contents are accessible only to the owning user
/// and required operating-system administrators.
pub fn create_owner_only_dir(path: &Path) -> Result<(), GhError> {
    create_owner_only_dir_impl(path).map_err(|source| GhError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Exclusively create and durably write an owner-only file. This is for
/// immutable identities where replacing an existing destination would be a
/// correctness bug rather than an update.
pub fn write_owner_only_new(path: &Path, contents: impl AsRef<[u8]>) -> Result<(), GhError> {
    if let Some(parent) = path.parent() {
        create_owner_only_dir_all(parent)?;
    }
    let mut file = create_owner_only(path).map_err(|source| GhError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if let Err(source) = file
        .write_all(contents.as_ref())
        .and_then(|_| file.sync_all())
    {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(GhError::Io {
            path: path.to_path_buf(),
            source,
        });
    }
    sync_parent(path)
}

pub fn create_owner_only_dir_all(path: &Path) -> Result<(), GhError> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && !metadata_is_reparse(&metadata) =>
        {
            return Ok(())
        }
        Ok(_) => {
            return Err(GhError::config(format!(
                "owner-only directory path is not a directory: {}",
                path.display()
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(GhError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty() && *parent != path)
    {
        create_owner_only_dir_all(parent)?;
    }
    match create_owner_only_dir(path) {
        Ok(()) => Ok(()),
        Err(GhError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse(_: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn create_owner_only_dir_impl(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

#[cfg(windows)]
fn create_owner_only_dir_impl(path: &Path) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

    let sddl: Vec<u16> = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)\0"
        .encode_utf16()
        .collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: Windows allocates the descriptor and it is released below.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let wide = crate::paths::wide_path(path)?;
    // SAFETY: the path and security attributes remain valid for the call.
    let created = unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) };
    unsafe { LocalFree(descriptor.cast()) };
    if created == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn create_owner_only_dir_impl(path: &Path) -> std::io::Result<()> {
    fs::create_dir(path)
}

/// A fully written and synced same-directory temporary file awaiting publish.
pub struct PreparedAtomicWrite {
    destination: PathBuf,
    temporary: PathBuf,
    owns_temporary: bool,
}

impl PreparedAtomicWrite {
    /// The exact temporary path owned by this write. Transaction journals use
    /// this only for crash cleanup; callers must not modify it.
    pub fn temporary_path(&self) -> &Path {
        &self.temporary
    }

    /// Atomically publish the prepared bytes and sync the containing directory.
    pub fn commit(mut self) -> Result<(), GhError> {
        if let Some(parent) = self.destination.parent() {
            fs::create_dir_all(parent).map_err(|source| GhError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        replace_file(&self.temporary, &self.destination).map_err(|source| GhError::Io {
            path: self.destination.clone(),
            source,
        })?;
        self.owns_temporary = false;
        sync_parent(&self.destination)
    }
}

impl Drop for PreparedAtomicWrite {
    fn drop(&mut self) {
        if self.owns_temporary {
            let _ = fs::remove_file(&self.temporary);
        }
    }
}

/// Prepare `contents` in an owner-only, exclusively created same-directory
/// temporary file without changing the destination.
pub fn prepare_atomic(
    path: &Path,
    contents: impl AsRef<[u8]>,
) -> Result<PreparedAtomicWrite, GhError> {
    let temporary_directory = path.parent().unwrap_or_else(|| Path::new("."));
    prepare_atomic_in(path, temporary_directory, contents)
}

/// Prepare an atomic replacement in a specified directory on the
/// destination's filesystem. This supports transactions that will replace a
/// destination parent directory before publishing the file.
pub fn prepare_atomic_in(
    path: &Path,
    temporary_directory: &Path,
    contents: impl AsRef<[u8]>,
) -> Result<PreparedAtomicWrite, GhError> {
    {
        let parent = temporary_directory;
        fs::create_dir_all(parent).map_err(|e| GhError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    let file_name = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tmp".to_string());
    let mut owned_tmp = None;
    let mut collision = None;
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let mut nonce = [0_u8; 16];
        rand::rngs::OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|error| GhError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::other(format!("generating temporary name: {error}")),
            })?;
        let tmp = temporary_directory.join(format!(".{file_name}.{}.tmp", hex_nonce(&nonce)));
        match create_owner_only(&tmp) {
            Ok(file) => {
                owned_tmp = Some((tmp, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                collision = Some(error);
            }
            Err(source) => return Err(GhError::Io { path: tmp, source }),
        }
    }
    let (tmp, mut file) = owned_tmp.ok_or_else(|| GhError::Io {
        path: path.to_path_buf(),
        source: collision
            .unwrap_or_else(|| std::io::Error::other("temporary name attempts exhausted")),
    })?;
    let result = (|| {
        file.write_all(contents.as_ref())
            .map_err(|source| GhError::Io {
                path: tmp.clone(),
                source,
            })?;
        file.sync_all().map_err(|source| GhError::Io {
            path: tmp.clone(),
            source,
        })?;
        drop(file);
        Ok(PreparedAtomicWrite {
            destination: path.to_path_buf(),
            temporary: tmp.clone(),
            owns_temporary: true,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Write `contents` to `path` atomically, creating parent dirs as needed.
///
/// The temp file is created in the *same directory* as the target so the final
/// `rename` stays on one filesystem (rename across mounts is not atomic).
pub fn write_atomic(path: &Path, contents: impl AsRef<[u8]>) -> Result<(), GhError> {
    prepare_atomic(path, contents)?.commit()
}

fn hex_nonce(bytes: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(unix)]
fn create_owner_only(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(windows)]
fn create_owner_only(path: &Path) -> std::io::Result<fs::File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{LocalFree, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE,
    };

    let sddl: Vec<u16> = "D:P(A;;FA;;;OW)(A;;FA;;;SY)(A;;FA;;;BA)\0"
        .encode_utf16()
        .collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: Windows allocates the descriptor and it is released with LocalFree below.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let wide = crate::paths::wide_path(path)?;
    // SAFETY: all pointers remain valid for the duration of CreateFileW.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_GENERIC_WRITE,
            0,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: descriptor was allocated by the conversion call.
    unsafe { LocalFree(descriptor.cast()) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned an owned handle.
    Ok(unsafe { fs::File::from_raw_handle(handle) })
}

#[cfg(not(any(unix, windows)))]
fn create_owner_only(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

/// Bounded retries for a contended replace; roughly 1.3s of total backoff.
#[cfg(windows)]
const REPLACE_ATTEMPTS: u32 = 12;

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION};
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source = crate::paths::wide_path(source)?;
    let destination = crate::paths::wide_path(destination)?;
    // Windows refuses the replace while anything else holds the destination
    // open: a concurrent publisher, a running harness reading its own config,
    // the search indexer, or antivirus. The window is short and the caller
    // cannot act on the failure, so retry briefly before giving up.
    let mut backoff = std::time::Duration::from_millis(1);
    for attempt in 0..REPLACE_ATTEMPTS {
        // SAFETY: both paths are valid, nul-terminated UTF-16 buffers.
        if unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } != 0
        {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        let contended = matches!(
            error.raw_os_error(),
            Some(code)
                if code == ERROR_ACCESS_DENIED as i32 || code == ERROR_SHARING_VIOLATION as i32
        );
        if !contended || attempt + 1 == REPLACE_ATTEMPTS {
            return Err(error);
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(std::time::Duration::from_millis(250));
    }
    Err(std::io::Error::from(std::io::ErrorKind::TimedOut))
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<(), GhError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    match fs::File::open(parent).and_then(|directory| directory.sync_all()) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
            ) =>
        {
            Ok(())
        }
        Err(source) => Err(GhError::Io {
            path: parent.to_path_buf(),
            source,
        }),
    }
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<(), GhError> {
    Ok(())
}

/// Atomically write a user-owned configuration file, preserving the previous
/// bytes in an owner-only timestamped backup first. Identical content is left
/// untouched, so periodic reconciliation does not create backup churn.
pub fn write_config_atomic(path: &Path, contents: impl AsRef<[u8]>) -> Result<(), GhError> {
    let contents = contents.as_ref();
    match fs::read(path) {
        Ok(previous) if previous == contents => return Ok(()),
        Ok(previous) => {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let pid = std::process::id();
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "config".to_string());
            let backup = path.with_file_name(format!("{name}.bak.harness.{timestamp}.{pid}"));
            write_atomic(&backup, previous)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(GhError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    }
    write_atomic(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[cfg(windows)]
    fn protected_acl_sids(path: &Path) -> (bool, Vec<String>) {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertSidToStringSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
        };
        use windows_sys::Win32::Security::{
            AclSizeInformation, GetAce, GetAclInformation, GetSecurityDescriptorControl,
            ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
        };
        let wide = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: all output pointers remain valid and the returned descriptor
        // is released with LocalFree below.
        let status = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(status, 0);
        assert!(!dacl.is_null());
        let mut control = 0_u16;
        let mut revision = 0_u32;
        assert_ne!(
            unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
            0
        );
        let mut info = unsafe { std::mem::zeroed::<ACL_SIZE_INFORMATION>() };
        assert_ne!(
            unsafe {
                GetAclInformation(
                    dacl,
                    (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                    std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                )
            },
            0
        );
        let mut sids = Vec::new();
        for index in 0..info.AceCount {
            let mut raw_ace = std::ptr::null_mut();
            assert_ne!(unsafe { GetAce(dacl, index, &mut raw_ace) }, 0);
            let ace = unsafe { &*(raw_ace.cast::<ACCESS_ALLOWED_ACE>()) };
            assert_eq!(ace.Header.AceType, 0);
            let sid = (&ace.SidStart as *const u32).cast_mut().cast();
            let mut string_sid = std::ptr::null_mut();
            assert_ne!(unsafe { ConvertSidToStringSidW(sid, &mut string_sid) }, 0);
            let mut length = 0;
            while unsafe { *string_sid.add(length) } != 0 {
                length += 1;
            }
            let value =
                String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(string_sid, length) });
            unsafe { LocalFree(string_sid.cast()) };
            sids.push(value);
        }
        unsafe { LocalFree(descriptor.cast()) };
        sids.sort();
        (control & SE_DACL_PROTECTED != 0, sids)
    }

    #[test]
    fn owner_only_writes_survive_past_the_legacy_windows_path_limit() {
        // Raw `W` calls get no `\\?\` promotion from `std`, which caps
        // `CreateDirectoryW` at 248 characters and `CreateFileW` at 260. A
        // managed marketplace plugin tree crosses both on an ordinary profile,
        // so exercise a directory, a file and a republish beyond the limit.
        let dir = std::env::temp_dir().join(format!("gh-atomic-long-{}", std::process::id()));
        let mut deep = dir.clone();
        while deep.as_os_str().len() < 280 {
            deep = deep.join("nested-directory-segment");
        }
        create_owner_only_dir_all(&deep).unwrap();
        assert!(deep.is_dir(), "{}", deep.display());
        let target = deep.join("config.json");
        write_atomic(&target, b"hello").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "hello");
        write_atomic(&target, b"world").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "world");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_and_creates_parents() {
        let dir = std::env::temp_dir().join(format!("gh-atomic-{}", std::process::id()));
        let target = dir.join("nested").join("file.txt");
        write_atomic(&target, b"hello").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "hello");
        write_atomic(&target, b"world").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "world");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepared_write_is_private_invisible_and_owned_until_commit() {
        let dir = std::env::temp_dir().join(format!("gh-atomic-prepared-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("config.json");
        fs::write(&target, b"old").unwrap();
        let prepared = prepare_atomic(&target, b"new").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert_eq!(fs::read(prepared.temporary_path()).unwrap(), b"new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(prepared.temporary_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let temporary = prepared.temporary_path().to_path_buf();
        drop(prepared);
        assert!(!temporary.exists());
        assert_eq!(fs::read(&target).unwrap(), b"old");
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(windows)]
    #[test]
    fn windows_files_and_directories_have_protected_owner_system_admin_acls() {
        let root = std::env::temp_dir().join(format!("gh-atomic-acl-{}", std::process::id()));
        create_owner_only_dir(&root).unwrap();
        let target = root.join("secret.json");
        write_atomic(&target, b"secret").unwrap();
        for path in [&root, &target] {
            let (protected, sids) = protected_acl_sids(path);
            assert!(protected, "{} DACL is inheritable", path.display());
            assert_eq!(
                sids,
                vec![
                    "S-1-3-4".to_owned(),
                    "S-1-5-18".to_owned(),
                    "S-1-5-32-544".to_owned(),
                ]
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn config_write_backs_up_changed_content_but_not_identical_content() {
        let dir = std::env::temp_dir().join(format!("gh-config-backup-{}", std::process::id()));
        let target = dir.join("config.toml");
        write_atomic(&target, b"old").unwrap();
        write_config_atomic(&target, b"new").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");

        let backups = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".bak.harness.")
            })
            .collect::<Vec<_>>();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read_to_string(backups[0].path()).unwrap(), "old");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(backups[0].path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        write_config_atomic(&target, b"new").unwrap();
        let backup_count = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".bak.harness.")
            })
            .count();
        assert_eq!(backup_count, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_writers_publish_only_complete_owner_only_files() {
        let dir = std::env::temp_dir().join(format!("gh-atomic-race-{}", std::process::id()));
        let target = dir.join("state.json");
        let writers = 32;
        let barrier = Arc::new(Barrier::new(writers));
        let handles = (0..writers)
            .map(|index| {
                let barrier = Arc::clone(&barrier);
                let target = target.clone();
                std::thread::spawn(move || {
                    let body = format!("writer-{index}:").repeat(8_192);
                    barrier.wait();
                    write_atomic(&target, body.as_bytes()).unwrap();
                    body
                })
            })
            .collect::<Vec<_>>();
        let bodies = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        let published = fs::read_to_string(&target).unwrap();
        assert!(bodies.contains(&published));
        assert_eq!(
            fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
                .count(),
            0
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn owner_only_creation_failure_is_fatal_and_leaves_no_temporary_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("gh-atomic-denied-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let target = dir.join("secret.json");
        let result = write_atomic(&target, b"secret");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        assert!(!target.exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cross_process_writer_helper() {
        let Some(target) = std::env::var_os("BLUE_ATOMIC_TEST_TARGET") else {
            return;
        };
        let body = std::env::var("BLUE_ATOMIC_TEST_BODY").unwrap();
        write_atomic(Path::new(&target), body).unwrap();
    }

    #[test]
    fn cross_process_writers_do_not_share_temporary_files() {
        let dir = std::env::temp_dir().join(format!("gh-atomic-process-{}", std::process::id()));
        let target = dir.join("state.json");
        let executable = std::env::current_exe().unwrap();
        let bodies = (0..8)
            .map(|index| format!("process-{index}:").repeat(1_024))
            .collect::<Vec<_>>();
        let children = bodies
            .iter()
            .map(|body| {
                std::process::Command::new(&executable)
                    .args(["--exact", "atomic::tests::cross_process_writer_helper"])
                    .env("BLUE_ATOMIC_TEST_TARGET", &target)
                    .env("BLUE_ATOMIC_TEST_BODY", body)
                    .spawn()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        assert!(bodies.contains(&fs::read_to_string(&target).unwrap()));
        assert_eq!(
            fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
                .count(),
            0
        );
        let _ = fs::remove_dir_all(dir);
    }
}
