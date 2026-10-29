-- Telegram: очередь входящих обновлений, callback-токены, экраны, формы и
-- исходящий outbox.

CREATE TYPE update_status AS ENUM ('pending', 'processed', 'dead');
CREATE TYPE outbox_status AS ENUM ('pending', 'sent', 'dead', 'skipped');
CREATE TYPE outbox_kind AS ENUM ('screen', 'message', 'edit');

-- Очередь входящих обновлений. Обработка идёт в транзакции под
-- FOR UPDATE SKIP LOCKED: упавший воркер освобождает строку сам.
CREATE TABLE telegram_updates (
    update_id bigint PRIMARY KEY,
    payload jsonb NOT NULL,
    status update_status NOT NULL DEFAULT 'pending',
    attempts integer NOT NULL DEFAULT 0,
    available_at timestamptz NOT NULL DEFAULT now(),
    last_error text,
    received_at timestamptz NOT NULL DEFAULT now(),
    processed_at timestamptz
);

CREATE INDEX telegram_updates_ready_idx
    ON telegram_updates (available_at, update_id)
    WHERE status = 'pending';
CREATE INDEX telegram_updates_received_idx
    ON telegram_updates (received_at);

-- Одноразовые callback-токены. В базе только SHA-256 хеш токена.
-- household_id пуст для пользователей без household (онбординг).
CREATE TABLE callback_tokens (
    token_hash text PRIMARY KEY,
    telegram_user_id bigint NOT NULL,
    household_id uuid REFERENCES households (id) ON DELETE CASCADE,
    scope text NOT NULL DEFAULT 'screen',
    action text NOT NULL,
    payload jsonb NOT NULL DEFAULT '{}'::jsonb,
    version integer,
    expires_at timestamptz NOT NULL,
    consumed_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT callback_tokens_scope CHECK (scope IN ('screen', 'notice')),
    CONSTRAINT callback_tokens_ttl CHECK (expires_at <= created_at + INTERVAL '24 hours')
);

CREATE INDEX callback_tokens_expires_idx ON callback_tokens (expires_at);

-- Активное сообщение экрана чата: edit вместо новых сообщений.
CREATE TABLE telegram_views (
    chat_id bigint PRIMARY KEY,
    message_id bigint,
    view_key text NOT NULL,
    state jsonb NOT NULL DEFAULT '{}'::jsonb,
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Пошаговая форма: одна активная форма на чат, TTL 15 минут.
CREATE TABLE conversation_states (
    chat_id bigint PRIMARY KEY,
    telegram_user_id bigint NOT NULL,
    kind text NOT NULL,
    data jsonb NOT NULL DEFAULT '{}'::jsonb,
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX conversation_states_expires_idx ON conversation_states (expires_at);

-- Исходящие сообщения. seq задаёт порядок доставки в рамках чата.
CREATE TABLE outbox (
    seq bigserial PRIMARY KEY,
    chat_id bigint NOT NULL,
    kind outbox_kind NOT NULL,
    payload jsonb NOT NULL,
    status outbox_status NOT NULL DEFAULT 'pending',
    attempts integer NOT NULL DEFAULT 0,
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    message_id bigint,
    dedupe_key text UNIQUE,
    last_error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    sent_at timestamptz
);

CREATE INDEX outbox_pending_idx ON outbox (chat_id, seq) WHERE status = 'pending';
CREATE INDEX outbox_created_idx ON outbox (created_at);
