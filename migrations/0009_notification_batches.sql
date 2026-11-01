-- Сводные сообщения о уведомлениях, отложенных тихими часами.
--
-- Одна строка `notification_batches` — одно выпущенное сводное сообщение
-- конкретному получателю. `page` хранит текущую страницу, чтобы пагинация и
-- действия перерисовывали это же сообщение по актуальному состоянию из БД.
-- `result_completed`/`result_changed` фиксируют итог «Завершить все»: повторное
-- нажатие не должно пересчитывать результат и менять сообщение.
--
-- `notification_deliveries.batch_id` связывает доставку со сводным сообщением.
-- `batch_version` — версия задачи на момент выпуска; с ней работает
-- «Завершить все», поэтому задача, изменённая после выпуска, не завершается.
-- Кнопка «Готово» в блоке берёт версию из текущей отрисовки (актуальный
-- `items.version`): по устаревшей версии действие ничего не меняет, а блок
-- перерисовывается по текущей.
-- `batch_state` — итог действия над блоком (`done`, `snoozed`, `missing`), а
-- `batch_snoozed_until` — время нового snooze для подписи «Напомню в HH:MM».
-- Состояния «изменена» нет: изменившаяся задача остаётся открытым блоком с
-- кнопками.
--
-- `deferred` означает «отложено тихими часами»: ручной выпуск, баннер и перенос
-- при всё ещё тихом интервале работают только с такими доставками.
-- `batched` — групповой snooze действия «Отложить все на час»: в свой срок он
-- собирается в сводное вместе с отложенными тихими часами, но ручной выпуск его
-- не ускоряет. Если его срок сам попадает в тихие часы, `deferred` выставляется
-- общим правилом.
--
-- Retention чистит батчи вместе с доставками по тому же сроку
-- `RETENTION_OUTBOX_DAYS`; удаляются только батчи, на которые больше не
-- ссылается ни одна доставка.

CREATE TABLE notification_batches (
    id uuid PRIMARY KEY,
    household_id uuid NOT NULL REFERENCES households (id) ON DELETE CASCADE,
    user_id uuid NOT NULL,
    chat_id bigint NOT NULL,
    page integer NOT NULL DEFAULT 0,
    result_completed integer,
    result_changed integer,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT notification_batches_user_household_fk
        FOREIGN KEY (user_id, household_id)
        REFERENCES users (id, household_id) ON DELETE CASCADE
);

ALTER TABLE notification_deliveries
    ADD COLUMN batch_id uuid REFERENCES notification_batches (id) ON DELETE SET NULL,
    ADD COLUMN batch_version integer,
    ADD COLUMN batch_state text,
    ADD COLUMN batch_snoozed_until timestamptz,
    ADD COLUMN batched boolean NOT NULL DEFAULT false,
    ADD CONSTRAINT notification_deliveries_batch_state_check
        CHECK (batch_state IS NULL
               OR batch_state IN ('done', 'snoozed', 'missing'));

CREATE INDEX notification_deliveries_batch_idx
    ON notification_deliveries (batch_id)
    WHERE batch_id IS NOT NULL;
