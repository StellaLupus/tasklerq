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

use crate::capture::breaker::CircuitBreaker;
use crate::capture::context::CaptureContext;
use crate::capture::draft::{self, NewCapture};
use crate::capture::llm::{self, Failure as LlmFailure};
use crate::capture::prompt::PROMPT_VERSION;
use crate::capture::schema::{ParseError, ParsedResponse};
use crate::domain::error::DomainError;
use crate::domain::models::{CaptureMode, Household, User};
use crate::home_assistant::AiTask;
use crate::storage::repositories::capture_jobs::{self, Failure, Job};
use crate::storage::repositories::llm_runs::{self, FAILURE, NewRun, SUCCESS};
use crate::storage::repositories::{households, users};

use super::bot::model::Screen;
use super::bot::{Bot, Recipient};

/// Попыток на задание: повторяются только сбои на стороне HA и сети.
pub const MAX_ATTEMPTS: u32 = 3;

/// Сколько разборов household может запустить за сутки по умолчанию.
pub const DEFAULT_DAILY_QUOTA: u32 = 200;

const FAILED: &str = "Не получилось разобрать сообщение. Проверьте текст и \
добавьте задачу вручную.";
const TOO_MANY: &str = "В сообщении больше 20 элементов. Разделите его на части \
и отправьте по отдельности.";
const NOTHING: &str = "Не нашёл в сообщении задач или покупок. Попробуйте сформулировать иначе.";
const CIRCUIT_OPEN: &str = "Автоматический разбор временно недоступен. \
Проверьте название и сохраните задачу вручную.";
const QUOTA_EXHAUSTED: &str = "На сегодня автоматический разбор закончился. \
Проверьте название и сохраните задачу вручную.";

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
    quota: u32,
    breaker: Arc<CircuitBreaker>,
}

/// Данные, которые читаются до вызова LLM.
struct Prepared {
    user: User,
    household: Household,
    context: CaptureContext,
}

/// Неудача разбора: что записать в очередь и что показать пользователю.
struct Issue<'a> {
    /// Исходный текст для ручной формы.
    text: &'a str,
    /// Короткий код без пользовательских данных.
    code: &'a str,
    /// Повтор может помочь.
    retryable: bool,
    /// Объяснение пользователю.
    message: &'a str,
    /// Запись журнала, если LLM уже вызывалась.
    run: Option<NewRun>,
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
            quota: DEFAULT_DAILY_QUOTA,
            breaker: Arc::new(CircuitBreaker::default()),
        }
    }

    /// Обрабатывать только задания одного чата (для тестов).
    #[must_use]
    pub fn only_chat(mut self, chat_id: i64) -> Self {
        self.only_chat = Some(chat_id);
        self
    }

    /// Суточная квота разборов на household.
    #[must_use]
    pub fn with_quota(mut self, quota: u32) -> Self {
        self.quota = quota;
        self
    }

    /// Общий предохранитель вызовов LLM.
    #[must_use]
    pub fn with_breaker(mut self, breaker: Arc<CircuitBreaker>) -> Self {
        self.breaker = breaker;
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

        // Квота и предохранитель проверяются до вызова: при отказе пользователь
        // сразу получает ручную форму с исходным текстом, а LLM не вызывается.
        if self.quota_exhausted(&job, now).await? {
            tracing::warn!(
                metric = "capture_quota_exhausted",
                "llm capture quota is exhausted"
            );
            return self
                .fail(
                    &job,
                    &prepared,
                    Issue {
                        text: &text,
                        code: "quota_exhausted",
                        retryable: false,
                        message: QUOTA_EXHAUSTED,
                        run: None,
                    },
                    now,
                )
                .await;
        }
        if !self.breaker.allow(now) {
            tracing::warn!(
                metric = "capture_circuit_open",
                "llm circuit breaker is open"
            );
            return self
                .fail(
                    &job,
                    &prepared,
                    Issue {
                        text: &text,
                        code: "circuit_open",
                        retryable: false,
                        message: CIRCUIT_OPEN,
                        run: None,
                    },
                    now,
                )
                .await;
        }

        let outcome = llm::generate(
            self.ai.as_ref(),
            &self.breaker,
            &prepared.context,
            &text,
            now,
        )
        .await;
        match outcome {
            Ok(generated) => {
                let now = generated.finished_at;
                let latency_ms = generated.latency_ms;
                let parsed = generated.parsed;
                if parsed.items.is_empty() {
                    let run = self.run_record(&job, &prepared, SUCCESS, None, latency_ms);
                    self.fail(
                        &job,
                        &prepared,
                        Issue {
                            text: &text,
                            code: "no_items",
                            retryable: false,
                            message: NOTHING,
                            run: Some(run),
                        },
                        now,
                    )
                    .await
                } else {
                    let run = self.run_record(&job, &prepared, SUCCESS, None, latency_ms);
                    self.finish(&job, &prepared, &text, parsed, now, run).await
                }
            }
            Err(failed) => {
                let now = failed.finished_at;
                let latency_ms = failed.latency_ms;
                match failed.failure {
                    LlmFailure::Ai(error) => {
                        tracing::warn!(
                            error = error.code(),
                            attempt = job.attempts,
                            "llm capture request failed"
                        );
                        let run = self.run_record(
                            &job,
                            &prepared,
                            FAILURE,
                            Some(error.code()),
                            latency_ms,
                        );
                        self.fail(
                            &job,
                            &prepared,
                            Issue {
                                text: &text,
                                code: error.code(),
                                retryable: error.is_retryable(),
                                message: FAILED,
                                run: Some(run),
                            },
                            now,
                        )
                        .await
                    }
                    LlmFailure::Parse(error) => {
                        tracing::warn!(error = error.code(), "llm capture response rejected");
                        let message = match error {
                            ParseError::TooManyItems { .. } => TOO_MANY,
                            _ => FAILED,
                        };
                        let run = self.run_record(
                            &job,
                            &prepared,
                            FAILURE,
                            Some(error.code()),
                            latency_ms,
                        );
                        self.fail(
                            &job,
                            &prepared,
                            Issue {
                                text: &text,
                                code: error.code(),
                                retryable: false,
                                message,
                                run: Some(run),
                            },
                            now,
                        )
                        .await
                    }
                }
            }
        }
    }

    /// Household исчерпал суточную квоту разборов.
    async fn quota_exhausted(&self, job: &Job, now: DateTime<Utc>) -> Result<bool, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        let used = llm_runs::count_since(
            &mut conn,
            job.household_id,
            now - chrono::Duration::hours(24),
        )
        .await?;
        Ok(used >= i64::from(self.quota))
    }

    /// Запись журнала об одном вызове LLM.
    fn run_record(
        &self,
        job: &Job,
        prepared: &Prepared,
        status: &'static str,
        error_code: Option<&'static str>,
        latency_ms: i64,
    ) -> NewRun {
        NewRun {
            household_id: job.household_id,
            user_id: Some(prepared.user.id),
            source: job.source,
            mode: CaptureMode::Draft,
            prompt_version: Some(PROMPT_VERSION),
            status,
            error_code,
            latency_ms,
            input_tokens: None,
            output_tokens: None,
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
            let context =
                draft::load_context_for(&mut conn, job.household_id, Some(user_id), now).await?;
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
        run: NewRun,
    ) -> Result<Pass, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        llm_runs::record(&mut tx, run, now).await?;
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

    /// Записать неудачу; когда повтор не нужен, показать ручную форму с
    /// исходным текстом, чтобы пользователь не потерял сообщение.
    async fn fail(
        &self,
        job: &Job,
        prepared: &Prepared,
        issue: Issue<'_>,
        now: DateTime<Utc>,
    ) -> Result<Pass, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        if let Some(run) = issue.run {
            llm_runs::record(&mut tx, run, now).await?;
        }
        let failure = capture_jobs::record_failure(
            &mut tx,
            job,
            issue.code,
            issue.retryable,
            MAX_ATTEMPTS,
            now,
        )
        .await?;
        match failure {
            Failure::Retry(at) => {
                tx.commit().await?;
                Ok(Pass::Retry(at))
            }
            Failure::Dead => {
                tracing::warn!(
                    metric = "capture_jobs_dead",
                    error = issue.code,
                    "capture job finished without a draft"
                );
                self.bot
                    .present_quick_add(
                        &mut tx,
                        now,
                        prepared.recipient(job.chat_id),
                        issue.text,
                        Some(issue.message.to_owned()),
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
