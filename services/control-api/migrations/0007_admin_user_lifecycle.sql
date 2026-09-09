ALTER TABLE users
    ADD COLUMN status text NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'suspended', 'removed')),
    ADD COLUMN protected boolean NOT NULL DEFAULT false,
    ADD COLUMN tokens_valid_after timestamptz;

UPDATE users
SET status = CASE WHEN active THEN 'active' ELSE 'suspended' END;

ALTER TABLE users
    ADD CONSTRAINT users_active_status_consistent
    CHECK (active = (status = 'active'));

CREATE INDEX users_org_status_email_idx
    ON users(organization_id, status, lower(email));

COMMENT ON COLUMN users.status IS
    'Governance lifecycle state. Removed users retain identity and historical ownership but have no authentication account.';
COMMENT ON COLUMN users.protected IS
    'Prevents lifecycle changes to deployment-managed bootstrap identities.';
COMMENT ON COLUMN users.tokens_valid_after IS
    'Bearer tokens issued at or before this instant are rejected by the Control API.';
