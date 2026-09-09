CREATE TABLE IF NOT EXISTS public.gateway_key_revocations (
    id uuid PRIMARY KEY,
    external_id text NOT NULL UNIQUE,
    attempts integer NOT NULL DEFAULT 0,
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    last_error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    completed_at timestamptz
);

CREATE OR REPLACE FUNCTION public.enqueue_gateway_key_revocation()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.source_key_hash IS NOT NULL THEN
        INSERT INTO public.gateway_key_revocations(id, external_id)
        VALUES (gen_random_uuid(), OLD.source_key_hash)
        ON CONFLICT (external_id) DO NOTHING;
    END IF;
    RETURN OLD;
END;
$$;

DROP TRIGGER IF EXISTS gateway_key_selection_revoke ON public.gateway_key_selections;
CREATE TRIGGER gateway_key_selection_revoke
AFTER DELETE ON public.gateway_key_selections
FOR EACH ROW EXECUTE FUNCTION public.enqueue_gateway_key_revocation();

