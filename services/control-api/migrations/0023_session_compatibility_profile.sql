ALTER TABLE captured_sessions
    ADD COLUMN compatibility_profile text NOT NULL DEFAULT 'legacy';

UPDATE captured_sessions
SET compatibility_profile = harness || '-v1';

ALTER TABLE captured_sessions
    DROP CONSTRAINT captured_sessions_user_id_harness_native_session_id_key;

ALTER TABLE captured_sessions
    ADD CONSTRAINT captured_sessions_user_harness_profile_native_key
    UNIQUE (user_id, harness, compatibility_profile, native_session_id);
