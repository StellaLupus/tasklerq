-- Отметка закрытия отправленного уведомления перерисовкой (design, решение 4):
-- `closed_at` — когда отправленное уведомление закрыто перерисовкой
-- («✅ Выполнено» / «👤 Назначено»); NULL — не закрыто. `closed_reason` —
-- причина закрытия: `completed` (задача выполнена) или `assigned` (назначена
-- другому); NULL — не закрыто. `closed_by_event` — выполнение, по которому
-- сообщение закрыто; для `assigned` пусто. При удалении события ссылка
-- обнуляется, а запись доставки остаётся. Частичный индекс по `updated_at`
-- для `sent`-доставок обслуживает выборку шага синхронизации.
--
-- Миграция аддитивна: существующие строки не переписываются, все колонки
-- nullable и без значения по умолчанию. Уже отправленные уведомления
-- считаются незакрытыми.

ALTER TABLE notification_deliveries ADD COLUMN closed_at timestamptz;

ALTER TABLE notification_deliveries
    ADD COLUMN closed_reason text,
    ADD CONSTRAINT notification_deliveries_closed_reason CHECK (
        closed_reason IS NULL OR closed_reason IN ('completed', 'assigned')
    );

ALTER TABLE notification_deliveries
    ADD COLUMN closed_by_event uuid REFERENCES item_events (id) ON DELETE SET NULL;

CREATE INDEX notification_deliveries_sent_idx
    ON notification_deliveries (updated_at)
    WHERE status = 'sent';
