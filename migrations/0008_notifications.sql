-- Уведомления и планировщик: дедупликация доставок и связь с outbox.
--
-- Одна строка — одно уведомление конкретному пользователю. Ключ доставки
-- `(user_id, item_id, occurrence_key, kind)` не даёт повторному тику или
-- перезапуску сервиса создать второе сообщение для того же вхождения.
--
-- `item_id` пуст у будущих сводок (уведомление не привязано к элементу),
-- поэтому уникальность объявлена с `NULLS NOT DISTINCT`: два NULL-элемента с
-- тем же пользователем, вхождением и типом считаются дублем. PostgreSQL 15+
-- поддерживает такую форму напрямую, отдельный индекс не нужен.
--
-- `occurrence_key` — срок вхождения: `due_at` в UTC для timed-задач и
-- `date:YYYY-MM-DD` для вседневных; у повторяющихся он меняется вместе со
-- сроком. `outbox_seq` ссылается на поставленное сообщение; после retention
-- outbox ссылка обнуляется, а запись доставки остаётся для аудита дедупликации.
--
-- `kind` уже содержит значения для следующих пунктов (`digest`, `assignment`),
-- чтобы добавлять источники без изменения типа. `snooze` — отдельная доставка
-- отложенного напоминания: её ключ включает время snooze и не совпадает с
-- `occurrence_key` срока, поэтому она не отменяется как устаревшее вхождение.
-- Статус `sent` означает «передано в outbox»; фактическую доставку и
-- `message_id` ведёт outbox.

CREATE TYPE notification_kind AS ENUM ('reminder', 'digest', 'assignment', 'snooze');
CREATE TYPE notification_status AS ENUM ('pending', 'sent', 'cancelled');

CREATE TABLE notification_deliveries (
    id uuid PRIMARY KEY,
    household_id uuid NOT NULL REFERENCES households (id) ON DELETE CASCADE,
    user_id uuid NOT NULL,
    item_id uuid,
    occurrence_key text NOT NULL,
    kind notification_kind NOT NULL,
    status notification_status NOT NULL DEFAULT 'pending',
    scheduled_at timestamptz NOT NULL,
    -- Запись хотя бы раз откладывалась из-за тихих часов. Нужно, чтобы
    -- перенесённая через полночь сводка или вседневное напоминание не
    -- отменялись как устаревшие только из-за смены локальной даты.
    deferred boolean NOT NULL DEFAULT false,
    outbox_seq bigint REFERENCES outbox (seq) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT notification_deliveries_user_household_fk
        FOREIGN KEY (user_id, household_id)
        REFERENCES users (id, household_id) ON DELETE CASCADE,
    CONSTRAINT notification_deliveries_item_household_fk
        FOREIGN KEY (item_id, household_id)
        REFERENCES items (id, household_id) ON DELETE CASCADE,
    CONSTRAINT notification_deliveries_key_unique
        UNIQUE NULLS NOT DISTINCT (user_id, item_id, occurrence_key, kind)
);

-- Выборка готовых к доставке записей и отмена устаревших pending.
CREATE INDEX notification_deliveries_pending_idx
    ON notification_deliveries (scheduled_at)
    WHERE status = 'pending';
