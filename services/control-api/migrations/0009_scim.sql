ALTER TABLE users
    ADD COLUMN provisioning_source text NOT NULL DEFAULT 'local'
        CHECK (provisioning_source IN ('local', 'scim'));

CREATE TABLE scim_user_resources (
    user_id uuid PRIMARY KEY REFERENCES users(id),
    organization_id uuid NOT NULL REFERENCES organizations(id),
    external_id text,
    user_name text NOT NULL,
    given_name text,
    family_name text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX scim_users_org_user_name_uidx ON scim_user_resources(organization_id, lower(user_name));
CREATE UNIQUE INDEX scim_users_org_external_id_uidx ON scim_user_resources(organization_id, external_id) WHERE external_id IS NOT NULL;

CREATE TABLE scim_groups (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations(id),
    external_id text,
    display_name text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX scim_groups_org_display_name_uidx ON scim_groups(organization_id, display_name);
CREATE UNIQUE INDEX scim_groups_org_external_id_uidx ON scim_groups(organization_id, external_id) WHERE external_id IS NOT NULL;

CREATE TABLE scim_group_members (
    group_id uuid NOT NULL REFERENCES scim_groups(id) ON DELETE CASCADE,
    user_id uuid NOT NULL REFERENCES users(id),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (group_id, user_id)
);
CREATE INDEX scim_group_members_user_idx ON scim_group_members(user_id);

COMMENT ON COLUMN users.provisioning_source IS
    'Authority for lifecycle and role changes. SCIM users are read-only in the dashboard.';
