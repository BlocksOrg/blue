//! `gh-config` builds launch-scoped, metaharness-owned runtime overlays. Native
//! user configuration is read as merge input but is not a governance target.
//! Each writer also returns the arguments/environment needed to opt the child
//! process into its overlay.

mod adapters;
pub mod implementations {
    //! Public harness implementation contract and compiled registry.
    pub use super::adapters::*;
}
mod compat;
mod package_archive;
mod packages;
mod plan;
pub mod session_bundle;
mod transaction;
mod util;

use std::collections::BTreeMap;
use std::path::PathBuf;

use gh_common::{paths, GhError, Harness};
use gh_service::{GatewayConfig, HarnessPolicy, ManagedPackage};
use serde::{Deserialize, Serialize};

use transaction::FileTransaction;

/// Resolve Blue's mutable data root while preserving hermetic `_at(home)` test
/// APIs. Production callers use the native `ClientPaths` roots.
pub(crate) fn managed_data_dir(home: &std::path::Path) -> PathBuf {
    if paths::home_dir().is_ok_and(|current| current == home) {
        paths::blue_data_dir().unwrap_or_else(|_| home.join(".config/blue"))
    } else {
        home.join(".config/blue")
    }
}

pub(crate) fn managed_runtime_dir(home: &std::path::Path) -> PathBuf {
    managed_data_dir(home).join("runtime")
}

pub use compat::{
    resolve as resolve_compatibility, supported_install, validate_package_adapter_for_policy,
    validate_package_adapters_for_policy, CompatibilityFailure, HarnessContext, ProfileStatus,
};
pub use packages::PackageFetcher;

pub struct AuthenticatedPackageFetcher<'a> {
    pub client: &'a gh_service::ServiceClient,
    pub session: &'a gh_service::Session,
}

impl PackageFetcher for AuthenticatedPackageFetcher<'_> {
    fn fetch(&self, source_ref: &str, artifact_id: Option<&str>) -> Result<Vec<u8>, GhError> {
        match artifact_id {
            Some(id) => self.client.download_package_artifact(self.session, id),
            None => packages::DirectPackageFetcher.fetch(source_ref, None),
        }
    }
}

/// Result of writing one harness's config.
#[derive(Debug, Default)]
pub struct HarnessWrite {
    /// Files (and skill dirs) written, for reporting.
    pub files: Vec<PathBuf>,
    /// Env vars the launcher/daemon must set so the harness sees the gateway
    /// token (Codex `env_key`; empty for in-file harnesses).
    pub env: BTreeMap<String, String>,
    /// Arguments prepended when launching the native CLI (for example a
    /// Codex profile or Claude's additional settings files).
    pub launch_args: Vec<String>,
    /// Package failures are reported independently so unrelated packages and
    /// harness configuration can still converge.
    pub package_errors: Vec<String>,
    /// Non-fatal capability gaps discovered by the selected version adapter.
    pub warnings: Vec<String>,
}

/// Read-only process configuration for launching an already-reconciled
/// harness. Unlike `HarnessWrite`, producing this value never mutates state.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct HarnessLaunchSpec {
    pub env: BTreeMap<String, String>,
    pub launch_args: Vec<String>,
}

impl HarnessWrite {
    /// Fold another write's files/env into this one (used when a harness emits
    /// multiple config files across helpers).
    pub fn merge(&mut self, other: HarnessWrite) {
        self.files.extend(other.files);
        self.env.extend(other.env);
        self.launch_args.extend(other.launch_args);
        self.package_errors.extend(other.package_errors);
        self.warnings.extend(other.warnings);
    }
}

/// Knobs that affect *how* config is written, independent of the policy.
#[derive(Debug, Clone, Copy)]
pub struct WriteOptions {
    /// When false, inference routing is never written even if the global policy
    /// has a `gateway` block (governance-only, e.g. local development).
    pub gateway_enabled: bool,
    /// Claude only: also write the un-overridable `managed-settings.json`.
    pub enforced: bool,
    /// The caller explicitly approved merging into existing user config.
    pub allow_existing_merge: bool,
    /// Install each compatible harness's native session-upload hook.
    pub session_upload_enabled: bool,
}

impl Default for WriteOptions {
    fn default() -> Self {
        WriteOptions {
            gateway_enabled: true,
            enforced: false,
            allow_existing_merge: false,
            session_upload_enabled: false,
        }
    }
}

pub fn resolve_launch_spec(
    context: &HarnessContext,
    policy: &HarnessPolicy,
    packages: &[ManagedPackage],
    gateway: Option<&GatewayConfig>,
    opts: WriteOptions,
) -> Result<HarnessLaunchSpec, GhError> {
    let home = paths::home_dir()?;
    resolve_launch_spec_at(&home, context, policy, packages, gateway, opts)
}

fn resolve_launch_spec_at(
    home: &std::path::Path,
    context: &HarnessContext,
    policy: &HarnessPolicy,
    packages: &[ManagedPackage],
    gateway: Option<&GatewayConfig>,
    opts: WriteOptions,
) -> Result<HarnessLaunchSpec, GhError> {
    let wiring = if opts.gateway_enabled {
        gateway
            .map(|gateway| context.profile.implementation.gateway_wiring(gateway))
            .transpose()?
    } else {
        None
    };
    let mut activation = packages::active_launch(context, packages, policy)?;
    crate::util::prepend_helper_paths(&mut activation.env, &activation.helpers);
    let spec = HarnessLaunchSpec {
        env: activation.env,
        launch_args: activation.launch_args,
    };
    context
        .profile
        .implementation
        .launch(home, wiring.as_ref(), spec)
}

/// Write all managed files for a single harness. This is the one entry point.
pub fn write_harness(
    context: &HarnessContext,
    policy: &HarnessPolicy,
    gateway: Option<&GatewayConfig>,
    opts: WriteOptions,
) -> Result<HarnessWrite, GhError> {
    write_harness_with_packages(context, policy, &[], gateway, opts)
}

/// Write a harness overlay and activate organization-managed packages.
pub fn write_harness_with_packages(
    context: &HarnessContext,
    policy: &HarnessPolicy,
    packages: &[ManagedPackage],
    gateway: Option<&GatewayConfig>,
    opts: WriteOptions,
) -> Result<HarnessWrite, GhError> {
    write_harness_with_package_fetcher(
        context,
        policy,
        packages,
        gateway,
        opts,
        &packages::DirectPackageFetcher,
    )
}

pub fn write_harness_with_package_fetcher(
    context: &HarnessContext,
    policy: &HarnessPolicy,
    packages: &[ManagedPackage],
    gateway: Option<&GatewayConfig>,
    opts: WriteOptions,
    fetcher: &dyn PackageFetcher,
) -> Result<HarnessWrite, GhError> {
    let harness = context.harness;
    let home = paths::home_dir()?;
    // A single global gateway applies uniformly to every reconciled harness.
    let wiring = if opts.gateway_enabled {
        gateway
            .map(|gateway| context.profile.implementation.gateway_wiring(gateway))
            .transpose()?
    } else {
        None
    };
    let wiring_ref = wiring.as_ref();

    let input = adapters::ReconcileInput {
        home: &home,
        policy,
        gateway: wiring_ref,
        options: opts,
        interval: &context.profile.interval,
    };
    let _lock = ReconcileLock::acquire(&home, harness)?;
    let mut package_write = packages::reconcile_with_fetcher(context, packages, policy, fetcher)?;
    if !package_write.errors.is_empty() {
        packages::commit_activation_state(harness, &mut package_write)?;
        return Ok(HarnessWrite {
            files: package_write.files,
            env: package_write.env,
            launch_args: package_write.launch_args,
            package_errors: package_write.errors,
            warnings: Vec::new(),
        });
    }
    reconcile_prepared_at(context, &input, &mut package_write, |prepared| {
        packages::commit_activation_state(harness, prepared)
    })
}

fn reconcile_prepared_at(
    context: &HarnessContext,
    input: &adapters::ReconcileInput<'_>,
    package_write: &mut packages::PreparedPackages,
    commit_packages: impl FnOnce(&mut packages::PreparedPackages) -> Result<(), GhError>,
) -> Result<HarnessWrite, GhError> {
    let home = input.home;
    // The selected implementation constructs exact bytes, modes, removals,
    // ownership, environment, and launch arguments without touching active
    // paths. Nothing changes until the complete plan validates and is staged.
    // Compatibility state is mutable input to planning: stale outputs from a
    // previous implementation must be present before the transaction takes
    // its snapshot, never deleted afterward as a finalize side effect.
    let previous_state = load_definition_state_at(home, context.definition)?;
    let mut plan = context
        .profile
        .implementation
        .plan(input, &package_write.resolved)?;
    add_stale_implementation_removals(context, &previous_state, &mut plan);
    if !input.options.allow_existing_merge {
        let native_changes = unowned_native_changes(context, home, &previous_state, &plan)?;
        if !native_changes.is_empty() {
            return Err(GhError::config(format!(
                "existing {} configuration requires explicit merge approval: {}; run `blue apply` interactively or enable mode.allow_noninteractive_merge",
                context.definition.metadata.key,
                native_changes.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join(", ")
            )));
        }
    }
    let mut report = plan.report();
    if let Some(warning) = &context.unverified_warning {
        report.warnings.push(warning.clone());
    }
    report.files.extend(package_write.files.iter().cloned());
    report.package_errors.extend(package_write.errors.clone());
    if report.package_errors.is_empty() {
        plan.writes.push(compatibility_state_write(
            home,
            context,
            &report,
            &plan.owned_paths,
        )?);
    }
    validate_plan(context, home, &plan)?;
    let mut transaction = FileTransaction::begin(home, &plan)?;
    transaction.apply(&plan)?;
    commit_packages(package_write)?;
    transaction.commit()?;
    report.warnings.extend(transaction.take_warnings());
    Ok(report)
}

/// Complete package preflight for a harness without changing active package
/// or compatibility state. Revision coordinators call this for every harness
/// before beginning the first configuration commit.
pub fn preflight_packages_with_fetcher(
    context: &HarnessContext,
    policy: &HarnessPolicy,
    packages: &[ManagedPackage],
    fetcher: &dyn PackageFetcher,
) -> Result<(), GhError> {
    if packages.is_empty() {
        return Ok(());
    }
    let home = paths::home_dir()?;
    let _lock = ReconcileLock::acquire(&home, context.harness)?;
    packages::preflight_with_fetcher(context, packages, policy, fetcher)
}

pub fn preflight_packages(
    context: &HarnessContext,
    policy: &HarnessPolicy,
    packages: &[ManagedPackage],
) -> Result<(), GhError> {
    preflight_packages_with_fetcher(context, policy, packages, &packages::DirectPackageFetcher)
}

/// Remove every path owned exclusively by Blue while leaving native/user
/// configuration untouched. Tenant transitions use this to ensure an inactive
/// deployment cannot keep supplying overlays, hooks, or gateway credentials to
/// agents launched outside Blue.
pub fn remove_all_managed_configuration() -> Result<(), GhError> {
    let home = paths::home_dir()?;
    remove_all_managed_configuration_at(&home)
}

fn remove_all_managed_configuration_at(home: &std::path::Path) -> Result<(), GhError> {
    let mut locks = Vec::new();
    let mut owned = Vec::new();
    for harness in Harness::ALL {
        locks.push(ReconcileLock::acquire(home, harness)?);
        owned.extend(load_compatibility_state_at(home, harness)?.owned_paths);
    }
    owned.sort();
    owned.dedup();
    let mut roots = Vec::<PathBuf>::new();
    for path in owned {
        validate_home_path(home, &path, true)?;
        if roots.iter().any(|root| path.starts_with(root)) {
            continue;
        }
        roots.retain(|root| !root.starts_with(&path));
        roots.push(path);
    }
    roots.sort();
    let plan = adapters::ReconcilePlan {
        remove_paths: roots.clone(),
        owned_paths: roots,
        ..Default::default()
    };
    let mut transaction = FileTransaction::begin(home, &plan)?;
    transaction.apply(&plan)?;
    transaction.commit()?;
    drop(locks);
    Ok(())
}

/// Validate each component without following links beneath the canonical home.
pub(crate) fn validate_home_path(
    home: &std::path::Path,
    path: &std::path::Path,
    allow_final_symlink: bool,
) -> Result<(), GhError> {
    use std::path::Component;
    let mut authorities = vec![home.to_path_buf()];
    if paths::home_dir().is_ok_and(|current| current == home) {
        if let Ok(client) = paths::ClientPaths::resolve() {
            authorities.extend([client.config, client.data]);
        }
    }
    authorities.sort_by_key(|root| std::cmp::Reverse(root.components().count()));
    let (authority, relative) = authorities
        .iter()
        .find_map(|root| relative_under(path, root).map(|relative| (root, relative)))
        .ok_or_else(|| {
            GhError::config(format!(
                "path escaped declared client roots: {}",
                path.display()
            ))
        })?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(GhError::config(format!(
            "invalid managed path: {}",
            path.display()
        )));
    }
    let mut current = authority.to_path_buf();
    let components = relative.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if is_link_or_reparse(&metadata) => {
                if !(allow_final_symlink && index + 1 == components.len()) {
                    return Err(GhError::config(format!(
                        "managed path traverses symlink: {}",
                        current.display()
                    )));
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(GhError::Io {
                    path: current,
                    source,
                })
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn relative_under(path: &std::path::Path, root: &std::path::Path) -> Option<PathBuf> {
    let path_components = path.components().collect::<Vec<_>>();
    let root_components = root.components().collect::<Vec<_>>();
    if root_components.len() > path_components.len()
        || !root_components
            .iter()
            .zip(&path_components)
            .all(|(left, right)| {
                left.as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
            })
    {
        return None;
    }
    Some(path_components[root_components.len()..].iter().collect())
}

#[cfg(not(windows))]
fn relative_under(path: &std::path::Path, root: &std::path::Path) -> Option<PathBuf> {
    path.strip_prefix(root)
        .ok()
        .map(std::path::Path::to_path_buf)
}

fn is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    false
}

fn validate_plan(
    context: &HarnessContext,
    home: &std::path::Path,
    plan: &adapters::ReconcilePlan,
) -> Result<(), GhError> {
    let declared = context.profile.implementation.paths(home);
    let previous = load_definition_state_at(home, context.definition)?;
    let state_path = definition_state_path(home, context.definition);
    let owns = |path: &std::path::Path| {
        declared
            .owned_outputs
            .iter()
            .any(|root| path.starts_with(root))
    };
    let authenticated = |path: &std::path::Path| {
        previous
            .owned_paths
            .iter()
            .any(|root| path.starts_with(root))
    };
    let authorized = |path: &std::path::Path| {
        owns(path)
            || declared
                .native_migrations
                .iter()
                .any(|target| target == path)
            || path == state_path
            || authenticated(path)
    };
    for path in plan.writes.iter().map(|write| &write.path) {
        validate_home_path(home, path, false)?;
        if !authorized(path) {
            return Err(GhError::config(format!(
                "implementation planned unauthorized write: {}",
                path.display()
            )));
        }
    }
    for path in &plan.remove_paths {
        validate_home_path(home, path, true)?;
        if !authorized(path) {
            return Err(GhError::config(format!(
                "implementation planned unauthorized removal: {}",
                path.display()
            )));
        }
        if std::fs::symlink_metadata(path).is_ok_and(|metadata| is_link_or_reparse(&metadata))
            && !declared.owned_outputs.contains(path)
            && !declared.native_migrations.contains(path)
            && !previous.owned_paths.contains(path)
            && !previous.files.contains(path)
        {
            return Err(GhError::config(
                "symlink removal requires exact declared or authenticated ownership",
            ));
        }
    }
    for path in &plan.owned_paths {
        validate_home_path(home, path, false)?;
        if !owns(path) {
            return Err(GhError::config(format!(
                "implementation claimed unauthorized ownership: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

struct ReconcileLock {
    file: Option<std::fs::File>,
    harness: Harness,
}
thread_local! {
    static HELD_RECONCILE_LOCKS: std::cell::RefCell<std::collections::BTreeSet<String>> =
        const { std::cell::RefCell::new(std::collections::BTreeSet::new()) };
}
impl ReconcileLock {
    fn acquire(home: &std::path::Path, harness: Harness) -> Result<Self, GhError> {
        let nested = HELD_RECONCILE_LOCKS.with(|locks| locks.borrow().contains(harness.key()));
        if nested {
            return Ok(Self {
                file: None,
                harness,
            });
        }
        let path = managed_data_dir(home)
            .join("locks")
            .join(format!("{}.lock", harness.key()));
        validate_home_path(home, &path, false)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| GhError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| GhError::Io {
                path: path.clone(),
                source,
            })?;
        lock_file(&file, &format!("{} reconciliation", harness.key()))?;
        HELD_RECONCILE_LOCKS.with(|locks| {
            locks.borrow_mut().insert(harness.key().to_owned());
        });
        Ok(Self {
            file: Some(file),
            harness,
        })
    }
}

#[cfg(unix)]
fn try_lock_file(file: &std::fs::File) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: flock only observes the valid descriptor owned by `file`.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(true)
    } else {
        let error = std::io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN)
        {
            Ok(false)
        } else {
            Err(error)
        }
    }
}

#[cfg(windows)]
fn try_lock_file(file: &std::fs::File) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;
    let mut overlapped = unsafe { std::mem::zeroed::<OVERLAPPED>() };
    // SAFETY: the handle is valid and overlapped points to writable storage for
    // this synchronous, fail-immediately operation.
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if result != 0 {
        Ok(true)
    } else {
        let error = std::io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(code) if code == 33 || code == 158) {
            Ok(false)
        } else {
            Err(error)
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn try_lock_file(_file: &std::fs::File) -> std::io::Result<bool> {
    Ok(true)
}

pub(crate) fn lock_file(file: &std::fs::File, label: &str) -> Result<(), GhError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let acquired = try_lock_file(file)
            .map_err(|error| GhError::config(format!("locking {label} lock failed: {error}")))?;
        if acquired {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(GhError::config(format!(
                "timed out waiting for {label} lock"
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(unix)]
pub(crate) fn unlock_file(file: &std::fs::File) {
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor remains valid through this call.
    let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}

#[cfg(windows)]
pub(crate) fn unlock_file(file: &std::fs::File) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;
    let mut overlapped = unsafe { std::mem::zeroed::<OVERLAPPED>() };
    // SAFETY: the handle and byte range match the successful lock operation.
    let _ = unsafe { UnlockFileEx(file.as_raw_handle(), 0, 1, 0, &mut overlapped) };
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn unlock_file(_file: &std::fs::File) {}

impl Drop for ReconcileLock {
    fn drop(&mut self) {
        let Some(file) = self.file.as_ref() else {
            return;
        };
        unlock_file(file);
        HELD_RECONCILE_LOCKS.with(|locks| {
            locks.borrow_mut().remove(self.harness.key());
        });
    }
}

/// Revision-wide rollback guard used by the daemon when several harnesses
/// must converge together. Individual harness commits remain atomic; this
/// outer snapshot restores earlier commits if a later required harness fails.
pub struct RevisionTransaction {
    inner: FileTransaction,
    _locks: Vec<ReconcileLock>,
}

impl RevisionTransaction {
    pub fn commit(&mut self) -> Result<(), GhError> {
        self.inner.commit()
    }
}

/// Revision-wide reconciliation locks acquired before package preflight.
///
/// This guard deliberately does not create a filesystem transaction. Dropping
/// it only releases the harness locks, so a failed preflight cannot restore an
/// earlier snapshot over edits made while preflight was running.
pub struct RevisionLocks {
    home: PathBuf,
    contexts: Vec<HarnessContext>,
    locks: Vec<ReconcileLock>,
}

impl RevisionLocks {
    /// Begin the durable rollback transaction while retaining every revision
    /// lock. Compatibility state is re-read here so targets reflect the state
    /// immediately before active reconciliation begins.
    pub fn begin_transaction(self) -> Result<RevisionTransaction, GhError> {
        let Self {
            home,
            contexts,
            locks,
        } = self;
        let mut affected = vec![
            managed_data_dir(&home).join("package-state.json"),
            managed_data_dir(&home).join("package-state"),
        ];
        let shared_state_paths = affected.len();
        for context in &contexts {
            let mut paths = context
                .profile
                .implementation
                .paths(&home)
                .transaction_targets();
            paths.push(definition_state_path(&home, context.definition));
            for path in &paths {
                if affected
                    .iter()
                    .skip(shared_state_paths)
                    .any(|existing| path.starts_with(existing) || existing.starts_with(path))
                {
                    return Err(GhError::config(format!(
                        "cross-harness reconciliation path collision at {}",
                        path.display()
                    )));
                }
            }
            affected.extend(paths);
            let previous = load_definition_state_at(&home, context.definition)?;
            affected.extend(previous.files);
            affected.extend(previous.owned_paths);
        }
        Ok(RevisionTransaction {
            inner: FileTransaction::begin_paths(&home, affected)?,
            _locks: locks,
        })
    }
}

pub fn acquire_revision_locks(contexts: &[HarnessContext]) -> Result<RevisionLocks, GhError> {
    let home = paths::home_dir()?;
    acquire_revision_locks_at(&home, contexts)
}

fn acquire_revision_locks_at(
    home: &std::path::Path,
    contexts: &[HarnessContext],
) -> Result<RevisionLocks, GhError> {
    let mut harnesses = contexts
        .iter()
        .map(|context| context.harness)
        .collect::<Vec<_>>();
    harnesses.sort_by_key(|harness| harness.key());
    harnesses.dedup();
    let locks = harnesses
        .into_iter()
        .map(|harness| ReconcileLock::acquire(home, harness))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RevisionLocks {
        home: home.to_path_buf(),
        contexts: contexts.to_vec(),
        locks,
    })
}
#[derive(Debug, Default, Serialize, Deserialize)]
struct CompatibilityState {
    #[serde(default = "compatibility_state_schema_version")]
    schema_version: u32,
    profile_id: String,
    version: String,
    files: Vec<PathBuf>,
    #[serde(default)]
    owned_paths: Vec<PathBuf>,
}
const fn compatibility_state_schema_version() -> u32 {
    4
}

#[cfg(test)]
fn compatibility_state_path(home: &std::path::Path, harness: Harness) -> PathBuf {
    definition_state_path(home, adapters::definition(harness))
}

/// Assert that a rendered path — an env-var value, say — names `expected`.
///
/// Production builds these by joining components, so on Windows they render
/// with `\` while a test literal like `".config/blue/runtime/kimi"` keeps `/`.
/// The two name the same file; only the strings differ. Comparing as `Path`
/// compares components and so is separator-agnostic.
#[cfg(test)]
fn assert_same_path(actual: Option<&str>, expected: &std::path::Path) {
    assert_eq!(
        actual.map(std::path::Path::new),
        Some(expected),
        "expected a path naming {}",
        expected.display()
    );
}
fn definition_state_path(
    home: &std::path::Path,
    definition: &adapters::HarnessDefinition,
) -> PathBuf {
    managed_runtime_dir(home)
        .join(definition.metadata.key)
        .join("compatibility-state.json")
}

fn load_compatibility_state(harness: Harness) -> Result<CompatibilityState, GhError> {
    let home = paths::home_dir()?;
    load_compatibility_state_at(&home, harness)
}

fn load_compatibility_state_at(
    home: &std::path::Path,
    harness: Harness,
) -> Result<CompatibilityState, GhError> {
    load_definition_state_at(home, adapters::definition(harness))
}

fn load_definition_state_at(
    home: &std::path::Path,
    definition: &'static adapters::HarnessDefinition,
) -> Result<CompatibilityState, GhError> {
    let harness = definition.metadata.key;
    let state_path = definition_state_path(home, definition);
    validate_home_path(home, &state_path, false)?;
    match std::fs::read(&state_path) {
        Ok(bytes) => {
            let state: CompatibilityState = serde_json::from_slice(&bytes)
                .map_err(|error| GhError::Serde(error.to_string()))?;
            if state.schema_version > compatibility_state_schema_version() {
                return Err(GhError::config(format!(
                    "unsupported compatibility state schema {} (client supports {})",
                    state.schema_version,
                    compatibility_state_schema_version()
                )));
            }
            let registration = definition.profile(&state.profile_id).ok_or_else(|| {
                GhError::config(format!(
                    "unknown persisted compatibility profile `{}` for {harness}",
                    state.profile_id
                ))
            })?;
            semver::Version::parse(&state.version).map_err(|error| {
                GhError::config(format!("invalid persisted harness version: {error}"))
            })?;
            let declared = registration.implementation.paths(home);
            for owned in &state.owned_paths {
                validate_home_path(home, owned, true)?;
                if !declared
                    .owned_outputs
                    .iter()
                    .any(|root| owned.starts_with(root))
                {
                    return Err(GhError::config(format!(
                        "persisted ownership is outside profile outputs: {}",
                        owned.display()
                    )));
                }
            }
            for file in &state.files {
                validate_home_path(home, file, true)?;
            }
            Ok(state)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(CompatibilityState::default())
        }
        Err(source) => Err(GhError::Io {
            path: state_path,
            source,
        }),
    }
}

fn add_stale_implementation_removals(
    context: &HarnessContext,
    previous: &CompatibilityState,
    plan: &mut adapters::ReconcilePlan,
) {
    if previous.profile_id.is_empty() || previous.profile_id == context.profile.id {
        return;
    }
    let current = plan.files.iter().collect::<std::collections::BTreeSet<_>>();
    plan.remove_paths.extend(
        previous
            .files
            .iter()
            .filter(|path| {
                !current.contains(path)
                    && previous
                        .owned_paths
                        .iter()
                        .any(|owned| path.starts_with(owned))
            })
            .cloned(),
    );
    plan.remove_paths.extend(
        previous
            .owned_paths
            .iter()
            .filter(|old| {
                !plan
                    .owned_paths
                    .iter()
                    .any(|current| old.starts_with(current) || current.starts_with(old))
            })
            .cloned(),
    );
    plan.remove_paths.sort();
    plan.remove_paths.dedup();
    plan.owned_paths.sort();
    plan.owned_paths.dedup();
}

fn unowned_native_changes(
    context: &HarnessContext,
    home: &std::path::Path,
    previous: &CompatibilityState,
    plan: &adapters::ReconcilePlan,
) -> Result<Vec<PathBuf>, GhError> {
    let targets = context.profile.implementation.paths(home).native_migrations;
    let mut changed = Vec::new();
    for target in targets {
        if previous
            .owned_paths
            .iter()
            .any(|owned| target.starts_with(owned))
        {
            continue;
        }
        let write_changes = plan
            .writes
            .iter()
            .find(|write| write.path == target)
            .is_some_and(|write| {
                std::fs::read(&target).map_or(true, |current| current != write.body)
            });
        let removal_changes = target.exists() && plan.remove_paths.contains(&target);
        if write_changes || removal_changes {
            changed.push(target);
        }
    }
    Ok(changed)
}

fn compatibility_state_write(
    home: &std::path::Path,
    context: &HarnessContext,
    report: &HarnessWrite,
    owned_paths: &[PathBuf],
) -> Result<adapters::PlannedFile, GhError> {
    let state_path = definition_state_path(home, context.definition);
    let current = report
        .files
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let state = CompatibilityState {
        schema_version: compatibility_state_schema_version(),
        profile_id: context.profile.id.to_owned(),
        version: context.version.to_string(),
        files: current.into_iter().collect(),
        owned_paths: owned_paths.to_vec(),
    };
    Ok(adapters::PlannedFile {
        path: state_path,
        body: serde_json::to_vec_pretty(&state)
            .map_err(|error| GhError::Serde(error.to_string()))?,
        mode: Some(0o600),
    })
}

pub use packages::{
    statuses as package_statuses, teardown_inactive as teardown_inactive_packages, PackageStatus,
};
/// Existing user configuration files that a write for this harness may
/// modify. The CLI uses this for an interactive preflight before manual apply.
pub fn existing_config_files(
    harness: Harness,
    policy: &HarnessPolicy,
    opts: WriteOptions,
) -> Result<Vec<PathBuf>, GhError> {
    let home = paths::home_dir()?;
    let _ = opts;
    let previous = load_compatibility_state(harness)?;
    let implementation = adapters::detected_context(harness, policy)?
        .profile
        .implementation;
    Ok(implementation
        .paths(&home)
        .native_migrations
        .into_iter()
        .filter(|path| path.exists())
        .filter(|path| {
            !previous
                .owned_paths
                .iter()
                .any(|owned| path.starts_with(owned))
        })
        .filter(|path| implementation.native_migration_needs_review(path, policy))
        .collect())
}

#[cfg(test)]
mod transaction_tests {
    use super::*;
    use crate::adapters::HarnessImplementation;

    /// A per-test scratch directory. The harness names each thread after its
    /// full test path, which contains `:` and is long enough to push nested
    /// transaction files past `MAX_PATH`, so hash it into a short component.
    fn test_directory(prefix: &str) -> std::path::PathBuf {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::thread::current()
            .name()
            .unwrap_or("test")
            .hash(&mut hasher);
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{:x}",
            std::process::id(),
            hasher.finish()
        ))
    }

    #[test]
    fn reconcile_lock_probe_child_helper() {
        let Some(path) = std::env::var_os("BLUE_RECONCILE_LOCK_TEST_PATH") else {
            return;
        };
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        assert!(!try_lock_file(&file).unwrap());
    }

    #[test]
    fn reconcile_lock_preserves_stable_path_and_cross_process_exclusion() {
        let home = test_directory("blue-reconcile-lock");
        std::fs::create_dir_all(&home).unwrap();
        let _lock = ReconcileLock::acquire(&home, Harness::Codex).unwrap();
        let path = home.join(".config/blue/locks/codex.lock");
        assert!(path.is_file());
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "transaction_tests::reconcile_lock_probe_child_helper",
            ])
            .env("BLUE_RECONCILE_LOCK_TEST_PATH", &path)
            .status()
            .unwrap();
        assert!(status.success());
        drop(_lock);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn abrupt_transaction_child_helper() {
        let Some(home) = std::env::var_os("BLUE_TRANSACTION_TEST_HOME") else {
            return;
        };
        let home = PathBuf::from(home);
        let target = home.join("owned/config.txt");
        let added = home.join("fresh/nested/new.txt");
        let plan = adapters::ReconcilePlan {
            writes: vec![
                adapters::PlannedFile {
                    path: target.clone(),
                    body: b"new-complete-value".to_vec(),
                    mode: Some(0o600),
                },
                adapters::PlannedFile {
                    path: added,
                    body: b"new-added-value".to_vec(),
                    mode: Some(0o600),
                },
            ],
            owned_paths: vec![home.join("owned"), home.join("fresh")],
            ..Default::default()
        };
        if std::env::var_os("BLUE_TRANSACTION_TEST_NESTED").is_some() {
            let outer =
                FileTransaction::begin_paths(&home, vec![target.clone(), home.join("fresh")])
                    .unwrap();
            let mut inner = FileTransaction::begin(&home, &plan).unwrap();
            inner.apply(&plan).unwrap();
            inner.commit().unwrap();
            std::mem::forget((inner, outer));
        } else {
            let mut transaction = FileTransaction::begin(&home, &plan).unwrap();
            transaction.apply(&plan).unwrap();
            transaction.commit().unwrap();
            std::mem::forget(transaction);
        }
        panic!("configured transaction crash point was not reached");
    }

    #[test]
    fn next_transaction_recovers_every_abrupt_single_and_nested_phase() {
        let phases = [
            "journal_persisted",
            "staging_directory_created",
            "first_prepared_file",
            "first_target_replacement",
            "committed_journal_persisted",
        ];
        for nested in [false, true] {
            for phase in phases {
                let home = std::env::temp_dir().join(format!(
                    "blue-transaction-recovery-{}-{nested}-{phase}",
                    std::process::id(),
                ));
                let target = home.join("owned/config.txt");
                let added = home.join("fresh/nested/new.txt");
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                std::fs::write(&target, "old-complete-value").unwrap();
                let executable = std::env::current_exe().unwrap();
                let mut child = std::process::Command::new(executable);
                child
                    .args([
                        "--exact",
                        "transaction_tests::abrupt_transaction_child_helper",
                    ])
                    .env("BLUE_TRANSACTION_TEST_HOME", &home)
                    .env("BLUE_TRANSACTION_CRASH_AT", phase);
                if nested {
                    child
                        .env("BLUE_TRANSACTION_TEST_NESTED", "1")
                        .env("BLUE_TRANSACTION_CRASH_DEPTH", "2");
                }
                assert!(!child.status().unwrap().success(), "{nested} {phase}");

                {
                    let _recovery =
                        FileTransaction::begin_paths(&home, vec![target.clone()]).unwrap();
                    let committed = !nested && phase == "committed_journal_persisted";
                    assert_eq!(
                        std::fs::read_to_string(&target).unwrap(),
                        if committed {
                            "new-complete-value"
                        } else {
                            "old-complete-value"
                        },
                        "{nested} {phase}",
                    );
                    assert_eq!(added.exists(), committed, "{nested} {phase}");
                }
                let leaked = walk_paths(&home)
                    .into_iter()
                    .filter(|path| {
                        path.file_name().is_some_and(|name| {
                            let name = name.to_string_lossy();
                            name.contains(".blue-stage-")
                                || name.contains(".blue-write-")
                                || name.contains(".blue-remove-")
                        })
                    })
                    .collect::<Vec<_>>();
                assert!(
                    leaked.is_empty(),
                    "leaked paths after {nested} {phase}: {leaked:?}"
                );
                if !nested && phase == "committed_journal_persisted" {
                    assert_eq!(
                        std::fs::read_to_string(&target).unwrap(),
                        "new-complete-value"
                    );
                    assert_eq!(std::fs::read_to_string(&added).unwrap(), "new-added-value");
                } else {
                    assert_eq!(
                        std::fs::read_to_string(&target).unwrap(),
                        "old-complete-value"
                    );
                    assert!(!added.exists());
                    assert!(!home.join("fresh").exists());
                }
                let _ = std::fs::remove_dir_all(home);
            }
        }
    }

    fn walk_paths(root: &std::path::Path) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let Ok(entries) = std::fs::read_dir(root) else {
            return paths;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            paths.push(path.clone());
            if path.is_dir() && !path.is_symlink() {
                paths.extend(walk_paths(&path));
            }
        }
        paths
    }

    #[test]
    fn transaction_enforces_owner_only_planned_mode() {
        let home = std::env::temp_dir().join(format!("blue-mode-{}", std::process::id()));
        let target = home.join("managed/config.json");
        let mut plan = adapters::ReconcilePlan {
            writes: vec![adapters::PlannedFile {
                path: target.clone(),
                body: b"{}".to_vec(),
                mode: Some(0o644),
            }],
            ..Default::default()
        };
        let mut transaction = FileTransaction::begin(&home, &plan).unwrap();
        let error = transaction.apply(&plan).unwrap_err().to_string();
        assert!(error.contains("must use 0600"), "{error}");
        drop(transaction);

        plan.writes[0].mode = Some(0o600);
        let mut transaction = FileTransaction::begin(&home, &plan).unwrap();
        transaction.apply(&plan).unwrap();
        transaction.commit().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn committed_cleanup_failure_retains_journal_and_retries_on_recovery() {
        let home = test_directory("blue-cleanup-retry");
        let target = home.join("managed/config.json");
        let plan = adapters::ReconcilePlan {
            writes: vec![adapters::PlannedFile {
                path: target.clone(),
                body: b"new".to_vec(),
                mode: Some(0o600),
            }],
            ..Default::default()
        };
        {
            let mut transaction = FileTransaction::begin(&home, &plan).unwrap();
            transaction.apply(&plan).unwrap();
            // The write landed durably before cleanup ran, so a cleanup failure
            // is a warning on a successful commit, not a failed apply.
            assert!(transaction.commit_with_cleanup_fault().is_ok());
            assert_eq!(transaction.take_warnings().len(), 1);
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert_eq!(
            std::fs::read_dir(home.join(".blue-transactions"))
                .unwrap()
                .count(),
            1
        );

        {
            let _recovery = FileTransaction::begin_paths(&home, vec![target.clone()]).unwrap();
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert_eq!(
            std::fs::read_dir(home.join(".blue-transactions"))
                .unwrap()
                .count(),
            0
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn stray_entries_in_the_transaction_root_are_skipped_not_fatal() {
        let home = test_directory("blue-stray-entry");
        let transactions = home.join(".blue-transactions");
        std::fs::create_dir_all(&transactions).unwrap();
        // One Finder visit is enough to leave this behind.
        std::fs::write(transactions.join(".DS_Store"), "junk").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&home, transactions.join("link")).unwrap();

        let target = home.join("managed/config.json");
        let plan = adapters::ReconcilePlan {
            writes: vec![adapters::PlannedFile {
                path: target.clone(),
                body: b"new".to_vec(),
                mode: Some(0o600),
            }],
            ..Default::default()
        };
        {
            let mut transaction = FileTransaction::begin(&home, &plan).unwrap();
            transaction.apply(&plan).unwrap();
            transaction.commit().unwrap();
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        // The stray entries are skipped, never deleted or followed.
        assert!(transactions.join(".DS_Store").exists());
        let _ = std::fs::remove_dir_all(home);
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_failing_again_during_recovery_still_lets_a_transaction_begin() {
        use std::os::unix::fs::PermissionsExt as _;

        let home = test_directory("blue-cleanup-recovery");
        let managed = home.join("managed");
        std::fs::create_dir_all(&managed).unwrap();
        let target = managed.join("config.json");
        std::fs::write(&target, "old").unwrap();
        let plan = adapters::ReconcilePlan {
            writes: vec![adapters::PlannedFile {
                path: target.clone(),
                body: b"new".to_vec(),
                mode: Some(0o600),
            }],
            ..Default::default()
        };
        {
            let mut transaction = FileTransaction::begin(&home, &plan).unwrap();
            transaction.apply(&plan).unwrap();
            transaction.commit_with_cleanup_fault().unwrap();
        }

        // The previous content is still parked in a trash sibling; deny writes
        // to its directory so recovery cannot remove it either.
        std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o500)).unwrap();
        let recovery = FileTransaction::begin_paths(&home, vec![target.clone()]);
        std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            recovery.is_ok(),
            "a repeated cleanup failure must not wedge `begin`"
        );
        drop(recovery);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert_eq!(
            std::fs::read_dir(home.join(".blue-transactions"))
                .unwrap()
                .count(),
            1,
            "the journal is retained until cleanup succeeds"
        );

        {
            let _recovery = FileTransaction::begin_paths(&home, vec![target.clone()]).unwrap();
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert_eq!(
            std::fs::read_dir(home.join(".blue-transactions"))
                .unwrap()
                .count(),
            0
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn rollback_preserves_concurrent_content_in_transaction_created_directories() {
        let home = test_directory("blue-concurrent-directory");
        let target = home.join("fresh/nested/config.json");
        let concurrent = home.join("fresh/concurrent.txt");
        let plan = adapters::ReconcilePlan {
            writes: vec![adapters::PlannedFile {
                path: target.clone(),
                body: b"managed".to_vec(),
                mode: Some(0o600),
            }],
            ..Default::default()
        };
        {
            let mut transaction = FileTransaction::begin(&home, &plan).unwrap();
            transaction.apply(&plan).unwrap();
            std::fs::write(&concurrent, "unrelated").unwrap();
        }
        assert!(!target.exists());
        assert_eq!(std::fs::read_to_string(&concurrent).unwrap(), "unrelated");
        assert!(!home.join("fresh/nested").exists());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn uncommitted_transaction_restores_existing_and_removes_new_outputs() {
        let home = std::env::temp_dir().join(format!("blue-transaction-{}", std::process::id()));
        let runtime = home.join(".config/blue/runtime/kimi");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(runtime.join("config.toml"), "last-known-good").unwrap();
        {
            let plan = adapters::kimi::v0_0_0::IMPLEMENTATION
                .plan(
                    &adapters::ReconcileInput {
                        home: &home,
                        policy: &HarnessPolicy::default(),
                        gateway: None,
                        options: WriteOptions::default(),
                        interval: &adapters::kimi::IMPLEMENTATIONS[0].interval,
                    },
                    &adapters::ResolvedPackages::default(),
                )
                .unwrap();
            let _transaction = FileTransaction::begin(&home, &plan).unwrap();
            std::fs::write(runtime.join("config.toml"), "partial-commit").unwrap();
            std::fs::write(runtime.join("new.toml"), "new").unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(runtime.join("config.toml")).unwrap(),
            "last-known-good"
        );
        assert!(!runtime.join("new.toml").exists());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn codex_plan_is_pure_and_native_migration_rolls_back() {
        let home = test_directory("blue-codex-plan");
        let native = home.join(".codex/config.toml");
        std::fs::create_dir_all(native.parent().unwrap()).unwrap();
        let original = "model_provider = \"governed\"\nmodel = \"gpt-test\"\n";
        std::fs::write(&native, original).unwrap();
        let policy = HarnessPolicy {
            managed_config: gh_service::ManagedConfig {
                model: Some("gpt-test".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let implementation = &adapters::codex::v0_145_0::IMPLEMENTATION;
        let plan = implementation
            .plan(
                &adapters::ReconcileInput {
                    home: &home,
                    policy: &policy,
                    gateway: None,
                    options: WriteOptions::default(),
                    interval: &adapters::codex::IMPLEMENTATIONS[0].interval,
                },
                &adapters::ResolvedPackages::default(),
            )
            .unwrap();
        assert_eq!(std::fs::read_to_string(&native).unwrap(), original);
        assert!(plan.writes.iter().any(|write| write.path == native));
        {
            let mut transaction = FileTransaction::begin(&home, &plan).unwrap();
            transaction.apply(&plan).unwrap();
            assert_ne!(std::fs::read_to_string(&native).unwrap(), original);
            // Dropping without commit simulates a later state-write fault.
        }
        assert_eq!(std::fs::read_to_string(&native).unwrap(), original);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn opencode_plan_remaps_session_plugin_out_of_render_home() {
        let home = test_directory("blue-opencode-plan");
        let unmanaged = home.join(".config/blue/runtime/opencode/node_modules/vendor.js");
        std::fs::create_dir_all(unmanaged.parent().unwrap()).unwrap();
        std::fs::write(&unmanaged, "unmanaged").unwrap();
        let options = WriteOptions {
            session_upload_enabled: true,
            ..WriteOptions::default()
        };
        let plan = adapters::opencode::v0_0_0::IMPLEMENTATION
            .plan(
                &adapters::ReconcileInput {
                    home: &home,
                    policy: &HarnessPolicy::default(),
                    gateway: None,
                    options,
                    interval: &adapters::opencode::IMPLEMENTATIONS[0].interval,
                },
                &adapters::ResolvedPackages::default(),
            )
            .unwrap();
        let plugin = plan
            .writes
            .iter()
            .find(|write| {
                write
                    .path
                    .ends_with("runtime/opencode/plugins/blue-session-upload.js")
            })
            .unwrap();
        assert!(plugin.path.starts_with(&home));
        assert!(!plugin.path.to_string_lossy().contains("blue-render-"));
        assert!(plan
            .writes
            .iter()
            .all(|write| !write.path.starts_with(unmanaged.parent().unwrap())));
        assert!(plan
            .remove_paths
            .iter()
            .all(|path| !path.starts_with(unmanaged.parent().unwrap())));
        assert_eq!(std::fs::read_to_string(&unmanaged).unwrap(), "unmanaged");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn current_codex_launch_spec_is_read_only() {
        let home = test_directory("blue-codex-launch-spec");
        let overlay = home.join(".codex/blue.config.toml");
        std::fs::create_dir_all(overlay.parent().unwrap()).unwrap();
        std::fs::write(&overlay, "[profiles.blue]\n").unwrap();
        let before = std::fs::read(&overlay).unwrap();
        let policy = HarnessPolicy::default();
        let version = semver::Version::new(0, 149, 1);
        let context = resolve_compatibility(
            Harness::Codex,
            Some(&version),
            Some("codex-cli 0.149.1"),
            &policy,
        )
        .unwrap();

        let spec =
            resolve_launch_spec_at(&home, &context, &policy, &[], None, WriteOptions::default())
                .unwrap();

        assert_eq!(
            spec.launch_args,
            vec![
                "--profile",
                "blue",
                "--config",
                "check_for_update_on_startup=false"
            ]
        );
        assert_eq!(std::fs::read(&overlay).unwrap(), before);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn current_kimi_launch_spec_sets_the_managed_home() {
        let home = test_directory("blue-kimi-launch-spec");
        let runtime = home.join(".config/blue/runtime/kimi");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(
            runtime.join("config.toml"),
            "default_model = \"governed\"\n",
        )
        .unwrap();
        let policy = HarnessPolicy::default();
        let version = semver::Version::new(0, 39, 1);
        let context = resolve_compatibility(
            Harness::Kimi,
            Some(&version),
            Some("kimi version 0.39.1"),
            &policy,
        )
        .unwrap();

        let spec =
            resolve_launch_spec_at(&home, &context, &policy, &[], None, WriteOptions::default())
                .unwrap();

        assert_same_path(spec.env.get("KIMI_CODE_HOME").map(String::as_str), &runtime);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn every_file_commit_fault_restores_the_complete_snapshot() {
        for fail_after in 1..=3 {
            let home = std::env::temp_dir()
                .join(format!("blue-fault-{}-{fail_after}", std::process::id()));
            let removed = home.join("owned/obsolete.txt");
            let existing = home.join("owned/config.txt");
            let added = home.join("owned/new.txt");
            std::fs::create_dir_all(removed.parent().unwrap()).unwrap();
            std::fs::write(&removed, "old-obsolete").unwrap();
            std::fs::write(&existing, "old-config").unwrap();
            let plan = adapters::ReconcilePlan {
                writes: vec![
                    adapters::PlannedFile {
                        path: existing.clone(),
                        body: b"new-config".to_vec(),
                        mode: Some(0o600),
                    },
                    adapters::PlannedFile {
                        path: added.clone(),
                        body: b"new-file".to_vec(),
                        mode: Some(0o600),
                    },
                ],
                remove_paths: vec![removed.clone()],
                owned_paths: vec![home.join("owned")],
                ..Default::default()
            };
            {
                let mut transaction = FileTransaction::begin(&home, &plan).unwrap();
                assert!(transaction
                    .apply_with_fault(&plan, Some(fail_after))
                    .is_err());
            }
            assert_eq!(std::fs::read_to_string(&removed).unwrap(), "old-obsolete");
            assert_eq!(std::fs::read_to_string(&existing).unwrap(), "old-config");
            assert!(!added.exists());
            let _ = std::fs::remove_dir_all(home);
        }
    }

    #[test]
    fn profile_transition_restores_stale_files_when_state_write_fails() {
        let home = std::env::temp_dir().join(format!(
            "blue-profile-transition-fault-{}",
            std::process::id()
        ));
        let runtime = home.join(".config/blue/runtime/codex");
        let stale = runtime.join("v1-only.toml");
        let current = runtime.join("config.toml");
        let state = runtime.join("compatibility-state.json");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(&stale, "stale-v1").unwrap();
        std::fs::write(&current, "old-current").unwrap();
        std::fs::write(&state, "old-state").unwrap();
        let plan = adapters::ReconcilePlan {
            writes: vec![
                adapters::PlannedFile {
                    path: current.clone(),
                    body: b"new-current".to_vec(),
                    mode: Some(0o600),
                },
                adapters::PlannedFile {
                    path: state.clone(),
                    body: b"new-state".to_vec(),
                    mode: Some(0o600),
                },
            ],
            remove_paths: vec![stale.clone()],
            owned_paths: vec![runtime],
            ..Default::default()
        };
        {
            let mut transaction = FileTransaction::begin(&home, &plan).unwrap();
            assert!(transaction.apply_with_fault(&plan, Some(3)).is_err());
        }
        assert_eq!(std::fs::read_to_string(stale).unwrap(), "stale-v1");
        assert_eq!(std::fs::read_to_string(current).unwrap(), "old-current");
        assert_eq!(std::fs::read_to_string(state).unwrap(), "old-state");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn unversioned_compatibility_state_migrates_to_v4_in_memory() {
        let state: CompatibilityState = serde_json::from_value(serde_json::json!({
            "profile_id": "codex-v1",
            "version": "0.149.1",
            "files": []
        }))
        .unwrap();
        assert_eq!(state.schema_version, 4);
        assert!(state.owned_paths.is_empty());
    }

    #[test]
    fn managed_cleanup_removes_owned_overlays_but_preserves_native_config() {
        let home = std::env::temp_dir().join(format!(
            "blue-managed-cleanup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let native = home.join(".codex/config.toml");
        let overlay = home.join(".codex/blue.config.toml");
        let runtime = home.join(".config/blue/runtime/claude/settings.json");
        std::fs::create_dir_all(native.parent().unwrap()).unwrap();
        std::fs::create_dir_all(runtime.parent().unwrap()).unwrap();
        std::fs::write(&native, "personal = true\n").unwrap();
        std::fs::write(&overlay, "managed = true\n").unwrap();
        std::fs::write(&runtime, "{}").unwrap();

        // Without authenticated state cleanup must not guess ownership.
        remove_all_managed_configuration_at(&home).unwrap();
        assert!(overlay.exists());
        assert!(runtime.exists());
        for (harness, file) in [(Harness::Codex, &overlay), (Harness::Claude, &runtime)] {
            let state_path = compatibility_state_path(&home, harness);
            std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
            let registration = &adapters::definition(harness).implementations[0];
            let state = CompatibilityState {
                schema_version: 4,
                profile_id: registration.interval.profile.into(),
                version: "1.0.0".into(),
                files: vec![file.clone()],
                owned_paths: registration.implementation.paths(&home).owned_outputs,
            };
            std::fs::write(state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        }
        remove_all_managed_configuration_at(&home).unwrap();

        assert_eq!(
            std::fs::read_to_string(native).unwrap(),
            "personal = true\n"
        );
        assert!(!overlay.exists());
        assert!(!runtime.exists());
        let _ = std::fs::remove_dir_all(home);
    }
}

#[cfg(test)]
mod contract_tests;
