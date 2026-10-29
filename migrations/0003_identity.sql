-- Идентичность и доступ: приглашения, аудит членства и безопасности,
-- единственный владелец household.

CREATE TYPE audit_event_type AS ENUM (
    'household_created',
    'member_joined',
    'member_left',
    'member_removed',
    'role_changed',
    'invite_created',
    'invite_used',
    'invite_revoked',
    'token_created',
    'token_revoked',
    'auth_failed',
    'rate_limited',
    'household_deleted'
);

CREATE TABLE invites (
    id uuid PRIMARY KEY,
    household_id uuid NOT NULL REFERENCES households (id) ON DELETE CASCADE,
    code_hash text NOT NULL UNIQUE,
    role user_role NOT NULL DEFAULT 'member',
    created_by_user_id uuid REFERENCES users (id) ON DELETE SET NULL,
    expires_at timestamptz NOT NULL,
    used_by_user_id uuid REFERENCES users (id) ON DELETE SET NULL,
    used_at timestamptz,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT invites_member_role CHECK (role = 'member'),
    CONSTRAINT invites_used_or_revoked CHECK (used_at IS NULL OR revoked_at IS NULL),
    CONSTRAINT invites_expiry_after_creation CHECK (expires_at > created_at)
);

CREATE INDEX invites_household_active_idx
    ON invites (household_id, expires_at)
    WHERE used_at IS NULL AND revoked_at IS NULL;

-- household_id NULL — системные события без household (например, отказы
-- аутентификации по неизвестному токену). Payload не содержит секретов.
CREATE TABLE audit_events (
    id uuid PRIMARY KEY,
    household_id uuid REFERENCES households (id) ON DELETE CASCADE,
    actor_user_id uuid REFERENCES users (id) ON DELETE SET NULL,
    actor_type actor_type NOT NULL,
    event_type audit_event_type NOT NULL,
    payload jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX audit_events_household_created_idx
    ON audit_events (household_id, created_at DESC, id DESC);
CREATE INDEX audit_events_created_idx
    ON audit_events (created_at);

-- В household не более одного активного владельца: второй owner получает
-- конфликт на уровне базы, передача владения выполняется в одной транзакции.
CREATE UNIQUE INDEX users_household_owner_idx
    ON users (household_id)
    WHERE role = 'owner' AND deleted_at IS NULL;
