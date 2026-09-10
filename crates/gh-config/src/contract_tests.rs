use super::*;
use adapters::{ReconcileInput, ReconcilePlan, ResolvedPackage, ResolvedPackages};
use std::path::{Path, PathBuf};

fn home(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "blue-contract-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn snapshot(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    if path.is_dir() {
        for entry in std::fs::read_dir(path).unwrap() {
            files.extend(snapshot(&entry.unwrap().path()));
        }
    } else if path.is_file() {
        files.insert(path.to_path_buf(), std::fs::read(path).unwrap());
    }
    files
}

/// A path spelled with `/`, whatever this platform's separator is. Unix paths
/// come back unchanged; `C:\Users\blue` comes back as `C:/Users/blue`.
fn render_path(path: &Path) -> String {
    let mut rendered = String::new();
    for component in path.components() {
        if matches!(component, std::path::Component::RootDir) {
            if !rendered.ends_with('/') {
                rendered.push('/');
            }
            continue;
        }
        if !rendered.is_empty() && !rendered.ends_with('/') {
            rendered.push('/');
        }
        rendered.push_str(&component.as_os_str().to_string_lossy());
    }
    rendered
}

/// Re-join a `PATH`-shaped value with `:` and `/`, so neither separator is the
/// platform's.
fn render_path_list(value: &str) -> String {
    std::env::split_paths(value)
        .map(|entry| render_path(&entry))
        .collect::<Vec<_>>()
        .join(":")
}

/// Where a path token ends in the documents a plan carries: TOML, JSON and
/// JavaScript all quote them, and `PATH` separates them.
const PATH_TERMINATORS: &[char] = &[
    '"', '\'', ',', ']', '}', ')', ':', ';', '=', ' ', '\t', '\r', '\n',
];

/// Replace every spelling of `root` in `value` with `marker`, normalising the
/// separators of the path that follows it.
///
/// The same root reaches us three ways: as this platform writes it, as a nested
/// JSON or JavaScript document escaped it (every `\` doubled), and already
/// rendered with `/`. On Unix all three coincide and this is a plain substring
/// replacement.
fn substitute(value: &str, root: &Path, marker: &str) -> String {
    let raw = root.to_string_lossy().into_owned();
    if raw.is_empty() {
        return value.to_owned();
    }
    // Longest first: the escaped spelling is a prefix-length superset of the raw
    // one, so matching the raw one first would leave a stray separator behind.
    let mut spellings = vec![raw.replace('\\', "\\\\"), raw, render_path(root)];
    spellings.dedup();
    let mut rendered = String::with_capacity(value.len());
    let mut rest = value;
    while !rest.is_empty() {
        let matched = spellings
            .iter()
            .find_map(|spelling| rest.strip_prefix(spelling.as_str()));
        let Some(tail) = matched else {
            let mut characters = rest.chars();
            rendered.push(characters.next().expect("rest is not empty"));
            rest = characters.as_str();
            continue;
        };
        rendered.push_str(marker);
        let end = tail.find(PATH_TERMINATORS).unwrap_or(tail.len());
        rendered.push_str(&tail[..end].replace("\\\\", "/").replace('\\', "/"));
        rest = &tail[end..];
    }
    rendered
}

/// Render a plan for comparison against its golden fixture.
///
/// One fixture serves every platform: paths are rendered with `/`, the roots
/// that vary per run collapse to `$HOME`, `$BLUE` and `$PATH`, and `PATH` is
/// rejoined with `:`. All of it happens *before* serialization — normalising
/// the serialized string instead means guessing how many levels of escaping
/// each nested document added, which is exactly what the fixtures got wrong
/// before.
fn render_plan(plan: &ReconcilePlan, home: &Path) -> String {
    let exe = std::env::current_exe().unwrap();
    let ambient_path = std::env::var("PATH")
        .ok()
        // An empty pattern would splice `$PATH` between every character.
        .filter(|path| !path.is_empty())
        .map(|path| render_path_list(&path));
    let text = |value: &str| {
        let mut rendered = substitute(value, home, "$HOME");
        if let Some(ambient) = ambient_path.as_deref() {
            rendered = rendered.replace(ambient, "$PATH");
        }
        substitute(&rendered, &exe, "$BLUE")
    };
    let path = |path: &PathBuf| text(&render_path(path));
    let value = serde_json::json!({
        "writes": plan.writes.iter().map(|write| serde_json::json!({
            "path": path(&write.path),
            "body": text(&String::from_utf8(write.body.clone()).unwrap()),
            "mode": write.mode,
        })).collect::<Vec<_>>(),
        "removals": plan.remove_paths.iter().map(path).collect::<Vec<_>>(),
        "ownership": plan.owned_paths.iter().map(path).collect::<Vec<_>>(),
        "files": plan.files.iter().map(path).collect::<Vec<_>>(),
        "env": plan.env.iter().map(|(key, value)| (
            key.clone(),
            text(&if key == "PATH" { render_path_list(value) } else { value.clone() }),
        )).collect::<BTreeMap<_, _>>(),
        "args": plan.launch_args.iter().map(|arg| text(arg)).collect::<Vec<_>>(),
        "warnings": plan.warnings.iter().map(|warning| text(warning)).collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&value).unwrap()
}

/// Compare a rendered plan against its fixture, treating TOML bodies as data.
///
/// `toml_write` refuses a basic string as soon as a value contains a backslash
/// and falls back to a literal one, so a Windows path makes it emit
/// `source = '...'` where Unix emits `source = "..."`. That is a quoting
/// difference, not a content one, and no amount of separator normalisation
/// undoes it — so parse both sides and let the quote style disappear.
fn assert_plan_matches(actual: &str, expected: &str, fixture: &Path) {
    let canonical = |rendered: &str| {
        let mut document = serde_json::from_str::<serde_json::Value>(rendered)
            .unwrap_or_else(|error| panic!("{}: {error}", fixture.display()));
        for write in document["writes"].as_array_mut().unwrap() {
            if !write["path"].as_str().unwrap().ends_with(".toml") {
                continue;
            }
            let body = write["body"].as_str().unwrap();
            let parsed = body
                .parse::<toml::Value>()
                .unwrap_or_else(|error| panic!("{}: {error}", fixture.display()));
            write["body"] = toml::to_string(&parsed).unwrap().into();
        }
        serde_json::to_string_pretty(&document).unwrap()
    };
    assert_eq!(
        canonical(actual),
        canonical(expected),
        "{}",
        fixture.display()
    );
}

fn context(harness: Harness, version: &str) -> HarnessContext {
    resolve_compatibility(
        harness,
        Some(&semver::Version::parse(version).unwrap()),
        Some(version),
        &HarnessPolicy::default(),
    )
    .unwrap()
}

fn assert_wrapped_update_controls(
    harness: Harness,
    home: &Path,
    env: &BTreeMap<String, String>,
    args: &[String],
) {
    match harness {
        Harness::Codex => assert!(args
            .windows(2)
            .any(|pair| { pair == ["--config", "check_for_update_on_startup=false"] })),
        Harness::Claude => assert_eq!(
            env.get("DISABLE_AUTOUPDATER").map(String::as_str),
            Some("1")
        ),
        Harness::Kimi => {
            assert_same_path(
                env.get("KIMI_CODE_HOME").map(String::as_str),
                &home.join(".config/blue/runtime/kimi"),
            );
            assert_eq!(
                env.get("KIMI_CODE_NO_AUTO_UPDATE").map(String::as_str),
                Some("1")
            );
        }
        Harness::Opencode => {
            let config: serde_json::Value =
                serde_json::from_str(env.get("OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
            assert_eq!(config["autoupdate"], false);
        }
    }
}

/// The fixtures are compared on Windows too, where a path reaches the renderer
/// both as the platform writes it and as the nested document escaped it.
#[test]
fn plan_rendering_collapses_every_spelling_of_a_root() {
    let home = home("render");
    let raw = home
        .join(".config/blue/runtime/kimi")
        .to_string_lossy()
        .into_owned();
    let escaped = raw.replace('\\', "\\\\");
    let body = format!("{{\"escaped\": \"{escaped}\", \"raw\": \"{raw}\"}}");
    assert_eq!(
        substitute(&body, &home, "$HOME"),
        "{\"escaped\": \"$HOME/.config/blue/runtime/kimi\", \"raw\": \"$HOME/.config/blue/runtime/kimi\"}"
    );
    std::fs::remove_dir_all(home).unwrap();
}

/// `PATH` carries both separators that vary: `;` between entries on Windows and
/// `\` inside them.
#[test]
fn path_lists_are_rejoined_platform_neutrally() {
    let home = home("path-list");
    let entries = [home.join("package/bin"), home.join(".local/bin")];
    let joined = std::env::join_paths(&entries).unwrap();
    assert_eq!(
        substitute(&render_path_list(joined.to_str().unwrap()), &home, "$HOME"),
        "$HOME/package/bin:$HOME/.local/bin"
    );
    std::fs::remove_dir_all(home).unwrap();
}

/// The one platform difference the fixtures cannot absorb: `toml_write` picks a
/// literal string over a basic one as soon as a value contains a backslash, so
/// Windows emits `source = '...'` where Unix emits `source = "..."`.
#[test]
fn toml_quote_style_is_not_a_content_difference() {
    let plan = |quote: char| {
        serde_json::json!({
            "writes": [{
                "path": "$HOME/.codex/config.toml",
                "body": format!("source = {quote}$HOME/x{quote}\n"),
                "mode": 384,
            }],
        })
        .to_string()
    };
    assert_plan_matches(&plan('"'), &plan('\''), Path::new("synthetic"));
}

#[test]
fn every_production_interval_has_a_pure_golden_plan() {
    for definition in adapters::HARNESS_DEFINITIONS.iter() {
        for registration in definition.implementations {
            for gateway_enabled in [false, true] {
                let home = home(registration.interval.profile);
                let skill = home.join("package/skills/example");
                std::fs::create_dir_all(&skill).unwrap();
                std::fs::write(skill.join("SKILL.md"), "# Example\n\nA managed skill.\n").unwrap();
                let mut packages = ResolvedPackages::default();
                let adapter = gh_service::PackageAdapter {
                    skills_dir: Some("skills".into()),
                    ..Default::default()
                };
                if registration
                    .implementation
                    .validate_components(&adapter)
                    .is_ok()
                {
                    packages.groups.push(ResolvedPackage {
                        id: "fixture".into(),
                        skills: vec![skill],
                        ..Default::default()
                    });
                }
                let package_root = home.join("package");
                let plugin = package_root.join("plugin");
                for directory in [".codex-plugin", ".claude-plugin"] {
                    std::fs::create_dir_all(plugin.join(directory)).unwrap();
                    std::fs::write(
                        plugin.join(directory).join("plugin.json"),
                        r#"{"name":"fixture-plugin","version":"1.0.0"}"#,
                    )
                    .unwrap();
                }
                let agents = package_root.join("agents");
                std::fs::create_dir_all(&agents).unwrap();
                std::fs::write(
                    agents.join("reviewer.md"),
                    "---\nname: reviewer\ndescription: Review changes\n---\nReview carefully.\n",
                )
                .unwrap();
                let hooks = package_root.join("hooks.json");
                std::fs::write(&hooks, r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"fixture-hook"}]}]}}"#).unwrap();
                let mut component = ResolvedPackage {
                    id: "components".into(),
                    settings_environment: BTreeMap::from([(
                        "HARNESS_PACKAGE_COMPONENTS_SETTINGS".into(),
                        r#"{"color":"blue"}"#.into(),
                    )]),
                    ..Default::default()
                };
                match definition.harness {
                    Harness::Codex => {
                        component.plugin = Some(plugin);
                        component.agents.push(agents);
                        component.hooks.push(hooks);
                    }
                    Harness::Claude if registration.interval.introduced == "2.0.12" => {
                        component.plugin = Some(plugin);
                    }
                    Harness::Claude if registration.interval.introduced == "1.0.38" => {
                        component.hooks.push(hooks);
                    }
                    Harness::Claude => {}
                    Harness::Kimi => {
                        let hooks = package_root.join("hooks.toml");
                        std::fs::write(
                            &hooks,
                            "[[hooks]]\nevent = \"Stop\"\ncommand = \"fixture-hook\"\n",
                        )
                        .unwrap();
                        component.agents.push(agents);
                        component.hooks.push(hooks);
                    }
                    Harness::Opencode => {
                        let module = package_root.join("fixture.js");
                        std::fs::write(&module, "export const Fixture = async () => ({})\n")
                            .unwrap();
                        component.agents.push(agents);
                        component.plugin_modules.push(module);
                    }
                }
                let helper = package_root.join("bin/helper");
                std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
                std::fs::write(&helper, "#!/bin/sh\nexit 0\n").unwrap();
                component.helpers.insert("helper".into(), helper);
                packages.groups.push(component);
                let policy: HarnessPolicy = serde_json::from_value(serde_json::json!({
                    "managed_config": {"model":"fixture-model", "auto_approve":false},
                    "mcp":[{"name":"fixture", "command":"fixture-mcp", "args":["--stdio"]}]
                }))
                .unwrap();
                let gateway: GatewayConfig = serde_json::from_value(serde_json::json!({"type":"litellm", "proxy_url":"https://gateway.example", "token":"fixture-token"})).unwrap();
                let wiring = gateway_enabled.then(|| {
                    registration
                        .implementation
                        .gateway_wiring(&gateway)
                        .unwrap()
                });
                let before = snapshot(&home);
                let plan = registration
                    .implementation
                    .plan(
                        &ReconcileInput {
                            home: &home,
                            policy: &policy,
                            gateway: wiring.as_ref(),
                            options: WriteOptions {
                                gateway_enabled,
                                session_upload_enabled: true,
                                ..Default::default()
                            },
                            interval: &registration.interval,
                        },
                        &packages,
                    )
                    .unwrap();
                assert_eq!(
                    snapshot(&home),
                    before,
                    "{} mutated its source home",
                    registration.interval.profile
                );
                let context = context(definition.harness, registration.interval.introduced);
                validate_plan(&context, &home, &plan).unwrap();
                assert_wrapped_update_controls(
                    definition.harness,
                    &home,
                    &plan.env,
                    &plan.launch_args,
                );
                // One fixture per interval, compared on every platform: the
                // rendering is normalised before it is serialized, so the only
                // platform difference left is TOML quote style, which
                // `assert_plan_matches` compares as data.
                let actual = render_plan(&plan, &home);
                let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/golden")
                    .join(format!(
                        "{}-{}.json",
                        registration.interval.profile,
                        if gateway_enabled {
                            "gateway"
                        } else {
                            "governance"
                        }
                    ));
                if std::env::var_os("BLUE_UPDATE_GOLDENS").is_some() {
                    std::fs::create_dir_all(fixture.parent().unwrap()).unwrap();
                    std::fs::write(&fixture, format!("{actual}\n")).unwrap();
                }
                assert_plan_matches(
                    &actual,
                    &std::fs::read_to_string(&fixture).unwrap(),
                    &fixture,
                );
                let mut transaction = FileTransaction::begin(&home, &plan).unwrap();
                transaction.apply(&plan).unwrap();
                transaction.commit().unwrap();
                let launch = registration
                    .implementation
                    .launch(&home, wiring.as_ref(), HarnessLaunchSpec::default())
                    .unwrap();
                assert_wrapped_update_controls(
                    definition.harness,
                    &home,
                    &launch.env,
                    &launch.launch_args,
                );
                assert!(launch
                    .launch_args
                    .iter()
                    .all(|arg| !arg.contains("blue-render-")));
                std::fs::remove_dir_all(home).unwrap();
            }
        }
    }
}

#[test]
fn boundaries_aliases_and_unsupported_packages_are_explicit() {
    for (harness, version, expected) in [
        (Harness::Codex, "0.113.99", "codex-v0_0_0"),
        (Harness::Codex, "0.114.0", "codex-v0_114_0"),
        (Harness::Codex, "0.144.99", "codex-v0_114_0"),
        (Harness::Codex, "0.145.0", "codex-v0_145_0"),
        (Harness::Claude, "1.0.37", "claude-v0_0_0"),
        (Harness::Claude, "1.0.38", "claude-v1_0_38"),
        (Harness::Claude, "2.0.11", "claude-v1_0_38"),
        (Harness::Claude, "2.0.12", "claude-v2_0_12"),
    ] {
        assert_eq!(context(harness, version).profile.id, expected);
    }
    for harness in Harness::ALL {
        let alias = format!("{}-v1", harness.key());
        assert_eq!(
            adapters::implementation_for_profile(harness, &alias)
                .unwrap()
                .interval
                .profile,
            adapters::definition(harness)
                .implementations
                .last()
                .unwrap()
                .interval
                .profile
        );
        assert!(adapters::implementation_for_profile(harness, "unknown-profile").is_none());
    }
    let plugin = gh_service::PackageAdapter {
        plugin_dir: Some("plugin".into()),
        ..Default::default()
    };
    assert!(context(Harness::Claude, "2.0.11")
        .profile
        .implementation
        .validate_components(&plugin)
        .is_err());
    assert!(context(Harness::Claude, "2.0.12")
        .profile
        .implementation
        .validate_components(&plugin)
        .is_ok());
    let install = supported_install(
        Harness::Claude,
        &HarnessPolicy {
            version_requirement: Some("<1.0.38".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(install.args.last().unwrap().contains("<1.0.38"));
    assert!(install.args.last().unwrap().contains(">=0.0.0"));
    assert!(supported_install(
        Harness::Claude,
        &HarnessPolicy {
            version_requirement: Some(">=2, <1".into()),
            ..Default::default()
        }
    )
    .is_err());
    assert!(supported_install(
        Harness::Claude,
        &HarnessPolicy {
            version_requirement: Some("invalid".into()),
            ..Default::default()
        }
    )
    .is_err());
}

#[test]
fn forged_removals_and_ownership_are_rejected() {
    let home = home("authority");
    let context = context(Harness::Codex, "0.145.0");
    for path in [
        home.join("blue-innocent"),
        home.join(".codex/../personal"),
        home.join(".codex/config.toml/child"),
    ] {
        let plan = ReconcilePlan {
            remove_paths: vec![path.clone()],
            owned_paths: vec![path],
            ..Default::default()
        };
        assert!(validate_plan(&context, &home, &plan).is_err());
    }
    let state_path = compatibility_state_path(&home, Harness::Codex);
    std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
    for profile in ["", "unknown-profile"] {
        std::fs::write(&state_path, serde_json::to_vec(&serde_json::json!({"schema_version":4,"profile_id":profile,"version":"0.145.0","files":[],"owned_paths":[]})).unwrap()).unwrap();
        assert!(load_compatibility_state_at(&home, Harness::Codex).is_err());
    }
    std::fs::remove_dir_all(home).unwrap();
}

#[cfg(unix)]
#[test]
fn symlink_escape_is_rejected_and_exact_symlink_removal_rolls_back() {
    use std::os::unix::fs::symlink;
    let outside = home("outside");
    let home = home("symlink");
    std::fs::write(outside.join("personal"), "safe").unwrap();
    symlink(&outside, home.join(".codex")).unwrap();
    let context = context(Harness::Codex, "0.145.0");
    let plan = ReconcilePlan {
        writes: vec![adapters::PlannedFile {
            path: home.join(".codex/blue.config.toml"),
            body: b"unsafe".to_vec(),
            mode: Some(0o600),
        }],
        ..Default::default()
    };
    assert!(validate_plan(&context, &home, &plan).is_err());
    std::fs::remove_file(home.join(".codex")).unwrap();
    std::fs::create_dir(home.join(".codex")).unwrap();
    let link = home.join(".codex/blue.config.toml");
    symlink(outside.join("personal"), &link).unwrap();
    assert!(validate_plan(&context, &home, &plan).is_err());
    let removal = ReconcilePlan {
        remove_paths: vec![link.clone()],
        ..Default::default()
    };
    validate_plan(&context, &home, &removal).unwrap();
    {
        let mut transaction = FileTransaction::begin(&home, &removal).unwrap();
        assert!(transaction.apply_with_fault(&removal, Some(1)).is_err());
    }
    assert!(link.is_symlink());
    assert_eq!(
        std::fs::read_to_string(outside.join("personal")).unwrap(),
        "safe"
    );
    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(outside).unwrap();
}

// Exercised only by the `#[cfg(unix)]` synthetic-definition test below.
#[cfg(unix)]
#[derive(Debug)]
struct SyntheticImplementation {
    directory: &'static str,
    schema: u32,
}
#[cfg(unix)]
static SYNTHETIC_OLD: SyntheticImplementation = SyntheticImplementation {
    directory: ".synthetic-one",
    schema: 1,
};
#[cfg(unix)]
static SYNTHETIC_NEW: SyntheticImplementation = SyntheticImplementation {
    directory: ".synthetic-two",
    schema: 2,
};
#[cfg(unix)]
static SYNTHETIC_METADATA: gh_common::HarnessMetadata = gh_common::HarnessMetadata {
    harness: Harness::Codex,
    key: "synthetic",
    aliases: &[],
    label: "Synthetic",
    description: "Test harness",
    binary_names: &["synthetic"],
    install_command_template: "synthetic-install {version}",
    install_program: "synthetic-install",
    install_args: &["{version}"],
};
#[cfg(unix)]
static SYNTHETIC: adapters::HarnessDefinition = adapters::HarnessDefinition {
    harness: Harness::Codex,
    metadata: &SYNTHETIC_METADATA,
    version_probes: &[adapters::VersionProbe::command(
        &["--version"],
        adapters::parse_semver_token,
    )],
    implementations: &[
        adapters::ImplementationRegistration {
            interval: adapters::VersionInterval {
                profile: "synthetic-one",
                aliases: &["synthetic-legacy"],
                introduced: "1.0.0",
                before: Some("2.0.0"),
                verified_before: "2.0.0-0",
                lifecycle: adapters::ImplementationLifecycle::Supported,
            },
            implementation: &SYNTHETIC_OLD,
        },
        adapters::ImplementationRegistration {
            interval: adapters::VersionInterval {
                profile: "synthetic-two",
                aliases: &[],
                introduced: "2.0.0",
                before: None,
                verified_before: "2.1.0-0",
                lifecycle: adapters::ImplementationLifecycle::Supported,
            },
            implementation: &SYNTHETIC_NEW,
        },
    ],
};
#[cfg(unix)]
impl adapters::HarnessImplementation for SyntheticImplementation {
    fn support(&self) -> &'static adapters::GenerationSupport {
        static SUPPORT: adapters::GenerationSupport = adapters::GenerationSupport {
            capabilities: &["mcp"],
            component_rules: gh_common::ComponentRules {
                agents_require_plugin: false,
                hooks_require_plugin: false,
                hooks_as_plugin_modules: false,
            },
        };
        &SUPPORT
    }
    fn paths(&self, home: &Path) -> adapters::ImplementationPaths {
        adapters::ImplementationPaths {
            owned_outputs: vec![home.join(self.directory)],
            ..Default::default()
        }
    }
    fn plan(
        &self,
        input: &ReconcileInput<'_>,
        packages: &ResolvedPackages,
    ) -> Result<ReconcilePlan, GhError> {
        let root = input.home.join(self.directory);
        let mut plan = ReconcilePlan {
            owned_paths: vec![root.clone()],
            files: vec![root.join("config")],
            ..Default::default()
        };
        let body = if self.schema == 1 {
            format!(
                "version=1\nhook=legacy\nmodel={}\n",
                input
                    .policy
                    .managed_config
                    .model
                    .as_deref()
                    .unwrap_or("default")
            )
        } else {
            serde_json::json!({"schema":2,"events":{"finished":"upload"},"gateway":input.gateway.map(|w| &w.base_url)}).to_string()
        };
        plan.write(&root.join("config"), body)?;
        for package in &packages.groups {
            plan.component_dirs(
                &package.skills,
                &root.join(if self.schema == 1 {
                    "skills"
                } else {
                    "extensions"
                }),
                &mut Vec::new(),
            )?;
        }
        plan.launch_args = vec![format!("--schema-{}", self.schema)];
        plan.env
            .insert("SYNTHETIC_ROOT".into(), root.display().to_string());
        plan.normalize();
        Ok(plan)
    }
    fn session_upload_disposition(
        &self,
        _: &ReconcileInput<'_>,
    ) -> adapters::SessionUploadDisposition {
        adapters::SessionUploadDisposition::Installed
    }
    fn gateway_wiring(
        &self,
        gateway: &GatewayConfig,
    ) -> Result<gh_gateway::GatewayWiring, GhError> {
        gh_gateway::wire_with(gateway, gh_gateway::AuthPlacement::InFile, None)
    }
    fn validate_components(&self, _: &gh_service::PackageAdapter) -> Result<(), GhError> {
        Ok(())
    }
    fn install_plan(
        &self,
        _: &adapters::VersionInterval,
        requirement: Option<&str>,
    ) -> gh_common::InstallInvocation {
        gh_common::InstallInvocation {
            program: "synthetic-install",
            args: vec![requirement.unwrap_or("latest").into()],
            display: "synthetic-install".into(),
        }
    }
    fn inspect(&self, _: &HarnessPolicy, files: &[PathBuf]) -> BTreeMap<String, String> {
        BTreeMap::from([("body".into(), std::fs::read_to_string(&files[0]).unwrap())])
    }
    fn proposed_values(
        &self,
        _: &HarnessPolicy,
        _: Option<&GatewayConfig>,
        _: WriteOptions,
        current: BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        current
    }
    fn launch(
        &self,
        home: &Path,
        _: Option<&gh_gateway::GatewayWiring>,
        mut spec: HarnessLaunchSpec,
    ) -> Result<HarnessLaunchSpec, GhError> {
        if !home.join(self.directory).join("config").is_file() {
            return Err(GhError::config("synthetic config missing"));
        }
        spec.launch_args.push(format!("--schema-{}", self.schema));
        Ok(spec)
    }
}

#[cfg(unix)]
#[test]
fn synthetic_definition_runs_detection_packages_transition_commit_rollback_and_launch() {
    use std::os::unix::fs::PermissionsExt;
    let home = home("synthetic");
    let binary = home.join("binary");
    let package_root = home.join("package");
    std::fs::create_dir_all(package_root.join("skills")).unwrap();
    std::fs::write(package_root.join("skills/example.txt"), "example").unwrap();
    let policy = HarnessPolicy::default();
    let gateway: GatewayConfig = serde_json::from_value(
        serde_json::json!({"type":"litellm","proxy_url":"https://gateway.example","token":"token"}),
    )
    .unwrap();
    for version in ["1.9.0", "2.0.0"] {
        std::fs::write(
            &binary,
            format!("#!/bin/sh\nprintf 'synthetic {version}\\n'\n"),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let detected = SYNTHETIC.detect_version(&binary).unwrap();
        let context = compat::resolve_for_definition(
            &SYNTHETIC,
            detected.version.as_ref(),
            Some(&detected.raw),
            &policy,
        )
        .unwrap();
        let implementation = context.profile.implementation;
        let wiring = implementation.gateway_wiring(&gateway).unwrap();
        let input = ReconcileInput {
            home: &home,
            policy: &policy,
            gateway: Some(&wiring),
            options: WriteOptions::default(),
            interval: &context.profile.interval,
        };
        let mut prepared = packages::PreparedPackages::default();
        prepared.resolved.groups.push(ResolvedPackage {
            id: "synthetic-package".into(),
            skills: vec![package_root.join("skills")],
            ..Default::default()
        });
        implementation
            .validate_components(&gh_service::PackageAdapter {
                skills_dir: Some("skills".into()),
                ..Default::default()
            })
            .unwrap();
        let before = snapshot(&home);
        implementation.plan(&input, &prepared.resolved).unwrap();
        assert_eq!(snapshot(&home), before);
        if version == "2.0.0" {
            assert!(
                reconcile_prepared_at(&context, &input, &mut prepared, |_| Err(GhError::other(
                    "injected activation failure"
                )))
                .is_err()
            );
            assert_eq!(
                snapshot(&home),
                before,
                "failed package activation must restore old config and V4 state"
            );
        }
        let report = reconcile_prepared_at(&context, &input, &mut prepared, |_| Ok(())).unwrap();
        let state = load_definition_state_at(&home, &SYNTHETIC).unwrap();
        assert_eq!(state.schema_version, 4);
        assert_eq!(state.profile_id, context.profile.id);
        assert_eq!(state.owned_paths, implementation.paths(&home).owned_outputs);
        let inspected = implementation.inspect(&policy, &report.files);
        assert!(!inspected["body"].is_empty());
        assert_eq!(
            implementation.proposed_values(
                &policy,
                Some(&gateway),
                WriteOptions::default(),
                inspected.clone()
            ),
            inspected
        );
        let launch = resolve_launch_spec_at(
            &home,
            &context,
            &policy,
            &[],
            Some(&gateway),
            WriteOptions::default(),
        )
        .unwrap();
        assert_eq!(launch.launch_args, report.launch_args);
        if version == "2.0.0" {
            assert!(!home.join(".synthetic-one").exists());
        }
    }
    assert!(SYNTHETIC.select(&semver::Version::new(0, 9, 0)).is_none());
    assert!(SYNTHETIC.detect_version(&home.join("missing")).is_err());
    std::fs::write(&binary, "#!/bin/sh\nprintf 'unknown\\n'\n").unwrap();
    assert!(SYNTHETIC.detect_version(&binary).unwrap().version.is_none());
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn production_boundary_transitions_preserve_v4_and_roll_back_activation_failure() {
    for (harness, versions) in [
        (
            Harness::Codex,
            vec!["0.113.99", "0.114.0", "0.145.0", "0.144.99", "0.113.99"],
        ),
        (
            Harness::Claude,
            vec!["1.0.37", "1.0.38", "2.0.12", "2.0.11", "1.0.37"],
        ),
    ] {
        let home = home("transition");
        let policy = HarnessPolicy::default();
        for (index, version) in versions.iter().enumerate() {
            let context = context(harness, version);
            let input = ReconcileInput {
                home: &home,
                policy: &policy,
                gateway: None,
                options: WriteOptions {
                    session_upload_enabled: true,
                    allow_existing_merge: true,
                    ..Default::default()
                },
                interval: &context.profile.interval,
            };
            let before = snapshot(&home);
            let mut prepared = packages::PreparedPackages::default();
            if index > 0 {
                assert!(
                    reconcile_prepared_at(&context, &input, &mut prepared, |_| Err(
                        GhError::other("activation fault")
                    ))
                    .is_err()
                );
                assert_eq!(snapshot(&home), before);
            }
            let report =
                reconcile_prepared_at(&context, &input, &mut prepared, |_| Ok(())).unwrap();
            let state = load_definition_state_at(&home, context.definition).unwrap();
            assert_eq!(state.schema_version, 4);
            assert_eq!(state.profile_id, context.profile.id);
            let supported = matches!(
                context
                    .profile
                    .implementation
                    .session_upload_disposition(&input),
                adapters::SessionUploadDisposition::Installed
            );
            assert_eq!(report.warnings.is_empty(), supported);
            let bodies = report
                .files
                .iter()
                .filter_map(|path| std::fs::read_to_string(path).ok())
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(bodies.contains("session-upload"), supported);
        }
        std::fs::remove_dir_all(home).unwrap();
    }
}

#[test]
fn revision_snapshot_restores_earlier_harness_commits() {
    let home = home("revision");
    let policy = HarnessPolicy::default();
    let contexts = [
        context(Harness::Codex, "0.145.0"),
        context(Harness::Claude, "2.0.12"),
    ];
    for context in &contexts {
        let input = ReconcileInput {
            home: &home,
            policy: &policy,
            gateway: None,
            options: WriteOptions {
                allow_existing_merge: true,
                ..Default::default()
            },
            interval: &context.profile.interval,
        };
        reconcile_prepared_at(
            context,
            &input,
            &mut packages::PreparedPackages::default(),
            |_| Ok(()),
        )
        .unwrap();
    }
    // Populate locks before taking the expected snapshot.
    {
        let locks = acquire_revision_locks_at(&home, &contexts).unwrap();
        let mut revision = locks.begin_transaction().unwrap();
        revision.commit().unwrap();
    }
    let before = snapshot(&home);
    {
        let locks = acquire_revision_locks_at(&home, &contexts).unwrap();
        let _revision = locks.begin_transaction().unwrap();
        let changed: HarnessPolicy =
            serde_json::from_value(serde_json::json!({"managed_config":{"model":"changed"}}))
                .unwrap();
        for (index, context) in contexts.iter().enumerate() {
            let input = ReconcileInput {
                home: &home,
                policy: &changed,
                gateway: None,
                options: WriteOptions {
                    allow_existing_merge: true,
                    ..Default::default()
                },
                interval: &context.profile.interval,
            };
            let result = reconcile_prepared_at(
                context,
                &input,
                &mut packages::PreparedPackages::default(),
                |_| {
                    if index == 1 {
                        Err(GhError::other("second harness failed"))
                    } else {
                        Ok(())
                    }
                },
            );
            assert_eq!(result.is_ok(), index == 0);
        }
    }
    assert_eq!(snapshot(&home), before);
    assert!(
        acquire_revision_locks_at(&home, &[contexts[0].clone(), contexts[0].clone()])
            .unwrap()
            .begin_transaction()
            .is_err(),
        "colliding transaction declarations must fail before snapshots"
    );
    std::fs::remove_dir_all(home).unwrap();
}
