//! `gh-telemetry` (optional) — metadata-only, redacted run events posted to the
//! operator-configured sink. Never carries file contents or prompts. This is
//! the *client-side* attribution path (Layer B); the strongest attribution is
//! server-side in gateway mode (see `gh-proxy` + the inference proxy).

use serde::{Deserialize, Serialize};

use gh_common::{GhError, Harness};

/// A single governed-run event. Metadata only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunEvent {
    pub run_id: String,
    pub harness: String,
    /// Governance-config revision in force for this run.
    pub revision: String,
    /// Whether inference was routed through the gateway.
    pub gateway: bool,
    /// Unix seconds.
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

impl RunEvent {
    pub fn start(
        run_id: impl Into<String>,
        harness: Harness,
        revision: impl Into<String>,
        gateway: bool,
        now: i64,
    ) -> Self {
        RunEvent {
            run_id: run_id.into(),
            harness: harness.key().to_string(),
            revision: revision.into(),
            gateway,
            started_at: now,
            exit_code: None,
        }
    }
}

/// Best-effort POST of an event to the sink. Telemetry failures never fail a run.
pub fn emit(sink_url: &str, event: &RunEvent) -> Result<(), GhError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| GhError::other(e.to_string()))?;
    let resp = client
        .post(sink_url)
        .json(event)
        .send()
        .map_err(|e| GhError::service(format!("telemetry POST: {e}")))?;
    if !resp.status().is_success() {
        tracing::debug!(status = %resp.status(), "telemetry sink returned non-2xx");
    }
    Ok(())
}
