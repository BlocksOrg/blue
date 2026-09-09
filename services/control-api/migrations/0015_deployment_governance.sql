ALTER TABLE governance_config_revisions
    ADD COLUMN origin text NOT NULL DEFAULT 'dashboard'
    CHECK (origin IN ('bootstrap', 'deployment', 'dashboard'));

-- The oldest revision for each organization is the pre-existing bootstrap
-- seed. Later historical rows were produced by dashboard operations.
UPDATE governance_config_revisions revision
SET origin = 'bootstrap'
WHERE revision.id = (
    SELECT min(candidate.id)
    FROM governance_config_revisions candidate
    WHERE candidate.organization_id = revision.organization_id
);

CREATE TABLE deployment_governance_state (
    organization_id uuid PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    baseline_document jsonb NOT NULL,
    source_sha256 text NOT NULL CHECK (source_sha256 ~ '^[0-9a-f]{64}$'),
    updated_at timestamptz NOT NULL DEFAULT now()
);
