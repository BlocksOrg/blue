//! PATH detection. Detect-not-bundle: we never ship the agent binaries, we
//! resolve the one the developer already installed and report its version.

use std::path::PathBuf;

use gh_common::Harness;
use semver::Version;

/// PATH resolution is shared with `gh-config`, which resolves the same
/// binaries when it selects a compatibility implementation.
pub use gh_common::path_search::{is_blue_or_shim, which, which_all};

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
    upstream_paths(harness)
        .into_iter()
        .next()
        .map(|path| detect_at(harness, path))
}

/// All upstream candidates in binary-name/PATH/PATHEXT order, excluding Blue.
pub fn upstream_paths(harness: Harness) -> Vec<PathBuf> {
    upstream_paths_with(harness, which_all)
}

fn upstream_paths_with(
    harness: Harness,
    candidates: impl Fn(&str) -> Vec<PathBuf>,
) -> Vec<PathBuf> {
    harness
        .binary_names()
        .iter()
        .flat_map(|name| candidates(name))
        .filter(|path| !is_blue_or_shim(path))
        .collect()
}

/// Inspect a specific executable, including shadowed copies for diagnostics.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn shared_candidates_preserve_path_order_and_duplicate_aliases_but_exclude_blue() {
        let root = std::env::temp_dir().join(format!("blue-upstream-paths-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let shim = root.join("shim");
        std::fs::write(
            &shim,
            gh_common::shim::render_shim(PathBuf::from("/usr/bin/blue").as_path(), Harness::Codex)
                .unwrap(),
        )
        .unwrap();
        let first = root.join("first");
        std::fs::write(&first, "upstream").unwrap();
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        let last = root.join("last");
        std::fs::write(&last, "second upstream").unwrap();
        let paths = upstream_paths_with(Harness::Codex, |name| {
            assert_eq!(name, "codex");
            vec![
                shim.clone(),
                first.clone(),
                alias.clone(),
                std::env::current_exe().unwrap(),
                last.clone(),
            ]
        });
        assert_eq!(paths, vec![first, alias, last]);
        std::fs::remove_dir_all(root).unwrap();
    }

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

        // A candidate past the size bound is ruled out without being read,
        // which is the case that matters: `codex` itself is a native binary.
        let binary = dir.join("codex-binary");
        std::fs::write(
            &binary,
            vec![0_u8; gh_common::shim::MAX_SHIM_BYTES as usize + 1],
        )
        .unwrap();
        assert!(!is_blue_or_shim(&binary));

        std::fs::remove_dir_all(dir).unwrap();
    }
}
