-- Базовая схема домена Tasklerq.
--
-- Идентификаторы — UUIDv7, их генерирует приложение. Время — UTC; вседневные
-- сроки хранятся как date. Перечисления повторяют docs/domain-model.md.

CREATE TYPE user_role AS ENUM ('owner', 'member');
CREATE TYPE list_kind AS ENUM ('tasks', 'shopping', 'checklist');
CREATE TYPE item_status AS ENUM ('open', 'done');
CREATE TYPE item_priority AS ENUM ('low', 'normal', 'high', 'urgent');
CREATE TYPE recurrence_freq AS ENUM ('daily', 'weekly', 'monthly', 'yearly');
CREATE TYPE month_rule AS ENUM ('anchor_day', 'last_day');
CREATE TYPE actor_type AS ENUM ('user', 'system', 'home_assistant', 'voice');
CREATE TYPE event_type AS ENUM (
    'created',
    'updated',
    'completed',
    'reopened',
    'deleted',
    'restored',
    'assigned',
    'unassigned',
    'recurrence_changed',
    'subtask_completed',
    'subtask_reopened'
);
CREATE TYPE item_source AS ENUM (
    'telegram',
    'ha_todo',
    'ha_voice',
    'ha_automation',
    'api',
    'import'
);
CREATE TYPE capture_source AS ENUM ('telegram', 'ha_voice', 'ha_automation', 'api', 'shortcut');
CREATE TYPE capture_mode AS ENUM ('auto', 'draft');
CREATE TYPE draft_status AS ENUM ('pending', 'applied', 'cancelled', 'expired', 'rejected');

CREATE TABLE households (
    id uuid PRIMARY KEY,
    name text NOT NULL,
    timezone text NOT NULL DEFAULT 'Europe/Moscow',
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    deleted_at timestamptz,
    CONSTRAINT households_name_len CHECK (char_length(btrim(name)) BETWEEN 1 AND 100),
    CONSTRAINT households_timezone_len CHECK (char_length(timezone) BETWEEN 1 AND 64)
);

CREATE TABLE users (
    id uuid PRIMARY KEY,
    household_id uuid NOT NULL REFERENCES households (id) ON DELETE CASCADE,
    telegram_user_id bigint NOT NULL,
    telegram_chat_id bigint NOT NULL,
    display_name text NOT NULL,
    username text,
    role user_role NOT NULL,
    timezone text,
    settings jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    deleted_at timestamptz,
    CONSTRAINT users_display_name_len CHECK (char_length(btrim(display_name)) BETWEEN 1 AND 64),
    CONSTRAINT users_username_len CHECK (username IS NULL OR char_length(username) <= 64),
    CONSTRAINT users_timezone_len CHECK (timezone IS NULL OR char_length(timezone) BETWEEN 1 AND 64),
    CONSTRAINT users_id_household_unique UNIQUE (id, household_id)
);

CREATE TABLE lists (
    id uuid PRIMARY KEY,
    household_id uuid NOT NULL REFERENCES households (id) ON DELETE CASCADE,
    kind list_kind NOT NULL,
    name text NOT NULL,
    is_default boolean NOT NULL DEFAULT false,
    is_archived boolean NOT NULL DEFAULT false,
    position integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    deleted_at timestamptz,
    CONSTRAINT lists_name_len CHECK (char_length(btrim(name)) BETWEEN 1 AND 64),
    CONSTRAINT lists_default_not_archived CHECK (NOT (is_default AND is_archived)),
    CONSTRAINT lists_id_household_unique UNIQUE (id, household_id)
);

-- capture_drafts создаётся до items, потому что items.capture_id ссылается на
-- неё. Логика capture появится в следующих фазах; таблица нужна retention и
-- внешним ключам домена.
CREATE TABLE capture_drafts (
    id uuid PRIMARY KEY,
    household_id uuid NOT NULL REFERENCES households (id) ON DELETE CASCADE,
    user_id uuid REFERENCES users (id) ON DELETE SET NULL,
    chat_id bigint,
    source capture_source NOT NULL,
    mode capture_mode NOT NULL,
    idempotency_key text,
    request_hash text,
    raw_text text NOT NULL,
    parsed jsonb,
    status draft_status NOT NULL DEFAULT 'pending',
    fallback boolean NOT NULL DEFAULT false,
    prompt_version text,
    telegram_message_id bigint,
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    applied_at timestamptz,
    CONSTRAINT capture_drafts_raw_text_len CHECK (char_length(raw_text) <= 2000),
    CONSTRAINT capture_drafts_id_household_unique UNIQUE (id, household_id)
);

CREATE TABLE items (
    id uuid PRIMARY KEY,
    household_id uuid NOT NULL REFERENCES households (id) ON DELETE CASCADE,
    list_id uuid NOT NULL,
    title text NOT NULL,
    notes text,
    status item_status NOT NULL DEFAULT 'open',
    priority item_priority NOT NULL DEFAULT 'normal',
    due_at timestamptz,
    due_date date,
    assignee_user_id uuid REFERENCES users (id) ON DELETE SET NULL,
    created_by_user_id uuid REFERENCES users (id) ON DELETE SET NULL,
    source item_source NOT NULL DEFAULT 'api',
    capture_id uuid REFERENCES capture_drafts (id) ON DELETE SET NULL,
    completed_at timestamptz,
    completed_by_user_id uuid REFERENCES users (id) ON DELETE SET NULL,
    quantity numeric(7, 3),
    unit text,
    position integer NOT NULL DEFAULT 0,
    version integer NOT NULL DEFAULT 1,
    recurrence_freq recurrence_freq,
    recurrence_interval smallint,
    recurrence_byweekday smallint[],
    recurrence_until date,
    recurrence_timezone text,
    recurrence_anchor_date date,
    recurrence_month_rule month_rule,
    search_vector tsvector GENERATED ALWAYS AS (
        to_tsvector('simple', coalesce(title, '') || ' ' || coalesce(notes, ''))
    ) STORED,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    deleted_at timestamptz,
    CONSTRAINT items_list_household_fk
        FOREIGN KEY (list_id, household_id) REFERENCES lists (id, household_id)
        ON DELETE CASCADE,
    CONSTRAINT items_title_len CHECK (char_length(btrim(title)) BETWEEN 1 AND 200),
    CONSTRAINT items_notes_len CHECK (notes IS NULL OR char_length(notes) <= 2000),
    CONSTRAINT items_unit_len CHECK (unit IS NULL OR char_length(unit) <= 16),
    CONSTRAINT items_due_exclusive CHECK (due_at IS NULL OR due_date IS NULL),
    CONSTRAINT items_quantity_range CHECK (
        quantity IS NULL OR (quantity > 0 AND quantity <= 9999.999)
    ),
    CONSTRAINT items_version_positive CHECK (version >= 1),
    CONSTRAINT items_done_completed CHECK (status <> 'done' OR completed_at IS NOT NULL),
    CONSTRAINT items_recurrence_shape CHECK (
        (
            recurrence_freq IS NULL
            AND recurrence_interval IS NULL
            AND recurrence_byweekday IS NULL
            AND recurrence_until IS NULL
            AND recurrence_timezone IS NULL
            AND recurrence_anchor_date IS NULL
            AND recurrence_month_rule IS NULL
        )
        OR (
            recurrence_freq IS NOT NULL
            AND recurrence_interval BETWEEN 1 AND 365
            AND recurrence_timezone IS NOT NULL
            AND recurrence_anchor_date IS NOT NULL
        )
    ),
    CONSTRAINT items_recurrence_weekly CHECK (
        recurrence_freq IS DISTINCT FROM 'weekly'
        OR (recurrence_byweekday IS NOT NULL AND cardinality(recurrence_byweekday) > 0)
    ),
    CONSTRAINT items_recurrence_weekday_range CHECK (
        recurrence_byweekday IS NULL
        OR recurrence_byweekday <@ ARRAY[1, 2, 3, 4, 5, 6, 7]::smallint[]
    ),
    CONSTRAINT items_recurrence_monthly CHECK (
        recurrence_freq IS DISTINCT FROM 'monthly' OR recurrence_month_rule IS NOT NULL
    ),
    CONSTRAINT items_recurrence_month_rule_only_monthly CHECK (
        recurrence_freq = 'monthly' OR recurrence_month_rule IS NULL
    ),
    CONSTRAINT items_recurrence_until CHECK (
        recurrence_until IS NULL
        OR (
            recurrence_until >= recurrence_anchor_date
            AND recurrence_until <= (recurrence_anchor_date + INTERVAL '5 years')::date
        )
    ),
    CONSTRAINT items_id_household_unique UNIQUE (id, household_id)
);

CREATE TABLE subtasks (
    id uuid PRIMARY KEY,
    item_id uuid NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    title text NOT NULL,
    status item_status NOT NULL DEFAULT 'open',
    position integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    completed_at timestamptz,
    CONSTRAINT subtasks_title_len CHECK (char_length(btrim(title)) BETWEEN 1 AND 200),
    CONSTRAINT subtasks_done_completed CHECK (status <> 'done' OR completed_at IS NOT NULL)
);

CREATE TABLE tags (
    id uuid PRIMARY KEY,
    household_id uuid NOT NULL REFERENCES households (id) ON DELETE CASCADE,
    name text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT tags_name_len CHECK (char_length(btrim(name)) BETWEEN 1 AND 32)
);

CREATE TABLE item_tags (
    item_id uuid NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    tag_id uuid NOT NULL REFERENCES tags (id) ON DELETE CASCADE,
    PRIMARY KEY (item_id, tag_id)
);

CREATE TABLE item_events (
    id uuid PRIMARY KEY,
    household_id uuid NOT NULL REFERENCES households (id) ON DELETE CASCADE,
    item_id uuid NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    actor_user_id uuid REFERENCES users (id) ON DELETE SET NULL,
    actor_type actor_type NOT NULL,
    event_type event_type NOT NULL,
    payload jsonb NOT NULL DEFAULT '{}'::jsonb,
    undo_of uuid REFERENCES item_events (id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE api_tokens (
    id uuid PRIMARY KEY,
    household_id uuid NOT NULL REFERENCES households (id) ON DELETE CASCADE,
    name text NOT NULL,
    token_hash text NOT NULL UNIQUE,
    prefix text NOT NULL,
    scopes text[] NOT NULL,
    created_by_user_id uuid REFERENCES users (id) ON DELETE SET NULL,
    expires_at timestamptz,
    last_used_at timestamptz,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT api_tokens_name_len CHECK (char_length(btrim(name)) BETWEEN 1 AND 64),
    CONSTRAINT api_tokens_prefix_len CHECK (char_length(prefix) BETWEEN 1 AND 32),
    CONSTRAINT api_tokens_scopes_not_empty CHECK (cardinality(scopes) > 0)
);
