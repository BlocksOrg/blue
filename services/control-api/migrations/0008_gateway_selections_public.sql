DO $$
BEGIN
    IF to_regclass('auth.gateway_key_selections') IS NOT NULL
       AND to_regclass('public.gateway_key_selections') IS NULL THEN
        ALTER TABLE auth.gateway_key_selections SET SCHEMA public;
    END IF;
END $$;

COMMENT ON TABLE public.gateway_key_selections IS
    'Per-user inference gateway selection and server-held proxy credential.';
