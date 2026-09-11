use super::*;

mod inspection;
mod writer;
pub mod v0_0_0;

pub const VERSION_PROBES: &[VersionProbe] = DEFAULT_VERSION_PROBES;
const DISABLE_AUTOUPDATE_ENV: &str = "OPENCODE_DISABLE_AUTOUPDATE";
type PluginRenderer = fn(&str) -> Result<String, GhError>;

#[derive(Debug)]
pub struct Operations {
    plan: fn(&Implementation, &ReconcileInput<'_>, &ResolvedPackages) -> Result<ReconcilePlan, GhError>,
    launch: fn(&Path, Option<&GatewayWiring>, crate::HarnessLaunchSpec) -> Result<crate::HarnessLaunchSpec, GhError>,
    launch_controls: fn(&Path, &HarnessPolicy, &mut crate::HarnessLaunchSpec) -> Result<(), GhError>,
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
pub struct VersionSpec { pub operations: &'static Operations, pub session_upload: Feature<PluginRenderer>, pub session_start: Feature<()>, pub session_resume: Feature<()>, pub support: &'static GenerationSupport }
#[derive(Debug)]
pub struct Implementation { spec: &'static VersionSpec }
impl Implementation { pub const fn new(spec: &'static VersionSpec) -> Self { Self { spec } } }

pub static V1_OPERATIONS: Operations = Operations { plan: plan_v1, launch, launch_controls: disable_auto_updates, paths, native_migration_needs_review, gateway_wiring, validate_components, validate_staged_components: accept_staged_components, package_components: collect_package_components, install_plan, inspect, proposed_values, transcript_path: payload_transcript_path };
pub static IMPLEMENTATIONS: &[ImplementationRegistration] = &[ImplementationRegistration {
    interval: VersionInterval { profile: "opencode-v0_0_0", aliases: &["opencode-v1"], introduced: "0.0.0", before: None, verified_before: "1.18.26-0", lifecycle: ImplementationLifecycle::Supported },
    implementation: &v0_0_0::IMPLEMENTATION,
}];

pub(crate) fn session_upload_plugin(profile: &str) -> Result<String, GhError> {
    let exe = serde_json::to_string(&crate::util::harness_executable()?.to_string_lossy().as_ref()).map_err(|error| GhError::Serde(error.to_string()))?;
    let profile = serde_json::to_string(profile).map_err(|error| GhError::Serde(error.to_string()))?;
    Ok(SESSION_PLUGIN_TEMPLATE
        .replace("__BLUE_EXE__", &exe)
        .replace("__BLUE_PROFILE__", &profile))
}

// OpenCode has no native SessionStart/SessionEnd hook, so the managed plugin
// synthesizes both. On the first event carrying a top-level `sessionID` it runs
// `blue session-start` to record the BLUE_SESSION_ID → native-session mapping;
// on idle it uploads. When BLUE_SESSION_ID is set the transcript is written to a
// stable path under the Blue data dir (and NOT deleted) so `blue run`'s
// post-exit fallback can upload it even if `session.idle` never fired.
const SESSION_PLUGIN_TEMPLATE: &str = r#"// Managed by Blue. Changes will be overwritten.
const BLUE_SESSION_ID = process.env.BLUE_SESSION_ID
const BLUE_DATA_DIR = process.env.BLUE_DATA_DIR
const blueTranscriptPath = (sessionID) =>
  BLUE_SESSION_ID && BLUE_DATA_DIR
    ? `${BLUE_DATA_DIR}/sessions/${BLUE_SESSION_ID}/opencode-transcript-${sessionID}.json`
    : `${process.env.TMPDIR || "/tmp"}/blue-opencode-${sessionID}.json`

export const BlueSessionUpload = async ({ client, directory }) => {
  const uploaded = new Set()
  const started = new Set()
  const ensureStarted = async (sessionID) => {
    if (!sessionID || started.has(sessionID)) return
    started.add(sessionID)
    try {
      const session = await client.session.get({ path: { id: sessionID } })
      if (session.data?.parentID) return
      const child = Bun.spawn([__BLUE_EXE__, "session-start", "opencode", "--profile", __BLUE_PROFILE__], {
        stdin: "pipe", stdout: "ignore", stderr: "inherit",
      })
      child.stdin.write(JSON.stringify({
        session_id: sessionID, source: "startup", cwd: directory,
        hook_event_name: "SessionStart", transcript_path: blueTranscriptPath(sessionID),
      }))
      child.stdin.end()
      await child.exited
    } catch (error) {
      started.delete(sessionID)
      console.error("Blue session start failed", error)
    }
  }
  return { event: async ({ event }) => {
    const sessionID = event.properties?.sessionID ?? event.properties?.info?.id
    await ensureStarted(sessionID)
    const idle = event.type === "session.idle" ||
      (event.type === "session.status" && event.properties?.status?.type === "idle")
    if (!idle) return
    if (!sessionID || uploaded.has(sessionID)) return
    uploaded.add(sessionID)
    let transcriptPath
    try {
      const [session, messages] = await Promise.all([
        client.session.get({ path: { id: sessionID } }),
        client.session.messages({ path: { id: sessionID } }),
      ])
      if (session.data?.parentID) return
      transcriptPath = blueTranscriptPath(sessionID)
      // Match `opencode export` exactly so restoration can use the native
      // `opencode import` contract without translating private database state.
      await Bun.write(transcriptPath, JSON.stringify({ info: session.data, messages: messages.data }))
      const child = Bun.spawn([__BLUE_EXE__, "session-upload", "opencode", "--profile", __BLUE_PROFILE__], {
        stdin: "pipe", stdout: "ignore", stderr: "inherit",
      })
      child.stdin.write(JSON.stringify({
        session_id: sessionID, transcript_path: transcriptPath, cwd: directory,
        hook_event_name: "SessionEnd", reason: "idle",
      }))
      child.stdin.end()
      await child.exited
    } catch (error) {
      uploaded.delete(sessionID)
      console.error("Blue session upload failed", error)
    } finally {
      if (transcriptPath && !BLUE_SESSION_ID) await Bun.file(transcriptPath).delete().catch(() => {})
    }
  } }
}
"#;

impl HarnessImplementation for Implementation {
    fn support(&self) -> &'static GenerationSupport { self.spec.support }
    fn launch(&self, home: &Path, wiring: Option<&GatewayWiring>, policy: &HarnessPolicy, spec: crate::HarnessLaunchSpec) -> Result<crate::HarnessLaunchSpec, GhError> {
        let mut spec = (self.spec.operations.launch)(home, wiring, spec)?;
        (self.spec.operations.launch_controls)(home, policy, &mut spec)?;
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
    fn transcript_path(&self, definition: &HarnessDefinition, home: &Path, session_id: &str, payload: &serde_json::Value) -> Result<PathBuf, GhError> { (self.spec.operations.transcript_path)(definition, home, session_id, payload) }
}

fn plan_v1(implementation: &Implementation, input: &ReconcileInput<'_>, packages: &ResolvedPackages) -> Result<ReconcilePlan, GhError> {
        let paths = implementation.paths(input.home);
        for path in paths.read_only_sources.iter().chain(&paths.owned_outputs).chain(&paths.native_migrations) { crate::validate_home_path(input.home, path, false)?; }
        let mut plan = ReconcilePlan { owned_paths: paths.owned_outputs, ..Default::default() };
        let components = implementation.package_components(packages);
        let mut report = execute(implementation, &mut plan, input, &components)?;
        let mut launch = crate::HarnessLaunchSpec { env: report.env, launch_args: report.launch_args };
        (implementation.spec.operations.launch_controls)(input.home, input.policy, &mut launch)?;
        report.env = launch.env; report.launch_args = launch.launch_args;
        plan.files = report.files; plan.env = report.env; plan.launch_args = report.launch_args; plan.warnings = report.warnings; plan.normalize(); Ok(plan)
}

fn launch(home: &Path, wiring: Option<&GatewayWiring>, mut spec: crate::HarnessLaunchSpec) -> Result<crate::HarnessLaunchSpec, GhError> {
    let runtime = crate::managed_runtime_dir(home).join("opencode"); let config = runtime.join("opencode.json");
    let contents = std::fs::read_to_string(&config).map_err(|source| GhError::Io { path: config, source })?;
    spec.env.insert("OPENCODE_CONFIG_CONTENT".into(), contents); spec.env.insert("OPENCODE_CONFIG_DIR".into(), runtime.display().to_string());
    if let Some(wiring) = wiring { spec.env.insert("BLUE_OPENCODE_GATEWAY_TOKEN".into(), wiring.token.clone()); }
    Ok(spec)
}
// OpenCode's updater reads `autoupdate` from `Config.getGlobal()`, which only
// merges `$HOME/.config/opencode/{config,opencode}.json{,c}`. Config supplied
// through OPENCODE_CONFIG_CONTENT lands in the per-directory "local" layer and
// OPENCODE_CONFIG_DIR in its own layer, so neither reaches that check and the
// managed `autoupdate: false` is silently ignored. The env var is read straight
// off the process environment, so it is the only reliable suppression for a
// launch Blue does not own the global config of. Keep the config key too: it
// still governs the in-session update notice.
fn disable_auto_updates(_: &Path, policy: &HarnessPolicy, spec: &mut crate::HarnessLaunchSpec) -> Result<(), GhError> {
    if !crate::compat::policy_requires_update_suppression(policy)? {
        return Ok(());
    }
    let contents = spec
        .env
        .get("OPENCODE_CONFIG_CONTENT")
        .ok_or_else(|| GhError::config("managed OpenCode launch config is missing"))?;
    let contents = writer::disable_autoupdate(contents)?;
    spec.env.insert("OPENCODE_CONFIG_CONTENT".into(), contents);
    spec.env.insert(DISABLE_AUTOUPDATE_ENV.into(), "1".into());
    Ok(())
}
fn paths(home: &Path) -> ImplementationPaths {
    let runtime = crate::managed_runtime_dir(home).join("opencode");
    ImplementationPaths {
        read_only_sources: vec![home.join(".config/opencode/opencode.json"), home.join(".local/share/opencode/auth.json"), home.join(".config/opencode/plugins/blue-session-upload.js")],
        owned_outputs: vec![runtime.join("opencode.json"), runtime.join("compatibility-state.json"), runtime.join("plugins"), runtime.join("skills"), runtime.join("agents"), runtime.join("hooks")],
        native_migrations: vec![home.join(".config/opencode/opencode.json"), home.join(".local/share/opencode/auth.json"), home.join(".config/opencode/plugins/blue-session-upload.js")],
    }
}
fn native_migration_needs_review(path: &Path, policy: &HarnessPolicy) -> bool { std::fs::read_to_string(path).is_ok_and(|body| body.contains("governed") || body.contains("session-upload") || policy.managed_config.model.as_ref().is_some_and(|model| body.contains(model))) }
fn gateway_wiring(gateway: &GatewayConfig) -> Result<GatewayWiring, GhError> { gh_gateway::wire_with(gateway, AuthPlacement::InFile, None) }
fn validate_components(adapter: &PackageAdapter) -> Result<(), GhError> { if adapter.hooks_file.is_some() { Err(GhError::config("hooks must be declared as plugin modules")) } else { Ok(()) } }
fn install_plan(requirement: Option<&str>) -> InstallInvocation { npm_install_plan("opencode-ai", requirement) }
fn inspect(policy: &HarnessPolicy, files: &[PathBuf]) -> std::collections::BTreeMap<String, String> { inspection::opencode_current(policy, files) }
fn proposed_values(policy: &HarnessPolicy, gateway: Option<&GatewayConfig>, options: WriteOptions, current: std::collections::BTreeMap<String, String>) -> std::collections::BTreeMap<String, String> { inspection::opencode_proposed(current, policy, gateway, options) }

fn execute(implementation: &Implementation, plan: &mut ReconcilePlan, input: &ReconcileInput<'_>, p: &PackageComponents) -> Result<HarnessWrite, GhError> {
    let disposition = implementation.session_upload_disposition(input);
    let plugin = match implementation.spec.session_upload { Feature::Supported(render) if input.options.session_upload_enabled => Some(render(input.interval.profile)?), _ => None };
    let mut report = writer::write(plan, input.home, input.policy, input.gateway, plugin)?;
    report.env.extend(p.env.clone()); report.launch_args.extend(p.launch_args.clone()); apply_session_upload_disposition(disposition, &mut report);
    writer::apply_packages(plan, input.home, &mut report, &p.plugin_modules, &p.skills_dirs, &p.agents_dirs, &p.hooks_files, &p.helpers)?; Ok(report)
}
