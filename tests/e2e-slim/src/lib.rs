//! Shared support for the `e2e-slim` suite.
//!
//! The suite drives the real `blue` CLI against a lean, **gateway-disabled**
//! control-api stack (Postgres + MinIO + a JWKS sidecar). Because the dashboard
//! / Better Auth stack is absent, we self-serve authentication: control-api
//! still verifies user tokens the normal way (RS256 signature via its cached
//! JWKS, plus issuer / audience / expiry), so we mint our own RS256 JWT with a
//! committed **test-only** key whose public half the sidecar serves.
//!
//! Every integration test starts with [`env_or_skip`]; when the stack env
//! (`E2E_SLIM_CONTROL_API_URL`) is unset the test early-returns, so the suite
//! stays green without the compose stack up. This crate is its own workspace
//! (not a root member) so its test-only deps stay out of the deployment image's
//! cargo-chef recipe; build/test it via its own manifest.

mod platform;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;

/// Key id shared by the signing header and the served JWK. Must byte-match the
/// `kid` in `fixtures/jwks/jwks.json`.
pub const KID: &str = "e2e-slim-rsa-1";
/// Token issuer. Must byte-match compose `HARNESS_AUTH_ISSUER`.
pub const ISSUER: &str = "https://e2e-slim.blue.test/";
/// Token audience. Must byte-match compose `HARNESS_AUTH_AUDIENCE`.
pub const AUDIENCE: &str = "https://control-api.e2e-slim.blue.test";
/// Signing algorithm. Must match the JWK `alg`.
pub const ALG: Algorithm = Algorithm::RS256;

/// Scopes the control-api user routes require: `governance:read` (config +
/// agent selection), `session:write` (presign + complete), `client-status:write`.
pub const SCOPES: &str = "governance:read session:write client-status:write";

/// Committed, **test-only** RSA private key (PKCS#8 PEM). Signs the RS256 user
/// tokens; the matching public JWK is served by `jwks-server.mjs`.
const SIGNING_KEY_PEM: &[u8] = include_bytes!("../fixtures/jwks/jwt-signing-key.pem");

/// Handle to the running slim stack, resolved from the environment.
pub struct Stack {
    pub control_api_url: String,
    pub database_url: String,
    pub minio_url: String,
    pub blue_bin: PathBuf,
    pub http: reqwest::blocking::Client,
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// CI must fail when coverage prerequisites are absent.
pub fn required() -> bool {
    std::env::var("E2E_SLIM_REQUIRED").as_deref() == Ok("1")
}
fn required_env(name: &str) -> Option<String> {
    let value = non_empty_env(name);
    assert!(
        !required() || value.is_some(),
        "missing required environment variable {name}"
    );
    value
}

/// A completed matrix cell, written only after all its assertions succeed.
pub fn record_cell(suite: &str, agent: &str, version: &str) {
    if let Some(dir) = non_empty_env("E2E_SLIM_REPORT_DIR") {
        let path = Path::new(&dir).join(format!("{suite}-{agent}-{version}.json"));
        std::fs::create_dir_all(&dir).expect("creating report directory");
        std::fs::write(
            path,
            serde_json::json!({"suite":suite,"agent":agent,"version":version,"status":"passed"})
                .to_string(),
        )
        .expect("writing cell evidence");
    }
}

/// Resolve the running stack from the environment, or `None` to skip.
///
/// Only `E2E_SLIM_CONTROL_API_URL` is required; the rest fall back to the
/// well-known host-published defaults from `docker-compose.yml`.
pub fn env_or_skip() -> Option<Stack> {
    let control_api_url = required_env("E2E_SLIM_CONTROL_API_URL")?;
    let database_url = required_env("E2E_SLIM_DATABASE_URL")
        .unwrap_or_else(|| "postgres://harness:harness@127.0.0.1:5432/governance".to_owned());
    let minio_url =
        required_env("E2E_SLIM_MINIO_URL").unwrap_or_else(|| "http://127.0.0.1:9000".to_owned());
    let blue_bin = PathBuf::from(
        non_empty_env("E2E_SLIM_BLUE_BIN")
            .unwrap_or_else(|| format!("target/debug/blue{}", std::env::consts::EXE_SUFFIX)),
    );
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("building reqwest client");
    Some(Stack {
        control_api_url: control_api_url.trim_end_matches('/').to_owned(),
        database_url,
        minio_url: minio_url.trim_end_matches('/').to_owned(),
        blue_bin,
        http,
    })
}

fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

impl Stack {
    /// Poll `GET /health` until it returns 200, up to ~60s.
    pub fn wait_healthy(&self) {
        let url = format!("{}/health", self.control_api_url);
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut last;
        loop {
            match self.http.get(&url).send() {
                Ok(response) if response.status().is_success() => return,
                Ok(response) => last = format!("status {}", response.status()),
                Err(error) => last = error.to_string(),
            }
            if Instant::now() >= deadline {
                panic!("control-api never became healthy at {url}: {last}");
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Read the bootstrapped org UUID from Postgres. The org id is randomly
    /// generated at bootstrap (`Uuid::new_v4()`), so it must never be hardcoded.
    /// Reads `id::text` to avoid a `postgres` uuid feature dependency.
    pub fn org_id(&self) -> String {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut last;
        loop {
            match self.try_org_id() {
                Ok(Some(id)) => return id,
                Ok(None) => last = "org slug 'e2e' not yet present".to_owned(),
                Err(error) => last = error,
            }
            if Instant::now() >= deadline {
                panic!("could not read org id from Postgres: {last}");
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn try_org_id(&self) -> Result<Option<String>, String> {
        let mut client = postgres::Client::connect(&self.database_url, postgres::NoTls)
            .map_err(|error| format!("connecting to Postgres: {error}"))?;
        let rows = client
            .query(
                "SELECT id::text FROM organizations WHERE slug = $1",
                &[&"e2e"],
            )
            .map_err(|error| format!("querying organizations: {error}"))?;
        Ok(rows.first().map(|row| row.get::<_, String>(0)))
    }

    /// Mint an RS256 admin user token for `org_id` bound to `sub`/`email`. Sets
    /// `iss`/`aud`/`iat`/`exp` (the validator enforces issuer/audience/expiry
    /// even though the CLI never inspects them) plus the scopes the user routes
    /// require. The user row is auto-created from these claims on first request.
    pub fn mint_jwt(&self, org_id: &str, sub: &str, email: &str) -> String {
        #[derive(Serialize)]
        struct Claims<'a> {
            sub: &'a str,
            email: &'a str,
            org_id: &'a str,
            role: &'a str,
            iss: &'a str,
            aud: &'a str,
            scope: &'a str,
            iat: i64,
            exp: i64,
            sid: &'a str,
        }
        let now = now_unix();
        let claims = Claims {
            sub,
            email,
            org_id,
            role: "admin",
            iss: ISSUER,
            aud: AUDIENCE,
            scope: SCOPES,
            iat: now,
            exp: now + 3600,
            sid: sub,
        };
        let mut header = Header::new(ALG);
        header.kid = Some(KID.to_owned());
        let key = EncodingKey::from_rsa_pem(SIGNING_KEY_PEM).expect("loading test signing key");
        encode(&header, &claims, &key).expect("encoding JWT")
    }

    /// Full bootstrap: wait healthy, read org id, mint a token for a **unique**
    /// per-process identity, lay down an isolated `$HOME` with `blue.toml` + a
    /// valid `session.json`, and warm up the user row.
    ///
    /// The unique identity matters under `cargo-nextest`: tests run as parallel
    /// processes, and control-api auto-creates the user on first authenticated
    /// request. A shared email would make those concurrent first-touches race to
    /// `INSERT` one `(org, email)` row and 409 with "record already exists". A
    /// distinct identity per process sidesteps that, and the warmup then creates
    /// the row once (serially) before any in-process concurrency — e.g. the
    /// session-upload test's detached workers.
    pub fn bootstrap_home(&self) -> Home {
        self.bootstrap_home_with(true)
    }

    /// Like [`Stack::bootstrap_home`], but lets the caller choose whether the
    /// written `blue.toml` forces governance-only mode. The gateway suite needs
    /// `force_governance_only = false` so `ensure_gateway_access` runs and the
    /// server can hand back a personalized gateway config; every other test
    /// keeps the governance-only default (no gateway is even up).
    pub fn bootstrap_home_with(&self, governance_only: bool) -> Home {
        self.bootstrap_home_pinned_with(governance_only, None)
    }

    /// Governance-only bootstrap that pins agent-binary selection to `bin_dir`
    /// (prepended to `PATH` for every `blue` invocation). Used by the version
    /// matrix so each `(agent, version)` cell drives the exact CLI build it
    /// installed; `None` behaves exactly like [`Stack::bootstrap_home`].
    pub fn bootstrap_home_pinned(&self, bin_dir: Option<PathBuf>) -> Home {
        self.bootstrap_home_pinned_with(true, bin_dir)
    }

    /// Shared bootstrap: choose governance-only mode and, optionally, a bin dir
    /// to prepend to `PATH` so a specific installed agent build is selected.
    fn bootstrap_home_pinned_with(
        &self,
        governance_only: bool,
        path_prepend: Option<PathBuf>,
    ) -> Home {
        self.wait_healthy();
        let org_id = self.org_id();
        let unique = uuid::Uuid::new_v4();
        let sub = format!("e2e-slim-{unique}");
        let email = format!("e2e-slim-{unique}@blue.test");
        let jwt = self.mint_jwt(&org_id, &sub, &email);
        let home = Home::create(
            self,
            &jwt,
            &org_id,
            &sub,
            &email,
            governance_only,
            path_prepend,
        );
        home.ensure_user();
        self.ensure_backing_session(&sub, &email);
        home
    }

    /// Revoke every bearer token issued to `email` before now, the way an
    /// administrator's "revoke sessions" action does (`users.tokens_valid_after`).
    /// The stored `session.json` keeps refreshing happily afterwards — which is
    /// exactly the state in which the CLI used to insist it was logged in.
    pub fn revoke_user_tokens(&self, email: &str) {
        let mut client = postgres::Client::connect(&self.database_url, postgres::NoTls)
            .expect("connecting to Postgres to revoke user tokens");
        let updated = client
            .execute(
                "UPDATE users SET tokens_valid_after=now(),updated_at=now() WHERE lower(email)=lower($1)",
                &[&email],
            )
            .expect("revoking user tokens");
        assert_eq!(updated, 1, "expected exactly one user row for {email}");
    }

    /// Expire the Better Auth browser session the CLI grant is bound to,
    /// leaving the OAuth refresh token untouched. This is the production
    /// failure: the browser session lives 12 hours, the refresh token 30 days,
    /// so a CLI that only checks its own token believes it is still signed in.
    ///
    /// Only observable in gateway mode — `personalize_gateway_config` is where
    /// the binding is read, and a governance-only deployment never gets there.
    pub fn expire_backing_session(&self, sub: &str) {
        let mut client = postgres::Client::connect(&self.database_url, postgres::NoTls)
            .expect("connecting to Postgres to expire the backing session");
        let updated = client
            .execute(
                "UPDATE auth.\"session\" SET \"expiresAt\"=now() - interval '1 minute',\"updatedAt\"=now() WHERE id=$1",
                &[&sub],
            )
            .expect("expiring the backing Better Auth session");
        assert_eq!(updated, 1, "expected exactly one backing session for {sub}");
    }

    /// Seed the Better Auth user + browser session the CLI's grant is bound to.
    ///
    /// **Load-bearing, not scaffolding.** control-api reads
    /// `auth."session"` (matched on the token's `sid`, which [`Stack::mint_jwt`]
    /// sets to `sub`) before it will mint a gateway inference token. Delete this
    /// and the gateway suite starts 401ing with no obvious cause.
    fn ensure_backing_session(&self, sub: &str, email: &str) {
        let mut client = postgres::Client::connect(&self.database_url, postgres::NoTls)
            .expect("connecting to Postgres for backing OAuth session");
        client
            .execute(
                "INSERT INTO auth.\"user\"(id,name,email,\"emailVerified\",\"updatedAt\") VALUES($1,$2,$3,true,now()) ON CONFLICT(id) DO NOTHING",
                &[&sub, &email, &email],
            )
            .expect("creating backing Better Auth user");
        let session_token = format!("e2e-session-token-{sub}");
        client
            .execute(
                "INSERT INTO auth.\"session\"(id,\"expiresAt\",token,\"updatedAt\",\"userId\") VALUES($1,now() + interval '1 hour',$2,now(),$3) ON CONFLICT(id) DO UPDATE SET \"expiresAt\"=excluded.\"expiresAt\",\"updatedAt\"=now()",
                &[&sub, &session_token, &sub],
            )
            .expect("creating backing Better Auth session");
    }
}

/// An isolated `$HOME` for one test process. Holds the tempdir alive; drop
/// cleans it up.
pub struct Home {
    profile: platform::TestProfile,
    blue_bin: PathBuf,
    control_api_url: String,
    minio_url: String,
    bearer: String,
    sub: String,
    email: String,
    /// When set, prepended to `PATH` for every `blue` invocation so a specific
    /// installed agent build is version-selected (matrix cells). `None` inherits
    /// the ambient `PATH` unchanged.
    path_prepend: Option<PathBuf>,
    http: reqwest::blocking::Client,
}

impl Home {
    #[allow(clippy::too_many_arguments)]
    fn create(
        stack: &Stack,
        jwt: &str,
        org_id: &str,
        sub: &str,
        email: &str,
        governance_only: bool,
        path_prepend: Option<PathBuf>,
    ) -> Home {
        let profile = platform::TestProfile::create();
        let blue_dir = profile.paths.config.clone();
        std::fs::create_dir_all(&blue_dir).expect("creating ~/.config/blue");

        // blue.toml: reach the control-api over HTTP, authenticate with a static
        // token (the minted JWT, via env ref), and set the operating mode. The
        // governance-only tests force governance-only so the CLI never tries to
        // reach a gateway; the gateway suite clears that flag so the server can
        // enable gateway mode (the client may only ever downgrade, never enable).
        // This mirrors what `blue setup` / `blue login` would persist — see the
        // README "Pitfalls".
        let blue_toml = format!(
            "[service]\n\
             url = \"{url}\"\n\n\
             [identity]\n\
             mode = \"token\"\n\
             token = \"env://E2E_SLIM_BEARER\"\n\n\
             [mode]\n\
             force_governance_only = {governance_only}\n",
            url = stack.control_api_url,
        );
        std::fs::write(blue_dir.join("blue.toml"), blue_toml).expect("writing blue.toml");

        // session.json: a valid, non-expired session so `blue login` short-
        // circuits to "Already logged in" and every API call carries the JWT.
        let expires_at = now_unix() + 3600;
        let session = serde_json::json!({
            "token": jwt,
            "email": email,
            "org_id": org_id,
            "expires_at": expires_at,
        });
        write_private(
            &blue_dir.join("session.json"),
            serde_json::to_vec_pretty(&session)
                .expect("serializing session.json")
                .as_slice(),
        );

        Home {
            profile,
            blue_bin: stack.blue_bin.clone(),
            control_api_url: stack.control_api_url.clone(),
            minio_url: stack.minio_url.clone(),
            bearer: jwt.to_owned(),
            sub: sub.to_owned(),
            email: email.to_owned(),
            path_prepend,
            http: stack.http.clone(),
        }
    }

    /// The isolated HOME directory root.
    pub fn path(&self) -> &Path {
        &self.profile.paths.profile
    }

    pub fn config_path(&self) -> &Path {
        &self.profile.paths.config
    }
    pub fn data_path(&self) -> &Path {
        &self.profile.paths.data
    }
    pub fn cache_path(&self) -> &Path {
        &self.profile.paths.cache
    }
    pub fn scratch_path(&self) -> &Path {
        self.profile.scratch()
    }
    pub fn marker_path(&self) -> PathBuf {
        self.profile.markers()
    }

    /// The unique per-process user email baked into this HOME's token. The
    /// gateway suite uses it to pre-create the matching LiteLLM user.
    pub fn email(&self) -> &str {
        &self.email
    }

    /// The unique per-process subject. Also the id of the backing Better Auth
    /// session, because [`Stack::mint_jwt`] sets `sid` to `sub`.
    pub fn sub(&self) -> &str {
        &self.sub
    }

    /// The path to this HOME's `session.json`.
    pub fn session_path(&self) -> PathBuf {
        self.config_path().join("session.json")
    }

    /// A `blue` command bound to this isolated HOME. PATH is inherited (so the
    /// suite's installed agent CLIs resolve); only HOME/XDG and the bearer env
    /// are overridden. When this HOME pins an agent build, its bin dir is
    /// prepended to `PATH` so `blue`'s `PATH`-scan version selection resolves that
    /// exact build — for both `blue agent` (eligibility) and `blue run` (launch).
    pub fn blue(&self) -> assert_cmd::Command {
        let mut command = assert_cmd::Command::new(&self.blue_bin);
        self.profile.configure(&mut command);
        command.env("E2E_SLIM_BEARER", &self.bearer);
        if let Some(dir) = &self.path_prepend {
            let inherited = std::env::var_os("PATH").unwrap_or_default();
            let mut entries = vec![dir.clone()];
            entries.extend(std::env::split_paths(&inherited));
            let path = std::env::join_paths(entries).expect("joining pinned PATH");
            command.env("PATH", path);
        }
        command
    }

    /// Select `name` as the default agent. Returns whether the agent is eligible
    /// (installed + allowed). When the CLI reports it is not eligible — e.g. the
    /// real agent binary was not installed (`E2E_SLIM_INSTALL_AGENTS` unset) —
    /// the caller should skip, keeping the suite runnable without agent CLIs.
    pub fn select_agent(&self, name: &str) -> AgentSelection {
        let output = self
            .blue()
            .args(["agent", name])
            .output()
            .expect("running `blue agent`");
        if output.status.success() {
            return AgentSelection::Selected;
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("not eligible") {
            assert!(
                !required(),
                "required agent {name} is not eligible: {stderr}"
            );
            AgentSelection::NotEligible
        } else {
            panic!(
                "`blue agent {name}` failed unexpectedly:\nstdout: {}\nstderr: {stderr}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }

    /// Run `blue run <name> -- <args>` in this HOME and return the raw output.
    /// Used by the gateway suite to launch the real, locked agent headlessly; a
    /// generous timeout absorbs a real (network) inference round-trip.
    pub fn run_agent(&self, name: &str, args: &[String]) -> std::process::Output {
        let mut command = self.blue();
        command.current_dir(self.scratch_path());
        command.arg("run").arg(name).arg("--").args(args);
        command.timeout(std::time::Duration::from_secs(240));
        command.output().expect("running `blue run`")
    }

    fn api(&self, method: reqwest::Method, path: &str) -> reqwest::blocking::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.control_api_url))
            .bearer_auth(&self.bearer)
    }

    /// Create the backing user row once, serially, before any in-process
    /// concurrency. control-api auto-creates the user on first authenticated
    /// request (`upsert_principal`); calling `/auth/me` here does that once so
    /// later concurrent requests (e.g. the session-upload workers) never race to
    /// insert it. Retries briefly to absorb JWKS-cache warmup on a cold server.
    pub fn ensure_user(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let (status, body) = self.auth_me();
            if status.is_success() {
                return;
            }
            if Instant::now() >= deadline {
                panic!("warmup /auth/me never succeeded: status {status}, body {body}");
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// `GET /auth/me` — proves end-to-end JWKS verification.
    pub fn auth_me(&self) -> (reqwest::StatusCode, serde_json::Value) {
        let response = self
            .api(reqwest::Method::GET, "/auth/me")
            .send()
            .expect("GET /auth/me");
        let status = response.status();
        let body = response.json().unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    /// `GET /session-uploads?per_page=<n>` — returns the paginated list body.
    pub fn list_session_uploads(&self, per_page: u32) -> serde_json::Value {
        self.api(
            reqwest::Method::GET,
            &format!("/session-uploads?per_page={per_page}"),
        )
        .send()
        .expect("GET /session-uploads")
        .error_for_status()
        .expect("session-uploads list rejected")
        .json()
        .expect("decoding session-uploads list")
    }

    /// `GET /session-uploads/:id` — returns the detail body (`session`, `artifacts`).
    pub fn get_upload(&self, id: &str) -> serde_json::Value {
        self.api(reqwest::Method::GET, &format!("/session-uploads/{id}"))
            .send()
            .expect("GET /session-uploads/:id")
            .error_for_status()
            .expect("session-upload detail rejected")
            .json()
            .expect("decoding session-upload detail")
    }

    /// Download the completed artifact bytes for `id`: `POST` to mint a presigned
    /// GET, then fetch it (the URL points at the host-published MinIO endpoint).
    pub fn download_upload(&self, id: &str) -> Vec<u8> {
        let signed: serde_json::Value = self
            .api(
                reqwest::Method::POST,
                &format!("/session-uploads/{id}/download"),
            )
            .send()
            .expect("POST /session-uploads/:id/download")
            .error_for_status()
            .expect("session-upload download rejected")
            .json()
            .expect("decoding download response");
        let url = signed
            .get("download_url")
            .and_then(serde_json::Value::as_str)
            .expect("download response has no download_url");
        self.http
            .get(url)
            .send()
            .expect("fetching presigned session artifact")
            .error_for_status()
            .expect("blob storage rejected the presigned GET")
            .bytes()
            .expect("reading session artifact bytes")
            .to_vec()
    }

    /// Host-published MinIO endpoint (for diagnostics/round-trip assertions).
    pub fn minio_url(&self) -> &str {
        &self.minio_url
    }
}

/// Result of selecting a default agent.
#[derive(Debug, PartialEq, Eq)]
pub enum AgentSelection {
    /// The agent is installed and allowed; it is now the default.
    Selected,
    /// The agent CLI is not installed/allowed; the caller should skip.
    NotEligible,
}

/// Env var carrying the version-matrix manifest path, written by `run.sh` when
/// `E2E_SLIM_MATRIX=1`. Absent (the default) ⇒ [`AgentMatrix::from_env`] returns
/// `None` and every matrix-only case self-skips.
pub const AGENT_MATRIX_ENV: &str = "E2E_SLIM_AGENT_MATRIX";

/// The per-`(agent, version)` install locations produced by `run.sh`'s matrix
/// install, loaded from the `E2E_SLIM_AGENT_MATRIX` manifest. The manifest shape
/// is `{ "<agent>": { "<version>": "<abs bin dir>" } }`; each bin dir is prepended
/// to `PATH` so `blue` version-selects that exact CLI build.
pub struct AgentMatrix {
    manifest: serde_json::Value,
}

impl AgentMatrix {
    /// Load the manifest named by `E2E_SLIM_AGENT_MATRIX`, or `None` when the env
    /// is unset/empty (matrix off) so callers self-skip.
    pub fn from_env() -> Option<AgentMatrix> {
        let path = required_env(AGENT_MATRIX_ENV)?;
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("reading agent matrix manifest {path}: {error}"));
        let manifest = serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("parsing agent matrix manifest {path}: {error}"));
        Some(AgentMatrix { manifest })
    }

    /// Absolute bin dir the `(agent, version)` cell was installed into, or `None`
    /// when the manifest has no such cell (that cell then self-skips).
    pub fn bin_dir(&self, agent: &str, version: &str) -> Option<PathBuf> {
        let result = self
            .manifest
            .pointer(&format!("/{agent}/{version}"))
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from);
        assert!(
            !required() || result.is_some(),
            "missing required matrix cell {agent} {version}"
        );
        result
    }
}

fn write_private(path: &Path, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new();
    file.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        file.mode(0o600);
    }
    let mut handle = file.open(path).expect("creating private file");
    handle.write_all(bytes).expect("writing private file");
}

/// A parsed `blue-session-bundle-v1` upload artifact: the manifest plus every
/// archived body keyed by its archive path (`files/NNNN-<name>`).
///
/// Session uploads are portable bundles (gzip tar with a `manifest.json`), not
/// raw transcript bytes, so tests unpack them before asserting on the captured
/// artifacts. Parsed leniently on purpose — the bundle contract itself is
/// enforced by the client's own validator; tests only need the contents.
pub struct SessionBundle {
    pub manifest: serde_json::Value,
    pub files: std::collections::BTreeMap<String, Vec<u8>>,
}

impl SessionBundle {
    /// The archived body of the manifest entry with `role`, if present.
    pub fn file_by_role(&self, role: &str) -> Option<&[u8]> {
        self.manifest
            .get("files")
            .and_then(serde_json::Value::as_array)?
            .iter()
            .find(|file| file.get("role").and_then(serde_json::Value::as_str) == Some(role))
            .and_then(|file| file.get("path").and_then(serde_json::Value::as_str))
            .and_then(|path| self.files.get(path))
            .map(Vec::as_slice)
    }

    /// Whether any archived body contains `needle` as a byte substring.
    pub fn any_file_contains(&self, needle: &[u8]) -> bool {
        !needle.is_empty()
            && self
                .files
                .values()
                .any(|body| body.windows(needle.len()).any(|window| window == needle))
    }
}

/// Unpack a downloaded session-upload artifact (`.bundle.tgz`). Panics on a
/// malformed archive — a corrupt upload is a test failure, not a skip.
pub fn parse_session_bundle(bytes: &[u8]) -> SessionBundle {
    use std::io::Read as _;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(std::io::Cursor::new(bytes)));
    let mut files = std::collections::BTreeMap::new();
    for entry in archive.entries().expect("reading session bundle archive") {
        let mut entry = entry.expect("reading session bundle entry");
        let path = entry
            .path()
            .expect("session bundle entry path")
            .to_string_lossy()
            .into_owned();
        let mut body = Vec::new();
        entry
            .read_to_end(&mut body)
            .expect("reading session bundle entry body");
        files.insert(path, body);
    }
    let manifest = files
        .remove("manifest.json")
        .expect("session bundle has no manifest.json");
    SessionBundle {
        manifest: serde_json::from_slice(&manifest).expect("parsing session bundle manifest"),
        files,
    }
}

/// Poll `condition` until it returns `Some`, up to `timeout`. Panics with
/// `label` on timeout. Used by tests to await the detached upload worker.
pub fn wait_for<T>(label: &str, timeout: Duration, mut condition: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = condition() {
            return value;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {label}");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Handle to the OPT-IN gateway stack (real LiteLLM fed by OpenRouter + a real
/// inference-proxy), layered on top of the base [`Stack`]. Resolved only when the
/// gateway env (set by `run.sh` when `OPENROUTER_API_KEY` is present) is exported;
/// otherwise every gateway test self-skips and the secret-free slim path is
/// unaffected.
pub struct GatewayStack {
    /// The underlying control-api stack.
    pub stack: Stack,
    /// Host-published LiteLLM base URL (for `/user/list` + `/spend/logs`).
    pub litellm_url: String,
    /// LiteLLM master key (admin auth for the calls above).
    pub litellm_master_key: String,
    /// Host-published inference-proxy URL (diagnostics only; the proxy URL the
    /// agent actually uses is injected server-side into the delivered config).
    pub inference_proxy_url: String,
    http: reqwest::blocking::Client,
}

/// Resolve the gateway stack from the environment, or `None` to skip. Mirrors
/// [`env_or_skip`] but additionally requires the LiteLLM coordinates `run.sh`
/// only exports in gateway mode, so the governance-only path never trips it.
pub fn gateway_env_or_skip() -> Option<GatewayStack> {
    let litellm_url = required_env("E2E_SLIM_LITELLM_URL")?
        .trim_end_matches('/')
        .to_owned();
    let litellm_master_key = required_env("E2E_SLIM_LITELLM_MASTER_KEY")?;
    let inference_proxy_url = required_env("E2E_SLIM_INFERENCE_PROXY_URL")
        .unwrap_or_else(|| "http://127.0.0.1:8081".to_owned())
        .trim_end_matches('/')
        .to_owned();
    let stack = env_or_skip()?;
    let http = stack.http.clone();
    Some(GatewayStack {
        stack,
        litellm_url,
        litellm_master_key,
        inference_proxy_url,
        http,
    })
}

impl GatewayStack {
    /// Bootstrap a HOME with `force_governance_only = false`, so `blue run` lets
    /// the server enable gateway mode and hand back a personalized gateway config.
    pub fn bootstrap_gateway_home(&self) -> Home {
        self.stack.bootstrap_home_with(false)
    }

    /// Like [`GatewayStack::bootstrap_gateway_home`], but pins agent-binary
    /// selection to `bin_dir` (prepended to `PATH`). Used by the certification
    /// version matrix so each cell certifies the exact CLI build it installed;
    /// `None` behaves exactly like [`GatewayStack::bootstrap_gateway_home`].
    pub fn bootstrap_gateway_home_pinned(&self, bin_dir: Option<PathBuf>) -> Home {
        self.stack.bootstrap_home_pinned_with(false, bin_dir)
    }

    fn litellm(&self, method: reqwest::Method, path: &str) -> reqwest::blocking::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.litellm_url))
            .bearer_auth(&self.litellm_master_key)
    }

    /// Look up the LiteLLM user id for `email`, or `None` when absent.
    fn litellm_user_id(&self, email: &str) -> Option<String> {
        let response = self
            .litellm(
                reqwest::Method::GET,
                &format!("/user/list?user_email={email}&page_size=100"),
            )
            .send()
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body = response.json::<serde_json::Value>().ok()?;
        let users = body
            .get("users")
            .and_then(serde_json::Value::as_array)
            .or_else(|| body.as_array())
            .cloned()
            .unwrap_or_default();
        users.iter().find_map(|user| {
            user.get("user_email")
                .and_then(serde_json::Value::as_str)
                .filter(|value| value.eq_ignore_ascii_case(email))?;
            user.get("user_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
    }

    /// Poll LiteLLM's spend logs until an entry lands for the user behind
    /// `email`, and return it. Each test runs as its own unique user, so the
    /// entry's `user` field (the LiteLLM user id the virtual key was minted for)
    /// correlates the entry to that test's inference — model names are shared
    /// across agents and cannot. Callers assert the entry's `model` (LiteLLM
    /// records the resolved upstream model here, not the requested name) and
    /// that `api_key` is a hashed virtual key (never the inference JWT the
    /// agent sent) — proof the proxy swapped client authentication for a real key.
    pub fn spend_log_for_user(&self, email: &str) -> serde_json::Value {
        let user_id = self.litellm_user_id(email).unwrap_or_else(|| {
            panic!("no LiteLLM user for {email} (the executable provisioner creates it during `blue apply`/`blue run`)")
        });
        wait_for(
            &format!("LiteLLM spend log for user `{email}`"),
            Duration::from_secs(120),
            || {
                let response = self
                    .litellm(reqwest::Method::GET, "/spend/logs")
                    .send()
                    .ok()?;
                if !response.status().is_success() {
                    return None;
                }
                let body = response.json::<serde_json::Value>().ok()?;
                let entries = body
                    .as_array()
                    .cloned()
                    .or_else(|| {
                        body.get("data")
                            .and_then(serde_json::Value::as_array)
                            .cloned()
                    })
                    .unwrap_or_default();
                entries.into_iter().rev().find(|entry| {
                    entry.get("user").and_then(serde_json::Value::as_str) == Some(user_id.as_str())
                })
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_mode_rejects_missing_endpoints() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::required_mode_child", "--nocapture"])
            .env("E2E_SLIM_REQUIRED", "1")
            .env("E2E_SLIM_REQUIRED_CHILD", "1")
            .env_remove("E2E_SLIM_CONTROL_API_URL")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("missing required environment variable E2E_SLIM_CONTROL_API_URL"));
    }

    #[test]
    fn required_mode_child() {
        if std::env::var("E2E_SLIM_REQUIRED_CHILD").as_deref() == Ok("1") {
            let _ = env_or_skip();
        }
    }

    #[test]
    fn parses_session_bundles() {
        let manifest = serde_json::json!({
            "harness": "codex",
            "native_session_id": "s1",
            "files": [{"role": "rollout", "path": "files/0000-s1.jsonl"}],
        });
        let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        for (path, body) in [
            ("manifest.json", manifest.to_string().into_bytes()),
            (
                "files/0000-s1.jsonl",
                b"{\"marker\":\"BLUE_SLIM_OK\"}\n".to_vec(),
            ),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o600);
            header.set_cksum();
            archive
                .append_data(&mut header, path, std::io::Cursor::new(body))
                .unwrap();
        }
        let bytes = archive.into_inner().unwrap().finish().unwrap();

        let bundle = parse_session_bundle(&bytes);
        assert_eq!(
            bundle
                .manifest
                .get("harness")
                .and_then(serde_json::Value::as_str),
            Some("codex")
        );
        assert_eq!(
            bundle.file_by_role("rollout"),
            Some(b"{\"marker\":\"BLUE_SLIM_OK\"}\n".as_slice())
        );
        assert_eq!(bundle.file_by_role("primary_transcript"), None);
        assert!(bundle.any_file_contains(b"BLUE_SLIM_OK"));
        assert!(!bundle.any_file_contains(b"BLUE_E2E_INVOCATION_missing"));
        assert!(!bundle.any_file_contains(b""));
    }
}
