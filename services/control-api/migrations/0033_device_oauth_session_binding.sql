ALTER TABLE auth."deviceCode"
    ADD COLUMN "blueOAuthSessionId" text
    REFERENCES auth."session" ("id") ON DELETE SET NULL;

CREATE INDEX "deviceCode_blueOAuthSessionId_idx"
    ON auth."deviceCode" ("blueOAuthSessionId")
    WHERE "blueOAuthSessionId" IS NOT NULL;
