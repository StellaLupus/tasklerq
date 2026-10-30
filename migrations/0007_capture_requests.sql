-- Резервы ключей идемпотентности внешнего capture.
--
-- Ключ уникален в рамках `(household_id, source)`. Строка появляется до вызова
-- LLM: пока `response` пуст, первый запрос ещё обрабатывается и повтор получает
-- `409`. После успеха здесь лежит готовый ответ, который возвращается при
-- повторе без нового разбора. Ошибка освобождает резерв, а зависший резерв
-- перехватывается после `capture_requests_stale_seconds` (60 секунд).
--
-- Ответ хранится ограниченное время и удаляется retention-job вместе с
-- `capture_drafts` (`RETENTION_DRAFTS_DAYS`).

CREATE TABLE capture_requests (
    household_id uuid NOT NULL REFERENCES households (id) ON DELETE CASCADE,
    source capture_source NOT NULL,
    idempotency_key text NOT NULL,
    request_hash text NOT NULL,
    capture_id uuid NOT NULL,
    response jsonb,
    created_at timestamptz NOT NULL,
    completed_at timestamptz,
    CONSTRAINT capture_requests_idempotency_unique
        UNIQUE (household_id, source, idempotency_key)
);

CREATE INDEX capture_requests_created_idx
    ON capture_requests (created_at);
