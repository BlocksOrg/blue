-- Migration 0010 may have installed pg_trgm outside `public` because migration
-- 0002 changed the connection search path. Keep the relocatable extension in a
-- predictable schema for queries and indexes created by later migrations.
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
