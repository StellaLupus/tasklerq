-- Слот перенесённого вхождения: дата правила, которую занимает текущий срок.
-- NULL — срок стоит на своей дате правила (обычная серия или перенос вернулся
-- на слот). Колонка аддитивная: старый код её не видит.

ALTER TABLE items ADD COLUMN recurrence_occurrence_date date NULL;

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
    )
    OR (
        recurrence_freq IS NOT NULL
        AND recurrence_interval BETWEEN 1 AND 365
        AND recurrence_timezone IS NOT NULL
        AND recurrence_anchor_date IS NOT NULL
    )
);
