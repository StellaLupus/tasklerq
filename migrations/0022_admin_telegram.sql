-- Telegram администратора: claim владельца и привязка учётной записи.
--
-- Миграция аддитивна: строки 0021 не переписываются. `admin_accounts`
-- расширяется колонками, у `admin_one_time_codes` заменяются только два
-- именованных ограничения (`DROP CONSTRAINT` + `ADD CONSTRAINT`), сама таблица
-- не переписывается. Код claim, привязки и восстановления (4.2+, 5.x) появится
-- позже; здесь только схема.
--
-- Назначения одноразовых кодов (design, решение 4): `setup` и `recovery` —
-- прежние; `owner_claim` — ссылка-claim владельца; `telegram_link` — ссылка
-- привязки Telegram к учётной записи администратора. `owner_claim` —
-- единственное назначение с household: `household_id` — дом без владельца или
-- NULL для пустой инсталляции в `single`. У `setup`, `recovery` и
-- `telegram_link` household всегда NULL: учётная запись администратора одна на
-- инсталляцию, поэтому и привязка к дому не относится (design, решение 6).
--
-- Ограничение срока не меняется: `expires_at` обязателен для всех назначений,
-- кроме `setup`. Сроки 24 часа у claim и 15 минут у привязки задаёт код, а не
-- схема: таймер не должен требовать миграции.
--
-- Гашение прежних кодов идёт по паре `(purpose, household_id)`:
--
--     UPDATE admin_one_time_codes SET used_at = now()
--      WHERE purpose = $1 AND household_id IS NOT DISTINCT FROM $2
--        AND used_at IS NULL;
--
-- Пара сравнивается через `IS NOT DISTINCT FROM`, поэтому NULL участвует в
-- сравнении как обычное значение. В `multi` выпуск ссылки дома B не гасит
-- ссылку дома A: у пары другой household. Индекс
-- `admin_one_time_codes_active_idx` (0021) обслуживает этот поиск.
--
-- `admin_accounts.telegram_user_id` — подтверждённая привязка, NULL пока её
-- нет. Тип — `bigint`, как `users.telegram_user_id`. Уникальный индекс не
-- мешает NULL (в PostgreSQL NULL не равны друг другу), поэтому непривязанное
-- состояние хранится как NULL без отдельного флага.
--
-- `pending_telegram_user_id` и `pending_telegram_name` — кандидат, открывший
-- `/start adm_…`: консоль показывает имя и кнопки «подтвердить / отклонить».
-- Оба поля NULL или оба заданы; подтверждение переносит кандидата в
-- `telegram_user_id`.
--
-- `console_origin` — origin консоли, на который уходит ссылка восстановления
-- (design, решение 6); обновляется при каждом успешном входе. NULL — адрес
-- ещё не известен, и подсказка ведёт к CLI.
--
-- Значения `audit_event_type` добавляются последними: PostgreSQL не даёт
-- обратиться к значению, добавленному в текущей транзакции, поэтому пишет эти
-- события код 5.6, а не эта миграция.

ALTER TABLE admin_one_time_codes
    DROP CONSTRAINT admin_one_time_codes_purpose,
    DROP CONSTRAINT admin_one_time_codes_purpose_household,
    ADD CONSTRAINT admin_one_time_codes_purpose CHECK (
        purpose IN ('setup', 'recovery', 'owner_claim', 'telegram_link')
    ),
    ADD CONSTRAINT admin_one_time_codes_purpose_household CHECK (
        household_id IS NULL OR purpose = 'owner_claim'
    );

ALTER TABLE admin_accounts
    ADD COLUMN telegram_user_id bigint,
    ADD COLUMN pending_telegram_user_id bigint,
    ADD COLUMN pending_telegram_name text,
    ADD COLUMN console_origin text,
    ADD CONSTRAINT admin_accounts_pending_telegram CHECK (
        (pending_telegram_user_id IS NULL) = (pending_telegram_name IS NULL)
    );

CREATE UNIQUE INDEX admin_accounts_telegram_user_id_idx
    ON admin_accounts (telegram_user_id);

ALTER TYPE audit_event_type ADD VALUE IF NOT EXISTS 'admin_telegram_linked';
ALTER TYPE audit_event_type ADD VALUE IF NOT EXISTS 'admin_telegram_unlinked';
