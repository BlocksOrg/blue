CREATE TABLE public.gateway_model_catalog (
    organization_id uuid NOT NULL REFERENCES public.organizations(id) ON DELETE CASCADE,
    gateway_type text NOT NULL,
    model_id text NOT NULL CHECK (btrim(model_id) <> ''),
    display_name text,
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb,
    fingerprint text NOT NULL,
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    unavailable_since timestamptz,
    PRIMARY KEY (organization_id, gateway_type, model_id)
);

CREATE TABLE public.gateway_model_catalog_sync_state (
    organization_id uuid NOT NULL REFERENCES public.organizations(id) ON DELETE CASCADE,
    gateway_type text NOT NULL,
    last_refresh_at timestamptz,
    last_successful_refresh_at timestamptz,
    last_successful_source_revision text,
    latest_error text,
    latest_error_at timestamptz,
    discovery_supported boolean,
    PRIMARY KEY (organization_id, gateway_type)
);
