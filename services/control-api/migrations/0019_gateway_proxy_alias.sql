ALTER TABLE public.gateway_key_selections
    ADD COLUMN proxy_key_alias text;

COMMENT ON COLUMN public.gateway_key_selections.proxy_key_alias IS
    'Deterministic blue:<normalized-email> alias of the server-held LiteLLM proxy key.';
