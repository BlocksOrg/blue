//! Reconcile the API's allowed harness list with binaries installed on PATH.

use std::collections::BTreeSet;
use std::path::PathBuf;

use gh_common::Harness;
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::{detect, Detected};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessInventoryEntry {
    pub name: String,
    pub api_allowed: bool,
    pub client_supported: bool,
    pub installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_version: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_version"
    )]
    pub version: Option<Version>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility_profile: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub compatibility_deprecated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility_warning: Option<String>,
    #[serde(default)]
    pub reconciled: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn deserialize_optional_version<'de, D>(deserializer: D) -> Result<Option<Version>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(value.as_deref().and_then(|raw| {
        Version::parse(raw).ok().or_else(|| {
            raw.split_whitespace()
                .map(|token| token.trim_start_matches('v'))
                .find_map(|token| Version::parse(token).ok())
        })
    }))
}

impl HarnessInventoryEntry {
    pub fn harness(&self) -> Option<Harness> {
        self.name.parse().ok()
    }

    pub fn eligible(&self) -> bool {
        self.api_allowed
            && self.client_supported
            && self.installed
            && self.compatibility_error.is_none()
    }

    pub fn status(&self) -> &'static str {
        if !self.client_supported {
            "unsupported-client"
        } else if let Some(error) = self.compatibility_error.as_deref() {
            if error.starts_with("unparseable") {
                "version-unparseable"
            } else if error.starts_with("policy") {
                "version-policy-mismatch"
            } else {
                "version-unsupported"
            }
        } else if self.compatibility_deprecated {
            "profile-deprecated"
        } else if !self.api_allowed {
            if self.installed {
                "installed-not-allowed"
            } else {
                "not-allowed"
            }
        } else if !self.installed {
            "not-installed"
        } else if self.reconciled {
            "reconciled"
        } else {
            "ready"
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct HarnessInventory {
    pub entries: Vec<HarnessInventoryEntry>,
}

impl HarnessInventory {
    pub fn discover(api_allowed: &[String]) -> Self {
        Self::discover_with_detector(api_allowed, detect)
    }

    pub fn discover_with_detector(
        api_allowed: &[String],
        detector: impl Fn(Harness) -> Option<Detected>,
    ) -> Self {
        let allowed = api_allowed.iter().cloned().collect::<BTreeSet<_>>();
        let allowed_harnesses = api_allowed
            .iter()
            .filter_map(|name| name.parse::<Harness>().ok())
            .collect::<std::collections::HashSet<_>>();
        let mut entries = Harness::ALL
            .iter()
            .map(|&harness| {
                let detected = detector(harness);
                HarnessInventoryEntry {
                    name: harness.key().to_owned(),
                    api_allowed: allowed_harnesses.contains(&harness),
                    client_supported: true,
                    installed: detected.is_some(),
                    path: detected.as_ref().map(|item| item.path.clone()),
                    raw_version: detected.as_ref().and_then(|item| item.raw_version.clone()),
                    version: detected.and_then(|item| item.version),
                    compatibility_profile: None,
                    compatibility_deprecated: false,
                    compatibility_error: None,
                    compatibility_warning: None,
                    reconciled: false,
                }
            })
            .collect::<Vec<_>>();
        entries.extend(
            allowed
                .iter()
                .filter(|name| name.parse::<Harness>().is_err())
                .map(|name| HarnessInventoryEntry {
                    name: name.clone(),
                    api_allowed: true,
                    client_supported: false,
                    installed: false,
                    path: None,
                    raw_version: None,
                    version: None,
                    compatibility_profile: None,
                    compatibility_deprecated: false,
                    compatibility_error: None,
                    compatibility_warning: None,
                    reconciled: false,
                }),
        );
        Self { entries }
    }

    pub fn eligible_names(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|entry| entry.eligible())
            .map(|entry| entry.name.clone())
            .collect()
    }

    pub fn mark_reconciled(&mut self, names: impl IntoIterator<Item = String>) {
        let names = names.into_iter().collect::<BTreeSet<_>>();
        for entry in &mut self.entries {
            entry.reconciled = names.contains(&entry.name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconciles_api_allowlist_with_local_detection() {
        let inventory = HarnessInventory::discover_with_detector(
            &["codex".into(), "claude".into(), "future-agent".into()],
            |harness| match harness {
                Harness::Codex | Harness::Kimi => Some(Detected {
                    harness,
                    path: PathBuf::from(format!("/bin/{}", harness.key())),
                    raw_version: Some("1.0.0".into()),
                    version: Some(Version::new(1, 0, 0)),
                }),
                _ => None,
            },
        );

        assert_eq!(inventory.eligible_names(), vec!["codex"]);
        let codex = inventory
            .entries
            .iter()
            .find(|entry| entry.name == "codex")
            .unwrap();
        assert_eq!(codex.status(), "ready");
        let claude = inventory
            .entries
            .iter()
            .find(|entry| entry.name == "claude")
            .unwrap();
        assert_eq!(claude.status(), "not-installed");
        let kimi = inventory
            .entries
            .iter()
            .find(|entry| entry.name == "kimi")
            .unwrap();
        assert_eq!(kimi.status(), "installed-not-allowed");
        let future = inventory
            .entries
            .iter()
            .find(|entry| entry.name == "future-agent")
            .unwrap();
        assert_eq!(future.status(), "unsupported-client");
    }

    #[test]
    fn reads_legacy_raw_version_from_applied_state() {
        let entry: HarnessInventoryEntry = serde_json::from_value(serde_json::json!({
            "name": "codex",
            "api_allowed": true,
            "client_supported": true,
            "installed": true,
            "version": "codex-cli 1.2.3",
            "reconciled": true
        }))
        .unwrap();
        assert_eq!(entry.version, Some(Version::new(1, 2, 3)));
    }
}
