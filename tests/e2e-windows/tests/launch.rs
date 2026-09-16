//! Drives the built blue executable, including its interactive supervisor.
#![cfg(windows)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

// Known Folders cannot be isolated by changing USERPROFILE. Reserve only new
// directories and refuse to run against an existing Blue/Codex installation.
struct OwnedDirs(Vec<PathBuf>);
impl OwnedDirs {
    fn reserve(&mut self, path: PathBuf) {
        std::fs::create_dir(&path).unwrap_or_else(|e| {
            panic!(
                "requires a fresh Windows test account: {}: {e}",
                path.display()
            )
        });
        self.0.push(path);
    }
}
impl Drop for OwnedDirs {
    fn drop(&mut self) {
        for path in self.0.iter().rev() {
            if let Err(error) = std::fs::remove_dir_all(path) {
                eprintln!("cleanup {}: {error}", path.display());
            }
        }
    }
}

struct PtyChild(Box<dyn portable_pty::Child + Send + Sync>);
impl Drop for PtyChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            if let Some(pid) = self.0.process_id() {
                let _ = std::process::Command::new("taskkill.exe")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .output();
            }
            let _ = self.0.kill();
        }
    }
}

fn drain_output(rx: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>) {
    for bytes in rx.try_iter() {
        output.extend(bytes);
    }
}

fn run_pty(
    blue: &Path,
    args: &[&str],
    env: &BTreeMap<String, String>,
    input: bool,
) -> (u32, String) {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(blue);
    command.args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = PtyChild(pair.slave.spawn_command(command).unwrap());
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = [0; 8192];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 || tx.send(buffer[..count].to_vec()).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut heartbeat = Instant::now();
    let mut output = Vec::new();
    let mut sent = false;
    loop {
        drain_output(&rx, &mut output);
        if input && !sent && String::from_utf8_lossy(&output).contains("WINDOWS_AGENT_READY") {
            pair.master
                .resize(PtySize {
                    rows: 40,
                    cols: 100,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            writer.write_all(b"hello from Windows\r").unwrap();
            writer.flush().unwrap();
            sent = true;
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            // Drain the terminal tail without waiting forever for ConPTY EOF.
            while let Ok(bytes) = rx.recv_timeout(Duration::from_millis(200)) {
                output.extend(bytes);
                if Instant::now() >= deadline {
                    break;
                }
            }
            let output = String::from_utf8_lossy(&output).into_owned();
            println!("exit={}\n{output}", status.exit_code());
            return (status.exit_code(), output);
        }
        if Instant::now() >= deadline {
            panic!(
                "TIMEOUT launching blue: {}",
                String::from_utf8_lossy(&output)
            );
        }
        if heartbeat.elapsed() >= Duration::from_secs(5) {
            println!(
                "waiting for blue: elapsed={}s, input_sent={sent}",
                60 - deadline.saturating_duration_since(Instant::now()).as_secs()
            );
            heartbeat = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn run_piped(
    blue: &Path,
    args: &[&str],
    env: &BTreeMap<String, String>,
    root: &Path,
) -> (i32, String) {
    let stdout = root.join("stdout.log");
    let stderr = root.join("stderr.log");
    let mut child = std::process::Command::new(blue)
        .args(args)
        .envs(env)
        .env_remove("E2E_WINDOWS_READ_INPUT")
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(&stdout).unwrap())
        .stderr(std::fs::File::create(&stderr).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut heartbeat = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let output = format!(
                "{}\n{}",
                std::fs::read_to_string(&stdout).unwrap(),
                std::fs::read_to_string(&stderr).unwrap()
            );
            println!("piped exit={status}\n{output}");
            return (status.code().unwrap(), output);
        }
        if Instant::now() >= deadline {
            let _ = std::process::Command::new("taskkill.exe")
                .args(["/PID", &child.id().to_string(), "/T", "/F"])
                .output();
            let _ = child.kill();
            panic!(
                "TIMEOUT in piped blue: stdout={:?}, stderr={:?}",
                std::fs::read_to_string(&stdout),
                std::fs::read_to_string(&stderr)
            );
        }
        if heartbeat.elapsed() >= Duration::from_secs(5) {
            println!("waiting for piped blue");
            heartbeat = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn windows_blue_launches_npm_wrapper_end_to_end() {
    let blue = PathBuf::from(
        std::env::var_os("E2E_WINDOWS_BLUE_BIN").expect("run tests/e2e-windows/run.ps1"),
    );
    assert!(blue.is_absolute() && blue.is_file());
    let profile = PathBuf::from(std::env::var_os("USERPROFILE").unwrap());
    assert!(
        !profile.join(".config/blue").exists(),
        "use a fresh Windows account (legacy Blue state exists)"
    );
    let mut owned = OwnedDirs(Vec::new());
    owned.reserve(profile.join(".codex"));
    owned.reserve(PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join("Blue"));
    let root = tempfile::Builder::new()
        .prefix("blue windows e2e ")
        .tempdir()
        .unwrap();
    let npm = root.path().join("npm with spaces");
    let config = root.path().join("config");
    std::fs::create_dir_all(config.join("Blue")).unwrap();
    std::fs::create_dir_all(&npm).unwrap();
    std::fs::copy(
        env!("CARGO_BIN_EXE_agent-fixture"),
        npm.join("agent-fixture.exe"),
    )
    .unwrap();
    // npm creates all three siblings. Selecting the extensionless file fails
    // with Win32 error 193, while PowerShell scripts are not process images.
    std::fs::write(npm.join("codex"), "#!/bin/sh\nexit 99\n").unwrap();
    std::fs::write(npm.join("codex.ps1"), "exit 98\n").unwrap();
    std::fs::write(
        npm.join("codex.cmd"),
        "@ECHO off\r\n\"%~dp0agent-fixture.exe\" %*\r\n",
    )
    .unwrap();
    std::fs::write(
        config.join("Blue/blue.toml"),
        "[mode]\nforce_governance_only = true\nallow_noninteractive_merge = true\n",
    )
    .unwrap();
    std::fs::write(config.join("Blue/blue.yaml"), "revision: windows-e2e\nallowed_harnesses: [codex]\nharnesses:\n  codex:\n    managed_config:\n      model: windows-e2e-model\n").unwrap();
    let log = root.path().join("argv.json");
    let mut paths = vec![npm.clone()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let mut env = BTreeMap::from([
        (
            "PATH".into(),
            std::env::join_paths(paths).unwrap().into_string().unwrap(),
        ),
        ("PATHEXT".into(), ".COM;.EXE;.BAT;.CMD".into()),
        ("XDG_CONFIG_HOME".into(), config.to_str().unwrap().into()),
        (
            "XDG_CACHE_HOME".into(),
            root.path().join("cache").to_str().unwrap().into(),
        ),
        ("E2E_WINDOWS_AGENT_LOG".into(), log.to_str().unwrap().into()),
        ("E2E_WINDOWS_READ_INPUT".into(), "1".into()),
    ]);
    let tail = [
        "spaces in argument",
        "say \"hello\"",
        r"C:\code\trailing\",
        "literal!bang",
    ];
    let mut args = vec!["run", "codex", "--"];
    args.extend(tail);
    let (code, output) = run_pty(&blue, &args, &env, true);
    assert_eq!(code, 7, "{output}");
    assert!(
        output.contains("WINDOWS_AGENT_INPUT:hello from Windows"),
        "{output}"
    );
    let actual: Vec<String> = serde_json::from_slice(&std::fs::read(&log).unwrap()).unwrap();
    assert!(
        actual.ends_with(&tail.map(str::to_string)),
        "argv: {actual:?}"
    );
    assert!(
        std::fs::read_to_string(profile.join(".codex/blue.config.toml"))
            .unwrap()
            .contains("windows-e2e-model")
    );

    // Redirected stdio follows launch_inherited, rather than the supervisor.
    let (code, output) = run_piped(&blue, &args, &env, root.path());
    assert_eq!(code, 7, "{output}");
    let actual: Vec<String> = serde_json::from_slice(&std::fs::read(&log).unwrap()).unwrap();
    assert!(
        actual.ends_with(&tail.map(str::to_string)),
        "piped argv: {actual:?}"
    );

    // Repeat after reconciliation; rejected arguments must never reach agent.
    env.remove("E2E_WINDOWS_READ_INPUT");
    for argument in ["a&b", "%USERPROFILE%", "a|b", "a>b", "a^b", "(a)"] {
        std::fs::remove_file(&log).ok();
        let (code, output) = run_pty(&blue, &["run", "codex", "--", argument], &env, false);
        assert_ne!(code, 0, "{output}");
        assert!(output.contains("Windows batch wrapper"), "{output}");
        assert!(!log.exists(), "rejected argv reached the agent");
    }
    std::fs::remove_file(npm.join("codex.cmd")).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_agent-fixture"), npm.join("codex.exe")).unwrap();
    let (code, output) = run_pty(&blue, &["run", "codex", "--", "a&b"], &env, false);
    assert_eq!(code, 7, "{output}");
    let actual: Vec<String> = serde_json::from_slice(&std::fs::read(&log).unwrap()).unwrap();
    assert_eq!(actual.last().map(String::as_str), Some("a&b"));
}
