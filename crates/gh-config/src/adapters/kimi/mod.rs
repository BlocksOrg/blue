use super::*;

mod inspection;
mod writer;
pub mod v0_0_0;

pub const VERSION_PROBES: &[VersionProbe] = DEFAULT_VERSION_PROBES;
type HookRenderer = fn(&str) -> Result<Vec<toml::Value>, GhError>;
type TranscriptResolver = fn(&Path, &str, &serde_json::Value) -> Result<PathBuf, GhError>;
type SessionFilePreparer = fn(&str, Vec<u8>) -> Result<Vec<u8>, GhError>;
type SessionFileComparator = fn(&str, &[u8], &[u8]) -> bool;
const DISABLE_AUTO_UPDATE_ENV: &str = "KIMI_CODE_NO_AUTO_UPDATE";

#[derive(Debug)]
pub struct Operations {
    plan: fn(&Implementation, &ReconcileInput<'_>, &ResolvedPackages) -> Result<ReconcilePlan, GhError>,
    launch: fn(&Path, crate::HarnessLaunchSpec) -> Result<crate::HarnessLaunchSpec, GhError>,
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
    transcript_path: TranscriptResolver,
    prepare_session_file: SessionFilePreparer,
    session_file_equivalent: SessionFileComparator,
}

#[derive(Debug)]
pub struct VersionSpec { pub operations: &'static Operations, pub session_upload: Feature<HookRenderer>, pub session_start: Feature<HookRenderer>, pub session_resume: Feature<()>, pub support: &'static GenerationSupport }
#[derive(Debug)]
pub struct Implementation { spec: &'static VersionSpec }
impl Implementation { pub const fn new(spec: &'static VersionSpec) -> Self { Self { spec } } }

pub static V1_OPERATIONS: Operations = Operations { plan: plan_v1, launch, launch_controls: disable_auto_updates, paths, native_migration_needs_review, gateway_wiring, validate_components, validate_staged_components, package_components: collect_package_components, install_plan, inspect, proposed_values, transcript_path, prepare_session_file, session_file_equivalent };
pub static IMPLEMENTATIONS: &[ImplementationRegistration] = &[ImplementationRegistration {
    interval: VersionInterval { profile: "kimi-v0_0_0", aliases: &["kimi-v1"], introduced: "0.0.0", before: None, verified_before: "0.39.2-0", lifecycle: ImplementationLifecycle::Supported },
    implementation: &v0_0_0::IMPLEMENTATION,
}];

pub(crate) fn session_upload_hook(profile: &str) -> Result<toml::Value, GhError> {
    let mut hook = toml::map::Map::new();
    hook.insert("event".into(), toml::Value::String("SessionEnd".into()));
    hook.insert("command".into(), toml::Value::String(crate::util::session_upload_command(Harness::Kimi, profile)?));
    hook.insert("timeout".into(), toml::Value::Integer(30));
    Ok(toml::Value::Table(hook))
}
pub(crate) fn session_upload_hooks(profile: &str) -> Result<Vec<toml::Value>, GhError> {
    let session_end = session_upload_hook(profile)?;
    let mut stop = session_end.clone();
    stop.as_table_mut().expect("session upload hook is always a table").insert("event".into(), toml::Value::String("Stop".into()));
    Ok(vec![session_end, stop])
}

pub(crate) fn session_start_hooks(profile: &str) -> Result<Vec<toml::Value>, GhError> {
    let mut hook = toml::map::Map::new();
    hook.insert("event".into(), toml::Value::String("SessionStart".into()));
    hook.insert("command".into(), toml::Value::String(crate::util::session_start_command(Harness::Kimi, profile)?));
    hook.insert("timeout".into(), toml::Value::Integer(30));
    Ok(vec![toml::Value::Table(hook)])
}

impl HarnessImplementation for Implementation {
    fn support(&self) -> &'static GenerationSupport { self.spec.support }
    fn launch(&self, home: &Path, _: Option<&GatewayWiring>, spec: crate::HarnessLaunchSpec) -> Result<crate::HarnessLaunchSpec, GhError> {
        let mut spec = (self.spec.operations.launch)(home, spec)?;
        (self.spec.operations.launch_controls)(home, &mut spec)?;
        Ok(spec)
    }
    fn session_upload_disposition(&self, input: &ReconcileInput<'_>) -> SessionUploadDisposition {
        if !input.options.session_upload_enabled { return SessionUploadDisposition::Disabled; }
        match self.spec.session_upload { Feature::Supported(_) => SessionUploadDisposition::Installed, Feature::Unsupported(reason) => SessionUploadDisposition::Unsupported { reason: reason.into() } }
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
    fn inspect(&self, policy: &HarnessPolicy, files: &[PathBuf]) -> std::collections::BTreeMap<String, String> { (self.spec.operations.inspect)(policy, files) }
    fn proposed_values(&self, policy: &HarnessPolicy, gateway: Option<&GatewayConfig>, options: WriteOptions, current: std::collections::BTreeMap<String, String>) -> std::collections::BTreeMap<String, String> { (self.spec.operations.proposed_values)(policy, gateway, options, current) }
    fn transcript_path(&self, _: &HarnessDefinition, home: &Path, session_id: &str, payload: &serde_json::Value) -> Result<PathBuf, GhError> { (self.spec.operations.transcript_path)(home, session_id, payload) }
    fn prepare_session_file(&self, role: &str, bytes: Vec<u8>) -> Result<Vec<u8>, GhError> { (self.spec.operations.prepare_session_file)(role, bytes) }
    fn session_file_equivalent(&self, role: &str, existing: &[u8], bundled: &[u8]) -> bool { (self.spec.operations.session_file_equivalent)(role, existing, bundled) }
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
        plan.files = report.files; plan.env = report.env; plan.launch_args = report.launch_args; plan.warnings = report.warnings; plan.normalize(); Ok(plan)
}

fn launch(home: &Path, mut spec: crate::HarnessLaunchSpec) -> Result<crate::HarnessLaunchSpec, GhError> {
    let runtime = home.join(".config/blue/runtime/kimi");
    if !runtime.join("config.toml").is_file() {
        return Err(GhError::config("managed Kimi configuration is missing"));
    }
    spec.env
        .insert("KIMI_CODE_HOME".into(), runtime.display().to_string());
    Ok(spec)
}
fn disable_auto_updates(_: &Path, spec: &mut crate::HarnessLaunchSpec) -> Result<(), GhError> {
    spec.env
        .insert(DISABLE_AUTO_UPDATE_ENV.into(), "1".into());
    Ok(())
}
fn paths(home: &Path) -> ImplementationPaths { ImplementationPaths { read_only_sources: vec![home.join(".kimi-code/config.toml"), home.join(".kimi-code/mcp.json")], owned_outputs: vec![home.join(".config/blue/runtime/kimi")], native_migrations: vec![home.join(".kimi-code/config.toml")] } }
fn native_migration_needs_review(path: &Path, policy: &HarnessPolicy) -> bool { std::fs::read_to_string(path).is_ok_and(|body| body.contains("governed") || body.contains("session-upload") || policy.managed_config.model.as_ref().is_some_and(|model| body.contains(model))) }
fn gateway_wiring(gateway: &GatewayConfig) -> Result<GatewayWiring, GhError> { gh_gateway::wire_with(gateway, AuthPlacement::InFile, Some("openai")) }
fn validate_components(_: &PackageAdapter) -> Result<(), GhError> { Ok(()) }
fn validate_staged_components(adapter: &PackageAdapter, root: &Path) -> Result<(), GhError> {
    if let Some(hooks_file) = &adapter.hooks_file {
        let path = crate::packages::checked_join(root, hooks_file)?;
        let document = std::fs::read_to_string(&path).map_err(|source| GhError::Io { path: path.clone(), source })?;
        let table = document.parse::<toml::Value>().map_err(|error| GhError::config(format!("parsing Kimi hooks {}: {error}", path.display())))?;
        if table.get("hooks").and_then(toml::Value::as_array).is_none() { return Err(GhError::config(format!("Kimi hooks {} must contain a hooks array", path.display()))); }
    }
    Ok(())
}
fn install_plan(requirement: Option<&str>) -> InstallInvocation { npm_install_plan("@moonshot-ai/kimi-code", requirement) }
fn inspect(policy: &HarnessPolicy, files: &[PathBuf]) -> std::collections::BTreeMap<String, String> { inspection::kimi_current(policy, files) }
fn proposed_values(policy: &HarnessPolicy, gateway: Option<&GatewayConfig>, options: WriteOptions, current: std::collections::BTreeMap<String, String>) -> std::collections::BTreeMap<String, String> { inspection::kimi_proposed(current, policy, gateway, options) }

fn sanitize_session_state(value: &mut serde_json::Value) {
    fn excluded(key: &str) -> bool {
        let key = key.to_ascii_lowercase();
        key.contains("credential")
            || key.contains("secret")
            || key.contains("approval")
            || key.contains("background_process")
            || key.contains("cron")
            || key.contains("queued_goal")
            || key.contains("future_goal")
            || matches!(
                key.as_str(),
                "token" | "access_token" | "refresh_token" | "api_key" | "processes"
            )
    }
    match value {
        serde_json::Value::Object(object) => {
            object.retain(|key, _| !excluded(key));
            object.values_mut().for_each(sanitize_session_state);
        }
        serde_json::Value::Array(values) => values.iter_mut().for_each(sanitize_session_state),
        _ => {}
    }
}

fn prepare_session_file(role: &str, bytes: Vec<u8>) -> Result<Vec<u8>, GhError> {
    if role != "kimi_state" {
        return Ok(bytes);
    }
    let mut value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| GhError::config(format!("invalid Kimi state JSON: {error}")))?;
    sanitize_session_state(&mut value);
    serde_json::to_vec_pretty(&value).map_err(|error| GhError::Serde(error.to_string()))
}

fn session_file_equivalent(role: &str, existing: &[u8], bundled: &[u8]) -> bool {
    existing == bundled
        || (role == "kimi_state"
            && prepare_session_file(role, existing.to_vec())
                .is_ok_and(|sanitized| sanitized == bundled))
}

fn transcript_path(home: &Path, session_id: &str, payload: &serde_json::Value) -> Result<PathBuf, GhError> {
    if let Some(path) = payload.get("transcript_path").and_then(serde_json::Value::as_str).filter(|path| !path.is_empty()) { return Ok(PathBuf::from(path)); }
    for root in [home.join(".config/blue/runtime/kimi/sessions"), home.join(".kimi-code/sessions")] { if let Some(transcript) = find_transcript(&root, session_id, 0)? { return Ok(transcript); } }
    Err(GhError::config("Kimi hook payload has no usable transcript_path in its managed or native session roots"))
}
fn find_transcript(root: &Path, session_id: &str, depth: usize) -> Result<Option<PathBuf>, GhError> {
    if depth > 4 || !root.is_dir() { return Ok(None); }
    for entry in std::fs::read_dir(root).map_err(|source| GhError::Io { path: root.to_path_buf(), source })? {
        let entry = entry.map_err(|source| GhError::Io { path: root.to_path_buf(), source })?;
        let file_type = entry.file_type().map_err(|source| GhError::Io { path: entry.path(), source })?;
        if !file_type.is_dir() || file_type.is_symlink() { continue; }
        let path = entry.path();
        if entry.file_name() == session_id { let transcript = path.join("agents/main/wire.jsonl"); if transcript.is_file() { return Ok(Some(transcript)); } }
        if let Some(found) = find_transcript(&path, session_id, depth + 1)? { return Ok(Some(found)); }
    }
    Ok(None)
}
fn execute(implementation: &Implementation, plan: &mut ReconcilePlan, input: &ReconcileInput<'_>, p: &PackageComponents) -> Result<HarnessWrite, GhError> {
    let disposition = implementation.session_upload_disposition(input);
    let hooks = match implementation.spec.session_upload { Feature::Supported(render) if input.options.session_upload_enabled => Some(render(input.interval.profile)?), _ => None };
    let session_start = match implementation.spec.session_start { Feature::Supported(render) if input.options.session_upload_enabled => Some(render(input.interval.profile)?), _ => None };
    let mut report = writer::write(plan, input.home, input.policy, input.gateway, hooks, session_start)?;
    report.env.extend(p.env.clone()); report.launch_args.extend(p.launch_args.clone()); apply_session_upload_disposition(disposition, &mut report);
    writer::apply_packages(plan, input.home, &mut report, &p.skills_dirs, &p.agents_dirs, &p.hooks_files, &p.helpers)?; Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versioned_session_state_policy_sanitizes_and_compares() {
        let implementation = &v0_0_0::IMPLEMENTATION;
        let bundled = implementation
            .prepare_session_file(
                "kimi_state",
                br#"{"conversation":{"ready":true,"token":"secret"},"approvals":[1],"cron_jobs":[2]}"#
                    .to_vec(),
            )
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bundled).unwrap();
        assert_eq!(value, serde_json::json!({"conversation": {"ready": true}}));

        let local = br#"{"conversation":{"ready":true},"access_token":"local-secret","approvals":["still-needed"]}"#;
        assert!(implementation.session_file_equivalent("kimi_state", local, &bundled));
        assert!(!implementation.session_file_equivalent(
            "kimi_state",
            br#"{"conversation":{"ready":false},"access_token":"local-secret"}"#,
            &bundled,
        ));

        let unchanged = br#"{"token":"not-special-for-another-role"}"#.to_vec();
        assert_eq!(
            implementation
                .prepare_session_file("session_state", unchanged.clone())
                .unwrap(),
            unchanged
        );
    }

    #[test]
    fn resolves_transcript_from_managed_kimi_code_home() {
        let home = std::env::temp_dir().join(format!("blue-kimi-transcript-{}", std::process::id()));
        let transcript = home.join(".config/blue/runtime/kimi/sessions/project/session-1/agents/main/wire.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap(); std::fs::write(&transcript, "{}\n").unwrap();
        let resolved = v0_0_0::IMPLEMENTATION.transcript_path(definition(Harness::Kimi), &home, "session-1", &serde_json::json!({})).unwrap();
        assert_eq!(resolved, transcript); let _ = std::fs::remove_dir_all(home);
    }
}
