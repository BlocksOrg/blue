//! `PATH` resolution for the CLIs Blue launches, and the recogniser that keeps
//! a Blue shim from being mistaken for the agent it fronts.
//!
//! This lives in `gh-common` because two layers resolve the same binaries:
//! `gh-harness` detects the agent for the inventory, and `gh-config` resolves
//! it again when it has to pick a compatibility implementation. A second,
//! simpler scan in the second place is how `blue` came to launch
//! `…\AppData\Roaming\npm\codex` on Windows — npm's POSIX shell script, which
//! `CreateProcess` rejects with `%1 is not a valid Win32 application`
//! (`os error 193`) — instead of the `codex.cmd` sibling Windows can run.

use std::path::{Path, PathBuf};

use crate::harness::Harness;

/// Locate `name` on `PATH`, skipping Blue itself and any Blue shim.
///
/// A shim dispatches back through `blue run <harness>`, so resolving one as
/// the agent turns any `--version` probe into a recursive launch.
pub fn which(name: &str) -> Option<PathBuf> {
    which_all(name)
        .into_iter()
        .find(|path| !is_blue_or_shim(path))
}

/// Locate every executable named `name` in `PATH` order, shims included.
pub fn which_all(name: &str) -> Vec<PathBuf> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    which_all_in(std::env::split_paths(&path), name)
}

/// `which_all` over an explicit directory list, so tests can scan a fixture
/// directory without mutating the shared process environment.
pub fn which_all_in(dirs: impl IntoIterator<Item = PathBuf>, name: &str) -> Vec<PathBuf> {
    let mut matches = Vec::new();
    for dir in dirs {
        for candidate in executable_candidates(&dir, name) {
            if is_executable(&candidate) {
                matches.push(candidate);
            }
        }
    }
    matches
}

#[cfg(not(windows))]
fn executable_candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    vec![dir.join(name)]
}

#[cfg(windows)]
fn executable_candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    let pathext = std::env::var_os("PATHEXT").unwrap_or_else(|| DEFAULT_PATHEXT.into());
    executable_candidates_with(dir, name, &pathext.to_string_lossy())
}

/// The extensions `cmd.exe` tries for a bare command name when `PATHEXT` is
/// unset. npm's `.cmd` wrappers are the reason `.CMD` has to be among them.
#[cfg(windows)]
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// Windows has no executable bit: a bare command name is only executable via
/// one of the `PATHEXT` extensions. An extensionless file of the same name is
/// a POSIX script that happens to share the directory — every npm global
/// install writes one next to its `.cmd` — and is never a candidate.
#[cfg(windows)]
fn executable_candidates_with(dir: &Path, name: &str, pathext: &str) -> Vec<PathBuf> {
    let path = Path::new(name);
    if path.extension().is_some() {
        return vec![dir.join(path)];
    }
    pathext
        .split(';')
        .map(str::trim)
        .filter(|extension| extension.starts_with('.') && extension.len() > 1)
        .map(|extension| dir.join(format!("{name}{extension}")))
        .collect()
}

/// Whether `path` is the running `blue` binary or a shim that dispatches to it.
pub fn is_blue_or_shim(path: &Path) -> bool {
    if blue_executable().is_some_and(|current| {
        std::fs::canonicalize(path).is_ok_and(|candidate| paths_equal(current, &candidate))
    }) {
        return true;
    }
    // Every candidate in every PATH entry reaches this point, and most of them
    // are real binaries — `codex` among them. A shim is a short text file, so
    // reject the resolved candidate by size before reading it into memory.
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.len() <= crate::shim::MAX_SHIM_BYTES => {}
        _ => return false,
    }
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    Harness::ALL
        .iter()
        .copied()
        .any(|harness| crate::shim::managed_shim(&contents, harness))
}

/// The canonical path of the running `blue` binary, resolved once. Detection
/// consults it for every candidate in every PATH entry.
fn blue_executable() -> Option<&'static Path> {
    static EXECUTABLE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    EXECUTABLE
        .get_or_init(|| {
            std::env::current_exe()
                .ok()
                .and_then(|current| std::fs::canonicalize(current).ok())
        })
        .as_deref()
}

#[cfg(windows)]
fn paths_equal(left: &Path, right: &Path) -> bool {
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

#[cfg(not(windows))]
fn paths_equal(left: &Path, right: &Path) -> bool {
    left == right
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("blue-path-search-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn shim_shadowing_skips_both_shim_formats_without_reading_binaries() {
        let dir = fixture_dir("shim");

        #[cfg(windows)]
        let blue = Path::new(r"C:\Program Files\Blue\blue.exe");
        #[cfg(not(windows))]
        let blue = Path::new("/usr/local/bin/blue");

        let current = dir.join("codex-current");
        std::fs::write(
            &current,
            crate::shim::render_shim(blue, Harness::Codex).unwrap(),
        )
        .unwrap();
        assert!(is_blue_or_shim(&current));

        #[cfg(unix)]
        {
            let legacy = dir.join("codex-legacy");
            std::fs::write(
                &legacy,
                "#!/usr/bin/env bash\n# blue shim\nexec \"/usr/local/bin/blue\" run codex -- \"$@\"\n",
            )
            .unwrap();
            assert!(is_blue_or_shim(&legacy));
        }

        let unrelated = dir.join("codex-unrelated");
        std::fs::write(&unrelated, "#!/bin/sh\nexec /usr/bin/codex \"$@\"\n").unwrap();
        assert!(!is_blue_or_shim(&unrelated));

        // A candidate past the size bound is ruled out without being read,
        // which is the case that matters: `codex` itself is a native binary.
        let binary = dir.join("codex-binary");
        std::fs::write(
            &binary,
            vec![0_u8; crate::shim::MAX_SHIM_BYTES as usize + 1],
        )
        .unwrap();
        assert!(!is_blue_or_shim(&binary));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn oversized_symlink_target_is_rejected_before_reading() {
        use std::os::unix::fs::symlink;

        let dir = fixture_dir("oversized-symlink");
        let executable = PathBuf::from(format!(
            "/{}",
            "a".repeat(crate::shim::MAX_SHIM_BYTES as usize)
        ));
        let contents = crate::shim::render_shim(&executable, Harness::Codex).unwrap();
        assert!(contents.len() as u64 > crate::shim::MAX_SHIM_BYTES);

        let target = dir.join("target");
        std::fs::write(&target, contents).unwrap();
        let link = dir.join("codex");
        symlink(&target, &link).unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().len() <= crate::shim::MAX_SHIM_BYTES);
        assert!(std::fs::metadata(&link).unwrap().len() > crate::shim::MAX_SHIM_BYTES);
        assert!(!is_blue_or_shim(&link));

        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The npm global directory layout: `codex` (a POSIX shell script Windows
    /// cannot execute) beside the `codex.cmd` that it can.
    #[test]
    #[cfg(windows)]
    fn windows_resolves_the_cmd_wrapper_and_never_its_posix_sibling() {
        let dir = fixture_dir("npm");
        std::fs::write(dir.join("codex"), "#!/bin/sh\nexec node codex.js \"$@\"\n").unwrap();
        std::fs::write(dir.join("codex.cmd"), "@node codex.js %*\r\n").unwrap();

        // The candidate carries the casing of the `PATHEXT` entry it was built
        // from, not the casing on disk; Windows resolves either.
        let found = which_all_in([dir.clone()], "codex");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].parent(), Some(dir.as_path()));
        assert!(
            found[0]
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("codex.cmd")),
            "{found:?}"
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(windows)]
    fn pathext_entries_are_normalized_and_an_explicit_extension_is_kept() {
        let dir = Path::new(r"C:\bin");
        assert_eq!(
            executable_candidates_with(dir, "codex", ".COM; .CMD ;;.;"),
            vec![dir.join("codex.COM"), dir.join("codex.CMD")]
        );
        assert_eq!(
            executable_candidates_with(dir, "codex.exe", ".COM;.CMD"),
            vec![dir.join("codex.exe")]
        );
    }

    #[test]
    #[cfg(unix)]
    fn unix_resolves_the_bare_name() {
        use std::os::unix::fs::PermissionsExt;

        let dir = fixture_dir("unix");
        let codex = dir.join("codex");
        std::fs::write(&codex, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();

        let unreadable = dir.join("claude");
        std::fs::write(&unreadable, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(which_all_in([dir.clone()], "codex"), vec![codex]);
        assert!(which_all_in([dir.clone()], "claude").is_empty());

        std::fs::remove_dir_all(dir).unwrap();
    }
}
