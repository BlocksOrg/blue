CREATE TABLE governance_package_audiences (
    revision text NOT NULL REFERENCES governance_config_revisions(revision) ON DELETE CASCADE,
    package_id text NOT NULL,
    scope text NOT NULL CHECK (scope IN ('organization', 'users')),
    PRIMARY KEY (revision, package_id)
);

CREATE TABLE governance_package_audience_users (
    revision text NOT NULL,
    package_id text NOT NULL,
    user_id uuid NOT NULL REFERENCES users(id),
    PRIMARY KEY (revision, package_id, user_id),
    FOREIGN KEY (revision, package_id)
        REFERENCES governance_package_audiences(revision, package_id)
        ON DELETE CASCADE
);

CREATE INDEX governance_package_audience_users_user_idx
    ON governance_package_audience_users(user_id, revision);
