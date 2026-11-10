-- Конец события: `due_at`/`due_date` — начало, новые колонки — конец.
-- Конец того же вида, что начало: `end_at` только при `due_at`, `end_date`
-- только при `due_date`; `end_date` хранится включительно. Правило «конец
-- только у событий» проверяет домен: ограничение БД не видит `lists.kind`.

ALTER TABLE items
    ADD COLUMN end_at timestamptz,
    ADD COLUMN end_date date,
    ADD CONSTRAINT items_end_exclusive
        CHECK (end_at IS NULL OR end_date IS NULL),
    ADD CONSTRAINT items_end_at_after_due_at
        CHECK (end_at IS NULL OR (due_at IS NOT NULL AND end_at > due_at)),
    ADD CONSTRAINT items_end_date_not_before_due_date
        CHECK (end_date IS NULL OR (due_date IS NOT NULL AND end_date >= due_date));

COMMENT ON COLUMN items.end_at IS
    'Конец события со временем; NULL — конец не задан';
COMMENT ON COLUMN items.end_date IS
    'Последний день вседневного события включительно; NULL — конец не задан';

-- Список «Календарь» каждому неудалённому household, у которого ещё нет списка
-- событий. Позиция — после существующих неудалённых списков. Если имя
-- «Календарь» уже занято списком другого kind, берётся «Календарь (события)»,
-- затем «Календарь (события 2)» и так далее. Идентификатор генерирует
-- gen_random_uuid() (есть с PostgreSQL 13), поэтому минимум 15 не нарушается.
-- Повторный запуск второй список не создаёт.
INSERT INTO lists (
    id, household_id, kind, name, is_default, is_archived, position,
    created_at, updated_at
)
SELECT
    gen_random_uuid(),
    h.id,
    'events',
    chosen.name,
    true,
    false,
    COALESCE((
        SELECT MAX(l.position) + 1
        FROM lists l
        WHERE l.household_id = h.id AND l.deleted_at IS NULL
    ), 0),
    now(),
    now()
FROM households h
CROSS JOIN LATERAL (
    SELECT candidate.name
    FROM (
        SELECT 'Календарь'::text AS name, 0 AS attempt, 0 AS ordinal
        UNION ALL
        SELECT 'Календарь (события)'::text, 1, 0
        UNION ALL
        SELECT ('Календарь (события ' || n || ')')::text, 2, n
        FROM generate_series(2, 100) AS n
    ) AS candidate
    WHERE NOT EXISTS (
        SELECT 1
        FROM lists l
        WHERE l.household_id = h.id
          AND l.deleted_at IS NULL
          AND lower(l.name) = lower(candidate.name)
    )
    ORDER BY candidate.attempt, candidate.ordinal
    LIMIT 1
) AS chosen
WHERE h.deleted_at IS NULL
  AND NOT EXISTS (
      SELECT 1
      FROM lists l
      WHERE l.household_id = h.id
        AND l.kind = 'events'
        AND l.deleted_at IS NULL
  );
