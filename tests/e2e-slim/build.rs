//! Generate one nextest case per `(agent, version)` version-matrix cell from the
//! committed data, so the case list stays data-driven — no hand-maintained tuple
//! list in Rust, no drift as versions come and go.
//!
//! Cells = the current lock pin (from `agents.lock.json`, the newest/blessed
//! version) plus every historical sample in `agents.matrix.json`. Two files land
//! in `OUT_DIR`, each `include!`d by the matching test:
//!   * `matrix_config_cases.rs` — `<agent>_<ver>_managed_config()` (Tier A).
//!   * `matrix_cert_cases.rs`   — `<agent>_<ver>_certifies()` (Tier B).
//!
//! Both delegate to a shared body in their test file; the body self-skips when
//! the matrix manifest (`E2E_SLIM_AGENT_MATRIX`) is absent.

use std::fmt::Write as _;
use std::path::Path;

/// The governed agents, in a stable order (matches `common::AGENTS`).
const AGENTS: [&str; 4] = ["codex", "claude", "kimi", "opencode"];

fn read_json(path: &Path) -> serde_json::Value {
    let bytes =
        std::fs::read(path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("parsing {}: {error}", path.display()))
}

/// Turn a semver into a valid Rust identifier fragment (`0.146.0` → `0_146_0`).
fn sanitize(version: &str) -> String {
    version
        .chars()
        .map(|c| if c == '.' || c == '-' { '_' } else { c })
        .collect()
}

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let lock_path = Path::new(&manifest_dir).join("../e2e/agents.lock.json");
    let matrix_path = Path::new(&manifest_dir).join("agents.matrix.json");
    println!("cargo:rerun-if-changed={}", lock_path.display());
    println!("cargo:rerun-if-changed={}", matrix_path.display());
    // The lock is a symlink into ../e2e; rerun if its target changes too.
    if let Ok(target) = std::fs::canonicalize(&lock_path) {
        println!("cargo:rerun-if-changed={}", target.display());
    }

    let lock = read_json(&lock_path);
    let matrix = read_json(&matrix_path);

    // Collect (agent, version) cells: the lock pin first (newest/blessed), then
    // the historical samples from the matrix.
    let mut cells: Vec<(&str, String)> = Vec::new();
    for agent in AGENTS {
        let pin = lock
            .pointer(&format!("/agents/{agent}/version"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("agents.lock.json has no version for {agent}"));
        cells.push((agent, pin.to_owned()));

        if let Some(versions) = matrix
            .pointer(&format!("/agents/{agent}/versions"))
            .and_then(serde_json::Value::as_array)
        {
            for entry in versions {
                let version = entry
                    .get("version")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_else(|| {
                        panic!("agents.matrix.json cell for {agent} has no version")
                    });
                cells.push((agent, version.to_owned()));
            }
        }
    }

    let mut config = String::new();
    let mut cert = String::new();
    for (agent, version) in &cells {
        let ident = format!("{agent}_{}", sanitize(version));
        writeln!(
            config,
            "#[test]\nfn {ident}_managed_config() {{ matrix_managed_config(\"{agent}\", \"{version}\"); }}"
        )
        .unwrap();
        writeln!(
            cert,
            "#[test]\nfn {ident}_certifies() {{ certify_cell(\"{agent}\", \"{version}\"); }}"
        )
        .unwrap();
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    std::fs::write(Path::new(&out_dir).join("matrix_config_cases.rs"), config)
        .expect("writing matrix_config_cases.rs");
    std::fs::write(Path::new(&out_dir).join("matrix_cert_cases.rs"), cert)
        .expect("writing matrix_cert_cases.rs");
}
