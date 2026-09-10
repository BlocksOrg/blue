-- Gateway authentication is bound to the Better Auth session that authorized
-- the governance response. Inference JWTs are deliberately not persisted.
CREATE TABLE public.gateway_auth_sessions (
    oauth_session_id text PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES public.users(id) ON DELETE CASCADE,
    source_expires_at timestamptz NOT NULL,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX gateway_auth_sessions_user_active_idx
    ON public.gateway_auth_sessions(user_id, source_expires_at)
    WHERE revoked_at IS NULL;

DROP TRIGGER IF EXISTS gateway_selection_cache_event ON public.gateway_key_selections;
DROP TRIGGER IF EXISTS gateway_user_cache_event ON public.users;
DROP FUNCTION IF EXISTS public.publish_gateway_selection_event();
DROP FUNCTION IF EXISTS public.publish_gateway_user_event();

ALTER TABLE public.gateway_cache_events
    ADD COLUMN oauth_session_id text;

ALTER TABLE public.gateway_cache_events
    DROP COLUMN pseudotoken_hash;

ALTER TABLE public.gateway_cache_events
    DROP CONSTRAINT gateway_cache_events_reason_check;

ALTER TABLE public.gateway_cache_events
    ADD CONSTRAINT gateway_cache_events_reason_check CHECK (
        reason IN (
            'credential_changed',
            'user_status_changed',
            'selection_deleted',
            'session_revoked'
        )
    );

CREATE OR REPLACE FUNCTION public.publish_gateway_selection_event()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    event_id bigint;
    affected_user_id uuid;
    affected_version uuid;
    affected_reason text;
BEGIN
    affected_user_id := CASE WHEN TG_OP = 'DELETE' THEN OLD.user_id ELSE NEW.user_id END;
    affected_version := CASE WHEN TG_OP = 'DELETE' THEN OLD.credential_version ELSE NEW.credential_version END;
    affected_reason := CASE WHEN TG_OP = 'DELETE' THEN 'selection_deleted' ELSE 'credential_changed' END;

    INSERT INTO public.gateway_cache_events(user_id, oauth_session_id, credential_version, reason)
    VALUES (affected_user_id, NULL, affected_version, affected_reason)
    RETURNING id INTO event_id;
    PERFORM pg_notify('gateway_cache_events', event_id::text);
    RETURN CASE WHEN TG_OP = 'DELETE' THEN OLD ELSE NEW END;
END;
$$;

CREATE TRIGGER gateway_selection_cache_event
AFTER INSERT OR UPDATE OF credential_ciphertext, credential_expires_at OR DELETE
ON public.gateway_key_selections
FOR EACH ROW EXECUTE FUNCTION public.publish_gateway_selection_event();

CREATE OR REPLACE FUNCTION public.publish_gateway_user_event()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    event_id bigint;
BEGIN
    IF OLD.active IS DISTINCT FROM NEW.active THEN
        IF NEW.active = false THEN
            UPDATE public.gateway_auth_sessions
            SET revoked_at = coalesce(revoked_at, now()), updated_at = now()
            WHERE user_id = NEW.id AND revoked_at IS NULL;
        END IF;
        INSERT INTO public.gateway_cache_events(user_id, oauth_session_id, credential_version, reason)
        SELECT NEW.id, NULL, credential_version, 'user_status_changed'
        FROM public.gateway_key_selections WHERE user_id = NEW.id
        RETURNING id INTO event_id;
        IF event_id IS NOT NULL THEN
            PERFORM pg_notify('gateway_cache_events', event_id::text);
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER gateway_user_cache_event
AFTER UPDATE OF active ON public.users
FOR EACH ROW EXECUTE FUNCTION public.publish_gateway_user_event();

CREATE OR REPLACE FUNCTION public.publish_gateway_auth_session_event()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    event_id bigint;
BEGIN
    IF OLD.revoked_at IS NULL AND NEW.revoked_at IS NOT NULL THEN
        INSERT INTO public.gateway_cache_events(user_id, oauth_session_id, credential_version, reason)
        VALUES (NEW.user_id, NEW.oauth_session_id, NULL, 'session_revoked')
        RETURNING id INTO event_id;
        PERFORM pg_notify('gateway_cache_events', event_id::text);
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER gateway_auth_session_cache_event
AFTER UPDATE OF revoked_at ON public.gateway_auth_sessions
FOR EACH ROW EXECUTE FUNCTION public.publish_gateway_auth_session_event();

CREATE OR REPLACE FUNCTION public.revoke_gateway_auth_session_on_source_delete()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE public.gateway_auth_sessions
    SET revoked_at = coalesce(revoked_at, now()), updated_at = now()
    WHERE oauth_session_id = OLD.id;
    RETURN OLD;
END;
$$;

CREATE TRIGGER gateway_auth_source_session_deleted
AFTER DELETE ON auth."session"
FOR EACH ROW EXECUTE FUNCTION public.revoke_gateway_auth_session_on_source_delete();

DROP INDEX IF EXISTS public.gateway_key_selections_pseudotoken_hash_idx;

ALTER TABLE public.gateway_key_selections
    DROP COLUMN pseudotoken,
    DROP COLUMN pseudotoken_hash,
    DROP COLUMN pseudotoken_ciphertext,
    DROP COLUMN pseudotoken_nonce,
    DROP COLUMN pseudotoken_wrapped_key;

-- This migration removes the previous authentication contract and is not safe
-- for older serving binaries.
UPDATE public.schema_compatibility
SET minimum_migration_version = 32, updated_at = now()
WHERE singleton = true;
