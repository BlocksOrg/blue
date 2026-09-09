CREATE TABLE package_artifacts (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations(id),
    connection_id text NOT NULL,
    provider text NOT NULL,
    repository text NOT NULL,
    requested_ref text NOT NULL,
    resolved_commit text NOT NULL,
    source_ref text NOT NULL,
    object_key text NOT NULL,
    sha256 text NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    size_bytes bigint NOT NULL CHECK (size_bytes >= 0),
    created_by uuid REFERENCES users(id),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (organization_id, connection_id, repository, resolved_commit)
);

CREATE INDEX package_artifacts_org_created_idx
    ON package_artifacts(organization_id, created_at DESC);
