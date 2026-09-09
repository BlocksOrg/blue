CREATE TABLE gateway_request_logs (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    profile_id text,
    profile_name text,
    occurred_at timestamptz NOT NULL,
    method text NOT NULL CHECK (char_length(method) BETWEEN 1 AND 16),
    path text NOT NULL CHECK (char_length(path) BETWEEN 1 AND 2048),
    model text,
    http_status integer CHECK (http_status BETWEEN 100 AND 599),
    result text NOT NULL CHECK (
        result IN ('success', 'redirect', 'client_error', 'server_error', 'transport_error')
    ),
    upstream_latency_ms bigint NOT NULL CHECK (upstream_latency_ms >= 0),
    harness text,
    repository text,
    branch text,
    commit_sha text,
    dirty boolean,
    run_id text,
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX gateway_request_logs_org_occurred_idx
    ON gateway_request_logs(organization_id, occurred_at DESC, id DESC);
CREATE INDEX gateway_request_logs_user_occurred_idx
    ON gateway_request_logs(user_id, occurred_at DESC, id DESC);
CREATE INDEX gateway_request_logs_expiry_idx
    ON gateway_request_logs(expires_at);
CREATE INDEX gateway_request_logs_model_idx
    ON gateway_request_logs(organization_id, model) WHERE model IS NOT NULL;

COMMENT ON TABLE gateway_request_logs IS
    'Metadata-only inference proxy request history. Bodies, query strings, headers, and credentials are never stored.';
