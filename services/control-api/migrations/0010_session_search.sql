CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE INDEX captured_sessions_native_id_trgm_idx
    ON captured_sessions USING gin (lower(native_session_id) gin_trgm_ops);

CREATE INDEX captured_sessions_cwd_trgm_idx
    ON captured_sessions USING gin (lower(coalesce(cwd, '')) gin_trgm_ops);
