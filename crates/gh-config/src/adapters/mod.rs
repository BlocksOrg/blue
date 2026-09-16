//! Trusted, compiled harness implementation registry. Governance selects
//! data; it cannot select executable translators or arbitrary renderer names.

use std::path::{Path, PathBuf};

use gh_common::{ComponentRules, GhError, Harness, HarnessMetadata, InstallInvocation};
use gh_gateway::{AuthPlacement, GatewayWiring};
use gh_service::{GatewayConfig, HarnessPolicy, PackageAdapter};
use semver::Version;

use crate::{HarnessWrite, WriteOptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImplementationLifecycle {
    Supported,
    Deprecated,
}

#[derive(Debug, Clone, Copy)]
pub struct VersionInterval {
    pub profile: &'static str,
    pub aliases: &'static [&'static str],
    pub introduced: &'static str,
    pub before: Option<&'static str>,
    /// Exclusive upper bound of releases certified against this implementation.
    pub verified_before: &'static str,
    pub lifecycle: ImplementationLifecycle,
}

#[derive(Clone, Copy)]
pub struct ImplementationRegistration {
    pub interval: VersionInterval,
    pub implementation: &'static dyn HarnessImplementation,
}

impl std::fmt::Debug for ImplementationRegistration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImplementationRegistration")
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, serde::Serialize)]
pub struct PublicGenerationMetadata {
    pub profile: &'static str,
    pub introduced: &'static str,
    pub before: Option<&'static str>,
    pub verified_before: &'static str,
    pub lifecycle: ImplementationLifecycle,
    pub capabilities: &'static [&'static str],
    pub component_rules: ComponentRules,
}

#[derive(Debug, Clone, Copy)]
pub struct GenerationSupport {
    pub capabilities: &'static [&'static str],
    pub component_rules: ComponentRules,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayModelExposure {
    Catalog,
    SelectedOnly,
}

#[derive(Debug, serde::Serialize)]
pub struct PublicHarnessMetadata {
    pub key: &'static str,
    pub aliases: &'static [&'static str],
    pub label: &'static str,
    pub description: &'static str,
    pub binary_names: &'static [&'static str],
    pub install_command_template: &'static str,
    pub gateway_model_exposure: GatewayModelExposure,
    /// Backward-compatible summary derived from every generation.
    pub capabilities: Vec<&'static str>,
    /// Backward-compatible current-generation rules. Consumers should prefer
    /// the generation-specific rules below.
    pub component_rules: ComponentRules,
    pub generations: Vec<PublicGenerationMetadata>,
}

#[derive(Debug, Clone)]
pub struct PlannedFile {
    pub path: PathBuf,
    pub body: Vec<u8>,
    pub mode: Option<u32>,
}

#[derive(Debug, Default)]
pub struct ReconcilePlan {
    pub writes: Vec<PlannedFile>,
    pub remove_paths: Vec<PathBuf>,
    pub owned_paths: Vec<PathBuf>,
    pub files: Vec<PathBuf>,
    pub env: std::collections::BTreeMap<String, String>,
    pub launch_args: Vec<String>,
    pub warnings: Vec<String>,
}

impl ReconcilePlan {
    pub(crate) fn report(&self) -> HarnessWrite {
        HarnessWrite {
            files: self.files.clone(),
            env: self.env.clone(),
            launch_args: self.launch_args.clone(),
            package_errors: Vec::new(),
            warnings: self.warnings.clone(),
        }
    }
}

pub struct ReconcileInput<'a> {
    pub home: &'a Path,
    pub policy: &'a HarnessPolicy,
    pub gateway: Option<&'a GatewayWiring>,
    pub options: WriteOptions,
    pub interval: &'a VersionInterval,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionUploadDisposition {
    Disabled,
    Installed,
    Unsupported { reason: String },
}

/// An explicitly selected version capability. Version specifications use this
/// instead of `Option` so unsupported behavior always carries an explanation.
#[derive(Debug, Clone, Copy)]
pub enum Feature<T> {
    Supported(T),
    Unsupported(&'static str),
}

pub(crate) type InspectionValues = std::collections::BTreeMap<String, String>;
pub(crate) type InspectOperation = fn(&HarnessPolicy, &[PathBuf]) -> InspectionValues;
pub(crate) type ProposedValuesOperation =
    fn(&HarnessPolicy, Option<&GatewayConfig>, WriteOptions, InspectionValues) -> InspectionValues;
pub(crate) type TranscriptOperation =
    fn(&HarnessDefinition, &Path, &str, &serde_json::Value) -> Result<PathBuf, GhError>;

pub(crate) fn accept_staged_components(
    _adapter: &PackageAdapter,
    _root: &Path,
) -> Result<(), GhError> {
    Ok(())
}

pub(crate) fn collect_package_components(packages: &ResolvedPackages) -> PackageComponents {
    let mut components = PackageComponents::default();
    for package in &packages.groups {
        components.append(package);
    }
    components
}

pub(crate) fn payload_transcript_path(
    definition: &HarnessDefinition,
    _home: &Path,
    _session_id: &str,
    payload: &serde_json::Value,
) -> Result<PathBuf, GhError> {
    payload
        .get("transcript_path")
        .and_then(serde_json::Value::as_str)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            GhError::config(format!(
                "{} hook payload has no usable transcript_path",
                definition.metadata.label
            ))
        })
}

pub(crate) fn apply_session_upload_disposition(
    disposition: SessionUploadDisposition,
    report: &mut HarnessWrite,
) {
    match disposition {
        SessionUploadDisposition::Installed | SessionUploadDisposition::Disabled => {}
        SessionUploadDisposition::Unsupported { reason } => {
            report.warnings.push(reason);
        }
    }
}

/// Filesystem authority for a single compatibility implementation.
#[derive(Debug, Default)]
pub struct ImplementationPaths {
    pub read_only_sources: Vec<PathBuf>,
    pub owned_outputs: Vec<PathBuf>,
    pub native_migrations: Vec<PathBuf>,
}

impl ImplementationPaths {
    pub(crate) fn transaction_targets(&self) -> Vec<PathBuf> {
        self.owned_outputs
            .iter()
            .chain(&self.native_migrations)
            .cloned()
            .collect()
    }
}

/// Immutable, package-grouped inputs. Activation state remains engine-private.
#[derive(Debug, Default, Clone)]
pub struct ResolvedPackages {
    pub groups: Vec<ResolvedPackage>,
}

#[derive(Debug, Default, Clone)]
pub struct ResolvedPackage {
    pub id: String,
    pub plugin: Option<PathBuf>,
    pub skills: Vec<PathBuf>,
    pub agents: Vec<PathBuf>,
    pub hooks: Vec<PathBuf>,
    pub plugin_modules: Vec<PathBuf>,
    pub helpers: std::collections::BTreeMap<String, PathBuf>,
    pub settings_environment: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Default)]
pub struct PackageComponents {
    pub launch_args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    pub skills_dirs: Vec<PathBuf>,
    pub agents_dirs: Vec<PathBuf>,
    pub hooks_files: Vec<PathBuf>,
    pub plugin_modules: Vec<PathBuf>,
    pub helpers: std::collections::BTreeMap<String, PathBuf>,
}

impl PackageComponents {
    pub(crate) fn append(&mut self, package: &ResolvedPackage) {
        self.env.extend(package.settings_environment.clone());
        self.skills_dirs.extend(package.skills.iter().cloned());
        self.agents_dirs.extend(package.agents.iter().cloned());
        self.hooks_files.extend(package.hooks.iter().cloned());
        self.plugin_modules
            .extend(package.plugin_modules.iter().cloned());
        self.helpers.extend(package.helpers.clone());
    }
}

pub trait HarnessImplementation: std::fmt::Debug + Sync {
    fn support(&self) -> &'static GenerationSupport;
    fn launch(
        &self,
        _home: &Path,
        _wiring: Option<&GatewayWiring>,
        _policy: &HarnessPolicy,
        spec: crate::HarnessLaunchSpec,
    ) -> Result<crate::HarnessLaunchSpec, GhError> {
        Ok(spec)
    }
    fn paths(&self, home: &Path) -> ImplementationPaths;
    fn native_migration_needs_review(&self, path: &Path, _policy: &HarnessPolicy) -> bool {
        path.exists()
    }

    fn plan(
        &self,
        input: &ReconcileInput<'_>,
        packages: &ResolvedPackages,
    ) -> Result<ReconcilePlan, GhError>;
    /// Declare native session-upload support for this exact compatibility
    /// interval. The implementation's planner owns the corresponding hook
    /// representation and must not infer a different version from PATH.
    fn session_upload_disposition(&self, input: &ReconcileInput<'_>) -> SessionUploadDisposition;
    /// Native restoration is versioned independently from capture. A profile
    /// may therefore remain upload/download capable without appearing in the
    /// remote resume picker.
    fn session_resume_capability(&self) -> Feature<()> {
        Feature::Unsupported(
            "this compatibility profile does not support native session restoration",
        )
    }
    fn gateway_wiring(&self, gateway: &GatewayConfig) -> Result<GatewayWiring, GhError>;
    fn validate_components(&self, adapter: &PackageAdapter) -> Result<(), GhError>;
    fn validate_staged_components(
        &self,
        _adapter: &PackageAdapter,
        _root: &Path,
    ) -> Result<(), GhError> {
        Ok(())
    }
    fn package_components(&self, packages: &ResolvedPackages) -> PackageComponents {
        collect_package_components(packages)
    }
    fn install_plan(
        &self,
        interval: &VersionInterval,
        requirement: Option<&str>,
    ) -> InstallInvocation;
    fn inspect(
        &self,
        policy: &HarnessPolicy,
        files: &[PathBuf],
    ) -> std::collections::BTreeMap<String, String>;
    fn proposed_values(
        &self,
        policy: &HarnessPolicy,
        gateway: Option<&GatewayConfig>,
        options: WriteOptions,
        current: std::collections::BTreeMap<String, String>,
    ) -> std::collections::BTreeMap<String, String>;
    fn transcript_path(
        &self,
        definition: &HarnessDefinition,
        _home: &Path,
        _session_id: &str,
        payload: &serde_json::Value,
    ) -> Result<PathBuf, GhError> {
        payload_transcript_path(definition, _home, _session_id, payload)
    }
    fn capture_session(
        &self,
        definition: &HarnessDefinition,
        home: &Path,
        session_id: &str,
        payload: &serde_json::Value,
    ) -> Result<Vec<crate::session_bundle::SessionSource>, GhError> {
        let source = self.transcript_path(definition, home, session_id, payload)?;
        Ok(vec![crate::session_bundle::SessionSource {
            role: "primary_transcript".into(),
            native_path: crate::session_bundle::portable_native_path(home, &source),
            source,
        }])
    }
    fn prepare_session_file(&self, _role: &str, bytes: Vec<u8>) -> Result<Vec<u8>, GhError> {
        Ok(bytes)
    }
    fn session_file_equivalent(&self, _role: &str, existing: &[u8], bundled: &[u8]) -> bool {
        existing == bundled
    }
}

#[derive(Debug, Clone)]
pub struct DetectedVersion {
    pub raw: String,
    pub version: Option<Version>,
}

#[derive(Debug, Clone, Copy)]
pub struct VersionProbe {
    pub args: &'static [&'static str],
    pub parser: fn(&str) -> Option<Version>,
}

impl VersionProbe {
    pub const fn command(
        args: &'static [&'static str],
        parser: fn(&str) -> Option<Version>,
    ) -> Self {
        Self { args, parser }
    }
}

#[derive(Debug)]
pub struct HarnessDefinition {
    pub harness: Harness,
    pub metadata: &'static HarnessMetadata,
    pub version_probes: &'static [VersionProbe],
    pub implementations: &'static [ImplementationRegistration],
}

impl HarnessDefinition {
    pub fn select(&'static self, version: &Version) -> Option<&'static ImplementationRegistration> {
        select_from(self.implementations, version)
    }
    pub fn profile(&'static self, profile: &str) -> Option<&'static ImplementationRegistration> {
        self.implementations.iter().find(|registration| {
            registration.interval.profile == profile
                || registration.interval.aliases.contains(&profile)
        })
    }

    pub fn detect_version(&self, binary: &Path) -> Result<DetectedVersion, GhError> {
        let mut last_raw = String::new();
        for probe in self.version_probes {
            let output = std::process::Command::new(binary)
                .args(probe.args)
                .output()
                .map_err(|source| GhError::Io {
                    path: binary.to_path_buf(),
                    source,
                })?;
            let raw = if output.stdout.is_empty() {
                &output.stderr
            } else {
                &output.stdout
            };
            last_raw = String::from_utf8_lossy(raw).trim().to_owned();
            if !output.status.success() {
                continue;
            }
            if let Some(version) = (probe.parser)(&last_raw) {
                return Ok(DetectedVersion {
                    raw: last_raw,
                    version: Some(version),
                });
            }
        }
        Ok(DetectedVersion {
            raw: last_raw,
            version: None,
        })
    }
}

pub fn parse_semver_token(raw: &str) -> Option<Version> {
    raw.split_whitespace()
        .map(|token| {
            token
                .trim_matches(|c: char| matches!(c, ',' | '(' | ')' | '[' | ']'))
                .trim_start_matches('v')
        })
        .find_map(|token| Version::parse(token).ok())
}

pub(crate) fn normalize_npm_requirement(value: &str) -> String {
    value
        .replace(',', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

static DEFAULT_VERSION_PROBES: &[VersionProbe] =
    &[VersionProbe::command(&["--version"], parse_semver_token)];

macro_rules! declare_harnesses {
    ($($harness:ident => $module:ident { $($fields:tt)* })*) => {
        $(pub mod $module;)*
        pub static HARNESS_DEFINITIONS: std::sync::LazyLock<Vec<HarnessDefinition>> =
            std::sync::LazyLock::new(|| vec![
            $(HarnessDefinition {
                harness: Harness::$harness,
                metadata: Harness::$harness.metadata(),
                version_probes: $module::VERSION_PROBES,
                implementations: $module::IMPLEMENTATIONS,
            }),*
        ]);
    };
}
gh_common::harness_catalog!(declare_harnesses);

pub(crate) fn npm_install_plan(package: &str, requirement: Option<&str>) -> InstallInvocation {
    let selector = requirement
        .map(normalize_npm_requirement)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "latest".to_owned());
    let args = vec![
        "install".into(),
        "-g".into(),
        format!("{package}@{selector}"),
    ];
    InstallInvocation {
        program: "npm",
        display: format!("npm install -g '{}'", args[2].replace('\'', "'\\''")),
        args,
    }
}

pub fn definition(harness: Harness) -> &'static HarnessDefinition {
    HARNESS_DEFINITIONS
        .iter()
        .find(|definition| definition.harness == harness)
        .expect("every Harness variant must have exactly one compiled definition")
}

pub fn registry_metadata() -> Vec<PublicHarnessMetadata> {
    HARNESS_DEFINITIONS
        .iter()
        .map(|definition| {
            let metadata = definition.metadata;
            let current_support = definition
                .implementations
                .last()
                .expect("every harness has an implementation")
                .implementation
                .support();
            let capabilities = definition
                .implementations
                .iter()
                .flat_map(|registration| registration.implementation.support().capabilities)
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            PublicHarnessMetadata {
                key: metadata.key,
                aliases: metadata.aliases,
                label: metadata.label,
                description: metadata.description,
                binary_names: metadata.binary_names,
                install_command_template: metadata.install_command_template,
                gateway_model_exposure: match metadata.key {
                    "opencode" | "kimi" => GatewayModelExposure::Catalog,
                    _ => GatewayModelExposure::SelectedOnly,
                },
                capabilities,
                component_rules: current_support.component_rules,
                generations: definition
                    .implementations
                    .iter()
                    .map(|registration| {
                        let generation = registration.interval;
                        PublicGenerationMetadata {
                            profile: generation.profile,
                            introduced: generation.introduced,
                            before: generation.before,
                            verified_before: generation.verified_before,
                            lifecycle: generation.lifecycle,
                            capabilities: registration.implementation.support().capabilities,
                            component_rules: registration.implementation.support().component_rules,
                        }
                    })
                    .collect(),
            }
        })
        .collect()
}

pub fn select_implementation(
    harness: Harness,
    version: &Version,
) -> Option<&'static ImplementationRegistration> {
    select_from(definition(harness).implementations, version)
}

pub fn implementation_for_profile(
    harness: Harness,
    profile: &str,
) -> Option<&'static ImplementationRegistration> {
    definition(harness)
        .implementations
        .iter()
        .find(|registration| {
            registration.interval.profile == profile
                || registration.interval.aliases.contains(&profile)
        })
}

/// Detection must succeed before any version-dependent operation.
pub fn detected_implementation(
    harness: Harness,
) -> Result<&'static ImplementationRegistration, GhError> {
    let detected = detected_version(harness)?;
    let version = detected.version.ok_or_else(|| {
        GhError::config(format!("unparseable {harness} version `{}`", detected.raw))
    })?;
    select_implementation(harness, &version)
        .ok_or_else(|| GhError::config(format!("unsupported {harness} version {version}")))
}

/// Detect and resolve a harness under the organization's compatibility policy.
pub fn detected_context(
    harness: Harness,
    policy: &gh_service::HarnessPolicy,
) -> Result<crate::compat::HarnessContext, GhError> {
    let detected = detected_version(harness)?;
    crate::compat::resolve(
        harness,
        detected.version.as_ref(),
        Some(&detected.raw),
        policy,
    )
    .map_err(Into::into)
}

fn detected_version(harness: Harness) -> Result<DetectedVersion, GhError> {
    // Resolve exactly the way detection does. A second, simpler scan here used
    // to pick the extensionless file that shares the directory with the real
    // executable: npm's POSIX script on Windows, which `CreateProcess` rejects
    // with `%1 is not a valid Win32 application`, and a Blue shim on Unix,
    // which turns this version probe into a recursive `blue run`.
    let binary = harness
        .binary_names()
        .iter()
        .find_map(|name| gh_common::path_search::which(name))
        .ok_or_else(|| {
            GhError::config(format!(
                "{harness} binary is missing; cannot select compatibility implementation"
            ))
        })?;
    definition(harness).detect_version(&binary)
}

/// Managed files are interpreted using the implementation that wrote them.
pub fn inspection_implementation(
    harness: Harness,
) -> Result<&'static ImplementationRegistration, GhError> {
    let state = crate::load_compatibility_state(harness)?;
    if state.profile_id.is_empty() {
        detected_implementation(harness)
    } else {
        implementation_for_profile(harness, &state.profile_id).ok_or_else(|| {
            GhError::config(format!("unknown persisted profile `{}`", state.profile_id))
        })
    }
}

fn select_from(
    registrations: &'static [ImplementationRegistration],
    version: &Version,
) -> Option<&'static ImplementationRegistration> {
    registrations.iter().rev().find(|registration| {
        let metadata = registration.interval;
        let introduced = Version::parse(metadata.introduced)
            .expect("compiled generation introduced boundary must be valid semver");
        let before = metadata.before.map(|before| {
            Version::parse(before)
                .expect("compiled generation before boundary must be valid semver")
        });
        version >= &introduced && before.as_ref().is_none_or(|before| version < before)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_complete_and_intervals_are_contiguous() {
        let mut keys = std::collections::BTreeSet::new();
        assert_eq!(HARNESS_DEFINITIONS.len(), Harness::ALL.len());
        for harness in Harness::ALL {
            let definition = definition(harness);
            assert_eq!(definition.harness, harness);
            assert!(keys.insert(definition.metadata.key));
            assert!(!definition.version_probes.is_empty());
            assert!(!definition.implementations.is_empty());
            let mut previous: Option<(Version, Option<Version>)> = None;
            for (index, registration) in definition.implementations.iter().enumerate() {
                let metadata = registration.interval;
                let introduced = Version::parse(metadata.introduced).unwrap();
                let before = metadata
                    .before
                    .map(|before| Version::parse(before).unwrap());
                let verified_before = Version::parse(metadata.verified_before).unwrap();
                assert!(introduced < verified_before);
                if let Some(before) = before.as_ref() {
                    assert!(verified_before <= *before);
                }
                if let Some(before) = before.as_ref() {
                    assert!(introduced < *before);
                }
                if let Some((previous_introduced, previous_before)) = previous {
                    assert!(previous_introduced < introduced);
                    assert_eq!(
                        previous_before.as_ref(),
                        Some(&introduced),
                        "generation intervals must be contiguous for {}",
                        harness.key()
                    );
                }
                assert_eq!(
                    before.is_none(),
                    index + 1 == definition.implementations.len()
                );
                let support = registration.implementation.support();
                assert!(!support.capabilities.is_empty());
                previous = Some((introduced, before));
            }
            let first = definition.implementations.first().unwrap().interval;
            assert_eq!(first.introduced, "0.0.0");
            assert!(select_implementation(harness, &Version::new(999, 0, 0)).is_some());
        }
    }

    #[test]
    fn registry_summaries_are_derived_from_version_specs() {
        for public in registry_metadata() {
            let definition = HARNESS_DEFINITIONS
                .iter()
                .find(|definition| definition.metadata.key == public.key)
                .unwrap();
            let expected_capabilities = definition
                .implementations
                .iter()
                .flat_map(|registration| registration.implementation.support().capabilities)
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            assert_eq!(public.capabilities, expected_capabilities);

            let current = definition
                .implementations
                .last()
                .unwrap()
                .implementation
                .support();
            assert_eq!(
                public.component_rules.agents_require_plugin,
                current.component_rules.agents_require_plugin
            );
            assert_eq!(
                public.component_rules.hooks_require_plugin,
                current.component_rules.hooks_require_plugin
            );
            assert_eq!(
                public.component_rules.hooks_as_plugin_modules,
                current.component_rules.hooks_as_plugin_modules
            );
        }
    }

    #[derive(Debug)]
    struct TestImplementation;
    impl HarnessImplementation for TestImplementation {
        fn support(&self) -> &'static GenerationSupport {
            static SUPPORT: GenerationSupport = GenerationSupport {
                capabilities: &["mcp"],
                component_rules: ComponentRules {
                    agents_require_plugin: false,
                    hooks_require_plugin: false,
                    hooks_as_plugin_modules: false,
                },
            };
            &SUPPORT
        }
        fn session_upload_disposition(
            &self,
            input: &ReconcileInput<'_>,
        ) -> SessionUploadDisposition {
            if input.options.session_upload_enabled {
                SessionUploadDisposition::Installed
            } else {
                SessionUploadDisposition::Disabled
            }
        }
        fn paths(&self, _: &Path) -> ImplementationPaths {
            ImplementationPaths::default()
        }
        fn plan(
            &self,
            _: &ReconcileInput<'_>,
            _: &ResolvedPackages,
        ) -> Result<ReconcilePlan, GhError> {
            Ok(ReconcilePlan::default())
        }
        fn gateway_wiring(&self, gateway: &GatewayConfig) -> Result<GatewayWiring, GhError> {
            gh_gateway::wire_with(gateway, AuthPlacement::InFile, None)
        }
        fn validate_components(&self, _: &PackageAdapter) -> Result<(), GhError> {
            Ok(())
        }
        fn install_plan(
            &self,
            _: &VersionInterval,
            requirement: Option<&str>,
        ) -> InstallInvocation {
            npm_install_plan("synthetic", requirement)
        }
        fn inspect(
            &self,
            _: &HarnessPolicy,
            _: &[PathBuf],
        ) -> std::collections::BTreeMap<String, String> {
            [("adapter".into(), "test".into())].into_iter().collect()
        }
        fn proposed_values(
            &self,
            _: &HarnessPolicy,
            _: Option<&GatewayConfig>,
            _: WriteOptions,
            mut current: std::collections::BTreeMap<String, String>,
        ) -> std::collections::BTreeMap<String, String> {
            current.insert("planned".into(), "true".into());
            current
        }
    }
    static TEST_IMPLEMENTATION: TestImplementation = TestImplementation;
    static TEST_REGISTRATIONS: &[ImplementationRegistration] = &[
        ImplementationRegistration {
            interval: VersionInterval {
                profile: "test-v1",
                aliases: &[],
                introduced: "0.0.0",
                before: Some("2.0.0-0"),
                verified_before: "2.0.0-0",
                lifecycle: ImplementationLifecycle::Deprecated,
            },
            implementation: &TEST_IMPLEMENTATION,
        },
        ImplementationRegistration {
            interval: VersionInterval {
                profile: "test-v2",
                aliases: &[],
                introduced: "2.0.0-0",
                before: None,
                verified_before: "2.1.0-0",
                lifecycle: ImplementationLifecycle::Supported,
            },
            implementation: &TEST_IMPLEMENTATION,
        },
    ];

    #[test]
    fn a_new_generation_is_selected_exactly_at_its_registered_boundary() {
        assert_eq!(
            select_from(TEST_REGISTRATIONS, &Version::new(1, 99, 99))
                .unwrap()
                .interval
                .profile,
            "test-v1"
        );
        assert_eq!(
            select_from(TEST_REGISTRATIONS, &Version::new(2, 0, 0))
                .unwrap()
                .interval
                .profile,
            "test-v2"
        );
    }

    #[test]
    fn session_upload_hooks_are_owned_and_scoped_by_each_profile() {
        let claude = claude::v2_0_12::session_upload_hooks("claude-v1").unwrap();
        assert_eq!(claude["SessionEnd"][0]["hooks"][0]["type"], "command");
        assert!(claude.to_string().contains("--profile 'claude-v1'"));

        let codex = codex::v0_145_0::session_upload_hook("codex-v1").unwrap();
        assert!(format!("{codex:?}").contains("--profile 'codex-v1'"));

        let kimi = kimi::v0_0_0::session_upload_hooks("kimi-v1").unwrap();
        assert_eq!(kimi[0]["event"].as_str(), Some("SessionEnd"));
        assert_eq!(kimi[1]["event"].as_str(), Some("Stop"));
        assert!(format!("{kimi:?}").contains("--profile 'kimi-v1'"));

        let opencode = opencode::v0_0_0::session_upload_plugin("opencode-v1").unwrap();
        assert!(opencode.contains("session.idle"));
        assert!(opencode.contains("\"--profile\", \"opencode-v1\""));

        let codex_start = codex::v0_114_0::session_start_hook("codex-v0_114_0").unwrap();
        assert!(format!("{codex_start:?}").contains("session-start codex"));
        assert!(format!("{codex_start:?}").contains("--profile 'codex-v0_114_0'"));

        let claude_start = claude::v2_0_12::session_start_hooks("claude-v1").unwrap();
        assert_eq!(
            claude_start["SessionStart"][0]["hooks"][0]["type"],
            "command"
        );
        assert!(claude_start.to_string().contains("session-start claude"));
        assert!(claude_start.to_string().contains("--profile 'claude-v1'"));

        let kimi_start = kimi::v0_0_0::session_start_hooks("kimi-v1").unwrap();
        assert_eq!(kimi_start.len(), 1);
        assert_eq!(kimi_start[0]["event"].as_str(), Some("SessionStart"));
        assert!(format!("{kimi_start:?}").contains("session-start kimi"));
        assert!(format!("{kimi_start:?}").contains("--profile 'kimi-v1'"));

        // The opencode plugin carries session-start inline (no separate renderer).
        assert!(opencode.contains("session-start"));
        assert!(opencode.contains("\"session-start\", \"opencode\""));

        assert!(implementation_for_profile(Harness::Claude, "claude-v1").is_some());
        assert!(implementation_for_profile(Harness::Claude, "codex-v1").is_none());

        let mut report = HarnessWrite::default();
        apply_session_upload_disposition(
            SessionUploadDisposition::Unsupported {
                reason: "session hooks unavailable".into(),
            },
            &mut report,
        );
        assert_eq!(report.warnings, ["session hooks unavailable"]);
    }

    #[test]
    fn version_specs_declare_capabilities_and_reuse_family_operations() {
        assert!(std::ptr::eq(
            codex::v0_0_0::SPEC.operations,
            codex::v0_145_0::SPEC.operations
        ));
        assert!(std::ptr::eq(
            codex::v0_0_0::SPEC.operations,
            codex::v0_114_0::SPEC.operations
        ));
        assert!(matches!(
            codex::v0_0_0::SPEC.session_upload,
            Feature::Unsupported(reason) if !reason.trim().is_empty()
        ));
        assert!(matches!(
            codex::v0_114_0::SPEC.session_upload,
            Feature::Unsupported(reason) if !reason.trim().is_empty()
        ));
        assert!(matches!(
            codex::v0_145_0::SPEC.session_upload,
            Feature::Supported(_)
        ));
        assert!(matches!(
            codex::v0_0_0::SPEC.session_resume,
            Feature::Unsupported(_)
        ));
        for capability in [
            codex::v0_145_0::SPEC.session_resume,
            claude::v2_0_12::SPEC.session_resume,
            kimi::v0_0_0::SPEC.session_resume,
            opencode::v0_0_0::SPEC.session_resume,
        ] {
            assert!(matches!(capability, Feature::Supported(())));
        }

        // session_start capability matrix across every version spec.
        assert!(matches!(
            codex::v0_0_0::SPEC.session_start,
            Feature::Unsupported(reason) if !reason.trim().is_empty()
        ));
        assert!(matches!(
            codex::v0_114_0::SPEC.session_start,
            Feature::Supported(_)
        ));
        assert!(matches!(
            codex::v0_145_0::SPEC.session_start,
            Feature::Supported(_)
        ));
        for feature in [
            claude::v0_0_0::SPEC.session_start,
            claude::v1_0_38::SPEC.session_start,
        ] {
            assert!(matches!(
                feature,
                Feature::Unsupported(reason) if !reason.trim().is_empty()
            ));
        }
        assert!(matches!(
            claude::v2_0_12::SPEC.session_start,
            Feature::Supported(_)
        ));
        assert!(matches!(
            kimi::v0_0_0::SPEC.session_start,
            Feature::Supported(_)
        ));
        assert!(matches!(
            opencode::v0_0_0::SPEC.session_start,
            Feature::Supported(())
        ));

        assert!(!std::ptr::eq(
            claude::v0_0_0::SPEC.operations,
            claude::v2_0_12::SPEC.operations
        ));
        assert!(!std::ptr::eq(
            claude::v0_0_0::SPEC.operations,
            claude::v1_0_38::SPEC.operations
        ));
        assert!(!std::ptr::eq(
            claude::v1_0_38::SPEC.operations,
            claude::v2_0_12::SPEC.operations
        ));
        for feature in [
            claude::v0_0_0::SPEC.session_upload,
            claude::v1_0_38::SPEC.session_upload,
        ] {
            assert!(matches!(
                feature,
                Feature::Unsupported(reason) if !reason.trim().is_empty()
            ));
        }
        assert!(matches!(
            claude::v2_0_12::SPEC.session_upload,
            Feature::Supported(_)
        ));
    }

    #[test]
    fn current_verified_ceilings_follow_the_certification_lock() {
        let lock: serde_json::Value =
            serde_json::from_str(include_str!("../../../../tests/e2e/agents.lock.json")).unwrap();
        for harness in Harness::ALL {
            let certified =
                Version::parse(lock["agents"][harness.key()]["version"].as_str().unwrap()).unwrap();
            assert!(
                certified.pre.is_empty(),
                "certification locks must pin stable releases"
            );
            let mut expected = Version::new(certified.major, certified.minor, certified.patch + 1);
            expected.pre = semver::Prerelease::new("0").unwrap();
            let current = definition(harness).implementations.last().unwrap().interval;
            assert_eq!(
                Version::parse(current.verified_before).unwrap(),
                expected,
                "{}",
                harness.key()
            );
        }
    }

    #[test]
    fn synthetic_implementation_exercises_the_contract_without_engine_changes() {
        let implementation: &dyn HarnessImplementation = &TEST_IMPLEMENTATION;
        assert_eq!(
            implementation.inspect(&HarnessPolicy::default(), &[])["adapter"],
            "test"
        );
        assert_eq!(
            implementation.proposed_values(
                &HarnessPolicy::default(),
                None,
                WriteOptions::default(),
                Default::default()
            )["planned"],
            "true"
        );
        assert!(implementation
            .transcript_path(
                definition(Harness::Codex),
                Path::new("/tmp"),
                "session",
                &serde_json::json!({"transcript_path":"/tmp/transcript.json"})
            )
            .is_ok());
    }

    #[test]
    fn parses_current_vendor_version_fixtures_and_prereleases() {
        for (harness, raw, expected) in [
            (Harness::Codex, "codex-cli 0.149.1", "0.149.1"),
            (Harness::Claude, "2.1.247 (Claude Code)", "2.1.247"),
            (Harness::Kimi, "kimi v0.37.1", "0.37.1"),
            (Harness::Opencode, "opencode 1.18.7", "1.18.7"),
            (Harness::Claude, "claude v2.1.0-beta.2", "2.1.0-beta.2"),
        ] {
            assert_eq!(
                (definition(harness).version_probes[0].parser)(raw),
                Some(Version::parse(expected).unwrap()),
                "fixture failed for {}",
                harness.key()
            );
        }
        assert!((definition(Harness::Codex).version_probes[0].parser)("codex unknown").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn version_detection_accepts_stderr_output() {
        use std::os::unix::fs::PermissionsExt;
        let path =
            std::env::temp_dir().join(format!("blue-version-fixture-{}", std::process::id()));
        std::fs::write(&path, "#!/bin/sh\necho 'codex-cli 0.149.1' >&2\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let detected = definition(Harness::Codex).detect_version(&path).unwrap();
        assert_eq!(detected.version, Some(Version::new(0, 149, 1)));
        let _ = std::fs::remove_file(path);
    }
}
