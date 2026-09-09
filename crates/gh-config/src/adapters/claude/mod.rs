use super::*;
use serde_json::json;

mod inspection;
mod writer;
pub mod v0_0_0;
pub mod v1_0_38;
pub mod v2_0_12;

pub const VERSION_PROBES: &[VersionProbe] = DEFAULT_VERSION_PROBES;
type HookRenderer = fn(&str) -> Result<serde_json::Value, GhError>;
const DISABLE_AUTOUPDATER_ENV: &str = "DISABLE_AUTOUPDATER";

#[derive(Debug)]
pub struct Operations {
    plan: fn(&Implementation, &ReconcileInput<'_>, &ResolvedPackages) -> Result<ReconcilePlan, GhError>,
    launch: fn(&Path, crate::HarnessLaunchSpec) -> Result<crate::HarnessLaunchSpec, GhError>,
    launch_controls: fn(&Path, &mut crate::HarnessLaunchSpec) -> Result<(), GhError>,
    paths: fn(&Path) -> ImplementationPaths,
    native_migration_needs_review: fn(&Path, &HarnessPolicy) -> bool,
    gateway_wiring: fn(&GatewayConfig) -> Result<GatewayWiring, GhError>,
    validate_components: fn(&PackageAdapter) -> Result<(), GhError>,
    validate_resolved_packages: fn(&ResolvedPackages) -> Result<(), GhError>,
    validate_staged_components: fn(&PackageAdapter, &Path) -> Result<(), GhError>,
    package_components: fn(&ResolvedPackages) -> PackageComponents,
    install_plan: fn(Option<&str>) -> InstallInvocation,
    inspect: InspectOperation,
    proposed_values: ProposedValuesOperation,
    transcript_path: TranscriptOperation,
    apply_packages: fn(&mut ReconcilePlan, &Path, &mut HarnessWrite, &PackageComponents) -> Result<(), GhError>,
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
pub struct Implementation {
    spec: &'static VersionSpec,
}

impl Implementation {
    pub const fn new(spec: &'static VersionSpec) -> Self {
        Self { spec }
    }
}

pub static PRE_HOOKS_OPERATIONS: Operations = Operations {
    plan,
    launch: launch_base,
    launch_controls: disable_auto_updates,
    paths,
    native_migration_needs_review,
    gateway_wiring,
    validate_components: validate_pre_hooks,
    validate_resolved_packages: validate_resolved_pre_hooks,
    validate_staged_components: accept_staged_components,
    package_components,
    install_plan,
    inspect,
    proposed_values,
    transcript_path: payload_transcript_path,
    apply_packages: apply_base_packages,
};
pub static HOOKS_ONLY_OPERATIONS: Operations = Operations {
    plan,
    launch: launch_base,
    launch_controls: disable_auto_updates,
    paths,
    native_migration_needs_review,
    gateway_wiring,
    validate_components: validate_hooks_only,
    validate_resolved_packages: validate_resolved_hooks_only,
    validate_staged_components: accept_staged_components,
    package_components,
    install_plan,
    inspect,
    proposed_values,
    transcript_path: payload_transcript_path,
    apply_packages: apply_hook_packages,
};
pub static PLUGIN_OPERATIONS: Operations = Operations {
    plan,
    launch: launch_plugins,
    launch_controls: disable_auto_updates,
    paths,
    native_migration_needs_review,
    gateway_wiring,
    validate_components: validate_plugins,
    validate_resolved_packages: validate_resolved_plugins,
    validate_staged_components: accept_staged_components,
    package_components,
    install_plan,
    inspect,
    proposed_values,
    transcript_path: payload_transcript_path,
    apply_packages: apply_plugin_packages,
};

pub static IMPLEMENTATIONS: &[ImplementationRegistration] = &[
    ImplementationRegistration {
        interval: VersionInterval {
            profile: "claude-v0_0_0",
            aliases: &[],
            introduced: "0.0.0",
            before: Some("1.0.38"),
            verified_before: "1.0.38-0",
            lifecycle: ImplementationLifecycle::Supported,
        },
        implementation: &v0_0_0::IMPLEMENTATION,
    },
    ImplementationRegistration {
        interval: VersionInterval {
            profile: "claude-v1_0_38",
            aliases: &[],
            introduced: "1.0.38",
            before: Some("2.0.12"),
            verified_before: "2.0.12-0",
            lifecycle: ImplementationLifecycle::Supported,
        },
        implementation: &v1_0_38::IMPLEMENTATION,
    },
    ImplementationRegistration {
        interval: VersionInterval {
            profile: "claude-v2_0_12",
            aliases: &["claude-v1"],
            introduced: "2.0.12",
            before: None,
            verified_before: "2.1.253-0",
            lifecycle: ImplementationLifecycle::Supported,
        },
        implementation: &v2_0_12::IMPLEMENTATION,
    },
];

pub(crate) fn session_upload_hooks(profile: &str) -> Result<serde_json::Value, GhError> {
    Ok(json!({ "SessionEnd": [{ "hooks": [{ "type": "command", "command": crate::util::session_upload_command(Harness::Claude, profile)?, "timeout": 30 }] }] }))
}

pub(crate) fn session_start_hooks(profile: &str) -> Result<serde_json::Value, GhError> {
    Ok(json!({ "SessionStart": [{ "hooks": [{ "type": "command", "command": crate::util::session_start_command(Harness::Claude, profile)?, "timeout": 30 }] }] }))
}

impl HarnessImplementation for Implementation {
    fn support(&self) -> &'static GenerationSupport {
        self.spec.support
    }
    fn launch(
        &self,
        home: &Path,
        _: Option<&GatewayWiring>,
        spec: crate::HarnessLaunchSpec,
    ) -> Result<crate::HarnessLaunchSpec, GhError> {
        let mut spec = (self.spec.operations.launch)(home, spec)?;
        (self.spec.operations.launch_controls)(home, &mut spec)?;
        Ok(spec)
    }
    fn session_upload_disposition(&self, input: &ReconcileInput<'_>) -> SessionUploadDisposition {
        if !input.options.session_upload_enabled {
            return SessionUploadDisposition::Disabled;
        }
        match self.spec.session_upload {
            Feature::Supported(_) => SessionUploadDisposition::Installed,
            Feature::Unsupported(reason) => {
                SessionUploadDisposition::Unsupported { reason: reason.into() }
            }
        }
    }
    fn session_resume_capability(&self) -> Feature<()> { self.spec.session_resume }
    fn native_migration_needs_review(&self, path: &Path, policy: &HarnessPolicy) -> bool {
        (self.spec.operations.native_migration_needs_review)(path, policy)
    }
    fn paths(&self, home: &Path) -> ImplementationPaths {
        (self.spec.operations.paths)(home)
    }
    fn plan(&self, input: &ReconcileInput<'_>, packages: &ResolvedPackages) -> Result<ReconcilePlan, GhError> {
        (self.spec.operations.plan)(self, input, packages)
    }
    fn gateway_wiring(&self, gateway: &GatewayConfig) -> Result<GatewayWiring, GhError> {
        (self.spec.operations.gateway_wiring)(gateway)
    }
    fn validate_components(&self, adapter: &PackageAdapter) -> Result<(), GhError> {
        (self.spec.operations.validate_components)(adapter)
    }
    fn validate_staged_components(
        &self,
        adapter: &PackageAdapter,
        root: &Path,
    ) -> Result<(), GhError> {
        (self.spec.operations.validate_staged_components)(adapter, root)
    }
    fn package_components(&self, packages: &ResolvedPackages) -> PackageComponents {
        (self.spec.operations.package_components)(packages)
    }
    fn install_plan(
        &self,
        _: &VersionInterval,
        requirement: Option<&str>,
    ) -> InstallInvocation {
        (self.spec.operations.install_plan)(requirement)
    }
    fn inspect(&self, policy: &HarnessPolicy, files: &[PathBuf]) -> std::collections::BTreeMap<String, String> {
        let mut values = (self.spec.operations.inspect)(policy, files);
        if matches!(self.spec.session_upload, Feature::Unsupported(_))
            && values.contains_key("session_upload_hook")
        {
            values.insert("session_upload_hook".into(), "unsupported".into());
        }
        values
    }
    fn proposed_values(
        &self,
        policy: &HarnessPolicy,
        gateway: Option<&GatewayConfig>,
        options: WriteOptions,
        current: std::collections::BTreeMap<String, String>,
    ) -> std::collections::BTreeMap<String, String> {
        let mut values =
            (self.spec.operations.proposed_values)(policy, gateway, options, current);
        if options.session_upload_enabled
            && matches!(self.spec.session_upload, Feature::Unsupported(_))
        {
            values.insert("session_upload_hook".into(), "unsupported".into());
        }
        values
    }
    fn transcript_path(
        &self,
        definition: &HarnessDefinition,
        home: &Path,
        session_id: &str,
        payload: &serde_json::Value,
    ) -> Result<PathBuf, GhError> {
        (self.spec.operations.transcript_path)(definition, home, session_id, payload)
    }
}

fn launch_base(home: &Path, mut spec: crate::HarnessLaunchSpec) -> Result<crate::HarnessLaunchSpec, GhError> {
    let runtime = home.join(".config/blue/runtime/claude");
    let settings = runtime.join("settings.json");
    let mcp = runtime.join("mcp.json");
    if !settings.is_file() || !mcp.is_file() {
        return Err(GhError::config("managed Claude configuration is missing"));
    }
    spec.launch_args.splice(0..0, ["--settings".to_owned(), settings.display().to_string(), "--mcp-config".to_owned(), mcp.display().to_string()]);
    Ok(spec)
}

fn disable_auto_updates(_: &Path, spec: &mut crate::HarnessLaunchSpec) -> Result<(), GhError> {
    spec.env.insert(DISABLE_AUTOUPDATER_ENV.into(), "1".into());
    Ok(())
}

fn launch_plugins(home: &Path, mut spec: crate::HarnessLaunchSpec) -> Result<crate::HarnessLaunchSpec, GhError> {
    spec = launch_base(home, spec)?;
    let standalone = home.join(".config/blue/runtime/claude/standalone-skills-plugin");
    if standalone.is_dir() {
        spec.launch_args
            .extend(["--plugin-dir".to_owned(), standalone.display().to_string()]);
    }
    Ok(spec)
}

fn package_components(packages: &ResolvedPackages) -> PackageComponents {
    let mut components = PackageComponents::default();
    for package in &packages.groups {
        let mut package = package.clone();
        if let Some(plugin) = &package.plugin {
            components
                .launch_args
                .extend(["--plugin-dir".into(), plugin.display().to_string()]);
            package.skills.clear();
        }
        components.append(&package);
    }
    components
}

fn plan(
    implementation: &Implementation,
    input: &ReconcileInput<'_>,
    packages: &ResolvedPackages,
) -> Result<ReconcilePlan, GhError> {
    (implementation.spec.operations.validate_resolved_packages)(packages)?;
    let paths = implementation.paths(input.home);
    for path in paths
        .read_only_sources
        .iter()
        .chain(&paths.owned_outputs)
        .chain(&paths.native_migrations)
    {
        crate::validate_home_path(input.home, path, false)?;
    }
    let mut plan = ReconcilePlan {
        owned_paths: paths.owned_outputs,
        ..Default::default()
    };
    let components = implementation.package_components(packages);
    let mut report = execute(implementation, &mut plan, input, &components)?;
    let mut launch = crate::HarnessLaunchSpec { env: report.env, launch_args: report.launch_args };
    (implementation.spec.operations.launch_controls)(input.home, &mut launch)?;
    report.env = launch.env; report.launch_args = launch.launch_args;
    plan.files = report.files;
    plan.env = report.env;
    plan.launch_args = report.launch_args;
    plan.warnings = report.warnings;
    plan.normalize();
    Ok(plan)
}

fn paths(home: &Path) -> ImplementationPaths {
    ImplementationPaths {
        read_only_sources: vec![home.join(".claude/settings.json"), home.join(".claude.json")],
        owned_outputs: vec![home.join(".config/blue/runtime/claude")],
        native_migrations: vec![home.join(".claude/settings.json")],
    }
}

fn native_migration_needs_review(path: &Path, policy: &HarnessPolicy) -> bool {
    std::fs::read_to_string(path).is_ok_and(|body| {
        body.contains("governed")
            || body.contains("session-upload")
            || body.contains("ANTHROPIC_BASE_URL")
            || body.contains("ANTHROPIC_AUTH_TOKEN")
            || policy
                .managed_config
                .model
                .as_ref()
                .is_some_and(|model| body.contains(model))
    })
}

fn gateway_wiring(gateway: &GatewayConfig) -> Result<GatewayWiring, GhError> {
    gh_gateway::wire_with(gateway, AuthPlacement::InFile, None)
}

fn install_plan(requirement: Option<&str>) -> InstallInvocation {
    npm_install_plan("@anthropic-ai/claude-code", requirement)
}

fn inspect(
    policy: &HarnessPolicy,
    files: &[PathBuf],
) -> std::collections::BTreeMap<String, String> {
    inspection::claude_current(policy, files)
}

fn proposed_values(
    policy: &HarnessPolicy,
    gateway: Option<&GatewayConfig>,
    options: WriteOptions,
    current: std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    inspection::claude_proposed(current, policy, gateway, options)
}

fn has_plugin_components(adapter: &PackageAdapter) -> bool {
    adapter.plugin_dir.is_some()
        || adapter.skills_dir.is_some()
        || adapter.agents_dir.is_some()
        || !adapter.plugins.is_empty()
}

fn validate_pre_hooks(adapter: &PackageAdapter) -> Result<(), GhError> {
    if has_plugin_components(adapter) {
        return Err(GhError::config(
            "Claude before 2.0.12 does not support Blue plugin packaging or standalone skills/agents",
        ));
    }
    if adapter.hooks_file.is_some() {
        return Err(GhError::config(
            "Claude before 1.0.38 does not support hooks",
        ));
    }
    Ok(())
}

fn validate_hooks_only(adapter: &PackageAdapter) -> Result<(), GhError> {
    if has_plugin_components(adapter) {
        Err(GhError::config(
            "Claude before 2.0.12 does not support Blue plugin packaging or standalone skills/agents",
        ))
    } else {
        Ok(())
    }
}

fn validate_plugins(adapter: &PackageAdapter) -> Result<(), GhError> {
    if adapter.plugin_dir.is_none()
        && (adapter.agents_dir.is_some() || adapter.hooks_file.is_some())
    {
        Err(GhError::config(
            "agents/hooks must be wrapped by plugin_dir",
        ))
    } else {
        Ok(())
    }
}

fn resolved_has_plugins(packages: &ResolvedPackages) -> bool {
    packages.groups.iter().any(|package| {
        package.plugin.is_some()
            || !package.skills.is_empty()
            || !package.agents.is_empty()
            || !package.plugin_modules.is_empty()
    })
}

fn validate_resolved_pre_hooks(packages: &ResolvedPackages) -> Result<(), GhError> {
    if resolved_has_plugins(packages)
        || packages
            .groups
            .iter()
            .any(|package| !package.hooks.is_empty())
    {
        Err(GhError::config(
            "package components are unsupported by this Claude interval",
        ))
    } else {
        Ok(())
    }
}

fn validate_resolved_hooks_only(packages: &ResolvedPackages) -> Result<(), GhError> {
    if resolved_has_plugins(packages) {
        Err(GhError::config(
            "package components are unsupported by this Claude interval",
        ))
    } else {
        Ok(())
    }
}

fn validate_resolved_plugins(_: &ResolvedPackages) -> Result<(), GhError> {
    Ok(())
}

fn execute(
    implementation: &Implementation,
    plan: &mut ReconcilePlan,
    input: &ReconcileInput<'_>,
    components: &PackageComponents,
) -> Result<HarnessWrite, GhError> {
    let disposition = implementation.session_upload_disposition(input);
    let hooks = match implementation.spec.session_upload {
        Feature::Supported(render) if input.options.session_upload_enabled => {
            Some(render(input.interval.profile)?)
        }
        _ => None,
    };
    let session_start = match implementation.spec.session_start {
        Feature::Supported(render) if input.options.session_upload_enabled => {
            Some(render(input.interval.profile)?)
        }
        _ => None,
    };
    let mut report = writer::write(
        plan,
        input.home,
        input.policy,
        input.gateway,
        hooks,
        session_start,
        input.options.enforced,
    )?;
    report.env.extend(components.env.clone());
    report.launch_args.extend(components.launch_args.clone());
    apply_session_upload_disposition(disposition, &mut report);
    (implementation.spec.operations.apply_packages)(
        plan,
        input.home,
        &mut report,
        components,
    )?;
    Ok(report)
}

fn apply_base_packages(
    plan: &mut ReconcilePlan,
    home: &Path,
    report: &mut HarnessWrite,
    components: &PackageComponents,
) -> Result<(), GhError> {
    writer::apply_packages(
        plan,
        home,
        report,
        &components.skills_dirs,
        &components.helpers,
    )
}

fn apply_plugin_packages(
    plan: &mut ReconcilePlan,
    home: &Path,
    report: &mut HarnessWrite,
    components: &PackageComponents,
) -> Result<(), GhError> {
    apply_base_packages(plan, home, report, components)
}

fn apply_hook_packages(
    plan: &mut ReconcilePlan,
    home: &Path,
    report: &mut HarnessWrite,
    components: &PackageComponents,
) -> Result<(), GhError> {
    apply_base_packages(plan, home, report, components)?;
    if components.hooks_files.is_empty() {
        return Ok(());
    }
    let settings = home.join(".config/blue/runtime/claude/settings.json");
    let mut document = plan.read_json_object(&settings)?;
    let hooks = document
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| GhError::config("Claude hooks must be an object"))?;
    for path in &components.hooks_files {
        let fragment = plan.read_json_object(path)?;
        let fragment = fragment
            .get("hooks")
            .and_then(serde_json::Value::as_object)
            .unwrap_or(&fragment);
        for (event, entries) in fragment {
            let entries = entries
                .as_array()
                .ok_or_else(|| GhError::config("Claude hook entries must be arrays"))?;
            hooks
                .entry(event.clone())
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .ok_or_else(|| GhError::config("Claude hook entries must be arrays"))?
                .extend(entries.iter().cloned());
        }
    }
    plan.write(
        &settings,
        crate::util::json_pretty(&serde_json::Value::Object(document))?,
    )
}
