-- Веб-консоль администратора: учётка, сессии и одноразовые коды.
--
-- Миграция аддитивна: существующие таблицы не меняются, всё новое живёт здесь.
-- Код учётки, кодов и сессий (5.3–5.4) появится позже; здесь только схема.
--
-- `admin_accounts` — единственная учётная запись инсталляции. Связи с `users`
-- нет: администратор — веб-версия оператора CLI, а не участник household (design,
-- решение 2). «Не более одной строки» держит уникальный индекс по константному
-- выражению `(true)`: колонки-синглтона в таблице нет, схема остаётся ровно из
-- пяти полей design, а второй `INSERT` получает нарушение уникальности. На этот
-- индекс не опирается ни один `ON CONFLICT`: запись учётки проверяет отсутствие
-- строки сама.
--
-- `password_changed_at` NOT NULL: учётка создаётся только вместе с паролем
-- (setup-код), состояния «пароль ещё не задавался» не бывает, и 5.4 не работает
-- с NULL. `password_hash` — строка PHC с параметрами argon2id; формат здесь не
-- проверяется, чтобы смена параметров хеша не требовала миграции.
--
-- `admin_sessions` — сессия консоли. `token_hash` — SHA-256 токена в нижнем
-- hexadecimal (`auth::tokens::hash_token`), как у API-токенов и приглашений.
-- CHECK на 64 hex-символа не даёт записать сюда сам токен: 256-битный токен в
-- base64url — 43 символа, а не 64. Сроки отдельными колонками не дублируются:
-- 7 дней без активности считаются от `last_seen_at`, 30 дней абсолютного срока —
-- от `created_at` (design, решение 4). `last_seen_at` обновляется не чаще раза
-- в минуту, поэтому CHECK `last_seen_at >= created_at` держится. Индекс по
-- `created_at` нужен retention (5.9), поиск по `token_hash` идёт по его
-- уникальному индексу.
--
-- `admin_one_time_codes` — одноразовые коды всех назначений. `code_hash` — тоже
-- SHA-256 в нижнем hexadecimal; CHECK не пропускает 22-символьный base64url
-- самого кода. В 0013 назначения `setup` и `recovery`, оба без household; 0014
-- добавит `owner_claim` и `telegram_link` с household, поэтому ограничения
-- назначений именованы: их заменяют `DROP CONSTRAINT` + `ADD CONSTRAINT`, не
-- переписывая таблицу. `setup` — единственное назначение без обязательного
-- срока: пока учётки нет, код гасит выпуск следующего, а не таймер. `recovery`
-- живёт 15 минут.
--
-- Использование кода — один `UPDATE` в транзакции с действием:
--
--     UPDATE admin_one_time_codes SET used_at = now()
--      WHERE code_hash = $1 AND used_at IS NULL
--        AND (expires_at IS NULL OR expires_at > now())
--      RETURNING id, purpose, household_id;
--
-- Повторный или истёкший код не возвращает строку. Выпуск нового кода гасит
-- прежние с той же парой:
--
--     UPDATE admin_one_time_codes SET used_at = now()
--      WHERE purpose = $1 AND household_id IS NOT DISTINCT FROM $2
--        AND used_at IS NULL;
--
-- Использование кода ищет строку по `code_hash` (уникальный индекс), гашение —
-- по паре `(purpose, household_id)` (частичный индекс по неиспользованным).
-- Retention (5.9) удаляет истёкшие и использованные коды старше 7 дней — для
-- этого индексы по `expires_at` и `used_at`.
--
-- `audit_event_type` расширяется значениями из design, решение 6. Использовать
-- их в этой же миграции нельзя: PostgreSQL не даёт обратиться к значению,
-- добавленному в текущей транзакции, поэтому `ADD VALUE` идут последними, а
-- пишет эти события код 5.6.

CREATE TABLE admin_accounts (
    id uuid PRIMARY KEY,
    username text NOT NULL CHECK (btrim(username) <> ''),
    password_hash text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    password_changed_at timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX admin_accounts_singleton_idx ON admin_accounts ((true));

CREATE TABLE admin_sessions (
    id uuid PRIMARY KEY,
    token_hash text NOT NULL UNIQUE CHECK (token_hash ~ '^[0-9a-f]{64}$'),
    created_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT admin_sessions_last_seen_after_created CHECK (
        last_seen_at >= created_at
    )
);

CREATE INDEX admin_sessions_created_idx ON admin_sessions (created_at);

CREATE TABLE admin_one_time_codes (
    id uuid PRIMARY KEY,
    purpose text NOT NULL,
    household_id uuid REFERENCES households (id) ON DELETE CASCADE,
    code_hash text NOT NULL UNIQUE CHECK (code_hash ~ '^[0-9a-f]{64}$'),
    expires_at timestamptz,
    used_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT admin_one_time_codes_purpose CHECK (
        purpose IN ('setup', 'recovery')
    ),
    CONSTRAINT admin_one_time_codes_purpose_household CHECK (
        household_id IS NULL
    ),
    CONSTRAINT admin_one_time_codes_expiry CHECK (
        purpose = 'setup' OR expires_at IS NOT NULL
    ),
    CONSTRAINT admin_one_time_codes_expiry_after_creation CHECK (
        expires_at IS NULL OR expires_at > created_at
    )
);

CREATE INDEX admin_one_time_codes_active_idx
    ON admin_one_time_codes (purpose, household_id)
    WHERE used_at IS NULL;
CREATE INDEX admin_one_time_codes_expires_idx
    ON admin_one_time_codes (expires_at)
    WHERE expires_at IS NOT NULL;
CREATE INDEX admin_one_time_codes_used_idx
    ON admin_one_time_codes (used_at)
    WHERE used_at IS NOT NULL;

ALTER TYPE audit_event_type ADD VALUE IF NOT EXISTS 'admin_login';
ALTER TYPE audit_event_type ADD VALUE IF NOT EXISTS 'admin_login_failed';
ALTER TYPE audit_event_type ADD VALUE IF NOT EXISTS 'admin_setup_code_failed';
ALTER TYPE audit_event_type ADD VALUE IF NOT EXISTS 'admin_account_created';
ALTER TYPE audit_event_type ADD VALUE IF NOT EXISTS 'admin_password_changed';
ALTER TYPE audit_event_type ADD VALUE IF NOT EXISTS 'admin_recovery_issued';
ALTER TYPE audit_event_type ADD VALUE IF NOT EXISTS 'admin_setting_changed';
