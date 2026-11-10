-- Trigram-индексы для поиска по подстроке: описание задачи и название
-- подзадачи. Нужны условиям `ILIKE '%…%'` в поиске; создаются только при
-- доступном pg_trgm (расширение ставит 0002), без CONCURRENTLY — миграции
-- идут в транзакции.

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_trgm') THEN
        EXECUTE 'CREATE INDEX IF NOT EXISTS items_notes_trgm_idx '
            'ON items USING gin (notes gin_trgm_ops)';
        EXECUTE 'CREATE INDEX IF NOT EXISTS subtasks_title_trgm_idx '
            'ON subtasks USING gin (title gin_trgm_ops)';
    END IF;
END
$$;
