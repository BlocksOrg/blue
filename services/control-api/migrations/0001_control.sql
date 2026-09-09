CREATE TABLE organizations (
    id uuid PRIMARY KEY,
    slug text NOT NULL UNIQUE,
    name text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE users (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations(id),
    subject text NOT NULL,
    email text NOT NULL,
    role text NOT NULL CHECK (role IN ('admin', 'member')),
    active boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (organization_id, subject),
    UNIQUE (organization_id, email)
);

CREATE TABLE login_credentials (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash text NOT NULL UNIQUE,
    label text NOT NULL,
    last_used_at timestamptz,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (user_id, label)
);

CREATE TABLE api_sessions (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash text NOT NULL UNIQUE,
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX api_sessions_user_idx ON api_sessions(user_id, created_at DESC);

CREATE TABLE governance_config_revisions (
    id bigserial PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations(id),
    revision text NOT NULL UNIQUE,
    yaml text NOT NULL,
    document jsonb NOT NULL,
    created_by uuid REFERENCES users(id),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (organization_id, id)
);

CREATE INDEX governance_config_org_revision_idx
    ON governance_config_revisions(organization_id, id DESC);

CREATE TABLE captured_sessions (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations(id),
    user_id uuid NOT NULL REFERENCES users(id),
    harness text NOT NULL CHECK (harness IN ('codex', 'claude', 'kimi', 'opencode')),
    native_session_id text NOT NULL,
    cwd text,
    current_artifact_id uuid,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (user_id, harness, native_session_id)
);

CREATE TABLE session_artifacts (
    id uuid PRIMARY KEY,
    captured_session_id uuid NOT NULL REFERENCES captured_sessions(id) ON DELETE CASCADE,
    object_key text NOT NULL UNIQUE,
    sha256 text NOT NULL,
    size_bytes bigint NOT NULL CHECK (size_bytes >= 0),
    content_type text NOT NULL,
    status text NOT NULL CHECK (status IN ('pending', 'complete', 'superseded', 'failed')),
    upload_expires_at timestamptz NOT NULL,
    retention_expires_at timestamptz,
    completed_at timestamptz,
    failure_reason text,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (captured_session_id, sha256)
);

ALTER TABLE captured_sessions
    ADD CONSTRAINT captured_sessions_current_artifact_fk
    FOREIGN KEY (current_artifact_id) REFERENCES session_artifacts(id);

CREATE INDEX captured_sessions_org_updated_idx
    ON captured_sessions(organization_id, updated_at DESC, id DESC);
CREATE INDEX captured_sessions_user_updated_idx
    ON captured_sessions(user_id, updated_at DESC, id DESC);
CREATE INDEX session_artifacts_session_created_idx
    ON session_artifacts(captured_session_id, created_at DESC);
