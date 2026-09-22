use std::collections::BTreeMap;

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::adapters;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EffectiveVerifiedCeilings(pub BTreeMap<String, BTreeMap<String, String>>);

impl EffectiveVerifiedCeilings {
    pub fn get(&self, harness: &str, profile: &str) -> Option<&str> {
        self.0.get(harness)?.get(profile).map(String::as_str)
    }

    pub fn effective<'a>(&'a self, harness: &str, profile: &str, compiled: &'a str) -> &'a str {
        self.get(harness, profile).unwrap_or(compiled)
    }

    pub fn extends_compiled_registry(&self) -> bool {
        adapters::HARNESS_DEFINITIONS.iter().any(|definition| {
            definition.implementations.iter().any(|registration| {
                self.get(definition.metadata.key, registration.interval.profile)
                    .is_some_and(|value| {
                        Version::parse(value).expect("validated ceiling")
                            > Version::parse(registration.interval.verified_before)
                                .expect("compiled ceiling")
                    })
            })
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownVersionsManifest {
    pub schema_version: u32,
    pub harnesses: BTreeMap<String, KnownHarnessVersion>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownHarnessVersion {
    pub profile: String,
    pub verified_before: String,
}

impl KnownVersionsManifest {
    pub fn parse_and_validate(bytes: &[u8]) -> Result<EffectiveVerifiedCeilings, String> {
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|error| format!("invalid known-agent-versions manifest: {error}"))?;
        manifest.validate()
    }

    pub fn validate(self) -> Result<EffectiveVerifiedCeilings, String> {
        if self.schema_version != 1 {
            return Err(format!(
                "unsupported known-agent-versions schema_version {}",
                self.schema_version
            ));
        }
        let mut result = EffectiveVerifiedCeilings::default();
        for (key, entry) in self.harnesses {
            let definition = adapters::HARNESS_DEFINITIONS
                .iter()
                .find(|definition| definition.metadata.key == key)
                .ok_or_else(|| format!("unknown harness `{key}`"))?;
            let registration = definition
                .implementations
                .iter()
                .find(|registration| registration.interval.profile == entry.profile)
                .ok_or_else(|| {
                    format!("unknown profile `{}` for harness `{key}`", entry.profile)
                })?;
            let ceiling = Version::parse(&entry.verified_before).map_err(|_| {
                format!(
                    "verified_before for {key}/{} must be canonical SemVer",
                    entry.profile
                )
            })?;
            if ceiling.to_string() != entry.verified_before {
                return Err(format!(
                    "verified_before for {key}/{} must be canonical SemVer",
                    entry.profile
                ));
            }
            let compiled =
                Version::parse(registration.interval.verified_before).expect("compiled ceiling");
            if ceiling < compiled {
                return Err(format!(
                    "verified_before for {key}/{} regresses below compiled ceiling",
                    entry.profile
                ));
            }
            if let Some(before) = registration.interval.before {
                if ceiling > Version::parse(before).expect("compiled interval") {
                    return Err(format!(
                        "verified_before for {key}/{} exceeds compiled adapter interval",
                        entry.profile
                    ));
                }
            }
            result
                .0
                .entry(key)
                .or_default()
                .insert(entry.profile, entry.verified_before);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_in_manifest_is_valid() {
        let bytes = include_bytes!("../../../known-agent-versions.json");
        KnownVersionsManifest::parse_and_validate(bytes).unwrap();
        let manifest: KnownVersionsManifest = serde_json::from_slice(bytes).unwrap();
        let lock: serde_json::Value =
            serde_json::from_slice(include_bytes!("../../../tests/e2e/agents.lock.json")).unwrap();
        for (harness, entry) in manifest.harnesses {
            let pinned = lock["agents"][&harness]["version"].as_str().unwrap();
            let mut expected = Version::parse(pinned).unwrap();
            expected.patch += 1;
            expected.pre = semver::Prerelease::new("0").unwrap();
            assert_eq!(entry.verified_before, expected.to_string(), "{harness}");
            assert_eq!(entry.profile, lock["agents"][&harness]["profile"]);
        }
    }

    #[test]
    fn rejects_unknown_fields_and_regressions() {
        assert!(KnownVersionsManifest::parse_and_validate(
            br#"{"schema_version":1,"extra":true,"harnesses":{}}"#
        )
        .is_err());
        assert!(KnownVersionsManifest::parse_and_validate(br#"{"schema_version":1,"harnesses":{"codex":{"profile":"codex-v0_145_0","verified_before":"0.150.0"}}}"#).is_err());
    }

    #[test]
    fn accepts_an_unbounded_profile_extension() {
        let ceilings = KnownVersionsManifest::parse_and_validate(br#"{"schema_version":1,"harnesses":{"opencode":{"profile":"opencode-v0_0_0","verified_before":"1.18.27-0"}}}"#).unwrap();
        assert!(ceilings.extends_compiled_registry());
    }
}
