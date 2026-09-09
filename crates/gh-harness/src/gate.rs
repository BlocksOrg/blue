//! Policy gate. Launching a harness not in the service's `allowed_harnesses`
//! is a hard error listing the allowed set.

use gh_common::{GhError, Harness};

/// Return `Ok` iff `harness` is present in `allowed`. Keyed on the harness key
/// so the caller can pass `GovernanceConfig.allowed_harnesses` directly.
pub fn ensure_allowed(harness: Harness, allowed: &[String]) -> Result<(), GhError> {
    if allowed.iter().any(|a| a == harness.key()) {
        Ok(())
    } else {
        let allowed_list = if allowed.is_empty() {
            "(none)".to_string()
        } else {
            allowed.join(", ")
        };
        Err(GhError::HarnessNotAllowed {
            requested: harness.key().to_string(),
            allowed: allowed_list,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_listed_denies_unlisted() {
        let allowed = vec!["codex".to_string(), "claude".to_string()];
        assert!(ensure_allowed(Harness::Codex, &allowed).is_ok());
        assert!(matches!(
            ensure_allowed(Harness::Kimi, &allowed),
            Err(GhError::HarnessNotAllowed { .. })
        ));
    }

    #[test]
    fn empty_allowlist_denies_all() {
        assert!(ensure_allowed(Harness::Codex, &[]).is_err());
    }
}
