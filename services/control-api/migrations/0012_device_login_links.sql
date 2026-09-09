ALTER TABLE auth."deviceCode"
    ADD COLUMN "browserToken" text;

CREATE UNIQUE INDEX device_code_browser_token_uidx
    ON auth."deviceCode" ("browserToken")
    WHERE "browserToken" IS NOT NULL;
