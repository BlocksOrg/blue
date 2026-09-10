-- Expand migration: binds device authorization codes to the Better Auth
-- session that approved them. Older serving binaries ignore the new column.
ALTER TABLE auth."deviceCode"
    ADD COLUMN "blueOAuthSessionId" text
    REFERENCES auth."session" ("id") ON DELETE SET NULL;

CREATE INDEX "deviceCode_blueOAuthSessionId_idx"
    ON auth."deviceCode" ("blueOAuthSessionId")
    WHERE "blueOAuthSessionId" IS NOT NULL;
