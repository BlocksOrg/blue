//! Foreground-only, consent-based repair of the Blue executable itself.
use anyhow::{bail, Context, Result};
use gh_service::GovernanceConfig;
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const RELEASES: &str = "https://github.com/BlocksOrg/blue/releases";

pub(crate) fn recommendation(installed: &str, recommended: &str, os: &str, arch: &str) -> String {
    let mut message = format!(
        "Blue {installed} is compatible, but this tenant recommends Blue {recommended}.\nRelease: {RELEASES}/tag/v{recommended}\n"
    );
    match (target(os, arch), stable_pin(recommended)) {
        (Some(target), Ok(())) => {
            let extension = if os == "windows" { "zip" } else { "tar.gz" };
            message.push_str(&format!(
                "Asset: blue-v{recommended}-{target}.{extension}\n"
            ));
            if os == "windows" {
                message.push_str(&format!("$env:BLUE_VERSION = \"v{recommended}\"\nirm {RELEASES}/download/v{recommended}/install.ps1 | iex\n"));
            } else {
                message.push_str(&format!("curl --proto '=https' --tlsv1.2 -LsSf {RELEASES}/download/v{recommended}/install.sh | BLUE_VERSION=v{recommended} sh\n"));
            }
        }
        _ => message.push_str(
            "Automatic installation is unavailable for this target or release. Consult the release page.\n",
        ),
    }
    message
}

pub(crate) fn warn_if_recommended(config: &GovernanceConfig) {
    let Some(required) = config.required_client_version.as_deref() else {
        return;
    };
    if let Ok(gh_service::ClientVersionCompatibility::SameMajorRecommendation {
        installed,
        recommended,
    }) = GovernanceConfig::client_version_compatibility(required, env!("CARGO_PKG_VERSION"))
    {
        eprintln!(
            "{}",
            recommendation(
                &installed,
                &recommended,
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        );
    }
}

fn target(os: &str, arch: &str) -> Option<String> {
    let arch = match arch {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => return None,
    };
    let os = match os {
        "linux" => "unknown-linux-musl",
        "macos" => "apple-darwin",
        "windows" => "pc-windows-msvc",
        _ => return None,
    };
    Some(format!("{arch}-{os}"))
}

fn stable_pin(required: &str) -> Result<()> {
    let version = GovernanceConfig::validate_client_version_pin(required)?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        bail!("automatic installation supports only published stable MAJOR.MINOR.PATCH releases");
    }
    Ok(())
}

pub(crate) fn remedy(installed: &str, required: &str, os: &str, arch: &str) -> String {
    let mut message = format!("Blue {installed} has an incompatible major version; this tenant requires Blue {required}.\nRelease: {RELEASES}/tag/v{required}\n");
    match (target(os, arch), stable_pin(required)) {
        (Some(target), Ok(())) => {
            let extension = if os == "windows" { "zip" } else { "tar.gz" };
            message.push_str(&format!("Asset: blue-v{required}-{target}.{extension}\n\n"));
            if os == "windows" {
                message.push_str(&format!("$env:BLUE_VERSION = \"v{required}\"\nirm {RELEASES}/download/v{required}/install.ps1 | iex\n"));
            } else {
                message.push_str(&format!("curl --proto '=https' --tlsv1.2 -LsSf {RELEASES}/download/v{required}/install.sh | BLUE_VERSION=v{required} sh\n"));
            }
        }
        _ => message.push_str("Automatic installation is unavailable for this target or release. Consult the release page.\n"),
    }
    message.push_str("\nOr run `blue reset` to detach from this tenant.");
    message
}

trait Runtime {
    fn destination(&mut self) -> Result<PathBuf>;
    fn confirm(&mut self, message: &str) -> Result<bool>;
    fn install(&mut self, destination: &Path, required: &str) -> Result<()>;
}

fn repair(
    runtime: &mut dyn Runtime,
    installed: &str,
    required: &str,
    interactive: bool,
) -> Result<Option<PathBuf>> {
    stable_pin(required)?;
    if !interactive {
        return Ok(None);
    }
    let destination = runtime.destination()?;
    let direction = if semver::Version::parse(installed)? < semver::Version::parse(required)? {
        "upgrade"
    } else {
        "downgrade"
    };
    if !runtime.confirm(&format!(
        "Blue {installed} must {direction} to {required}. Install Blue {required} now at {}?",
        destination.display()
    ))? {
        return Ok(None);
    }
    runtime.install(&destination, required)?;
    Ok(Some(destination))
}

/// Returns true when this is the specific error handled here. The caller always
/// exits unsuccessfully: even a successful repair did not run the user's command.
pub(crate) fn handle(error: &anyhow::Error, foreground: bool) -> bool {
    let Some(gh_common::GhError::ClientVersionMismatch {
        installed,
        required,
    }) = error.downcast_ref::<gh_common::GhError>()
    else {
        return false;
    };
    eprintln!(
        "{}",
        remedy(
            installed,
            required,
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    );
    if target(std::env::consts::OS, std::env::consts::ARCH).is_none() {
        return true;
    }
    let interactive =
        foreground && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    match repair(&mut NativeRuntime, installed, required, interactive) {
        Ok(Some(path)) => eprintln!(
            "Installed Blue {required} at {}. Rerun your command to continue.",
            path.display()
        ),
        Ok(None) => {}
        Err(error) => {
            eprintln!("Blue installation did not complete: {error:#}. Use the manual remedy above.")
        }
    }
    true
}

struct NativeRuntime;
impl Runtime for NativeRuntime {
    fn destination(&mut self) -> Result<PathBuf> {
        let path = std::env::current_exe()?.canonicalize()?;
        check_destination(&path)?;
        // Check directory writability before asking for consent.
        let _probe = AttemptDirectory::new(path.parent().context("executable has no parent")?)?;
        Ok(path)
    }
    fn confirm(&mut self, message: &str) -> Result<bool> {
        Ok(cliclack::confirm(message).initial_value(false).interact()?)
    }
    fn install(&mut self, destination: &Path, required: &str) -> Result<()> {
        install(destination, required)
    }
}

fn check_destination(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.permissions().readonly() {
        bail!("destination is not a writable regular executable");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let parent = fs::metadata(path.parent().context("executable has no parent")?)?;
        // Avoid replacing another user's or a shared/package-linked executable.
        let uid = unsafe { libc::geteuid() };
        if metadata.uid() != uid
            || parent.uid() != uid
            || parent.mode() & 0o022 != 0
            || metadata.nlink() != 1
        {
            bail!("destination ownership, directory permissions, or hard-link layout requires manual installation");
        }
    }
    Ok(())
}

struct AttemptDirectory(PathBuf);
impl AttemptDirectory {
    fn new(parent: &Path) -> Result<Self> {
        let path = parent.join(format!(".blue-install-{}", uuid::Uuid::new_v4()));
        #[cfg(unix)]
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        #[cfg(not(unix))]
        let builder = fs::DirBuilder::new();
        builder.create(&path)?;
        Ok(Self(path))
    }
}
impl Drop for AttemptDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// File-backed output avoids a full stdout pipe deadlocking the child. All
// processes have a deadline; installer descendants are terminated on timeout.
fn bounded_command(command: &mut Command, directory: &Path, timeout: Duration) -> Result<String> {
    let output_path = directory.join(format!("output-{}", uuid::Uuid::new_v4()));
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output_path)?;
    command
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .context("starting installer or version probe")?;
    let started = Instant::now();
    let mut heartbeat = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            #[cfg(windows)]
            {
                let _ = Command::new("taskkill.exe")
                    .args(["/PID", &child.id().to_string(), "/T", "/F"])
                    .status();
            }
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "installer or version probe timed out after {} seconds",
                timeout.as_secs()
            );
        }
        if heartbeat.elapsed() >= Duration::from_secs(15) {
            eprintln!(
                "Waiting for Blue installer ({} seconds elapsed)…",
                started.elapsed().as_secs()
            );
            heartbeat = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut output = String::new();
    File::open(&output_path)?
        .take(64 * 1024)
        .read_to_string(&mut output)?;
    if !status.success() {
        bail!(
            "installer or version probe exited with {status}: {}",
            gh_service::source::bounded_detail(&output).unwrap_or_default()
        );
    }
    Ok(output)
}

fn probe(path: &Path, required: &str, directory: &Path) -> Result<()> {
    let output = bounded_command(
        Command::new(path).arg("version"),
        directory,
        Duration::from_secs(10),
    )?;
    if output.trim() != format!("Blue metaharness {required}") {
        bail!("{} did not report Blue {required}", path.display());
    }
    Ok(())
}

fn path_executable() -> Option<PathBuf> {
    let name = if cfg!(windows) { "blue.exe" } else { "blue" };
    let mut dirs: Vec<PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    if cfg!(windows) {
        dirs.insert(0, std::env::current_dir().ok()?);
    }
    dirs.into_iter().map(|dir| dir.join(name)).find(|path| {
        let Ok(metadata) = fs::metadata(path) else {
            return false;
        };
        if !metadata.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    })
}

fn replace_verified(
    destination: &Path,
    staged: &Path,
    required: &str,
    directory: &Path,
) -> Result<()> {
    replace_with_verification(destination, staged, directory, || {
        probe(destination, required, directory).and_then(|()| {
            if let Some(selected) = path_executable() {
                if selected.canonicalize()? != destination {
                    probe(&selected, required, directory).with_context(|| {
                        format!("PATH selects a different Blue at {}", selected.display())
                    })?;
                }
            }
            Ok(())
        })
    })
}

fn replace_with_verification(
    destination: &Path,
    staged: &Path,
    _directory: &Path,
    verify: impl FnOnce() -> Result<()>,
) -> Result<()> {
    check_destination(destination)?;
    let backup = destination.with_file_name(format!(".blue-backup-{}", uuid::Uuid::new_v4()));
    #[cfg(unix)]
    fs::hard_link(destination, &backup).context("backing up current executable")?;
    #[cfg(windows)]
    fs::rename(destination, &backup).context("backing up running executable")?;
    if let Err(error) = fs::rename(staged, destination) {
        #[cfg(windows)]
        fs::rename(&backup, destination).with_context(|| {
            format!(
                "replacement and rollback failed; original executable retained at {}",
                backup.display()
            )
        })?;
        #[cfg(unix)]
        let _ = fs::remove_file(&backup);
        return Err(error).context("replacing executable");
    }
    let verification = verify();
    if let Err(error) = verification {
        #[cfg(windows)]
        fs::rename(destination, _directory.join("rejected-blue.exe"))?;
        fs::rename(&backup, destination).with_context(|| {
            format!(
                "rollback failed; original executable retained at {}",
                backup.display()
            )
        })?;
        return Err(error);
    }
    // Retain and name a backup which Windows still maps into this process.
    if let Err(error) = fs::remove_file(&backup) {
        eprintln!(
            "Previous executable retained at {}: {error}",
            backup.display()
        );
    }
    Ok(())
}

fn install(destination: &Path, required: &str) -> Result<()> {
    stable_pin(required)?;
    check_destination(destination)?;
    let parent = destination.parent().context("executable has no parent")?;
    // Keep the lock inode permanently: unlinking an unlocked file races another
    // process which already opened it. OS locks release on crash as well.
    let lock_path = parent.join(format!(
        ".{}.install.lock",
        destination
            .file_name()
            .context("executable has no name")?
            .to_string_lossy()
    ));
    if fs::symlink_metadata(&lock_path).is_ok_and(|metadata| !metadata.is_file()) {
        bail!("unsafe installation lock");
    }
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options.open(lock_path)?;
    lock.try_lock()
        .context("another Blue installation is in progress")?;
    let attempt = AttemptDirectory::new(parent)?;
    let directory = &attempt.0;
    let script_name = if cfg!(windows) {
        "install.ps1"
    } else {
        "install.sh"
    };
    let script = directory.join(script_name);
    let mut response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?
        .get(format!("{RELEASES}/download/v{required}/{script_name}"))
        .send()?
        .error_for_status()?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut response)
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        bail!("release installer is too large");
    }
    fs::write(&script, bytes)?;
    let staging = directory.join("staging");
    fs::create_dir(&staging)?;
    let mut command = if cfg!(windows) {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ]);
        command
    } else {
        Command::new("sh")
    };
    command
        .arg(&script)
        .env("BLUE_VERSION", format!("v{required}"))
        .env("BLUE_INSTALL_DIR", &staging)
        .env("BLUE_REPOSITORY", "BlocksOrg/blue")
        .env("BLUE_UPDATE_PATH", "0");
    bounded_command(&mut command, directory, Duration::from_secs(300))?;
    let staged = staging.join(if cfg!(windows) { "blue.exe" } else { "blue" });
    if !fs::symlink_metadata(&staged)?.is_file() {
        bail!("installer did not produce a regular executable");
    }
    probe(&staged, required, directory)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&staged)?
        .sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
    }
    replace_verified(destination, &staged, required, directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        consent: Option<bool>,
        confirms: usize,
        installs: usize,
        fail: bool,
    }
    impl Runtime for Fake {
        fn destination(&mut self) -> Result<PathBuf> {
            Ok("/fixture/blue".into())
        }
        fn confirm(&mut self, message: &str) -> Result<bool> {
            self.confirms += 1;
            assert!(message.contains("/fixture/blue"));
            self.consent.context("cancelled")
        }
        fn install(&mut self, _: &Path, _: &str) -> Result<()> {
            self.installs += 1;
            if self.fail {
                bail!("fixture installer failure");
            }
            Ok(())
        }
    }
    #[test]
    fn recommendation_is_neutral_and_does_not_invoke_repair() {
        for (installed, recommended) in [("1.2.2", "1.2.3"), ("1.3.0", "1.2.3")] {
            let message = recommendation(installed, recommended, "linux", "x86_64");
            assert!(message.starts_with(&format!(
                "Blue {installed} is compatible, but this tenant recommends Blue {recommended}."
            )));
            assert!(!message.contains("must upgrade"));
            assert!(!message.contains("must downgrade"));
            assert!(!message.contains("Install Blue"));
        }
    }
    #[test]
    fn consent_is_required_for_upgrade_and_downgrade() {
        for installed in ["1.0.0", "3.0.0"] {
            for consent in [Some(true), Some(false), None] {
                for interactive in [true, false] {
                    let mut fake = Fake {
                        consent,
                        confirms: 0,
                        installs: 0,
                        fail: false,
                    };
                    let result = repair(&mut fake, installed, "2.0.0", interactive);
                    assert_eq!(fake.confirms, usize::from(interactive));
                    assert_eq!(
                        fake.installs,
                        usize::from(interactive && consent == Some(true))
                    );
                    match (interactive, consent) {
                        (true, None) => assert!(result.is_err()),
                        (true, Some(true)) => {
                            assert_eq!(result.unwrap(), Some(PathBuf::from("/fixture/blue")))
                        }
                        _ => assert!(result.unwrap().is_none()),
                    }
                }
            }
        }
        let mut fake = Fake {
            consent: Some(true),
            confirms: 0,
            installs: 0,
            fail: true,
        };
        assert!(repair(&mut fake, "1.0.0", "2.0.0", true).is_err());
    }
    #[test]
    fn foreground_commands_do_not_grant_implicit_consent() {
        use clap::Parser;
        for args in [
            vec!["blue"],
            vec!["blue", "run", "codex"],
            vec!["blue", "codex"],
            vec!["blue", "apply", "--yes"],
        ] {
            let cli = crate::Cli::try_parse_from(args).unwrap();
            assert!(crate::foreground_command(&cli.command));
            let mut fake = Fake {
                consent: Some(false),
                confirms: 0,
                installs: 0,
                fail: false,
            };
            assert!(repair(&mut fake, "1.0.0", "2.0.0", true).unwrap().is_none());
            assert_eq!(fake.installs, 0);
        }
        for args in [
            vec!["blue", "daemon"],
            vec!["blue", "session-upload", "codex"],
            vec!["blue", "session-start", "codex"],
            vec!["blue", "session-upload-worker"],
        ] {
            let cli = crate::Cli::try_parse_from(args).unwrap();
            assert!(!crate::foreground_command(&cli.command));
        }
    }
    #[test]
    fn installation_lock_excludes_another_attempt() {
        let root = AttemptDirectory::new(&std::env::temp_dir()).unwrap();
        let path = root.0.join("install.lock");
        let first = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        first.try_lock().unwrap();
        let second = OpenOptions::new().write(true).open(&path).unwrap();
        assert!(second.try_lock().is_err());
        drop(first);
        second.try_lock().unwrap();
    }

    #[test]
    fn remedies_for_all_published_targets() {
        for (os, suffix, ext) in [
            ("macos", "apple-darwin", "tar.gz"),
            ("linux", "unknown-linux-musl", "tar.gz"),
            ("windows", "pc-windows-msvc", "zip"),
        ] {
            for arch in ["x86_64", "aarch64"] {
                let actual = remedy("1.0.0", "2.0.0", os, arch);
                let command = if os == "windows" {
                    "$env:BLUE_VERSION = \"v2.0.0\"\nirm https://github.com/BlocksOrg/blue/releases/download/v2.0.0/install.ps1 | iex"
                } else {
                    "curl --proto '=https' --tlsv1.2 -LsSf https://github.com/BlocksOrg/blue/releases/download/v2.0.0/install.sh | BLUE_VERSION=v2.0.0 sh"
                };
                assert_eq!(actual, format!("Blue 1.0.0 has an incompatible major version; this tenant requires Blue 2.0.0.\nRelease: https://github.com/BlocksOrg/blue/releases/tag/v2.0.0\nAsset: blue-v2.0.0-{arch}-{suffix}.{ext}\n\n{command}\n\nOr run `blue reset` to detach from this tenant."));
            }
        }
        let unsupported = remedy("1.0.0", "2.0.0", "linux", "riscv64");
        assert!(unsupported.contains("unavailable"));
        assert!(unsupported.contains("blue reset"));
        assert!(!unsupported.contains("curl"));
    }
    #[cfg(unix)]
    fn fixture_binary(path: &Path, version: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(
            path,
            format!("#!/bin/sh\nprintf 'Blue metaharness {version}\\n'\n"),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn native_replacement_and_failed_verification_restore_original() {
        let root = AttemptDirectory::new(&std::env::temp_dir()).unwrap();
        let destination = root.0.join("blue");
        let staged = root.0.join("next-blue");
        fixture_binary(&destination, "1.0.0");
        fixture_binary(&staged, "2.0.0");
        replace_with_verification(&destination, &staged, &root.0, || {
            probe(&destination, "2.0.0", &root.0)
        })
        .unwrap();
        probe(&destination, "2.0.0", &root.0).unwrap();
        let before = fs::read(&destination).unwrap();
        fixture_binary(&staged, "9.9.9");
        assert!(
            replace_with_verification(&destination, &staged, &root.0, || probe(
                &destination,
                "2.0.0",
                &root.0
            ))
            .is_err()
        );
        assert_eq!(before, fs::read(&destination).unwrap());
    }
    #[test]
    #[cfg(unix)]
    fn wrong_version_readonly_and_failed_installer_do_not_replace() {
        use std::os::unix::fs::PermissionsExt;
        let root = AttemptDirectory::new(&std::env::temp_dir()).unwrap();
        let destination = root.0.join("blue");
        fixture_binary(&destination, "1.0.0");
        assert!(probe(&destination, "2.0.0", &root.0).is_err());
        assert!(bounded_command(
            Command::new("sh").args(["-c", "echo checksum verification failed; exit 1"]),
            &root.0,
            Duration::from_secs(2)
        )
        .is_err());
        probe(&destination, "1.0.0", &root.0).unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o555)).unwrap();
        assert!(check_destination(&destination).is_err());
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    #[test]
    fn running_executable_fixture() {
        if std::env::var_os("BLUE_RUNNING_EXECUTABLE_FIXTURE").is_some() {
            println!("blue-fixture-ready");
            std::thread::sleep(Duration::from_secs(20));
        }
    }
    #[test]
    fn locked_windows_destination_remains_intact() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = AttemptDirectory::new(&std::env::temp_dir()).unwrap();
        let destination = root.0.join("blue.exe");
        let staged = root.0.join("next.exe");
        fs::write(&destination, b"original").unwrap();
        fs::write(&staged, b"replacement").unwrap();
        let locked = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&destination)
            .unwrap();
        assert!(replace_with_verification(&destination, &staged, &root.0, || Ok(())).is_err());
        drop(locked);
        assert_eq!(fs::read(&destination).unwrap(), b"original");
    }

    #[test]
    fn replacing_running_windows_executable_preserves_a_working_installation() {
        use std::io::BufRead;
        let root = AttemptDirectory::new(&std::env::temp_dir()).unwrap();
        let destination = root.0.join("blue.exe");
        let staged = root.0.join("next-blue.exe");
        fs::copy(std::env::current_exe().unwrap(), &destination).unwrap();
        fs::copy(&destination, &staged).unwrap();
        let original = fs::read(&destination).unwrap();
        let mut child = Command::new(&destination)
            .args([
                "--exact",
                "client_version::windows_tests::running_executable_fixture",
                "--nocapture",
            ])
            .env("BLUE_RUNNING_EXECUTABLE_FIXTURE", "1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(|line| line.ok())
            {
                if line.contains("blue-fixture-ready") {
                    let _ = sender.send(());
                    break;
                }
            }
        });
        let ready = receiver.recv_timeout(Duration::from_secs(10));
        if ready.is_err() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("fixture did not start");
        }
        let result = replace_with_verification(&destination, &staged, &root.0, || Ok(()));
        // Either native rename succeeds or the filesystem refuses sharing;
        // refusal must leave the original working executable at its path.
        assert_eq!(fs::read(&destination).unwrap(), original, "{result:?}");
        let _ = child.kill();
        child.wait().unwrap();
        fs::copy(&destination, &staged).unwrap();
        assert!(
            replace_with_verification(&destination, &staged, &root.0, || anyhow::bail!(
                "failed installed probe"
            ))
            .is_err()
        );
        assert_eq!(fs::read(&destination).unwrap(), original);
    }
}
