//! Воркер разбора текста через LLM.
//!
//! Вызов Home Assistant занимает до 20 секунд, поэтому он идёт вне транзакции
//! и не держит ни соединение пула, ни блокировки: задание захватывается
//! арендой (`capture_jobs`), контекст читается коротким запросом, затем идёт
//! вызов, и только результат — черновик, завершение задания и экран в
//! `outbox` — фиксируется одной транзакцией. Если воркер упал посреди вызова,
//! аренда истечёт и задание подберёт другой проход.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::FutureExt;
use sqlx::PgPool;
use tokio::sync::{Notify, watch};

use crate::capture::context::CaptureContext;
use crate::capture::draft::{self, NewCapture};
use crate::capture::prompt::{self, PROMPT_VERSION};
use crate::capture::schema::{ParseError, ParsedResponse, parse_response};
use crate::domain::error::DomainError;
use crate::domain::models::{CaptureMode, Household, User};
use crate::home_assistant::{AiTask, GenerateData, MAX_GENERATE_TIMEOUT};
use crate::storage::repositories::capture_jobs::{self, Failure, Job};
use crate::storage::repositories::{households, users};

use super::bot::model::Screen;
use super::bot::{Bot, Recipient};

/// Попыток на задание: повторяются только сбои на стороне HA и сети.
pub const MAX_ATTEMPTS: u32 = 3;

const FAILED: &str = "Не получилось разобрать сообщение. Попробуйте ещё раз \
или добавьте задачу вручную через «➕ Добавить».";
const TOO_MANY: &str = "В сообщении больше 20 элементов. Разделите его на части \
и отправьте по отдельности.";
const NOTHING: &str = "Не нашёл в сообщении задач или покупок. Попробуйте сформулировать иначе.";

/// Итог одного прохода.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pass {
    /// Очередь пуста.
    Idle,
    /// Черновик создан, пользователь получил карточку.
    Drafted,
    /// Попытка не удалась; задание вернётся в очередь.
    Retry(DateTime<Utc>),
    /// Задание закончено без черновика; пользователь получил объяснение.
    Dead,
}

/// Воркер очереди `capture_jobs`.
pub struct CaptureWorker {
    pool: PgPool,
    ai: Arc<dyn AiTask>,
    bot: Bot,
    wake_outbox: Arc<Notify>,
    only_chat: Option<i64>,
}

/// Данные, которые читаются до вызова LLM.
struct Prepared {
    user: User,
    household: Household,
    context: CaptureContext,
}

impl Prepared {
    fn recipient(&self, chat_id: i64) -> Recipient<'_> {
        Recipient {
            user: &self.user,
            household: &self.household,
            chat_id,
        }
    }
}

impl CaptureWorker {
    /// Создать воркер. `wake_outbox` будит доставку после коммита.
    pub fn new(pool: PgPool, ai: Arc<dyn AiTask>, bot: Bot, wake_outbox: Arc<Notify>) -> Self {
        Self {
            pool,
            ai,
            bot,
            wake_outbox,
            only_chat: None,
        }
    }

    /// Обрабатывать только задания одного чата (для тестов).
    #[must_use]
    pub fn only_chat(mut self, chat_id: i64) -> Self {
        self.only_chat = Some(chat_id);
        self
    }

    /// Обработать одно готовое задание.
    pub async fn run_once(&self, now: DateTime<Utc>) -> Result<Pass, sqlx::Error> {
        let claimed = {
            let mut conn = self.pool.acquire().await?;
            capture_jobs::claim_next(&mut conn, now, MAX_ATTEMPTS, self.only_chat).await?
        };
        let Some(job) = claimed else {
            return Ok(Pass::Idle);
        };
        self.process(job, now).await
    }

    async fn process(&self, job: Job, now: DateTime<Utc>) -> Result<Pass, sqlx::Error> {
        let (Some(text), Some(user_id)) = (job.text().map(str::to_owned), job.user_id) else {
            // Неизвестный вид входа или автора удалили: разбирать нечего.
            return self.abandon(&job, "unusable_job", now).await;
        };

        let prepared = match self.prepare(&job, user_id, now).await? {
            Some(prepared) => prepared,
            None => return self.abandon(&job, "user_gone", now).await,
        };

        let instructions = prompt::build_instructions(&prepared.context, &text);
        let structure = prompt::response_structure();
        let request = GenerateData {
            task_name: prompt::TASK_NAME,
            instructions: &instructions,
            structure: &structure,
        };
        let started = std::time::Instant::now();
        let generated = self.ai.generate_data(&request, MAX_GENERATE_TIMEOUT).await;

        // Результат записывается моментом «после вызова»: к `now` прибавляется
        // длительность вызова, а не берутся новые часы, чтобы время под
        // управлением вызывающего (тесты, replay) не расходилось.
        let now = now + chrono::Duration::from_std(started.elapsed()).unwrap_or_default();
        match generated {
            Err(error) => {
                tracing::warn!(
                    error = error.code(),
                    attempt = job.attempts,
                    "llm capture request failed"
                );
                self.fail(
                    &job,
                    &prepared,
                    error.code(),
                    error.is_retryable(),
                    FAILED,
                    now,
                )
                .await
            }
            Ok(data) => match parse_response(&data) {
                Ok(parsed) if parsed.items.is_empty() => {
                    self.fail(&job, &prepared, "no_items", false, NOTHING, now)
                        .await
                }
                Ok(parsed) => self.finish(&job, &prepared, &text, parsed, now).await,
                Err(error) => {
                    tracing::warn!(error = error.code(), "llm capture response rejected");
                    let message = match error {
                        ParseError::TooManyItems { .. } => TOO_MANY,
                        _ => FAILED,
                    };
                    self.fail(&job, &prepared, error.code(), false, message, now)
                        .await
                }
            },
        }
    }

    /// Прочитать автора, household и контекст разбора. `None` — автора или
    /// household больше нет.
    async fn prepare(
        &self,
        job: &Job,
        user_id: uuid::Uuid,
        now: DateTime<Utc>,
    ) -> Result<Option<Prepared>, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        let loaded = async {
            let user = users::get(&mut conn, job.household_id, user_id).await?;
            let household = households::get(&mut conn, job.household_id).await?;
            let context = draft::load_context(&mut conn, job.household_id, now).await?;
            Ok::<_, DomainError>(Prepared {
                user,
                household,
                context,
            })
        }
        .await;
        match loaded {
            Ok(prepared) => Ok(Some(prepared)),
            Err(DomainError::NotFound) => Ok(None),
            Err(DomainError::Database(error)) => Err(error),
            Err(error) => {
                tracing::warn!(%error, "capture context could not be loaded");
                Ok(None)
            }
        }
    }

    /// Сохранить черновик и показать карточку.
    async fn finish(
        &self,
        job: &Job,
        prepared: &Prepared,
        text: &str,
        parsed: ParsedResponse,
        now: DateTime<Utc>,
    ) -> Result<Pass, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let created = draft::create(
            &mut tx,
            NewCapture {
                household_id: job.household_id,
                user_id: Some(prepared.user.id),
                chat_id: Some(job.chat_id),
                source: job.source,
                mode: CaptureMode::Draft,
                idempotency_key: None,
                request_hash: None,
                raw_text: text.to_owned(),
                parsed,
                fallback: false,
                prompt_version: Some(PROMPT_VERSION.to_owned()),
            },
            now,
        )
        .await
        .map_err(into_sqlx)?;
        capture_jobs::complete(&mut tx, job.id, Some(created.id), now).await?;
        self.bot
            .present_to(
                &mut tx,
                now,
                prepared.recipient(job.chat_id),
                Screen::Draft {
                    draft_id: created.id,
                },
                None,
            )
            .await?;
        tx.commit().await?;
        self.wake_outbox.notify_one();
        Ok(Pass::Drafted)
    }

    /// Записать неудачу; когда повтор не нужен, объяснить пользователю.
    async fn fail(
        &self,
        job: &Job,
        prepared: &Prepared,
        code: &str,
        retryable: bool,
        message: &str,
        now: DateTime<Utc>,
    ) -> Result<Pass, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let failure =
            capture_jobs::record_failure(&mut tx, job, code, retryable, MAX_ATTEMPTS, now).await?;
        match failure {
            Failure::Retry(at) => {
                tx.commit().await?;
                Ok(Pass::Retry(at))
            }
            Failure::Dead => {
                tracing::warn!(
                    metric = "capture_jobs_dead",
                    error = code,
                    "capture job finished without a draft"
                );
                self.bot
                    .present_to(
                        &mut tx,
                        now,
                        prepared.recipient(job.chat_id),
                        Screen::Menu,
                        Some(message.to_owned()),
                    )
                    .await?;
                tx.commit().await?;
                self.wake_outbox.notify_one();
                Ok(Pass::Dead)
            }
        }
    }

    /// Закончить задание, о котором некому сообщить.
    async fn abandon(
        &self,
        job: &Job,
        code: &str,
        now: DateTime<Utc>,
    ) -> Result<Pass, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        capture_jobs::record_failure(&mut conn, job, code, false, MAX_ATTEMPTS, now).await?;
        Ok(Pass::Dead)
    }

    /// Цикл воркера до сигнала остановки.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow() {
                return;
            }
            // Паника или сбой базы не должны останавливать разбор: упавшее
            // задание вернётся по истечении аренды.
            match AssertUnwindSafe(self.run_once(Utc::now()))
                .catch_unwind()
                .await
            {
                Ok(Ok(Pass::Idle | Pass::Retry(_) | Pass::Dead)) => {}
                Ok(Ok(Pass::Drafted)) => continue,
                Ok(Err(error)) => tracing::error!(%error, "capture pass failed"),
                Err(_) => tracing::error!("capture pass panicked"),
            }
            tokio::select! {
                _ = shutdown.changed() => {}
                () = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        }
    }
}

fn into_sqlx(error: DomainError) -> sqlx::Error {
    match error {
        DomainError::Database(error) => error,
        other => sqlx::Error::Protocol(format!("unexpected domain error: {other}")),
    }
}
