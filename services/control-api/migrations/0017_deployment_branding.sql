CREATE TABLE deployment_branding (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    logo_url text,
    favicon_url text,
    updated_by uuid REFERENCES users(id) ON DELETE SET NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (logo_url IS NULL OR char_length(logo_url) <= 2048),
    CHECK (favicon_url IS NULL OR char_length(favicon_url) <= 2048)
);

INSERT INTO deployment_branding (singleton) VALUES (true);
