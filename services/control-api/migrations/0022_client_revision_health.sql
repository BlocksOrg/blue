ALTER TABLE public.client_status
    ADD COLUMN IF NOT EXISTS architecture TEXT,
    ADD COLUMN IF NOT EXISTS revision_matches BOOLEAN,
    ADD COLUMN IF NOT EXISTS status_reasons JSONB NOT NULL DEFAULT '[]'::jsonb;

-- Existing reports did not persist a revision-relative result. Preserve their
-- observable health as a conservative baseline until the client reports again
-- and the control API can compare it with the immutable historical revision.
UPDATE public.client_status
SET revision_matches = CASE
        WHEN config_revision IS NULL THEN NULL
        ELSE applied
            AND files_ok
            AND (error IS NULL OR error = 'configuration is not current')
            AND NOT EXISTS (
                SELECT 1
                FROM jsonb_array_elements(
                    CASE WHEN jsonb_typeof(packages) = 'array' THEN packages ELSE '[]'::jsonb END
                ) package
                WHERE coalesce(package->>'state', 'unknown') <> 'applied'
            )
            AND NOT EXISTS (
                SELECT 1
                FROM jsonb_array_elements(
                    CASE WHEN jsonb_typeof(harnesses) = 'array' THEN harnesses ELSE '[]'::jsonb END
                ) harness
                WHERE jsonb_typeof(harness) = 'object'
                  AND nullif(harness->>'compatibility_error', '') IS NOT NULL
            )
    END,
    status_reasons = CASE
        WHEN config_revision IS NOT NULL AND (
            NOT applied OR NOT files_ok
            OR (error IS NOT NULL AND error <> 'configuration is not current')
            OR EXISTS (
                SELECT 1
                FROM jsonb_array_elements(
                    CASE WHEN jsonb_typeof(packages) = 'array' THEN packages ELSE '[]'::jsonb END
                ) package
                WHERE coalesce(package->>'state', 'unknown') <> 'applied'
            )
            OR EXISTS (
                SELECT 1
                FROM jsonb_array_elements(
                    CASE WHEN jsonb_typeof(harnesses) = 'array' THEN harnesses ELSE '[]'::jsonb END
                ) harness
                WHERE jsonb_typeof(harness) = 'object'
                  AND nullif(harness->>'compatibility_error', '') IS NOT NULL
            )
        ) THEN '["Client report indicates local revision drift; run harness status for details."]'::jsonb
        ELSE '[]'::jsonb
    END;
