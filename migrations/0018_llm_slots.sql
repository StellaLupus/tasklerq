-- Учёт попыток слотов LLM и результат проверки слота.
--
-- `llm_runs` получает слот и номер попытки в разборе: каждая попытка — отдельная
-- строка, поэтому разбор через основной и резервный слот виден целиком. Старые
-- строки остаются без слота (`NULL`): квота считает их как разборы.
--
-- `llm_slot_checks` хранит последнюю проверку слота smoke-набором: отпечаток
-- действующей конфигурации (вид слота, модель, хост, хеш контракта, хеш набора,
-- таймаут) и его части, число пройденных и всех фраз, p95 задержки, отчёт и
-- время проверки. Токен и квота в отпечаток не входят. В отчёте нет текста
-- пользователя и ответов модели: только результаты сравнения по полям.

ALTER TABLE llm_runs
    ADD COLUMN slot text CHECK (slot IS NULL OR btrim(slot) <> ''),
    ADD COLUMN attempt smallint CHECK (attempt IS NULL OR attempt >= 1);

CREATE TABLE llm_slot_checks (
    slot text PRIMARY KEY CHECK (btrim(slot) <> ''),
    fingerprint text NOT NULL CHECK (btrim(fingerprint) <> ''),
    fingerprint_parts jsonb NOT NULL,
    passed integer NOT NULL CHECK (passed >= 0),
    total integer NOT NULL CHECK (total >= 0),
    p95_ms integer NOT NULL CHECK (p95_ms >= 0),
    report jsonb NOT NULL,
    checked_at timestamptz NOT NULL,
    CHECK (passed <= total)
);
