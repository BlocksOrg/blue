//! The one version string every client surface reports.
//!
//! Release builds carry `CARGO_PKG_VERSION`, which is the workspace version
//! `scripts/set-version.sh` writes and `scripts/check-release-version.sh`
//! guards. Release *candidates* are built from a ref whose tree still holds the
//! previous version, so the release workflow bakes the resolved candidate
//! string — `<version>-rc.g<sha7>` — in through `BLUE_BUILD_VERSION`. Without
//! it a candidate cut from the release branch self-reports as the release it is
//! only a candidate for, in `blue version`, in the splash line, in the
//! user-agent, and in the `client_version` the control API stores.
//!
//! Unset — every normal build, local or CI — the two are byte-identical.

/// The version this binary reports, everywhere.
pub fn blue_version() -> &'static str {
    option_env!("BLUE_BUILD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::blue_version;

    #[test]
    fn falls_back_to_the_package_version() {
        // The test suite never sets BLUE_BUILD_VERSION, so this pins the
        // fallback arm: an ordinary build reports exactly what it did before
        // the variable existed.
        assert_eq!(blue_version(), env!("CARGO_PKG_VERSION"));
        assert!(option_env!("BLUE_BUILD_VERSION").is_none());
    }
}
