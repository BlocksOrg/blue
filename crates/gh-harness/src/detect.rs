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
        if let Some(path) = which(name) {
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
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
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
}
