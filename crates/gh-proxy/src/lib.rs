//! `gh-proxy` (fast-follow, M5b) — the local CA-less attribution proxy.
//!
//! Attribution is a core goal: associate each inference request with local
//! workflow context (cwd, git branch/commit, repo remote, dirty state) the
//! server can't otherwise see. The mechanism reuses base-URL indirection we
//! already own: point the agent's `base_url` at a loopback proxy that injects
//! `X-Harness-*` headers and **re-originates TLS** to the real upstream — no CA
//! install, no `HTTP_PROXY`.
//!
//! v1 ships the **context-gathering half** (below), which is fully testable; the
//! forwarding server is the M5b deliverable (see [`serve_ephemeral`]).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use gh_common::GhError;

/// Header names injected on outbound requests to a trusted upstream. Metadata
/// only — never file contents or prompts.
pub mod headers {
    pub const CWD: &str = "X-Harness-Cwd";
    pub const GIT_BRANCH: &str = "X-Harness-Git-Branch";
    pub const REPO: &str = "X-Harness-Repo";
    pub const COMMIT: &str = "X-Harness-Commit";
    pub const DIRTY: &str = "X-Harness-Dirty";
    pub const RUN_ID: &str = "X-Harness-Run-Id";
    pub const AGENT: &str = "X-Harness-Agent";
}

/// Local workflow context captured for attribution. Git fields are re-read per
/// request (cheap) so branch/commit switches are caught; runtime `cd` inside a
/// session is a known blind spot (plan risk #15).
#[derive(Debug, Clone, Default)]
pub struct AttributionContext {
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub repo: Option<String>,
    pub commit: Option<String>,
    pub dirty: Option<bool>,
    pub run_id: Option<String>,
    pub agent: Option<String>,
}

impl AttributionContext {
    /// Gather context rooted at `dir` (the launch repo root).
    pub fn gather(dir: &Path) -> Self {
        AttributionContext {
            cwd: Some(dir.display().to_string()),
            git_branch: git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            repo: git(dir, &["config", "--get", "remote.origin.url"]),
            commit: git(dir, &["rev-parse", "HEAD"]),
            dirty: git(dir, &["status", "--porcelain"]).map(|s| !s.trim().is_empty()),
            run_id: None,
            agent: None,
        }
    }

    /// Render the non-empty fields as HTTP header pairs.
    pub fn to_headers(&self) -> BTreeMap<String, String> {
        let mut h = BTreeMap::new();
        let mut put = |k: &str, v: &Option<String>| {
            if let Some(val) = v {
                if !val.is_empty() {
                    h.insert(k.to_string(), val.clone());
                }
            }
        };
        put(headers::CWD, &self.cwd);
        put(headers::GIT_BRANCH, &self.git_branch);
        put(headers::REPO, &self.repo);
        put(headers::COMMIT, &self.commit);
        put(headers::RUN_ID, &self.run_id);
        put(headers::AGENT, &self.agent);
        if let Some(d) = self.dirty {
            h.insert(headers::DIRTY.to_string(), d.to_string());
        }
        h
    }
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Start an ephemeral per-run loopback proxy bound to `ctx`, forwarding to
/// `upstream` with the `X-Harness-*` headers injected. **Not yet implemented**
/// (M5b) — the context-gathering half above is what's shipped in v1.
pub fn serve_ephemeral(_upstream: &str, _ctx: AttributionContext) -> Result<u16, GhError> {
    Err(GhError::other(
        "attribution proxy is a fast-follow (M5b); base_url currently points at the upstream proxy directly",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_skip_empty_fields() {
        let ctx = AttributionContext {
            cwd: Some("/work/repo".into()),
            git_branch: Some("main".into()),
            dirty: Some(true),
            ..Default::default()
        };
        let h = ctx.to_headers();
        assert_eq!(h.get(headers::CWD).unwrap(), "/work/repo");
        assert_eq!(h.get(headers::DIRTY).unwrap(), "true");
        assert!(!h.contains_key(headers::REPO));
    }
}
