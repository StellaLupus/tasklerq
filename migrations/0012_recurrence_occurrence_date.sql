-- Слот перенесённого вхождения: дата правила, которую занимает текущий срок,
-- и время суток серии (локальное время в recurrence_timezone). NULL в дате —
-- срок стоит на своей дате правила (обычная серия или перенос вернулся на
-- слот); NULL во времени при заданной дате — вседневная серия. Колонки
-- аддитивные: старый код их не видит.

ALTER TABLE items ADD COLUMN recurrence_occurrence_date date NULL;
ALTER TABLE items ADD COLUMN recurrence_occurrence_time time NULL;

COMMENT ON COLUMN items.recurrence_occurrence_date IS
    'Дата правила, которую занимает перенесённое вхождение; NULL — срок на своей дате правила';
COMMENT ON COLUMN items.recurrence_occurrence_time IS
    'Локальное время серии в recurrence.timezone при заданной дате слота; NULL — вседневная серия';

ALTER TABLE items DROP CONSTRAINT items_recurrence_shape;

ALTER TABLE items ADD CONSTRAINT items_recurrence_shape CHECK (
    (
        recurrence_freq IS NULL
        AND recurrence_interval IS NULL
        AND recurrence_byweekday IS NULL
        AND recurrence_until IS NULL
        AND recurrence_timezone IS NULL
        AND recurrence_anchor_date IS NULL
        AND recurrence_month_rule IS NULL
        AND recurrence_occurrence_date IS NULL
        AND recurrence_occurrence_time IS NULL
    )
    OR (
        recurrence_freq IS NOT NULL
        AND recurrence_interval BETWEEN 1 AND 365
        AND recurrence_timezone IS NOT NULL
        AND recurrence_anchor_date IS NOT NULL
        AND (
            recurrence_occurrence_time IS NULL
            OR recurrence_occurrence_date IS NOT NULL
        )
    )
);
