-- Expand migration: records the oldest application migration generation that
-- may safely use the current schema. Future expand/data migrations leave this
-- value unchanged; a contract migration must raise it to its own version.
CREATE TABLE public.schema_compatibility (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    minimum_migration_version bigint NOT NULL CHECK (minimum_migration_version > 0),
    updated_at timestamptz NOT NULL DEFAULT now()
);

INSERT INTO public.schema_compatibility(singleton, minimum_migration_version)
VALUES (true, 31);
