-- Видимость экрана: Telegram не сообщает об удалении якоря пользователем,
-- поэтому outbox сам помечает экран засыпанным. Новое значение enum
-- добавляется здесь, но не используется в этой же транзакции: Postgres это
-- запрещает.

ALTER TYPE outbox_kind ADD VALUE 'delete';

ALTER TABLE telegram_views ADD COLUMN buried boolean NOT NULL DEFAULT false;
