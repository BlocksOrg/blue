ALTER TABLE public.gateway_model_catalog
    ADD COLUMN acknowledged_fingerprint text,
    ADD COLUMN acknowledged_at timestamptz,
    ADD COLUMN acknowledged_by uuid REFERENCES public.users(id) ON DELETE SET NULL;

UPDATE public.gateway_model_catalog
SET acknowledged_fingerprint = fingerprint,
    acknowledged_at = first_seen_at;

ALTER TABLE public.gateway_model_catalog
    ALTER COLUMN acknowledged_fingerprint SET NOT NULL,
    ALTER COLUMN acknowledged_at SET NOT NULL;

CREATE INDEX gateway_model_catalog_availability_idx
    ON public.gateway_model_catalog (organization_id, gateway_type, unavailable_since, model_id);
