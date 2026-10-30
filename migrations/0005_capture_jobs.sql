-- Очередь разбора текста через LLM.
--
-- Вызов Home Assistant занимает до 20 секунд, поэтому он идёт не в транзакции
-- воркера обновлений Telegram: бот ставит задание в очередь и сразу отвечает,
-- а отдельный воркер разбирает текст и редактирует сообщение карточкой
-- черновика. Задание захватывается арендой (`locked_until`): воркер, упавший
-- посреди вызова, не теряет задание — оно становится доступным после аренды.
--
-- `input_kind` и `payload` оставлены общими: для текста это
-- `{"text": "..."}`, другие виды входа (например, голосовые сообщения) добавят
-- свою форму payload без изменения таблицы.

CREATE TYPE capture_job_status AS ENUM ('pending', 'running', 'done', 'dead');

CREATE TABLE capture_jobs (
    id uuid PRIMARY KEY,
    household_id uuid NOT NULL REFERENCES households (id) ON DELETE CASCADE,
    user_id uuid REFERENCES users (id) ON DELETE SET NULL,
    chat_id bigint NOT NULL,
    source capture_source NOT NULL DEFAULT 'telegram',
    input_kind text NOT NULL DEFAULT 'text',
    payload jsonb NOT NULL,
    status capture_job_status NOT NULL DEFAULT 'pending',
    attempts integer NOT NULL DEFAULT 0,
    available_at timestamptz NOT NULL,
    locked_until timestamptz,
    last_error text,
    draft_id uuid REFERENCES capture_drafts (id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz
);

CREATE INDEX capture_jobs_ready_idx
    ON capture_jobs (available_at, id)
    WHERE status = 'pending';
CREATE INDEX capture_jobs_running_idx
    ON capture_jobs (locked_until)
    WHERE status = 'running';
CREATE INDEX capture_jobs_finished_idx
    ON capture_jobs (finished_at)
    WHERE finished_at IS NOT NULL;
