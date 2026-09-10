//! PATH detection. Detect-not-bundle: we never ship the agent binaries, we
//! resolve the one the developer already installed and report its version.

use std::path::PathBuf;

use gh_common::Harness;
use semver::Version;

/// A harness found on `PATH`.
#[derive(Debug, Clone)]
pub struct Detected {
    pub harness: Harness,
    pub path: PathBuf,
    /// `--version` output (first line), best-effort.
    pub raw_version: Option<String>,
    /// Harness-specific normalized semantic version.
    pub version: Option<Version>,
}

/// Locate `harness` on `PATH`, if installed.
pub fn detect(harness: Harness) -> Option<Detected> {
    for name in harness.binary_names() {
        for path in which_all(name) {
            if is_blue_or_shim(&path) {
                continue;
            }
            return Some(detect_at(harness, path));
        }
    }
    None
}

/// Inspect a specific harness executable instead of resolving it from PATH.
/// Install repair uses this to prove that it updated the binary the user was
/// already launching, rather than a shadowed copy elsewhere on PATH.
pub fn detect_at(harness: Harness, path: PathBuf) -> Detected {
    let detected = gh_config::implementations::definition(harness)
        .detect_version(&path)
        .ok();
    let raw_version = detected.as_ref().map(|value| value.raw.clone());
    let version = detected.and_then(|value| value.version);
    Detected {
        harness,
        path,
        raw_version,
        version,
    }
}

/// Parse the deliberately small set of version formats emitted by supported
/// harnesses. The first semver-looking token is used, allowing vendor prefixes
/// such as `codex-cli 1.2.3` and a leading `v`.
pub fn parse_version(harness: Harness, raw: &str) -> Option<Version> {
    gh_config::implementations::definition(harness)
        .version_probes
        .iter()
        .find_map(|probe| (probe.parser)(raw))
}

/// Detect every known harness.
pub fn detect_all() -> Vec<(Harness, Option<Detected>)> {
    Harness::ALL.iter().map(|&h| (h, detect(h))).collect()
}

/// Minimal `which`: scan `$PATH` for an executable file named `name`.
pub fn which(name: &str) -> Option<PathBuf> {
    which_all(name)
        .into_iter()
        .find(|path| !is_blue_or_shim(path))
}

/// Locate every matching executable in PATH order.
pub fn which_all(name: &str) -> Vec<PathBuf> {
    let mut matches = Vec::new();
    let Some(path) = std::env::var_os("PATH") else {
        return matches;
    };
    for dir in std::env::split_paths(&path) {
        for candidate in executable_candidates(&dir, name) {
            if is_executable(&candidate) {
                matches.push(candidate);
            }
        }
    }
    matches
}

#[cfg(not(windows))]
fn executable_candidates(dir: &std::path::Path, name: &str) -> Vec<PathBuf> {
    vec![dir.join(name)]
}

#[cfg(windows)]
fn executable_candidates(dir: &std::path::Path, name: &str) -> Vec<PathBuf> {
    let path = std::path::Path::new(name);
    if path.extension().is_some() {
        return vec![dir.join(path)];
    }
    let pathext = std::env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
    pathext
        .to_string_lossy()
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| dir.join(format!("{name}{extension}")))
        .collect()
}

fn is_blue_or_shim(path: &std::path::Path) -> bool {
    if std::env::current_exe()
        .ok()
        .and_then(|current| std::fs::canonicalize(current).ok())
        .zip(std::fs::canonicalize(path).ok())
        .is_some_and(|(current, candidate)| paths_equal(&current, &candidate))
    {
        return true;
    }
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    Harness::ALL
        .iter()
        .copied()
        .any(|harness| gh_common::shim::managed_shim(&contents, harness))
}

#[cfg(windows)]
fn paths_equal(left: &std::path::Path, right: &std::path::Path) -> bool {
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

#[cfg(not(windows))]
fn paths_equal(left: &std::path::Path, right: &std::path::Path) -> bool {
    left == right
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prefixed_harness_versions() {
        assert_eq!(
            parse_version(Harness::Codex, "codex-cli 1.24.3"),
            Some(Version::new(1, 24, 3))
        );
        assert_eq!(
            parse_version(Harness::Claude, "claude v2.1.0-beta.2"),
            Some(Version::parse("2.1.0-beta.2").unwrap())
        );
        assert!(parse_version(Harness::Kimi, "unknown").is_none());
    }

    #[test]
    #[cfg(unix)]
    fn shim_shadowing_skips_both_shim_formats_without_reading_binaries() {
        let dir = std::env::temp_dir().join(format!("blue-detect-shim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let current = dir.join("codex-current");
        std::fs::write(
            &current,
            gh_common::shim::render_shim(
                std::path::Path::new("/usr/local/bin/blue"),
                Harness::Codex,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(is_blue_or_shim(&current));

        let legacy = dir.join("codex-legacy");
        std::fs::write(
            &legacy,
            "#!/usr/bin/env bash\n# blue shim\nexec \"/usr/local/bin/blue\" run codex -- \"$@\"\n",
        )
        .unwrap();
        assert!(is_blue_or_shim(&legacy));

        let unrelated = dir.join("codex-unrelated");
        std::fs::write(&unrelated, "#!/bin/sh\nexec /usr/bin/codex \"$@\"\n").unwrap();
        assert!(!is_blue_or_shim(&unrelated));

        std::fs::remove_dir_all(dir).unwrap();
    }
}
