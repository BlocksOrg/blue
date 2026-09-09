ALTER TABLE public.gateway_key_selections
    ADD COLUMN IF NOT EXISTS credential_version uuid NOT NULL DEFAULT gen_random_uuid();

CREATE TABLE IF NOT EXISTS public.gateway_cache_events (
    id bigserial PRIMARY KEY,
    user_id uuid NOT NULL,
    pseudotoken_hash text,
    credential_version uuid,
    reason text NOT NULL CHECK (reason IN ('credential_changed', 'user_status_changed', 'selection_deleted')),
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS gateway_cache_events_created_idx
    ON public.gateway_cache_events(created_at);

CREATE OR REPLACE FUNCTION public.publish_gateway_selection_event()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    event_id bigint;
BEGIN
    IF TG_OP = 'DELETE' THEN
        INSERT INTO public.gateway_cache_events(user_id, pseudotoken_hash, credential_version, reason)
        VALUES (OLD.user_id, OLD.pseudotoken_hash, OLD.credential_version, 'selection_deleted')
        RETURNING id INTO event_id;
        PERFORM pg_notify('gateway_cache_events', event_id::text);
        RETURN OLD;
    END IF;
    INSERT INTO public.gateway_cache_events(user_id, pseudotoken_hash, credential_version, reason)
    VALUES (NEW.user_id, NEW.pseudotoken_hash, NEW.credential_version, 'credential_changed')
    RETURNING id INTO event_id;
    PERFORM pg_notify('gateway_cache_events', event_id::text);
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS gateway_selection_cache_event ON public.gateway_key_selections;
CREATE TRIGGER gateway_selection_cache_event
AFTER INSERT OR UPDATE OF pseudotoken_hash, credential_ciphertext, credential_expires_at OR DELETE
ON public.gateway_key_selections
FOR EACH ROW EXECUTE FUNCTION public.publish_gateway_selection_event();

CREATE OR REPLACE FUNCTION public.publish_gateway_user_event()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    event_id bigint;
BEGIN
    IF OLD.active IS DISTINCT FROM NEW.active THEN
        INSERT INTO public.gateway_cache_events(user_id, pseudotoken_hash, credential_version, reason)
        SELECT NEW.id, pseudotoken_hash, credential_version, 'user_status_changed'
        FROM public.gateway_key_selections WHERE user_id = NEW.id
        RETURNING id INTO event_id;
        IF event_id IS NOT NULL THEN
            PERFORM pg_notify('gateway_cache_events', event_id::text);
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS gateway_user_cache_event ON public.users;
CREATE TRIGGER gateway_user_cache_event
AFTER UPDATE OF active ON public.users
FOR EACH ROW EXECUTE FUNCTION public.publish_gateway_user_event();
