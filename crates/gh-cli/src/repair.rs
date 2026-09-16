//! Installation-aware repair. Plans contain argv, never executable shell strings.
use anyhow::{anyhow, bail, Context, Result};
use gh_common::{Harness, InstallInvocation};
use gh_config::{resolve_compatibility, supported_install, CompatibilityFailure, HarnessContext};
use gh_harness::{Detected, InstallMethod, Installation};
use gh_service::HarnessPolicy;
use semver::Version;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

fn run_kimi_standalone_installer(install_root: &Path, version: &semver::Version) -> Result<()> {
    let url = "https://code.kimi.com/kimi-code/install.sh";
    let display = RepairPlan::KimiInstaller {
        root: install_root.to_owned(),
        version: version.clone(),
    }
    .display();
    let download = std::process::Command::new("curl")
        .args(["-fsSL", url])
        .output()
        .context("downloading Kimi's official installer")?;
    if !download.status.success() {
        let stderr = String::from_utf8_lossy(&download.stderr).trim().to_owned();
        bail!(
            "downloading Kimi's official installer failed{}",
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        );
    }
    let child_path =
        kimi_installer_path(install_root, &std::env::var_os("PATH").unwrap_or_default())?;
    let mut child = kimi_installer_command(install_root, version, &child_path)
        .spawn()
        .context("starting Kimi's official installer")?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("Kimi installer stdin was unavailable"))?
        .write_all(&download.stdout)
        .context("sending Kimi's official installer to bash")?;
    let status = child
        .wait()
        .context("waiting for Kimi's official installer")?;
    if !status.success() {
        bail!("Kimi installer exited with {status}; retry with `{display}`");
    }
    Ok(())
}

fn resolve_npm_install_version(
    harness: Harness,
    invocation: &InstallInvocation,
) -> Result<semver::Version> {
    let package = invocation.args.last().ok_or_else(|| {
        anyhow!("the {harness} install plan did not contain an npm package selector")
    })?;
    let output = std::process::Command::new("npm")
        .args(["view", package, "version", "--json"])
        .output()
        .with_context(|| format!("npm is required for read-only release lookup of a policy-supported {harness} version"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!(
            "npm could not resolve a policy-supported {harness} release{}",
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        );
    }
    highest_npm_version(&output.stdout)
        .with_context(|| format!("resolving a published policy-supported {harness} release"))
}

fn highest_npm_version(output: &[u8]) -> Result<semver::Version> {
    let value: serde_json::Value = serde_json::from_slice(output)
        .context("npm returned an invalid harness version response")?;
    let values = match &value {
        serde_json::Value::String(_) => vec![&value],
        serde_json::Value::Array(values) => values.iter().collect(),
        _ => bail!("npm returned an invalid harness version response"),
    };
    values
        .into_iter()
        .map(|value| {
            let text = value
                .as_str()
                .ok_or_else(|| anyhow!("npm returned a non-string harness version"))?;
            Version::parse(text.trim_start_matches('v'))
                .context("npm returned an invalid harness version")
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .max()
        .ok_or_else(|| anyhow!("npm found no published harness release matching the policy"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RepairPlan {
    Command { program: PathBuf, args: Vec<String> },
    KimiInstaller { root: PathBuf, version: Version },
}

impl RepairPlan {
    fn display(&self) -> String {
        match self {
            Self::Command { program, args } => format!("{:?} {}", program, args.iter().map(|arg| format!("{arg:?}")).collect::<Vec<_>>().join(" ")),
            Self::KimiInstaller { root, version } => format!("Kimi official installer --version {version} (KIMI_INSTALL_DIR={root:?}, KIMI_NO_MODIFY_PATH=1; installation bin excluded from child PATH to skip legacy migration)"),
        }
    }
}

fn refusal(method: &InstallMethod) -> Option<String> {
    match method {
        InstallMethod::Homebrew { package } => Some(format!("Homebrew package `{package}`: automatic exact-version pinning is unsupported; see https://docs.brew.sh/FAQ and the vendor's installation guide")),
        InstallMethod::KimiUvLegacy => Some("legacy uv kimi-cli distribution: its Python versions are not the current kimi-code release line; see https://www.kimi.com/code/docs/en/kimi-code-cli/guides/migration.html".into()),
        InstallMethod::Unknown { reason } => Some(reason.clone()),
        _ => None,
    }
}

fn manual_action(installation: &Installation) -> String {
    match &installation.method {
        InstallMethod::NpmGlobal { prefix, package } => format!("npm install -g --prefix {prefix:?} {package}@<exact-policy-compatible-version>"),
        InstallMethod::ClaudeNative => format!("{:?} install <exact-policy-compatible-version>", installation.executable),
        InstallMethod::KimiStandalone { root } => format!("use the Kimi official installer with --version <exact-policy-compatible-version>, KIMI_INSTALL_DIR={root:?}, KIMI_NO_MODIFY_PATH=1; exclude that root's bin from the installer PATH to skip legacy migration"),
        InstallMethod::OpenCodeStandalone => format!("{:?} upgrade <exact-policy-compatible-version> --method curl", installation.executable),
        other => format!("{}; manually install a policy-compatible release at the active path using its owner", refusal(other).expect("unsupported method")),
    }
}

fn method_name(method: &InstallMethod) -> &'static str {
    match method {
        InstallMethod::NpmGlobal { .. } => "npm global",
        InstallMethod::ClaudeNative => "Claude native",
        InstallMethod::KimiStandalone { .. } => "Kimi standalone",
        InstallMethod::OpenCodeStandalone => "OpenCode standalone",
        InstallMethod::Homebrew { .. } => "Homebrew",
        InstallMethod::KimiUvLegacy => "legacy Kimi uv",
        InstallMethod::Unknown { .. } => "unknown",
    }
}

fn guidance(installation: &Installation) -> String {
    let action = manual_action(installation);
    let method = method_name(&installation.method);
    format!(
        "active path `{}` (resolved `{}`, method {method}); {action}",
        installation.executable.display(),
        installation.canonical.display()
    )
}

/// Keeps operational details available without flattening them into the terminal message.
#[derive(Debug)]
pub(crate) struct ManualRepairRequired {
    failure: CompatibilityFailure,
    installation: Installation,
    message: String,
}

impl std::fmt::Display for ManualRepairRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ManualRepairRequired {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.failure)
    }
}

fn compatibility_summary(failure: &CompatibilityFailure) -> String {
    match failure {
        CompatibilityFailure::UnparseableVersion { harness, .. } => {
            format!("Blue could not read the installed {harness} version.")
        }
        CompatibilityFailure::PolicyMismatch {
            harness,
            requirement,
            installed,
        } => format!(
            "{harness} {installed} does not match the required version range {requirement}."
        ),
        CompatibilityFailure::UnsupportedGeneration { harness, installed } => {
            format!("{harness} {installed} is not supported by this Blue release.")
        }
        CompatibilityFailure::UnverifiedGeneration {
            harness, installed, ..
        } => format!("{harness} {installed} is newer than Blue's tested versions."),
        CompatibilityFailure::InvalidPolicy { .. } => failure.to_string(),
    }
}

fn npm_migration_command(invocation: &InstallInvocation) -> Option<&str> {
    match invocation.args.as_slice() {
        [install, global, selector]
            if invocation.program == "npm"
                && install == "install"
                && global == "-g"
                && selector.rsplit_once('@').is_some_and(|(package, range)| {
                    !package.is_empty() && !range.is_empty() && range != "latest"
                }) =>
        {
            Some(&invocation.display)
        }
        _ => None,
    }
}

impl ManualRepairRequired {
    pub(crate) fn new(
        failure: CompatibilityFailure,
        installation: Installation,
        harness: Harness,
        policy: &HarnessPolicy,
    ) -> Self {
        let reason = match &installation.method {
            InstallMethod::Homebrew { .. } => {
                "Blue cannot automatically install a specific version with Homebrew."
            }
            InstallMethod::KimiUvLegacy => {
                "This is the legacy Python version of Kimi. Blue requires the current Kimi CLI."
            }
            InstallMethod::Unknown { .. } => {
                "Blue cannot safely replace this installation automatically."
            }
            _ => "Automatic repair requires an interactive terminal.",
        };
        let remedy = if refusal(&installation.method).is_none() {
            format!(
                "To repair manually:\n  {}\nThen retry Blue.",
                manual_action(&installation)
            )
        } else {
            match supported_install(harness, policy) {
                Err(_) => "Ask your administrator to select a version range supported by this Blue release, or update Blue.".into(),
                Ok(invocation) => match npm_migration_command(&invocation) {
                    Some(command) => {
                        let preparation = match &installation.method {
                            InstallMethod::Homebrew { .. } => format!("remove this {harness} installation using Homebrew"),
                            InstallMethod::KimiUvLegacy => "remove the legacy kimi-cli installation using uv".into(),
                            _ => "identify and remove the old installation using its installer, or use a separate npm prefix".into(),
                        };
                        format!("To migrate to npm, {preparation}, then run (POSIX shell or PowerShell):\n  {command}\nEnsure npm's executable directory is on PATH, then retry Blue.")
                    }
                    None => "Manually install a policy-compatible release using the current manager or vendor's installation guide, then retry Blue.".into(),
                },
            }
        };
        let message = format!(
            "{}\nInstallation: {} — {}\n{reason}\n\n{remedy}",
            compatibility_summary(&failure),
            method_name(&installation.method),
            installation.executable.display(),
        );
        let diagnostic = Self {
            failure,
            installation,
            message,
        };
        tracing::debug!(failure = ?diagnostic.failure, installation = ?diagnostic.installation, "Manual harness repair required");
        diagnostic
    }
}

fn plan(installation: &Installation, version: &Version) -> Result<RepairPlan> {
    if let Some(reason) = refusal(&installation.method) {
        bail!("{reason}");
    }
    let (program, args) = match &installation.method {
        InstallMethod::NpmGlobal { prefix, package } => (
            PathBuf::from("npm"),
            vec![
                "install".into(),
                "-g".into(),
                "--prefix".into(),
                prefix
                    .to_str()
                    .ok_or_else(|| anyhow!("npm prefix is not UTF-8"))?
                    .into(),
                format!("{package}@{version}"),
            ],
        ),
        InstallMethod::ClaudeNative => (
            installation.executable.clone(),
            vec!["install".into(), version.to_string()],
        ),
        InstallMethod::OpenCodeStandalone => (
            installation.executable.clone(),
            vec![
                "upgrade".into(),
                version.to_string(),
                "--method".into(),
                "curl".into(),
            ],
        ),
        InstallMethod::KimiStandalone { root } => {
            return Ok(RepairPlan::KimiInstaller {
                root: root.clone(),
                version: version.clone(),
            })
        }
        _ => unreachable!("refused above"),
    };
    Ok(RepairPlan::Command { program, args })
}

// The vendor installer explicitly skips legacy migration when NO_MODIFY_PATH
// is set and its own bin is absent from PATH. Do not let a repair remove copies.
fn kimi_installer_path(root: &Path, path: &std::ffi::OsStr) -> Result<std::ffi::OsString> {
    let own_bin = root.join("bin");
    Ok(std::env::join_paths(std::env::split_paths(path).filter(
        |entry| {
            entry != &own_bin
                && !std::fs::canonicalize(entry)
                    .ok()
                    .zip(std::fs::canonicalize(&own_bin).ok())
                    .is_some_and(|(entry, own)| entry == own)
        },
    ))?)
}

fn kimi_installer_command(
    root: &Path,
    version: &Version,
    child_path: &std::ffi::OsStr,
) -> std::process::Command {
    let mut command = std::process::Command::new("bash");
    command
        .args(["-s", "--", "--version", &version.to_string()])
        .env("KIMI_INSTALL_DIR", root)
        .env("KIMI_NO_MODIFY_PATH", "1")
        .env("PATH", child_path)
        .stdin(Stdio::piped());
    command
}

pub(crate) trait Runtime {
    fn installation(&mut self, detected: &Detected) -> Installation;
    fn lookup(&mut self, harness: Harness, invocation: &InstallInvocation) -> Result<Version>;
    fn confirm(&mut self, message: &str) -> Result<bool>;
    fn execute(&mut self, plan: &RepairPlan) -> Result<()>;
    fn detect(&mut self, harness: Harness) -> Option<Detected>;
    fn candidates(&mut self, harness: Harness) -> Vec<Detected>;
}

pub(crate) struct NativeRuntime;
impl Runtime for NativeRuntime {
    fn installation(&mut self, detected: &Detected) -> Installation {
        gh_harness::detect_installation(detected)
    }
    fn lookup(&mut self, harness: Harness, invocation: &InstallInvocation) -> Result<Version> {
        resolve_npm_install_version(harness, invocation)
    }
    fn confirm(&mut self, message: &str) -> Result<bool> {
        Ok(cliclack::confirm(message).initial_value(false).interact()?)
    }
    fn execute(&mut self, plan: &RepairPlan) -> Result<()> {
        cliclack::log::info(format!("Running {}", plan.display()))?;
        match plan {
            RepairPlan::Command { program, args } => {
                let status = std::process::Command::new(program)
                    .args(args)
                    .status()
                    .with_context(|| format!("starting {}", program.display()))?;
                if !status.success() {
                    bail!(
                        "installer exited with {status}; command: {}",
                        plan.display()
                    );
                }
                Ok(())
            }
            RepairPlan::KimiInstaller { root, version } => {
                run_kimi_standalone_installer(root, version)
            }
        }
    }
    fn detect(&mut self, harness: Harness) -> Option<Detected> {
        gh_harness::detect(harness)
    }
    fn candidates(&mut self, harness: Harness) -> Vec<Detected> {
        let mut seen = std::collections::HashSet::new();
        gh_harness::upstream_paths(harness)
            .into_iter()
            .filter(|path| {
                seen.insert(std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
            })
            .map(|path| gh_harness::detect_at(harness, path))
            .collect()
    }
}

pub(crate) fn ensure_compatible_version(
    harness: Harness,
    detected: Detected,
    policy: &HarnessPolicy,
    interactive: bool,
    runtime: &mut impl Runtime,
) -> Result<(Detected, HarnessContext, bool)> {
    let error = match resolve_compatibility(
        harness,
        detected.version.as_ref(),
        detected.raw_version.as_deref(),
        policy,
    ) {
        Ok(context) => return Ok((detected, context, false)),
        Err(error) if !error.is_installable() => return Err(error.into()),
        Err(error) => error,
    };
    let installation = runtime.installation(&detected);
    if !interactive || refusal(&installation.method).is_some() {
        return Err(ManualRepairRequired::new(error, installation, harness, policy).into());
    }
    let manual = guidance(&installation);
    let invocation = supported_install(harness, policy)?;
    let version = runtime.lookup(harness, &invocation)?;
    // A registry response is not authority to bypass the original policy.
    resolve_compatibility(harness, Some(&version), Some(&version.to_string()), policy)
        .context("published repair version is not policy-compatible")?;
    let plan = plan(&installation, &version)?;
    if !runtime.confirm(&format!(
        "{error}\n{manual}\nInstall {harness} {version} with {}?",
        plan.display()
    ))? {
        bail!("{harness} installation declined; {manual}");
    }
    runtime.execute(&plan)?;
    let refreshed = runtime.detect(harness).ok_or_else(|| {
        anyhow!(
            "installer completed ({}), but {harness} cannot be found on PATH",
            plan.display()
        )
    })?;
    let context = match resolve_compatibility(
        harness,
        refreshed.version.as_ref(),
        refreshed.raw_version.as_deref(),
        policy,
    ) {
        Ok(context) => context,
        Err(error) => {
            for candidate in runtime.candidates(harness) {
                if resolve_compatibility(
                    harness,
                    candidate.version.as_ref(),
                    candidate.raw_version.as_deref(),
                    policy,
                )
                .is_ok()
                {
                    bail!("installer completed, but PATH winner `{}` ({}) shadows compatible `{}`; move the compatible directory earlier on PATH, or uninstall the shadowing entry through its owner; {error}", refreshed.path.display(), refreshed.raw_version.as_deref().unwrap_or("unknown version"), candidate.path.display());
                }
            }
            bail!("repair command {} did not produce a usable installation: PATH winner `{}` ({}); no compatible copy was found on PATH; {error}", plan.display(), refreshed.path.display(), refreshed.raw_version.as_deref().unwrap_or("unknown version"));
        }
    };
    if refreshed.path != detected.path {
        cliclack::log::info(format!(
            "PATH winner changed from `{}` to `{}`",
            detected.path.display(),
            refreshed.path.display()
        ))?;
    }
    cliclack::log::success(format!(
        "{harness} {} is installed and supported at `{}`",
        context.version,
        refreshed.path.display()
    ))?;
    Ok((refreshed, context, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn detected(path: &str, version: &str) -> Detected {
        Detected {
            harness: Harness::Claude,
            path: path.into(),
            raw_version: Some(version.into()),
            version: Version::parse(version).ok(),
        }
    }
    fn installation(method: InstallMethod) -> Installation {
        Installation {
            executable: "/native/claude".into(),
            canonical: "/versions/old".into(),
            method,
        }
    }
    struct Fake {
        method: InstallMethod,
        calls: Vec<&'static str>,
        approved: bool,
        fails: bool,
        release: Result<Version>,
        winner: Option<Detected>,
        others: Vec<Detected>,
    }
    impl Default for Fake {
        fn default() -> Self {
            Self {
                method: InstallMethod::ClaudeNative,
                calls: Vec::new(),
                approved: true,
                fails: false,
                release: Ok(Version::new(2, 1, 252)),
                winner: Some(detected("/native/claude", "2.1.252")),
                others: Vec::new(),
            }
        }
    }
    impl Runtime for Fake {
        fn installation(&mut self, detected: &Detected) -> Installation {
            self.calls.push("ownership");
            Installation {
                executable: detected.path.clone(),
                ..installation(self.method.clone())
            }
        }
        fn lookup(&mut self, _: Harness, _: &InstallInvocation) -> Result<Version> {
            self.calls.push("lookup");
            self.release
                .as_ref()
                .map(Clone::clone)
                .map_err(|error| anyhow!(error.to_string()))
        }
        fn confirm(&mut self, message: &str) -> Result<bool> {
            self.calls.push("confirm");
            assert!(message.contains("/native/claude"));
            assert!(message.contains("2.1.252"));
            Ok(self.approved)
        }
        fn execute(&mut self, _: &RepairPlan) -> Result<()> {
            self.calls.push("execute");
            if self.fails {
                bail!("installer failure");
            }
            Ok(())
        }
        fn detect(&mut self, _: Harness) -> Option<Detected> {
            self.calls.push("detect");
            self.winner.clone()
        }
        fn candidates(&mut self, _: Harness) -> Vec<Detected> {
            self.calls.push("candidates");
            self.others.clone()
        }
    }
    fn run(fake: &mut Fake, interactive: bool) -> Result<(Detected, HarnessContext, bool)> {
        ensure_compatible_version(
            Harness::Claude,
            detected("/native/claude", "2.1.273"),
            &HarnessPolicy::default(),
            interactive,
            fake,
        )
    }

    #[test]
    fn plans_preserve_exact_argv_and_installation_owner() {
        let version = Version::new(2, 1, 252);
        assert_eq!(
            plan(&installation(InstallMethod::ClaudeNative), &version).unwrap(),
            RepairPlan::Command {
                program: "/native/claude".into(),
                args: vec!["install".into(), version.to_string()]
            }
        );
        for harness in Harness::ALL {
            let package = gh_harness::install::npm_package(harness);
            let prefix = PathBuf::from("/custom npm prefix");
            assert_eq!(
                plan(
                    &installation(InstallMethod::NpmGlobal {
                        prefix: prefix.clone(),
                        package: package.into()
                    }),
                    &version
                )
                .unwrap(),
                RepairPlan::Command {
                    program: "npm".into(),
                    args: vec![
                        "install".into(),
                        "-g".into(),
                        "--prefix".into(),
                        prefix.to_str().unwrap().into(),
                        format!("{package}@{version}")
                    ]
                }
            );
        }
        let mut opencode = installation(InstallMethod::OpenCodeStandalone);
        opencode.executable = "/home/user/.opencode/bin/opencode".into();
        assert_eq!(
            plan(&opencode, &version).unwrap(),
            RepairPlan::Command {
                program: opencode.executable.clone(),
                args: vec![
                    "upgrade".into(),
                    version.to_string(),
                    "--method".into(),
                    "curl".into()
                ]
            }
        );
        assert_eq!(
            plan(
                &installation(InstallMethod::KimiStandalone {
                    root: "/home/user/.kimi-code".into()
                }),
                &version
            )
            .unwrap(),
            RepairPlan::KimiInstaller {
                root: "/home/user/.kimi-code".into(),
                version
            }
        );
    }

    #[test]
    fn unsupported_and_noninteractive_never_lookup_confirm_or_install() {
        for method in [
            InstallMethod::Homebrew {
                package: "claude-code".into(),
            },
            InstallMethod::KimiUvLegacy,
            InstallMethod::Unknown {
                reason: "unverified wrapper".into(),
            },
        ] {
            assert!(plan(&installation(method.clone()), &Version::new(2, 1, 252)).is_err());
            let mut fake = Fake {
                method,
                ..Fake::default()
            };
            let error = run(&mut fake, true).unwrap_err().to_string();
            assert!(error.contains("/native/claude"));
            assert!(error.contains("npm install -g"));
            assert!(error.contains("<2."));
            assert_eq!(fake.calls, ["ownership"]);
        }
        let mut fake = Fake::default();
        assert!(run(&mut fake, false)
            .unwrap_err()
            .to_string()
            .contains("install <exact-policy-compatible-version>"));
        assert_eq!(fake.calls, ["ownership"]);
    }

    #[test]
    fn migration_uses_each_harness_plan_without_runtime_work() {
        for harness in Harness::ALL {
            for (method, path) in [
                (
                    InstallMethod::Homebrew {
                        package: harness.to_string(),
                    },
                    "/opt/homebrew/bin/agent",
                ),
                (
                    InstallMethod::Unknown {
                        reason: "pnpm/bun wrapper or broken link details".into(),
                    },
                    r"C:\tools\agent.cmd",
                ),
            ] {
                let mut fake = Fake {
                    method,
                    ..Fake::default()
                };
                let detected = Detected {
                    harness,
                    ..detected(path, "99.0.0")
                };
                let error = ensure_compatible_version(
                    harness,
                    detected,
                    &HarnessPolicy::default(),
                    true,
                    &mut fake,
                )
                .unwrap_err();
                let message = error.to_string();
                let expected = supported_install(harness, &HarnessPolicy::default()).unwrap();
                assert!(message.contains(&expected.display), "{message}");
                assert!(message.contains(path));
                assert!(!message.contains("wrapper or broken"));
                assert!(!message.contains("profile"));
                assert!(!message.contains("resolved"));
                assert_eq!(fake.calls, ["ownership"]);
            }
        }
    }

    #[test]
    fn homebrew_codex_refusal_is_readable_and_preserves_details() {
        let failure = resolve_compatibility(
            Harness::Codex,
            Some(&Version::new(0, 154, 0)),
            None,
            &HarnessPolicy::default(),
        )
        .unwrap_err();
        let error = ManualRepairRequired::new(
            failure,
            Installation {
                executable: "/opt/homebrew/bin/codex".into(),
                canonical: "/opt/homebrew/Caskroom/codex/0.154.0/bin/codex".into(),
                method: InstallMethod::Homebrew {
                    package: "codex".into(),
                },
            },
            Harness::Codex,
            &HarnessPolicy::default(),
        );
        assert_eq!(error.to_string(), "codex 0.154.0 is newer than Blue's tested versions.\nInstallation: Homebrew — /opt/homebrew/bin/codex\nBlue cannot automatically install a specific version with Homebrew.\n\nTo migrate to npm, remove this codex installation using Homebrew, then run (POSIX shell or PowerShell):\n  npm install -g '@openai/codex@>=0.145.0 <0.151.1-0'\nEnsure npm's executable directory is on PATH, then retry Blue.");
        assert!(format!("{error:?}").contains("/opt/homebrew/Caskroom/codex"));
        assert!(std::error::Error::source(&error)
            .unwrap()
            .to_string()
            .contains("codex-v0_145_0"));
    }

    #[test]
    fn migration_respects_policy_and_handles_no_intersection() {
        for (requirement, unverified) in [
            (">=0.146.0, <0.150.0", false),
            ("=0.150.0", false),
            (">=0.154.0, <0.155.0", true),
            (">=99.0.0", false),
        ] {
            let policy = HarnessPolicy {
                version_requirement: Some(requirement.into()),
                allow_unverified_versions: unverified,
                ..HarnessPolicy::default()
            };
            let mut fake = Fake {
                method: InstallMethod::Homebrew {
                    package: "codex".into(),
                },
                ..Fake::default()
            };
            let error = ensure_compatible_version(
                Harness::Codex,
                Detected {
                    harness: Harness::Codex,
                    ..detected("/brew/codex", "0.144.0")
                },
                &policy,
                true,
                &mut fake,
            )
            .unwrap_err()
            .to_string();
            match supported_install(Harness::Codex, &policy) {
                Ok(invocation) => assert!(error.contains(&invocation.display)),
                Err(_) => {
                    assert!(error.contains("Ask your administrator"));
                    assert!(!error.contains("npm install"));
                }
            }
            assert!(error.contains(requirement));
            assert_eq!(fake.calls, ["ownership"]);
        }
    }

    #[test]
    fn legacy_kimi_and_unreadable_versions_have_short_explanations() {
        let mut fake = Fake {
            method: InstallMethod::KimiUvLegacy,
            ..Fake::default()
        };
        let error = ensure_compatible_version(
            Harness::Kimi,
            Detected {
                harness: Harness::Kimi,
                ..detected("/uv/bin/kimi", "1.0.0")
            },
            &HarnessPolicy::default(),
            true,
            &mut fake,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("legacy Python version of Kimi"));
        assert!(error.contains("using uv"));
        assert!(error.contains("@moonshot-ai/kimi-code@"));
        assert_eq!(fake.calls, ["ownership"]);
        let summary = compatibility_summary(&CompatibilityFailure::UnparseableVersion {
            harness: Harness::Codex,
            raw: "very noisy output".into(),
        });
        assert_eq!(summary, "Blue could not read the installed codex version.");
    }

    #[test]
    fn migration_rejects_non_npm_and_unexpected_invocations() {
        for (program, args) in [
            ("brew", vec!["install", "-g", "codex@1.0.0"]),
            ("npm", vec!["install", "codex@1.0.0"]),
            ("npm", vec!["install", "-g", "codex@latest"]),
        ] {
            let invocation = InstallInvocation {
                program,
                args: args.into_iter().map(String::from).collect(),
                display: "do not show".into(),
            };
            assert!(npm_migration_command(&invocation).is_none());
        }
    }

    #[test]
    fn noninteractive_advice_preserves_each_supported_method() {
        for (method, expected) in [
            (
                InstallMethod::NpmGlobal {
                    prefix: "/custom npm prefix".into(),
                    package: "@anthropic-ai/claude-code".into(),
                },
                "npm install -g --prefix \"/custom npm prefix\"",
            ),
            (
                InstallMethod::ClaudeNative,
                "install <exact-policy-compatible-version>",
            ),
            (
                InstallMethod::OpenCodeStandalone,
                "upgrade <exact-policy-compatible-version> --method curl",
            ),
            (
                InstallMethod::KimiStandalone {
                    root: "/custom kimi root".into(),
                },
                "KIMI_INSTALL_DIR=\"/custom kimi root\"",
            ),
        ] {
            let mut fake = Fake {
                method,
                ..Fake::default()
            };
            let message = run(&mut fake, false).unwrap_err().to_string();
            assert!(message.contains("Automatic repair requires an interactive terminal."));
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains("migrate"));
            assert_eq!(fake.calls, ["ownership"]);
        }
    }

    #[test]
    fn decline_and_installer_failure_stop_before_detection() {
        let mut fake = Fake {
            approved: false,
            ..Fake::default()
        };
        assert!(run(&mut fake, true)
            .unwrap_err()
            .to_string()
            .contains("declined"));
        assert_eq!(fake.calls, ["ownership", "lookup", "confirm"]);
        let mut fake = Fake {
            fails: true,
            ..Fake::default()
        };
        assert!(run(&mut fake, true)
            .unwrap_err()
            .to_string()
            .contains("installer failure"));
        assert_eq!(fake.calls, ["ownership", "lookup", "confirm", "execute"]);
    }

    #[test]
    fn missing_lookup_prerequisite_and_out_of_policy_response_do_not_install() {
        for release in [
            Err(anyhow!("npm executable unavailable")),
            Ok(Version::new(999, 0, 0)),
        ] {
            let mut fake = Fake {
                release,
                ..Fake::default()
            };
            assert!(run(&mut fake, true).is_err());
            assert_eq!(fake.calls, ["ownership", "lookup"]);
        }
    }

    #[test]
    fn fresh_winner_can_keep_or_change_entry_path() {
        for path in ["/native/claude", "/new-prefix/bin/claude"] {
            let mut fake = Fake {
                winner: Some(detected(path, "2.1.252")),
                ..Fake::default()
            };
            let (actual, context, repaired) = run(&mut fake, true).unwrap();
            assert_eq!(actual.path, Path::new(path));
            assert_eq!(context.version, Version::new(2, 1, 252));
            assert!(repaired);
            assert_eq!(
                fake.calls,
                ["ownership", "lookup", "confirm", "execute", "detect"]
            );
        }
    }

    #[test]
    fn missing_winner_and_shadowing_have_actionable_diagnostics() {
        let mut fake = Fake {
            winner: None,
            ..Fake::default()
        };
        assert!(run(&mut fake, true)
            .unwrap_err()
            .to_string()
            .contains("cannot be found on PATH"));
        assert_eq!(
            fake.calls,
            ["ownership", "lookup", "confirm", "execute", "detect"]
        );
        for version in ["2.1.273", "unparseable"] {
            let mut fake = Fake {
                winner: Some(detected("/shadow/claude", version)),
                others: vec![detected("/compatible/claude", "2.1.252")],
                ..Fake::default()
            };
            let error = run(&mut fake, true).unwrap_err().to_string();
            assert!(error.contains("/shadow/claude"));
            assert!(error.contains("/compatible/claude"));
            assert!(error.contains("earlier on PATH"));
            fake.others.clear();
            let error = run(&mut fake, true).unwrap_err().to_string();
            assert!(error.contains("no compatible copy"));
            assert!(error.contains("/shadow/claude"));
        }
    }

    #[test]
    fn compatible_and_invalid_policy_paths_do_not_probe_ownership() {
        let mut fake = Fake::default();
        let result = ensure_compatible_version(
            Harness::Claude,
            detected("/native/claude", "2.1.252"),
            &HarnessPolicy::default(),
            true,
            &mut fake,
        )
        .unwrap();
        assert!(!result.2);
        let policy = HarnessPolicy {
            version_requirement: Some("invalid".into()),
            ..HarnessPolicy::default()
        };
        assert!(ensure_compatible_version(
            Harness::Claude,
            detected("/native/claude", "2.1.252"),
            &policy,
            true,
            &mut fake
        )
        .is_err());
        assert!(fake.calls.is_empty());
    }

    #[test]
    fn kimi_command_preserves_root_and_disables_path_changes_and_migration() {
        let root = Path::new("/home/test/.kimi-code");
        let path = std::env::join_paths([
            root.join("bin"),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
        ])
        .unwrap();
        let child_path = kimi_installer_path(root, &path).unwrap();
        assert!(!std::env::split_paths(&child_path).any(|entry| entry == root.join("bin")));
        let command = kimi_installer_command(root, &Version::new(1, 2, 3), &child_path);
        assert_eq!(command.get_program(), "bash");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["-s", "--", "--version", "1.2.3"]
        );
        let env = command
            .get_envs()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            env.get(std::ffi::OsStr::new("KIMI_INSTALL_DIR")),
            Some(&Some(root.as_os_str()))
        );
        assert_eq!(
            env.get(std::ffi::OsStr::new("KIMI_NO_MODIFY_PATH")),
            Some(&Some(std::ffi::OsStr::new("1")))
        );
        assert_eq!(
            env.get(std::ffi::OsStr::new("PATH")),
            Some(&Some(child_path.as_os_str()))
        );
    }

    #[test]
    fn npm_version_lookup_selects_highest_and_rejects_bad_results() {
        assert_eq!(
            highest_npm_version(br#"["1.18.23","1.18.25","1.18.24"]"#).unwrap(),
            Version::new(1, 18, 25)
        );
        assert_eq!(
            highest_npm_version(br#""v1.18.25""#).unwrap(),
            Version::new(1, 18, 25)
        );
        for bad in [
            b"[]".as_slice(),
            b"{}",
            b"null",
            b"not JSON",
            br#"["1.2.3", false]"#,
            br#"["1.2.3", "bad"]"#,
            br#"["bad"]"#,
        ] {
            assert!(highest_npm_version(bad).is_err());
        }
    }
}
