-- Migration 0010 installed pg_trgm while migration 0002 still had `auth` on the
-- connection search_path, so the extension (and its `gin_trgm_ops` operator
-- class) can live outside `public`. Migration 0026 later reset the search_path,
-- so the trigram indexes below cannot resolve `gin_trgm_ops` until pg_trgm is
-- relocated. Do it here, before the indexes are created. Migration 0030 repeats
-- this as an idempotent no-op for databases migrated before this fix landed.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM pg_extension extension
        JOIN pg_namespace namespace ON namespace.oid = extension.extnamespace
        WHERE extension.extname = 'pg_trgm' AND namespace.nspname <> 'public'
    ) THEN
        ALTER EXTENSION pg_trgm SET SCHEMA public;
    END IF;
END $$;

ALTER TABLE captured_sessions
    ADD COLUMN artifact_format text NOT NULL DEFAULT 'legacy-raw',
    ADD COLUMN resumable boolean NOT NULL DEFAULT false,
    ADD COLUMN title text,
    ADD COLUMN summary text,
    ADD COLUMN source_captured_at timestamptz,
    ADD COLUMN repository_root text,
    ADD COLUMN repository_remote text,
    ADD COLUMN sharing_mode text NOT NULL DEFAULT 'private'
        CHECK (sharing_mode IN ('private', 'workspace', 'selected'));

CREATE TABLE captured_session_grants (
    captured_session_id uuid NOT NULL REFERENCES captured_sessions(id) ON DELETE CASCADE,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (captured_session_id, user_id)
);

CREATE INDEX captured_session_grants_user_idx
    ON captured_session_grants(user_id, captured_session_id);

CREATE INDEX captured_sessions_resumable_updated_idx
    ON captured_sessions(organization_id, resumable, updated_at DESC, id DESC);

CREATE INDEX captured_sessions_title_trgm_idx
    ON captured_sessions USING gin (lower(coalesce(title, '')) gin_trgm_ops);

CREATE INDEX captured_sessions_summary_trgm_idx
    ON captured_sessions USING gin (lower(coalesce(summary, '')) gin_trgm_ops);
