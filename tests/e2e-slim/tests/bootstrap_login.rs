//! (a) Bootstrap + login + `/auth/me` smoke.
//!
//! Proves the whole self-served auth path works end-to-end: a JWT we mint with
//! the committed test key is accepted by control-api after it fetches the JWK
//! from the sidecar and validates the signature, issuer, audience and expiry.
//!
//! `blue login` reaching "Already logged in" is a **live** result, not a local
//! one: it asks the service before it says so. It passes here because
//! `Stack::bootstrap_home` seeds the backing `auth."session"` row the token's
//! `sid` points at. Drop that seeding and this test fails for a reason nothing
//! in this file mentions. The rejection side is covered by
//! `session_recovery.rs`.

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

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(home.session_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    #[cfg(windows)]
    {
        // Local Administrators/SYSTEM are expected; other ordinary accounts are not.
        let status = std::process::Command::new("powershell.exe")
            .env("BLUE_TEST_SESSION", home.session_path())
            .args(["-NoProfile", "-NonInteractive", "-Command", r#"
$ErrorActionPreference = 'Stop'
$acl = Get-Acl -LiteralPath $env:BLUE_TEST_SESSION
$user = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$allowed = @($user, 'S-1-5-18', 'S-1-5-32-544', 'S-1-3-0')
foreach ($entry in $acl.Access) {
  $sid = $entry.IdentityReference.Translate([System.Security.Principal.SecurityIdentifier]).Value
  if ($entry.AccessControlType -eq 'Allow' -and $sid -notin $allowed) { throw "Unexpected session ACL principal $sid" }
}
"#]).status().expect("checking native session ACL");
        assert!(
            status.success(),
            "session file must be isolated to the disposable account"
        );
    }

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
