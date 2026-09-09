-- Migration 0002 selects the Better Auth schema for its connection. SQLx may
-- reuse that connection for later migrations, so unqualified Control API
-- tables created after it can land in auth. Move every affected table to the
-- public application schema and restore the connection default for future
-- migrations. The guards make this safe for databases where the tables were
-- already created in public.
DO $$
DECLARE
    table_name text;
BEGIN
    FOREACH table_name IN ARRAY ARRAY[
        'scim_user_resources',
        'scim_groups',
        'scim_group_members',
        'package_artifacts',
        'deployment_governance_state',
        'governance_package_audiences',
        'governance_package_audience_users',
        'deployment_branding',
        'gateway_request_logs'
    ]
    LOOP
        IF to_regclass(format('auth.%I', table_name)) IS NOT NULL
           AND to_regclass(format('public.%I', table_name)) IS NULL THEN
            EXECUTE format('ALTER TABLE auth.%I SET SCHEMA public', table_name);
        END IF;
    END LOOP;
END $$;

RESET search_path;
