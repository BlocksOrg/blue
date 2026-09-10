-- Expand migration: lets a CLI logout be undone by logging back in from the
-- same live browser session, and records when that happened so JWTs minted
-- before the logout stay dead.

ALTER TABLE public.gateway_auth_sessions
    ADD COLUMN reactivated_at timestamptz;

-- Fire on both edges of the revoked/active transition so a reactivation
-- invalidates proxy caches the same way a revocation does.
--
-- The (OLD IS NULL) IS DISTINCT FROM (NEW IS NULL) guard is load-bearing:
-- AFTER UPDATE OF revoked_at fires whenever the column appears in the SET
-- list, changed or not, and the renewal upsert now always sets
-- revoked_at=null. Without the guard every 300s poll would emit a NOTIFY.
CREATE OR REPLACE FUNCTION public.publish_gateway_auth_session_event()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    event_id bigint;
BEGIN
    IF (OLD.revoked_at IS NULL) IS DISTINCT FROM (NEW.revoked_at IS NULL) THEN
        INSERT INTO public.gateway_cache_events(user_id, oauth_session_id, credential_version, reason)
        VALUES (NEW.user_id, NEW.oauth_session_id, NULL, 'session_revoked')
        RETURNING id INTO event_id;
        PERFORM pg_notify('gateway_cache_events', event_id::text);
    END IF;
    RETURN NEW;
END;
$$;
