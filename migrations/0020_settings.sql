-- Настройки инсталляции и household для консоли администратора.
--
-- Строка — одна настройка с ключом из фиксированного перечня в коде
-- (`llm.enabled`, `llm.primary.url`, `llm.daily_quota`, `wizard.llm_skipped`,
-- позже ключи Telegram). Уровень задаёт `scope`: `installation` — настройка
-- всей инсталляции, `household` — одного household. Обычное значение лежит в
-- `value` (jsonb), секрет — в `secret` (шифротекст, формат из design, решение
-- 8); ровно одно из двух. Ключ шифрования живёт в окружении, а не в базе.
--
-- `household_id` ссылается на `households` с `ON DELETE CASCADE`: настройки
-- household без самого household не имеют смысла и уходят вместе с остальными
-- его данными (users, lists, items, ...). CHECK связывает уровень и ссылку:
-- `installation` всегда без household, `household` — всегда с ним.
--
-- Уникальность — по `(scope, household_id, key)`. `UNIQUE` считает NULL
-- различными, поэтому для installation в индексе стоит
-- `coalesce(household_id, nil uuid)`: nil не может быть настоящим
-- идентификатором (ids создаёт приложение как UUID v7). Тот же индекс служит
-- целью `ON CONFLICT` при записи настройки.
--
-- Суррогатного `id` нет: настройку всегда читают и пишут по ключу, а первичный
-- ключ на `(scope, household_id, key)` невозможен из-за NULL в `household_id`.
-- Роль идентификатора строки играет уникальный индекс по выражению.
--
-- Миграция только создаёт таблицу: прежние значения приходят из окружения,
-- данные не переносятся.

CREATE TABLE settings (
    scope text NOT NULL CHECK (scope IN ('installation', 'household')),
    household_id uuid REFERENCES households (id) ON DELETE CASCADE,
    key text NOT NULL CHECK (btrim(key) <> ''),
    value jsonb,
    secret bytea,
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT settings_value_or_secret CHECK (num_nonnulls(value, secret) = 1),
    CONSTRAINT settings_scope_household CHECK (
        (scope = 'installation') = (household_id IS NULL)
    )
);

CREATE UNIQUE INDEX settings_scope_household_key_uniq ON settings (
    scope,
    coalesce(household_id, '00000000-0000-0000-0000-000000000000'::uuid),
    key
);
