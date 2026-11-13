-- Стандарт сравнения названий при проверке слота LLM.
--
-- Одна строка на слот: `strict` (только основы слов) или `normalized` (ещё вид
-- глагола и синонимы). Строку создаёт команда `capture-eval --titles`;
-- отсутствие строки означает `strict`, поэтому миграция не заполняет таблицу и
-- прежнее поведение сохраняется.
--
-- Стандарт входит в отпечаток проверки (`llm_slot_checks`), поэтому его смена
-- делает сохранённую проверку устаревшей, как смена модели или таймаута.

CREATE TABLE llm_slot_settings (
    slot text PRIMARY KEY CHECK (btrim(slot) <> ''),
    titles text NOT NULL DEFAULT 'strict'
        CHECK (titles IN ('strict', 'normalized')),
    updated_at timestamptz NOT NULL DEFAULT now()
);
