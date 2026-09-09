//! Atomic file writes. Every managed file the harness owns is written via
//! tempfile-in-same-dir + `rename`, so a reader (or a desktop app polling the
//! file) never observes a half-written config.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::GhError;

/// Write `contents` to `path` atomically, creating parent dirs as needed.
///
/// The temp file is created in the *same directory* as the target so the final
/// `rename` stays on one filesystem (rename across mounts is not atomic).
pub fn write_atomic(path: &Path, contents: impl AsRef<[u8]>) -> Result<(), GhError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| GhError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    // Deterministic-but-unique temp name (pid keeps concurrent writers apart).
    let pid = std::process::id();
    let file_name = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tmp".to_string());
    let tmp = path.with_file_name(format!(".{file_name}.{pid}.tmp"));

    {
        let mut f = fs::File::create(&tmp).map_err(|e| GhError::Io {
            path: tmp.clone(),
            source: e,
        })?;
        f.write_all(contents.as_ref()).map_err(|e| GhError::Io {
            path: tmp.clone(),
            source: e,
        })?;
        f.flush().map_err(|e| GhError::Io {
            path: tmp.clone(),
            source: e,
        })?;
    }

    fs::rename(&tmp, path).map_err(|e| {
        // Best-effort cleanup; ignore secondary errors.
        let _ = fs::remove_file(&tmp);
        GhError::Io {
            path: path.to_path_buf(),
            source: e,
        }
    })?;

    // Managed files may carry secrets (pseudotokens); keep them owner-only.
    restrict_permissions(path);
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

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

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
}
