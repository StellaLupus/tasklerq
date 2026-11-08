-- Привязка snooze к вхождению срока, из которого его отложили. Формат ключа
-- совпадает с `occurrence_key` напоминания (`due_at` в UTC или
-- `date:YYYY-MM-DD`); у задачи без срока — `no-due`. У остальных видов
-- доставки колонка пуста. У snooze `NULL` означает запись, созданную до
-- миграции: такие записи доставляются по старому правилу. Колонка аддитивная,
-- без бэкфилла и без перезаписи таблицы; старый код её не читает.

ALTER TABLE notification_deliveries ADD COLUMN origin_key text;

COMMENT ON COLUMN notification_deliveries.origin_key IS
    'Вхождение срока для snooze: ключ напоминания, no-due без срока или NULL у записи до миграции';
