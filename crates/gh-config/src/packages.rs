//! Digest-pinned extension package reconciliation.
//!
//! Package archives are immutable inputs. They are extracted beneath the
//! metaharness root and activated only through launch arguments/environment or
//! generated runtime overlays. Native user extension directories are never an
//! ownership target.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use gh_common::{paths, write_atomic, GhError, Harness};
use gh_service::{HarnessPolicy, ManagedPackage, PackageAdapter};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::HarnessContext;

#[derive(Debug, Default)]
pub(crate) struct PreparedPackages {
    pub resolved: crate::adapters::ResolvedPackages,
    pub files: Vec<PathBuf>,
    pub launch_args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub plugin_modules: Vec<PathBuf>,
    pub skills_dirs: Vec<PathBuf>,
    pub agents_dirs: Vec<PathBuf>,
    pub hooks_files: Vec<PathBuf>,
    pub helpers: BTreeMap<String, PathBuf>,
    pub errors: Vec<String>,
    pending_state: Option<Box<PackageState>>,
    pending_inactive: BTreeMap<String, InstalledPackage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageStatus {
    pub id: String,
    pub version: String,
    pub sha256: String,
    pub harness: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PackageState {
    #[serde(default = "package_state_schema_version")]
    schema_version: u32,
    #[serde(default)]
    harnesses: BTreeMap<String, BTreeMap<String, InstalledPackage>>,
    #[serde(default)]
    failures: BTreeMap<String, BTreeMap<String, PackageStatus>>,
    #[serde(default)]
    quarantine: Vec<PackageStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HarnessPackageState {
    #[serde(default = "package_state_schema_version")]
    schema_version: u32,
    #[serde(default)]
    packages: BTreeMap<String, InstalledPackage>,
    #[serde(default)]
    failures: BTreeMap<String, PackageStatus>,
    #[serde(default)]
    quarantine: Vec<PackageStatus>,
}

impl Default for PackageState {
    fn default() -> Self {
        Self {
            schema_version: package_state_schema_version(),
            harnesses: BTreeMap::new(),
            failures: BTreeMap::new(),
            quarantine: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstalledPackage {
    version: String,
    sha256: String,
    root: PathBuf,
    tree_sha256: String,
    #[serde(default)]
    selected_adapter: PackageAdapter,
    #[serde(default = "zero_version")]
    introduced: semver::Version,
    #[serde(default)]
    before: Option<semver::Version>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InactivePackage {
    id: String,
    inactive_since: u64,
    record: InstalledPackage,
}

const INACTIVE_RETENTION_SECONDS: u64 = 7 * 24 * 60 * 60;

const fn package_state_schema_version() -> u32 {
    4
}
fn zero_version() -> semver::Version {
    semver::Version::new(0, 0, 0)
}

pub trait PackageFetcher {
    fn fetch(&self, source_ref: &str, artifact_id: Option<&str>) -> Result<Vec<u8>, GhError>;
}

pub struct DirectPackageFetcher;

impl PackageFetcher for DirectPackageFetcher {
    fn fetch(&self, source_ref: &str, artifact_id: Option<&str>) -> Result<Vec<u8>, GhError> {
        if artifact_id.is_some() {
            return Err(GhError::service(
                "managed package artifact requires an authenticated control service",
            ));
        }
        fetch_bytes(source_ref)
    }
}

/// Resolve, fetch, verify, and inspect every enabled package for one harness
/// without changing its activation state. Immutable content may be populated
/// in the content-addressed store and is activated only by the later commit.
pub(crate) fn preflight_with_fetcher(
    context: &HarnessContext,
    packages: &[ManagedPackage],
    policy: &HarnessPolicy,
    fetcher: &dyn PackageFetcher,
) -> Result<(), GhError> {
    let base = package_base()?;
    let mut helpers = BTreeSet::new();
    for package in packages {
        let Some(adapter) = package.adapters.get(context.harness.key()) else {
            continue;
        };
        if policy
            .package_overrides
            .get(&package.id)
            .and_then(|item| item.enabled)
            == Some(false)
        {
            continue;
        }
        let (adapter, interval) = select_adapter(adapter, &context.version)?;
        context
            .profile
            .implementation
            .validate_components(&adapter)?;
        let (_, activation) = install_and_resolve(
            &base,
            context.harness,
            package,
            &adapter,
            &interval,
            context.profile.implementation,
            fetcher,
        )?;
        for helper in activation.helpers.keys() {
            if !helpers.insert(helper.clone()) {
                return Err(GhError::config(format!(
                    "helper command `{helper}` collides with another package"
                )));
            }
        }
    }
    Ok(())
}

pub fn reconcile_with_fetcher(
    context: &HarnessContext,
    packages: &[ManagedPackage],
    policy: &HarnessPolicy,
    fetcher: &dyn PackageFetcher,
) -> Result<PreparedPackages, GhError> {
    let harness = context.harness;
    let base = package_base()?;
    let mut state = load_state()?;
    let harness_key = harness.key().to_owned();
    let previous = state
        .harnesses
        .get(&harness_key)
        .cloned()
        .unwrap_or_default();
    let mut next = BTreeMap::new();
    let mut failures = BTreeMap::new();
    let mut activation = PreparedPackages::default();

    for package in packages {
        let Some(adapter) = package.adapters.get(harness.key()) else {
            continue;
        };
        let (adapter, interval) = match select_adapter(adapter, &context.version) {
            Ok(selected) => selected,
            Err(error) => {
                let message = error.to_string();
                activation.errors.push(format!("{}: {message}", package.id));
                failures.insert(
                    package.id.clone(),
                    PackageStatus {
                        id: package.id.clone(),
                        version: package.version.clone(),
                        sha256: package.sha256.clone(),
                        harness: harness_key.clone(),
                        state: "failed".into(),
                        error: Some(message),
                    },
                );
                retain_previous(
                    &previous,
                    &package.id,
                    harness,
                    package,
                    &context.version,
                    policy,
                    context.profile.implementation,
                    &mut next,
                    &mut activation,
                );
                continue;
            }
        };
        if let Err(error) = context.profile.implementation.validate_components(&adapter) {
            let message = error.to_string();
            activation.errors.push(format!("{}: {message}", package.id));
            failures.insert(
                package.id.clone(),
                PackageStatus {
                    id: package.id.clone(),
                    version: package.version.clone(),
                    sha256: package.sha256.clone(),
                    harness: harness_key.clone(),
                    state: "failed".into(),
                    error: Some(message),
                },
            );
            retain_previous(
                &previous,
                &package.id,
                harness,
                package,
                &context.version,
                policy,
                context.profile.implementation,
                &mut next,
                &mut activation,
            );
            continue;
        }
        let enabled = policy
            .package_overrides
            .get(&package.id)
            .and_then(|item| item.enabled)
            .unwrap_or(true);
        if !enabled {
            continue;
        }

        match install_and_resolve(
            &base,
            harness,
            package,
            &adapter,
            &interval,
            context.profile.implementation,
            fetcher,
        ) {
            Ok((record, resolved)) => {
                let mut resolved = resolved;
                add_settings_env(&mut resolved, package, policy);
                if let Some(name) = resolved
                    .helpers
                    .keys()
                    .find(|name| activation.helpers.contains_key(*name))
                {
                    let message = format!("helper command `{name}` collides with another package");
                    activation.errors.push(format!("{}: {message}", package.id));
                    failures.insert(
                        package.id.clone(),
                        PackageStatus {
                            id: package.id.clone(),
                            version: package.version.clone(),
                            sha256: package.sha256.clone(),
                            harness: harness_key.clone(),
                            state: "failed".into(),
                            error: Some(message),
                        },
                    );
                    retain_previous(
                        &previous,
                        &package.id,
                        harness,
                        package,
                        &context.version,
                        policy,
                        context.profile.implementation,
                        &mut next,
                        &mut activation,
                    );
                    continue;
                }
                activation.resolved.groups.extend(resolved.resolved.groups);
                activation.files.push(record.root.clone());
                activation.launch_args.extend(resolved.launch_args);
                activation.env.extend(resolved.env);
                activation.plugin_modules.extend(resolved.plugin_modules);
                activation.skills_dirs.extend(resolved.skills_dirs);
                activation.agents_dirs.extend(resolved.agents_dirs);
                activation.hooks_files.extend(resolved.hooks_files);
                activation.helpers.extend(resolved.helpers);
                next.insert(package.id.clone(), record);
            }
            Err(error) => {
                let message = error.to_string();
                activation.errors.push(format!("{}: {message}", package.id));
                failures.insert(
                    package.id.clone(),
                    PackageStatus {
                        id: package.id.clone(),
                        version: package.version.clone(),
                        sha256: package.sha256.clone(),
                        harness: harness_key.clone(),
                        state: "failed".into(),
                        error: Some(message),
                    },
                );
                retain_previous(
                    &previous,
                    &package.id,
                    harness,
                    package,
                    &context.version,
                    policy,
                    context.profile.implementation,
                    &mut next,
                    &mut activation,
                );
            }
        }
    }

    // A harness package set is one activation transaction. Any required
    // package failure restores the entire previous set; successfully staged
    // immutable content may remain on disk but is not referenced or active.
    if !activation.errors.is_empty() {
        let errors = std::mem::take(&mut activation.errors);
        activation = PreparedPackages::default();
        activation.errors = errors;
        next.clear();
        for package in packages {
            let enabled = policy
                .package_overrides
                .get(&package.id)
                .and_then(|item| item.enabled)
                .unwrap_or(true);
            if enabled && package.adapters.contains_key(harness.key()) {
                retain_previous(
                    &previous,
                    &package.id,
                    harness,
                    package,
                    &context.version,
                    policy,
                    context.profile.implementation,
                    &mut next,
                    &mut activation,
                );
            }
        }
    }

    activation.pending_inactive = previous
        .iter()
        .filter(|(id, record)| next.get(*id).is_none_or(|next| next.root != record.root))
        .map(|(id, record)| (id.clone(), record.clone()))
        .collect();
    state.harnesses.insert(harness_key.clone(), next);
    state.failures.insert(harness_key, failures);
    // Obsolete immutable content is intentionally retained until an explicit
    // inactive-harness teardown. This keeps rollback possible after package
    // state has been prepared but a later renderer/commit step fails.
    activation.pending_state = Some(Box::new(state));
    Ok(activation)
}

/// Resolve the already-activated package set for launch without downloading,
/// hashing, chmodding, or changing package state.
pub(crate) fn active_launch(
    context: &HarnessContext,
    packages: &[ManagedPackage],
    policy: &HarnessPolicy,
) -> Result<PreparedPackages, GhError> {
    if !packages.iter().any(|package| {
        package.adapters.contains_key(context.harness.key())
            && policy
                .package_overrides
                .get(&package.id)
                .and_then(|item| item.enabled)
                != Some(false)
    }) {
        return Ok(PreparedPackages::default());
    }
    let state = load_state()?;
    let installed = state
        .harnesses
        .get(context.harness.key())
        .cloned()
        .unwrap_or_default();
    let mut activation = PreparedPackages::default();
    for package in packages {
        if policy
            .package_overrides
            .get(&package.id)
            .and_then(|item| item.enabled)
            == Some(false)
            || !package.adapters.contains_key(context.harness.key())
        {
            continue;
        }
        let record = installed.get(&package.id).ok_or_else(|| {
            GhError::config(format!(
                "managed package `{}` is not active for {}",
                package.id, context.harness
            ))
        })?;
        let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        let expected_sha = if package.platform_sources.is_empty() {
            package.sha256.as_str()
        } else {
            package
                .platform_sources
                .get(&platform)
                .ok_or_else(|| {
                    GhError::config(format!(
                        "managed package `{}` has no archive for {platform}",
                        package.id
                    ))
                })?
                .sha256
                .as_str()
        };
        if record.version != package.version
            || !record.sha256.eq_ignore_ascii_case(expected_sha)
            || !record.root.is_dir()
            || context.version < record.introduced
            || record
                .before
                .as_ref()
                .is_some_and(|before| &context.version >= before)
        {
            return Err(GhError::config(format!(
                "managed package `{}` requires reconciliation for {}",
                package.id, context.harness
            )));
        }
        let mut resolved = resolve_adapter(
            context.harness,
            package,
            &record.selected_adapter,
            &record.root,
            context.profile.implementation,
            false,
        )?;
        add_settings_env(&mut resolved, package, policy);
        if let Some(name) = resolved
            .helpers
            .keys()
            .find(|name| activation.helpers.contains_key(*name))
        {
            return Err(GhError::config(format!(
                "helper command `{name}` collides with another package"
            )));
        }
        activation.resolved.groups.extend(resolved.resolved.groups);
        activation.files.push(record.root.clone());
        activation.launch_args.extend(resolved.launch_args);
        activation.env.extend(resolved.env);
        activation.helpers.extend(resolved.helpers);
    }
    Ok(activation)
}

pub(crate) fn commit_activation_state(
    harness: Harness,
    activation: &mut PreparedPackages,
) -> Result<(), GhError> {
    let state = activation
        .pending_state
        .take()
        .ok_or_else(|| GhError::config("package activation has no prepared state"))?;
    save_harness_state(&state, harness.key())?;
    mark_inactive_candidates(&package_base()?, &activation.pending_inactive)
}

pub(crate) fn select_adapter(
    adapter: &PackageAdapter,
    version: &semver::Version,
) -> Result<(PackageAdapter, gh_service::PackageAdapterInterval), GhError> {
    let availability = adapter.availability().map_err(GhError::config)?;
    if !availability.contains(version) {
        let before = availability
            .before
            .as_ref()
            .map(|value| format!(", <{value}"))
            .unwrap_or_default();
        return Err(GhError::config(format!(
            "package adapter requires harness version >={}{}; installed version is {version}",
            availability.introduced, before
        )));
    }
    let intervals = validate_adapter_intervals(adapter)?;
    let mut matching = Vec::new();
    for (variant, interval) in adapter.variants.iter().zip(intervals) {
        if interval.contains(version) {
            matching.push((variant, interval));
        }
    }
    if matching.len() > 1 {
        return Err(GhError::config(format!(
            "multiple package adapter variants match harness version {version}"
        )));
    }
    if let Some((selected, interval)) = matching.first() {
        return Ok((selected.as_adapter(), interval.clone()));
    }
    let has_default = adapter.plugin_dir.is_some()
        || adapter.skills_dir.is_some()
        || adapter.agents_dir.is_some()
        || adapter.hooks_file.is_some()
        || !adapter.plugins.is_empty()
        || !adapter.helpers.is_empty();
    if !adapter.variants.is_empty() && !has_default {
        return Err(GhError::config(format!(
            "no package adapter variant matches harness version {version}"
        )));
    }
    let mut fallback = adapter.clone();
    fallback.variants.clear();
    fallback.introduced = None;
    fallback.before = None;
    Ok((fallback, availability))
}

fn validate_adapter_intervals(
    adapter: &PackageAdapter,
) -> Result<Vec<gh_service::PackageAdapterInterval>, GhError> {
    let availability = adapter.availability().map_err(GhError::config)?;
    let intervals = adapter
        .variants
        .iter()
        .map(|variant| variant.interval().map_err(GhError::config))
        .collect::<Result<Vec<_>, _>>()?;
    for (index, interval) in intervals.iter().enumerate() {
        if interval.introduced < availability.introduced
            || match (&interval.before, &availability.before) {
                (Some(before), Some(available_before)) => before > available_before,
                (None, Some(_)) => true,
                _ => false,
            }
        {
            return Err(GhError::config(format!(
                "package adapter variant at {} escapes adapter availability",
                interval.introduced
            )));
        }
        if intervals[..index]
            .iter()
            .any(|existing| existing.overlaps(interval))
        {
            return Err(GhError::config(format!(
                "package adapter intervals overlap at {}",
                interval.introduced
            )));
        }
    }
    if intervals
        .windows(2)
        .any(|pair| pair[0].introduced >= pair[1].introduced)
    {
        return Err(GhError::config(
            "package adapter intervals must be ordered by introduced version",
        ));
    }
    let has_fallback = adapter.plugin_dir.is_some()
        || adapter.skills_dir.is_some()
        || adapter.agents_dir.is_some()
        || adapter.hooks_file.is_some()
        || !adapter.plugins.is_empty()
        || !adapter.helpers.is_empty();
    if !has_fallback && !intervals.is_empty() {
        let ordered = intervals.iter().collect::<Vec<_>>();
        if ordered[0].introduced != availability.introduced {
            return Err(GhError::config(
                "package adapter intervals have an initial gap and no fallback",
            ));
        }
        if ordered
            .windows(2)
            .any(|pair| pair[0].before.as_ref() != Some(&pair[1].introduced))
            || ordered.last().and_then(|interval| interval.before.as_ref())
                != availability.before.as_ref()
        {
            return Err(GhError::config(
                "package adapter intervals have a gap and no fallback",
            ));
        }
    }
    Ok(intervals)
}

#[allow(clippy::too_many_arguments)]
fn retain_previous(
    previous: &BTreeMap<String, InstalledPackage>,
    id: &str,
    harness: Harness,
    package: &ManagedPackage,
    version: &semver::Version,
    policy: &HarnessPolicy,
    implementation: &'static dyn crate::adapters::HarnessImplementation,
    next: &mut BTreeMap<String, InstalledPackage>,
    activation: &mut PreparedPackages,
) {
    let Some(record) = previous.get(id) else {
        return;
    };
    // Retention is unconditional; activation is deliberately conditional on
    // the exact adapter interval that produced the last-known-good record.
    next.insert(id.to_owned(), record.clone());
    let compatible = version >= &record.introduced
        && record.before.as_ref().is_none_or(|before| version < before);
    if !compatible {
        return;
    }
    let Ok(mut resolved) = resolve_adapter(
        harness,
        package,
        &record.selected_adapter,
        &record.root,
        implementation,
        true,
    ) else {
        return;
    };
    if resolved
        .helpers
        .keys()
        .any(|name| activation.helpers.contains_key(name))
    {
        return;
    }
    add_settings_env(&mut resolved, package, policy);
    activation.resolved.groups.extend(resolved.resolved.groups);
    activation.files.push(record.root.clone());
    activation.launch_args.extend(resolved.launch_args);
    activation.env.extend(resolved.env);
    activation.plugin_modules.extend(resolved.plugin_modules);
    activation.skills_dirs.extend(resolved.skills_dirs);
    activation.agents_dirs.extend(resolved.agents_dirs);
    activation.hooks_files.extend(resolved.hooks_files);
    activation.helpers.extend(resolved.helpers);
}

fn add_settings_env(
    activation: &mut PreparedPackages,
    package: &ManagedPackage,
    policy: &HarnessPolicy,
) {
    let mut settings = package.settings.clone();
    if let Some(override_settings) = policy.package_overrides.get(&package.id) {
        settings.extend(override_settings.settings.clone());
    }
    if !settings.is_empty() {
        if let Ok(value) = serde_json::to_string(&settings) {
            activation.env.insert(
                format!("HARNESS_PACKAGE_{}_SETTINGS", env_key(&package.id)),
                value,
            );
        }
    }
    if let Some(group) = activation
        .resolved
        .groups
        .iter_mut()
        .find(|group| group.id == package.id)
    {
        group.settings_environment = activation.env.clone();
    }
}

/// Remove activations belonging to harnesses no longer present in policy.
pub fn teardown_inactive(active_harnesses: &[String]) -> Result<Vec<PathBuf>, GhError> {
    let base = package_base()?;
    let mut state = load_state()?;
    let active = active_harnesses.iter().cloned().collect::<BTreeSet<_>>();
    let stale = state
        .harnesses
        .keys()
        .filter(|key| !active.contains(*key))
        .cloned()
        .collect::<Vec<_>>();
    let mut removed_records = BTreeMap::new();
    for key in stale {
        if let Some(records) = state.harnesses.remove(&key) {
            removed_records.extend(records);
        }
        state.failures.remove(&key);
    }
    let before = state.quarantine.len();
    garbage_collect(&base, &mut state, &removed_records)?;
    let quarantined = state.quarantine[before..]
        .iter()
        .filter_map(|status| status.error.as_deref().map(PathBuf::from))
        .collect();
    save_state(&state)?;
    Ok(quarantined)
}

pub fn statuses() -> Result<Vec<PackageStatus>, GhError> {
    let state = load_state()?;
    let mut out = Vec::new();
    for (harness, packages) in &state.harnesses {
        for (id, record) in packages {
            if let Some(failure) = state.failures.get(harness).and_then(|items| items.get(id)) {
                out.push(failure.clone());
                continue;
            }
            let current = tree_sha256(&record.root).ok();
            out.push(PackageStatus {
                id: id.clone(),
                version: record.version.clone(),
                sha256: record.sha256.clone(),
                harness: harness.clone(),
                state: if current.as_deref() == Some(record.tree_sha256.as_str()) {
                    "applied".into()
                } else {
                    "drifted".into()
                },
                error: None,
            });
        }
    }
    for (harness, failures) in &state.failures {
        for (id, failure) in failures {
            let active = state
                .harnesses
                .get(harness)
                .is_some_and(|items| items.contains_key(id));
            if !active {
                out.push(failure.clone());
            }
        }
    }
    out.extend(state.quarantine.into_iter().filter(|status| {
        status
            .error
            .as_deref()
            .is_some_and(|path| Path::new(path).exists())
    }));
    Ok(out)
}

fn package_base() -> Result<PathBuf, GhError> {
    Ok(paths::blue_config_dir()?.join("packages"))
}

fn legacy_state_path() -> Result<PathBuf, GhError> {
    Ok(paths::blue_config_dir()?.join("package-state.json"))
}

fn state_dir() -> Result<PathBuf, GhError> {
    Ok(paths::blue_config_dir()?.join("package-state"))
}

fn harness_state_path(harness: &str) -> Result<PathBuf, GhError> {
    Ok(state_dir()?.join(format!("{harness}.json")))
}

fn load_state() -> Result<PackageState, GhError> {
    let legacy_path = legacy_state_path()?;
    let mut aggregate = match std::fs::read(&legacy_path) {
        Ok(bytes) => {
            let state: PackageState = serde_json::from_slice(&bytes)
                .map_err(|error| GhError::Serde(error.to_string()))?;
            if state.schema_version > package_state_schema_version() {
                return Err(GhError::config(format!(
                    "unsupported package state schema {} (client supports {})",
                    state.schema_version,
                    package_state_schema_version()
                )));
            }
            state
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => PackageState::default(),
        Err(source) => {
            return Err(GhError::Io {
                path: legacy_path,
                source,
            })
        }
    };
    let dir = state_dir()?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(aggregate),
        Err(source) => return Err(GhError::Io { path: dir, source }),
    };
    for entry in entries {
        let entry = entry.map_err(|source| GhError::Io {
            path: dir.clone(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(harness) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let bytes = std::fs::read(&path).map_err(|source| GhError::Io {
            path: path.clone(),
            source,
        })?;
        let state: HarnessPackageState =
            serde_json::from_slice(&bytes).map_err(|error| GhError::Serde(error.to_string()))?;
        if state.schema_version > package_state_schema_version() {
            return Err(GhError::config(format!(
                "unsupported {harness} package state schema {} (client supports {})",
                state.schema_version,
                package_state_schema_version()
            )));
        }
        if harness == "_global" {
            aggregate.quarantine = state.quarantine;
            continue;
        }
        // A per-harness file is authoritative even when empty; this acts as a
        // migration tombstone over a retained legacy aggregate file.
        aggregate
            .harnesses
            .insert(harness.to_owned(), state.packages);
        aggregate
            .failures
            .insert(harness.to_owned(), state.failures);
    }
    Ok(aggregate)
}

fn save_harness_state(state: &PackageState, harness: &str) -> Result<(), GhError> {
    let state = HarnessPackageState {
        schema_version: package_state_schema_version(),
        packages: state.harnesses.get(harness).cloned().unwrap_or_default(),
        failures: state.failures.get(harness).cloned().unwrap_or_default(),
        quarantine: Vec::new(),
    };
    let bytes =
        serde_json::to_vec_pretty(&state).map_err(|error| GhError::Serde(error.to_string()))?;
    write_atomic(&harness_state_path(harness)?, bytes)
}

fn save_state(state: &PackageState) -> Result<(), GhError> {
    let mut harnesses = state.harnesses.keys().cloned().collect::<BTreeSet<_>>();
    harnesses.extend(state.failures.keys().cloned());
    if let Ok(entries) = std::fs::read_dir(state_dir()?) {
        harnesses.extend(entries.filter_map(Result::ok).filter_map(|entry| {
            entry
                .path()
                .file_stem()
                .and_then(|value| value.to_str())
                .map(str::to_owned)
        }));
    }
    for harness in harnesses {
        save_harness_state(state, &harness)?;
    }
    let global = HarnessPackageState {
        schema_version: package_state_schema_version(),
        packages: BTreeMap::new(),
        failures: BTreeMap::new(),
        quarantine: state.quarantine.clone(),
    };
    write_atomic(
        &harness_state_path("_global")?,
        serde_json::to_vec_pretty(&global).map_err(|error| GhError::Serde(error.to_string()))?,
    )?;
    Ok(())
}

fn install_and_resolve(
    base: &Path,
    harness: Harness,
    package: &ManagedPackage,
    adapter: &PackageAdapter,
    interval: &gh_service::PackageAdapterInterval,
    implementation: &'static dyn crate::adapters::HarnessImplementation,
    fetcher: &dyn PackageFetcher,
) -> Result<(InstalledPackage, PreparedPackages), GhError> {
    validate_id(&package.id)?;
    let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let (source_ref, artifact_id, sha256) = if package.platform_sources.is_empty() {
        (
            &package.source_ref,
            package.artifact_id.as_deref(),
            &package.sha256,
        )
    } else {
        let source = package.platform_sources.get(&platform).ok_or_else(|| {
            GhError::config(format!(
                "managed package `{}` has no archive for {platform}",
                package.id
            ))
        })?;
        (
            &source.source_ref,
            source.artifact_id.as_deref(),
            &source.sha256,
        )
    };
    validate_sha(&package.id, sha256)?;
    let digest = sha256.to_ascii_lowercase();
    let root = base.join(&package.id).join(&digest).join("content");
    let marker = root.parent().unwrap().join("package.json");
    let installed = std::fs::read(&marker)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<InstalledPackage>(&bytes).ok())
        .filter(|record| record.sha256.eq_ignore_ascii_case(&digest) && record.root == root);

    let record = if let Some(record) = installed {
        if tree_sha256(&root)? != record.tree_sha256 {
            return Err(GhError::config(format!(
                "managed package `{}` was modified locally",
                package.id
            )));
        }
        record
    } else {
        let bytes = fetcher.fetch(source_ref, artifact_id)?;
        let actual = hex::encode(Sha256::digest(&bytes));
        if !actual.eq_ignore_ascii_case(&digest) {
            return Err(GhError::config(format!(
                "package `{}` sha mismatch: expected {}, got {actual}",
                package.id, sha256
            )));
        }
        let staging = base
            .join(".staging")
            .join(format!("{}-{}", package.id, std::process::id()));
        if staging.exists() {
            std::fs::remove_dir_all(&staging).map_err(|source| GhError::Io {
                path: staging.clone(),
                source,
            })?;
        }
        std::fs::create_dir_all(&staging).map_err(|source| GhError::Io {
            path: staging.clone(),
            source,
        })?;
        extract_safe(&bytes, &staging, &package.id)?;
        validate_adapter_paths(adapter, &staging)?;
        let tree = tree_sha256(&staging)?;
        if let Some(parent) = root.parent() {
            std::fs::create_dir_all(parent).map_err(|source| GhError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        if root.exists() {
            std::fs::remove_dir_all(&root).map_err(|source| GhError::Io {
                path: root.clone(),
                source,
            })?;
        }
        std::fs::rename(&staging, &root).map_err(|source| GhError::Io {
            path: root.clone(),
            source,
        })?;
        let record = InstalledPackage {
            version: package.version.clone(),
            sha256: digest,
            root: root.clone(),
            tree_sha256: tree,
            selected_adapter: adapter.clone(),
            introduced: interval.introduced.clone(),
            before: interval.before.clone(),
        };
        write_atomic(
            &marker,
            serde_json::to_vec_pretty(&record)
                .map_err(|error| GhError::Serde(error.to_string()))?,
        )?;
        record
    };

    let activation = resolve_adapter(
        harness,
        package,
        adapter,
        &record.root,
        implementation,
        true,
    )?;
    let mut record = record;
    // Migrate a pre-v2 record only after it has passed current validation.
    record.selected_adapter = adapter.clone();
    record.introduced = interval.introduced.clone();
    record.before = interval.before.clone();
    Ok((record, activation))
}

fn resolve_adapter(
    _harness: Harness,
    package: &ManagedPackage,
    adapter: &PackageAdapter,
    root: &Path,
    implementation: &'static dyn crate::adapters::HarnessImplementation,
    prepare_helpers: bool,
) -> Result<PreparedPackages, GhError> {
    validate_adapter_paths(adapter, root)?;
    validate_harness_adapter(implementation, adapter, root)?;
    let mut out = PreparedPackages::default();
    out.resolved.groups.push(crate::adapters::ResolvedPackage {
        id: package.id.clone(),
        ..Default::default()
    });
    if let Some(plugin_dir) = &adapter.plugin_dir {
        let path = checked_join(root, plugin_dir)?;
        out.resolved.groups[0].plugin = Some(path);
    }
    if let Some(skills) = &adapter.skills_dir {
        let path = checked_join(root, skills)?;
        out.skills_dirs.push(path);
    }
    if let Some(agents) = &adapter.agents_dir {
        out.agents_dirs.push(checked_join(root, agents)?);
    }
    if let Some(hooks) = &adapter.hooks_file {
        out.hooks_files.push(checked_join(root, hooks)?);
    }
    for plugin in &adapter.plugins {
        out.plugin_modules.push(checked_join(root, plugin)?);
    }
    let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    for (name, asset) in &adapter.helpers {
        let relative = asset
            .paths
            .get(&platform)
            .or_else(|| asset.paths.get("default"))
            .ok_or_else(|| {
                GhError::config(format!(
                    "package `{}` helper `{name}` has no asset for {platform}",
                    package.id
                ))
            })?;
        let path = checked_join(root, relative)?;
        if prepare_helpers {
            make_executable(&path)?;
        } else if !path.is_file() {
            return Err(GhError::config(format!(
                "managed helper `{name}` is missing at {}",
                path.display()
            )));
        }
        out.helpers.insert(name.clone(), path);
    }
    let group = &mut out.resolved.groups[0];
    group.skills = out.skills_dirs.clone();
    group.agents = out.agents_dirs.clone();
    group.hooks = out.hooks_files.clone();
    group.plugin_modules = out.plugin_modules.clone();
    group.helpers = out.helpers.clone();
    let components = implementation.package_components(&out.resolved);
    out.launch_args = components.launch_args;
    out.env = components.env;
    out.skills_dirs = components.skills_dirs;
    out.agents_dirs = components.agents_dirs;
    out.hooks_files = components.hooks_files;
    out.plugin_modules = components.plugin_modules;
    Ok(out)
}

fn validate_harness_adapter(
    implementation: &'static dyn crate::adapters::HarnessImplementation,
    adapter: &PackageAdapter,
    root: &Path,
) -> Result<(), GhError> {
    implementation.validate_components(adapter)?;
    implementation.validate_staged_components(adapter, root)
}

fn validate_id(value: &str) -> Result<(), GhError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(GhError::config(format!("invalid package id `{value}`")));
    }
    Ok(())
}

fn validate_sha(id: &str, value: &str) -> Result<(), GhError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GhError::config(format!(
            "package `{id}` must have a 64-character sha256"
        )));
    }
    Ok(())
}

fn validate_adapter_paths(adapter: &PackageAdapter, root: &Path) -> Result<(), GhError> {
    let directories = adapter
        .plugin_dir
        .iter()
        .chain(adapter.skills_dir.iter())
        .chain(adapter.agents_dir.iter());
    for relative in directories {
        let path = checked_join(root, relative)?;
        if !path.is_dir() {
            return Err(GhError::config(format!(
                "package component directory `{relative}` does not exist"
            )));
        }
    }
    let files = adapter
        .hooks_file
        .iter()
        .chain(adapter.plugins.iter())
        .chain(
            adapter
                .helpers
                .values()
                .flat_map(|asset| asset.paths.values()),
        );
    for relative in files {
        let path = checked_join(root, relative)?;
        if !path.is_file() {
            return Err(GhError::config(format!(
                "package component file `{relative}` does not exist"
            )));
        }
    }
    Ok(())
}

pub(crate) fn checked_join(root: &Path, relative: &str) -> Result<PathBuf, GhError> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(GhError::config(format!("unsafe package path `{relative}`")));
    }
    Ok(root.join(path))
}

fn extract_safe(bytes: &[u8], dest: &Path, id: &str) -> Result<(), GhError> {
    let decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    for entry in archive
        .entries()
        .map_err(|error| GhError::other(format!("reading package `{id}`: {error}")))?
    {
        let mut entry = entry
            .map_err(|error| GhError::other(format!("reading package `{id}` entry: {error}")))?;
        let relative = entry
            .path()
            .map_err(|error| GhError::other(format!("reading package `{id}` path: {error}")))?;
        let relative_text = relative.to_string_lossy();
        let target = checked_join(dest, &relative_text)?;
        let kind = entry.header().entry_type();
        // GitHub codeload archives include a harmless POSIX global metadata
        // header. `tar` applies local PAX metadata while iterating entries;
        // global metadata has no filesystem payload and should be ignored.
        if kind.is_pax_global_extensions() {
            continue;
        }
        if kind.is_symlink() || kind.is_hard_link() || !(kind.is_file() || kind.is_dir()) {
            return Err(GhError::config(format!(
                "package `{id}` contains unsupported link or special entry `{relative_text}`"
            )));
        }
        if kind.is_dir() {
            std::fs::create_dir_all(&target).map_err(|source| GhError::Io {
                path: target,
                source,
            })?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|source| GhError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            let mut file = File::create(&target).map_err(|source| GhError::Io {
                path: target.clone(),
                source,
            })?;
            std::io::copy(&mut entry, &mut file).map_err(|source| GhError::Io {
                path: target,
                source,
            })?;
        }
    }
    Ok(())
}

fn fetch_bytes(source_ref: &str) -> Result<Vec<u8>, GhError> {
    const MAX_PACKAGE_BYTES: u64 = 100 * 1024 * 1024;
    if source_ref.starts_with("http://") || source_ref.starts_with("https://") {
        let response = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|error| GhError::service(format!("building package client: {error}")))?
            .get(source_ref)
            .send()
            .map_err(|error| GhError::service(format!("fetching package {source_ref}: {error}")))?;
        if !response.status().is_success() {
            return Err(GhError::service(format!(
                "fetching package {source_ref}: HTTP {}",
                response.status()
            )));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_PACKAGE_BYTES)
        {
            return Err(GhError::service(format!(
                "package {source_ref} exceeds the 100 MiB limit"
            )));
        }
        let mut bytes = Vec::new();
        response
            .take(MAX_PACKAGE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| GhError::service(format!("reading package {source_ref}: {error}")))?;
        if bytes.len() as u64 > MAX_PACKAGE_BYTES {
            return Err(GhError::service(format!(
                "package {source_ref} exceeds the 100 MiB limit"
            )));
        }
        Ok(bytes)
    } else {
        let path = PathBuf::from(source_ref.strip_prefix("file://").unwrap_or(source_ref));
        std::fs::read(&path).map_err(|source| GhError::Io { path, source })
    }
}

fn tree_sha256(root: &Path) -> Result<String, GhError> {
    let mut paths = Vec::new();
    collect_files(root, root, &mut paths)?;
    paths.sort();
    let mut digest = Sha256::new();
    for relative in paths {
        let full = root.join(&relative);
        digest.update(relative.to_string_lossy().as_bytes());
        digest.update([0]);
        let mut file = File::open(&full).map_err(|source| GhError::Io {
            path: full.clone(),
            source,
        })?;
        let mut buffer = [0_u8; 8192];
        loop {
            let count = file.read(&mut buffer).map_err(|source| GhError::Io {
                path: full.clone(),
                source,
            })?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
    }
    Ok(hex::encode(digest.finalize()))
}

fn collect_files(root: &Path, current: &Path, out: &mut Vec<PathBuf>) -> Result<(), GhError> {
    for entry in std::fs::read_dir(current).map_err(|source| GhError::Io {
        path: current.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| GhError::Io {
            path: current.to_path_buf(),
            source,
        })?;
        let kind = entry.file_type().map_err(|source| GhError::Io {
            path: entry.path(),
            source,
        })?;
        if kind.is_symlink() {
            return Err(GhError::config(format!(
                "managed package contains symlink {}",
                entry.path().display()
            )));
        }
        if kind.is_dir() {
            collect_files(root, &entry.path(), out)?;
        } else if kind.is_file() {
            out.push(entry.path().strip_prefix(root).unwrap().to_path_buf());
        }
    }
    Ok(())
}

fn garbage_collect(
    base: &Path,
    state: &mut PackageState,
    candidates: &BTreeMap<String, InstalledPackage>,
) -> Result<(), GhError> {
    let referenced = state
        .harnesses
        .values()
        .flat_map(|packages| packages.values())
        .map(|record| record.root.clone())
        .collect::<BTreeSet<_>>();
    mark_inactive_candidates(base, candidates)?;
    let inactive_dir = base.join(".inactive");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    for entry in std::fs::read_dir(&inactive_dir).map_err(|source| GhError::Io {
        path: inactive_dir.clone(),
        source,
    })? {
        let entry = entry.map_err(|source| GhError::Io {
            path: inactive_dir.clone(),
            source,
        })?;
        let marker = entry.path();
        if marker.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let inactive: InactivePackage =
            serde_json::from_slice(&std::fs::read(&marker).map_err(|source| GhError::Io {
                path: marker.clone(),
                source,
            })?)
            .map_err(|error| GhError::Serde(error.to_string()))?;
        let id = inactive.id;
        let record = inactive.record;
        if referenced.contains(&record.root) {
            std::fs::remove_file(&marker).map_err(|source| GhError::Io {
                path: marker,
                source,
            })?;
            continue;
        }
        if now.saturating_sub(inactive.inactive_since) < INACTIVE_RETENTION_SECONDS {
            continue;
        }
        if !record.root.exists() {
            std::fs::remove_file(&marker).map_err(|source| GhError::Io {
                path: marker,
                source,
            })?;
            continue;
        }
        if tree_sha256(&record.root).ok().as_deref() == Some(record.tree_sha256.as_str()) {
            if let Some(version_dir) = record.root.parent() {
                std::fs::remove_dir_all(version_dir).map_err(|source| GhError::Io {
                    path: version_dir.to_path_buf(),
                    source,
                })?;
            }
        } else {
            let quarantine = base.join("quarantine").join(format!(
                "{}-{}-{}",
                record
                    .root
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    .unwrap_or("package"),
                std::process::id(),
                state.quarantine.len() + 1
            ));
            if let Some(parent) = quarantine.parent() {
                std::fs::create_dir_all(parent).map_err(|source| GhError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            std::fs::rename(record.root.parent().unwrap(), &quarantine).map_err(|source| {
                GhError::Io {
                    path: quarantine.clone(),
                    source,
                }
            })?;
            state.quarantine.push(PackageStatus {
                id,
                version: record.version.clone(),
                sha256: record.sha256.clone(),
                harness: "removed".into(),
                state: "quarantined".into(),
                error: Some(quarantine.display().to_string()),
            });
        }
        std::fs::remove_file(&marker).map_err(|source| GhError::Io {
            path: marker,
            source,
        })?;
    }
    Ok(())
}

fn mark_inactive_candidates(
    base: &Path,
    candidates: &BTreeMap<String, InstalledPackage>,
) -> Result<(), GhError> {
    let inactive_dir = base.join(".inactive");
    std::fs::create_dir_all(&inactive_dir).map_err(|source| GhError::Io {
        path: inactive_dir.clone(),
        source,
    })?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    for (id, record) in candidates {
        if !record.root.exists() {
            continue;
        }
        let key = hex::encode(Sha256::digest(record.root.to_string_lossy().as_bytes()));
        let marker = inactive_dir.join(format!("{key}.json"));
        if marker.exists() {
            continue;
        }
        write_atomic(
            &marker,
            serde_json::to_vec_pretty(&InactivePackage {
                id: id.clone(),
                inactive_since: now,
                record: record.clone(),
            })
            .map_err(|error| GhError::Serde(error.to_string()))?,
        )?;
    }
    Ok(())
}

pub(crate) fn env_key(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), GhError> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)
        .map_err(|source| GhError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .permissions();
    permissions.set_mode(permissions.mode() | 0o500);
    std::fs::set_permissions(path, permissions).map_err(|source| GhError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn make_executable(path: &Path) -> Result<(), GhError> {
    if path.is_file() {
        Ok(())
    } else {
        Err(GhError::config(format!(
            "helper {} is not a file",
            path.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_component_paths() {
        let root = std::env::temp_dir();
        assert!(checked_join(&root, "../outside").is_err());
        assert!(checked_join(&root, "/outside").is_err());
        assert!(checked_join(&root, "skills/review").is_ok());
    }

    #[test]
    fn validates_package_ids_and_digests() {
        assert!(validate_id("ponytail-1").is_ok());
        assert!(validate_id("Ponytail").is_err());
        assert!(validate_id("../ponytail").is_err());
        assert!(validate_sha("p", &"a".repeat(64)).is_ok());
        assert!(validate_sha("p", "main").is_err());
    }

    #[test]
    fn unversioned_package_state_migrates_to_v4_in_memory() {
        let state: PackageState = serde_json::from_value(serde_json::json!({
            "harnesses": {},
            "failures": {},
            "quarantine": []
        }))
        .unwrap();
        assert_eq!(state.schema_version, 4);
    }

    #[test]
    fn selects_one_versioned_adapter_and_rejects_overlap() {
        let mut adapter = PackageAdapter {
            variants: vec![
                gh_service::PackageAdapterVariant {
                    introduced: Some("0.0.0".into()),
                    before: Some("2.0.0".into()),
                    skills_dir: Some("skills-v1".into()),
                    ..Default::default()
                },
                gh_service::PackageAdapterVariant {
                    introduced: Some("2.0.0".into()),
                    skills_dir: Some("skills-v2".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            select_adapter(&adapter, &semver::Version::new(2, 1, 0))
                .unwrap()
                .0
                .skills_dir
                .as_deref(),
            Some("skills-v2")
        );
        adapter.variants.push(gh_service::PackageAdapterVariant {
            introduced: Some("2.1.0".into()),
            plugin_dir: Some("overlap".into()),
            ..Default::default()
        });
        assert!(select_adapter(&adapter, &semver::Version::new(2, 1, 0)).is_err());
    }

    #[test]
    fn failed_reconcile_retains_incompatible_previous_content_without_activation() {
        let root = std::env::temp_dir().join(format!("blue-retain-{}", std::process::id()));
        std::fs::create_dir_all(root.join("skills")).unwrap();
        let record = InstalledPackage {
            version: "1.0.0".into(),
            sha256: "a".repeat(64),
            root: root.clone(),
            tree_sha256: String::new(),
            selected_adapter: PackageAdapter {
                skills_dir: Some("skills".into()),
                ..Default::default()
            },
            introduced: semver::Version::new(1, 0, 0),
            before: Some(semver::Version::new(2, 0, 0)),
        };
        let previous = [("kit".into(), record)].into_iter().collect();
        let package = ManagedPackage {
            id: "kit".into(),
            name: None,
            version: "2.0.0".into(),
            source_ref: String::new(),
            artifact_id: None,
            sha256: "b".repeat(64),
            platform_sources: Default::default(),
            settings: Default::default(),
            adapters: Default::default(),
        };
        let mut next = BTreeMap::new();
        let mut activation = PreparedPackages::default();
        retain_previous(
            &previous,
            "kit",
            Harness::Codex,
            &package,
            &semver::Version::new(2, 0, 0),
            &HarnessPolicy::default(),
            crate::adapters::codex::IMPLEMENTATIONS[0].implementation,
            &mut next,
            &mut activation,
        );
        assert!(next.contains_key("kit"));
        assert!(root.exists());
        assert!(activation.skills_dirs.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_links_in_package_archives() {
        let mut compressed = Vec::new();
        {
            let encoder =
                flate2::write::GzEncoder::new(&mut compressed, flate2::Compression::default());
            let mut archive = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            header.set_cksum();
            archive
                .append_link(&mut header, "skills/escape", "../../outside")
                .unwrap();
            archive.finish().unwrap();
        }
        let dest = std::env::temp_dir().join(format!("gh-package-link-{}", std::process::id()));
        std::fs::create_dir_all(&dest).unwrap();
        let error = extract_safe(&compressed, &dest, "unsafe")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported link"));
        let _ = std::fs::remove_dir_all(dest);
    }

    #[test]
    fn installs_verified_package_and_detects_local_drift() {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let body = br#"{"name":"review-kit","version":"1.0.0"}"#;
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, "plugin/.codex-plugin/plugin.json", &body[..])
            .unwrap();
        let encoder = archive.into_inner().unwrap();
        let bytes = encoder.finish().unwrap();

        let root = std::env::temp_dir().join(format!("gh-package-install-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("review-kit.tar.gz");
        std::fs::write(&source, &bytes).unwrap();
        let package = ManagedPackage {
            id: "review-kit".into(),
            name: None,
            version: "1.0.0".into(),
            source_ref: root.join("unused-default.tar.gz").display().to_string(),
            artifact_id: None,
            sha256: "b".repeat(64),
            platform_sources: [(
                format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
                gh_service::PackageSource {
                    source_ref: source.display().to_string(),
                    artifact_id: None,
                    sha256: hex::encode(Sha256::digest(&bytes)),
                },
            )]
            .into_iter()
            .collect(),
            settings: Default::default(),
            adapters: Default::default(),
        };
        let adapter = PackageAdapter {
            plugin_dir: Some("plugin".into()),
            ..Default::default()
        };
        let (record, activation) = install_and_resolve(
            &root.join("store"),
            Harness::Claude,
            &package,
            &adapter,
            &gh_service::PackageAdapterInterval {
                introduced: semver::Version::new(0, 0, 0),
                before: None,
            },
            &crate::adapters::claude::v2_0_12::IMPLEMENTATION,
            &DirectPackageFetcher,
        )
        .unwrap();
        assert!(record
            .root
            .join("plugin/.codex-plugin/plugin.json")
            .is_file());
        assert_eq!(activation.launch_args[0], "--plugin-dir");
        std::fs::write(
            record.root.join("plugin/.codex-plugin/plugin.json"),
            "changed",
        )
        .unwrap();
        let error = install_and_resolve(
            &root.join("store"),
            Harness::Claude,
            &package,
            &adapter,
            &gh_service::PackageAdapterInterval {
                introduced: semver::Version::new(0, 0, 0),
                before: None,
            },
            &crate::adapters::claude::v2_0_12::IMPLEMENTATION,
            &DirectPackageFetcher,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("modified locally"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unreferenced_content_is_retained_for_seven_days() {
        let root =
            std::env::temp_dir().join(format!("blue-inactive-retention-{}", std::process::id()));
        let base = root.join("packages");
        let content = base.join("kit/digest/content");
        std::fs::create_dir_all(&content).unwrap();
        std::fs::write(content.join("file.txt"), "managed").unwrap();
        let record = InstalledPackage {
            version: "1.0.0".into(),
            sha256: "a".repeat(64),
            root: content.clone(),
            tree_sha256: tree_sha256(&content).unwrap(),
            selected_adapter: PackageAdapter::default(),
            introduced: semver::Version::new(0, 0, 0),
            before: None,
        };
        let candidates = [("kit".into(), record)].into_iter().collect();
        let mut state = PackageState::default();
        garbage_collect(&base, &mut state, &candidates).unwrap();
        assert!(content.exists(), "newly inactive content must be retained");

        let marker = std::fs::read_dir(base.join(".inactive"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut inactive: InactivePackage =
            serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
        inactive.inactive_since = inactive
            .inactive_since
            .saturating_sub(INACTIVE_RETENTION_SECONDS + 1);
        std::fs::write(&marker, serde_json::to_vec(&inactive).unwrap()).unwrap();
        garbage_collect(&base, &mut state, &BTreeMap::new()).unwrap();
        assert!(
            !content.exists(),
            "expired inactive content should be removed"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
