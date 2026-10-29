-- Индексы домена. GIN-индекс по search_vector обязателен; trigram-индекс
-- создаётся только при доступном расширении pg_trgm (иначе поиск использует
-- strpos/ILIKE и остаётся корректным, но медленнее).

CREATE UNIQUE INDEX users_telegram_active_idx
    ON users (telegram_user_id)
    WHERE deleted_at IS NULL;
CREATE INDEX users_household_idx
    ON users (household_id)
    WHERE deleted_at IS NULL;

CREATE UNIQUE INDEX lists_household_lower_name_idx
    ON lists (household_id, lower(name))
    WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX lists_household_kind_default_idx
    ON lists (household_id, kind)
    WHERE is_default AND deleted_at IS NULL;
CREATE INDEX lists_household_position_idx
    ON lists (household_id, position)
    WHERE deleted_at IS NULL;

CREATE INDEX items_household_list_status_idx
    ON items (household_id, list_id, status)
    WHERE deleted_at IS NULL;
CREATE INDEX items_household_assignee_idx
    ON items (household_id, assignee_user_id, status)
    WHERE deleted_at IS NULL;
CREATE INDEX items_due_idx
    ON items (household_id, due_at)
    WHERE deleted_at IS NULL AND due_at IS NOT NULL;
CREATE INDEX items_due_date_idx
    ON items (household_id, due_date)
    WHERE deleted_at IS NULL AND due_date IS NOT NULL;
CREATE INDEX items_search_idx
    ON items USING gin (search_vector);
CREATE INDEX items_capture_idx
    ON items (capture_id)
    WHERE capture_id IS NOT NULL;
CREATE INDEX items_household_updated_idx
    ON items (household_id, updated_at);
CREATE INDEX items_household_deleted_idx
    ON items (household_id, deleted_at)
    WHERE deleted_at IS NOT NULL;

CREATE UNIQUE INDEX tags_household_lower_name_idx
    ON tags (household_id, lower(name));
CREATE INDEX item_tags_tag_idx
    ON item_tags (tag_id);

CREATE INDEX subtasks_item_position_idx
    ON subtasks (item_id, position);

CREATE INDEX item_events_item_created_idx
    ON item_events (item_id, created_at DESC);
CREATE INDEX item_events_household_created_idx
    ON item_events (household_id, created_at DESC);
CREATE INDEX item_events_undo_idx
    ON item_events (undo_of)
    WHERE undo_of IS NOT NULL;

CREATE UNIQUE INDEX capture_idempotency_idx
    ON capture_drafts (household_id, source, idempotency_key)
    WHERE idempotency_key IS NOT NULL;
CREATE INDEX capture_source_applied_idx
    ON capture_drafts (household_id, source, applied_at DESC);

DO $$
BEGIN
    CREATE EXTENSION IF NOT EXISTS pg_trgm;
EXCEPTION
    WHEN OTHERS THEN
        RAISE NOTICE 'pg_trgm is not available: %', SQLERRM;
END
$$;

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_trgm') THEN
        EXECUTE 'CREATE INDEX IF NOT EXISTS items_title_trgm_idx '
            'ON items USING gin (title gin_trgm_ops)';
    END IF;
END
$$;
