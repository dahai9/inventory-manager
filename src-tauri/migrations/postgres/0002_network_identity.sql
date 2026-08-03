-- Inventory Platform v2, network identity and authorization.
--
-- Authentication identities are tenant scoped. Every relationship between
-- tenant-owned rows includes tenant_id, preventing a valid identifier from a
-- different tenant from being attached accidentally. Passwords and opaque
-- bearer tokens are represented only by one-way hashes.

CREATE TABLE users (
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE RESTRICT,
    id uuid NOT NULL,
    login text NOT NULL CHECK (length(btrim(login)) > 0),
    normalized_login text NOT NULL CHECK (
        length(btrim(normalized_login)) > 0
        AND normalized_login = lower(btrim(normalized_login))
    ),
    display_name text NOT NULL CHECK (length(btrim(display_name)) > 0),
    email text,
    status text NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'disabled')),
    created_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, id),
    UNIQUE (tenant_id, normalized_login),
    UNIQUE (tenant_id, email)
);

CREATE TABLE credentials (
    tenant_id uuid NOT NULL,
    user_id uuid NOT NULL,
    password_hash text NOT NULL CHECK (password_hash LIKE '$argon2id$%'),
    failed_login_count integer NOT NULL DEFAULT 0 CHECK (failed_login_count >= 0),
    locked_until timestamptz,
    password_changed_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_authenticated_at timestamptz,
    updated_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, user_id),
    FOREIGN KEY (tenant_id, user_id)
        REFERENCES users (tenant_id, id) ON DELETE CASCADE
);

CREATE TABLE memberships (
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE RESTRICT,
    id uuid NOT NULL,
    user_id uuid NOT NULL,
    status text NOT NULL DEFAULT 'active'
        CHECK (status IN ('invited', 'active', 'suspended', 'revoked')),
    consumes_license_seat boolean NOT NULL DEFAULT true,
    joined_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, id),
    UNIQUE (tenant_id, user_id),
    UNIQUE (tenant_id, id, user_id),
    FOREIGN KEY (tenant_id, user_id)
        REFERENCES users (tenant_id, id) ON DELETE CASCADE
);

CREATE TABLE roles (
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE RESTRICT,
    id uuid NOT NULL,
    code text NOT NULL CHECK (
        length(btrim(code)) > 0 AND code = lower(btrim(code))
    ),
    name text NOT NULL CHECK (length(btrim(name)) > 0),
    description text,
    active boolean NOT NULL DEFAULT true,
    system_role boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, id),
    UNIQUE (tenant_id, code)
);

CREATE TABLE permissions (
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE RESTRICT,
    id uuid NOT NULL,
    code text NOT NULL CHECK (
        length(btrim(code)) > 0 AND code = lower(btrim(code))
    ),
    description text NOT NULL CHECK (length(btrim(description)) > 0),
    created_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, id),
    UNIQUE (tenant_id, code)
);

-- The architecture names roles and permissions separately; this join is the
-- tenant-safe relation that turns both catalogs into an RBAC graph.
CREATE TABLE role_permissions (
    tenant_id uuid NOT NULL,
    role_id uuid NOT NULL,
    permission_id uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, role_id, permission_id),
    FOREIGN KEY (tenant_id, role_id)
        REFERENCES roles (tenant_id, id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, permission_id)
        REFERENCES permissions (tenant_id, id) ON DELETE CASCADE
);

CREATE TABLE membership_roles (
    tenant_id uuid NOT NULL,
    membership_id uuid NOT NULL,
    role_id uuid NOT NULL,
    assigned_by_user_id uuid,
    assigned_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, membership_id, role_id),
    FOREIGN KEY (tenant_id, membership_id)
        REFERENCES memberships (tenant_id, id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, role_id)
        REFERENCES roles (tenant_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (tenant_id, assigned_by_user_id)
        REFERENCES users (tenant_id, id) ON DELETE RESTRICT
);

CREATE TABLE devices (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    membership_id uuid NOT NULL,
    user_id uuid NOT NULL,
    device_fingerprint text NOT NULL CHECK (length(btrim(device_fingerprint)) > 0),
    display_name text NOT NULL CHECK (length(btrim(display_name)) > 0),
    status text NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'blocked', 'retired')),
    first_seen_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_seen_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, id),
    UNIQUE (tenant_id, device_fingerprint),
    UNIQUE (tenant_id, id, membership_id, user_id),
    FOREIGN KEY (tenant_id, membership_id, user_id)
        REFERENCES memberships (tenant_id, id, user_id) ON DELETE CASCADE
);

CREATE TABLE sessions (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    membership_id uuid NOT NULL,
    user_id uuid NOT NULL,
    device_id uuid NOT NULL,
    token_hash bytea NOT NULL CHECK (octet_length(token_hash) = 32),
    issued_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at timestamptz NOT NULL,
    last_seen_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    revoked_at timestamptz,
    revoke_reason text,
    PRIMARY KEY (tenant_id, id),
    UNIQUE (tenant_id, token_hash),
    UNIQUE (tenant_id, id, membership_id, user_id),
    CHECK (expires_at > issued_at),
    CHECK (revoked_at IS NULL OR revoked_at >= issued_at),
    CHECK (revoked_at IS NOT NULL OR revoke_reason IS NULL),
    FOREIGN KEY (tenant_id, membership_id, user_id)
        REFERENCES memberships (tenant_id, id, user_id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, device_id, membership_id, user_id)
        REFERENCES devices (tenant_id, id, membership_id, user_id) ON DELETE RESTRICT
);

CREATE TABLE refresh_tokens (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    session_id uuid NOT NULL,
    membership_id uuid NOT NULL,
    user_id uuid NOT NULL,
    token_hash bytea NOT NULL CHECK (octet_length(token_hash) = 32),
    issued_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at timestamptz NOT NULL,
    used_at timestamptz,
    revoked_at timestamptz,
    replaced_by_token_id uuid,
    PRIMARY KEY (tenant_id, id),
    UNIQUE (tenant_id, token_hash),
    CHECK (expires_at > issued_at),
    CHECK (used_at IS NULL OR used_at >= issued_at),
    CHECK (revoked_at IS NULL OR revoked_at >= issued_at),
    FOREIGN KEY (tenant_id, session_id, membership_id, user_id)
        REFERENCES sessions (tenant_id, id, membership_id, user_id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, replaced_by_token_id)
        REFERENCES refresh_tokens (tenant_id, id) ON DELETE RESTRICT
);

CREATE TABLE license_entitlements (
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE RESTRICT,
    id uuid NOT NULL,
    license_id text NOT NULL CHECK (length(btrim(license_id)) > 0),
    edition text NOT NULL CHECK (edition IN ('network', 'enterprise')),
    status text NOT NULL DEFAULT 'active'
        CHECK (status IN ('pending', 'active', 'suspended', 'expired', 'revoked')),
    seat_limit integer NOT NULL CHECK (seat_limit > 0),
    features jsonb NOT NULL DEFAULT '{}'::jsonb
        CHECK (jsonb_typeof(features) = 'object'),
    starts_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    issued_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    revoked_at timestamptz,
    issuer text NOT NULL CHECK (length(btrim(issuer)) > 0),
    signature text NOT NULL CHECK (length(btrim(signature)) > 0),
    PRIMARY KEY (tenant_id, id),
    UNIQUE (tenant_id, license_id),
    CHECK (expires_at > starts_at),
    CHECK (revoked_at IS NULL OR revoked_at >= issued_at)
);

CREATE INDEX users_login_status_idx
    ON users (tenant_id, normalized_login, status);
CREATE INDEX credentials_lockout_idx
    ON credentials (tenant_id, locked_until)
    WHERE locked_until IS NOT NULL;
CREATE INDEX membership_roles_role_idx
    ON membership_roles (tenant_id, role_id, membership_id);
CREATE INDEX role_permissions_permission_idx
    ON role_permissions (tenant_id, permission_id, role_id);
CREATE INDEX sessions_principal_idx
    ON sessions (tenant_id, membership_id, expires_at DESC)
    WHERE revoked_at IS NULL;
CREATE INDEX refresh_tokens_session_idx
    ON refresh_tokens (tenant_id, session_id, expires_at DESC)
    WHERE revoked_at IS NULL AND used_at IS NULL;
CREATE INDEX license_entitlements_validity_idx
    ON license_entitlements (tenant_id, status, starts_at, expires_at)
    WHERE revoked_at IS NULL;

ALTER TABLE users ENABLE ROW LEVEL SECURITY;
ALTER TABLE users FORCE ROW LEVEL SECURITY;
CREATE POLICY users_current_context ON users
    USING (tenant_id = app.current_tenant_id())
    WITH CHECK (tenant_id = app.current_tenant_id());

ALTER TABLE credentials ENABLE ROW LEVEL SECURITY;
ALTER TABLE credentials FORCE ROW LEVEL SECURITY;
CREATE POLICY credentials_current_context ON credentials
    USING (tenant_id = app.current_tenant_id())
    WITH CHECK (tenant_id = app.current_tenant_id());

ALTER TABLE memberships ENABLE ROW LEVEL SECURITY;
ALTER TABLE memberships FORCE ROW LEVEL SECURITY;
CREATE POLICY memberships_current_context ON memberships
    USING (tenant_id = app.current_tenant_id())
    WITH CHECK (tenant_id = app.current_tenant_id());

ALTER TABLE roles ENABLE ROW LEVEL SECURITY;
ALTER TABLE roles FORCE ROW LEVEL SECURITY;
CREATE POLICY roles_current_context ON roles
    USING (tenant_id = app.current_tenant_id())
    WITH CHECK (tenant_id = app.current_tenant_id());

ALTER TABLE permissions ENABLE ROW LEVEL SECURITY;
ALTER TABLE permissions FORCE ROW LEVEL SECURITY;
CREATE POLICY permissions_current_context ON permissions
    USING (tenant_id = app.current_tenant_id())
    WITH CHECK (tenant_id = app.current_tenant_id());

ALTER TABLE role_permissions ENABLE ROW LEVEL SECURITY;
ALTER TABLE role_permissions FORCE ROW LEVEL SECURITY;
CREATE POLICY role_permissions_current_context ON role_permissions
    USING (tenant_id = app.current_tenant_id())
    WITH CHECK (tenant_id = app.current_tenant_id());

ALTER TABLE membership_roles ENABLE ROW LEVEL SECURITY;
ALTER TABLE membership_roles FORCE ROW LEVEL SECURITY;
CREATE POLICY membership_roles_current_context ON membership_roles
    USING (tenant_id = app.current_tenant_id())
    WITH CHECK (tenant_id = app.current_tenant_id());

ALTER TABLE devices ENABLE ROW LEVEL SECURITY;
ALTER TABLE devices FORCE ROW LEVEL SECURITY;
CREATE POLICY devices_current_context ON devices
    USING (tenant_id = app.current_tenant_id())
    WITH CHECK (tenant_id = app.current_tenant_id());

ALTER TABLE sessions ENABLE ROW LEVEL SECURITY;
ALTER TABLE sessions FORCE ROW LEVEL SECURITY;
CREATE POLICY sessions_current_context ON sessions
    USING (tenant_id = app.current_tenant_id())
    WITH CHECK (tenant_id = app.current_tenant_id());

ALTER TABLE refresh_tokens ENABLE ROW LEVEL SECURITY;
ALTER TABLE refresh_tokens FORCE ROW LEVEL SECURITY;
CREATE POLICY refresh_tokens_current_context ON refresh_tokens
    USING (tenant_id = app.current_tenant_id())
    WITH CHECK (tenant_id = app.current_tenant_id());

ALTER TABLE license_entitlements ENABLE ROW LEVEL SECURITY;
ALTER TABLE license_entitlements FORCE ROW LEVEL SECURITY;
CREATE POLICY license_entitlements_current_context ON license_entitlements
    USING (tenant_id = app.current_tenant_id())
    WITH CHECK (tenant_id = app.current_tenant_id());
