use super::*;

mod inspection;
mod writer;
pub mod v0_0_0;
pub mod v0_114_0;
pub mod v0_145_0;

pub const VERSION_PROBES: &[VersionProbe] = DEFAULT_VERSION_PROBES;
type HookRenderer = fn(&str) -> Result<toml::Value, GhError>;
const UPDATE_CHECK_OVERRIDE: &str = "check_for_update_on_startup=false";

#[derive(Debug)]
pub struct Operations {
    plan: fn(&Implementation, &ReconcileInput<'_>, &ResolvedPackages) -> Result<ReconcilePlan, GhError>,
    launch: fn(&Path, Option<&GatewayWiring>, crate::HarnessLaunchSpec) -> Result<crate::HarnessLaunchSpec, GhError>,
    launch_controls: fn(&Path, &mut crate::HarnessLaunchSpec) -> Result<(), GhError>,
    paths: fn(&Path) -> ImplementationPaths,
    native_migration_needs_review: fn(&Path, &HarnessPolicy) -> bool,
    gateway_wiring: fn(&GatewayConfig) -> Result<GatewayWiring, GhError>,
    validate_components: fn(&PackageAdapter) -> Result<(), GhError>,
    validate_staged_components: fn(&PackageAdapter, &Path) -> Result<(), GhError>,
    package_components: fn(&ResolvedPackages) -> PackageComponents,
    install_plan: fn(Option<&str>) -> InstallInvocation,
    inspect: InspectOperation,
    proposed_values: ProposedValuesOperation,
    transcript_path: TranscriptOperation,
}

#[derive(Debug)]
pub struct VersionSpec {
    pub operations: &'static Operations,
    pub session_upload: Feature<HookRenderer>,
    pub session_start: Feature<HookRenderer>,
    pub session_resume: Feature<()>,
    pub support: &'static GenerationSupport,
}

#[derive(Debug)]
pub struct Implementation { spec: &'static VersionSpec }
impl Implementation { pub const fn new(spec: &'static VersionSpec) -> Self { Self { spec } } }

pub static V1_OPERATIONS: Operations = Operations {
    plan: plan_v1, launch, launch_controls: disable_update_checks, paths, native_migration_needs_review, gateway_wiring,
    validate_components, validate_staged_components, package_components,
    install_plan, inspect, proposed_values, transcript_path: payload_transcript_path,
};

pub static IMPLEMENTATIONS: &[ImplementationRegistration] = &[
    ImplementationRegistration {
        interval: VersionInterval { profile: "codex-v0_0_0", aliases: &[], introduced: "0.0.0", before: Some("0.114.0"), verified_before: "0.114.0-0", lifecycle: ImplementationLifecycle::Supported },
        implementation: &v0_0_0::IMPLEMENTATION,
    },
    ImplementationRegistration {
        interval: VersionInterval { profile: "codex-v0_114_0", aliases: &[], introduced: "0.114.0", before: Some("0.145.0"), verified_before: "0.145.0-0", lifecycle: ImplementationLifecycle::Supported },
        implementation: &v0_114_0::IMPLEMENTATION,
    },
    ImplementationRegistration {
        interval: VersionInterval { profile: "codex-v0_145_0", aliases: &["codex-v1"], introduced: "0.145.0", before: None, verified_before: "0.151.1-0", lifecycle: ImplementationLifecycle::Supported },
        implementation: &v0_145_0::IMPLEMENTATION,
    },
];

pub(crate) fn session_upload_hook(profile: &str) -> Result<toml::Value, GhError> {
    session_hook(crate::util::session_upload_command(Harness::Codex, profile)?)
}

pub(crate) fn session_start_hook(profile: &str) -> Result<toml::Value, GhError> {
    session_hook(crate::util::session_start_command(Harness::Codex, profile)?)
}

fn session_hook(command: String) -> Result<toml::Value, GhError> {
    let mut handler = toml::map::Map::new();
    handler.insert("type".into(), toml::Value::String("command".into()));
    handler.insert("command".into(), toml::Value::String(command));
    handler.insert("timeout".into(), toml::Value::Integer(30));
    let mut group = toml::map::Map::new();
    group.insert("hooks".into(), toml::Value::Array(vec![toml::Value::Table(handler)]));
    Ok(toml::Value::Table(group))
}

impl HarnessImplementation for Implementation {
    fn support(&self) -> &'static GenerationSupport { self.spec.support }
    fn launch(&self, home: &Path, wiring: Option<&GatewayWiring>, spec: crate::HarnessLaunchSpec) -> Result<crate::HarnessLaunchSpec, GhError> {
        let mut spec = (self.spec.operations.launch)(home, wiring, spec)?;
        (self.spec.operations.launch_controls)(home, &mut spec)?;
        Ok(spec)
    }
    fn session_upload_disposition(&self, input: &ReconcileInput<'_>) -> SessionUploadDisposition {
        if !input.options.session_upload_enabled { return SessionUploadDisposition::Disabled; }
        match self.spec.session_upload {
            Feature::Supported(_) => SessionUploadDisposition::Installed,
            Feature::Unsupported(reason) => SessionUploadDisposition::Unsupported { reason: reason.into() },
        }
    }
    fn session_resume_capability(&self) -> Feature<()> { self.spec.session_resume }
    fn native_migration_needs_review(&self, path: &Path, policy: &HarnessPolicy) -> bool { (self.spec.operations.native_migration_needs_review)(path, policy) }
    fn paths(&self, home: &Path) -> ImplementationPaths { (self.spec.operations.paths)(home) }
    fn plan(&self, input: &ReconcileInput<'_>, packages: &ResolvedPackages) -> Result<ReconcilePlan, GhError> {
        (self.spec.operations.plan)(self, input, packages)
    }
    fn gateway_wiring(&self, gateway: &GatewayConfig) -> Result<GatewayWiring, GhError> { (self.spec.operations.gateway_wiring)(gateway) }
    fn validate_components(&self, adapter: &PackageAdapter) -> Result<(), GhError> { (self.spec.operations.validate_components)(adapter) }
    fn validate_staged_components(&self, adapter: &PackageAdapter, root: &Path) -> Result<(), GhError> { (self.spec.operations.validate_staged_components)(adapter, root) }
    fn package_components(&self, packages: &ResolvedPackages) -> PackageComponents { (self.spec.operations.package_components)(packages) }
    fn install_plan(&self, _: &VersionInterval, requirement: Option<&str>) -> InstallInvocation { (self.spec.operations.install_plan)(requirement) }
    fn inspect(&self, policy: &HarnessPolicy, files: &[PathBuf]) -> std::collections::BTreeMap<String, String> {
        let mut values = (self.spec.operations.inspect)(policy, files);
        if matches!(self.spec.session_upload, Feature::Unsupported(_)) && values.contains_key("session_upload_hook") { values.insert("session_upload_hook".into(), "unsupported".into()); }
        values
    }
    fn proposed_values(&self, policy: &HarnessPolicy, gateway: Option<&GatewayConfig>, options: WriteOptions, current: std::collections::BTreeMap<String, String>) -> std::collections::BTreeMap<String, String> {
        let mut values = (self.spec.operations.proposed_values)(policy, gateway, options, current);
        if options.session_upload_enabled && matches!(self.spec.session_upload, Feature::Unsupported(_)) { values.insert("session_upload_hook".into(), "unsupported".into()); }
        values
    }
    fn transcript_path(&self, definition: &HarnessDefinition, home: &Path, session_id: &str, payload: &serde_json::Value) -> Result<PathBuf, GhError> {
        (self.spec.operations.transcript_path)(definition, home, session_id, payload)
    }
}

fn plan_v1(implementation: &Implementation, input: &ReconcileInput<'_>, packages: &ResolvedPackages) -> Result<ReconcilePlan, GhError> {
        let paths = implementation.paths(input.home);
        for path in paths.read_only_sources.iter().chain(&paths.owned_outputs).chain(&paths.native_migrations) { crate::validate_home_path(input.home, path, false)?; }
        let mut plan = ReconcilePlan { owned_paths: paths.owned_outputs, ..Default::default() };
        let components = implementation.package_components(packages);
        let mut report = execute(implementation, &mut plan, input, &components)?;
        let mut launch = crate::HarnessLaunchSpec { env: report.env, launch_args: report.launch_args };
        (implementation.spec.operations.launch_controls)(input.home, &mut launch)?;
        report.env = launch.env; report.launch_args = launch.launch_args;
        plan.files = report.files; plan.env = report.env; plan.launch_args = report.launch_args; plan.warnings = report.warnings; plan.normalize();
        Ok(plan)
}

fn launch(home: &Path, wiring: Option<&GatewayWiring>, mut spec: crate::HarnessLaunchSpec) -> Result<crate::HarnessLaunchSpec, GhError> {
    if !home.join(".codex/blue.config.toml").is_file() { return Err(GhError::config("managed Codex profile is missing")); }
    spec.env.retain(|key, _| !key.starts_with("HARNESS_CODEX_PLUGIN_"));
    if let Some(wiring) = wiring { spec.env.insert(gh_gateway::CODEX_ENV_KEY.into(), wiring.token.clone()); }
    spec.launch_args.splice(0..0, ["--profile".to_owned(), "blue".to_owned()]); Ok(spec)
}

fn disable_update_checks(_: &Path, spec: &mut crate::HarnessLaunchSpec) -> Result<(), GhError> {
    let insert_at = spec
        .launch_args
        .windows(2)
        .position(|pair| pair == ["--profile", "blue"])
        .map(|index| index + 2)
        .unwrap_or(0);
    spec.launch_args.splice(
        insert_at..insert_at,
        ["--config".to_owned(), UPDATE_CHECK_OVERRIDE.to_owned()],
    );
    Ok(())
}

fn paths(home: &Path) -> ImplementationPaths {
    let mut sources = vec![home.join(".codex/config.toml")];
    if let Ok(entries) = std::fs::read_dir(home.join(".codex")) {
        sources.extend(entries.filter_map(Result::ok).filter(|entry| entry.file_name().to_str().is_some_and(|name| name.starts_with("config.toml.bak.harness."))).map(|entry| entry.path()));
    }
    ImplementationPaths {
        read_only_sources: sources,
        owned_outputs: vec![home.join(".codex/blue.config.toml"), crate::managed_runtime_dir(home).join("codex"), home.join(".codex/plugins/cache/governance-blue-managed-standalone-skills/blue-managed-standalone-skills")],
        native_migrations: vec![home.join(".codex/config.toml")],
    }
}

fn native_migration_needs_review(path: &Path, policy: &HarnessPolicy) -> bool { std::fs::read_to_string(path).is_ok_and(|body| body.contains("governed") || body.contains("session-upload") || policy.managed_config.model.as_ref().is_some_and(|model| body.contains(model))) }
fn gateway_wiring(gateway: &GatewayConfig) -> Result<GatewayWiring, GhError> { gh_gateway::wire_with(gateway, AuthPlacement::EnvVar(gh_gateway::CODEX_ENV_KEY.to_owned()), Some("responses")) }
fn validate_components(adapter: &PackageAdapter) -> Result<(), GhError> { if adapter.plugin_dir.is_none() && (adapter.agents_dir.is_some() || adapter.hooks_file.is_some()) { Err(GhError::config("agents/hooks must be wrapped by plugin_dir")) } else { Ok(()) } }

fn validate_staged_components(adapter: &PackageAdapter, root: &Path) -> Result<(), GhError> {
    if let Some(plugin_dir) = &adapter.plugin_dir {
        let manifest = crate::packages::checked_join(root, plugin_dir)?.join(".codex-plugin/plugin.json");
        let document: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest).map_err(|source| GhError::Io { path: manifest.clone(), source })?).map_err(|error| GhError::config(format!("parsing Codex plugin {}: {error}", manifest.display())))?;
        if document.get("name").and_then(serde_json::Value::as_str).is_none() { return Err(GhError::config(format!("Codex plugin {} has no name", manifest.display()))); }
    }
    Ok(())
}

fn package_components(packages: &ResolvedPackages) -> PackageComponents {
    let mut components = PackageComponents::default();
    for package in &packages.groups {
        let mut package = package.clone();
        if let Some(plugin) = &package.plugin { components.env.insert(format!("HARNESS_CODEX_PLUGIN_{}", crate::packages::env_key(&package.id)), plugin.display().to_string()); package.skills.clear(); }
        components.append(&package);
    }
    components
}
fn install_plan(requirement: Option<&str>) -> InstallInvocation { npm_install_plan("@openai/codex", requirement) }
fn inspect(policy: &HarnessPolicy, files: &[PathBuf]) -> std::collections::BTreeMap<String, String> { inspection::codex_current(policy, files) }
fn proposed_values(policy: &HarnessPolicy, gateway: Option<&GatewayConfig>, options: WriteOptions, current: std::collections::BTreeMap<String, String>) -> std::collections::BTreeMap<String, String> { inspection::codex_proposed(current, policy, gateway, options) }

fn execute(implementation: &Implementation, plan: &mut ReconcilePlan, input: &ReconcileInput<'_>, p: &PackageComponents) -> Result<HarnessWrite, GhError> {
    let disposition = implementation.session_upload_disposition(input);
    let hook = match implementation.spec.session_upload { Feature::Supported(render) if input.options.session_upload_enabled => Some(render(input.interval.profile)?), _ => None };
    let session_start = match implementation.spec.session_start { Feature::Supported(render) if input.options.session_upload_enabled => Some(render(input.interval.profile)?), _ => None };
    let mut report = writer::write(plan, input.home, input.policy, input.gateway, hook, session_start)?;
    report.env.extend(p.env.clone()); report.launch_args.extend(p.launch_args.clone()); apply_session_upload_disposition(disposition, &mut report);
    writer::apply_packages(plan, input.home, &mut report, &p.skills_dirs, &p.agents_dirs, &p.hooks_files, &p.helpers)?;
    Ok(report)
}
