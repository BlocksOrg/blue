ALTER TABLE public.gateway_key_selections
    ADD COLUMN IF NOT EXISTS credential_state text NOT NULL DEFAULT 'ready'
        CHECK (credential_state IN ('ready', 'invalid', 'recovering', 'error')),
    ADD COLUMN IF NOT EXISTS invalidated_at timestamptz,
    ADD COLUMN IF NOT EXISTS invalidation_reason text,
    ADD COLUMN IF NOT EXISTS recovery_attempts integer NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS next_recovery_at timestamptz,
    ADD COLUMN IF NOT EXISTS recovery_lease_until timestamptz,
    ADD COLUMN IF NOT EXISTS last_validated_at timestamptz;

UPDATE public.gateway_key_selections
SET credential_state = CASE
    WHEN credential_ciphertext IS NOT NULL THEN 'ready'
    WHEN provisioning_error IS NOT NULL THEN 'error'
    ELSE 'invalid'
END;
