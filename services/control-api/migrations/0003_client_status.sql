CREATE TABLE client_status (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations(id),
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    instance_id text NOT NULL,
    hostname text,
    client_version text NOT NULL,
    platform text NOT NULL,
    config_revision text,
    applied boolean NOT NULL DEFAULT false,
    files_ok boolean NOT NULL DEFAULT false,
    harnesses jsonb NOT NULL DEFAULT '[]'::jsonb,
    error text,
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (user_id, instance_id)
);

CREATE INDEX client_status_org_seen_idx ON client_status(organization_id, last_seen_at DESC);
