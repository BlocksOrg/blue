//! (d) `blue session-upload` → MinIO round-trip, per harness.
//!
//! For each of the four harnesses we lay out a native session on disk (the
//! bundle validator requires every artifact to live inside the harness's real
//! native session root and match the advertised session id), feed a lifecycle
//! hook payload into `blue session-upload`, which captures the session into a
//! portable bundle, spools it, and spawns a detached worker that presigns →
//! PUTs the blob → completes the record. We then poll the control-api list
//! until all four captured sessions appear, and for one of them assert the
//! stored artifact is `complete` and the transcript inside the downloaded
//! bundle round-trips exactly (proving the presigned PUT/GET against the
//! host-published MinIO endpoint).
//!
//! Unlike the config tests this needs no agent CLI installed — `session-upload`
//! only needs a valid compatibility profile and a capturable native session —
//! so it runs whenever the stack is up.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// `(harness, profile, transcript role in the bundle manifest)`.
const HARNESSES: [(&str, &str, &str); 4] = [
    ("codex", "codex-v1", "rollout"),
    ("claude", "claude-v1", "primary_transcript"),
    ("kimi", "kimi-v1", "agent_wire_history"),
    ("opencode", "opencode-v1", "portable_export"),
];

/// Lay out a minimal but contract-valid native session for `harness` and return
/// the transcript path. The bundle validator pins each harness's artifacts to
/// its real native session root, so the fixture mirrors the native layout:
/// Codex rollouts under `.codex/sessions`, Claude transcripts under
/// `.claude/projects`, Kimi wire history (plus its mandatory `state.json`)
/// under `.kimi-code/sessions/<project>/<session>`. OpenCode's portable export
/// is imported natively rather than restored to a home path, so any location
/// works.
fn write_native_session(home: &Path, harness: &str, session_id: &str, bytes: &[u8]) -> PathBuf {
    let transcript_path = match harness {
        "codex" => home.join(format!(".codex/sessions/rollout-{session_id}.jsonl")),
        "claude" => home.join(format!(".claude/projects/e2e-slim/{session_id}.jsonl")),
        "kimi" => home.join(format!(
            ".kimi-code/sessions/e2e-slim/{session_id}/agents/main/wire.jsonl"
        )),
        "opencode" => home.join(format!("{harness}-transcript.json")),
        _ => unreachable!("unknown harness {harness}"),
    };
    std::fs::create_dir_all(transcript_path.parent().expect("transcript parent"))
        .expect("creating native session directories");
    std::fs::write(&transcript_path, bytes).expect("writing transcript fixture");
    if harness == "kimi" {
        let state = home.join(format!(
            ".kimi-code/sessions/e2e-slim/{session_id}/state.json"
        ));
        std::fs::write(&state, format!("{{\"id\":\"{session_id}\"}}\n"))
            .expect("writing kimi state fixture");
    }
    transcript_path
}

#[test]
fn session_upload_round_trip() {
    let Some(stack) = e2e_slim::env_or_skip() else {
        eprintln!("e2e-slim stack env unset; skipping");
        return;
    };
    let home = stack.bootstrap_home();

    // Upload one native session per harness, each with distinct transcript
    // bytes and a unique native session id.
    let mut transcripts: Vec<(&str, &str, String, Vec<u8>)> = Vec::new();
    for (harness, profile, role) in HARNESSES {
        let session_id = format!("e2e-slim-{harness}-session");
        let bytes = format!(
            "{{\"harness\":\"{harness}\",\"session\":\"{session_id}\",\"marker\":\"BLUE_SLIM_OK\"}}\n"
        )
        .into_bytes();
        let transcript_path = write_native_session(home.path(), harness, &session_id, &bytes);

        let payload = serde_json::json!({
            "session_id": session_id,
            "transcript_path": transcript_path,
            "cwd": home.path(),
        });
        home.blue()
            .args(["session-upload", harness, "--profile", profile])
            .write_stdin(serde_json::to_vec(&payload).expect("encoding hook payload"))
            .assert()
            .success();

        transcripts.push((harness, role, session_id, bytes));
    }

    // The uploads happen in detached workers; poll until all four of THIS
    // test's sessions land. Matching our own native session ids (not a bare
    // item count) keeps the wait immune to sessions uploaded concurrently by
    // other tests — the certification tests' session-upload hooks land real
    // agent sessions in the same list.
    let items = e2e_slim::wait_for("4 captured sessions", Duration::from_secs(60), || {
        let body = home.list_session_uploads(25);
        let items = body
            .get("items")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        transcripts
            .iter()
            .all(|(harness, _, session_id, _)| {
                items.iter().any(|item| {
                    item.get("harness").and_then(serde_json::Value::as_str) == Some(*harness)
                        && item
                            .get("native_session_id")
                            .and_then(serde_json::Value::as_str)
                            == Some(session_id.as_str())
                })
            })
            .then_some(items)
    });

    // Assert every harness is represented among the captured sessions.
    for (harness, _, session_id, _) in &transcripts {
        assert!(
            items.iter().any(|item| {
                item.get("harness").and_then(serde_json::Value::as_str) == Some(*harness)
                    && item
                        .get("native_session_id")
                        .and_then(serde_json::Value::as_str)
                        == Some(session_id.as_str())
            }),
            "captured sessions should include {harness}/{session_id}: {items:#?}"
        );
    }

    // Deep-check one harness: artifact status complete + the transcript inside
    // the downloaded bundle round-trips exactly.
    let (probe_harness, probe_role, probe_session, probe_bytes) = &transcripts[0];
    let id = items
        .iter()
        .find(|item| {
            item.get("harness").and_then(serde_json::Value::as_str) == Some(*probe_harness)
                && item
                    .get("native_session_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(probe_session.as_str())
        })
        .and_then(|item| item.get("id").and_then(serde_json::Value::as_str))
        .expect("probe session id")
        .to_owned();

    let detail = home.get_upload(&id);
    let status = detail
        .pointer("/artifacts/0/status")
        .and_then(serde_json::Value::as_str);
    assert_eq!(
        status,
        Some("complete"),
        "probe artifact should be complete: {detail}"
    );

    // The stored artifact is a portable session bundle, not the raw transcript
    // bytes: unpack it, check the manifest advertises the probe session, and
    // byte-compare the archived transcript against the uploaded fixture.
    let bundle = e2e_slim::parse_session_bundle(&home.download_upload(&id));
    assert_eq!(
        bundle
            .manifest
            .get("harness")
            .and_then(serde_json::Value::as_str),
        Some(*probe_harness),
        "bundle manifest should advertise the probe harness: {}",
        bundle.manifest
    );
    assert_eq!(
        bundle
            .manifest
            .get("native_session_id")
            .and_then(serde_json::Value::as_str),
        Some(probe_session.as_str()),
        "bundle manifest should advertise the probe session: {}",
        bundle.manifest
    );
    let archived = bundle
        .file_by_role(probe_role)
        .unwrap_or_else(|| panic!("bundle has no {probe_role} artifact: {}", bundle.manifest));
    assert_eq!(
        archived, *probe_bytes,
        "archived transcript bytes should match the uploaded transcript exactly"
    );
}
