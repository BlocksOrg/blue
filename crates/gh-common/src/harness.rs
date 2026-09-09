//! The set of coding-agent CLIs the meta-harness governs.
//!
//! Mirrors control-sdk's `LLM` enum (reference only) but is intentionally
//! declared by the central harness catalog below.

use std::fmt;
use std::str::FromStr;

use crate::error::GhError;
use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ComponentRules {
    pub agents_require_plugin: bool,
    pub hooks_require_plugin: bool,
    pub hooks_as_plugin_modules: bool,
}

#[derive(Debug, Serialize)]
pub struct HarnessMetadata {
    #[serde(skip)]
    pub harness: Harness,
    pub key: &'static str,
    pub aliases: &'static [&'static str],
    pub label: &'static str,
    pub description: &'static str,
    pub binary_names: &'static [&'static str],
    pub install_command_template: &'static str,
    #[serde(skip)]
    pub install_program: &'static str,
    #[serde(skip)]
    pub install_args: &'static [&'static str],
}

/// Single catalog consumed by metadata and implementation registry generators.
#[macro_export]
macro_rules! harness_catalog {
    ($consumer:ident) => {
        $consumer! {
            Codex => codex {
                key: "codex",
                aliases: &[],
                label: "Codex",
                description: "OpenAI Codex CLI and app settings",
                binary_names: &["codex"],
                install_command_template: "npm install -g '@openai/codex@{version}'",
                install_program: "npm",
                install_args: &["install", "-g", "@openai/codex@{version}"],
            }
            Claude => claude {
                key: "claude",
                aliases: &["claude-code"],
                label: "Claude",
                description: "Anthropic Claude Code settings",
                binary_names: &["claude"],
                install_command_template: "npm install -g '@anthropic-ai/claude-code@{version}'",
                install_program: "npm",
                install_args: &["install", "-g", "@anthropic-ai/claude-code@{version}"],
            }
            Kimi => kimi {
                key: "kimi",
                aliases: &[],
                label: "Kimi",
                description: "Kimi Code CLI settings",
                binary_names: &["kimi"],
                install_command_template: "npm install -g '@moonshot-ai/kimi-code@{version}'",
                install_program: "npm",
                install_args: &["install", "-g", "@moonshot-ai/kimi-code@{version}"],
            }
            Opencode => opencode {
                key: "opencode",
                aliases: &["open-code"],
                label: "OpenCode",
                description: "OpenCode agent settings",
                binary_names: &["opencode"],
                install_command_template: "npm install -g 'opencode-ai@{version}'",
                install_program: "npm",
                install_args: &["install", "-g", "opencode-ai@{version}"],
            }
        }
    };
}

macro_rules! define_harness_catalog {
    ($($variant:ident => $module:ident { $($fields:tt)* })*) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Harness { $($variant),* }
        impl Harness {
            pub const ALL: [Harness; [$(stringify!($variant)),*].len()] = [$(Harness::$variant),*];
        }
        static HARNESS_METADATA: &[HarnessMetadata] = &[
            $(HarnessMetadata { harness: Harness::$variant, $($fields)* }),*
        ];
    };
}
harness_catalog!(define_harness_catalog);

pub fn harness_registry() -> &'static [HarnessMetadata] {
    HARNESS_METADATA
}

impl Harness {
    /// The canonical policy/config key (`"codex"`, `"claude"`, ...).
    pub fn key(self) -> &'static str {
        self.metadata().key
    }

    /// Candidate binary names to look for on `PATH`, in priority order.
    ///
    /// Detect-not-bundle: the harness never ships these binaries, it only
    /// resolves the one the developer already installed.
    pub fn binary_names(self) -> &'static [&'static str] {
        self.metadata().binary_names
    }

    pub fn metadata(self) -> &'static HarnessMetadata {
        HARNESS_METADATA
            .iter()
            .find(|metadata| metadata.harness == self)
            .expect("every Harness variant must have exactly one registry entry")
    }

    /// Render the vendor's version-aware install command. The selector is
    /// expected to be an npm-compatible exact version, tag, or semver range.
    pub fn install_command(self, version_selector: &str) -> String {
        self.install_invocation(version_selector).display
    }

    /// Return a structured invocation so callers can execute the installer
    /// directly without passing governance data through a shell.
    pub fn install_invocation(self, version_selector: &str) -> InstallInvocation {
        let args = self
            .metadata()
            .install_args
            .iter()
            .map(|arg| arg.replace("{version}", version_selector))
            .collect::<Vec<_>>();
        InstallInvocation {
            program: self.metadata().install_program,
            display: display_command(self.metadata().install_program, &args),
            args,
        }
    }
}

fn display_command(program: &str, args: &[String]) -> String {
    std::iter::once(program.to_owned())
        .chain(args.iter().map(|arg| {
            if !arg.is_empty()
                && arg
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_=+.,/:".contains(&byte))
            {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        }))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallInvocation {
    pub program: &'static str,
    pub args: Vec<String>,
    pub display: String,
}

impl fmt::Display for Harness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

impl FromStr for Harness {
    type Err = GhError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.trim().to_ascii_lowercase();
        HARNESS_METADATA
            .iter()
            .find(|metadata| {
                metadata.key == normalized || metadata.aliases.contains(&normalized.as_str())
            })
            .map(|metadata| metadata.harness)
            .ok_or_else(|| GhError::UnknownHarness {
                requested: normalized,
                known: HARNESS_METADATA
                    .iter()
                    .map(|metadata| metadata.key)
                    .collect::<Vec<_>>()
                    .join(", "),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_keys() {
        for h in Harness::ALL {
            assert_eq!(Harness::from_str(h.key()).unwrap(), h);
        }
    }

    #[test]
    fn accepts_aliases() {
        assert_eq!(Harness::from_str("claude-code").unwrap(), Harness::Claude);
        assert_eq!(Harness::from_str(" OpenCode ").unwrap(), Harness::Opencode);
    }

    #[test]
    fn rejects_unknown() {
        assert!(matches!(
            Harness::from_str("gemini"),
            Err(GhError::UnknownHarness { .. })
        ));
    }

    #[test]
    fn registry_is_complete_and_unique() {
        use std::collections::BTreeSet;
        assert_eq!(HARNESS_METADATA.len(), Harness::ALL.len());
        let variants = HARNESS_METADATA
            .iter()
            .map(|item| item.harness)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(variants.len(), Harness::ALL.len());
        let mut names = BTreeSet::new();
        for item in HARNESS_METADATA {
            assert!(names.insert(item.key));
            for alias in item.aliases {
                assert!(names.insert(alias));
            }
            assert_eq!(
                item.install_command_template.matches("{version}").count(),
                1
            );
            assert_eq!(
                item.install_args
                    .iter()
                    .filter(|arg| arg.contains("{version}"))
                    .count(),
                1
            );
            let rendered = item.harness.install_invocation("1.2.3");
            assert_eq!(
                rendered.display,
                item.install_command_template.replace("{version}", "1.2.3")
            );
        }
    }
}
