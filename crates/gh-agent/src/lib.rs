//! `gh-agent` — reconciliation for managed coding-agent configuration. The
//! daemon keeps one explicitly selected harness current; other harnesses are
//! reconciled by the CLI when launched.
//!
//! `apply_once` reconciles a single config revision; `reconcile_loop` polls the
//! service on `revision`/TTL and reconciles on change. In gateway mode it also
//! publishes the inference token to GUI-visible environments so desktop apps see it.

use std::collections::{BTreeMap, BTreeSet};

use gh_common::{GhError, Harness};
use gh_config::{
    acquire_revision_locks, preflight_packages, preflight_packages_with_fetcher,
    resolve_compatibility, teardown_inactive_packages, write_harness_with_package_fetcher,
    write_harness_with_packages, AuthenticatedPackageFetcher, HarnessWrite, PackageFetcher,
    ProfileStatus, WriteOptions,
};
use gh_harness::HarnessInventory;
use gh_service::{GovernanceConfig, HarnessPolicy, ServiceClient, Session};

/// One harness's reconcile outcome.
pub struct HarnessReconcile {
    pub harness: Harness,
    pub result: Result<HarnessWrite, GhError>,
}

/// Annotate a PATH inventory with the exact compatibility decision that will
/// be used by reconciliation and launch.
pub fn evaluate_inventory(config: &GovernanceConfig, inventory: &mut HarnessInventory) {
    let empty = HarnessPolicy::default();
    for entry in &mut inventory.entries {
        if !(entry.api_allowed && entry.client_supported && entry.installed) {
            continue;
        }
        let Some(harness) = entry.harness() else {
            continue;
        };
        let policy = config.policy(&entry.name).unwrap_or(&empty);
        match resolve_compatibility(
            harness,
            entry.version.as_ref(),
            entry.raw_version.as_deref(),
            policy,
        ) {
            Ok(context) => {
                entry.compatibility_profile = Some(context.profile.id.to_owned());
                entry.compatibility_deprecated =
                    context.profile.status == ProfileStatus::Deprecated;
                entry.compatibility_error = None;
                entry.compatibility_warning = context.unverified_warning;
            }
            Err(error) => {
                entry.compatibility_profile = None;
                entry.compatibility_deprecated = false;
                entry.compatibility_error = Some(error.with_install_hint(policy));
                entry.compatibility_warning = None;
            }
        }
    }
}

/// Reconcile every **allowed** harness against a single config revision.
///
/// Unknown/unsupported harness keys in `allowed_harnesses` are skipped (not
/// fatal) so a newer server can list harnesses this client build doesn't have.
pub fn apply_once(config: &GovernanceConfig, opts: WriteOptions) -> Vec<HarnessReconcile> {
    let mut inventory = HarnessInventory::discover(&config.allowed_harnesses);
    evaluate_inventory(config, &mut inventory);
    apply_once_with_inventory(config, opts, &inventory)
}

/// Reconcile only the API-allowed harnesses that the supplied PATH inventory
/// found locally. Passing the precomputed inventory keeps the CLI preview and
/// the actual write set identical.
pub fn apply_once_with_inventory(
    config: &GovernanceConfig,
    opts: WriteOptions,
    inventory: &HarnessInventory,
) -> Vec<HarnessReconcile> {
    apply_once_with_inventory_and_optional_fetcher(config, opts, inventory, None)
}

/// Reconcile with a caller-provided package fetcher. HTTP clients use this so
/// governed package artifacts are downloaded with the active session.
pub fn apply_once_with_inventory_and_fetcher(
    config: &GovernanceConfig,
    opts: WriteOptions,
    inventory: &HarnessInventory,
    fetcher: &dyn PackageFetcher,
) -> Vec<HarnessReconcile> {
    apply_once_with_inventory_and_optional_fetcher(config, opts, inventory, Some(fetcher))
}

fn apply_once_with_inventory_and_optional_fetcher(
    config: &GovernanceConfig,
    opts: WriteOptions,
    inventory: &HarnessInventory,
    fetcher: Option<&dyn PackageFetcher>,
) -> Vec<HarnessReconcile> {
    let mut opts = opts;
    opts.session_upload_enabled = config.session_upload.is_some();
    let empty = HarnessPolicy::default();

    // Compatibility is a revision-wide preflight. Never begin mutating one
    // harness if another required, installed harness cannot be planned.
    let mut candidates = Vec::new();
    let mut preflight_failures = Vec::new();
    for entry in &inventory.entries {
        if entry.api_allowed && !entry.client_supported {
            tracing::warn!(harness = %entry.name, "server allows a harness this client doesn't support; skipping");
            continue;
        }
        if entry.api_allowed && entry.client_supported && !entry.installed {
            tracing::info!(harness = %entry.name, "allowed harness is not installed on PATH; skipping");
            continue;
        }
        if !(entry.api_allowed && entry.client_supported && entry.installed) {
            continue;
        }
        let Some(harness) = entry.harness() else {
            continue;
        };
        let policy = config.policy(&entry.name).unwrap_or(&empty);
        match resolve_compatibility(
            harness,
            entry.version.as_ref(),
            entry.raw_version.as_deref(),
            policy,
        ) {
            Err(error) => {
                preflight_failures.push((harness, error.with_install_hint(policy)));
            }
            Ok(context) => candidates.push((harness, policy, context)),
        }
    }
    if !preflight_failures.is_empty() {
        return candidates
            .into_iter()
            .map(|(harness, _, _)| HarnessReconcile {
                harness,
                result: Err(GhError::config(
                    "revision preflight failed for another required harness; no configuration was changed",
                )),
            })
            .chain(preflight_failures.into_iter().map(|(harness, error)| {
                HarnessReconcile {
                    harness,
                    result: Err(GhError::config(error)),
                }
            }))
            .collect();
    }

    // Hold every required harness lock in stable key order before package
    // state is read or any final implementation plan is produced. Do not take
    // the rollback snapshot until every package preflight has succeeded.
    let contexts = candidates
        .iter()
        .map(|(_, _, context)| context.clone())
        .collect::<Vec<_>>();
    let revision_locks = match acquire_revision_locks(&contexts) {
        Ok(locks) => locks,
        Err(error) => {
            let message = error.to_string();
            return candidates
                .into_iter()
                .map(|(harness, _, _)| HarnessReconcile {
                    harness,
                    result: Err(GhError::config(format!(
                        "acquiring revision locks: {message}"
                    ))),
                })
                .collect();
        }
    };

    // Package selection, downloads, archive/component validation, and helper
    // collision checks are also revision-wide preflight. Content populated
    // here is immutable and inactive until its harness commit persists state.
    let mut package_failures = Vec::new();
    for (harness, policy, context) in &candidates {
        let result = match fetcher {
            Some(fetcher) => {
                preflight_packages_with_fetcher(context, policy, &config.packages, fetcher)
            }
            None => preflight_packages(context, policy, &config.packages),
        };
        if let Err(error) = result {
            package_failures.push((*harness, error.to_string()));
        }
    }
    if !package_failures.is_empty() {
        return candidates
            .into_iter()
            .map(|(harness, _, _)| {
                let error = package_failures
                    .iter()
                    .find(|(failed, _)| *failed == harness)
                    .map(|(_, error)| error.clone())
                    .unwrap_or_else(|| {
                        "revision package preflight failed for another required harness; no configuration was changed".into()
                    });
                HarnessReconcile {
                    harness,
                    result: Err(GhError::config(error)),
                }
            })
            .collect();
    }

    let mut revision_transaction = match revision_locks.begin_transaction() {
        Ok(transaction) => transaction,
        Err(error) => {
            let message = error.to_string();
            return candidates
                .into_iter()
                .map(|(harness, _, _)| HarnessReconcile {
                    harness,
                    result: Err(GhError::config(format!(
                        "starting revision transaction: {message}"
                    ))),
                })
                .collect();
        }
    };

    let mut out = Vec::new();
    for (harness, policy, context) in candidates {
        let result = match fetcher {
            Some(fetcher) => write_harness_with_package_fetcher(
                &context,
                policy,
                &config.packages,
                config.gateway.as_ref(),
                opts,
                fetcher,
            ),
            None => write_harness_with_packages(
                &context,
                policy,
                &config.packages,
                config.gateway.as_ref(),
                opts,
            ),
        };
        out.push(HarnessReconcile { harness, result });
    }
    let all_succeeded = out.iter().all(|item| {
        item.result
            .as_ref()
            .is_ok_and(|write| write.package_errors.is_empty())
    });
    if all_succeeded {
        if let Err(error) = revision_transaction.commit() {
            let message = error.to_string();
            for item in &mut out {
                item.result = Err(GhError::config(format!(
                    "committing revision transaction: {message}"
                )));
            }
            return out;
        }
        for write in out.iter().filter_map(|item| item.result.as_ref().ok()) {
            if !write.env.is_empty() {
                publish_gui_env(&write.env);
            }
        }
        if let Err(error) = teardown_inactive_packages(&config.allowed_harnesses) {
            tracing::error!(%error, "post-commit managed package teardown failed");
        }
    }
    out
}

/// Poll the service and reconcile whenever the `revision` changes. Runs until
/// the process is killed. `sleep` is injected so this is testable / interruptible.
#[allow(clippy::too_many_arguments)]
pub fn reconcile_loop(
    client: &ServiceClient,
    session: &Session,
    target: Harness,
    opts: WriteOptions,
    now: impl Fn() -> i64,
    sleep: impl Fn(u64),
    mut on_reconciled: impl FnMut(&GovernanceConfig, &mut HarnessInventory, &[HarnessReconcile], bool),
    mut on_unauthorized: impl FnMut(&str),
) {
    let mut last_revision: Option<String> = None;
    let mut published_env: BTreeSet<String> = BTreeSet::new();
    let mut unauthorized = false;
    loop {
        let ttl = match client.fetch_or_cached(session, now()) {
            Ok(config) => {
                unauthorized = false;
                let packages_drifted = gh_config::package_statuses()
                    .map(|statuses| {
                        statuses.iter().any(|status| {
                            status.harness == target.key() && status.state != "applied"
                        })
                    })
                    .unwrap_or(true);
                let revision_changed = last_revision.as_deref() != Some(config.revision.as_str());
                if revision_changed || packages_drifted {
                    tracing::info!(revision = %config.revision, "reconciling governed config");
                } else {
                    tracing::debug!(revision = %config.revision, "checking governed config and local inventory");
                }
                // Re-run the idempotent reconciliation every poll so local
                // drift and newly-installed harness binaries converge even
                // when the server revision did not change.
                let mut inventory = HarnessInventory::discover(&config.allowed_harnesses);
                evaluate_inventory(&config, &mut inventory);
                let scoped_inventory = HarnessInventory {
                    entries: inventory
                        .entries
                        .iter()
                        .filter(|entry| entry.name == target.key())
                        .cloned()
                        .collect(),
                };
                let fetcher = AuthenticatedPackageFetcher { client, session };
                let results = apply_once_with_inventory_and_optional_fetcher(
                    &config,
                    opts,
                    &scoped_inventory,
                    Some(&fetcher),
                );
                let all_succeeded = results.iter().all(|result| {
                    result
                        .result
                        .as_ref()
                        .is_ok_and(|write| write.package_errors.is_empty())
                }) && scoped_inventory.entries.iter().all(|entry| {
                    !(entry.api_allowed && entry.client_supported && entry.installed)
                        || entry.compatibility_error.is_none()
                });
                for r in &results {
                    match &r.result {
                        Ok(w) => {
                            for warning in &w.warnings {
                                tracing::warn!(harness = %r.harness, %warning, "reconciled with warning");
                            }
                            if w.package_errors.is_empty() {
                                if revision_changed || packages_drifted {
                                    tracing::info!(harness = %r.harness, files = w.files.len(), "reconciled")
                                }
                            } else {
                                tracing::error!(harness = %r.harness, errors = ?w.package_errors, "configuration reconciled with package failures")
                            }
                        }
                        Err(e) => {
                            tracing::error!(harness = %r.harness, error = %e, "reconcile failed")
                        }
                    }
                }
                published_env.extend(
                    results
                        .iter()
                        .filter_map(|result| result.result.as_ref().ok())
                        .flat_map(|write| write.env.keys().cloned()),
                );
                on_reconciled(&config, &mut inventory, &results, all_succeeded);
                if all_succeeded {
                    last_revision = Some(config.revision.clone());
                } else {
                    tracing::warn!(revision = %config.revision, "revision was not marked applied because one or more required harness plans failed");
                }
                config.ttl_seconds()
            }
            Err(error) => {
                let (ttl, needs_user) = failure_backoff(&error);
                if needs_user {
                    // Reconciling here would rewrite agent config from a cache
                    // that carries no usable token. Back off, log once on the
                    // transition rather than every poll, and take the dead
                    // credentials back out of the GUI environment.
                    if !unauthorized {
                        unauthorized = true;
                        tracing::error!(%error, "control service needs the user to act; not reconciling");
                        if !published_env.is_empty() {
                            unpublish_gui_env(&published_env);
                            published_env.clear();
                        }
                    }
                    on_unauthorized(&error.to_string());
                } else {
                    unauthorized = false;
                    tracing::error!(%error, "config fetch failed; will retry");
                }
                ttl
            }
        };
        sleep(ttl);
    }
}

/// Poll interval after a failed fetch, and whether the failure is one only the
/// user can clear. An outage should be retried promptly; a dead session should
/// not be, because nothing the daemon does will fix it.
fn failure_backoff(error: &GhError) -> (u64, bool) {
    match error {
        GhError::ClientVersionMismatch { .. }
        | GhError::Unauthorized(_)
        | GhError::Forbidden(_)
        | GhError::ActionRequired(_) => (GovernanceConfig::DEFAULT_TTL_SECONDS * 4, true),
        _ => (GovernanceConfig::DEFAULT_TTL_SECONDS, false),
    }
}

/// Publish env vars to GUI-visible environments (macOS `launchctl setenv`,
/// Linux `systemctl --user set-environment`). Best-effort — GUI apps don't
/// inherit shell rc, so this is how Codex's `env_key` token reaches them.
pub fn publish_gui_env(vars: &BTreeMap<String, String>) {
    for (k, v) in vars {
        let ok = if cfg!(target_os = "macos") {
            run("launchctl", &["setenv", k, v])
        } else {
            run(
                "systemctl",
                &["--user", "set-environment", &format!("{k}={v}")],
            )
        };
        if !ok {
            tracing::debug!(var = %k, "could not publish env var to GUI environment (best-effort)");
        }
    }
}

/// Remove env vars from the GUI-visible environment. A GUI Codex that finds a
/// known-dead JWT there fails with an opaque proxy 401; one that finds nothing
/// says it has no credentials, which is both true and actionable.
pub fn unpublish_gui_env(names: &BTreeSet<String>) {
    for name in names {
        let ok = if cfg!(target_os = "macos") {
            run("launchctl", &["unsetenv", name])
        } else {
            run("systemctl", &["--user", "unset-environment", name])
        };
        if !ok {
            tracing::debug!(var = %name, "could not clear env var from GUI environment (best-effort)");
        }
    }
}

fn run(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_harness::{HarnessInventory, HarnessInventoryEntry};
    use gh_service::{ManagedPackage, PackageAdapter};

    struct EditingFailingFetcher {
        path: std::path::PathBuf,
        replacement: Vec<u8>,
    }

    impl PackageFetcher for EditingFailingFetcher {
        fn fetch(&self, _source_ref: &str, _artifact_id: Option<&str>) -> Result<Vec<u8>, GhError> {
            std::fs::write(&self.path, &self.replacement).unwrap();
            Err(GhError::other("injected package preflight failure"))
        }
    }

    fn config() -> GovernanceConfig {
        GovernanceConfig {
            revision: "r1".into(),
            contract_version: GovernanceConfig::CONTRACT_VERSION,
            required_capabilities: Vec::new(),
            minimum_client_version: None,
            required_client_version: None,
            ttl_seconds: None,
            allowed_harnesses: vec!["codex".into(), "claude".into()],
            harnesses: Default::default(),
            packages: Vec::new(),
            gateway: None,
            session_upload: None,
            telemetry: None,
            required: false,
        }
    }

    #[test]
    fn missing_allowed_harnesses_are_not_written() {
        let inventory = HarnessInventory {
            entries: vec![HarnessInventoryEntry {
                name: "codex".into(),
                api_allowed: true,
                client_supported: true,
                installed: false,
                path: None,
                raw_version: None,
                version: None,
                compatibility_profile: None,
                compatibility_deprecated: false,
                compatibility_error: None,
                compatibility_warning: None,
                reconciled: false,
            }],
        };
        assert!(
            apply_once_with_inventory(&config(), WriteOptions::default(), &inventory).is_empty()
        );
    }

    #[test]
    fn one_incompatible_harness_blocks_every_required_harness_before_writes() {
        let mut config = config();
        config.harnesses.insert(
            "claude".into(),
            HarnessPolicy {
                version_requirement: Some(">=2.0.0".into()),
                ..Default::default()
            },
        );
        let entry = |name: &str, version: semver::Version| HarnessInventoryEntry {
            name: name.into(),
            api_allowed: true,
            client_supported: true,
            installed: true,
            path: Some(std::path::PathBuf::from(format!("/fake/{name}"))),
            raw_version: Some(version.to_string()),
            version: Some(version),
            compatibility_profile: None,
            compatibility_deprecated: false,
            compatibility_error: None,
            compatibility_warning: None,
            reconciled: false,
        };
        let inventory = HarnessInventory {
            entries: vec![
                entry("codex", semver::Version::new(0, 149, 1)),
                entry("claude", semver::Version::new(1, 0, 0)),
            ],
        };
        let results = apply_once_with_inventory(&config, WriteOptions::default(), &inventory);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|result| result.result.is_err()));
        assert!(results.iter().any(|result| result
            .result
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("revision preflight failed")));
    }

    #[test]
    fn failed_package_preflight_child_helper() {
        let Some(home) = std::env::var_os("BLUE_PREFLIGHT_ROLLBACK_TEST_HOME") else {
            return;
        };
        let home = std::path::PathBuf::from(home);
        let native = home.join(".codex/config.toml");
        let concurrent_edit = b"model = \"concurrent-user-edit\"\n".to_vec();

        let mut config = config();
        config.allowed_harnesses = vec!["codex".into()];
        config.packages = vec![ManagedPackage {
            id: "failing-package".into(),
            name: None,
            version: "1.0.0".into(),
            source_ref: "mock://failing-package".into(),
            artifact_id: None,
            sha256: "a".repeat(64),
            platform_sources: Default::default(),
            settings: Default::default(),
            adapters: [(
                "codex".into(),
                PackageAdapter {
                    skills_dir: Some("skills".into()),
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
        }];
        let inventory = HarnessInventory {
            entries: vec![HarnessInventoryEntry {
                name: "codex".into(),
                api_allowed: true,
                client_supported: true,
                installed: true,
                path: Some(home.join("bin/codex")),
                raw_version: Some("0.149.1".into()),
                version: Some(semver::Version::new(0, 149, 1)),
                compatibility_profile: None,
                compatibility_deprecated: false,
                compatibility_error: None,
                compatibility_warning: None,
                reconciled: false,
            }],
        };
        let fetcher = EditingFailingFetcher {
            path: native.clone(),
            replacement: concurrent_edit.clone(),
        };

        let results = apply_once_with_inventory_and_fetcher(
            &config,
            WriteOptions::default(),
            &inventory,
            &fetcher,
        );
        assert_eq!(results.len(), 1);
        assert!(results[0].result.is_err());
        assert!(results[0]
            .result
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("injected package preflight failure"));
        assert_eq!(std::fs::read(&native).unwrap(), concurrent_edit);

        let transaction_root = home.join(".blue-transactions");
        assert!(
            !transaction_root.exists()
                || std::fs::read_dir(&transaction_root)
                    .unwrap()
                    .next()
                    .is_none(),
            "failed preflight must not create a transaction journal"
        );
        assert!(!home
            .join(".config/blue/runtime/codex/compatibility-state.json")
            .exists());
        assert!(!home.join(".config/blue/package-state.json").exists());
        assert!(!home.join(".config/blue/package-state/codex.json").exists());
        assert!(!home.join(".config/blue/packages/.staging").exists());
        assert!(no_transaction_staging_paths(&home));
    }

    fn no_transaction_staging_paths(path: &std::path::Path) -> bool {
        if path.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name.contains(".blue-stage-")
                || name.contains(".blue-write-")
                || name.contains(".blue-remove-")
        }) {
            return false;
        }
        if path.is_dir() {
            return std::fs::read_dir(path).unwrap().all(|entry| {
                let path = entry.unwrap().path();
                no_transaction_staging_paths(&path)
            });
        }
        true
    }

    #[test]
    fn failed_package_preflight_preserves_concurrent_native_edit_without_transaction_artifacts() {
        let home = std::env::temp_dir().join(format!(
            "blue-preflight-rollback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let native = home.join(".codex/config.toml");
        std::fs::create_dir_all(native.parent().unwrap()).unwrap();
        std::fs::write(&native, b"model = \"original\"\n").unwrap();

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::failed_package_preflight_child_helper"])
            .env("HOME", &home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_CACHE_HOME")
            .env("BLUE_PREFLIGHT_ROLLBACK_TEST_HOME", &home)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::read(&native).unwrap(),
            b"model = \"concurrent-user-edit\"\n"
        );
        std::fs::remove_dir_all(home).unwrap();
    }
}

#[cfg(test)]
mod unauthorized_tests {
    use super::*;

    #[test]
    fn only_a_rejected_session_backs_the_daemon_off() {
        assert_eq!(
            failure_backoff(&GhError::service("connection refused")),
            (GovernanceConfig::DEFAULT_TTL_SECONDS, false)
        );
        assert_eq!(
            failure_backoff(&GhError::unauthorized("your session is no longer valid")),
            (GovernanceConfig::DEFAULT_TTL_SECONDS * 4, true)
        );
        assert_eq!(
            failure_backoff(&GhError::forbidden("account is not provisioned")),
            (GovernanceConfig::DEFAULT_TTL_SECONDS * 4, true)
        );
        assert_eq!(
            failure_backoff(&GhError::action_required("run `blue gateway`")),
            (GovernanceConfig::DEFAULT_TTL_SECONDS * 4, true)
        );
    }
}
