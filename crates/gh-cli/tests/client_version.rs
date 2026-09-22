//! Exercise actual dispatch, including the interactive bare entrypoint.
#![cfg(unix)]
use std::fs;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

struct Fixture(PathBuf);
impl Fixture {
    fn new(required: &str) -> Self {
        let root = std::env::temp_dir().join(format!("blue-version-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("config/blue")).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("config/blue/blue.yaml"), format!("revision: mismatch\nrequired_client_version: {required}\nallowed_harnesses: [codex]\nharnesses:\n  codex:\n    managed_config:\n      model: must-not-be-written\n")).unwrap();
        fs::write(
            root.join("config/blue/blue.toml"),
            "[service]\n[ui]\npreferred_harness = \"codex\"\n",
        )
        .unwrap();
        fs::write(
            root.join("bin/codex"),
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'codex-cli 0.150.0'; exit 0; fi\necho launched > \"$BLUE_TEST_MARKER\"\n",
        )
        .unwrap();
        fs::set_permissions(root.join("bin/codex"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_blue"), root.join("bin/blue")).unwrap();
        Self(root)
    }
    fn command(&self) -> Command {
        let mut command = Command::new(self.0.join("bin/blue"));
        command
            .current_dir(&self.0)
            .env("HOME", &self.0)
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_CACHE_HOME", self.0.join("cache"))
            .env("PATH", self.0.join("bin"))
            .env("BLUE_TEST_MARKER", self.0.join("launched"))
            .env("TERM", "xterm");
        command
    }
    fn assert_blocked(&self, output: &str, required: &str) {
        assert!(
            output.contains(&format!("tenant requires Blue {required}")),
            "{output}"
        );
        assert!(output.contains("blue reset"), "{output}");
        assert!(!self.0.join("launched").exists());
        assert!(!self.0.join("cache/blue/governance-config.json").exists());
        assert_eq!(
            fs::read_to_string(self.0.join("config/blue/blue.toml")).unwrap(),
            "[service]\n[ui]\npreferred_harness = \"codex\"\n"
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_pty(
    fixture: &Fixture,
    args: &[&str],
    decline_prompt: Option<&str>,
) -> (std::process::ExitStatus, String, bool) {
    let (mut master, mut slave) = (0, 0);
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    let mut master = unsafe { fs::File::from_raw_fd(master) };
    let slave = unsafe { fs::File::from_raw_fd(slave) };
    let mut child = fixture
        .command()
        .args(args)
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave))
        .spawn()
        .unwrap();
    let mut reader = master.try_clone().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = [0; 4096];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 || sender.send(buffer[..count].to_vec()).is_err() {
                break;
            }
        }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut output = String::new();
    let mut declined = false;
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        match receiver.recv_timeout(remaining) {
            Ok(bytes) => {
                output.push_str(&String::from_utf8_lossy(&bytes));
                if !declined && decline_prompt.is_some_and(|prompt| output.contains(prompt)) {
                    master.write_all(b"n\r").unwrap();
                    declined = true;
                }
            }
            Err(_) => break,
        }
    }
    let status = match child.try_wait().unwrap() {
        Some(status) => status,
        None => {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("Blue did not terminate: {output}");
        }
    };
    (status, output, declined)
}

fn run_pty_without_input(fixture: &Fixture, args: &[&str]) -> (std::process::ExitStatus, String) {
    let (status, output, responded) = run_pty(fixture, args, None);
    assert!(!responded);
    (status, output)
}

#[test]
fn explicit_shorthand_and_apply_fail_before_harness_detection_or_managed_writes() {
    for required in ["99.0.0"] {
        let fixture = Fixture::new(required);
        for args in [
            vec!["run", "codex"],
            vec!["codex"],
            vec!["apply", "--yes"],
            vec!["config"],
            vec!["doctor"],
        ] {
            let output = fixture.command().args(args).output().unwrap();
            assert!(!output.status.success());
            fixture.assert_blocked(&String::from_utf8_lossy(&output.stderr), required);
        }
        // A malformed policy proves reset has no dependency on a usable fetch.
        fs::write(fixture.0.join("config/blue/blue.yaml"), "not valid: [").unwrap();
        let output = fixture.command().args(["reset", "--yes"]).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!fixture.0.join("config/blue/blue.toml").exists());
    }
}

#[test]
fn same_major_commands_continue_with_one_noninteractive_recommendation() {
    for args in [
        vec!["run", "codex"],
        vec!["codex"],
        vec!["apply", "--yes"],
        vec!["config"],
        vec!["doctor"],
    ] {
        let fixture = Fixture::new("0.0.1");
        let output = fixture.command().args(args).output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert_eq!(
            stderr
                .matches("is compatible, but this tenant recommends")
                .count(),
            1,
            "{stderr}"
        );
        assert!(!stderr.contains("Install Blue"), "{stderr}");
        assert!(fixture.0.join("cache/blue/governance-config.json").exists());
    }
}

#[test]
fn same_major_interactive_decline_continues_the_command() {
    let installed = gh_common::blue_version();
    let mut version = semver::Version::parse(installed).unwrap();
    version.patch += 1;
    version.pre = semver::Prerelease::EMPTY;
    version.build = semver::BuildMetadata::EMPTY;
    let required = version.to_string();
    let fixture = Fixture::new(&required);
    let prompt = format!("Install Blue {required} now");
    let (status, output, declined) = run_pty(&fixture, &["config"], Some(&prompt));

    assert!(declined, "no compatible install prompt: {output}");
    assert!(status.success(), "{output}");
    assert!(output.contains("\"revision\": \"mismatch\""), "{output}");
}

#[test]
fn apply_yes_never_offers_client_replacement_in_a_terminal() {
    let installed = gh_common::blue_version();
    let mut version = semver::Version::parse(installed).unwrap();
    version.patch += 1;
    version.pre = semver::Prerelease::EMPTY;
    version.build = semver::BuildMetadata::EMPTY;
    let compatible = Fixture::new(&version.to_string());
    let (status, output) = run_pty_without_input(&compatible, &["apply", "--yes"]);
    assert!(status.success(), "{output}");
    assert!(
        output.contains("is compatible, but this tenant recommends"),
        "{output}"
    );
    assert!(!output.contains("Install Blue"), "{output}");

    let incompatible = Fixture::new("99.0.0");
    let (status, output) = run_pty_without_input(&incompatible, &["apply", "--yes"]);
    assert!(!status.success(), "{output}");
    assert!(!output.contains("Install Blue 99.0.0 now"), "{output}");
    incompatible.assert_blocked(&output, "99.0.0");
}

#[test]
fn bare_blue_decline_remains_blocked_and_never_launches() {
    let fixture = Fixture::new("99.0.0");
    let (status, output, declined) = run_pty(&fixture, &[], Some("Install Blue 99.0.0 now"));
    assert!(!status.success());
    assert!(declined, "no install prompt: {output}");
    fixture.assert_blocked(&output, "99.0.0");
}
