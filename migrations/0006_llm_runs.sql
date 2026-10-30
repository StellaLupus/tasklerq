-- Метаданные разборов LLM.
--
-- Исходный текст и ответ модели здесь не хранятся: только версия промпта,
-- источник, режим, статус, задержка и код ошибки. Этого достаточно для квоты,
-- метрик и разбора инцидентов без утечки пользовательских данных.
--
-- Записи старше `RETENTION_LLM_RUNS_DAYS` удаляет retention-job.

CREATE TABLE llm_runs (
    id uuid PRIMARY KEY,
    household_id uuid NOT NULL REFERENCES households (id) ON DELETE CASCADE,
    user_id uuid REFERENCES users (id) ON DELETE SET NULL,
    source capture_source NOT NULL,
    mode capture_mode NOT NULL,
    prompt_version text,
    status text NOT NULL CHECK (status IN ('success', 'failure')),
    error_code text,
    latency_ms integer NOT NULL CHECK (latency_ms >= 0),
    input_tokens integer CHECK (input_tokens >= 0),
    output_tokens integer CHECK (output_tokens >= 0),
    created_at timestamptz NOT NULL
);

CREATE INDEX llm_runs_household_created_idx
    ON llm_runs (household_id, created_at DESC);
CREATE INDEX llm_runs_created_idx
    ON llm_runs (created_at);
