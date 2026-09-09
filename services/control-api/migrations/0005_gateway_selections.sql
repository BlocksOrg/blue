CREATE TABLE gateway_key_selections (
    user_id uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    gateway_user_id text NOT NULL,
    gateway_email text NOT NULL,
    source_key_hash text NOT NULL,
    source_key_name text,
    source_key_alias text,
    source_team_id text,
    source_models jsonb NOT NULL DEFAULT '[]'::jsonb,
    pseudotoken text NOT NULL UNIQUE,
    proxy_virtual_key text NOT NULL,
    synced_at timestamptz NOT NULL DEFAULT now(),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX gateway_key_selections_email_idx
    ON gateway_key_selections(lower(gateway_email));

COMMENT ON COLUMN gateway_key_selections.proxy_virtual_key IS
    'Server-side LiteLLM virtual key minted for the governance inference proxy; never returned to clients.';
