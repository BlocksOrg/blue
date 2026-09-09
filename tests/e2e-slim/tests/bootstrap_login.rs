//! (a) Bootstrap + login + `/auth/me` smoke.
//!
//! Proves the whole self-served auth path works end-to-end: a JWT we mint with
//! the committed test key is accepted by control-api after it fetches the JWK
//! from the sidecar and validates the signature, issuer, audience and expiry.

use predicates::str::contains;

#[test]
fn bootstrap_login_and_auth_me() {
    let Some(stack) = e2e_slim::env_or_skip() else {
        eprintln!("e2e-slim stack env unset; skipping");
        return;
    };
    let home = stack.bootstrap_home();

    // A valid, non-expired session.json means `blue login` short-circuits.
    home.blue()
        .arg("login")
        .assert()
        .success()
        .stdout(contains("Already logged in"));

    // `/auth/me` exercises the full JWKS verification path server-side.
    let (status, body) = home.auth_me();
    assert_eq!(
        status, 200,
        "auth/me should accept the minted token: {body}"
    );
    assert_eq!(
        body.get("role").and_then(|value| value.as_str()),
        Some("admin"),
        "auth/me role should be admin: {body}"
    );
    assert!(
        body.get("org_id")
            .and_then(|value| value.as_str())
            .is_some(),
        "auth/me should report an org_id: {body}"
    );
}
