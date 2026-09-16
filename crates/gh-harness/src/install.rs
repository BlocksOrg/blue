//! Filesystem-based installation layout recognition, used only on the repair path.
//! Detection spawns no external commands and does not prove installer provenance.
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::Detected;
use gh_common::Harness;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    pub executable: PathBuf,
    pub canonical: PathBuf,
    pub method: InstallMethod,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethod {
    /// An npm-compatible global layout, regardless of which installer created it.
    NpmGlobal {
        prefix: PathBuf,
        package: String,
    },
    ClaudeNative,
    KimiStandalone {
        root: PathBuf,
    },
    OpenCodeStandalone,
    Homebrew {
        package: String,
    },
    KimiUvLegacy,
    Unknown {
        reason: String,
    },
}

pub fn npm_package(harness: Harness) -> &'static str {
    match harness {
        Harness::Codex => "@openai/codex",
        Harness::Claude => "@anthropic-ai/claude-code",
        Harness::Kimi => "@moonshot-ai/kimi-code",
        Harness::Opencode => "opencode-ai",
    }
}

/// Recognize filesystem layouts without spawning external commands.
/// A matching npm layout does not prove npm originally installed the package.
pub fn detect_installation(detected: &Detected) -> Installation {
    let home =
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    inspect(detected, home.as_deref())
}

fn unknown(reason: impl Into<String>) -> InstallMethod {
    InstallMethod::Unknown {
        reason: reason.into(),
    }
}

fn same_file(left: &Path, right: &Path) -> bool {
    std::fs::canonicalize(left)
        .ok()
        .zip(std::fs::canonicalize(right).ok())
        .is_some_and(|(left, right)| left == right)
}

// Bound metadata reads; never load a native executable into memory.
fn small_text(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > 64 * 1024 {
        return None;
    }
    let mut text = String::new();
    file.take(64 * 1024 + 1).read_to_string(&mut text).ok()?;
    (text.len() <= 64 * 1024).then_some(text)
}

fn native_binary(path: &Path) -> bool {
    let mut magic = [0; 4];
    if std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_err()
    {
        return false;
    }
    matches!(
        magic,
        [0x7f, b'E', b'L', b'F']
            | [0xcf, 0xfa, 0xed, 0xfe]
            | [0xfe, 0xed, 0xfa, 0xcf]
            | [0xca, 0xfe, 0xba, 0xbe]
    )
}

fn inspect(detected: &Detected, home: Option<&Path>) -> Installation {
    let canonical =
        match std::fs::canonicalize(&detected.path) {
            Ok(path) if path.is_file() => path,
            _ => return Installation {
                executable: detected.path.clone(),
                canonical: detected.path.clone(),
                method: unknown(
                    "cannot resolve executable (broken symlink, inaccessible path, or non-file)",
                ),
            },
        };
    let method = classify(detected, &canonical, home);
    Installation {
        executable: detected.path.clone(),
        canonical,
        method,
    }
}

fn classify(detected: &Detected, canonical: &Path, home: Option<&Path>) -> InstallMethod {
    let harness = detected.harness;
    let package = npm_package(harness);
    // Recognize npm-compatible layouts, including manually copied packages.
    // The bin entry and canonical package root must share a prefix; package
    // symlinks into pnpm's store (or npm link) are unsupported.
    if let Some(bin) = detected
        .path
        .parent()
        .filter(|bin| bin.file_name().is_some_and(|name| name == "bin"))
    {
        if let Some(prefix) = bin.parent() {
            let root = prefix.join("lib/node_modules");
            let package_dir = root.join(package);
            if canonical.starts_with(&package_dir) {
                let metadata = small_text(&package_dir.join("package.json"))
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
                if let Some(metadata) = metadata {
                    let target = metadata.get("bin").and_then(|bin| {
                        bin.as_str()
                            .or_else(|| bin.get(harness.key()).and_then(|value| value.as_str()))
                    });
                    if metadata.get("name").and_then(|name| name.as_str()) == Some(package)
                        && std::fs::canonicalize(&package_dir).ok().as_deref()
                            == Some(package_dir.as_path())
                        && target
                            .is_some_and(|target| same_file(&package_dir.join(target), canonical))
                    {
                        return InstallMethod::NpmGlobal {
                            prefix: prefix.to_owned(),
                            package: package.into(),
                        };
                    }
                }
                return unknown("npm-shaped installation lacks matching package/bin metadata or canonical package layout");
            }
        }
    }
    let brew_package = match harness {
        Harness::Claude => "claude-code",
        Harness::Codex => "codex",
        Harness::Opencode => "opencode",
        Harness::Kimi => "kimi-code",
    };
    for ancestor in canonical.ancestors() {
        if ancestor
            .file_name()
            .is_some_and(|name| name == brew_package)
            && ancestor
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "Caskroom" || name == "Cellar")
        {
            return InstallMethod::Homebrew {
                package: brew_package.into(),
            };
        }
    }
    if harness == Harness::Kimi {
        for ancestor in canonical.ancestors().take(6) {
            if small_text(&ancestor.join("uv-receipt.toml"))
                .is_some_and(|text| text.contains("name = \"kimi-cli\""))
                && ancestor.join("pyvenv.cfg").is_file()
            {
                return InstallMethod::KimiUvLegacy;
            }
        }
    }
    // Windows shims/native layouts are deliberately refused until equivalent
    // ownership evidence is available. A filename alone is never sufficient.
    if cfg!(windows) {
        return unknown("unverified Windows executable or package-manager wrapper");
    }
    if let Some(home) = home {
        if harness == Harness::Claude {
            let versions = home.join(".local/share/claude/versions");
            if canonical.parent() == Some(versions.as_path())
                && canonical
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| semver::Version::parse(name).is_ok())
                && same_file(&home.join(".local/bin/claude"), &detected.path)
                && native_binary(canonical)
            {
                return InstallMethod::ClaudeNative;
            }
        }
        // These installers leave no receipt. Support only the documented default
        // root, a regular native binary, and no symlinked parent or wrapper.
        let default = match harness {
            Harness::Kimi => Some(home.join(".kimi-code/bin/kimi")),
            Harness::Opencode => Some(home.join(".opencode/bin/opencode")),
            _ => None,
        };
        if default.as_deref() == Some(detected.path.as_path())
            && canonical == detected.path
            && native_binary(canonical)
        {
            return if harness == Harness::Kimi {
                InstallMethod::KimiStandalone {
                    root: home.join(".kimi-code"),
                }
            } else {
                InstallMethod::OpenCodeStandalone
            };
        }
    }
    unknown("installation layout is unrecognized (standalone copy, custom root, or unsupported manager/wrapper)")
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "blue-install-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(std::fs::canonicalize(root).unwrap())
        }
        fn file(&self, name: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
            let path = self.0.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, bytes).unwrap();
            path
        }
        #[cfg(unix)]
        fn link(&self, name: &str, target: &Path) -> PathBuf {
            let path = self.0.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(target, &path).unwrap();
            path
        }
        fn inspect(&self, harness: Harness, path: PathBuf) -> Installation {
            inspect(
                &Detected {
                    harness,
                    path,
                    version: None,
                    raw_version: None,
                },
                Some(&self.0),
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    #[cfg(unix)]
    fn claude_native_resolves_relative_symlink_chain() {
        let f = Fixture::new();
        let target = f.file(".local/share/claude/versions/2.1.273", b"\x7fELFbinary");
        f.link(".local/share/claude/current", Path::new("versions/2.1.273"));
        let path = f.link(".local/bin/claude", Path::new("../share/claude/current"));
        let installation = f.inspect(Harness::Claude, path.clone());
        assert_eq!(installation.method, InstallMethod::ClaudeNative);
        assert_eq!(installation.executable, path);
        assert_eq!(installation.canonical, target);
        std::fs::write(target, "#!/bin/sh\nwrapper").unwrap();
        assert!(matches!(
            f.inspect(Harness::Claude, path).method,
            InstallMethod::Unknown { .. }
        ));
    }

    #[test]
    #[cfg(unix)]
    fn npm_compatible_manual_layouts_preserve_prefix_for_every_harness() {
        let f = Fixture::new();
        // Assemble files and symlinks manually: npm is neither run nor required.
        for harness in Harness::ALL {
            for prefix in ["nvm/versions/node/v24", "custom prefix"] {
                let prefix = format!("{prefix}/{}", harness.key());
                let package_dir = format!("{prefix}/lib/node_modules/{}", npm_package(harness));
                let target = f.file(&format!("{package_dir}/cli.js"), "#!/usr/bin/env node");
                let metadata = serde_json::json!({"name": npm_package(harness), "bin": { harness.key(): "cli.js" }});
                let metadata_path =
                    f.file(&format!("{package_dir}/package.json"), metadata.to_string());
                let path = f.link(&format!("{prefix}/bin/{}", harness.key()), &target);
                let detected = Detected {
                    harness,
                    path: path.clone(),
                    version: None,
                    raw_version: None,
                };
                let installation = inspect(&detected, Some(&f.0));
                assert_eq!(
                    installation.method,
                    InstallMethod::NpmGlobal {
                        prefix: f.0.join(&prefix),
                        package: npm_package(harness).into()
                    }
                );
                for (label, invalid_metadata) in [
                    ("wrong bin target", metadata.to_string().replace("cli.js", "other.js")),
                    ("wrong package name", serde_json::json!({"name": "other-package", "bin": { harness.key(): "cli.js" }}).to_string()),
                    ("malformed metadata", "{".into()),
                ] {
                    std::fs::write(&metadata_path, invalid_metadata).unwrap();
                    assert!(matches!(
                        inspect(&detected, Some(&f.0)).method,
                        InstallMethod::Unknown { .. }
                    ), "{label}");
                    std::fs::write(&metadata_path, metadata.to_string()).unwrap();
                }
                std::fs::remove_file(&metadata_path).unwrap();
                assert!(matches!(
                    inspect(&detected, Some(&f.0)).method,
                    InstallMethod::Unknown { .. }
                ));
                std::fs::write(&metadata_path, metadata.to_string()).unwrap();
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn pnpm_store_and_linked_package_layouts_are_unsupported() {
        let f = Fixture::new();
        let target = f.file("store/.pnpm/codex/bin/codex.js", "node");
        f.file(
            "store/.pnpm/codex/package.json",
            r#"{"name":"@openai/codex","bin":{"codex":"bin/codex.js"}}"#,
        );
        f.link(
            "prefix/lib/node_modules/@openai/codex",
            &f.0.join("store/.pnpm/codex"),
        );
        let path = f.link("prefix/bin/codex", &target);
        let detected = Detected {
            harness: Harness::Codex,
            path,
            version: None,
            raw_version: None,
        };
        assert!(matches!(
            inspect(&detected, Some(&f.0)).method,
            InstallMethod::Unknown { .. }
        ));
    }

    #[test]
    fn homebrew_cask_and_cellar_custom_prefixes() {
        let f = Fixture::new();
        for prefix in ["opt/homebrew", "usr/local", "custom"] {
            for (harness, area, package) in [
                (Harness::Codex, "Caskroom", "codex"),
                (Harness::Claude, "Caskroom", "claude-code"),
                (Harness::Opencode, "Cellar", "opencode"),
            ] {
                let path = f.file(
                    &format!("{prefix}/{area}/{package}/1.2.3/bin/{}", harness.key()),
                    "binary",
                );
                assert_eq!(
                    f.inspect(harness, path).method,
                    InstallMethod::Homebrew {
                        package: package.into()
                    }
                );
            }
        }
    }

    #[test]
    fn standalone_defaults_require_native_files_and_custom_roots_refuse() {
        let f = Fixture::new();
        let kimi = f.file(".kimi-code/bin/kimi", b"\x7fELFbinary");
        let opencode = f.file(".opencode/bin/opencode", b"\xcf\xfa\xed\xfebinary");
        if !cfg!(windows) {
            assert_eq!(
                f.inspect(Harness::Kimi, kimi.clone()).method,
                InstallMethod::KimiStandalone {
                    root: f.0.join(".kimi-code")
                }
            );
            assert_eq!(
                f.inspect(Harness::Opencode, opencode).method,
                InstallMethod::OpenCodeStandalone
            );
        }
        for name in [
            "usr/local/bin/kimi",
            "custom/bin/kimi",
            ".local/bin/codex.exe",
            "npm/codex.cmd",
        ] {
            let path = f.file(name, b"\x7fELFbinary");
            assert!(matches!(
                f.inspect(Harness::Kimi, path).method,
                InstallMethod::Unknown { .. }
            ));
        }
        std::fs::write(&kimi, "#!/bin/sh\nuv tool run kimi-cli").unwrap();
        assert!(matches!(
            f.inspect(Harness::Kimi, kimi).method,
            InstallMethod::Unknown { .. }
        ));
    }

    #[test]
    #[cfg(unix)]
    fn legacy_uv_needs_receipt_and_environment_and_broken_links_refuse() {
        let f = Fixture::new();
        let target = f.file("custom-uv/kimi-cli/bin/kimi", "#!/python");
        f.file("custom-uv/kimi-cli/pyvenv.cfg", "home = python");
        let path = f.link(".local/bin/kimi", &target);
        assert!(matches!(
            f.inspect(Harness::Kimi, path.clone()).method,
            InstallMethod::Unknown { .. }
        ));
        f.file(
            "custom-uv/kimi-cli/uv-receipt.toml",
            "[tool]\nrequirements = [{ name = \"kimi-cli\" }]",
        );
        assert_eq!(
            f.inspect(Harness::Kimi, path).method,
            InstallMethod::KimiUvLegacy
        );
        let broken = f.link("broken/kimi", Path::new("missing"));
        assert!(matches!(
            f.inspect(Harness::Kimi, broken).method,
            InstallMethod::Unknown { .. }
        ));
    }
}
