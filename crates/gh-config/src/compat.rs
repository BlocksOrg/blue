//! Harness-version compatibility routing.
//!
//! Profiles are ordered breaking-generation boundaries. A generation applies
//! from its inclusive `introduced` version until the next generation. Adding a
//! breaking format therefore means appending one entry and retaining the old
//! renderer until its announced deprecation.

use crate::adapters::{self, HarnessImplementation, ImplementationLifecycle, VersionInterval};
use crate::known_versions::EffectiveVerifiedCeilings;
use gh_common::{GhError, Harness, InstallInvocation};
use gh_service::HarnessPolicy;
use semver::{Version, VersionReq};

pub(crate) fn policy_requires_update_suppression(policy: &HarnessPolicy) -> Result<bool, GhError> {
    if !policy.allow_unverified_versions {
        return Ok(true);
    }
    let Some(requirement) = policy
        .version_requirement
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    else {
        return Err(GhError::config(
            "allow_unverified_versions requires an explicit version_requirement",
        ));
    };
    let requirement = VersionReq::parse(requirement).map_err(|error| {
        GhError::config(format!(
            "invalid harness version requirement `{requirement}`: {error}"
        ))
    })?;
    Ok(requirement
        .comparators
        .iter()
        .any(|comparator| !matches!(comparator.op, semver::Op::Greater | semver::Op::GreaterEq)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileStatus {
    Supported,
    Deprecated,
}

#[derive(Debug, Clone, Copy)]
pub struct CompatibilityProfile {
    pub id: &'static str,
    pub introduced: &'static str,
    pub before: Option<&'static str>,
    pub status: ProfileStatus,
    pub implementation: &'static dyn HarnessImplementation,
    pub interval: VersionInterval,
}

#[derive(Debug, Clone)]
pub struct HarnessContext {
    pub definition: &'static adapters::HarnessDefinition,
    pub harness: Harness,
    pub version: Version,
    pub raw_version: String,
    pub profile: CompatibilityProfile,
    pub unverified_warning: Option<String>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum CompatibilityFailure {
    #[error("unparseable harness version `{raw}` for {harness}; refusing version-dependent reconciliation")]
    UnparseableVersion { harness: Harness, raw: String },
    #[error("invalid {harness} version requirement `{requirement}`: {reason}")]
    InvalidPolicy {
        harness: Harness,
        requirement: String,
        reason: String,
    },
    #[error("policy requires {harness} `{requirement}`, but installed version is {installed}")]
    PolicyMismatch {
        harness: Harness,
        requirement: String,
        installed: Version,
    },
    #[error(
        "unsupported {harness} version {installed}; no maintained compatibility profile matches"
    )]
    UnsupportedGeneration {
        harness: Harness,
        installed: Version,
    },
    #[error("{harness} version {installed} is outside Blue's certified range for profile {profile} (verified before {verified_before}); set a matching version_requirement and allow_unverified_versions: true to accept possible breaking changes")]
    UnverifiedGeneration {
        harness: Harness,
        installed: Version,
        profile: &'static str,
        verified_before: String,
    },
}

impl CompatibilityFailure {
    pub fn is_installable(&self) -> bool {
        matches!(
            self,
            Self::UnparseableVersion { .. }
                | Self::PolicyMismatch { .. }
                | Self::UnsupportedGeneration { .. }
                | Self::UnverifiedGeneration { .. }
        )
    }

    pub fn harness(&self) -> Harness {
        match self {
            Self::UnparseableVersion { harness, .. }
            | Self::InvalidPolicy { harness, .. }
            | Self::PolicyMismatch { harness, .. }
            | Self::UnsupportedGeneration { harness, .. }
            | Self::UnverifiedGeneration { harness, .. } => *harness,
        }
    }

    pub fn with_install_hint(&self, policy: &HarnessPolicy) -> String {
        if self.is_installable() {
            format!("{self}; {}", install_hint(self.harness(), policy))
        } else {
            self.to_string()
        }
    }
}

impl From<CompatibilityFailure> for GhError {
    fn from(value: CompatibilityFailure) -> Self {
        GhError::config(value.to_string())
    }
}

fn profile_for(
    definition: &'static adapters::HarnessDefinition,
    version: &Version,
) -> Option<CompatibilityProfile> {
    let registration = definition.select(version)?;
    let metadata = registration.interval;
    Some(CompatibilityProfile {
        id: metadata.profile,
        introduced: metadata.introduced,
        before: metadata.before,
        status: match metadata.lifecycle {
            ImplementationLifecycle::Supported => ProfileStatus::Supported,
            ImplementationLifecycle::Deprecated => ProfileStatus::Deprecated,
        },
        implementation: registration.implementation,
        interval: registration.interval,
    })
}

#[cfg(test)]
fn select_profile(harness: Harness, version: &Version) -> Option<CompatibilityProfile> {
    profile_for(adapters::definition(harness), version)
}

pub fn supported_install(
    harness: Harness,
    harness_policy: &HarnessPolicy,
) -> Result<InstallInvocation, GhError> {
    supported_install_with_effective(
        harness,
        harness_policy,
        &EffectiveVerifiedCeilings::default(),
    )
}

pub fn supported_install_with_effective(
    harness: Harness,
    harness_policy: &HarnessPolicy,
    ceilings: &EffectiveVerifiedCeilings,
) -> Result<InstallInvocation, GhError> {
    if harness_policy.allow_unverified_versions
        && harness_policy
            .version_requirement
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
    {
        return Err(GhError::config(
            "allow_unverified_versions requires an explicit version_requirement",
        ));
    }
    let requirement = harness_policy
        .version_requirement
        .as_deref()
        .filter(|value| !value.trim().is_empty());
    let policy = requirement
        .map(VersionReq::parse)
        .transpose()
        .map_err(|error| {
            GhError::config(format!("invalid {harness} install requirement: {error}"))
        })?;
    let registration = adapters::definition(harness)
        .implementations
        .iter()
        .rev()
        .find(|registration| {
            interval_intersects_with_effective(
                &registration.interval,
                policy.as_ref(),
                harness_policy.allow_unverified_versions,
                ceilings.effective(
                    harness.key(),
                    registration.interval.profile,
                    registration.interval.verified_before,
                ),
            )
        })
        .ok_or_else(|| {
            GhError::config(format!(
                "no supported {harness} implementation intersects the install requirement"
            ))
        })?;
    let interval = registration.interval;
    let mut selector = policy
        .as_ref()
        .map(|value| format!("{value}, "))
        .unwrap_or_default();
    selector.push_str(&format!(">={}", interval.introduced));
    let upper = if harness_policy.allow_unverified_versions {
        interval.before
    } else {
        let effective =
            ceilings.effective(harness.key(), interval.profile, interval.verified_before);
        match interval.before {
            Some(before)
                if Version::parse(before).expect("compiled interval")
                    < Version::parse(effective).expect("validated ceiling") =>
            {
                Some(before)
            }
            _ => Some(effective),
        }
    };
    if let Some(upper) = upper {
        selector.push_str(&format!(", <{upper}"));
    }
    Ok(registration
        .implementation
        .install_plan(&interval, Some(&selector)))
}

/// Comparator endpoints and their immediate successors contain a witness for
/// every nonempty stable semver intersection. Prerelease endpoints are also
/// tested explicitly because VersionReq only admits named prerelease tuples.
pub(crate) fn interval_intersects(
    interval: &VersionInterval,
    requirement: Option<&VersionReq>,
    allow_unverified: bool,
) -> bool {
    interval_intersects_with_effective(
        interval,
        requirement,
        allow_unverified,
        interval.verified_before,
    )
}

fn interval_intersects_with_effective(
    interval: &VersionInterval,
    requirement: Option<&VersionReq>,
    allow_unverified: bool,
    effective_verified_before: &str,
) -> bool {
    let lower = Version::parse(interval.introduced).expect("compiled interval");
    let mut upper = interval
        .before
        .map(|value| Version::parse(value).expect("compiled interval"));
    if !allow_unverified {
        let verified =
            Version::parse(effective_verified_before).expect("validated verified ceiling");
        if upper.as_ref().is_none_or(|value| verified < *value) {
            upper = Some(verified);
        }
    }
    let Some(requirement) = requirement else {
        return true;
    };
    let mut candidates = vec![lower.clone()];
    candidates.extend(requirement_candidates(requirement));
    candidates.into_iter().any(|version| {
        version >= lower
            && upper.as_ref().is_none_or(|upper| &version < upper)
            && requirement.matches(&version)
    })
}

fn requirement_candidates(requirement: &VersionReq) -> Vec<Version> {
    let mut candidates = Vec::new();
    for comparator in &requirement.comparators {
        let mut endpoint = Version::new(
            comparator.major,
            comparator.minor.unwrap_or(0),
            comparator.patch.unwrap_or(0),
        );
        endpoint.pre = comparator.pre.clone();
        candidates.extend(boundary_candidates(&endpoint));
    }
    candidates
}

fn boundary_candidates(endpoint: &Version) -> Vec<Version> {
    let mut candidates = vec![
        endpoint.clone(),
        Version::new(endpoint.major, endpoint.minor, endpoint.patch),
    ];
    if !endpoint.pre.is_empty() {
        let mut next = endpoint.clone();
        next.pre = semver::Prerelease::new(&format!("{}.0", endpoint.pre))
            .expect("valid prerelease successor");
        candidates.push(next);
    }
    if let Some(patch) = endpoint.patch.checked_add(1) {
        candidates.push(Version::new(endpoint.major, endpoint.minor, patch));
    }
    if let Some(minor) = endpoint.minor.checked_add(1) {
        candidates.push(Version::new(endpoint.major, minor, 0));
    }
    if let Some(major) = endpoint.major.checked_add(1) {
        candidates.push(Version::new(major, 0, 0));
    }
    candidates
}

fn install_hint(harness: Harness, policy: &HarnessPolicy) -> String {
    match supported_install(harness, policy) {
        Ok(invocation) => format!("install a supported version with `{}`", invocation.display),
        Err(error) => error.to_string(),
    }
}

/// Validate every effective package mapping reachable through an organization's
/// version policy against the implementation that would consume it.
pub fn validate_package_adapter_for_policy(
    harness: Harness,
    policy: &HarnessPolicy,
    adapter: &gh_service::PackageAdapter,
) -> Result<(), GhError> {
    if policy.allow_unverified_versions
        && policy
            .version_requirement
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
    {
        return Err(GhError::config(
            "allow_unverified_versions requires an explicit version_requirement",
        ));
    }
    let requirement = policy
        .version_requirement
        .as_deref()
        .map(VersionReq::parse)
        .transpose()
        .map_err(|error| {
            GhError::config(format!("invalid {harness} version requirement: {error}"))
        })?;
    let definition = adapters::definition(harness);
    let mut candidates = Vec::new();
    let availability = adapter.availability().map_err(GhError::config)?;
    candidates.extend(boundary_candidates(&availability.introduced));
    if let Some(before) = availability.before.clone() {
        candidates.extend(boundary_candidates(&before));
    }
    for registration in definition.implementations {
        if !interval_intersects(
            &registration.interval,
            requirement.as_ref(),
            policy.allow_unverified_versions,
        ) {
            continue;
        }
        candidates
            .push(Version::parse(registration.interval.introduced).expect("compiled interval"));
        for variant in &adapter.variants {
            let interval = variant.interval().map_err(GhError::config)?;
            candidates.extend(boundary_candidates(&interval.introduced));
            if let Some(before) = interval.before {
                candidates.extend(boundary_candidates(&before));
            }
        }
        if let Some(requirement) = requirement.as_ref() {
            candidates.extend(requirement_candidates(requirement));
        }
    }
    candidates.sort();
    candidates.dedup();
    let mut checked = false;
    for version in candidates {
        let Some(registration) = definition.select(&version) else {
            continue;
        };
        let ceiling = Version::parse(registration.interval.verified_before)
            .expect("compiled verified ceiling");
        if !policy.allow_unverified_versions && version >= ceiling {
            continue;
        }
        if requirement
            .as_ref()
            .is_some_and(|requirement| !requirement.matches(&version))
        {
            continue;
        }
        let (selected, _) = crate::packages::select_adapter(adapter, &version)?;
        registration
            .implementation
            .validate_components(&selected)
            .map_err(|error| {
                GhError::config(format!(
                    "{harness} package mapping is incompatible with {} at {version}: {error}",
                    registration.interval.profile
                ))
            })?;
        checked = true;
    }
    if !checked {
        return Err(GhError::config(format!(
            "no supported {harness} release intersects the package and harness policy"
        )));
    }
    Ok(())
}

/// Validate a set of package adapters together, including conflicts that only
/// appear after selecting version-specific variants.
pub fn validate_package_adapters_for_policy(
    harness: Harness,
    policy: &HarnessPolicy,
    adapters: &[(&str, &gh_service::PackageAdapter)],
) -> Result<(), GhError> {
    for (package_id, adapter) in adapters {
        validate_package_adapter_for_policy(harness, policy, adapter)
            .map_err(|error| GhError::config(format!("package `{package_id}`: {error}")))?;
    }

    let requirement = policy
        .version_requirement
        .as_deref()
        .map(VersionReq::parse)
        .transpose()
        .map_err(|error| {
            GhError::config(format!("invalid {harness} version requirement: {error}"))
        })?;
    let definition = adapters::definition(harness);
    let mut candidates = definition
        .implementations
        .iter()
        .map(|registration| {
            Version::parse(registration.interval.introduced).expect("compiled interval")
        })
        .collect::<Vec<_>>();
    if let Some(requirement) = requirement.as_ref() {
        candidates.extend(requirement_candidates(requirement));
    }
    for (_, adapter) in adapters {
        let availability = adapter.availability().map_err(GhError::config)?;
        candidates.extend(boundary_candidates(&availability.introduced));
        if let Some(before) = availability.before {
            candidates.extend(boundary_candidates(&before));
        }
        for variant in &adapter.variants {
            let interval = variant.interval().map_err(GhError::config)?;
            candidates.extend(boundary_candidates(&interval.introduced));
            if let Some(before) = interval.before {
                candidates.extend(boundary_candidates(&before));
            }
        }
    }
    candidates.sort();
    candidates.dedup();

    for version in candidates {
        let Some(registration) = definition.select(&version) else {
            continue;
        };
        let ceiling = Version::parse(registration.interval.verified_before)
            .expect("compiled verified ceiling");
        if !policy.allow_unverified_versions && version >= ceiling {
            continue;
        }
        if requirement
            .as_ref()
            .is_some_and(|requirement| !requirement.matches(&version))
        {
            continue;
        }

        let mut helpers = std::collections::BTreeMap::<String, &str>::new();
        for (package_id, adapter) in adapters {
            let (selected, _) = crate::packages::select_adapter(adapter, &version)?;
            for helper in selected.helpers.keys() {
                if let Some(existing) = helpers.insert(helper.clone(), *package_id) {
                    return Err(GhError::config(format!(
                        "packages `{existing}` and `{package_id}` both expose helper `{helper}` to {harness} at version {version}"
                    )));
                }
            }
        }
    }
    Ok(())
}

pub fn resolve(
    harness: Harness,
    version: Option<&Version>,
    raw_version: Option<&str>,
    policy: &HarnessPolicy,
) -> Result<HarnessContext, CompatibilityFailure> {
    resolve_with_effective(
        harness,
        version,
        raw_version,
        policy,
        &EffectiveVerifiedCeilings::default(),
    )
}

pub fn resolve_with_effective(
    harness: Harness,
    version: Option<&Version>,
    raw_version: Option<&str>,
    policy: &HarnessPolicy,
    ceilings: &EffectiveVerifiedCeilings,
) -> Result<HarnessContext, CompatibilityFailure> {
    resolve_for_definition_with_effective(
        adapters::definition(harness),
        version,
        raw_version,
        policy,
        ceilings,
    )
}

#[cfg(test)]
pub fn resolve_for_definition(
    definition: &'static adapters::HarnessDefinition,
    version: Option<&Version>,
    raw_version: Option<&str>,
    policy: &HarnessPolicy,
) -> Result<HarnessContext, CompatibilityFailure> {
    resolve_for_definition_with_effective(
        definition,
        version,
        raw_version,
        policy,
        &EffectiveVerifiedCeilings::default(),
    )
}

fn resolve_for_definition_with_effective(
    definition: &'static adapters::HarnessDefinition,
    version: Option<&Version>,
    raw_version: Option<&str>,
    policy: &HarnessPolicy,
    ceilings: &EffectiveVerifiedCeilings,
) -> Result<HarnessContext, CompatibilityFailure> {
    let harness = definition.harness;
    let raw = raw_version.unwrap_or("unknown");
    if policy.allow_unverified_versions
        && policy
            .version_requirement
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
    {
        return Err(CompatibilityFailure::InvalidPolicy {
            harness,
            requirement: "<missing>".into(),
            reason: "allow_unverified_versions requires an explicit version_requirement".into(),
        });
    }
    let version = version.ok_or_else(|| CompatibilityFailure::UnparseableVersion {
        harness,
        raw: raw.to_owned(),
    })?;
    if let Some(requirement) = policy.version_requirement.as_deref() {
        let requirement = VersionReq::parse(requirement).map_err(|error| {
            CompatibilityFailure::InvalidPolicy {
                harness,
                requirement: requirement.to_owned(),
                reason: error.to_string(),
            }
        })?;
        if !requirement.matches(version) {
            return Err(CompatibilityFailure::PolicyMismatch {
                harness,
                requirement: requirement.to_string(),
                installed: version.clone(),
            });
        }
    }
    let profile = profile_for(definition, version).ok_or_else(|| {
        CompatibilityFailure::UnsupportedGeneration {
            harness,
            installed: version.clone(),
        }
    })?;
    let effective_ceiling =
        ceilings.effective(harness.key(), profile.id, profile.interval.verified_before);
    let verified_before = Version::parse(effective_ceiling).expect("validated verified ceiling");
    let unverified = version >= &verified_before;
    if unverified && !policy.allow_unverified_versions {
        return Err(CompatibilityFailure::UnverifiedGeneration {
            harness,
            installed: version.clone(),
            profile: profile.id,
            verified_before: effective_ceiling.to_owned(),
        });
    }
    let unverified_warning = unverified.then(|| format!("{harness} {version} is at or beyond Blue's exclusive certified ceiling {} for {}; continuing because allow_unverified_versions is enabled. Vendor breaking changes may produce invalid configuration", effective_ceiling, profile.id));
    Ok(HarnessContext {
        definition,
        harness,
        version: version.clone(),
        raw_version: raw.to_owned(),
        profile,
        unverified_warning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppresses_updates_unless_unverified_versions_have_no_maximum() {
        for (allow_unverified_versions, requirement, expected) in [
            (false, None, true),
            (false, Some("*"), true),
            (false, Some(">=1.2.0"), true),
            (true, Some("*"), false),
            (true, Some(">=1.2.0"), false),
            (true, Some(">1.2.0, >=2.0.0"), false),
            (true, Some("<2.0.0"), true),
            (true, Some("<=2.0.0"), true),
            (true, Some("=1.2.3"), true),
            (true, Some("1.2"), true),
            (true, Some("1.*"), true),
            (true, Some("^1.2.3"), true),
            (true, Some("~1.2.3"), true),
            (true, Some(">=1.2.0, <2.0.0"), true),
        ] {
            let policy = HarnessPolicy {
                version_requirement: requirement.map(str::to_owned),
                allow_unverified_versions,
                ..Default::default()
            };
            assert_eq!(
                policy_requires_update_suppression(&policy).unwrap(),
                expected,
                "allow_unverified_versions={allow_unverified_versions}, requirement={requirement:?}"
            );
        }
    }

    #[test]
    fn uncapped_updates_require_a_valid_explicit_range() {
        let missing = HarnessPolicy {
            allow_unverified_versions: true,
            ..Default::default()
        };
        assert!(policy_requires_update_suppression(&missing).is_err());

        let policy = HarnessPolicy {
            version_requirement: Some("not semver".into()),
            allow_unverified_versions: true,
            ..Default::default()
        };
        assert!(policy_requires_update_suppression(&policy).is_err());
    }

    #[test]
    fn enforces_governance_requirement() {
        let policy = HarnessPolicy {
            version_requirement: Some(">=0.149.0, <0.150.0".into()),
            ..Default::default()
        };
        let version = Version::new(0, 149, 1);
        assert_eq!(
            resolve(Harness::Codex, Some(&version), Some("1.5.0"), &policy)
                .unwrap()
                .profile
                .id,
            "codex-v0_145_0"
        );
        let error = resolve(
            Harness::Codex,
            Some(&Version::new(0, 150, 0)),
            Some("2.0.0"),
            &policy,
        )
        .unwrap_err();
        assert!(error.is_installable());
        assert!(error
            .with_install_hint(&policy)
            .contains("npm install -g '@openai/codex@>=0.149.0 <0.150.0 >=0.145.0 <0.151.1-0'"));
        let install = supported_install(Harness::Codex, &policy).unwrap();
        assert_eq!(install.program, "npm");
        assert_eq!(
            install.args,
            [
                "install",
                "-g",
                "@openai/codex@>=0.149.0 <0.150.0 >=0.145.0 <0.151.1-0"
            ]
        );
    }

    #[test]
    fn installer_stays_below_each_certified_ceiling() {
        for harness in Harness::ALL {
            let install = supported_install(harness, &HarnessPolicy::default()).unwrap();
            let current = adapters::definition(harness)
                .implementations
                .last()
                .unwrap();
            assert_eq!(install.program, "npm");
            assert_eq!(&install.args[..2], ["install", "-g"]);
            assert!(install.args[2].contains(&format!("<{}", current.interval.verified_before)));
        }
    }

    #[test]
    fn bounded_lower_requirement_keeps_supported_interval() {
        let policy = HarnessPolicy {
            version_requirement: Some(">0.140.0, <0.149.5".into()),
            ..Default::default()
        };
        let install = supported_install(Harness::Codex, &policy).unwrap();
        assert_eq!(
            install.args,
            [
                "install",
                "-g",
                "@openai/codex@>0.140.0 <0.149.5 >=0.145.0 <0.151.1-0"
            ]
        );
    }

    #[test]
    fn certified_ceiling_is_exclusive_and_unverified_opt_in_warns() {
        let certified = resolve(
            Harness::Codex,
            Some(&Version::new(0, 151, 0)),
            None,
            &HarnessPolicy::default(),
        )
        .unwrap();
        assert!(certified.unverified_warning.is_none());

        let ceiling = Version::parse("0.151.1-0").unwrap();
        let prerelease = Version::parse("0.151.1-beta.1").unwrap();
        assert!(matches!(
            resolve(
                Harness::Codex,
                Some(&prerelease),
                None,
                &HarnessPolicy::default(),
            ),
            Err(CompatibilityFailure::UnverifiedGeneration { .. })
        ));
        let error = resolve(
            Harness::Codex,
            Some(&ceiling),
            None,
            &HarnessPolicy::default(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            CompatibilityFailure::UnverifiedGeneration { .. }
        ));

        let policy = HarnessPolicy {
            version_requirement: Some("=0.151.1-0".into()),
            allow_unverified_versions: true,
            ..Default::default()
        };
        let context = resolve(Harness::Codex, Some(&ceiling), None, &policy).unwrap();
        assert!(context
            .unverified_warning
            .as_deref()
            .is_some_and(|warning| warning.contains("possible breaking changes")
                || warning.contains("breaking changes")));
        let install = supported_install(Harness::Codex, &policy).unwrap();
        assert!(!install.args[2].contains("<0.151.1-0"));
    }

    #[test]
    fn unverified_install_opt_in_requires_an_explicit_range() {
        let policy = HarnessPolicy {
            allow_unverified_versions: true,
            ..Default::default()
        };
        assert!(supported_install(Harness::Codex, &policy)
            .unwrap_err()
            .to_string()
            .contains("requires an explicit version_requirement"));
        assert!(matches!(
            resolve(
                Harness::Codex,
                Some(&Version::new(0, 151, 0)),
                None,
                &policy,
            ),
            Err(CompatibilityFailure::InvalidPolicy { .. })
        ));
    }

    #[test]
    fn package_mapping_is_checked_against_every_reachable_generation() {
        let adapter = gh_service::PackageAdapter {
            plugin_dir: Some("plugin".into()),
            ..Default::default()
        };
        let error = validate_package_adapter_for_policy(
            Harness::Claude,
            &HarnessPolicy::default(),
            &adapter,
        )
        .unwrap_err();
        assert!(error.to_string().contains("claude-v0_0_0"));

        let policy = HarnessPolicy {
            version_requirement: Some(">=2.0.12, <2.1.253".into()),
            ..Default::default()
        };
        validate_package_adapter_for_policy(Harness::Claude, &policy, &adapter).unwrap();
    }

    #[test]
    fn prerelease_only_policy_produces_a_package_validation_partition() {
        let policy = HarnessPolicy {
            version_requirement: Some("=1.5.0-beta.1".into()),
            ..Default::default()
        };
        let adapter = gh_service::PackageAdapter {
            helpers: [(
                "tool".into(),
                gh_service::PlatformAsset {
                    paths: [("default".into(), "bin/tool".into())]
                        .into_iter()
                        .collect(),
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        validate_package_adapter_for_policy(Harness::Claude, &policy, &adapter).unwrap();
    }

    #[test]
    fn package_availability_must_cover_the_harness_policy() {
        let adapter = gh_service::PackageAdapter {
            introduced: Some("2.0.12".into()),
            plugin_dir: Some("plugin".into()),
            ..Default::default()
        };
        let error = validate_package_adapter_for_policy(
            Harness::Claude,
            &HarnessPolicy::default(),
            &adapter,
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("requires harness version >=2.0.12"));

        let policy = HarnessPolicy {
            version_requirement: Some(">=2.0.12, <2.1.253-0".into()),
            ..Default::default()
        };
        assert!(validate_package_adapter_for_policy(Harness::Claude, &policy, &adapter).is_ok());

        let prerelease_cutoff = gh_service::PackageAdapter {
            introduced: Some("2.0.12".into()),
            before: Some("2.1.0-beta.1".into()),
            plugin_dir: Some("plugin".into()),
            ..Default::default()
        };
        let error =
            validate_package_adapter_for_policy(Harness::Claude, &policy, &prerelease_cutoff)
                .unwrap_err();
        assert!(error.to_string().contains("installed version is 2.1.0"));
    }

    #[test]
    fn effective_variant_helpers_are_checked_for_collisions() {
        fn adapter(before: &str) -> gh_service::PackageAdapter {
            gh_service::PackageAdapter {
                before: Some(before.into()),
                variants: vec![gh_service::PackageAdapterVariant {
                    introduced: Some("0.0.0".into()),
                    before: Some(before.into()),
                    helpers: [(
                        "tool".into(),
                        gh_service::PlatformAsset {
                            paths: [("default".into(), "bin/tool".into())]
                                .into_iter()
                                .collect(),
                        },
                    )]
                    .into_iter()
                    .collect(),
                    ..Default::default()
                }],
                ..Default::default()
            }
        }

        let first = adapter("2.0.12-0");
        let overlapping = adapter("2.0.12-0");
        let error = validate_package_adapters_for_policy(
            Harness::Claude,
            &HarnessPolicy {
                version_requirement: Some("<2.0.12-0".into()),
                ..Default::default()
            },
            &[("first", &first), ("second", &overlapping)],
        )
        .unwrap_err();
        assert!(error.to_string().contains("both expose helper `tool`"));
        assert!(error.to_string().contains("at version"));
    }

    #[test]
    fn stable_dashboard_requirement_rejects_prereleases() {
        let version = Version::parse("0.152.0-beta.1").unwrap();
        let policy = HarnessPolicy {
            version_requirement: Some(">=0.150.0".into()),
            ..Default::default()
        };
        let error = resolve(
            Harness::Codex,
            Some(&version),
            Some("codex-cli 0.152.0-beta.1"),
            &policy,
        )
        .unwrap_err();
        assert!(matches!(error, CompatibilityFailure::PolicyMismatch { .. }));
    }

    #[test]
    fn missing_parsed_version_fails_closed() {
        assert!(resolve(
            Harness::Claude,
            None,
            Some("Claude unknown"),
            &HarnessPolicy::default()
        )
        .is_err());
    }

    #[test]
    fn invalid_policy_is_not_installable() {
        let policy = HarnessPolicy {
            version_requirement: Some("not semver".into()),
            ..Default::default()
        };
        let error = resolve(
            Harness::Codex,
            Some(&Version::new(1, 0, 0)),
            Some("1.0.0"),
            &policy,
        )
        .unwrap_err();
        assert!(!error.is_installable());
        assert!(!error.with_install_hint(&policy).contains("npm install"));
    }

    #[test]
    fn final_generation_dispatch_is_open_ended() {
        assert_eq!(
            select_profile(Harness::Codex, &Version::new(0, 149, 99))
                .unwrap()
                .id,
            "codex-v0_145_0"
        );
        assert_eq!(
            select_profile(Harness::Codex, &Version::new(0, 151, 0))
                .unwrap()
                .id,
            "codex-v0_145_0"
        );
        for version in [Version::new(0, 152, 0), Version::new(2, 0, 0)] {
            let profile = select_profile(Harness::Codex, &version).unwrap();
            assert_eq!(profile.id, "codex-v0_145_0");
            assert_eq!(profile.before, None);
        }
    }

    #[test]
    fn current_codex_releases_satisfy_open_ended_dashboard_policy() {
        let policy = HarnessPolicy {
            version_requirement: Some(">=0.150.0".into()),
            ..Default::default()
        };

        for version in [Version::new(0, 150, 0), Version::new(0, 151, 0)] {
            let context = resolve(Harness::Codex, Some(&version), None, &policy).unwrap();
            assert_eq!(context.profile.id, "codex-v0_145_0");
        }
    }
}
