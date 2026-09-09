//! Limits, validation, and secure streaming extraction for managed packages.

use std::collections::BTreeSet;
use std::fs::File;
use std::path::Path;

use gh_common::GhError;

pub(crate) const MAX_EXPANDED_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;
pub(crate) const MAX_ARCHIVE_ENTRIES: usize = 10_000;
const MAX_PATH_COMPONENTS: usize = 32;
const MAX_PATH_BYTES: usize = 4_096;
pub(crate) const MAX_METADATA_ENTRY_BYTES: u64 = 64 * 1024;
const MAX_METADATA_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(crate) struct ExtractionLimits {
    pub(crate) expanded_bytes: u64,
    pub(crate) file_bytes: u64,
    pub(crate) entries: usize,
    pub(crate) path_components: usize,
    pub(crate) path_bytes: usize,
}

pub(crate) const EXTRACTION_LIMITS: ExtractionLimits = ExtractionLimits {
    expanded_bytes: MAX_EXPANDED_BYTES,
    file_bytes: MAX_FILE_BYTES,
    entries: MAX_ARCHIVE_ENTRIES,
    path_components: MAX_PATH_COMPONENTS,
    path_bytes: MAX_PATH_BYTES,
};

pub(crate) fn entry_count_within_budget(count: usize, limits: ExtractionLimits) -> bool {
    count <= limits.entries
}

pub(crate) fn expanded_size_within_budget(
    expanded: u64,
    declared: u64,
    limits: ExtractionLimits,
) -> bool {
    declared <= limits.file_bytes && expanded.saturating_add(declared) <= limits.expanded_bytes
}

pub(crate) fn extract_safe(bytes: &[u8], dest: &Path, id: &str) -> Result<(), GhError> {
    extract_safe_with_limits(bytes, dest, id, EXTRACTION_LIMITS, available_space)
}

pub(crate) fn extract_safe_with_limits(
    bytes: &[u8],
    dest: &Path,
    id: &str,
    limits: ExtractionLimits,
    space_probe: fn(&Path) -> Result<Option<u64>, GhError>,
) -> Result<(), GhError> {
    secure_create_dir_all(dest)?;
    // This is advisory only. Limits below are authoritative even when the
    // filesystem cannot report available capacity.
    let mut available_budget = match space_probe(dest) {
        Ok(available) => available,
        Err(error) => {
            tracing::debug!(%error, path = %dest.display(), "package free-space probe failed");
            None
        }
    };
    validate_raw_archive(bytes, id, limits)?;
    let decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    let mut entries_seen = 0usize;
    let mut expanded = 0u64;
    let mut paths_seen = BTreeSet::new();
    for entry in archive
        .entries()
        .map_err(|error| GhError::other(format!("reading package `{id}`: {error}")))?
    {
        let mut entry = entry
            .map_err(|error| GhError::other(format!("reading package `{id}` entry: {error}")))?;
        entries_seen += 1;
        if !entry_count_within_budget(entries_seen, limits) {
            return Err(GhError::config(format!(
                "package `{id}` exceeds the {} entry limit",
                limits.entries
            )));
        }
        let kind = entry.header().entry_type();
        let path_bytes = entry.path_bytes();
        if path_bytes.len() > limits.path_bytes {
            return Err(GhError::config(format!(
                "package `{id}` contains a path longer than {} bytes",
                limits.path_bytes
            )));
        }
        let mut relative_text = std::str::from_utf8(&path_bytes)
            .map_err(|_| GhError::config(format!("package `{id}` contains a non-UTF-8 path")))?
            .to_owned();
        if kind.is_dir() {
            relative_text.truncate(relative_text.trim_end_matches('/').len());
        }
        let components = relative_text.split('/').collect::<Vec<_>>();
        if components.len() > limits.path_components
            || components.iter().any(|part| {
                part.is_empty()
                    || matches!(*part, "." | "..")
                    || part.contains(['\\', '\0'])
                    || part.contains(':')
            })
        {
            return Err(GhError::config(format!(
                "package `{id}` contains unsafe path `{relative_text}`"
            )));
        }
        if !paths_seen.insert(relative_text.clone()) {
            return Err(GhError::config(format!(
                "package `{id}` contains duplicate path `{relative_text}`"
            )));
        }
        // GitHub codeload archives may include global metadata with no payload.
        if kind.is_pax_global_extensions() {
            continue;
        }
        if kind.is_symlink() || kind.is_hard_link() || !(kind.is_file() || kind.is_dir()) {
            return Err(GhError::config(format!(
                "package `{id}` contains unsupported link or special entry `{relative_text}`"
            )));
        }
        let target = dest.join(&relative_text);
        if kind.is_dir() {
            secure_create_dir_all(&target)?;
            continue;
        }

        let declared = entry.size();
        if !expanded_size_within_budget(expanded, declared, limits) {
            return Err(GhError::config(format!(
                "package `{id}` exceeds an expanded archive limit at `{relative_text}`"
            )));
        }
        if available_budget.is_some_and(|available| available < declared) {
            return Err(GhError::config(format!(
                "package `{id}` does not have enough free space for `{relative_text}`"
            )));
        }
        if let Some(parent) = target.parent() {
            secure_create_dir_all(parent)?;
        }
        let mut file = create_package_file(&target)?;
        let remaining_total = limits.expanded_bytes - expanded;
        let allowed = limits.file_bytes.min(remaining_total);
        let copied = std::io::copy(&mut std::io::Read::take(&mut entry, allowed + 1), &mut file)
            .map_err(|source| GhError::Io {
                path: target.clone(),
                source,
            })?;
        if copied > allowed || copied != declared {
            return Err(GhError::config(format!(
                "package `{id}` entry `{relative_text}` exceeded or disagreed with its declared size"
            )));
        }
        expanded += copied;
        if let Some(available) = &mut available_budget {
            *available = available.saturating_sub(copied);
        }
        file.sync_all().map_err(|source| GhError::Io {
            path: target,
            source,
        })?;
    }
    sync_extracted_directories(dest)?;
    Ok(())
}

/// Validate raw TAR members before `tar` processes extended path metadata.
/// Normal iteration eagerly buffers GNU long-name and local PAX records, so
/// their declared sizes must be bounded first to keep compressed archives from
/// causing unbounded allocations before the regular extraction checks run.
fn validate_raw_archive(bytes: &[u8], id: &str, limits: ExtractionLimits) -> Result<(), GhError> {
    let decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| GhError::other(format!("reading package `{id}`: {error}")))?
        .raw(true);
    let mut raw_entries = 0usize;
    let mut expanded = 0u64;
    let mut metadata = 0u64;
    for entry in entries {
        let entry = entry
            .map_err(|error| GhError::other(format!("reading package `{id}` entry: {error}")))?;
        raw_entries += 1;
        if raw_entries > limits.entries.saturating_mul(2).saturating_add(16) {
            return Err(GhError::config(format!(
                "package `{id}` exceeds the raw archive entry limit"
            )));
        }
        let kind = entry.header().entry_type();
        let declared = entry.size();
        if kind.is_gnu_longname()
            || kind.is_pax_local_extensions()
            || kind.is_pax_global_extensions()
        {
            if declared > MAX_METADATA_ENTRY_BYTES
                || metadata.saturating_add(declared) > MAX_METADATA_BYTES
            {
                return Err(GhError::config(format!(
                    "package `{id}` exceeds an archive metadata limit"
                )));
            }
            metadata += declared;
        } else if kind.is_file() {
            if !expanded_size_within_budget(expanded, declared, limits) {
                return Err(GhError::config(format!(
                    "package `{id}` exceeds an expanded archive limit"
                )));
            }
            expanded += declared;
        } else if kind.is_dir() {
            if declared != 0 {
                return Err(GhError::config(format!(
                    "package `{id}` contains a directory entry with a payload"
                )));
            }
        } else {
            return Err(GhError::config(format!(
                "package `{id}` contains an unsupported link or special entry"
            )));
        }
    }
    Ok(())
}

fn sync_extracted_directories(path: &Path) -> Result<(), GhError> {
    for entry in std::fs::read_dir(path).map_err(|source| GhError::Io {
        path: path.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| GhError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let metadata = entry.metadata().map_err(|source| GhError::Io {
            path: entry.path(),
            source,
        })?;
        if metadata.is_dir() {
            sync_extracted_directories(&entry.path())?;
        }
    }
    sync_directory(path)
}

#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> Result<(), GhError> {
    match std::fs::File::open(path).and_then(|directory| directory.sync_all()) {
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
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(not(unix))]
pub(crate) fn sync_directory(_path: &Path) -> Result<(), GhError> {
    Ok(())
}

pub(crate) fn secure_create_dir_all(path: &Path) -> Result<(), GhError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(GhError::config(format!(
                    "managed package path is not a regular directory: {}",
                    path.display()
                )));
            }
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(GhError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    }
    if let Some(parent) = path.parent() {
        secure_create_dir_all(parent)?;
    }
    match gh_common::create_owner_only_dir(path) {
        Ok(()) => Ok(()),
        Err(GhError::Io { source, .. }) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            secure_create_dir_all(path)
        }
        Err(error) => Err(error),
    }
}

fn create_package_file(path: &Path) -> Result<File, GhError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|source| GhError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(unix)]
fn available_space(path: &Path) -> Result<Option<u64>, GhError> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| GhError::config("package staging path contains NUL"))?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(c_path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(GhError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::last_os_error(),
        });
    }
    let stats = unsafe { stats.assume_init() };
    Ok(Some(
        u64::from(stats.f_bavail).saturating_mul(stats.f_frsize),
    ))
}

#[cfg(windows)]
fn available_space(path: &Path) -> Result<Option<u64>, GhError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0_u64;
    if unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(GhError::Io {
            path: std::path::PathBuf::from(String::from_utf16_lossy(&wide[..wide.len() - 1])),
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(Some(available))
}

#[cfg(not(any(unix, windows)))]
fn available_space(_path: &Path) -> Result<Option<u64>, GhError> {
    Ok(None)
}
