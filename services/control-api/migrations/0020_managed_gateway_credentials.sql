ALTER TABLE public.gateway_key_selections
    ALTER COLUMN gateway_user_id DROP NOT NULL,
    ALTER COLUMN source_key_hash DROP NOT NULL,
    ALTER COLUMN pseudotoken DROP NOT NULL,
    ALTER COLUMN proxy_virtual_key DROP NOT NULL,
    ADD COLUMN IF NOT EXISTS pseudotoken_hash text,
    ADD COLUMN IF NOT EXISTS pseudotoken_ciphertext bytea,
    ADD COLUMN IF NOT EXISTS pseudotoken_nonce bytea,
    ADD COLUMN IF NOT EXISTS pseudotoken_wrapped_key bytea,
    ADD COLUMN IF NOT EXISTS credential_ciphertext bytea,
    ADD COLUMN IF NOT EXISTS credential_nonce bytea,
    ADD COLUMN IF NOT EXISTS credential_wrapped_key bytea,
    ADD COLUMN IF NOT EXISTS encryption_key_id text,
    ADD COLUMN IF NOT EXISTS provisioner_config_hash text,
    ADD COLUMN IF NOT EXISTS provisioner_metadata jsonb NOT NULL DEFAULT '{}'::jsonb,
    ADD COLUMN IF NOT EXISTS credential_expires_at timestamptz,
    ADD COLUMN IF NOT EXISTS last_reconciled_at timestamptz,
    ADD COLUMN IF NOT EXISTS provisioning_error text;

CREATE UNIQUE INDEX IF NOT EXISTS gateway_key_selections_pseudotoken_hash_idx
    ON public.gateway_key_selections(pseudotoken_hash) WHERE pseudotoken_hash IS NOT NULL;

COMMENT ON COLUMN public.gateway_key_selections.credential_ciphertext IS
    'AES-256-GCM encrypted gateway credential; plaintext is never persisted.';
COMMENT ON COLUMN public.gateway_key_selections.pseudotoken_hash IS
    'SHA-256 lookup digest for the high-entropy pseudotoken.';
