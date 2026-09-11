//! (f) `blue login` must not claim a session the service has stopped accepting.
//!
//! The reported bug: bare `blue` fails with "your session is no longer valid",
//! `blue login` answers "Already logged in", and the loop never breaks. Both
//! statements were true of different things — `login` checked the local
//! `expires_at` and the OAuth refresh grant (30 days), while the control-api
//! checked the browser session that authorized the CLI (12 hours). Nothing on
//! the client asked the service.
//!
//! This suite has no browser session to expire (see
//! [`e2e_slim::Stack::expire_backing_session`], which needs gateway mode), so
//! the rejection is produced the other way an administrator can produce it:
//! `users.tokens_valid_after`. From the CLI's side the two are identical —
//! the stored token still refreshes, and the service still says no.

use predicates::str::contains;

#[test]
fn login_reports_a_session_the_service_rejects() {
    let Some(stack) = e2e_slim::env_or_skip() else {
        eprintln!("e2e-slim stack env unset; skipping");
        return;
    };
    let home = stack.bootstrap_home();

    // Baseline: the live probe must not break the healthy path.
    home.blue()
        .arg("login")
        .assert()
        .success()
        .stdout(contains("Already logged in"));

    stack.revoke_user_tokens(home.email());

    // The session file is untouched and still unexpired, so every local check
    // still passes. Only asking the service reveals the truth.
    let session: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.session_path()).expect("reading session.json"))
            .expect("parsing session.json");
    let expires_at = session
        .get("expires_at")
        .and_then(serde_json::Value::as_i64)
        .expect("session.json records expires_at");
    assert!(
        expires_at > time::OffsetDateTime::now_utc().unix_timestamp(),
        "the stored session must still look valid locally: {session}"
    );

    let login = home
        .blue()
        .arg("login")
        .output()
        .expect("running `blue login`");
    let stdout = String::from_utf8_lossy(&login.stdout);
    let stderr = String::from_utf8_lossy(&login.stderr);
    assert!(
        !stdout.contains("Already logged in"),
        "`blue login` must not claim a session the service rejects:\n{stdout}"
    );
    assert!(
        !login.status.success(),
        "`blue login` should fail when it cannot repair the session:\n{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("session is no longer valid"),
        "`blue login` should say the service rejected the session:\n{stderr}"
    );

    // And the same rejection, reported the same way, on a command that needs
    // policy — the two used to disagree, which is what made it a loop.
    let config = home
        .blue()
        .arg("config")
        .output()
        .expect("running `blue config`");
    let stderr = String::from_utf8_lossy(&config.stderr);
    assert!(
        !config.status.success(),
        "`blue config` should fail with a rejected session: {}",
        String::from_utf8_lossy(&config.stdout)
    );
    assert!(
        stderr.contains("session is no longer valid"),
        "`blue config` should report the same rejection:\n{stderr}"
    );
}
