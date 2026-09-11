//! Subcommand implementations. Thin glue over the library crates — the CLI owns
//! no governance logic itself.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as FmtWrite;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};

use gh_common::{BlueToml, Harness, IdentityConfig, InstallInvocation};
use gh_config::{
    package_statuses, resolve_compatibility, supported_install, write_harness_with_package_fetcher,
    AuthenticatedPackageFetcher, HarnessContext, WriteOptions,
};
use gh_harness::{Detected, HarnessInventory, HarnessInventoryEntry};
use gh_service::{
    now_unix, GovernanceConfig, HarnessPolicy, RevisionStreamEnd, ServiceClient, Session,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};

/// Build the service client and load the (possibly default) client config.
fn load_client() -> Result<(BlueToml, ServiceClient)> {
    let cfg = BlueToml::load().context("loading blue.toml")?;
    let client = ServiceClient::from_config(&cfg).context("building service client")?;
    Ok((cfg, client))
}

/// Resolve a session: the persisted login, or an anonymous session when using a
/// local `file` config source (dev has no auth). HTTP sources require login.
fn session_for(cfg: &BlueToml) -> Result<Session> {
    if let Some(mut s) = Session::load()? {
        s.adopt_refresh_context(&cfg.service.url, identity_scopes(cfg));
        match s.refresh_if_needed(now_unix()) {
            Ok(()) => return Ok(s),
            Err(error) if cfg.has_http_service() => return Err(error.into()),
            Err(_) => {} // local-file development does not require OAuth
        }
    }
    if cfg.has_http_service() {
        bail!("not logged in — run `blue login` first");
    }
    Ok(Session::bearer(""))
}

/// The scopes `blue.toml` configures for the device flow. A `token`-mode
/// deployment has none; `adopt_refresh_context` falls back to the defaults
/// `device_login` would have requested.
fn identity_scopes(cfg: &BlueToml) -> &[String] {
    match &cfg.identity {
        IdentityConfig::Oidc { scopes, .. } => scopes,
        _ => &[],
    }
}

/// Whether this deployment has a device flow at all. In `token` mode a
/// "re-login" resolves the same static token and changes nothing, so offering
/// one turns a dead session into a quieter dead session.
fn is_oidc(cfg: &BlueToml) -> bool {
    matches!(cfg.identity, IdentityConfig::Oidc { .. })
}

fn write_options(cfg: &BlueToml) -> WriteOptions {
    WriteOptions {
        gateway_enabled: !cfg.mode.force_governance_only,
        enforced: cfg.mode.enforced,
        allow_existing_merge: cfg.mode.allow_noninteractive_merge,
        session_upload_enabled: false,
    }
}

fn interactive_terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

pub(crate) fn version_text() -> String {
    format!("Blue metaharness {}", env!("CARGO_PKG_VERSION"))
}

pub fn version() -> Result<()> {
    println!("{}", version_text());
    Ok(())
}

fn prompt_control_api_url() -> Result<String> {
    if !interactive_terminal() {
        bail!(
            "Blue setup requires an interactive terminal; run `blue setup` in a terminal or provision blue.toml"
        );
    }
    cliclack::intro(console::style(" Welcome to Blue ").cyan().bold())?;
    println!("Connect once; policy, login, and your preferred coding agent are handled here.");
    let value: String = cliclack::input("Control API URL")
        .placeholder("https://harness.example.com")
        .validate(|value: &String| {
            gh_service::validate_deployment_url(value, "Control API URL")
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
        .interact()?;
    Ok(value)
}

fn discover_configuration(url: &str) -> Result<BlueToml> {
    let spinner = cliclack::spinner();
    spinner.start("Discovering deployment…");
    let document = match gh_service::discover(url) {
        Ok(document) => document,
        Err(error) => {
            spinner.error("Deployment discovery failed");
            return Err(error.into());
        }
    };
    let mut cfg = BlueToml::default();
    cfg.service.url = document.control_api_url;
    cfg.identity = IdentityConfig::Oidc {
        issuer: document.oauth.issuer,
        client_id: document.oauth.client_id,
        scopes: document.oauth.scopes,
    };
    spinner.stop("Deployment connected");
    Ok(cfg)
}

#[derive(Serialize, Deserialize)]
struct TenantArchiveManifest {
    schema_version: u32,
    canonical_url: String,
    saved_at: i64,
}

const TENANT_ARCHIVE_SCHEMA_VERSION: u32 = 1;
const TENANT_CONFIG_ENTRIES: &[&str] = &[
    "blue.toml",
    "mcp.json",
    "applied-state.json",
    "merge-approvals.json",
    "packages",
    "package-state.json",
    "package-state",
    "session-upload-spool",
];

fn tenant_id(url: &str) -> String {
    hex::encode(Sha256::digest(url.trim_end_matches('/').as_bytes()))
}

fn tenant_archive_dir(url: &str) -> Result<PathBuf> {
    Ok(gh_common::paths::blue_config_dir()?
        .join("tenants")
        .join(tenant_id(url)))
}

fn tenant_cache_archive_dir(url: &str) -> Result<PathBuf> {
    Ok(gh_common::paths::cache_home()?
        .join("blue")
        .join("tenants")
        .join(tenant_id(url)))
}

fn copy_owned_tree(source: &Path, destination: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source)
        .with_context(|| format!("reading {}", source.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("refusing to archive symlink {}", source.display());
    }
    if metadata.is_dir() {
        std::fs::create_dir_all(destination)
            .with_context(|| format!("creating {}", destination.display()))?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_owned_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(source, destination).with_context(|| {
            format!("copying {} to {}", source.display(), destination.display())
        })?;
    }
    Ok(())
}

fn remove_owned_path(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::remove_dir_all(path)?
        }
        Ok(_) => std::fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn archive_active_tenant(cfg: &BlueToml) -> Result<()> {
    let url = cfg.service.url.trim_end_matches('/');
    if url.is_empty() {
        return Ok(());
    }
    let archive = tenant_archive_dir(url)?;
    let staging = archive.with_extension(format!("staging-{}", uuid::Uuid::new_v4()));
    let state = staging.join("state");
    let config_root = gh_common::paths::blue_config_dir()?;
    for entry in TENANT_CONFIG_ENTRIES {
        let source = config_root.join(entry);
        if source.exists() {
            copy_owned_tree(&source, &state.join(entry))?;
        }
    }
    gh_common::write_atomic(
        &staging.join("manifest.json"),
        serde_json::to_vec_pretty(&TenantArchiveManifest {
            schema_version: TENANT_ARCHIVE_SCHEMA_VERSION,
            canonical_url: url.to_owned(),
            saved_at: now_unix(),
        })?,
    )?;
    if archive.exists() {
        remove_owned_path(&archive)?;
    }
    std::fs::rename(&staging, &archive).context("committing tenant archive")?;

    let cache = gh_common::paths::governance_cache_path()?;
    let cache_archive = tenant_cache_archive_dir(url)?;
    if cache_archive.exists() {
        remove_owned_path(&cache_archive)?;
    }
    if cache.exists() {
        if let Some(parent) = cache_archive.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir_all(&cache_archive)?;
        copy_owned_tree(&cache, &cache_archive.join("governance-config.json"))?;
    }
    Ok(())
}

fn clear_active_tenant_state() -> Result<()> {
    let config_root = gh_common::paths::blue_config_dir()?;
    for entry in TENANT_CONFIG_ENTRIES {
        remove_owned_path(&config_root.join(entry))?;
    }
    remove_owned_path(&gh_common::paths::governance_cache_path()?)?;
    Ok(())
}

fn restore_tenant_state(discovered: &mut BlueToml) -> Result<bool> {
    let url = discovered.service.url.trim_end_matches('/');
    let archive = tenant_archive_dir(url)?;
    let manifest_path = archive.join("manifest.json");
    if !manifest_path.exists() {
        return Ok(false);
    }
    let manifest: TenantArchiveManifest = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
    if manifest.schema_version > TENANT_ARCHIVE_SCHEMA_VERSION || manifest.canonical_url != url {
        bail!("tenant archive metadata does not match {url}");
    }
    let saved = BlueToml::load_from(&archive.join("state/blue.toml"))?;
    discovered.mode = saved.mode;
    discovered.ui = saved.ui;
    let config_root = gh_common::paths::blue_config_dir()?;
    for entry in TENANT_CONFIG_ENTRIES {
        let source = archive.join("state").join(entry);
        if source.exists() {
            let destination = config_root.join(entry);
            remove_owned_path(&destination)?;
            copy_owned_tree(&source, &destination)?;
        }
    }
    let cached = tenant_cache_archive_dir(url)?.join("governance-config.json");
    if cached.exists() {
        copy_owned_tree(&cached, &gh_common::paths::governance_cache_path()?)?;
    }
    Ok(true)
}

fn activate_discovered(mut cfg: BlueToml) -> Result<BlueToml> {
    let restored = restore_tenant_state(&mut cfg)?;
    cfg.save().context("saving discovered blue.toml")?;
    if restored {
        remove_owned_path(&tenant_archive_dir(&cfg.service.url)?)?;
        remove_owned_path(&tenant_cache_archive_dir(&cfg.service.url)?)?;
        cliclack::log::success("Restored saved state for this deployment")?;
    }
    Ok(cfg)
}

/// Explicitly replace connection metadata. Credentials from the old
/// deployment are removed so they can never be sent to the new one.
pub fn setup() -> Result<()> {
    let url = prompt_control_api_url()?;
    let mut discovered = discover_configuration(&url)?;
    let active_path = gh_common::paths::blue_toml_path()?;
    if active_path.exists() {
        let current = BlueToml::load().context("loading current blue.toml")?;
        if current.service.url.trim_end_matches('/') != discovered.service.url.trim_end_matches('/')
        {
            detach_tenant(&current)?;
            discovered = activate_discovered(discovered)?;
        } else {
            discovered.mode = current.mode;
            discovered.ui = current.ui;
            discovered.save().context("refreshing blue.toml")?;
            Session::remove()?;
        }
    } else {
        discovered = activate_discovered(discovered)?;
        Session::remove()?;
    }
    let _cfg = discovered;
    login(false)?;
    cliclack::outro("Setup complete. Run `blue` anytime to start your agent.")?;
    Ok(())
}

fn detach_tenant(cfg: &BlueToml) -> Result<()> {
    let mut session = Session::load()?;
    archive_active_tenant(cfg).context("archiving tenant state")?;
    gh_config::remove_all_managed_configuration()
        .context("removing Blue-managed agent configuration")?;
    clear_active_tenant_state().context("clearing active tenant state")?;
    Session::remove()?;
    if let Some(session) = session.as_mut() {
        session.adopt_refresh_context(&cfg.service.url, identity_scopes(cfg));
        let _ = session.refresh_if_needed(now_unix());
        if let Err(error) = session.revoke_gateway_session(&cfg.service.url) {
            tracing::warn!(%error, "gateway session revocation failed during detach");
        }
        if let Err(error) = session.revoke() {
            tracing::warn!(%error, "remote token revocation failed; local tokens were removed");
        }
    }
    Ok(())
}

pub fn reset(yes: bool) -> Result<()> {
    let path = gh_common::paths::blue_toml_path()?;
    if !path.exists() {
        println!("Blue is already reset; no deployment is configured.");
        return Ok(());
    }
    let cfg = BlueToml::load().context("loading blue.toml")?;
    if !yes {
        if !interactive_terminal() {
            bail!("blue reset requires an interactive terminal; pass --yes to confirm non-interactively");
        }
        let label = if cfg.service.url.trim().is_empty() {
            "the current local configuration".to_owned()
        } else {
            cfg.service.url.clone()
        };
        let confirmed = cliclack::confirm(format!("Reset Blue and disconnect from {label}?"))
            .initial_value(false)
            .interact()?;
        if !confirmed {
            cliclack::outro_cancel("Reset cancelled; no files were changed.")?;
            return Ok(());
        }
    }
    detach_tenant(&cfg)?;
    println!("Blue reset complete. Run `blue` to connect to a deployment.");
    Ok(())
}

fn ensure_session_for_start(cfg: &BlueToml) -> Result<Session> {
    match session_for(cfg) {
        Ok(session) => Ok(session),
        Err(error) if cfg.has_http_service() && interactive_terminal() => {
            tracing::info!(%error, "login is missing or expired; starting device authorization");
            Session::remove()?;
            let session = gh_service::login(cfg).context("login")?;
            println!("{}", describe_session(&session, "Logged in"));
            Ok(session)
        }
        Err(error) => Err(error),
    }
}

fn choose_preferred_harness(cfg: &mut BlueToml, eligible: &[String]) -> Result<String> {
    if eligible.is_empty() {
        bail!("no eligible coding agent is installed; run `blue doctor`, then install one allowed by policy");
    }
    let selected = if let Some(preferred) =
        resolved_preference(eligible, cfg.ui.preferred_harness.as_deref())
    {
        preferred
    } else {
        if !interactive_terminal() {
            bail!(
                "multiple eligible agents found and no preference is configured; run bare `blue` in a terminal once"
            );
        }
        let mut prompt = cliclack::select("Choose your coding agent");
        for name in eligible {
            prompt = prompt.item(name.clone(), name, "installed and allowed");
        }
        prompt.interact()?
    };
    if cfg.ui.preferred_harness.as_deref() != Some(&selected) {
        cfg.ui.preferred_harness = Some(selected.clone());
        cfg.save().context("saving preferred coding agent")?;
    }
    Ok(selected)
}

struct PreparedLaunch {
    cfg: BlueToml,
    client: ServiceClient,
    session: Session,
    config: GovernanceConfig,
    inventory: HarnessInventory,
}

/// Whether a session the *service* rejected can be repaired here and now.
/// `ensure_session_for_start` only recovers from a failed local refresh; a
/// rejection arrives with the refresh working perfectly.
fn can_reauthenticate(cfg: &BlueToml) -> bool {
    cfg.has_http_service() && is_oidc(cfg) && interactive_terminal()
}

fn gateway_rejected_session(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<GatewayHttpError>()
        .is_some_and(|error| {
            matches!(
                error.status,
                reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
            )
        })
}

fn refresh_grant_was_superseded(previous: &Session, replacement: &Session) -> bool {
    previous.refresh_token.is_some() && previous.refresh_token != replacement.refresh_token
}

fn retire_superseded_session_with(
    previous: &Session,
    replacement: &Session,
    revoke: impl FnOnce(&Session) -> std::result::Result<(), gh_common::GhError>,
) {
    if refresh_grant_was_superseded(previous, replacement) {
        if let Err(error) = revoke(previous) {
            tracing::warn!(%error, "revoking the superseded refresh token failed");
        }
    }
}

fn reauthenticate_replacing(cfg: &BlueToml, previous: &Session) -> Result<Session> {
    // Persist the replacement before retiring the old grant. If the user
    // cancels device authorization, their existing local session remains
    // untouched; revocation failure must not discard a valid replacement.
    let replacement = gh_service::login(cfg).context("login")?;
    retire_superseded_session_with(previous, &replacement, Session::revoke);
    Ok(replacement)
}

fn prepare_launch(
    cfg: BlueToml,
    client: ServiceClient,
    mut session: Session,
) -> Result<PreparedLaunch> {
    let applied = load_applied_state()?;
    let mut reauthenticated = false;
    // Gateway ensure can invalidate or replace the credential used while
    // personalizing governance config. Keep these operations ordered so the
    // config request never observes an intermediate lifecycle state.
    if let Err(error) = ensure_gateway_access(&cfg, &session) {
        if !can_reauthenticate(&cfg) || !gateway_rejected_session(&error) {
            return Err(error);
        }
        println!("{error}");
        session = reauthenticate_replacing(&cfg, &session)?;
        reauthenticated = true;
        println!("{}", describe_session(&session, "Logged in"));
        // A replacement grant that is also rejected is not repaired by
        // opening another browser flow. Retry exactly once and surface it.
        ensure_gateway_access(&cfg, &session)?;
    }
    let config = match client.fetch_or_cached(&session, now_unix()) {
        Ok(config) => config,
        Err(error) => {
            // The recovery `ensure_session_for_start` performs, moved to where
            // the error actually surfaces. Without it bare `blue` loops: the
            // OAuth refresh succeeds, so `session_for` returns `Ok` and the
            // rejection lands here, past the recovery point.
            //
            // `fetch_or_cached` reports `Unauthorized` for two different
            // things, though: a session the service rejected, and a cached
            // gateway config that cannot carry an inference token. The second
            // is an outage. Confirm against the live source — which answers
            // `Service` when it is unreachable — before opening a browser.
            let rejected = !reauthenticated
                && matches!(error, gh_common::GhError::Unauthorized(_))
                && can_reauthenticate(&cfg)
                && matches!(
                    client.fetch(&session, now_unix()),
                    Err(gh_common::GhError::Unauthorized(_))
                );
            if !rejected {
                return Err(anyhow!(error).context("fetching governance config"));
            }
            println!("{error}");
            session = reauthenticate_replacing(&cfg, &session)?;
            println!("{}", describe_session(&session, "Logged in"));
            ensure_gateway_access(&cfg, &session)?;
            client
                .fetch_or_cached(&session, now_unix())
                .context("fetching governance config")?
        }
    };
    let mut inventory = discover_inventory_cached(&config.allowed_harnesses, applied.as_ref());
    gh_agent::evaluate_inventory(&config, &mut inventory);
    Ok(PreparedLaunch {
        cfg,
        client,
        session,
        config,
        inventory,
    })
}

fn resolved_preference(eligible: &[String], preferred: Option<&str>) -> Option<String> {
    if eligible.len() == 1 {
        return Some(eligible[0].clone());
    }
    preferred
        .filter(|preferred| eligible.iter().any(|name| name == preferred))
        .map(str::to_owned)
}

fn eligible_harnesses(config: &GovernanceConfig) -> Result<Vec<String>> {
    let applied = load_applied_state()?;
    let mut inventory = discover_inventory_cached(&config.allowed_harnesses, applied.as_ref());
    gh_agent::evaluate_inventory(config, &mut inventory);
    let eligible = inventory.eligible_names();
    if eligible.is_empty() {
        bail!(
            "no eligible coding agent is installed; run `blue doctor`, then install one allowed by policy"
        );
    }
    Ok(eligible)
}

fn validate_agent_selection(eligible: &[String], name: &str) -> Result<String> {
    let harness: Harness = name
        .parse()
        .with_context(|| format!("`{name}` is not a known harness"))?;
    let name = harness.key();
    if !eligible.iter().any(|eligible| eligible == name) {
        bail!(
            "agent `{name}` is not eligible; available agents: {}",
            eligible.join(", ")
        );
    }
    Ok(name.to_owned())
}

fn configured_default_harness(cfg: &BlueToml, inventory: &HarnessInventory) -> Result<Harness> {
    let preferred =
        cfg.ui.preferred_harness.as_deref().ok_or_else(|| {
            anyhow!("no default agent is configured; run `blue agent <name>` first")
        })?;
    let eligible = inventory.eligible_names();
    let selected = validate_agent_selection(&eligible, preferred).map_err(|_| {
        anyhow!(
            "default agent `{preferred}` is not installed, allowed, and compatible; run `blue agent <name>` to choose an eligible agent"
        )
    })?;
    selected.parse().context("parsing configured default agent")
}

fn inventory_for_harness(inventory: &HarnessInventory, harness: Harness) -> HarnessInventory {
    HarnessInventory {
        entries: inventory
            .entries
            .iter()
            .filter(|entry| entry.name == harness.key())
            .cloned()
            .collect(),
    }
}

fn agent_context() -> Result<(BlueToml, Vec<String>)> {
    let (cfg, client) = load_client()?;
    let session = session_for(&cfg)?;
    let config = client
        .fetch_or_cached(&session, now_unix())
        .context("fetching governance config")?;
    let eligible = eligible_harnesses(&config)?;
    Ok((cfg, eligible))
}

pub(crate) struct AgentOptions {
    pub eligible: Vec<String>,
    pub current: Option<String>,
}

pub(crate) fn agent_options() -> Result<AgentOptions> {
    let (cfg, eligible) = agent_context()?;
    Ok(AgentOptions {
        eligible,
        current: cfg.ui.preferred_harness,
    })
}

pub(crate) fn set_preferred_agent(name: &str) -> Result<String> {
    let (mut cfg, eligible) = agent_context()?;
    let selected = validate_agent_selection(&eligible, name)?;
    cfg.ui.preferred_harness = Some(selected.clone());
    cfg.save().context("saving preferred coding agent")?;
    Ok(selected)
}

pub fn agent(name: Option<&str>) -> Result<()> {
    let selected = if let Some(name) = name {
        set_preferred_agent(name)?
    } else {
        if !interactive_terminal() {
            bail!("agent selection requires a name in a non-interactive terminal; run `blue agent <name>`");
        }
        let (mut cfg, eligible) = agent_context()?;
        let current = cfg.ui.preferred_harness.as_deref();
        let mut prompt = cliclack::select("Choose your default coding agent");
        for name in &eligible {
            let hint = if current == Some(name.as_str()) {
                "current default"
            } else {
                "installed and allowed"
            };
            prompt = prompt.item(name.clone(), name, hint);
        }
        let selected = prompt.interact()?;
        cfg.ui.preferred_harness = Some(selected.clone());
        cfg.save().context("saving preferred coding agent")?;
        selected
    };
    println!("Default agent set to {selected}.");
    Ok(())
}

/// Primary guided experience for bare `blue`.
pub fn start() -> Result<()> {
    if !interactive_terminal() {
        bail!(
            "bare `blue` is interactive; use `blue run <agent> -- <args>` for non-interactive automation"
        );
    }
    let path = gh_common::paths::blue_toml_path()?;
    let mut cfg = if path.exists() {
        BlueToml::load().context("loading blue.toml")?
    } else {
        activate_discovered(discover_configuration(&prompt_control_api_url()?)?)?
    };
    let session = ensure_session_for_start(&cfg)?;
    let client = ServiceClient::from_config(&cfg).context("building service client")?;
    let prepared = prepare_launch(cfg.clone(), client, session)?;
    let preferred = choose_preferred_harness(&mut cfg, &prepared.inventory.eligible_names())?;
    run_prepared(&preferred, &[], prepared)
}

/// What to do with a stored session, given how the service answered a live
/// probe made with it.
///
/// Kept separate from [`login`] and pure: the guarantee that an *offline* user
/// is never dragged through a browser flow lives entirely in these arms, and
/// it is only testable if the triage is a value rather than control flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginDecision {
    /// The service accepted it.
    Valid,
    /// The session is fine; something else has to happen first.
    ActionRequired,
    /// Rejected, and this deployment has a device flow that can repair it.
    Reauthenticate,
    /// Rejected, but re-running the configured identity provider would resolve
    /// the same credential and change nothing.
    Rejected,
    /// We could not reach the service, so we know nothing about the session.
    Unverified,
}

fn login_decision(
    is_oidc: bool,
    probe: &std::result::Result<GovernanceConfig, gh_common::GhError>,
) -> LoginDecision {
    match probe {
        Ok(_) => LoginDecision::Valid,
        Err(gh_common::GhError::ActionRequired(_)) => LoginDecision::ActionRequired,
        Err(gh_common::GhError::Unauthorized(_)) if is_oidc => LoginDecision::Reauthenticate,
        Err(gh_common::GhError::Unauthorized(_)) => LoginDecision::Rejected,
        // Transport, 5xx, decode. `identity.rs` and `source.rs` both keep
        // `Service` distinct from `Unauthorized` precisely so this arm exists.
        Err(_) => LoginDecision::Unverified,
    }
}

fn describe_session(session: &Session, prefix: &str) -> String {
    match (&session.email, &session.org_id) {
        (Some(email), Some(org)) => format!("{prefix} as {email} (org {org})."),
        (Some(email), None) => format!("{prefix} as {email}."),
        _ => format!("{prefix}."),
    }
}

pub fn login(force: bool) -> Result<()> {
    let cfg = BlueToml::load().context("loading blue.toml")?;
    // A truncated or hand-edited session.json used to make `blue login` itself
    // fail, with no command left that could clear it.
    let stored = Session::load().unwrap_or_else(|error| {
        tracing::warn!(%error, "stored session could not be read; treating it as absent");
        None
    });

    let mut replacement_previous = None;
    if force {
        // Otherwise every forced re-login orphans a refresh token that stays
        // live server-side for its full lifetime. Best-effort: a user forcing
        // a re-login is already past caring whether the old grant answers.
        if let Some(session) = stored.as_ref() {
            if let Err(error) = session.revoke() {
                tracing::debug!(%error, "revoking the previous refresh token failed");
            }
        }
    } else if let Some(mut session) = stored {
        session.adopt_refresh_context(&cfg.service.url, identity_scopes(&cfg));
        if session.refresh_if_needed(now_unix()).is_ok() {
            // A local clock check is exactly the check that lies here: the
            // access token refreshes for 30 days, while the browser session
            // that authorized the CLI lives 12 hours. Ask the service, live —
            // never `fetch_or_cached`, whose whole job is to answer without
            // asking, and never `/auth/me`, which never reaches the code that
            // fails.
            let probe = if cfg.has_http_service() {
                let client = ServiceClient::from_config(&cfg).context("building service client")?;
                Some(client.fetch(&session, now_unix()))
            } else {
                None
            };
            let decision = match &probe {
                Some(probe) => login_decision(is_oidc(&cfg), probe),
                // A `file` source has no session for a service to reject.
                None => LoginDecision::Valid,
            };
            match decision {
                LoginDecision::Valid => {
                    println!("{}", describe_session(&session, "Already logged in"));
                    println!("Run `blue logout` before signing in with a different account.");
                    return Ok(());
                }
                LoginDecision::ActionRequired => {
                    println!("{}", describe_session(&session, "Already logged in"));
                    if let Some(Err(error)) = &probe {
                        println!("{error}");
                    }
                    return Ok(());
                }
                LoginDecision::Unverified => {
                    println!("{}", describe_session(&session, "Already logged in"));
                    if let Some(Err(error)) = &probe {
                        println!("Could not verify the session with the service: {error}");
                    }
                    println!("Run `blue login --force` to sign in again anyway.");
                    return Ok(());
                }
                LoginDecision::Rejected => match &probe {
                    Some(Err(error)) => bail!("{error}"),
                    _ => unreachable!("Rejected is only produced from a rejection"),
                },
                // Fall through to the device flow.
                LoginDecision::Reauthenticate => {
                    if let Some(Err(error)) = &probe {
                        println!("{error}");
                    }
                    replacement_previous = Some(session);
                }
            }
        } else {
            replacement_previous = Some(session);
        }
    }

    // Deliberately not `Session::remove()` first: a Ctrl-C during the browser
    // step would then have destroyed a session that still worked.
    // `gh_service::login` overwrites the file only once it has succeeded.
    let session = match replacement_previous.as_ref() {
        Some(previous) => reauthenticate_replacing(&cfg, previous)?,
        None => gh_service::login(&cfg).context("login")?,
    };
    println!("{}", describe_session(&session, "Logged in"));
    Ok(())
}

pub fn logout() -> Result<()> {
    let cfg = BlueToml::load().ok();
    let mut session = Session::load()?;
    let gateway_revocation = session.as_mut().map(|session| {
        if let Some(cfg) = cfg.as_ref() {
            session.adopt_refresh_context(&cfg.service.url, identity_scopes(cfg));
        }
        let _ = session.refresh_if_needed(now_unix());
        cfg.as_ref()
            .map(|cfg| session.revoke_gateway_session(&cfg.service.url))
            .unwrap_or(Ok(()))
    });
    let revocation = session.as_ref().map(|session| session.revoke()).transpose();
    Session::remove()?;
    if let Some(Err(error)) = gateway_revocation {
        tracing::warn!(%error, "remote gateway session revocation failed; local tokens were removed");
    }
    if let Err(error) = revocation {
        tracing::warn!(%error, "remote token revocation failed; local tokens were removed");
    }
    println!("Logged out. Local OAuth tokens removed.");
    Ok(())
}

#[derive(Debug, Deserialize)]
struct GatewayKeyResponse {
    enabled: bool,
    email: String,
    status: String,
    alias: Option<String>,
    external_id: Option<String>,
    expires_at: Option<String>,
    last_reconciled_at: Option<String>,
    error: Option<String>,
    invalidation_reason: Option<String>,
    next_retry_at: Option<String>,
}

#[derive(Debug)]
struct GatewayHttpError {
    status: reqwest::StatusCode,
    /// The server's `error` field when it sent JSON, else the raw body. Used
    /// verbatim for the generic arms.
    message: String,
    /// Only the structured `{"error": …}` string, bounded. `None` when an
    /// intermediary answered with something that is not the Control API's
    /// error shape — an HTML 401 page from a load balancer, say.
    detail: Option<String>,
}

impl std::fmt::Display for GatewayHttpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&gateway_api_error_message(
            self.status,
            &self.message,
            self.detail.as_deref(),
        ))
    }
}

impl std::error::Error for GatewayHttpError {}

fn gateway_api_error_message(
    status: reqwest::StatusCode,
    message: &str,
    detail: Option<&str>,
) -> String {
    if matches!(
        status,
        reqwest::StatusCode::BAD_GATEWAY | reqwest::StatusCode::SERVICE_UNAVAILABLE
    ) {
        return gateway_unavailable_message();
    }
    // `blue run` reaches this hop before it ever fetches policy, so without
    // this arm an expired session surfaces as a bare "gateway access: ..." and
    // the user is never told what to do about it. Mirrors the governance-config
    // path so the same rejection reads the same wherever it surfaces.
    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        return gh_service::session_rejected_message(
            status == reqwest::StatusCode::FORBIDDEN,
            detail,
        );
    }
    format!("gateway access: {message}")
}

fn gateway_unavailable_message() -> String {
    concat!(
        "Gateway setup is currently unavailable.\n\n",
        "Blue couldn't prepare gateway access, so your coding agent was not started.\n",
        "The configured gateway or its provisioner may be temporarily unavailable."
    )
    .to_owned()
}

fn gateway_provisioning_failure(error: Option<&str>, fallback: &str) -> String {
    match error {
        Some(error) if error.contains("gateway is unavailable") => gateway_unavailable_message(),
        Some(error) => format!("gateway key provisioning failed: {error}"),
        None => format!("gateway key provisioning failed: {fallback}"),
    }
}

fn gateway_api<T: serde::de::DeserializeOwned>(
    cfg: &BlueToml,
    session: &Session,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<T> {
    let base = reqwest::Url::parse(&cfg.service.url).context("parsing service URL")?;
    let url = base.join(path).context("building gateway API URL")?;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .context("building gateway API client")?;
    let request = if let Some(body) = body {
        client.post(url).bearer_auth(&session.token).json(&body)
    } else {
        client.get(url).bearer_auth(&session.token)
    };
    let response = request
        .send()
        .context("contacting gateway access service")?;
    let status = response.status();
    let text = response.text().context("reading gateway access response")?;
    if !status.is_success() {
        let detail = gh_service::server_error_detail(&text);
        let message = detail.clone().unwrap_or(text);
        return Err(GatewayHttpError {
            status,
            message,
            detail,
        }
        .into());
    }
    serde_json::from_str(&text).context("decoding gateway access response")
}

/// Whether an ensure/key response still needs gateway provisioning to complete.
///
/// A disabled response (`enabled:false`) is authoritative "gateway mode is off"
/// — governance-only deployments answer this way — so the ensure step is a
/// no-op. Only gateway-on responses that aren't `ready` require follow-up.
fn gateway_access_requires_provisioning(access: &GatewayKeyResponse) -> bool {
    access.enabled && access.status != "ready"
}

fn ensure_gateway_access(cfg: &BlueToml, session: &Session) -> Result<()> {
    if !cfg.has_http_service() || cfg.mode.force_governance_only {
        return Ok(());
    }
    let mut access: GatewayKeyResponse = match gateway_api(
        cfg,
        session,
        "/gateway/key/ensure",
        Some(serde_json::json!({})),
    ) {
        Ok(access) => access,
        Err(error) if retryable_gateway_http_error(&error) => {
            retry_gateway_setup(cfg, session, None)?
        }
        Err(error) => return Err(error),
    };
    if access.enabled && access.status == "error" {
        access = retry_gateway_setup(
            cfg,
            session,
            access.error.as_deref().or(access.next_retry_at.as_deref()),
        )?;
    }
    if access.enabled && access.status == "invalid" {
        let reason = access
            .invalidation_reason
            .as_deref()
            .unwrap_or("the stored gateway key is no longer valid");
        if !interactive_terminal() {
            bail!(
                "gateway key is invalid: {reason}; run `blue gateway` in an interactive terminal to provision a replacement"
            );
        }
        cliclack::log::warning(format!("Gateway key is invalid: {reason}"))?;
        let confirmed = cliclack::confirm("Provision a new gateway key now?")
            .initial_value(false)
            .interact()?;
        if !confirmed {
            bail!("gateway key replacement declined; run `blue gateway` when ready");
        }
        access = gateway_api(
            cfg,
            session,
            "/gateway/key/ensure",
            Some(serde_json::json!({"manual": true})),
        )?;
    }
    if gateway_access_requires_provisioning(&access) {
        let fallback = match access.status.as_str() {
            "invalid" => "the stored gateway key is invalid",
            "recovering" => "gateway key replacement is already in progress; try again shortly",
            "missing" => "gateway key must be provisioned",
            _ => "gateway key provisioning did not complete",
        };
        bail!(gateway_provisioning_failure(
            access.error.as_deref(),
            fallback
        ));
    }
    Ok(())
}

fn retryable_gateway_http_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<GatewayHttpError>()
        .is_some_and(|error| {
            matches!(
                error.status,
                reqwest::StatusCode::BAD_GATEWAY | reqwest::StatusCode::SERVICE_UNAVAILABLE
            )
        })
}

fn retry_gateway_setup(
    cfg: &BlueToml,
    session: &Session,
    detail: Option<&str>,
) -> Result<GatewayKeyResponse> {
    if !interactive_terminal() {
        let detail = detail.unwrap_or("gateway provisioning encountered a temporary error");
        bail!(
            "gateway setup needs attention: {detail}; run `blue gateway` in an interactive terminal to retry"
        );
    }
    cliclack::log::warning("Gateway setup could not be completed.")?;
    let confirmed = cliclack::confirm("Retry gateway setup now?")
        .initial_value(true)
        .interact()?;
    if !confirmed {
        bail!("gateway setup retry declined; run `blue gateway` when ready");
    }
    gateway_api(
        cfg,
        session,
        "/gateway/key/ensure",
        Some(serde_json::json!({"manual": true})),
    )
}

pub fn gateway() -> Result<()> {
    println!("{}", gateway_text()?);
    Ok(())
}

pub(crate) fn gateway_text() -> Result<String> {
    let (cfg, _) = load_client()?;
    if !cfg.has_http_service() {
        bail!("gateway access requires an HTTP governance service");
    }
    let session = session_for(&cfg)?;
    ensure_gateway_access(&cfg, &session)?;
    let access: GatewayKeyResponse = gateway_api(&cfg, &session, "/gateway/key", None)?;
    if !access.enabled {
        return Ok("Gateway mode is not enabled by the governance service.".into());
    }
    let output = [
        format!("Gateway account: {}", access.email),
        format!("  status          : {}", access.status),
        format!(
            "  alias           : {}",
            access.alias.as_deref().unwrap_or("—")
        ),
        format!(
            "  gateway id      : {}",
            access.external_id.as_deref().unwrap_or("—")
        ),
        format!(
            "  last reconciled : {}",
            access.last_reconciled_at.as_deref().unwrap_or("—")
        ),
        format!(
            "  expires         : {}",
            access.expires_at.as_deref().unwrap_or("—")
        ),
    ]
    .join("\n");
    if let Some(error) = access.error {
        bail!("gateway provisioning: {error}");
    }
    Ok(output)
}

#[derive(Deserialize)]
struct DependencyHealthCheck {
    name: String,
    status: String,
    latency_ms: u64,
    error: Option<String>,
}

#[derive(Deserialize)]
struct DependencyHealthResponse {
    status: String,
    checked_at: String,
    checks: Vec<DependencyHealthCheck>,
}

pub(crate) fn health_text() -> Result<String> {
    let (cfg, _) = load_client()?;
    if !cfg.has_http_service() {
        return Ok([
            "Blue health: healthy".to_owned(),
            "  control_api      : local file mode".to_owned(),
            "  dependencies     : not applicable".to_owned(),
        ]
        .join("\n"));
    }
    let session = session_for(&cfg)?;
    let health: DependencyHealthResponse =
        gateway_api(&cfg, &session, "/health/dependencies", None)?;
    let mut lines = vec![
        format!("Blue health: {}", health.status),
        format!("  checked          : {}", health.checked_at),
        "  control_api      : healthy".to_owned(),
    ];
    for check in health.checks {
        let mut value = format!("{} ({} ms)", check.status, check.latency_ms);
        if let Some(error) = check.error {
            value.push_str(&format!(" — {error}"));
        }
        lines.push(format!("  {:<17}: {value}", check.name));
    }
    Ok(lines.join("\n"))
}

pub fn doctor() -> Result<()> {
    println!("{}", doctor_text()?);
    Ok(())
}

pub(crate) fn doctor_text() -> Result<String> {
    let (cfg, client) = load_client()?;
    let mut lines = vec![
        "blue doctor".to_owned(),
        format!("  config source : {}", client.describe_source()),
    ];
    // A file-existence check is exactly the check that lies here: the session
    // file is still present and its access token still refreshes long after
    // the backing browser session is gone. Ask the service instead, live —
    // never through the cache, which cannot tell us anything about auth.
    let live = session_for(&cfg).map(|session| client.fetch(&session, now_unix()));
    match (Session::load()?, cfg.has_http_service(), &live) {
        (None, true, _) => lines.push("  session       : MISSING (run `blue login`)".into()),
        (None, false, _) => lines.push("  session       : n/a (local file source)".into()),
        (Some(_), _, Ok(Err(gh_common::GhError::Unauthorized(_)))) => {
            lines.push("  session       : EXPIRED (run `blue login`)".into())
        }
        (Some(_), _, Err(error))
            if error
                .downcast_ref::<gh_common::GhError>()
                .is_some_and(|error| matches!(error, gh_common::GhError::Unauthorized(_))) =>
        {
            lines.push("  session       : EXPIRED (run `blue login`)".into())
        }
        (Some(_), _, Ok(Err(gh_common::GhError::ActionRequired(message)))) => {
            lines.push("  session       : present".into());
            lines.push(format!("  gateway       : {message}"));
        }
        (Some(_), _, Ok(Err(_)) | Err(_)) => {
            lines.push("  session       : present".into());
            lines.push("  service       : unreachable".into());
        }
        (Some(_), _, Ok(Ok(_))) => lines.push("  session       : present".into()),
    }

    let allowed: Option<GovernanceConfig> = match live {
        Ok(Ok(config)) => Some(config),
        // Only fall back to cache for a genuine outage; an auth failure must
        // not be papered over with stale policy.
        Ok(Err(gh_common::GhError::Unauthorized(_) | gh_common::GhError::ActionRequired(_))) => {
            None
        }
        _ => gh_service::cache::load()
            .ok()
            .flatten()
            .map(|cached| cached.config),
    };
    if let Some(c) = &allowed {
        lines.push(format!("  revision      : {}", c.revision));
        lines.push(format!(
            "  allowed       : {}",
            c.allowed_harnesses.join(", ")
        ));
        if let Some(expires_at) = c
            .gateway
            .as_ref()
            .and_then(|gateway| gateway.token.as_deref())
            .and_then(jwt_expires_at)
        {
            let remaining = (expires_at - now_unix()).max(0) / 60;
            lines.push(format!("  gateway token : expires in {remaining}m"));
        }
    } else {
        lines.push("  revision      : (config unavailable)".into());
    }

    lines.push(String::new());
    lines.push("Harnesses:".into());
    for (harness, detected) in gh_harness::detect_all() {
        let status: String;
        if let Some(d) = detected {
            let ver = d
                .version
                .map(|version| version.to_string())
                .or(d.raw_version)
                .unwrap_or_else(|| "unknown version".into());
            status = format!("{}  [{}]", d.path.display(), ver);
        } else {
            status = String::from("not installed");
        }
        let policy = match &allowed {
            Some(c) if c.is_allowed(harness.key()) => "allowed",
            Some(_) => "denied ",
            None => "unknown",
        };
        lines.push(format!("  {:<9} {:<8} {}", harness.key(), policy, status));
    }
    Ok(lines.join("\n"))
}

#[derive(Clone, Serialize, Deserialize)]
struct AppliedState {
    #[serde(default)]
    schema_version: u32,
    revision: String,
    applied_at: i64,
    files: Vec<PathBuf>,
    harnesses: Vec<String>,
    #[serde(default)]
    file_sha256: BTreeMap<PathBuf, String>,
    #[serde(default)]
    harness_inventory: Vec<HarnessInventoryEntry>,
    #[serde(default)]
    files_by_harness: BTreeMap<String, Vec<PathBuf>>,
    #[serde(default)]
    binary_fingerprints: BTreeMap<String, BinaryFingerprint>,
    #[serde(default)]
    blue_binary_fingerprint: Option<BinaryFingerprint>,
    #[serde(default)]
    managed_fingerprints: BTreeMap<String, Vec<ManagedPathFingerprint>>,
    #[serde(default)]
    gateway_enabled: bool,
}

const APPLIED_STATE_SCHEMA_VERSION: u32 = 4;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct BinaryFingerprint {
    path: PathBuf,
    canonical_path: PathBuf,
    size: u64,
    modified_nanos: u128,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
struct ManagedPathFingerprint {
    path: PathBuf,
    kind: String,
    size: u64,
    modified_nanos: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    changed_nanos: Option<i128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inode: Option<u64>,
}

fn binary_fingerprint(path: &Path) -> Option<BinaryFingerprint> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified_nanos = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(BinaryFingerprint {
        path: path.to_path_buf(),
        canonical_path: std::fs::canonicalize(path).ok()?,
        size: metadata.len(),
        modified_nanos,
    })
}

fn blue_binary_fingerprint() -> Option<BinaryFingerprint> {
    binary_fingerprint(&std::env::current_exe().ok()?)
}

fn managed_path_fingerprint(path: &Path) -> Result<ManagedPathFingerprint> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading metadata for {}", path.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        bail!("managed path {} is a symlink", path.display());
    }
    let kind = if file_type.is_file() {
        "file"
    } else if file_type.is_dir() {
        "directory"
    } else {
        bail!("managed path {} has an unsupported type", path.display());
    };
    let modified_nanos = metadata
        .modified()
        .with_context(|| format!("reading modification time for {}", path.display()))?
        .duration_since(std::time::UNIX_EPOCH)
        .with_context(|| format!("invalid modification time for {}", path.display()))?
        .as_nanos();
    #[cfg(unix)]
    let (changed_nanos, device, inode) = {
        use std::os::unix::fs::MetadataExt;
        let changed =
            i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec());
        (Some(changed), Some(metadata.dev()), Some(metadata.ino()))
    };
    #[cfg(not(unix))]
    let (changed_nanos, device, inode) = (None, None, None);
    Ok(ManagedPathFingerprint {
        path: path.to_path_buf(),
        kind: kind.to_owned(),
        size: metadata.len(),
        modified_nanos,
        changed_nanos,
        device,
        inode,
    })
}

fn managed_fingerprints(paths: &[PathBuf]) -> Result<Vec<ManagedPathFingerprint>> {
    fn visit(path: &Path, entries: &mut BTreeMap<PathBuf, ManagedPathFingerprint>) -> Result<()> {
        let fingerprint = managed_path_fingerprint(path)?;
        let is_dir = fingerprint.kind == "directory";
        entries.insert(path.to_path_buf(), fingerprint);
        if is_dir {
            let mut children = std::fs::read_dir(path)
                .with_context(|| format!("reading managed directory {}", path.display()))?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<Vec<_>>>()?;
            children.sort();
            for child in children {
                visit(&child, entries)?;
            }
        }
        Ok(())
    }

    let mut entries = BTreeMap::new();
    for path in paths {
        visit(path, &mut entries)?;
    }
    Ok(entries.into_values().collect())
}

fn file_sha256(path: &Path) -> Result<String> {
    if path.is_file() {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        return Ok(hex::encode(Sha256::digest(bytes)));
    }
    if !path.is_dir() {
        bail!("managed path {} is missing", path.display());
    }
    fn visit(root: &Path, current: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(current)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                bail!(
                    "managed directory contains symlink {}",
                    entry.path().display()
                );
            }
            if kind.is_dir() {
                visit(root, &entry.path(), paths)?;
            } else if kind.is_file() {
                paths.push(entry.path().strip_prefix(root)?.to_path_buf());
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    visit(path, path, &mut paths)?;
    paths.sort();
    let mut digest = Sha256::new();
    for relative in paths {
        digest.update(relative.to_string_lossy().as_bytes());
        digest.update([0]);
        digest.update(std::fs::read(path.join(relative))?);
    }
    Ok(hex::encode(digest.finalize()))
}

fn applied_harness_files_match(state: &AppliedState, harness: Harness) -> bool {
    state
        .files_by_harness
        .get(harness.key())
        .is_some_and(|files| {
            !files.is_empty()
                && files.iter().all(|file| {
                    state.file_sha256.get(file).is_some_and(|expected| {
                        file_sha256(file).is_ok_and(|actual| actual == *expected)
                    })
                })
        })
}

fn applied_state_path() -> Result<PathBuf> {
    Ok(gh_common::paths::blue_config_dir()?.join("applied-state.json"))
}

fn load_applied_state() -> Result<Option<AppliedState>> {
    let path = applied_state_path()?;
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("decoding applied-state.json")?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn selected_applied_state_is_current(
    state: &AppliedState,
    config: &GovernanceConfig,
    harness: Harness,
    binary: &Path,
    gateway_enabled: bool,
) -> bool {
    if state.schema_version != APPLIED_STATE_SCHEMA_VERSION
        || state.revision != config.revision
        || state.gateway_enabled != gateway_enabled
        || !state.harnesses.iter().any(|name| name == harness.key())
        || state.binary_fingerprints.get(harness.key()) != binary_fingerprint(binary).as_ref()
        || state.blue_binary_fingerprint != blue_binary_fingerprint()
    {
        return false;
    }
    let Some(paths) = state.files_by_harness.get(harness.key()) else {
        return false;
    };
    let Some(expected) = state.managed_fingerprints.get(harness.key()) else {
        return false;
    };
    !paths.is_empty()
        && !expected.is_empty()
        && managed_fingerprints(paths)
            .map(|actual| actual == *expected)
            .unwrap_or(false)
}

fn selected_applied_state_is_current_after_version_check(
    state: &AppliedState,
    config: &GovernanceConfig,
    harness: Harness,
    binary: &Path,
    gateway_enabled: bool,
    version_repaired: bool,
) -> bool {
    !version_repaired
        && selected_applied_state_is_current(state, config, harness, binary, gateway_enabled)
}

fn discover_inventory_cached(
    allowed: &[String],
    applied: Option<&AppliedState>,
) -> HarnessInventory {
    let applied = applied.filter(|state| state.schema_version == APPLIED_STATE_SCHEMA_VERSION);
    let cached_entries = applied
        .map(|state| {
            state
                .harness_inventory
                .iter()
                .map(|entry| (entry.name.clone(), entry.clone()))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let cached_fingerprints = applied
        .map(|state| state.binary_fingerprints.clone())
        .unwrap_or_default();
    let detections = std::thread::scope(|scope| {
        Harness::ALL
            .iter()
            .copied()
            .map(|harness| {
                let cached = cached_entries.get(harness.key()).cloned();
                let fingerprint = cached_fingerprints.get(harness.key()).cloned();
                scope.spawn(move || {
                    let path = harness
                        .binary_names()
                        .iter()
                        .find_map(|name| gh_harness::which(name));
                    let reused = path.as_ref().and_then(|path| {
                        let current = binary_fingerprint(path)?;
                        let cached_fingerprint = fingerprint.as_ref()?;
                        let cached = cached.as_ref()?;
                        (current == *cached_fingerprint).then(|| Detected {
                            harness,
                            path: path.clone(),
                            raw_version: cached.raw_version.clone(),
                            version: cached.version.clone(),
                        })
                    });
                    (
                        harness.key().to_owned(),
                        reused.or_else(|| gh_harness::detect(harness)),
                    )
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().expect("harness detection thread panicked"))
            .collect::<BTreeMap<_, _>>()
    });
    HarnessInventory::discover_with_detector(allowed, |harness| {
        detections.get(harness.key()).cloned().flatten()
    })
}

fn inventory_fingerprints(inventory: &HarnessInventory) -> BTreeMap<String, BinaryFingerprint> {
    inventory
        .entries
        .iter()
        .filter_map(|entry| {
            Some((
                entry.name.clone(),
                binary_fingerprint(entry.path.as_deref()?)?,
            ))
        })
        .collect()
}

fn save_applied_state(state: &AppliedState) -> Result<()> {
    gh_common::write_atomic(&applied_state_path()?, serde_json::to_vec_pretty(state)?)?;
    Ok(())
}

fn refresh_selected_applied_state(
    state: &mut AppliedState,
    revision: &str,
    harness: Harness,
    write: &gh_config::HarnessWrite,
    inventory: &HarnessInventory,
    gateway_enabled: bool,
) -> Result<()> {
    update_selected_applied_state(state, revision, harness, write, inventory, gateway_enabled)?;
    save_applied_state(state)
}

fn update_selected_applied_state(
    state: &mut AppliedState,
    revision: &str,
    harness: Harness,
    write: &gh_config::HarnessWrite,
    inventory: &HarnessInventory,
    gateway_enabled: bool,
) -> Result<()> {
    if state.schema_version != APPLIED_STATE_SCHEMA_VERSION
        || state.revision != revision
        || state.gateway_enabled != gateway_enabled
    {
        state.revision = revision.to_owned();
        state.files.clear();
        state.harnesses.clear();
        state.file_sha256.clear();
        state.files_by_harness.clear();
        state.managed_fingerprints.clear();
    }
    if let Some(previous) = state.files_by_harness.remove(harness.key()) {
        let previous = previous.into_iter().collect::<BTreeSet<_>>();
        state.files.retain(|path| !previous.contains(path));
        state.file_sha256.retain(|path, _| !previous.contains(path));
    }
    for path in &write.files {
        state.file_sha256.insert(path.clone(), file_sha256(path)?);
        state.files.push(path.clone());
    }
    state.files.sort();
    state.files.dedup();
    state
        .files_by_harness
        .insert(harness.key().to_owned(), write.files.clone());
    state.managed_fingerprints.insert(
        harness.key().to_owned(),
        managed_fingerprints(&write.files)?,
    );
    if !state.harnesses.iter().any(|name| name == harness.key()) {
        state.harnesses.push(harness.key().to_owned());
        state.harnesses.sort();
    }
    state.schema_version = APPLIED_STATE_SCHEMA_VERSION;
    state.gateway_enabled = gateway_enabled;
    let mut applied_inventory = inventory.clone();
    applied_inventory.mark_reconciled(state.harnesses.clone());
    state.harness_inventory = applied_inventory.entries;
    state.binary_fingerprints = inventory_fingerprints(inventory);
    state.blue_binary_fingerprint = blue_binary_fingerprint();
    state.applied_at = now_unix();
    Ok(())
}

fn read_instance_id(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(value) => {
            let value = value.trim();
            if value.is_empty() {
                bail!("tenant instance ID at {} is empty", path.display());
            }
            Ok(Some(value.to_owned()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("reading tenant instance ID at {}", path.display()))
        }
    }
}

fn create_instance_id(path: &Path, candidate: &str) -> Result<String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating tenant identity directory {}", parent.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(candidate.as_bytes())?;
            file.sync_all()?;
            Ok(candidate.to_owned())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            read_instance_id(path)?.ok_or_else(|| anyhow!("tenant instance ID disappeared"))
        }
        Err(error) => {
            Err(error).with_context(|| format!("creating tenant instance ID at {}", path.display()))
        }
    }
}

fn tenant_instance_id_at(config_root: &Path, cfg: &BlueToml, session: &Session) -> Result<String> {
    let scoped = match session
        .org_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => {
            let organization_id = uuid::Uuid::parse_str(value)
                .context("authenticated session contains an invalid organization ID")?;
            config_root
                .join("identities")
                .join("org")
                .join(organization_id.to_string())
                .join("instance-id")
        }
        None => config_root
            .join("identities")
            .join("deployment")
            .join(tenant_id(&cfg.service.url))
            .join("instance-id"),
    };
    if let Some(value) = read_instance_id(&scoped)? {
        // Once a scoped identity exists, the legacy global value must not be
        // adopted by a second tenant.
        let legacy = config_root.join("instance-id");
        if legacy.exists() {
            std::fs::remove_file(&legacy)
                .with_context(|| format!("removing legacy instance ID {}", legacy.display()))?;
        }
        return Ok(value);
    }

    let legacy = config_root.join("instance-id");
    let candidate = read_instance_id(&legacy)?.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let value = create_instance_id(&scoped, &candidate)?;
    if legacy.exists() {
        std::fs::remove_file(&legacy)
            .with_context(|| format!("removing legacy instance ID {}", legacy.display()))?;
    }
    Ok(value)
}

fn instance_id(cfg: &BlueToml, session: &Session) -> Result<String> {
    tenant_instance_id_at(&gh_common::paths::blue_config_dir()?, cfg, session)
}

fn report_status(
    cfg: &BlueToml,
    session: &Session,
    state: Option<&AppliedState>,
    inventory: &[HarnessInventoryEntry],
    files_ok: bool,
    error: Option<&str>,
) {
    report_status_with_attempt(cfg, session, state, inventory, files_ok, error, None);
}

fn report_status_with_attempt(
    cfg: &BlueToml,
    session: &Session,
    state: Option<&AppliedState>,
    inventory: &[HarnessInventoryEntry],
    files_ok: bool,
    error: Option<&str>,
    reconciliation_attempt: Option<(Harness, &str)>,
) {
    if !cfg.has_http_service() || session.token.is_empty() {
        return;
    }
    let Ok(endpoint) =
        reqwest::Url::parse(&cfg.service.url).and_then(|url| url.join("client-status"))
    else {
        return;
    };
    let hostname = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok());
    let Ok(id) = instance_id(cfg, session) else {
        return;
    };
    let body = serde_json::json!({
        "instance_id": id,
        "hostname": hostname,
        "client_version": env!("CARGO_PKG_VERSION"),
        "platform": std::env::consts::OS,
        "architecture": std::env::consts::ARCH,
        "config_revision": state.map(|item| item.revision.as_str()),
        "applied": state.is_some(),
        "files_ok": files_ok,
        "harnesses": inventory,
        "packages": package_statuses().unwrap_or_default(),
        "reconciliation_attempt": reconciliation_attempt.map(|(harness, revision)| serde_json::json!({
            "harness": harness.key(),
            "revision": revision,
        })),
        "error": error,
    });
    let client = match reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(1))
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(client) => client,
        Err(report_error) => {
            tracing::warn!(error=%report_error, "building client status reporter failed");
            return;
        }
    };
    if let Err(report_error) = client
        .post(endpoint)
        .bearer_auth(&session.token)
        .json(&body)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
    {
        tracing::warn!(error=%report_error, "client status report failed");
    }
}

fn report_status_async(
    cfg: BlueToml,
    session: Session,
    state: Option<AppliedState>,
    inventory: Vec<HarnessInventoryEntry>,
    harness: Harness,
    error: Option<String>,
) {
    std::thread::spawn(move || {
        let files_ok = state
            .as_ref()
            .is_some_and(|state| applied_harness_files_match(state, harness));
        let error = error.or_else(|| {
            (!files_ok).then(|| "managed configuration is missing or has drifted".to_owned())
        });
        report_status(
            &cfg,
            &session,
            state.as_ref(),
            &inventory,
            files_ok,
            error.as_deref(),
        );
    });
}

fn report_inventory_failure(
    cfg: &BlueToml,
    session: &Session,
    inventory: &HarnessInventory,
    harness: Harness,
    revision: &str,
    error: &anyhow::Error,
) {
    let applied = load_applied_state().ok().flatten();
    let error = format!("{error:#}");
    report_status_with_attempt(
        cfg,
        session,
        applied.as_ref(),
        &inventory.entries,
        false,
        Some(&error),
        Some((harness, revision)),
    );
}

fn daemon_reconcile_errors(
    inventory: &HarnessInventory,
    results: &[gh_agent::HarnessReconcile],
) -> String {
    let mut failures = inventory
        .entries
        .iter()
        .filter_map(|entry| {
            entry
                .compatibility_error
                .as_ref()
                .map(|error| format!("{}: {error}", entry.name))
        })
        .collect::<Vec<_>>();
    for result in results {
        match &result.result {
            Ok(write) => failures.extend(
                write
                    .package_errors
                    .iter()
                    .map(|error| format!("{} package: {error}", result.harness.key())),
            ),
            Err(error) => failures.push(format!("{}: {error}", result.harness.key())),
        }
    }
    if failures.is_empty() {
        "configuration reconciliation failed".to_owned()
    } else {
        failures.join("; ")
    }
}

pub fn status(strict: bool) -> Result<()> {
    println!("{}", status_text(strict)?);
    Ok(())
}

pub(crate) fn status_text(strict: bool) -> Result<String> {
    let (cfg, client) = load_client()?;
    let session = session_for(&cfg)?;
    ensure_gateway_access(&cfg, &session)?;
    let desired = client
        .fetch_or_cached(&session, now_unix())
        .context("fetching desired configuration")?;
    let applied = load_applied_state()?;
    let revision_ok = applied
        .as_ref()
        .map(|state| state.revision == desired.revision)
        .unwrap_or(false);
    let mut inventory = HarnessInventory::discover(&desired.allowed_harnesses);
    gh_agent::evaluate_inventory(&desired, &mut inventory);
    let default = configured_default_harness(&cfg, &inventory).ok();
    let files_ok = default.is_some_and(|harness| {
        let binary = inventory
            .entries
            .iter()
            .find(|entry| entry.name == harness.key())
            .and_then(|entry| entry.path.as_deref());
        applied.as_ref().is_some_and(|state| {
            binary.is_some_and(|binary| {
                selected_applied_state_is_current(
                    state,
                    &desired,
                    harness,
                    binary,
                    desired.gateway.is_some() && !cfg.mode.force_governance_only,
                )
            })
        })
    });
    if revision_ok {
        if let Some(state) = applied.as_ref() {
            inventory.mark_reconciled(state.harnesses.clone());
        }
    }
    let mut lines = vec![
        format!(
            "Authentication : {}",
            session.email.as_deref().unwrap_or("authenticated")
        ),
        format!("Desired config : {}", desired.revision),
        format!(
            "Applied config : {}",
            applied
                .as_ref()
                .map(|state| state.revision.as_str())
                .unwrap_or("not applied")
        ),
        format!(
            "Managed files  : {}",
            if files_ok {
                "present"
            } else {
                "missing or not applied"
            }
        ),
        format!(
            "Overall        : {}",
            if revision_ok && files_ok {
                "up to date"
            } else {
                "ACTION REQUIRED — run `blue apply`"
            }
        ),
    ];
    let packages = package_statuses().unwrap_or_default();
    if !desired.packages.is_empty() || !packages.is_empty() {
        lines.push("Managed packages:".into());
        for package in packages {
            lines.push(format!(
                "  {:<16} {:<9} {:<10} {}{}",
                package.id,
                package.harness,
                package.state,
                package.version,
                package
                    .error
                    .as_deref()
                    .map(|error| format!(" — {error}"))
                    .unwrap_or_default()
            ));
        }
    }
    report_status(
        &cfg,
        &session,
        applied.as_ref(),
        &inventory.entries,
        files_ok,
        if files_ok {
            None
        } else {
            Some("managed configuration is missing or has drifted")
        },
    );
    if strict && (!revision_ok || !files_ok) {
        bail!("managed configuration verification failed");
    }
    Ok(lines.join("\n"))
}

pub fn config() -> Result<()> {
    let (cfg, client) = load_client()?;
    let session = session_for(&cfg)?;
    ensure_gateway_access(&cfg, &session)?;
    let config = client
        .fetch_or_cached(&session, now_unix())
        .context("fetching governance config")?;
    println!("# source: {}", client.describe_source());
    println!("{}", serde_json::to_string_pretty(&config)?);
    Ok(())
}

fn inventory_detail(entry: &HarnessInventoryEntry) -> String {
    let mut detail = entry
        .path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "not found on PATH".into());
    if let Some(version) = &entry.version {
        detail.push_str(&format!("  [{version}]"));
    } else if let Some(raw) = &entry.raw_version {
        detail.push_str(&format!("  [{raw}]"));
    }
    if let Some(profile) = &entry.compatibility_profile {
        detail.push_str(&format!("  profile={profile}"));
    }
    if let Some(error) = &entry.compatibility_error {
        detail.push_str(&format!("  {error}"));
    }
    if let Some(warning) = &entry.compatibility_warning {
        detail.push_str(&format!("  warning={warning}"));
    }
    detail
}

fn show_harness_inventory(inventory: &HarnessInventory, styled: bool) -> Result<()> {
    if styled {
        cliclack::intro(
            console::style(" Local harness reconciliation ")
                .cyan()
                .bold(),
        )?;
        for entry in &inventory.entries {
            let message = format!(
                "{:<9} {:<22} {}",
                entry.name,
                entry.status(),
                inventory_detail(entry)
            );
            match entry.status() {
                "ready" | "reconciled" => cliclack::log::success(message)?,
                "not-installed" | "unsupported-client" => cliclack::log::warning(message)?,
                _ => cliclack::log::remark(message)?,
            }
        }
        cliclack::outro("Only API-allowed harnesses found on PATH will be reconciled.")?;
    } else {
        println!("Harness inventory (API policy × local PATH):");
        for entry in &inventory.entries {
            println!(
                "  {:<9} {:<22} {}",
                entry.name,
                entry.status(),
                inventory_detail(entry)
            );
        }
    }
    Ok(())
}

fn ensure_compatible_version(
    harness: Harness,
    detected: Detected,
    policy: &HarnessPolicy,
    interactive: bool,
) -> Result<(Detected, HarnessContext, bool)> {
    match resolve_compatibility(
        harness,
        detected.version.as_ref(),
        detected.raw_version.as_deref(),
        policy,
    ) {
        Ok(context) => return Ok((detected, context, false)),
        Err(error) if !interactive || !error.is_installable() => {
            return Err(anyhow!(error.with_install_hint(policy)))
        }
        Err(error) => cliclack::log::warning(error.with_install_hint(policy))?,
    }

    let invocation = supported_install(harness, policy)?;
    let confirmed = cliclack::confirm(format!(
        "Install a policy-supported {} version now?",
        harness.metadata().label
    ))
    .initial_value(false)
    .interact()?;
    if !confirmed {
        bail!(
            "{} installation declined; run `{}` when ready",
            harness.metadata().label,
            invocation.display
        );
    }

    if harness == Harness::Opencode {
        let version = resolve_npm_install_version(harness, &invocation)?;
        let program = detected.path.clone();
        let args = vec!["upgrade".into(), version.to_string()];
        let display = format!("'{}' upgrade {version}", program.display());
        run_installer_command(harness, &program, &args, &display)?;
    } else if harness == Harness::Kimi && !path_is_symlink(&detected.path) {
        let version = resolve_npm_install_version(harness, &invocation)?;
        run_kimi_standalone_installer(&detected.path, &version)?;
    } else {
        let program = PathBuf::from(invocation.program);
        run_installer_command(harness, &program, &invocation.args, &invocation.display)?;
    }

    let refreshed = gh_harness::detect_at(harness, detected.path.clone());
    let context = resolve_compatibility(
        harness,
        refreshed.version.as_ref(),
        refreshed.raw_version.as_deref(),
        policy,
    )
    .with_context(|| {
        format!(
            "the installer completed, but `{}` is still incompatible",
            detected.path.display()
        )
    })?;
    cliclack::log::success(format!(
        "{} {} is installed and supported",
        harness.metadata().label,
        context.version
    ))?;
    Ok((refreshed, context, true))
}

fn update_inventory_after_version_check(
    inventory: &mut HarnessInventory,
    detected: &Detected,
    context: &HarnessContext,
) {
    let Some(entry) = inventory
        .entries
        .iter_mut()
        .find(|entry| entry.name == detected.harness.key())
    else {
        return;
    };
    entry.installed = true;
    entry.path = Some(detected.path.clone());
    entry.raw_version = detected.raw_version.clone();
    entry.version = detected.version.clone();
    entry.compatibility_profile = Some(context.profile.id.to_owned());
    entry.compatibility_deprecated = context.profile.status == gh_config::ProfileStatus::Deprecated;
    entry.compatibility_error = None;
    entry.compatibility_warning = context.unverified_warning.clone();
    entry.reconciled = false;
}

fn path_is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn run_installer_command(
    harness: Harness,
    program: &Path,
    args: &[String],
    display: &str,
) -> Result<()> {
    cliclack::log::info(format!("Running `{display}`"))?;
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("starting `{}`", program.display()))?;
    if !status.success() {
        bail!(
            "{} installer exited with {}; retry with `{}`",
            harness.metadata().label,
            status,
            display
        );
    }
    Ok(())
}

fn run_kimi_standalone_installer(path: &Path, version: &semver::Version) -> Result<()> {
    let install_root = kimi_install_root(path).ok_or_else(|| {
        anyhow!(
            "cannot determine the Kimi installation root for `{}`",
            path.display()
        )
    })?;
    let url = "https://code.kimi.com/kimi-code/install.sh";
    let display = format!(
        "curl -fsSL {url} | KIMI_INSTALL_DIR='{}' KIMI_NO_MODIFY_PATH=1 bash -s -- --version {version}",
        install_root.display()
    );
    cliclack::log::info(format!("Running `{display}`"))?;
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
    let mut child = std::process::Command::new("bash")
        .args(["-s", "--", "--version", &version.to_string()])
        .env("KIMI_INSTALL_DIR", install_root)
        .env("KIMI_NO_MODIFY_PATH", "1")
        .stdin(Stdio::piped())
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

fn kimi_install_root(path: &Path) -> Option<&Path> {
    path.parent().and_then(Path::parent)
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
        .with_context(|| format!("looking up a published policy-supported {harness} version"))?;
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
        .context("npm returned an invalid OpenCode version response")?;
    let values = match &value {
        serde_json::Value::String(value) => vec![value.as_str()],
        serde_json::Value::Array(values) => values
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect(),
        _ => Vec::new(),
    };
    values
        .into_iter()
        .filter_map(|value| semver::Version::parse(value.trim_start_matches('v')).ok())
        .max()
        .ok_or_else(|| anyhow!("npm found no published OpenCode release matching the policy"))
}

fn repair_incompatible_inventory(
    config: &GovernanceConfig,
    inventory: &mut HarnessInventory,
    interactive: bool,
    target: Option<Harness>,
) -> Result<()> {
    if !interactive {
        return Ok(());
    }
    let incompatible = inventory
        .entries
        .iter()
        .filter(|entry| {
            entry.api_allowed
                && entry.client_supported
                && entry.installed
                && entry.compatibility_error.is_some()
        })
        .filter_map(HarnessInventoryEntry::harness)
        .filter(|harness| target.is_none_or(|target| target == *harness))
        .collect::<Vec<_>>();
    for harness in incompatible.iter().copied() {
        let detected = gh_harness::detect(harness)
            .ok_or_else(|| anyhow!("installed {} binary disappeared from PATH", harness.key()))?;
        let policy = config.policy(harness.key()).cloned().unwrap_or_default();
        ensure_compatible_version(harness, detected, &policy, true)?;
    }
    if !incompatible.is_empty() {
        *inventory = HarnessInventory::discover(&config.allowed_harnesses);
        gh_agent::evaluate_inventory(config, inventory);
    }
    Ok(())
}

pub fn run(name: &str, args: &[String]) -> Result<()> {
    let config_path = gh_common::paths::blue_toml_path()?;
    let newly_configured = !config_path.exists();
    let (cfg, client) = if newly_configured {
        if !interactive_terminal() {
            bail!(
                "Blue is not configured; run `blue setup` in a terminal before launching an agent"
            );
        }
        let cfg = activate_discovered(discover_configuration(&prompt_control_api_url()?)?)?;
        let client = ServiceClient::from_config(&cfg).context("building service client")?;
        (cfg, client)
    } else {
        load_client()?
    };
    let session = if newly_configured {
        ensure_session_for_start(&cfg)?
    } else {
        session_for(&cfg)?
    };
    let prepared = prepare_launch(cfg, client, session)?;
    run_prepared(name, args, prepared)
}

fn run_prepared(name: &str, args: &[String], prepared: PreparedLaunch) -> Result<()> {
    let startup_started = std::time::Instant::now();
    let harness: Harness = name
        .parse()
        .with_context(|| format!("`{name}` is not a known harness"))?;
    let interactive = interactive_terminal();
    let mut startup = interactive
        .then(|| crate::supervisor::StartupView::new(harness.key()))
        .transpose()?;
    let PreparedLaunch {
        cfg,
        client,
        session,
        config,
        mut inventory,
    } = prepared;
    if let Some(startup) = startup.as_mut() {
        startup.connected(if cfg.has_http_service() {
            cfg.service.url.clone()
        } else {
            client.describe_source()
        })?;
    }
    if let Some(startup) = startup.as_mut() {
        startup.authenticated(session.email.as_deref().unwrap_or("local session"))?;
    }

    let mut status_state = load_applied_state()?;
    let revision_current = status_state
        .as_ref()
        .is_some_and(|state| state.revision == config.revision);
    if let Some(startup) = startup.as_mut() {
        startup.policy(&config.revision, revision_current)?;
    }
    if !interactive {
        show_harness_inventory(&inventory, false)?;
    }
    gh_harness::ensure_allowed(harness, &config.allowed_harnesses)?;
    let detected_entry = inventory
        .entries
        .iter()
        .find(|entry| entry.name == harness.key())
        .filter(|entry| entry.installed)
        .ok_or_else(|| anyhow!("harness `{harness}` is allowed but not installed on PATH"))?;

    let policy = config.policy(harness.key()).cloned().unwrap_or_default();
    let detected = Detected {
        harness,
        path: detected_entry
            .path
            .clone()
            .expect("installed harness has a path"),
        raw_version: detected_entry.raw_version.clone(),
        version: detected_entry.version.clone(),
    };
    let (detected, context, version_repaired) =
        match ensure_compatible_version(harness, detected, &policy, interactive)
            .context("checking installed harness version")
        {
            Ok(result) => result,
            Err(error) => {
                report_inventory_failure(
                    &cfg,
                    &session,
                    &inventory,
                    harness,
                    &config.revision,
                    &error,
                );
                return Err(error);
            }
        };
    update_inventory_after_version_check(&mut inventory, &detected, &context);
    let detected_path = detected.path;
    let stage_started = std::time::Instant::now();
    let selected_current = status_state.as_ref().is_some_and(|state| {
        selected_applied_state_is_current_after_version_check(
            state,
            &config,
            harness,
            &detected_path,
            config.gateway.is_some() && !cfg.mode.force_governance_only,
            version_repaired,
        )
    });
    tracing::debug!(
        elapsed_ms = stage_started.elapsed().as_millis(),
        harness = %harness,
        current = selected_current,
        "selected managed state checked"
    );
    let (write, reconciled_for_launch) = {
        let mut opts = write_options(&cfg);
        opts.session_upload_enabled = config.session_upload.is_some();
        let fast_spec = selected_current.then(|| {
                let stage_started = std::time::Instant::now();
                let result = gh_config::resolve_launch_spec(
                    &context,
                    &policy,
                    &config.packages,
                    config.gateway.as_ref(),
                    opts,
                );
                if result.is_ok() {
                    tracing::debug!(elapsed_ms = stage_started.elapsed().as_millis(), harness = %harness, "read-only launch specification resolved");
                }
                result
            });
        match fast_spec.transpose() {
            Ok(Some(spec)) => (
                gh_config::HarnessWrite {
                    env: spec.env,
                    launch_args: spec.launch_args,
                    ..Default::default()
                },
                false,
            ),
            Ok(None) => {
                tracing::debug!(harness = %harness, "selected managed state requires reconciliation");
                if !opts.allow_existing_merge {
                    let mut selected = config.clone();
                    selected.allowed_harnesses = vec![harness.key().to_owned()];
                    confirm_existing_merges(&selected, opts, false)?;
                    opts.allow_existing_merge = true;
                }
                let fetcher = AuthenticatedPackageFetcher {
                    client: &client,
                    session: &session,
                };
                let stage_started = std::time::Instant::now();
                let write = match write_harness_with_package_fetcher(
                    &context,
                    &policy,
                    &config.packages,
                    config.gateway.as_ref(),
                    opts,
                    &fetcher,
                )
                .context("writing managed config")
                {
                    Ok(write) => write,
                    Err(error) => {
                        report_inventory_failure(
                            &cfg,
                            &session,
                            &inventory,
                            harness,
                            &config.revision,
                            &error,
                        );
                        return Err(error);
                    }
                };
                tracing::debug!(elapsed_ms = stage_started.elapsed().as_millis(), harness = %harness, "selected harness configuration reconciled");
                (write, true)
            }
            Err(fast_path_error) => {
                tracing::debug!(harness = %harness, error = %fast_path_error, "launch fast path requires selected-harness reconciliation");
                if !opts.allow_existing_merge {
                    let mut selected = config.clone();
                    selected.allowed_harnesses = vec![harness.key().to_owned()];
                    confirm_existing_merges(&selected, opts, false)?;
                    opts.allow_existing_merge = true;
                }
                let fetcher = AuthenticatedPackageFetcher {
                    client: &client,
                    session: &session,
                };
                let stage_started = std::time::Instant::now();
                let write = match write_harness_with_package_fetcher(
                    &context,
                    &policy,
                    &config.packages,
                    config.gateway.as_ref(),
                    opts,
                    &fetcher,
                )
                .context("writing managed config")
                {
                    Ok(write) => write,
                    Err(error) => {
                        report_inventory_failure(
                            &cfg,
                            &session,
                            &inventory,
                            harness,
                            &config.revision,
                            &error,
                        );
                        return Err(error);
                    }
                };
                tracing::debug!(elapsed_ms = stage_started.elapsed().as_millis(), harness = %harness, "selected harness configuration reconciled");
                (write, true)
            }
        }
    };
    if !write.package_errors.is_empty() {
        let error = anyhow!(
            "managed package activation failed: {}",
            write.package_errors.join("; ")
        );
        report_inventory_failure(
            &cfg,
            &session,
            &inventory,
            harness,
            &config.revision,
            &error,
        );
        return Err(error);
    }
    if reconciled_for_launch {
        let state = status_state.get_or_insert_with(|| AppliedState {
            schema_version: APPLIED_STATE_SCHEMA_VERSION,
            revision: config.revision.clone(),
            applied_at: now_unix(),
            files: Vec::new(),
            harnesses: Vec::new(),
            file_sha256: BTreeMap::new(),
            harness_inventory: Vec::new(),
            files_by_harness: BTreeMap::new(),
            binary_fingerprints: BTreeMap::new(),
            blue_binary_fingerprint: None,
            managed_fingerprints: BTreeMap::new(),
            gateway_enabled: false,
        });
        refresh_selected_applied_state(
            state,
            &config.revision,
            harness,
            &write,
            &inventory,
            config.gateway.is_some() && !cfg.mode.force_governance_only,
        )?;
        if let Some(startup) = startup.as_mut() {
            startup.policy(&config.revision, true)?;
        }
    }
    for warning in &write.warnings {
        tracing::warn!(harness = %harness, %warning, "managed configuration warning");
    }
    if reconciled_for_launch {
        tracing::info!(
            harness = %harness,
            files = write.files.len(),
            gateway = config.gateway.is_some() && !cfg.mode.force_governance_only,
            "governed config written"
        );
    }
    if let Some(state) = status_state.as_ref() {
        inventory.mark_reconciled(state.harnesses.clone());
    }
    let stage_started = std::time::Instant::now();
    report_status_async(
        cfg.clone(),
        session.clone(),
        status_state,
        inventory.entries.clone(),
        harness,
        None,
    );
    tracing::debug!(
        elapsed_ms = stage_started.elapsed().as_millis(),
        "client status report dispatched"
    );
    tracing::debug!(elapsed_ms = startup_started.elapsed().as_millis(), harness = %harness, "coding agent ready to launch");

    let launch_args = write
        .launch_args
        .iter()
        .cloned()
        .chain(args.iter().cloned())
        .collect::<Vec<_>>();
    // Mint this blue run's session identity and inject it into the agent process
    // env only. It must NEVER enter the gh-config plan env (goldens/fingerprints
    // stay deterministic); the managed session-start hook reads it back to map
    // the blue uuid to the harness's native session id.
    let blue_session_id = uuid::Uuid::new_v4().to_string();
    let session_upload_enabled = config.session_upload.is_some();
    let mut launch_env = write.env.clone();
    launch_env.insert("BLUE_SESSION_ID".into(), blue_session_id.clone());
    let notices = start_revision_watcher(&cfg, session.clone(), config.revision.clone(), harness);
    let pending_notice = notices.as_ref().map(|notices| notices.revision.clone());
    // Read the deadline off the token the child is actually being handed.
    let gateway_token_expires_at = config
        .gateway
        .as_ref()
        .and_then(|gateway| gateway.token.as_deref())
        .and_then(jwt_expires_at);
    let code = if interactive {
        if let Some(startup) = startup.as_mut() {
            startup.ready()?;
        }
        let gateway_state = if config.gateway.is_some() && !cfg.mode.force_governance_only {
            "managed"
        } else {
            "direct"
        };
        match crate::supervisor::supervise(
            &detected_path,
            &launch_args,
            &launch_env,
            harness.key(),
            "current",
            gateway_state,
            crate::supervisor::SupervisorRuntime {
                connectivity_health_url: cfg
                    .has_http_service()
                    .then(|| format!("{}/health", cfg.service.url.trim_end_matches('/'))),
                revision_notice: pending_notice.clone(),
                auth_notice: notices.as_ref().map(|notices| notices.auth.clone()),
                gateway_token_expires_at,
                gateway_available: config.gateway.is_some(),
                direct_mode: cfg.mode.force_governance_only,
            },
        )
        .context("supervising harness")?
        {
            crate::supervisor::SupervisorExit::Child(code) => code,
            // Restart/Switch/Resume recurse into run() before the exit point
            // below, so flush this blue session here. The recursed run mints a
            // fresh uuid (restart = a new blue session), which is correct.
            crate::supervisor::SupervisorExit::Restart => {
                finalize_blue_session(&blue_session_id, session_upload_enabled);
                return run(name, args);
            }
            crate::supervisor::SupervisorExit::Switch(selected) => {
                finalize_blue_session(&blue_session_id, session_upload_enabled);
                return run(&selected, &[]);
            }
            crate::supervisor::SupervisorExit::Resume(selected) => {
                finalize_blue_session(&blue_session_id, session_upload_enabled);
                if let Some(cwd) = selected.cwd {
                    std::env::set_current_dir(&cwd).with_context(|| {
                        format!("changing to resume directory {}", cwd.display())
                    })?;
                }
                return run(&selected.harness, &selected.args);
            }
        }
    } else {
        gh_harness::launch(&detected_path, &launch_args, &launch_env)
            .context("launching harness")?
    };
    if let Some(message) =
        pending_notice.and_then(|pending| pending.lock().ok().and_then(|message| message.clone()))
    {
        eprintln!("\n{message}");
    }
    // Post-exit fallback: upload the transcript for versions with no SessionEnd
    // hook (codex <0.145) and opencode (no exit event), if it isn't already up.
    finalize_blue_session(&blue_session_id, session_upload_enabled);
    std::process::exit(code);
}

/// Reads `exp` out of a JWT without verifying it. The CLI is not the audience
/// and holds no verification key — it only needs to know when the token it was
/// handed stops working, and a wrong answer costs a spurious warning, not
/// access. Deliberately avoids pulling `jsonwebtoken` into the client.
fn jwt_expires_at(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let mut decoded = String::new();
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut buffer = 0u32;
    let mut bits = 0u32;
    let mut bytes = Vec::new();
    for byte in payload.bytes() {
        if byte == b'=' {
            break;
        }
        let value = alphabet.iter().position(|candidate| *candidate == byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((buffer >> bits) as u8);
        }
    }
    decoded.push_str(std::str::from_utf8(&bytes).ok()?);
    #[derive(serde::Deserialize)]
    struct Claims {
        exp: Option<i64>,
    }
    serde_json::from_str::<Claims>(&decoded).ok()?.exp
}

/// Notices the background watcher can raise for the supervisor: a new policy
/// revision, and a session the control service has stopped accepting.
#[derive(Clone, Default)]
pub(crate) struct WatcherNotices {
    pub(crate) revision: Arc<Mutex<Option<String>>>,
    pub(crate) auth: Arc<Mutex<Option<String>>>,
}

fn start_revision_watcher(
    cfg: &BlueToml,
    mut session: Session,
    active_revision: String,
    harness: Harness,
) -> Option<WatcherNotices> {
    if !cfg.has_http_service() {
        return None;
    }
    tracing::info!(
        harness = %harness,
        revision = %active_revision,
        "revision watcher active; updates will trigger a desktop notification"
    );
    let cfg = cfg.clone();
    // The watcher thread refreshes on its own for hours and `poll_for_revisions`
    // never sees a `BlueToml`, so the deployment context has to be attached
    // here, before the session is moved in.
    session.adopt_refresh_context(&cfg.service.url, identity_scopes(&cfg));
    let notices = WatcherNotices::default();
    let pending_for_thread = notices.revision.clone();
    let auth_for_thread = notices.auth.clone();
    std::thread::spawn(move || {
        let client = match ServiceClient::from_config(&cfg) {
            Ok(client) => client,
            Err(error) => {
                tracing::debug!(%error, "revision watcher could not start");
                return;
            }
        };
        let mut seen = BTreeSet::new();
        let mut reconnect_seconds = 1u64;
        loop {
            if let Err(error) = session.refresh_if_needed(now_unix()) {
                tracing::debug!(%error, "revision watcher could not refresh login");
                record_auth_notice(&error, &auth_for_thread);
                std::thread::sleep(std::time::Duration::from_secs(60));
                continue;
            }

            let stream = client.stream_revisions(&session, |revision| {
                handle_revision_notice(
                    &active_revision,
                    harness,
                    revision,
                    &mut seen,
                    &pending_for_thread,
                );
            });
            if matches!(stream, Ok(RevisionStreamEnd::Unsupported)) {
                poll_for_revisions(
                    &client,
                    &mut session,
                    &active_revision,
                    harness,
                    &mut seen,
                    &pending_for_thread,
                    &auth_for_thread,
                );
                return;
            }
            if let Err(error) = stream {
                tracing::debug!(%error, "revision event stream unavailable; retrying");
            }

            // A disconnected stream may have missed an event. Refresh through
            // the authoritative endpoint before reconnecting.
            // The foreground launch already made the explicit fail-soft
            // decision. Background invalidation checks must be live-only so a
            // transient outage does not repeatedly emit cache-fallback WARNs
            // or treat cached data as a newly observed server revision.
            // This fetch is already live-only (no cache fallback), which makes
            // it the one place a dead session reliably surfaces: the OAuth
            // refresh above still succeeds after the browser session is gone.
            match client.fetch(&session, now_unix()) {
                Ok(config) => handle_revision_notice(
                    &active_revision,
                    harness,
                    config.revision,
                    &mut seen,
                    &pending_for_thread,
                ),
                Err(error) => record_auth_notice(&error, &auth_for_thread),
            }
            std::thread::sleep(std::time::Duration::from_secs(reconnect_seconds));
            reconnect_seconds = (reconnect_seconds * 2).min(60);
        }
    });
    Some(notices)
}

#[allow(clippy::too_many_arguments)]
fn poll_for_revisions(
    client: &ServiceClient,
    session: &mut Session,
    active_revision: &str,
    harness: Harness,
    seen: &mut BTreeSet<String>,
    pending: &Arc<Mutex<Option<String>>>,
    auth: &Arc<Mutex<Option<String>>>,
) {
    loop {
        match session.refresh_if_needed(now_unix()) {
            Ok(()) => match client.fetch(session, now_unix()) {
                Ok(config) => {
                    handle_revision_notice(active_revision, harness, config.revision, seen, pending)
                }
                Err(error) => record_auth_notice(&error, auth),
            },
            Err(error) => record_auth_notice(&error, auth),
        }
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

/// Records a notice for errors only the user can clear, and clears it when the
/// service starts answering again.
fn record_auth_notice(error: &gh_common::GhError, auth: &Arc<Mutex<Option<String>>>) {
    let message = match error {
        gh_common::GhError::Unauthorized(message) | gh_common::GhError::ActionRequired(message) => {
            message.clone()
        }
        _ => return,
    };
    if let Ok(mut auth) = auth.lock() {
        if auth.as_deref() != Some(message.as_str()) {
            tracing::info!(%message, "control service needs the user to act");
            *auth = Some(message);
        }
    }
}

fn handle_revision_notice(
    active_revision: &str,
    harness: Harness,
    revision: String,
    seen: &mut BTreeSet<String>,
    pending: &Arc<Mutex<Option<String>>>,
) {
    let received_revision = revision.clone();
    let Some(message) = revision_notice_message(active_revision, harness, revision, seen) else {
        return;
    };
    tracing::info!(revision = %received_revision, harness = %harness, "new governance revision received");
    let delivered = record_and_send_revision_notice(message, pending, send_desktop_notification);
    tracing::debug!(delivered, "desktop revision notification command completed");
    if !delivered {
        eprint!("\x07");
    }
}

fn record_and_send_revision_notice(
    message: String,
    pending: &Arc<Mutex<Option<String>>>,
    notify: impl FnOnce(&str) -> bool,
) -> bool {
    if let Ok(mut pending) = pending.lock() {
        *pending = Some(message.clone());
    }
    notify(&message)
}

fn revision_notice_message(
    active_revision: &str,
    _harness: Harness,
    revision: String,
    seen: &mut BTreeSet<String>,
) -> Option<String> {
    if revision == active_revision || !seen.insert(revision.clone()) {
        return None;
    }
    Some("New Blue policy available".to_owned())
}

fn send_desktop_notification(message: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("/usr/bin/osascript")
            .args([
                "-e",
                "on run argv",
                "-e",
                "display notification (item 1 of argv) with title (item 2 of argv)",
                "-e",
                "end run",
                "--",
                message,
                "Blue",
            ])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("notify-send")
            .args(["Blue", message])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = message;
        false
    }
}

/// Shorthand dispatch: `blue codex …` → `run("codex", …)`.
pub fn run_external(argv: &[String]) -> Result<()> {
    let (name, rest) = argv
        .split_first()
        .ok_or_else(|| anyhow!("no harness specified"))?;
    run(name, rest)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum MergeChoice {
    Review,
    Approve,
    Cancel,
}

fn current_merge_values(
    implementation: &dyn gh_config::implementations::HarnessImplementation,
    policy: &gh_service::HarnessPolicy,
    files: &[PathBuf],
) -> BTreeMap<String, String> {
    implementation.inspect(policy, files)
}

fn proposed_merge_values(
    implementation: &dyn gh_config::implementations::HarnessImplementation,
    policy: &gh_service::HarnessPolicy,
    gateway: Option<&gh_service::GatewayConfig>,
    opts: WriteOptions,
    values: BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    implementation.proposed_values(policy, gateway, opts, values)
}

fn values_text(values: &BTreeMap<String, String>) -> String {
    values
        .iter()
        .map(|(key, value)| format!("{key} = {value}\n"))
        .collect()
}

#[derive(Default, Deserialize, Serialize)]
struct MergeApprovals {
    #[serde(default)]
    approvals: BTreeMap<String, String>,
}

fn merge_approvals_path() -> Result<PathBuf> {
    Ok(gh_common::paths::blue_config_dir()?.join("merge-approvals.json"))
}

fn load_merge_approvals() -> Result<MergeApprovals> {
    let path = merge_approvals_path()?;
    match std::fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("decoding {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(MergeApprovals::default()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn save_merge_approvals(approvals: &MergeApprovals) -> Result<()> {
    gh_common::write_atomic(
        &merge_approvals_path()?,
        serde_json::to_vec_pretty(approvals)?,
    )?;
    Ok(())
}

fn merge_approval_digest(
    harness: Harness,
    files: &[PathBuf],
    current: &BTreeMap<String, String>,
    proposed: &BTreeMap<String, String>,
) -> String {
    let mut digest = Sha256::new();
    digest.update(harness.key().as_bytes());
    digest.update([0]);
    for path in files {
        digest.update(path.to_string_lossy().as_bytes());
        digest.update([0]);
    }
    digest.update(values_text(current).as_bytes());
    digest.update([0]);
    digest.update(values_text(proposed).as_bytes());
    hex::encode(digest.finalize())
}

fn unapproved_merge_digest(
    approvals: &MergeApprovals,
    harness: Harness,
    files: &[PathBuf],
    current: &BTreeMap<String, String>,
    proposed: &BTreeMap<String, String>,
) -> Option<String> {
    if current == proposed {
        return None;
    }
    let digest = merge_approval_digest(harness, files, current, proposed);
    (approvals.approvals.get(harness.key()) != Some(&digest)).then_some(digest)
}

fn colored_diff(before: &str, after: &str) -> String {
    let diff = TextDiff::from_lines(before, after);
    let mut output = String::new();
    for change in diff.iter_all_changes() {
        let (sign, rendered) = match change.tag() {
            ChangeTag::Delete => ("-", console::style(change.to_string()).red()),
            ChangeTag::Insert => ("+", console::style(change.to_string()).green()),
            ChangeTag::Equal => (" ", console::style(change.to_string()).dim()),
        };
        let _ = write!(output, "{sign} {rendered}");
    }
    output.trim_end().to_string()
}

fn confirm_existing_merges(
    config: &GovernanceConfig,
    opts: WriteOptions,
    assume_yes: bool,
) -> Result<bool> {
    let empty = gh_service::HarnessPolicy::default();
    let mut approvals = load_merge_approvals()?;
    let applied_files = load_applied_state()?
        .map(|state| state.files)
        .unwrap_or_default();
    let mut prompts = Vec::new();
    for name in &config.allowed_harnesses {
        let Ok(harness) = name.parse::<Harness>() else {
            continue;
        };
        let policy = config.policy(name).unwrap_or(&empty);
        let files = gh_config::existing_config_files(harness, policy, opts)?;
        if !files.is_empty() {
            let mut inspection_files = files.clone();
            inspection_files.extend(applied_files.iter().cloned());
            let implementation =
                gh_config::implementations::inspection_implementation(harness)?.implementation;
            let current = current_merge_values(implementation, policy, &inspection_files);
            let proposed = proposed_merge_values(
                gh_config::implementations::detected_context(harness, policy)?
                    .profile
                    .implementation,
                policy,
                config.gateway.as_ref(),
                opts,
                current.clone(),
            );
            if let Some(digest) =
                unapproved_merge_digest(&approvals, harness, &files, &current, &proposed)
            {
                prompts.push((harness, files, current, proposed, digest));
            }
        }
    }
    if prompts.is_empty() {
        return Ok(false);
    }
    if assume_yes {
        for (harness, _, _, _, digest) in prompts {
            approvals.approvals.insert(harness.key().to_owned(), digest);
        }
        save_merge_approvals(&approvals)?;
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        bail!(
            "existing harness configuration requires interactive merge approval; run in a terminal or pass --yes"
        );
    }

    cliclack::intro(console::style(" Blue — configuration merge ").cyan().bold())?;
    cliclack::log::info(
        "Unrelated settings are preserved; changed files receive timestamped backups.",
    )?;
    let mut approved = Vec::new();
    for (harness, files, current, proposed, digest) in prompts {
        let paths = files
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        cliclack::note(format!("{} existing files", harness.key()), paths)?;
        let diff = colored_diff(&values_text(&current), &values_text(&proposed));

        loop {
            let choice = cliclack::select(format!("Review {} merge", harness.key()))
                .initial_value(MergeChoice::Review)
                .item(
                    MergeChoice::Review,
                    "View managed-value diff",
                    "recommended first",
                )
                .item(
                    MergeChoice::Approve,
                    "Approve semantic merge",
                    "backup before writing",
                )
                .item(MergeChoice::Cancel, "Cancel apply", "no files will change")
                .interact()?;
            match choice {
                MergeChoice::Review => {
                    cliclack::note(format!("{} proposed changes", harness.key()), &diff)?;
                }
                MergeChoice::Approve => {
                    cliclack::log::success(format!("{} merge approved", harness.key()))?;
                    approved.push((harness, digest));
                    break;
                }
                MergeChoice::Cancel => {
                    cliclack::outro_cancel("Apply cancelled; no files were changed.")?;
                    bail!("apply cancelled before any files were changed");
                }
            }
        }
    }
    let confirmed = cliclack::confirm("Apply all approved merges?")
        .initial_value(false)
        .interact()?;
    if !confirmed {
        cliclack::outro_cancel("Apply cancelled; no files were changed.")?;
        bail!("apply cancelled before any files were changed");
    }
    for (harness, digest) in approved {
        approvals.approvals.insert(harness.key().to_owned(), digest);
    }
    save_merge_approvals(&approvals)?;
    cliclack::outro("Merge plan approved. Applying configuration…")?;
    Ok(true)
}

pub fn apply(assume_yes: bool) -> Result<()> {
    apply_internal(assume_yes, false, None)
}

pub(crate) fn apply_text() -> Result<String> {
    apply_internal(true, true, None)?;
    Ok(
        "Configuration applied. The running agent may require a restart for changed settings."
            .into(),
    )
}

pub(crate) fn set_direct_mode(enabled: bool, active_harness: Option<Harness>) -> Result<()> {
    let mut cfg = BlueToml::load().context("loading blue.toml")?;
    let previous = cfg.mode.force_governance_only;
    if previous == enabled {
        return Ok(());
    }
    cfg.mode.force_governance_only = enabled;
    cfg.save().context("saving direct-mode preference")?;
    if let Some(active_harness) = active_harness {
        if let Err(error) = apply_internal(true, true, Some(active_harness)) {
            cfg.mode.force_governance_only = previous;
            cfg.save()
                .context("restoring gateway-mode preference after reconciliation failed")?;
            return Err(error.context("reconciling the selected inference mode"));
        }
    }
    Ok(())
}

fn apply_internal(assume_yes: bool, quiet: bool, target_override: Option<Harness>) -> Result<()> {
    let (cfg, client) = load_client()?;
    let session = session_for(&cfg)?;
    ensure_gateway_access(&cfg, &session)?;
    let config = client
        .fetch_or_cached(&session, now_unix())
        .context("fetching governance config")?;
    let applied = load_applied_state()?;
    let mut inventory = discover_inventory_cached(&config.allowed_harnesses, applied.as_ref());
    gh_agent::evaluate_inventory(&config, &mut inventory);
    let target = target_override
        .map(Ok)
        .unwrap_or_else(|| configured_default_harness(&cfg, &inventory))?;
    apply_loaded(
        &cfg, &client, &session, &config, inventory, target, assume_yes, quiet, true,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_loaded(
    cfg: &BlueToml,
    client: &ServiceClient,
    session: &Session,
    config: &GovernanceConfig,
    mut inventory: HarnessInventory,
    target: Harness,
    assume_yes: bool,
    quiet: bool,
    report_success: bool,
) -> Result<()> {
    let interactive = std::io::stdin().is_terminal() && !assume_yes;
    let styled = interactive && !quiet;
    if !quiet {
        show_harness_inventory(&inventory, styled)?;
    }
    if let Err(error) =
        repair_incompatible_inventory(config, &mut inventory, interactive, Some(target))
    {
        report_inventory_failure(cfg, session, &inventory, target, &config.revision, &error);
        return Err(error);
    }
    let scoped_inventory = inventory_for_harness(&inventory, target);
    let mut reconcile_config = config.clone();
    reconcile_config.allowed_harnesses = vec![target.key().to_owned()];

    let mut opts = write_options(cfg);
    opts.session_upload_enabled = config.session_upload.is_some();
    confirm_existing_merges(&reconcile_config, opts, assume_yes)?;
    opts.allow_existing_merge = true;
    if styled {
        cliclack::intro(format!(" Applying revision {} ", config.revision))?;
    } else if !quiet {
        println!("Reconciling revision {} …", config.revision);
    }
    let fetcher = AuthenticatedPackageFetcher { client, session };
    let results =
        gh_agent::apply_once_with_inventory_and_fetcher(config, opts, &scoped_inventory, &fetcher);
    let mut failures = Vec::new();
    for r in &results {
        match &r.result {
            Ok(w) => {
                if styled {
                    cliclack::log::success(format!(
                        "{:<9} merged ({} files)",
                        r.harness.key(),
                        w.files.len()
                    ))?;
                } else if !quiet {
                    println!("  {:<9} ok ({} files)", r.harness.key(), w.files.len());
                }
                for warning in &w.warnings {
                    if styled {
                        cliclack::log::warning(format!("{:<9} {warning}", r.harness.key()))?;
                    } else if !quiet {
                        println!("  {:<9} warning: {warning}", r.harness.key());
                    }
                }
                failures.extend(
                    w.package_errors
                        .iter()
                        .map(|error| format!("{} package: {error}", r.harness.key())),
                );
            }
            Err(e) => {
                if styled {
                    cliclack::log::error(format!("{:<9} FAILED: {e}", r.harness.key()))?;
                } else if !quiet {
                    println!("  {:<9} FAILED: {e}", r.harness.key());
                }
                failures.push(format!("{}: {e}", r.harness.key()));
            }
        }
    }
    if failures.is_empty() {
        let mut state = load_applied_state()?.unwrap_or_else(|| AppliedState {
            schema_version: APPLIED_STATE_SCHEMA_VERSION,
            revision: config.revision.clone(),
            applied_at: now_unix(),
            files: Vec::new(),
            harnesses: Vec::new(),
            file_sha256: BTreeMap::new(),
            harness_inventory: Vec::new(),
            files_by_harness: BTreeMap::new(),
            binary_fingerprints: BTreeMap::new(),
            blue_binary_fingerprint: None,
            managed_fingerprints: BTreeMap::new(),
            gateway_enabled: false,
        });
        let write = results
            .iter()
            .find(|result| result.harness == target)
            .and_then(|result| result.result.as_ref().ok())
            .ok_or_else(|| anyhow!("default agent `{target}` was not reconciled"))?;
        refresh_selected_applied_state(
            &mut state,
            &config.revision,
            target,
            write,
            &inventory,
            config.gateway.is_some() && !cfg.mode.force_governance_only,
        )?;
        inventory.mark_reconciled(state.harnesses.clone());
        if report_success {
            report_status(cfg, session, Some(&state), &inventory.entries, true, None);
        }
        if styled {
            cliclack::outro("Configuration merged successfully.")?;
        }
    } else {
        let error = failures.join("; ");
        report_status_with_attempt(
            cfg,
            session,
            None,
            &inventory.entries,
            false,
            Some(&error),
            Some((target, &config.revision)),
        );
        if styled {
            cliclack::outro_cancel("One or more harness merges failed.")?;
        }
        bail!("one or more harness configurations failed to apply");
    }
    Ok(())
}

pub fn daemon(interval: Option<u64>) -> Result<()> {
    let (cfg, client) = load_client()?;
    let session = session_for(&cfg)?;
    ensure_gateway_access(&cfg, &session)?;
    let desired = client
        .fetch_or_cached(&session, now_unix())
        .context("fetching governance config")?;
    let applied = load_applied_state()?;
    let mut initial_inventory =
        discover_inventory_cached(&desired.allowed_harnesses, applied.as_ref());
    gh_agent::evaluate_inventory(&desired, &mut initial_inventory);
    let target = configured_default_harness(&cfg, &initial_inventory)?;
    println!(
        "Starting reconcile daemon for {} (source: {}) — Ctrl-C to stop.",
        target,
        client.describe_source(),
    );
    let sleep = move |ttl: u64| {
        let secs = interval.map(|i| i.min(ttl)).unwrap_or(ttl).max(1);
        std::thread::sleep(std::time::Duration::from_secs(secs));
    };
    let on_reconciled = |config: &GovernanceConfig,
                         inventory: &mut HarnessInventory,
                         results: &[gh_agent::HarnessReconcile],
                         all_succeeded: bool| {
        if all_succeeded {
            let updated = (|| {
                let write = results
                    .iter()
                    .find(|result| result.harness == target)
                    .and_then(|result| result.result.as_ref().ok())
                    .ok_or_else(|| anyhow!("default agent `{target}` was not reconciled"))?;
                let mut state = load_applied_state()?.unwrap_or_else(|| AppliedState {
                    schema_version: APPLIED_STATE_SCHEMA_VERSION,
                    revision: config.revision.clone(),
                    applied_at: now_unix(),
                    files: Vec::new(),
                    harnesses: Vec::new(),
                    file_sha256: BTreeMap::new(),
                    harness_inventory: Vec::new(),
                    files_by_harness: BTreeMap::new(),
                    binary_fingerprints: BTreeMap::new(),
                    blue_binary_fingerprint: None,
                    managed_fingerprints: BTreeMap::new(),
                    gateway_enabled: false,
                });
                refresh_selected_applied_state(
                    &mut state,
                    &config.revision,
                    target,
                    write,
                    inventory,
                    config.gateway.is_some() && !cfg.mode.force_governance_only,
                )?;
                inventory.mark_reconciled(state.harnesses.clone());
                Ok::<_, anyhow::Error>(state)
            })();
            match updated {
                Ok(state) => {
                    report_status(&cfg, &session, Some(&state), &inventory.entries, true, None);
                }
                Err(error) => {
                    tracing::error!(revision = %config.revision, %error, "persisting daemon reconciliation failed");
                    let message = format!("persisting applied state: {error}");
                    report_status_with_attempt(
                        &cfg,
                        &session,
                        None,
                        &inventory.entries,
                        false,
                        Some(&message),
                        Some((target, &config.revision)),
                    );
                }
            }
        } else {
            let scoped_inventory = inventory_for_harness(inventory, target);
            let error = daemon_reconcile_errors(&scoped_inventory, results);
            report_status_with_attempt(
                &cfg,
                &session,
                None,
                &inventory.entries,
                false,
                Some(&error),
                Some((target, &config.revision)),
            );
        }
    };
    let on_unauthorized = |message: &str| {
        eprintln!("blue: {message}");
        report_status_with_attempt(
            &cfg,
            &session,
            None,
            &initial_inventory.entries,
            false,
            Some(message),
            None,
        );
    };
    gh_agent::reconcile_loop(
        &client,
        &session,
        target,
        write_options(&cfg),
        now_unix,
        sleep,
        on_reconciled,
        on_unauthorized,
    );
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionSpoolRecord {
    harness: String,
    compatibility_profile: String,
    session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    sha256: String,
    size_bytes: usize,
    content_type: String,
    #[serde(default = "legacy_raw_artifact_format")]
    artifact_format: String,
    #[serde(default)]
    resumable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(default)]
    captured_at_unix_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repository: Option<gh_config::session_bundle::RepositoryIdentity>,
    #[serde(alias = "transcript_path")]
    artifact_path: PathBuf,
    /// The `BLUE_SESSION_ID` that produced this spool record, when known. Absent
    /// for hooks in env-scrubbing CLIs; durable upload state is still keyed by
    /// the native session so uploads and dedup remain correct without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    blue_session_id: Option<String>,
}

fn legacy_raw_artifact_format() -> String {
    "legacy-raw".into()
}

/// Durable record that a native session's transcript was uploaded, keyed by the
/// spool identity (`sha256(harness\0profile\0session_id)`). One native session
/// maps to many `BLUE_SESSION_ID`s across resumes, so this is keyed by the
/// native session — never the blue uuid — and `sha256` is the dedup key: an
/// unchanged transcript is skipped, a grown one is uploaded again.
#[derive(Debug, Serialize, Deserialize)]
struct SessionUploadState {
    schema_version: u32,
    harness: String,
    compatibility_profile: String,
    session_id: String,
    sha256: String,
    size_bytes: usize,
    uploaded_at_unix: i64,
    #[serde(default)]
    blue_session_ids: Vec<String>,
}

/// Terminal disposition of an upload attempt. Only `Completed` should stamp
/// durable upload state; a policy-disabled skip must not record a phantom
/// upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UploadOutcome {
    Completed,
    PolicyDisabled,
}

fn session_upload_state_dir() -> Result<PathBuf> {
    Ok(gh_common::paths::blue_config_dir()?.join("session-upload-state"))
}

/// Stamp durable upload state after a successful upload. Read-merges so every
/// `BLUE_SESSION_ID` that resumed the same native session shares one record;
/// refreshes the sha/size/timestamp to the just-uploaded transcript. The spool
/// key is the record's filename stem — never recomputed — so it always matches
/// the spool blob it was uploaded from.
fn stamp_upload_state(spool_key: &str, record: &SessionSpoolRecord) -> Result<()> {
    stamp_upload_state_in(&session_upload_state_dir()?, spool_key, record)
}

fn stamp_upload_state_in(dir: &Path, spool_key: &str, record: &SessionSpoolRecord) -> Result<()> {
    let path = dir.join(format!("{spool_key}.json"));
    let mut blue_session_ids = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<SessionUploadState>(&bytes).ok())
        .map(|existing| existing.blue_session_ids)
        .unwrap_or_default();
    if let Some(id) = &record.blue_session_id {
        if !blue_session_ids.contains(id) {
            blue_session_ids.push(id.clone());
        }
    }
    let state = SessionUploadState {
        schema_version: 1,
        harness: record.harness.clone(),
        compatibility_profile: record.compatibility_profile.clone(),
        session_id: record.session_id.clone(),
        sha256: record.sha256.clone(),
        size_bytes: record.size_bytes,
        uploaded_at_unix: now_unix(),
        blue_session_ids,
    };
    gh_common::write_atomic(&path, serde_json::to_vec_pretty(&state)?)?;
    Ok(())
}

fn session_spool_dir() -> Result<PathBuf> {
    Ok(gh_common::paths::blue_config_dir()?.join("session-upload-spool"))
}

/// Stable spool identity for a native session. Shared by `spool_session` and
/// the post-exit fallback so a record and its durable upload-state file always
/// agree on their key.
fn spool_key(harness: Harness, profile: &str, session_id: &str) -> String {
    hex::encode(Sha256::digest(format!(
        "{}\0{}\0{}",
        harness.key(),
        profile,
        session_id
    )))
}

/// Best-effort post-exit upload for the blue session that just finished. Never
/// errors and never alters the exit code; failures only log. This is what makes
/// upload work for versions with no SessionEnd hook (codex before 0.145 and the
/// claude hooks-only band) and for opencode, which has no exit event.
fn finalize_blue_session(blue_session_id: &str, session_upload_enabled: bool) {
    if !session_upload_enabled {
        return;
    }
    if let Err(error) = finalize_blue_session_inner(blue_session_id) {
        tracing::warn!(%error, "post-exit session upload fallback failed");
    }
}

fn finalize_blue_session_inner(blue_session_id: &str) -> Result<()> {
    let metadata_path = session_metadata_path(&blue_sessions_dir()?, blue_session_id);
    let Some(metadata) = std::fs::read(&metadata_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<BlueSessionMetadata>(&bytes).ok())
    else {
        // No session-start capability, or the hook never fired (e.g. codex
        // exited before its first turn). Clean no-op.
        return Ok(());
    };
    let harness: Harness = metadata.harness.parse()?;
    let profile = metadata.compatibility_profile.as_str();
    let session_id = metadata.coding_agent_session_id.as_str();

    // Synthesize a hook-style payload from the metadata so each harness routes
    // through its native capture path (kimi via its filesystem resolver,
    // opencode via its stable plugin export).
    let payload = serde_json::json!({
        "session_id": session_id,
        "transcript_path": metadata.transcript_path,
        "cwd": metadata.cwd,
    });
    // Capture and spool the session bundle, then upload ONLY the just-spooled
    // record. A full spool drain would bound exit latency by unrelated stale
    // records; those are still retried by future hook-triggered workers. The
    // upload dedups server-side by content hash, and the stamp unions this blue
    // uuid into the native session's durable upload state, so a session the
    // SessionEnd/idle hook already uploaded is not re-sent.
    let record_path = match spool_session(
        harness,
        profile,
        session_id,
        &payload,
        Some(blue_session_id),
    ) {
        Ok(path) => path,
        Err(error) => {
            // Best-effort: the agent may have produced no capturable session
            // (e.g. it exited before its first turn). A later hook-triggered
            // worker still uploads if a transcript appears.
            tracing::debug!(%error, "post-exit fallback found no capturable session");
            return Ok(());
        }
    };
    let mut failures = Vec::new();
    process_spooled_record(&record_path, &mut failures);
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("post-exit fallback upload failed: {}", failures.join("; "))
    }
}

fn spool_session(
    harness: Harness,
    profile: &str,
    session_id: &str,
    payload: &serde_json::Value,
    blue_session_id: Option<&str>,
) -> Result<PathBuf> {
    let home = gh_common::paths::home_dir()?;
    let definition = gh_config::implementations::definition(harness);
    let implementation = gh_config::implementations::implementation_for_profile(harness, profile)
        .ok_or_else(|| anyhow!("unknown compatibility profile `{profile}` for {harness}"))?
        .implementation;
    let mut sources = implementation.capture_session(definition, &home, session_id, payload)?;
    let source = sources
        .first()
        .ok_or_else(|| anyhow!("session capture operation returned no artifacts"))?
        .source
        .clone();
    sources[0].role = match harness {
        Harness::Codex => "rollout",
        Harness::Claude => "primary_transcript",
        Harness::Kimi => "agent_wire_history",
        Harness::Opencode => "portable_export",
    }
    .into();
    // OpenCode owns its storage layout. Its portable export is imported by the
    // native CLI and must never be treated as a home-relative file target.
    if harness == Harness::Opencode {
        sources[0].native_path = None;
    }
    let companions = session_bundle_sources(harness, session_id, &source, &home)?;
    for companion in companions.into_iter().skip(1) {
        sources.push(companion);
    }
    let (title, summary) = native_session_display(harness, session_id, payload, &source, &home)?;
    let cwd = payload
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let bundle = gh_config::session_bundle::create_with(
        harness,
        profile,
        session_id,
        cwd.clone(),
        title,
        summary,
        sources,
        |role, bytes| implementation.prepare_session_file(role, bytes),
    )?;
    let bytes = bundle.bytes;
    let sha256 = hex::encode(Sha256::digest(&bytes));
    let key = spool_key(harness, profile, session_id);
    let root = session_spool_dir()?;
    let transcript = root.join(format!("{key}.bundle.tgz"));
    let record_path = root.join(format!("{key}.json"));
    gh_common::write_atomic(&transcript, &bytes)?;
    let record = SessionSpoolRecord {
        harness: harness.key().to_owned(),
        compatibility_profile: profile.to_owned(),
        session_id: session_id.to_owned(),
        cwd,
        sha256,
        size_bytes: bytes.len(),
        content_type: gh_config::session_bundle::CONTENT_TYPE.into(),
        artifact_format: gh_config::session_bundle::ARTIFACT_FORMAT.into(),
        resumable: true,
        title: bundle.manifest.title,
        summary: bundle.manifest.summary,
        captured_at_unix_ms: bundle.manifest.captured_at_unix_ms,
        repository: bundle.manifest.repository,
        artifact_path: transcript,
        blue_session_id: blue_session_id.map(str::to_owned),
    };
    gh_common::write_atomic(&record_path, serde_json::to_vec_pretty(&record)?)?;
    Ok(record_path)
}

fn spawn_session_upload_worker() -> Result<()> {
    let exe = std::env::current_exe().context("resolving session upload worker executable")?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("session-upload-worker")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command
        .spawn()
        .context("starting detached session upload worker")?;
    Ok(())
}

/// Per-`blue run` session identity written by the managed session-start hook.
/// Keyed by the minted `BLUE_SESSION_ID`; records the native session id so the
/// post-exit fallback can resolve and upload the transcript. One native session
/// maps to many `BLUE_SESSION_ID`s across resumes, so durable upload state is
/// tracked separately, keyed by the native session (see `SessionUploadState`).
#[derive(Debug, Serialize, Deserialize)]
struct BlueSessionMetadata {
    schema_version: u32,
    blue_session_id: String,
    harness: String,
    compatibility_profile: String,
    coding_agent_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    created_at_unix: i64,
    updated_at_unix: i64,
}

fn blue_sessions_dir() -> Result<PathBuf> {
    Ok(gh_common::paths::blue_config_dir()?.join("sessions"))
}

fn session_metadata_path(dir: &Path, blue_session_id: &str) -> PathBuf {
    dir.join(blue_session_id).join("metadata.json")
}

/// Receive a native session-start hook payload and record the mapping from the
/// injected `BLUE_SESSION_ID` to the harness's native session id. Errors are
/// returned so hook logs surface them, but an agent launched outside `blue run`
/// (no `BLUE_SESSION_ID`) is a silent no-op.
pub fn session_start(name: &str, profile: Option<&str>) -> Result<()> {
    let harness: Harness = name
        .parse()
        .with_context(|| format!("`{name}` is not a known harness"))?;
    let Some(blue_session_id) = std::env::var("BLUE_SESSION_ID")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    // BLUE_SESSION_ID reaches us through the agent's environment, so validate it
    // is a well-formed UUID before joining it into any filesystem path.
    if uuid::Uuid::parse_str(&blue_session_id).is_err() {
        bail!("BLUE_SESSION_ID is not a well-formed UUID");
    }
    let mut input = Vec::new();
    std::io::stdin()
        .read_to_end(&mut input)
        .context("reading session-start hook payload")?;
    let payload: serde_json::Value =
        serde_json::from_slice(&input).context("decoding session-start hook payload")?;
    let embedded_profile = payload
        .get("profile")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| anyhow!("hook profile must be a string"))
        })
        .transpose()?;
    if profile.is_some() && embedded_profile.is_some() && profile != embedded_profile {
        bail!("hook payload profile disagrees with command profile");
    }
    let profile = profile.or(embedded_profile);
    let registration = match profile {
        Some(profile) => {
            gh_config::implementations::implementation_for_profile(harness, profile)
                .ok_or_else(|| anyhow!("unknown compatibility profile `{profile}` for {harness}"))?
        }
        None => gh_config::implementations::detected_implementation(harness)?,
    };
    let profile = registration.interval.profile;
    record_session_start(
        &blue_sessions_dir()?,
        &blue_session_id,
        harness,
        profile,
        &payload,
    )
}

/// Write (or refresh) `sessions/<uuid>/metadata.json`. Last write wins for the
/// mapped native session id, which handles resumes and the pre-2.1.73 Claude
/// duplicate `SessionStart`; the original `created_at_unix` is preserved.
fn record_session_start(
    dir: &Path,
    blue_session_id: &str,
    harness: Harness,
    profile: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    let session_id = payload
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("session-start hook payload has no session_id"))?;
    let path = session_metadata_path(dir, blue_session_id);
    let now = now_unix();
    let created_at_unix = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<BlueSessionMetadata>(&bytes).ok())
        .map(|existing| existing.created_at_unix)
        .unwrap_or(now);
    let metadata = BlueSessionMetadata {
        schema_version: 1,
        blue_session_id: blue_session_id.to_owned(),
        harness: harness.key().to_owned(),
        compatibility_profile: profile.to_owned(),
        coding_agent_session_id: session_id.to_owned(),
        transcript_path: payload
            .get("transcript_path")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        cwd: payload
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        source: payload
            .get("source")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        created_at_unix,
        updated_at_unix: now,
    };
    gh_common::write_atomic(&path, serde_json::to_vec_pretty(&metadata)?)?;
    Ok(())
}

/// Receive a native lifecycle-hook payload and atomically spool the transcript.
/// Network work happens in a detached worker so agents with short or non-awaited
/// shutdown hooks cannot truncate the upload.
pub fn session_upload(name: &str, profile: Option<&str>) -> Result<()> {
    let harness: Harness = name
        .parse()
        .with_context(|| format!("`{name}` is not a known harness"))?;
    let mut input = Vec::new();
    std::io::stdin()
        .read_to_end(&mut input)
        .context("reading session hook payload")?;
    let payload: serde_json::Value =
        serde_json::from_slice(&input).context("decoding session hook payload")?;
    let embedded_profile = payload
        .get("profile")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| anyhow!("hook profile must be a string"))
        })
        .transpose()?;
    if profile.is_some() && embedded_profile.is_some() && profile != embedded_profile {
        bail!("hook payload profile disagrees with command profile");
    }
    let profile = profile.or(embedded_profile);
    let registration = match profile {
        Some(profile) => {
            gh_config::implementations::implementation_for_profile(harness, profile)
                .ok_or_else(|| anyhow!("unknown compatibility profile `{profile}` for {harness}"))?
        }
        None => gh_config::implementations::detected_implementation(harness)?,
    };
    let profile = registration.interval.profile;

    let session_id = payload
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("hook payload has no session_id"))?;

    // Hooks inherit the agent's environment in all four CLIs, so the injected
    // BLUE_SESSION_ID is available here (absent in env-scrubbing configs).
    let blue_session_id = std::env::var("BLUE_SESSION_ID")
        .ok()
        .filter(|value| !value.is_empty());
    spool_session(
        harness,
        profile,
        session_id,
        &payload,
        blue_session_id.as_deref(),
    )?;
    spawn_session_upload_worker()
}

pub fn session_upload_worker() -> Result<()> {
    let root = session_spool_dir()?;
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", root.display())),
    };
    let mut failures = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        process_spooled_record(&path, &mut failures);
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(
            "session upload worker retained failed spool records: {}",
            failures.join("; ")
        )
    }
}

/// Claim, upload, stamp, and delete a single spool record. Shared by the worker
/// and the post-exit fallback so both take the same lock, retry, and dedup path.
fn process_spooled_record(path: &Path, failures: &mut Vec<String>) {
    // Every lifecycle hook starts a detached worker. Claim each record with
    // create_new so concurrent directory scans cannot both process it or
    // abort after another worker removes it.
    let lock_path = path.with_extension("json.lock");
    let lock = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return,
        Err(error) => {
            failures.push(format!("{}: claiming record: {error}", path.display()));
            return;
        }
    };
    let record = match std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))
        .and_then(|bytes| {
            serde_json::from_slice::<SessionSpoolRecord>(&bytes)
                .with_context(|| format!("decoding {}", path.display()))
        }) {
        Ok(record) => record,
        Err(error) => {
            failures.push(format!("{}: {error:#}", path.display()));
            drop(lock);
            let _ = std::fs::remove_file(&lock_path);
            return;
        }
    };
    let mut result = upload_spooled_session(&record);
    for retry in 1..=2 {
        if result.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250 * retry));
        result = upload_spooled_session(&record);
    }
    match result {
        Ok(outcome) => {
            // Stamp durable upload state BEFORE deleting the spool blob and
            // record. Only a real upload stamps; a policy-disabled skip must
            // not record a phantom upload. A stamp failure still deletes.
            if outcome == UploadOutcome::Completed {
                let spool_key = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or_default();
                if let Err(error) = stamp_upload_state(spool_key, &record) {
                    tracing::warn!(session = %record.session_id, %error, "failed to stamp session upload state");
                }
            }
            let _ = std::fs::remove_file(&record.artifact_path);
            let _ = std::fs::remove_file(path);
        }
        Err(error) => failures.push(format!("{}: {error:#}", record.session_id)),
    }
    drop(lock);
    let _ = std::fs::remove_file(&lock_path);
}

pub struct RestoredSession {
    pub harness: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

fn restore_bundle(
    path: &Path,
    preflight: bool,
    destination: Option<&Path>,
) -> Result<RestoredSession> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading session bundle {}", path.display()))?;
    let bundle = gh_config::session_bundle::verify(&bytes)?;
    restore_verified_bundle(&bundle, preflight, destination)
}

fn restore_verified_bundle(
    bundle: &gh_config::session_bundle::VerifiedBundle,
    preflight: bool,
    destination: Option<&Path>,
) -> Result<RestoredSession> {
    let harness: Harness = bundle.manifest.harness.parse()?;
    let registration = gh_config::implementations::implementation_for_profile(
        harness,
        &bundle.manifest.compatibility_profile,
    )
    .ok_or_else(|| {
        anyhow!(
            "this Blue version does not support session profile `{}`; update Blue before restoring",
            bundle.manifest.compatibility_profile
        )
    })?;
    if matches!(
        registration.implementation.session_resume_capability(),
        gh_config::implementations::Feature::Unsupported(_)
    ) {
        bail!(
            "profile `{}` can be downloaded but does not support native resume",
            bundle.manifest.compatibility_profile
        );
    }
    let home = gh_common::paths::home_dir()?;
    let targets = gh_config::session_bundle::preflight_home_files_with(
        bundle,
        &home,
        |role, existing, bundled| {
            registration
                .implementation
                .session_file_equivalent(role, existing, bundled)
        },
    )?;
    let newly_created = targets
        .iter()
        .filter(|(path, _)| !path.exists())
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    // Parse and validate the current native index before the active agent is
    // stopped. Recompute it immediately before publication to retain sessions
    // another local process may have added in the meantime.
    prepare_native_session_index(harness, bundle, &home)?;
    if harness == Harness::Opencode
        && opencode_session_exists(destination, &bundle.manifest.native_session_id)?
    {
        bail!(
            "native session collision for {}; the local OpenCode session was not changed",
            bundle.manifest.native_session_id
        );
    }
    if !preflight {
        gh_config::session_bundle::restore_home_files_with(
            bundle,
            &home,
            |role, existing, bundled| {
                registration
                    .implementation
                    .session_file_equivalent(role, existing, bundled)
            },
        )?;
        let publish_index =
            prepare_native_session_index(harness, bundle, &home).and_then(|update| {
                if let Some((path, bytes)) = update {
                    gh_common::write_atomic(&path, bytes)?;
                }
                Ok(())
            });
        if let Err(error) = publish_index {
            for restored in newly_created {
                let _ = std::fs::remove_file(restored);
            }
            return Err(error).context("publishing restored native session index");
        }
    }
    let mut native_session_id = bundle.manifest.native_session_id.clone();
    if harness == Harness::Opencode && !preflight {
        let export = bundle
            .manifest
            .files
            .iter()
            .find(|file| file.role == "portable_export")
            .and_then(|file| bundle.files.get(&file.path))
            .ok_or_else(|| anyhow!("OpenCode bundle has no portable export"))?;
        let import_path =
            session_spool_dir()?.join(format!("opencode-import-{}.json", std::process::id()));
        gh_common::write_atomic(&import_path, export)?;
        let mut command = std::process::Command::new("opencode");
        command.arg("import").arg(&import_path);
        if let Some(destination) = destination {
            command.current_dir(destination);
        }
        let output = command.output().context("running opencode import")?;
        let _ = std::fs::remove_file(&import_path);
        if !output.status.success() {
            bail!(
                "opencode import failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        native_session_id = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.trim().strip_prefix("Imported session:"))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("opencode import did not report an imported session ID"))?;
    }
    let args = match harness {
        Harness::Codex => vec!["resume", native_session_id.as_str()],
        Harness::Claude => vec!["--resume", native_session_id.as_str()],
        Harness::Kimi => vec!["--session", native_session_id.as_str()],
        Harness::Opencode => vec!["--session", native_session_id.as_str()],
    };
    let result = RestoredSession {
        harness: harness.key().into(),
        args: args.into_iter().map(str::to_owned).collect(),
        cwd: destination
            .map(Path::to_path_buf)
            .or_else(|| bundle.manifest.cwd.as_deref().map(PathBuf::from)),
    };
    if preflight {
        return Ok(result);
    }
    Ok(result)
}

fn opencode_session_exists(destination: Option<&Path>, session_id: &str) -> Result<bool> {
    let mut command = std::process::Command::new("opencode");
    command.args([
        "session",
        "list",
        "--max-count",
        "2147483647",
        "--format",
        "json",
    ]);
    if let Some(destination) = destination {
        command.current_dir(destination);
    }
    let output = command
        .output()
        .context("checking for a local OpenCode session collision")?;
    if !output.status.success() {
        bail!(
            "could not safely check OpenCode session collisions: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let sessions: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("decoding `opencode session list --format json`")?;
    Ok(sessions.as_array().is_some_and(|sessions| {
        sessions.iter().any(|session| {
            session.get("id").and_then(serde_json::Value::as_str) == Some(session_id)
        })
    }))
}

fn prepare_native_session_index(
    harness: Harness,
    bundle: &gh_config::session_bundle::VerifiedBundle,
    home: &Path,
) -> Result<Option<(PathBuf, Vec<u8>)>> {
    match harness {
        Harness::Kimi => prepare_kimi_session_index(bundle, home).map(Some),
        Harness::Claude => prepare_claude_session_index(bundle, home).map(Some),
        _ => Ok(None),
    }
}

fn prepare_kimi_session_index(
    bundle: &gh_config::session_bundle::VerifiedBundle,
    home: &Path,
) -> Result<(PathBuf, Vec<u8>)> {
    let native = bundle
        .manifest
        .files
        .iter()
        .filter_map(|file| file.native_path.as_deref())
        .find(|path| path.ends_with("state.json"))
        .or_else(|| {
            bundle
                .manifest
                .files
                .iter()
                .filter_map(|file| file.native_path.as_deref())
                .find(|path| path.contains("/sessions/"))
        })
        .ok_or_else(|| anyhow!("Kimi bundle has no native session directory"))?;
    let native = PathBuf::from(native);
    let session_dir = if native.ends_with("state.json") {
        native.parent().unwrap_or(&native)
    } else {
        native
            .ancestors()
            .find(|path| {
                path.file_name().and_then(|value| value.to_str())
                    == Some(&bundle.manifest.native_session_id)
            })
            .unwrap_or(native.parent().unwrap_or(&native))
    };
    let index = if native.starts_with(".config/blue/runtime/kimi") {
        home.join(".config/blue/runtime/kimi/session_index.jsonl")
    } else {
        home.join(".kimi-code/session_index.jsonl")
    };
    gh_config::session_bundle::ensure_safe_home_destination(home, &index)?;
    let existing = match std::fs::read_to_string(&index) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("reading {}", index.display())),
    };
    let already = existing
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .any(|value| {
            value
                .get("session_id")
                .or_else(|| value.get("sessionId"))
                .or_else(|| value.get("id"))
                .and_then(serde_json::Value::as_str)
                == Some(&bundle.manifest.native_session_id)
        });
    if already {
        return Ok((index, existing.into_bytes()));
    }
    let mut body = existing;
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&serde_json::to_string(&serde_json::json!({
        "sessionId": bundle.manifest.native_session_id,
        "sessionDir": home.join(session_dir).to_string_lossy(),
        "workDir": bundle.manifest.cwd,
    }))?);
    body.push('\n');
    Ok((index, body.into_bytes()))
}

fn prepare_claude_session_index(
    bundle: &gh_config::session_bundle::VerifiedBundle,
    home: &Path,
) -> Result<(PathBuf, Vec<u8>)> {
    let transcript = bundle
        .manifest
        .files
        .iter()
        .find(|file| file.role == "primary_transcript")
        .and_then(|file| file.native_path.as_deref())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("Claude bundle has no native primary transcript path"))?;
    let Some(project_dir) = transcript.parent() else {
        bail!("Claude primary transcript has no project directory");
    };
    let index = home.join(project_dir).join("sessions-index.json");
    gh_config::session_bundle::ensure_safe_home_destination(home, &index)?;
    let mut document = match std::fs::read(&index) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .context("decoding Claude sessions-index.json")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            serde_json::json!({"version":1,"entries":[]})
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", index.display())),
    };
    let entries = if let Some(entries) = document.as_array_mut() {
        entries
    } else {
        document
            .as_object_mut()
            .and_then(|object| {
                if object.contains_key("entries") {
                    object.get_mut("entries")
                } else {
                    object.get_mut("sessions")
                }
            })
            .and_then(serde_json::Value::as_array_mut)
            .ok_or_else(|| anyhow!("unsupported Claude sessions-index.json shape"))?
    };
    if entries.iter().any(|entry| {
        entry
            .get("sessionId")
            .or_else(|| entry.get("session_id"))
            .and_then(serde_json::Value::as_str)
            == Some(&bundle.manifest.native_session_id)
    }) {
        return Ok((index, serde_json::to_vec_pretty(&document)?));
    }
    let captured_nanos = i128::try_from(bundle.manifest.captured_at_unix_ms)
        .ok()
        .and_then(|value| value.checked_mul(1_000_000))
        .ok_or_else(|| anyhow!("captured session timestamp is outside the supported range"))?;
    let captured = time::OffsetDateTime::from_unix_timestamp_nanos(captured_nanos)
        .context("decoding captured session timestamp")?
        .format(&time::format_description::well_known::Rfc3339)
        .context("formatting captured session timestamp")?;
    let message_count = bundle
        .manifest
        .files
        .iter()
        .find(|file| file.role == "primary_transcript")
        .and_then(|file| bundle.files.get(&file.path))
        .map(|bytes| {
            bytes
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .count()
        })
        .unwrap_or_default();
    entries.push(serde_json::json!({
        "sessionId": bundle.manifest.native_session_id,
        "fullPath": home.join(&transcript).to_string_lossy(),
        "fileMtime": bundle.manifest.captured_at_unix_ms,
        "firstPrompt": bundle.manifest.summary.as_deref().unwrap_or_default(),
        "summary": bundle.manifest.title.as_deref().unwrap_or_default(),
        "messageCount": message_count,
        "created": captured,
        "modified": captured,
        "gitBranch": "",
        "projectPath": bundle.manifest.cwd.as_deref().unwrap_or_default(),
        "isSidechain": false,
    }));
    Ok((index, serde_json::to_vec_pretty(&document)?))
}

pub fn session_restore(path: &Path, preflight: bool) -> Result<()> {
    let result = restore_bundle(path, preflight, None)?;
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "harness": result.harness,
            "cwd": result.cwd,
            "launch_args": result.args,
            "preflight": preflight,
        }))?
    );
    Ok(())
}

#[derive(Clone, Debug, Deserialize)]
struct RemoteSessionPage {
    items: Vec<RemoteSession>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct RemoteSession {
    pub id: String,
    pub harness: String,
    pub native_session_id: String,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub cwd: Option<String>,
    pub user_email: String,
    pub updated_at: String,
    #[serde(default)]
    pub shared: bool,
}

fn remote_session_client() -> Result<(Session, reqwest::Url, reqwest::blocking::Client)> {
    let (cfg, client) = load_client()?;
    let session = session_for(&cfg)?;
    let governance = client
        .fetch_or_cached(&session, now_unix())
        .context("loading session policy")?;
    let Some(upload) = governance.session_upload else {
        bail!("remote resume is unavailable because session upload is not configured");
    };
    let base = reqwest::Url::parse(&upload.presign_url).context("invalid session service URL")?;
    let http = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    Ok((session, base, http))
}

pub fn remote_sessions() -> Result<Vec<RemoteSession>> {
    let (session, base, http) = remote_session_client()?;
    let list_url = base
        .join("../session-uploads?resumable=true&limit=100")
        .context("building resumable session URL")?;
    let page: RemoteSessionPage = http
        .get(list_url)
        .bearer_auth(&session.token)
        .send()
        .context("listing remote sessions")?
        .error_for_status()
        .context("remote session list rejected")?
        .json()?;
    Ok(page.items)
}

pub struct PreparedRemoteSession {
    pub(crate) bundle: gh_config::session_bundle::VerifiedBundle,
    pub(crate) destination: PathBuf,
    pub recorded_repository: Option<gh_config::session_bundle::RepositoryIdentity>,
    pub destination_repository: Option<gh_config::session_bundle::RepositoryIdentity>,
}

impl PreparedRemoteSession {
    pub fn repository_mismatch(&self) -> bool {
        self.recorded_repository
            .as_ref()
            .and_then(|repository| repository.remote.as_deref())
            .is_some_and(|recorded| {
                self.destination_repository
                    .as_ref()
                    .and_then(|repository| repository.remote.as_deref())
                    != Some(recorded)
            })
    }
}

pub fn prepare_remote_session(
    selected: &RemoteSession,
    destination: &Path,
) -> Result<PreparedRemoteSession> {
    let (session, base, http) = remote_session_client()?;
    if !destination.is_dir() {
        bail!("chosen destination does not exist or is not a directory");
    }
    let download_url = base.join(&format!("../session-uploads/{}/download", selected.id))?;
    let download: serde_json::Value = http
        .post(download_url)
        .bearer_auth(&session.token)
        .send()
        .context("requesting remote session download")?
        .error_for_status()
        .context("remote session download rejected")?
        .json()?;
    let artifact_url = download
        .get("download_url")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("download response has no URL"))?;
    let bytes = http
        .get(artifact_url)
        .send()
        .context("downloading session bundle")?
        .error_for_status()?
        .bytes()?;
    let bundle = gh_config::session_bundle::verify(&bytes)?;
    restore_verified_bundle(&bundle, true, Some(destination))?;
    Ok(PreparedRemoteSession {
        recorded_repository: bundle.manifest.repository.clone(),
        destination_repository: gh_config::session_bundle::repository_identity(Some(
            &destination.to_string_lossy(),
        )),
        bundle,
        destination: destination.to_path_buf(),
    })
}

pub fn finish_remote_resume(selected: PreparedRemoteSession) -> Result<RestoredSession> {
    restore_verified_bundle(&selected.bundle, false, Some(&selected.destination))
}

fn upload_spooled_session(record: &SessionSpoolRecord) -> Result<UploadOutcome> {
    let harness: Harness = record.harness.parse()?;
    gh_config::implementations::implementation_for_profile(harness, &record.compatibility_profile)
        .ok_or_else(|| {
            anyhow!(
                "unknown compatibility profile `{}` for {harness}",
                record.compatibility_profile
            )
        })?;
    let (cfg, client) = load_client()?;
    let session = session_for(&cfg)?;
    let governance = client
        .fetch_or_cached(&session, now_unix())
        .context("loading session-upload policy")?;
    let Some(upload) = governance.session_upload.as_ref() else {
        return Ok(UploadOutcome::PolicyDisabled);
    };
    let bytes = std::fs::read(&record.artifact_path)
        .with_context(|| format!("reading spooled session {}", record.artifact_path.display()))?;
    if bytes.len() != record.size_bytes || hex::encode(Sha256::digest(&bytes)) != record.sha256 {
        bail!("spooled session bytes do not match their recorded digest");
    }

    let http = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building session upload client")?;
    let presigned: gh_service::PresignedUpload = http
        .post(&upload.presign_url)
        .bearer_auth(&session.token)
        .json(&serde_json::json!({
            "harness": harness.key(),
            "compatibility_profile": record.compatibility_profile,
            "session_id": record.session_id,
            "sha256": record.sha256,
            "size_bytes": record.size_bytes,
            "content_type": record.content_type,
            "cwd": record.cwd,
            "artifact_format": record.artifact_format,
            "resumable": record.resumable,
            "title": record.title,
            "summary": record.summary,
            "captured_at_unix_ms": record.captured_at_unix_ms,
            "repository_root": record.repository.as_ref().and_then(|value| value.root.as_deref()),
            "repository_remote": record.repository.as_ref().and_then(|value| value.remote.as_deref()),
        }))
        .send()
        .context("requesting presigned session upload")?
        .error_for_status()
        .context("session upload presign endpoint rejected the request")?
        .json()
        .context("decoding presigned session upload")?;

    if presigned.status == "complete" {
        return Ok(UploadOutcome::Completed);
    }

    let method = presigned
        .method
        .as_deref()
        .unwrap_or("PUT")
        .to_ascii_uppercase();
    if method != "PUT" && method != "POST" {
        bail!("presigned session upload method must be PUT or POST, got {method}");
    }
    let method = reqwest::Method::from_bytes(method.as_bytes()).context("invalid upload method")?;
    let upload_url = presigned
        .upload_url
        .as_deref()
        .ok_or_else(|| anyhow!("presign response has no upload URL"))?;
    let mut request = http.request(method, upload_url);
    let mut has_content_type = false;
    for (name, value) in presigned.headers {
        if name.eq_ignore_ascii_case("content-type") {
            has_content_type = true;
        }
        request = request.header(&name, &value);
    }
    if !has_content_type {
        request = request.header(reqwest::header::CONTENT_TYPE, &record.content_type);
    }
    request
        .body(bytes)
        .send()
        .context("uploading session bundle")?
        .error_for_status()
        .context("blob storage rejected the session bundle upload")?;

    let complete_url = presigned
        .complete_url
        .as_deref()
        .ok_or_else(|| anyhow!("presign response has no completion URL"))?;
    let complete_url = reqwest::Url::parse(complete_url).or_else(|_| {
        reqwest::Url::parse(&upload.presign_url).and_then(|url| url.join(complete_url))
    })?;
    http.post(complete_url)
        .bearer_auth(&session.token)
        .json(&serde_json::json!({ "sha256": record.sha256 }))
        .send()
        .context("registering the uploaded session")?
        .error_for_status()
        .context("control service rejected session completion")?;
    Ok(UploadOutcome::Completed)
}

fn session_bundle_sources(
    harness: Harness,
    session_id: &str,
    transcript: &Path,
    home: &Path,
) -> Result<Vec<gh_config::session_bundle::SessionSource>> {
    use gh_config::session_bundle::SessionSource;
    let native = |path: &Path| path.strip_prefix(home).ok().map(Path::to_path_buf);
    let mut sources = vec![SessionSource {
        role: match harness {
            Harness::Codex => "rollout",
            Harness::Claude => "primary_transcript",
            Harness::Kimi => "agent_wire_history",
            Harness::Opencode => "portable_export",
        }
        .into(),
        source: transcript.to_path_buf(),
        native_path: native(transcript),
    }];
    let companion_root = match harness {
        Harness::Kimi => transcript.ancestors().nth(3).map(Path::to_path_buf),
        Harness::Claude => transcript.parent().map(|parent| parent.join(session_id)),
        _ => None,
    };
    if let Some(root) = companion_root.filter(|root| root.is_dir()) {
        collect_session_companions(harness, &root, &root, home, &mut sources)?;
    }
    Ok(sources)
}

fn collect_session_companions(
    harness: Harness,
    root: &Path,
    dir: &Path,
    home: &Path,
    output: &mut Vec<gh_config::session_bundle::SessionSource>,
) -> Result<()> {
    if output.len() >= gh_config::session_bundle::MAX_FILES {
        bail!("session has too many artifacts");
    }
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("reading session directory {}", dir.display()))?
    {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            bail!("session artifact {} is a symlink", entry.path().display());
        }
        if kind.is_dir() {
            collect_session_companions(harness, root, &entry.path(), home, output)?;
            continue;
        }
        if !kind.is_file() {
            bail!("unsupported session entry {}", entry.path().display());
        }
        let path = entry.path();
        if output.iter().any(|item| item.source == path) {
            continue;
        }
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let allowed = match harness {
            Harness::Kimi => relative
                .components()
                .next()
                .and_then(|c| c.as_os_str().to_str())
                .is_some_and(|top| ["state.json", "agents", "plans", "attachments"].contains(&top)),
            Harness::Claude => matches!(
                path.extension().and_then(|v| v.to_str()),
                Some("json" | "jsonl" | "txt")
            ),
            _ => false,
        };
        if !allowed {
            continue;
        }
        output.push(gh_config::session_bundle::SessionSource {
            role: match harness {
                Harness::Kimi
                    if path.file_name().and_then(|value| value.to_str()) == Some("state.json") =>
                {
                    "kimi_state"
                }
                Harness::Kimi => "session_state",
                _ => "session_companion",
            }
            .into(),
            native_path: path.strip_prefix(home).ok().map(Path::to_path_buf),
            source: path,
        });
    }
    Ok(())
}

fn normalized_preview(value: &str, max: usize) -> Option<String> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!value.is_empty()).then(|| value.chars().take(max).collect())
}

fn message_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) => normalized_preview(value, 1024),
        serde_json::Value::Array(values) => {
            let joined = values
                .iter()
                .filter_map(message_text)
                .collect::<Vec<_>>()
                .join(" ");
            normalized_preview(&joined, 1024)
        }
        serde_json::Value::Object(object) => ["text", "content", "parts", "prompt"]
            .iter()
            .find_map(|key| object.get(*key).and_then(message_text)),
        _ => None,
    }
}

fn first_user_message(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Array(values) => values.iter().find_map(first_user_message),
        serde_json::Value::Object(object) => {
            let direct_user = object
                .get("role")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|role| role == "user")
                || object
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|kind| kind == "user");
            let nested_user = ["message", "info"].iter().find_map(|key| {
                object.get(*key).filter(|nested| {
                    nested
                        .get("role")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|role| role == "user")
                })
            });
            if direct_user || nested_user.is_some() {
                let candidate = nested_user
                    .and_then(message_text)
                    .or_else(|| message_text(value));
                if candidate.as_deref().is_some_and(|text| {
                    !text.starts_with("# AGENTS.md instructions for")
                        && !text.starts_with("<environment_context>")
                }) {
                    return candidate;
                }
            }
            object.values().find_map(first_user_message)
        }
        _ => None,
    }
}

fn first_user_message_bytes(bytes: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|value| first_user_message(&value))
        .or_else(|| {
            bytes
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
                .find_map(|value| first_user_message(&value))
        })
}

fn preview_title(value: &str) -> Option<String> {
    normalized_preview(value, 80).map(|mut title| {
        if value.chars().count() > 80 {
            title.push('…');
        }
        title
    })
}

fn native_session_display(
    harness: Harness,
    session_id: &str,
    payload: &serde_json::Value,
    transcript: &Path,
    home: &Path,
) -> Result<(Option<String>, Option<String>)> {
    fn text(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
        for key in keys {
            if let Some(found) = value.get(*key).and_then(serde_json::Value::as_str) {
                let found: String = found.chars().take(1024).collect();
                if !found.trim().is_empty() {
                    return Some(found);
                }
            }
        }
        value
            .as_object()?
            .values()
            .find_map(|value| text(value, keys))
    }
    fn matching(value: &serde_json::Value, session_id: &str) -> Option<serde_json::Value> {
        if value.as_object().is_some_and(|object| {
            ["id", "session_id", "sessionId"]
                .iter()
                .any(|key| object.get(*key).and_then(serde_json::Value::as_str) == Some(session_id))
        }) {
            return Some(value.clone());
        }
        match value {
            serde_json::Value::Array(values) => {
                values.iter().find_map(|value| matching(value, session_id))
            }
            serde_json::Value::Object(values) => values
                .values()
                .find_map(|value| matching(value, session_id)),
            _ => None,
        }
    }
    let index = match harness {
        Harness::Codex => Some(home.join(".codex/session_index.jsonl")),
        Harness::Claude => transcript
            .parent()
            .map(|parent| parent.join("sessions-index.json")),
        Harness::Kimi => transcript
            .ancestors()
            .nth(3)
            .map(|root| root.join("state.json")),
        Harness::Opencode => Some(transcript.to_path_buf()),
    };
    let indexed = index
        .as_deref()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| {
            serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|value| matching(&value, session_id))
                .or_else(|| {
                    bytes
                        .split(|byte| *byte == b'\n')
                        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
                        .find_map(|value| matching(&value, session_id))
                })
        });
    let mut title = indexed
        .as_ref()
        .and_then(|value| {
            text(
                value,
                &["title", "session_title", "thread_name", "name", "summary"],
            )
        })
        .or_else(|| text(payload, &["title", "session_title", "name"]));
    let mut summary = indexed
        .as_ref()
        .and_then(|value| text(value, &["summary", "firstPrompt", "prompt", "preview"]))
        .or_else(|| text(payload, &["summary", "prompt", "preview"]));
    if summary.is_none() {
        let bytes = std::fs::read(transcript).with_context(|| {
            format!(
                "reading native session metadata from {}",
                transcript.display()
            )
        })?;
        summary = first_user_message_bytes(&bytes);
    }
    summary = summary.and_then(|value| normalized_preview(&value, 1024));
    if title.is_none() {
        title = summary.as_deref().and_then(preview_title);
    }
    Ok((title.as_deref().and_then(preview_title), summary))
}

const SHIM_MARKER: &str = "# blue shim";

fn shim_dir(dir: Option<&str>) -> Result<std::path::PathBuf> {
    match dir {
        Some(d) => Ok(std::path::PathBuf::from(d)),
        None => {
            let home = gh_common::paths::home_dir()?;
            Ok(home.join(".local").join("bin"))
        }
    }
}

pub fn shim_install(dir: Option<&str>) -> Result<()> {
    let dir = shim_dir(dir)?;
    std::fs::create_dir_all(&dir)?;
    let exe = std::env::current_exe().context("resolving blue binary path")?;
    for harness in Harness::ALL {
        let script = format!(
            "#!/usr/bin/env bash\n{SHIM_MARKER}\nexec \"{}\" run {} -- \"$@\"\n",
            exe.display(),
            harness.key()
        );
        let path = dir.join(harness.key());
        std::fs::write(&path, script)?;
        set_executable(&path);
        println!("  installed shim: {}", path.display());
    }
    println!(
        "Shims installed in {}. Ensure it precedes the real binaries on PATH.",
        dir.display()
    );
    Ok(())
}

pub fn shim_uninstall(dir: Option<&str>) -> Result<()> {
    let dir = shim_dir(dir)?;
    for harness in Harness::ALL {
        let path = dir.join(harness.key());
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if contents.contains(SHIM_MARKER) {
                std::fs::remove_file(&path)?;
                println!("  removed shim: {}", path.display());
            } else {
                println!("  skipped (not a harness shim): {}", path.display());
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
}

#[cfg(not(unix))]
fn set_executable(_path: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn verified_bundle(
        harness: Harness,
        profile: &str,
        session_id: &str,
        root: &Path,
        files: &[(&str, &str, &str)],
    ) -> gh_config::session_bundle::VerifiedBundle {
        std::fs::create_dir_all(root).unwrap();
        let sources = files
            .iter()
            .enumerate()
            .map(|(index, (role, native, body))| {
                let source = root.join(format!("source-{index}"));
                std::fs::write(&source, body).unwrap();
                gh_config::session_bundle::SessionSource {
                    role: (*role).into(),
                    source,
                    native_path: Some(PathBuf::from(native)),
                }
            })
            .collect();
        let made = gh_config::session_bundle::create(
            harness,
            profile,
            session_id,
            Some("/workspace/project".into()),
            Some("Session title".into()),
            Some("First prompt".into()),
            sources,
        )
        .unwrap();
        gh_config::session_bundle::verify(&made.bytes).unwrap()
    }

    #[test]
    fn kimi_index_uses_native_documented_fields_and_absolute_directory() {
        let root = std::env::temp_dir().join(format!("blue-kimi-index-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let bundle = verified_bundle(
            Harness::Kimi,
            "kimi-v0_0_0",
            "session-1",
            &root.join("sources"),
            &[
                (
                    "agent_wire_history",
                    ".config/blue/runtime/kimi/sessions/wd/session-1/agents/main/wire.jsonl",
                    "{}\n",
                ),
                (
                    "kimi_state",
                    ".config/blue/runtime/kimi/sessions/wd/session-1/state.json",
                    "{}",
                ),
            ],
        );
        let (path, body) = prepare_kimi_session_index(&bundle, &home).unwrap();
        assert_eq!(
            path,
            home.join(".config/blue/runtime/kimi/session_index.jsonl")
        );
        let entry: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(entry["sessionId"], "session-1");
        assert_eq!(entry["workDir"], "/workspace/project");
        assert_eq!(
            entry["sessionDir"],
            home.join(".config/blue/runtime/kimi/sessions/wd/session-1")
                .to_string_lossy()
                .as_ref()
        );
        assert!(entry.get("session_id").is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn claude_index_entry_matches_native_shape_before_files_are_written() {
        let root = std::env::temp_dir().join(format!("blue-claude-index-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let bundle = verified_bundle(
            Harness::Claude,
            "claude-v2_0_12",
            "session-1",
            &root.join("sources"),
            &[(
                "primary_transcript",
                ".claude/projects/work/session-1.jsonl",
                "{\"type\":\"user\"}\n",
            )],
        );
        let (path, body) = prepare_claude_session_index(&bundle, &home).unwrap();
        assert_eq!(path, home.join(".claude/projects/work/sessions-index.json"));
        let document: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let entry = &document["entries"][0];
        assert_eq!(entry["sessionId"], "session-1");
        assert_eq!(entry["messageCount"], 1);
        assert!(entry["created"].as_str().unwrap().ends_with('Z'));
        assert!(entry["isSidechain"].is_boolean());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn session_preview_extracts_first_user_turn_across_native_shapes() {
        let codex = br#"{"type":"session_meta","payload":{"id":"s"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Build a portable resume picker"}]}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Working on it"}]}}"#;
        assert_eq!(
            first_user_message_bytes(codex).as_deref(),
            Some("Build a portable resume picker")
        );

        let claude = br#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Fix the dashboard sharing flow"}]}}
{"type":"assistant","message":{"role":"assistant","content":"Sure"}}"#;
        assert_eq!(
            first_user_message_bytes(claude).as_deref(),
            Some("Fix the dashboard sharing flow")
        );

        let portable = br#"{"messages":[{"info":{"role":"user"},"parts":[{"type":"text","text":"Explain this session at a glance"}]}]}"#;
        assert_eq!(
            first_user_message_bytes(portable).as_deref(),
            Some("Explain this session at a glance")
        );
    }

    #[test]
    fn session_preview_skips_injected_environment_context() {
        let transcript = br##"{"role":"user","content":"# AGENTS.md instructions for /workspace"}
{"role":"user","content":"The actual task to display"}"##;
        assert_eq!(
            first_user_message_bytes(transcript).as_deref(),
            Some("The actual task to display")
        );
    }

    #[test]
    fn session_preview_title_is_bounded() {
        let message = "a".repeat(120);
        let title = preview_title(&message).unwrap();
        assert_eq!(title.chars().count(), 81);
        assert!(title.ends_with('…'));
    }

    fn daemon_test_config(revision: &str) -> GovernanceConfig {
        GovernanceConfig {
            revision: revision.to_owned(),
            contract_version: GovernanceConfig::CONTRACT_VERSION,
            required_capabilities: Vec::new(),
            minimum_client_version: None,
            ttl_seconds: None,
            allowed_harnesses: vec!["codex".to_owned()],
            harnesses: Default::default(),
            packages: Vec::new(),
            gateway: None,
            session_upload: None,
            telemetry: None,
            required: false,
        }
    }

    fn test_inventory_entry(name: &str, path: PathBuf) -> HarnessInventoryEntry {
        HarnessInventoryEntry {
            name: name.to_owned(),
            api_allowed: true,
            client_supported: true,
            installed: true,
            path: Some(path),
            raw_version: None,
            version: None,
            compatibility_profile: None,
            compatibility_deprecated: false,
            compatibility_error: None,
            compatibility_warning: None,
            reconciled: false,
        }
    }

    fn selected_test_state(
        revision: &str,
        harness: Harness,
        managed: PathBuf,
        binary: &Path,
    ) -> AppliedState {
        let name = harness.key().to_owned();
        AppliedState {
            schema_version: APPLIED_STATE_SCHEMA_VERSION,
            revision: revision.to_owned(),
            applied_at: now_unix(),
            files: vec![managed.clone()],
            harnesses: vec![name.clone()],
            file_sha256: BTreeMap::new(),
            harness_inventory: Vec::new(),
            files_by_harness: BTreeMap::from([(name.clone(), vec![managed.clone()])]),
            binary_fingerprints: BTreeMap::from([(
                name.clone(),
                binary_fingerprint(binary).unwrap(),
            )]),
            blue_binary_fingerprint: blue_binary_fingerprint(),
            managed_fingerprints: BTreeMap::from([(
                name,
                managed_fingerprints(&[managed]).unwrap(),
            )]),
            gateway_enabled: false,
        }
    }

    #[test]
    fn selected_state_fast_path_detects_managed_tree_drift() {
        let root =
            std::env::temp_dir().join(format!("harness-selected-state-{}", uuid::Uuid::new_v4()));
        let managed = root.join("managed");
        let nested = managed.join("skills/example.md");
        let binary = root.join("codex");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, "governed").unwrap();
        std::fs::write(&binary, "binary").unwrap();
        let config = daemon_test_config("revision");
        let state = selected_test_state("revision", Harness::Codex, managed, &binary);

        assert!(selected_applied_state_is_current(
            &state,
            &config,
            Harness::Codex,
            &binary,
            false,
        ));
        std::fs::write(&nested, "locally changed").unwrap();
        assert!(!selected_applied_state_is_current(
            &state,
            &config,
            Harness::Codex,
            &binary,
            false,
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn selected_state_fast_path_detects_blue_binary_relocation() {
        let root = std::env::temp_dir().join(format!("blue-relocation-{}", uuid::Uuid::new_v4()));
        let managed = root.join("managed.toml");
        let binary = root.join("codex");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&managed, "managed = true").unwrap();
        std::fs::write(&binary, "binary").unwrap();
        let config = daemon_test_config("revision");
        let mut state = selected_test_state("revision", Harness::Codex, managed, &binary);
        state.blue_binary_fingerprint.as_mut().unwrap().path = root.join("old-checkout/blue");

        assert!(!selected_applied_state_is_current(
            &state,
            &config,
            Harness::Codex,
            &binary,
            false,
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn repaired_version_invalidates_fast_path_and_refreshes_inventory() {
        let root =
            std::env::temp_dir().join(format!("harness-repaired-version-{}", uuid::Uuid::new_v4()));
        let managed = root.join("managed.toml");
        let binary = root.join("opencode");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&managed, "managed = true").unwrap();
        std::fs::write(&binary, "binary").unwrap();
        let config = daemon_test_config("revision");
        let state = selected_test_state("revision", Harness::Opencode, managed, &binary);
        assert!(selected_applied_state_is_current(
            &state,
            &config,
            Harness::Opencode,
            &binary,
            false,
        ));
        assert!(!selected_applied_state_is_current_after_version_check(
            &state,
            &config,
            Harness::Opencode,
            &binary,
            false,
            true,
        ));

        let stale_version = semver::Version::new(1, 18, 29);
        let repaired_version = semver::Version::new(1, 18, 25);
        let mut inventory = HarnessInventory {
            entries: vec![HarnessInventoryEntry {
                raw_version: Some("1.18.29".into()),
                version: Some(stale_version),
                compatibility_error: Some("outside certified range".into()),
                reconciled: true,
                ..test_inventory_entry("opencode", binary.clone())
            }],
        };
        let detected = Detected {
            harness: Harness::Opencode,
            path: binary,
            raw_version: Some("1.18.25".into()),
            version: Some(repaired_version.clone()),
        };
        let context = resolve_compatibility(
            Harness::Opencode,
            Some(&repaired_version),
            detected.raw_version.as_deref(),
            &HarnessPolicy::default(),
        )
        .unwrap();
        update_inventory_after_version_check(&mut inventory, &detected, &context);
        let entry = &inventory.entries[0];
        assert_eq!(entry.version.as_ref(), Some(&repaired_version));
        assert_eq!(entry.raw_version.as_deref(), Some("1.18.25"));
        assert_eq!(
            entry.compatibility_profile.as_deref(),
            Some("opencode-v0_0_0")
        );
        assert!(entry.compatibility_error.is_none());
        assert!(!entry.reconciled);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stale_state_schema_does_not_supply_cached_harness_versions() {
        let root =
            std::env::temp_dir().join(format!("harness-stale-cache-{}", uuid::Uuid::new_v4()));
        let binary = root.join("opencode");
        let managed = root.join("managed.toml");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&binary, "binary").unwrap();
        std::fs::write(&managed, "managed = true").unwrap();
        let mut state = selected_test_state("revision", Harness::Opencode, managed, &binary);
        state.schema_version = APPLIED_STATE_SCHEMA_VERSION - 1;
        state.harness_inventory = vec![HarnessInventoryEntry {
            raw_version: Some("cached-stale-version".into()),
            version: Some(semver::Version::new(999, 0, 0)),
            ..test_inventory_entry("opencode", binary)
        }];

        let inventory = discover_inventory_cached(&["opencode".into()], Some(&state));
        let entry = inventory
            .entries
            .iter()
            .find(|entry| entry.name == "opencode")
            .unwrap();
        assert_ne!(entry.raw_version.as_deref(), Some("cached-stale-version"));
        assert_ne!(entry.version, Some(semver::Version::new(999, 0, 0)));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn selected_state_fast_path_detects_gateway_mode_changes() {
        let root =
            std::env::temp_dir().join(format!("harness-selected-mode-{}", uuid::Uuid::new_v4()));
        let managed = root.join("managed.toml");
        let binary = root.join("codex");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&managed, "managed = true").unwrap();
        std::fs::write(&binary, "binary").unwrap();
        let config = daemon_test_config("revision");
        let state = selected_test_state("revision", Harness::Codex, managed, &binary);

        assert!(!selected_applied_state_is_current(
            &state,
            &config,
            Harness::Codex,
            &binary,
            true,
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn selected_state_requires_matching_harness_binary_and_schema() {
        let root = std::env::temp_dir().join(format!(
            "harness-selected-identity-{}",
            uuid::Uuid::new_v4()
        ));
        let managed = root.join("managed.toml");
        let binary = root.join("codex");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&managed, "managed = true").unwrap();
        std::fs::write(&binary, "binary").unwrap();
        let config = daemon_test_config("revision");
        let state = selected_test_state("revision", Harness::Codex, managed, &binary);

        assert!(!selected_applied_state_is_current(
            &state,
            &config,
            Harness::Claude,
            &binary,
            false,
        ));
        std::fs::write(&binary, "replacement binary").unwrap();
        assert!(!selected_applied_state_is_current(
            &state,
            &config,
            Harness::Codex,
            &binary,
            false,
        ));
        let mut legacy = state;
        legacy.schema_version = 0;
        assert!(!selected_applied_state_is_current(
            &legacy,
            &config,
            Harness::Codex,
            &binary,
            false,
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    /// The offline guarantee, in the one form a test can hold: a user with no
    /// network must never be pushed into a browser flow, and a `token`-mode
    /// deployment must never be offered a device flow that resolves the same
    /// static credential.
    #[test]
    fn the_login_triage_only_reauthorizes_on_an_actual_rejection() {
        let config: GovernanceConfig =
            serde_json::from_str(r#"{"revision":"r1","allowed_harnesses":["codex"]}"#).unwrap();

        assert_eq!(login_decision(true, &Ok(config)), LoginDecision::Valid);
        assert_eq!(
            login_decision(true, &Err(gh_common::GhError::unauthorized("dead binding"))),
            LoginDecision::Reauthenticate
        );
        assert_eq!(
            login_decision(
                false,
                &Err(gh_common::GhError::unauthorized("dead binding"))
            ),
            LoginDecision::Rejected
        );
        assert_eq!(
            login_decision(
                true,
                &Err(gh_common::GhError::action_required("run `blue gateway`"))
            ),
            LoginDecision::ActionRequired
        );
        // Transport failure, a 5xx, and an undecodable body all mean "we do
        // not know", never "log in again".
        for error in [
            gh_common::GhError::service("connection refused"),
            gh_common::GhError::service("service returned 503 for governance-config"),
            gh_common::GhError::config("cached config is unsupported"),
        ] {
            assert_eq!(login_decision(true, &Err(error)), LoginDecision::Unverified);
        }
    }

    fn session_with_refresh_token(token: Option<&str>) -> Session {
        let mut session = Session::bearer("access");
        session.refresh_token = token.map(str::to_owned);
        session
    }

    #[test]
    fn gateway_auth_rejections_are_distinct_from_other_gateway_failures() {
        for status in [
            reqwest::StatusCode::UNAUTHORIZED,
            reqwest::StatusCode::FORBIDDEN,
        ] {
            let error = anyhow::Error::new(GatewayHttpError {
                status,
                message: "rejected".into(),
                detail: None,
            });
            assert!(gateway_rejected_session(&error));
        }

        for status in [
            reqwest::StatusCode::BAD_REQUEST,
            reqwest::StatusCode::BAD_GATEWAY,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
        ] {
            let error = anyhow::Error::new(GatewayHttpError {
                status,
                message: "not an authentication rejection".into(),
                detail: None,
            });
            assert!(!gateway_rejected_session(&error));
        }
        assert!(!gateway_rejected_session(&anyhow!("transport failure")));
    }

    #[test]
    fn superseded_refresh_grants_are_retired_best_effort() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let previous = session_with_refresh_token(Some("old-refresh"));
        let replacement = session_with_refresh_token(Some("new-refresh"));
        let calls = AtomicUsize::new(0);
        retire_superseded_session_with(&previous, &replacement, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(gh_common::GhError::service("revocation unavailable"))
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let unchanged = session_with_refresh_token(Some("old-refresh"));
        retire_superseded_session_with(&previous, &unchanged, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let without_refresh = session_with_refresh_token(None);
        retire_superseded_session_with(&without_refresh, &replacement, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn gateway_outage_message_is_plain_and_actionable() {
        let message = gateway_api_error_message(
            reqwest::StatusCode::BAD_GATEWAY,
            "gateway unavailable: error sending request for url (http://internal.example/user/list)",
            None,
        );

        assert!(message.starts_with("Gateway setup is currently unavailable."));
        assert!(message.contains("coding agent was not started"));
        assert!(message.contains("gateway or its provisioner"));
        assert!(!message.contains("http://"));
        assert!(!message.contains("error sending request"));
    }

    #[test]
    fn a_rejected_session_names_the_command_that_fixes_it() {
        // `blue run` hits /gateway/key/ensure before it fetches policy, so this
        // is the first place an expired session becomes visible.
        for status in [
            reqwest::StatusCode::UNAUTHORIZED,
            reqwest::StatusCode::FORBIDDEN,
        ] {
            let message = gateway_api_error_message(status, "unauthorized", None);
            assert!(message.contains("blue login"), "{message}");
        }
        assert!(gateway_api_error_message(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "boom",
            None
        )
        .contains("boom"));
    }

    #[test]
    fn a_rejected_session_relays_why_when_the_server_says_why() {
        // The dead-binding 401 is the one the CLI could never explain before:
        // the OAuth refresh still works, so "log in again" alone reads as a
        // contradiction of what `blue login` just printed.
        let message = gateway_api_error_message(
            reqwest::StatusCode::UNAUTHORIZED,
            "your browser sign-in that authorized this CLI has expired; run `blue login` to re-authorize",
            Some("your browser sign-in that authorized this CLI has expired; run `blue login` to re-authorize"),
        );
        assert!(message.contains("browser sign-in"), "{message}");

        // A 403 is a different problem; it must not claim the session expired.
        let forbidden = gateway_api_error_message(
            reqwest::StatusCode::FORBIDDEN,
            "missing OAuth scope governance:read",
            Some("missing OAuth scope governance:read"),
        );
        assert!(forbidden.contains("missing OAuth scope"), "{forbidden}");
        assert!(!forbidden.contains("no longer valid"), "{forbidden}");
    }

    fn gateway_access(enabled: bool, status: &str) -> GatewayKeyResponse {
        serde_json::from_value(serde_json::json!({
            "enabled": enabled,
            "email": "user@example.com",
            "status": status,
        }))
        .unwrap()
    }

    #[test]
    fn disabled_gateway_response_needs_no_provisioning() {
        // Governance-only: the server answers ensure with enabled:false, so the
        // CLI's ensure-first step is inert instead of hitting the old 400. A
        // legacy "missing" status is equally inert while gateway mode is off.
        assert!(!gateway_access_requires_provisioning(&gateway_access(
            false, "disabled"
        )));
        assert!(!gateway_access_requires_provisioning(&gateway_access(
            false, "missing"
        )));

        // Gateway-on paths are unchanged: a provisioned key is a no-op, an
        // unfinished one still requires follow-up.
        assert!(!gateway_access_requires_provisioning(&gateway_access(
            true, "ready"
        )));
        assert!(gateway_access_requires_provisioning(&gateway_access(
            true, "missing"
        )));
    }

    #[test]
    fn only_transient_gateway_http_failures_offer_retry() {
        let unavailable = anyhow::Error::new(GatewayHttpError {
            status: reqwest::StatusCode::BAD_GATEWAY,
            message: "temporary".into(),
            detail: None,
        });
        let invalid_config = anyhow::Error::new(GatewayHttpError {
            status: reqwest::StatusCode::BAD_REQUEST,
            message: "invalid configuration".into(),
            detail: None,
        });

        assert!(retryable_gateway_http_error(&unavailable));
        assert!(!retryable_gateway_http_error(&invalid_config));
    }

    #[test]
    fn embedded_gateway_outage_details_are_replaced_with_actionable_copy() {
        let message = gateway_provisioning_failure(
            Some(
                "gateway is unavailable: failed to start provisioner executable: \
                 No such file or directory (os error 2)",
            ),
            "gateway key must be provisioned",
        );

        assert!(message.contains("gateway or its provisioner"));
        assert!(!message.contains("No such file or directory"));
        assert!(!message.contains("os error"));
    }

    #[test]
    fn selected_updates_merge_at_one_revision_and_reset_at_the_next() {
        let root = std::env::temp_dir().join(format!(
            "harness-partial-applied-state-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let codex_file = root.join("codex.toml");
        let claude_file = root.join("claude.json");
        let codex_binary = root.join("codex");
        let claude_binary = root.join("claude");
        for path in [&codex_file, &claude_file, &codex_binary, &claude_binary] {
            std::fs::write(path, b"managed").unwrap();
        }
        let inventory = HarnessInventory {
            entries: vec![
                test_inventory_entry("codex", codex_binary),
                test_inventory_entry("claude", claude_binary),
            ],
        };
        let mut state = selected_test_state(
            "r1",
            Harness::Codex,
            codex_file.clone(),
            inventory.entries[0].path.as_deref().unwrap(),
        );
        let claude_write = gh_config::HarnessWrite {
            files: vec![claude_file.clone()],
            ..Default::default()
        };

        update_selected_applied_state(
            &mut state,
            "r1",
            Harness::Claude,
            &claude_write,
            &inventory,
            false,
        )
        .unwrap();
        assert_eq!(state.harnesses, vec!["claude", "codex"]);
        assert!(state.files.contains(&codex_file));
        assert!(state.files.contains(&claude_file));

        update_selected_applied_state(
            &mut state,
            "r2",
            Harness::Claude,
            &claude_write,
            &inventory,
            false,
        )
        .unwrap();
        assert_eq!(state.revision, "r2");
        assert_eq!(state.harnesses, vec!["claude"]);
        assert_eq!(state.files, vec![claude_file]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn daemon_failure_includes_package_and_compatibility_errors() {
        let inventory = HarnessInventory {
            entries: vec![HarnessInventoryEntry {
                name: "claude".to_owned(),
                api_allowed: true,
                client_supported: true,
                installed: true,
                path: None,
                raw_version: None,
                version: None,
                compatibility_profile: None,
                compatibility_deprecated: false,
                compatibility_error: Some("unsupported version".to_owned()),
                compatibility_warning: None,
                reconciled: false,
            }],
        };
        let results = vec![gh_agent::HarnessReconcile {
            harness: Harness::Codex,
            result: Ok(gh_config::HarnessWrite {
                package_errors: vec!["digest mismatch".to_owned()],
                ..Default::default()
            }),
        }];

        let error = daemon_reconcile_errors(&inventory, &results);

        assert!(error.contains("claude: unsupported version"));
        assert!(error.contains("codex package: digest mismatch"));
    }

    #[test]
    fn unchanged_approved_merge_does_not_prompt_again() {
        let files = vec![PathBuf::from("/home/user/.codex/config.toml")];
        let current = BTreeMap::from([("reasoning_effort".to_owned(), "medium".to_owned())]);
        let proposed = BTreeMap::from([("reasoning_effort".to_owned(), "low".to_owned())]);
        let mut approvals = MergeApprovals::default();

        let digest =
            unapproved_merge_digest(&approvals, Harness::Codex, &files, &current, &proposed)
                .expect("the first semantic merge requires approval");
        approvals
            .approvals
            .insert(Harness::Codex.key().to_owned(), digest);

        assert!(
            unapproved_merge_digest(&approvals, Harness::Codex, &files, &current, &proposed,)
                .is_none()
        );
        let changed = BTreeMap::from([("reasoning_effort".to_owned(), "high".to_owned())]);
        assert!(
            unapproved_merge_digest(&approvals, Harness::Codex, &files, &current, &changed,)
                .is_some()
        );
    }

    #[test]
    fn existing_codex_overlay_is_the_current_managed_state() {
        let root = std::env::temp_dir().join(format!(
            "harness-codex-merge-inspection-{}",
            uuid::Uuid::new_v4()
        ));
        let codex = root.join(".codex");
        std::fs::create_dir_all(&codex).unwrap();
        let native = codex.join("config.toml");
        let overlay = codex.join("blue.config.toml");
        std::fs::write(&native, "model_reasoning_effort = \"medium\"\n").unwrap();
        std::fs::write(
            &overlay,
            "approval_policy = \"on-request\"\nmodel_reasoning_effort = \"low\"\n",
        )
        .unwrap();
        let policy: gh_service::HarnessPolicy = serde_json::from_value(serde_json::json!({
            "managed_config": { "reasoning_effort": "low" }
        }))
        .unwrap();

        let current = current_merge_values(
            gh_config::implementations::implementation_for_profile(Harness::Codex, "codex-v1")
                .unwrap()
                .implementation,
            &policy,
            &[native.clone(), overlay.clone()],
        );
        let proposed = proposed_merge_values(
            gh_config::implementations::implementation_for_profile(Harness::Codex, "codex-v1")
                .unwrap()
                .implementation,
            &policy,
            None,
            WriteOptions::default(),
            current.clone(),
        );

        assert_eq!(current["reasoning_effort"], "low");
        assert_eq!(current, proposed);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn proposal_preserves_existing_values_and_adds_governed_changes() {
        let current = BTreeMap::from([
            ("model".to_string(), "personal-model".to_string()),
            ("mcp".to_string(), "personal-mcp".to_string()),
        ]);
        let policy: gh_service::HarnessPolicy = serde_json::from_value(serde_json::json!({
            "managed_config": {
                "model": "governed-model",
                "approval_policy": "never"
            },
            "mcp": [{ "name": "blocks", "command": "npx" }]
        }))
        .unwrap();
        let proposed = proposed_merge_values(
            gh_config::implementations::implementation_for_profile(Harness::Codex, "codex-v1")
                .unwrap()
                .implementation,
            &policy,
            None,
            WriteOptions {
                allow_existing_merge: true,
                ..WriteOptions::default()
            },
            current,
        );

        assert_eq!(proposed["model"], "governed-model");
        assert_eq!(proposed["approval_policy"], "never");
        assert_eq!(proposed["mcp"], "blocks, personal-mcp");
    }

    #[test]
    fn reconciled_codex_values_produce_no_merge_diff() {
        let current = BTreeMap::from([
            ("approval_policy".to_string(), "never".to_string()),
            ("fast_mode".to_string(), "false".to_string()),
            (
                "gateway".to_string(),
                "https://inference.example".to_string(),
            ),
            ("mcp".to_string(), "blocks, personal".to_string()),
            ("model".to_string(), "gpt-5.6-sol".to_string()),
            ("model_provider".to_string(), "governed".to_string()),
            ("reasoning_effort".to_string(), "medium".to_string()),
            ("sandbox_mode".to_string(), "workspace-write".to_string()),
            ("service_tier".to_string(), "default".to_string()),
        ]);
        let policy: gh_service::HarnessPolicy = serde_json::from_value(serde_json::json!({
            "managed_config": {
                "model": "gpt-5.6-sol",
                "reasoning_effort": "medium",
                "fast_mode": false,
                "approval_policy": "never",
                "sandbox_mode": "workspace-write"
            },
            "mcp": [{ "name": "blocks", "command": "npx" }]
        }))
        .unwrap();
        let gateway: gh_service::GatewayConfig = serde_json::from_value(serde_json::json!({
            "type": "litellm",
            "proxy_url": "https://inference.example",
            "token": "token"
        }))
        .unwrap();

        let proposed = proposed_merge_values(
            gh_config::implementations::implementation_for_profile(Harness::Codex, "codex-v1")
                .unwrap()
                .implementation,
            &policy,
            Some(&gateway),
            WriteOptions::default(),
            current.clone(),
        );
        assert_eq!(proposed, current);
    }

    #[test]
    fn diff_marks_removed_and_added_values() {
        let rendered = colored_diff("model = personal\n", "model = governed\n");
        let plain = console::strip_ansi_codes(&rendered);
        assert!(plain.contains("- model = personal"));
        assert!(plain.contains("+ model = governed"));
    }

    #[test]
    fn jwt_expiry_is_read_without_verifying_the_signature() {
        // {"exp":2000000000,"scope":"gateway:infer"}, base64url, no padding.
        let payload = "eyJleHAiOjIwMDAwMDAwMDAsInNjb3BlIjoiZ2F0ZXdheTppbmZlciJ9";
        assert_eq!(
            jwt_expires_at(&format!("header.{payload}.signature")),
            Some(2_000_000_000)
        );
        assert_eq!(jwt_expires_at("not-a-jwt"), None);
        assert_eq!(jwt_expires_at("header.!!!not-base64!!!.sig"), None);
        // A token with no exp claim is not an error, just unknown.
        assert_eq!(jwt_expires_at("header.eyJhIjoxfQ.sig"), None);
    }

    #[test]
    fn only_actionable_errors_raise_an_auth_notice() {
        let notice = Arc::new(Mutex::new(None));

        record_auth_notice(&gh_common::GhError::service("connection refused"), &notice);
        assert_eq!(*notice.lock().unwrap(), None);

        record_auth_notice(&gh_common::GhError::unauthorized("expired"), &notice);
        assert_eq!(notice.lock().unwrap().as_deref(), Some("expired"));

        record_auth_notice(
            &gh_common::GhError::action_required("run `blue gateway`"),
            &notice,
        );
        assert_eq!(
            notice.lock().unwrap().as_deref(),
            Some("run `blue gateway`")
        );
    }

    #[test]
    fn revision_notices_skip_active_and_duplicate_revisions() {
        let mut seen = BTreeSet::new();
        assert!(revision_notice_message("r1", Harness::Codex, "r1".into(), &mut seen).is_none());
        let message =
            revision_notice_message("r1", Harness::Codex, "r2".into(), &mut seen).unwrap();
        assert_eq!(message, "New Blue policy available");
        assert!(revision_notice_message("r1", Harness::Codex, "r2".into(), &mut seen).is_none());
        assert!(revision_notice_message("r1", Harness::Codex, "r3".into(), &mut seen).is_some());
    }

    #[test]
    fn revision_notice_is_recorded_when_desktop_delivery_succeeds_or_fails() {
        for delivered in [true, false] {
            let pending = Arc::new(Mutex::new(None));
            assert_eq!(
                record_and_send_revision_notice(
                    "New Blue policy available".into(),
                    &pending,
                    |_| delivered,
                ),
                delivered
            );
            assert_eq!(
                pending.lock().unwrap().as_deref(),
                Some("New Blue policy available")
            );
        }
    }

    #[test]
    fn preference_auto_selects_one_and_rejects_stale_values() {
        assert_eq!(
            resolved_preference(&["codex".into()], None).as_deref(),
            Some("codex")
        );
        let several = vec!["codex".into(), "claude".into()];
        assert_eq!(
            resolved_preference(&several, Some("claude")).as_deref(),
            Some("claude")
        );
        assert_eq!(resolved_preference(&several, Some("kimi")), None);
        assert_eq!(resolved_preference(&several, None), None);
    }

    #[test]
    fn tenant_identity_ignores_a_trailing_url_slash() {
        assert_eq!(
            tenant_id("https://control.example.com"),
            tenant_id("https://control.example.com/")
        );
        assert_ne!(
            tenant_id("https://control.example.com"),
            tenant_id("https://other.example.com")
        );
    }

    fn tenant_identity_test_root() -> PathBuf {
        std::env::temp_dir().join(format!("blue-tenant-identity-{}", uuid::Uuid::new_v4()))
    }

    fn tenant_session(organization_id: uuid::Uuid) -> Session {
        let mut session = Session::bearer("token");
        session.org_id = Some(organization_id.to_string());
        session
    }

    #[test]
    fn instance_ids_are_stable_within_and_distinct_between_tenants() {
        let root = tenant_identity_test_root();
        let mut cfg = BlueToml::default();
        cfg.service.url = "https://control.example.com".into();
        let first_tenant = tenant_session(uuid::Uuid::new_v4());
        let second_tenant = tenant_session(uuid::Uuid::new_v4());

        let first = tenant_instance_id_at(&root, &cfg, &first_tenant).unwrap();
        let second = tenant_instance_id_at(&root, &cfg, &second_tenant).unwrap();
        assert_ne!(first, second);
        assert_eq!(
            tenant_instance_id_at(&root, &cfg, &first_tenant).unwrap(),
            first
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn active_tenant_claims_the_legacy_global_instance_id_once() {
        let root = tenant_identity_test_root();
        let legacy = "workstation-legacy-id".to_owned();
        gh_common::write_atomic(&root.join("instance-id"), legacy.as_bytes()).unwrap();
        let mut cfg = BlueToml::default();
        cfg.service.url = "https://control.example.com".into();

        let claimed =
            tenant_instance_id_at(&root, &cfg, &tenant_session(uuid::Uuid::new_v4())).unwrap();
        assert_eq!(claimed, legacy);
        assert!(!root.join("instance-id").exists());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sessions_without_an_organization_use_a_deployment_scoped_id() {
        let root = tenant_identity_test_root();
        let mut cfg = BlueToml::default();
        cfg.service.url = "https://control.example.com/".into();
        let session = Session::bearer("token");
        let first = tenant_instance_id_at(&root, &cfg, &session).unwrap();
        cfg.service.url = "https://control.example.com".into();
        assert_eq!(tenant_instance_id_at(&root, &cfg, &session).unwrap(), first);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_session_organization_does_not_fall_back_to_deployment_identity() {
        let root = tenant_identity_test_root();
        let mut cfg = BlueToml::default();
        cfg.service.url = "https://control.example.com".into();
        let mut session = Session::bearer("token");
        session.org_id = Some("not-an-organization-uuid".into());
        let error = tenant_instance_id_at(&root, &cfg, &session).unwrap_err();
        assert!(error.to_string().contains("invalid organization ID"));
        assert!(!root.exists());
    }

    #[test]
    fn agent_selection_requires_a_known_eligible_agent() {
        let eligible = vec!["codex".into(), "claude".into()];
        assert_eq!(
            validate_agent_selection(&eligible, "claude").unwrap(),
            "claude"
        );

        let error = validate_agent_selection(&eligible, "kimi").unwrap_err();
        assert!(error.to_string().contains("agent `kimi` is not eligible"));
        assert!(error.to_string().contains("codex, claude"));

        let error = validate_agent_selection(&eligible, "unknown").unwrap_err();
        assert!(error.to_string().contains("not a known harness"));
    }

    #[test]
    fn maintenance_requires_and_scopes_to_the_configured_default() {
        let root = std::env::temp_dir();
        let inventory = HarnessInventory {
            entries: vec![
                test_inventory_entry("codex", root.join("codex")),
                test_inventory_entry("claude", root.join("claude")),
            ],
        };
        let mut cfg = BlueToml::default();
        assert!(configured_default_harness(&cfg, &inventory)
            .unwrap_err()
            .to_string()
            .contains("no default agent"));

        cfg.ui.preferred_harness = Some("claude".into());
        assert_eq!(
            configured_default_harness(&cfg, &inventory).unwrap(),
            Harness::Claude
        );
        let scoped = inventory_for_harness(&inventory, Harness::Claude);
        assert_eq!(scoped.entries.len(), 1);
        assert_eq!(scoped.entries[0].name, "claude");
    }

    #[test]
    fn npm_version_lookup_selects_the_highest_published_match() {
        assert_eq!(
            highest_npm_version(br#"["1.18.23","1.18.25","1.18.24"]"#).unwrap(),
            semver::Version::new(1, 18, 25)
        );
        assert_eq!(
            highest_npm_version(br#""v1.18.25""#).unwrap(),
            semver::Version::new(1, 18, 25)
        );
        assert!(highest_npm_version(br#"[]"#).is_err());
    }

    #[test]
    fn kimi_standalone_repair_targets_the_detected_installation_root() {
        assert_eq!(
            kimi_install_root(Path::new("/Users/example/.kimi-code/bin/kimi")),
            Some(Path::new("/Users/example/.kimi-code"))
        );
        assert_eq!(
            kimi_install_root(Path::new("/usr/local/bin/kimi")),
            Some(Path::new("/usr/local"))
        );
    }

    fn read_session_metadata(dir: &Path, uuid: &str) -> BlueSessionMetadata {
        let bytes = std::fs::read(session_metadata_path(dir, uuid)).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn record_session_start_writes_native_session_mapping() {
        let dir = std::env::temp_dir().join(format!("blue-session-start-{}", uuid::Uuid::new_v4()));
        let uuid = uuid::Uuid::new_v4().to_string();
        record_session_start(
            &dir,
            &uuid,
            Harness::Codex,
            "codex-v0_114_0",
            &serde_json::json!({
                "session_id": "native-abc",
                "transcript_path": "/tmp/native-abc.jsonl",
                "cwd": "/work",
                "source": "startup",
            }),
        )
        .unwrap();
        let metadata = read_session_metadata(&dir, &uuid);
        assert_eq!(metadata.schema_version, 1);
        assert_eq!(metadata.blue_session_id, uuid);
        assert_eq!(metadata.harness, "codex");
        assert_eq!(metadata.compatibility_profile, "codex-v0_114_0");
        assert_eq!(metadata.coding_agent_session_id, "native-abc");
        assert_eq!(
            metadata.transcript_path.as_deref(),
            Some("/tmp/native-abc.jsonl")
        );
        assert_eq!(metadata.cwd.as_deref(), Some("/work"));
        assert_eq!(metadata.source.as_deref(), Some("startup"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn record_session_start_is_last_write_wins_and_preserves_created_at() {
        let dir = std::env::temp_dir().join(format!("blue-session-lww-{}", uuid::Uuid::new_v4()));
        let uuid = uuid::Uuid::new_v4().to_string();
        // A pre-2.1.73 Claude fires a throwaway startup event before the real
        // resume event; the later write must win but keep the first created_at.
        record_session_start(
            &dir,
            &uuid,
            Harness::Claude,
            "claude-v2_0_12",
            &serde_json::json!({ "session_id": "first" }),
        )
        .unwrap();
        let created = read_session_metadata(&dir, &uuid).created_at_unix;
        record_session_start(
            &dir,
            &uuid,
            Harness::Claude,
            "claude-v2_0_12",
            &serde_json::json!({ "session_id": "second" }),
        )
        .unwrap();
        let metadata = read_session_metadata(&dir, &uuid);
        assert_eq!(metadata.coding_agent_session_id, "second");
        assert_eq!(metadata.created_at_unix, created);
        assert!(metadata.updated_at_unix >= created);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn record_session_start_requires_a_session_id() {
        let dir = std::env::temp_dir().join(format!("blue-session-nosid-{}", uuid::Uuid::new_v4()));
        let uuid = uuid::Uuid::new_v4().to_string();
        let error = record_session_start(
            &dir,
            &uuid,
            Harness::Kimi,
            "kimi-v0_0_0",
            &serde_json::json!({ "cwd": "/work" }),
        )
        .unwrap_err();
        assert!(error.to_string().contains("no session_id"));
        assert!(!session_metadata_path(&dir, &uuid).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn session_start_without_blue_session_id_is_a_no_op() {
        // No BLUE_SESSION_ID in the environment ⇒ agent launched outside blue.
        std::env::remove_var("BLUE_SESSION_ID");
        assert!(session_start("codex", Some("codex-v0_114_0")).is_ok());
    }

    fn spool_record(session_id: &str, sha: &str, blue: Option<&str>) -> SessionSpoolRecord {
        SessionSpoolRecord {
            harness: "codex".into(),
            compatibility_profile: "codex-v0_145_0".into(),
            session_id: session_id.into(),
            cwd: None,
            sha256: sha.into(),
            size_bytes: sha.len(),
            content_type: "application/json".into(),
            artifact_format: "legacy-raw".into(),
            resumable: false,
            title: None,
            summary: None,
            captured_at_unix_ms: 0,
            repository: None,
            artifact_path: PathBuf::from("/tmp/none"),
            blue_session_id: blue.map(str::to_owned),
        }
    }

    fn read_upload_state(dir: &Path, key: &str) -> SessionUploadState {
        serde_json::from_slice(&std::fs::read(dir.join(format!("{key}.json"))).unwrap()).unwrap()
    }

    #[test]
    fn stamp_upload_state_unions_blue_sessions_and_refreshes_digest() {
        let dir = std::env::temp_dir().join(format!("blue-upload-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = "spoolkey";

        // First upload from blue session A.
        stamp_upload_state_in(
            &dir,
            key,
            &spool_record("native-1", "sha-a", Some("uuid-a")),
        )
        .unwrap();
        let state = read_upload_state(&dir, key);
        assert_eq!(state.schema_version, 1);
        assert_eq!(state.session_id, "native-1");
        assert_eq!(state.sha256, "sha-a");
        assert_eq!(state.blue_session_ids, ["uuid-a"]);

        // A resume through blue (new uuid, same native session) with a grown
        // transcript unions the uuid and refreshes the digest.
        stamp_upload_state_in(
            &dir,
            key,
            &spool_record("native-1", "sha-b", Some("uuid-b")),
        )
        .unwrap();
        let state = read_upload_state(&dir, key);
        assert_eq!(state.sha256, "sha-b");
        assert_eq!(state.blue_session_ids, ["uuid-a", "uuid-b"]);

        // Idempotent: re-stamping the same uuid does not duplicate it.
        stamp_upload_state_in(
            &dir,
            key,
            &spool_record("native-1", "sha-b", Some("uuid-b")),
        )
        .unwrap();
        assert_eq!(
            read_upload_state(&dir, key).blue_session_ids,
            ["uuid-a", "uuid-b"]
        );

        // Env-scrubbed hook (no blue uuid) still stamps the digest.
        stamp_upload_state_in(&dir, key, &spool_record("native-1", "sha-c", None)).unwrap();
        let state = read_upload_state(&dir, key);
        assert_eq!(state.sha256, "sha-c");
        assert_eq!(state.blue_session_ids, ["uuid-a", "uuid-b"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn spool_key_matches_the_identity_digest_spool_session_writes() {
        // spool_session now derives its filename from spool_key, so the fallback
        // and the spooler always agree. Pin the exact identity digest.
        let expected = hex::encode(Sha256::digest("codex\0codex-v0_145_0\0native-1"));
        assert_eq!(
            spool_key(Harness::Codex, "codex-v0_145_0", "native-1"),
            expected
        );
    }

    #[test]
    fn finalize_blue_session_no_ops_without_metadata_or_when_disabled() {
        // Disabled ⇒ pure no-op regardless of anything on disk.
        finalize_blue_session("irrelevant", false);
        // A minted uuid with no session-start metadata resolves cleanly.
        let uuid = uuid::Uuid::new_v4().to_string();
        assert!(finalize_blue_session_inner(&uuid).is_ok());
    }

    #[test]
    fn prepared_resume_treats_missing_or_different_destination_origin_as_a_mismatch() {
        let prepared = |destination_remote: Option<&str>| PreparedRemoteSession {
            bundle: gh_config::session_bundle::VerifiedBundle {
                manifest: gh_config::session_bundle::BundleManifest {
                    schema_version: 1,
                    artifact_format: gh_config::session_bundle::ARTIFACT_FORMAT.into(),
                    harness: "codex".into(),
                    compatibility_profile: "codex-v0_145_0".into(),
                    native_session_id: "native-1".into(),
                    captured_at_unix_ms: 0,
                    cwd: None,
                    repository: None,
                    title: None,
                    summary: None,
                    files: Vec::new(),
                },
                files: std::collections::BTreeMap::new(),
            },
            destination: PathBuf::from("/destination"),
            recorded_repository: Some(gh_config::session_bundle::RepositoryIdentity {
                root: Some("/recorded".into()),
                remote: Some("git@example.com:team/repo.git".into()),
            }),
            destination_repository: destination_remote.map(|remote| {
                gh_config::session_bundle::RepositoryIdentity {
                    root: Some("/destination".into()),
                    remote: Some(remote.into()),
                }
            }),
        };

        let matching = prepared(Some("git@example.com:team/repo.git"));
        assert_eq!(matching.bundle.manifest.native_session_id, "native-1");
        assert_eq!(matching.destination, PathBuf::from("/destination"));
        assert!(!matching.repository_mismatch());
        assert!(prepared(Some("git@example.com:other/repo.git")).repository_mismatch());
        assert!(prepared(None).repository_mismatch());
    }
}
