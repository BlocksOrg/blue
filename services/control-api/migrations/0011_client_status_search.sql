CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE INDEX client_status_instance_id_trgm_idx
    ON public.client_status USING gin (lower(instance_id) gin_trgm_ops);

CREATE INDEX client_status_hostname_trgm_idx
    ON public.client_status USING gin (lower(coalesce(hostname, '')) gin_trgm_ops);

CREATE INDEX users_email_trgm_idx
    ON users USING gin (lower(email) gin_trgm_ops);
