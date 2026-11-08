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
use sqlx::{PgConnection, PgPool};
use tokio::sync::{Notify, watch};

use crate::capture::breaker::CircuitBreaker;
use crate::capture::context::CaptureContext;
use crate::capture::draft::{self, NewCapture};
use crate::capture::llm::{self, Failure as LlmFailure};
use crate::capture::prompt::PROMPT_VERSION;
use crate::capture::schema::{ParseError, ParsedResponse};
use crate::capture::validate::validate_item;
use crate::domain::error::DomainError;
use crate::domain::models::{Actor, CaptureMode, Household, Item, ItemSource, ListKind, User};
use crate::domain::validation::validate_new_item;
use crate::home_assistant::{AiTask, MAX_GENERATE_TIMEOUT};
use crate::metrics;
use crate::security_audit::{Audit, LlmValidation};
use crate::storage::repositories::capture_jobs::{self, Failure, Job};
use crate::storage::repositories::llm_runs::{self, FAILURE, NewRun, SUCCESS};
use crate::storage::repositories::{households, items, users};
use crate::telegram::ru;

use super::bot::capture::{split_banner, split_titles};
use super::bot::model::{
    Action, Duplicate, DuplicateChoice, ItemDraft, Screen, duplicate_notice, same_unit,
};
use super::bot::{Banner, Bot, Recipient, into_sqlx};
use super::views;
use crate::telegram::rich::button;

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
    audit: Audit,
}

/// Данные, которые читаются до вызова LLM.
struct Prepared {
    user: User,
    household: Household,
    context: CaptureContext,
}

/// Неудача разбора: что записать в очередь и что показать пользователю.
struct Issue<'a> {
    /// Исходный текст для редактора.
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

/// Подставить список задач с активного экрана в пункты без указанного списка.
///
/// Промпт требует заполнять `target_list` всегда, поэтому модель подставляет
/// туда общий список задач, даже если пользователь список не называл. Такой
/// автоподбор узнаём по совпадению с текущим списком по умолчанию; если же
/// пользователь явно назвал этот список в тексте, оставляем его. Покупки и
/// явно выбранные другие списки не трогаются. Если список с экрана уже удалён
/// или заархивирован, работает обычный выбор по умолчанию.
fn apply_default_list(
    job: &Job,
    context: &CaptureContext,
    text: &str,
    parsed: &mut ParsedResponse,
) {
    let Some(list_id) = job.default_list_id() else {
        return;
    };
    let Some(list) = context
        .lists
        .iter()
        .find(|list| list.id == list_id && list.kind == ListKind::Tasks)
    else {
        return;
    };
    let fallback = context
        .default_list(ListKind::Tasks)
        .map(|list| fold(&list.name));
    let text = fold(text);
    for item in &mut parsed.items {
        if item.target_kind != ListKind::Tasks {
            continue;
        }
        match item.target_list.as_deref() {
            Some(target) if !target.trim().is_empty() => {
                let target = fold(target);
                let autofilled = fallback.as_deref() == Some(target.as_str());
                let named_by_user = text.contains(&target);
                if autofilled && !named_by_user {
                    item.target_list = Some(list.name.clone());
                }
            }
            _ => item.target_list = Some(list.name.clone()),
        }
    }
}

/// Сравнение названий списков без регистра и «ё».
fn fold(value: &str) -> String {
    value.trim().to_lowercase().replace('ё', "е")
}

/// Позиция в баннере: название и количество, как в списке.
fn position_label(item: &Item) -> String {
    ru::position_label(&item.title, item.quantity, item.unit.as_deref())
}

/// Текст баннера после прямого добавления.
fn result_banner(
    target_name: &str,
    added: &[String],
    duplicates: &[DuplicateChoice],
    on_list: bool,
    has_draft: bool,
) -> Option<String> {
    let mut parts = Vec::new();
    if !added.is_empty() {
        if on_list || has_draft {
            parts.push(format!("Добавлено: {}", added.join(", ")));
        } else {
            parts.push(format!("Добавлено в «{target_name}»: {}", added.join(", ")));
        }
    }
    if let Some(notice) = duplicate_notice(duplicates) {
        parts.push(notice);
    }
    if parts.is_empty() {
        if has_draft {
            None
        } else {
            Some("Не нашёл в сообщении покупок.".to_owned())
        }
    } else {
        Some(parts.join(" "))
    }
}

/// Активный экран чата, если его состояние читается.
async fn active_screen(
    conn: &mut PgConnection,
    chat_id: i64,
) -> Result<Option<Screen>, sqlx::Error> {
    Ok(views::get(conn, chat_id)
        .await?
        .and_then(|view| serde_json::from_value::<Screen>(view.state).ok()))
}

impl CaptureWorker {
    /// Создать воркер. `wake_outbox` будит доставку после коммита, `audit` —
    /// общий журнал событий процесса.
    pub fn new(
        pool: PgPool,
        ai: Arc<dyn AiTask>,
        bot: Bot,
        wake_outbox: Arc<Notify>,
        audit: Audit,
    ) -> Self {
        Self {
            audit,
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

        let Some(prepared) = self.prepare(&job, user_id, now).await? else {
            return self.abandon(&job, "user_gone", now).await;
        };

        // Квота и предохранитель проверяются до вызова: при отказе пользователь
        // сразу получает редактор с исходным текстом, а LLM не вызывается.
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
            MAX_GENERATE_TIMEOUT,
        )
        .await;
        match outcome {
            Ok(generated) => {
                let now = generated.finished_at;
                let latency_ms = generated.latency_ms;
                let parsed = generated.parsed;
                let run = self.run_record(&job, &prepared, SUCCESS, None, latency_ms);
                if parsed.items.is_empty() {
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
                } else if let Some(target) =
                    job.target()
                        .filter(|target| target.direct)
                        .filter(|target| {
                            prepared.context.lists.iter().any(|list| {
                                list.id == target.list_id && list.kind == ListKind::Shopping
                            })
                        })
                {
                    self.finish_direct(&job, &prepared, &text, parsed, target.list_id, now, run)
                        .await
                } else {
                    self.finish(&job, &prepared, &text, parsed, now, run).await
                }
            }
            Err(failed) => {
                let now = failed.finished_at;
                let failure = failed.failure;
                match &failure {
                    LlmFailure::Ai(error) => tracing::warn!(
                        error = error.code(),
                        attempt = job.attempts,
                        "llm capture request failed"
                    ),
                    LlmFailure::Parse(error) => {
                        tracing::warn!(error = error.code(), "llm capture response rejected");
                        self.audit
                            .llm_validation_failed(
                                LlmValidation {
                                    source: "telegram",
                                    stage: "parse",
                                    code: error.code(),
                                    prompt_version: PROMPT_VERSION,
                                    household_id: job.household_id,
                                    actor: crate::domain::models::Actor::user(
                                        prepared.user.id,
                                        prepared.user.role,
                                    ),
                                    capture_id: None,
                                    job_id: Some(job.id),
                                },
                                now,
                            )
                            .await;
                    }
                }
                let message =
                    if matches!(failure, LlmFailure::Parse(ParseError::TooManyItems { .. })) {
                        TOO_MANY
                    } else {
                        FAILED
                    };
                let code = failure.code();
                let run = self.run_record(&job, &prepared, FAILURE, Some(code), failed.latency_ms);
                self.fail(
                    &job,
                    &prepared,
                    Issue {
                        text: &text,
                        code,
                        retryable: failure.is_retryable(),
                        message,
                        run: Some(run),
                    },
                    now,
                )
                .await
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
        let mut parsed = parsed;
        apply_default_list(job, &prepared.context, text, &mut parsed);
        let mut tx = self.pool.begin().await?;
        llm_runs::record(&mut tx, run, now).await?;
        let created = draft::create(
            &mut tx,
            NewCapture {
                id: None,
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
        metrics::capture("telegram", "drafted");
        Ok(Pass::Drafted)
    }

    /// Сохранить прямое добавление: покупки в целевой список, остальное в
    /// черновик, всё одной транзакцией.
    #[allow(clippy::too_many_arguments)]
    async fn finish_direct(
        &self,
        job: &Job,
        prepared: &Prepared,
        text: &str,
        parsed: ParsedResponse,
        target_list_id: uuid::Uuid,
        now: DateTime<Utc>,
        run: NewRun,
    ) -> Result<Pass, sqlx::Error> {
        let context = &prepared.context;
        let target_name = context
            .lists
            .iter()
            .find(|list| list.id == target_list_id)
            .map(|list| list.name.clone())
            .unwrap_or_default();

        let ParsedResponse {
            items: parsed_items,
            clarifications,
        } = parsed;
        let mut direct = Vec::new();
        let mut draft_items = Vec::new();
        for mut item in parsed_items {
            if item.target_kind == ListKind::Shopping {
                // Экран задаёт целевой список: разбор не должен уводить
                // покупку в другой список.
                item.target_list = Some(target_name.clone());
                direct.push(item);
            } else {
                draft_items.push(item);
            }
        }

        let mut tx = self.pool.begin().await?;
        llm_runs::record(&mut tx, run, now).await?;
        let actor = Actor::user(prepared.user.id, prepared.user.role);
        let timezone = context.timezone.name();
        let today = context.today();
        let mut existing = items::list_by_list(&mut tx, job.household_id, target_list_id)
            .await
            .map_err(into_sqlx)?;

        let mut created: Vec<Item> = Vec::new();
        let mut duplicates: Vec<DuplicateChoice> = Vec::new();
        for raw in direct {
            let checked = validate_item(context, ItemSource::Telegram, raw.clone());
            let Some(mut new_item) = checked.item else {
                // Невалидную покупку не теряем: её можно поправить в черновике.
                draft_items.push(raw);
                continue;
            };
            // Имя списка могло совпасть с другим: экран задаёт идентификатор.
            new_item.list_id = target_list_id;
            if let Some(existing_item) = Duplicate::find(&existing, &new_item.title) {
                let quantity = new_item.quantity.unwrap_or(1.0);
                let unit = new_item.unit.clone();
                let effective_unit = unit.as_deref().or(existing_item.unit.as_deref());
                if let Some(pending) = duplicates.iter_mut().find(|pending| {
                    pending.existing.item_id == existing_item.item_id
                        && same_unit(
                            pending.unit.as_deref().or(pending.existing.unit.as_deref()),
                            effective_unit,
                        )
                }) {
                    pending.quantity += quantity;
                } else {
                    duplicates.push(DuplicateChoice {
                        existing: existing_item,
                        title: new_item.title.clone(),
                        quantity,
                        unit,
                    });
                }
                continue;
            }
            let assignee = new_item.assignee_user_id;
            let validated = validate_new_item(ListKind::Shopping, timezone, new_item, today)
                .map_err(into_sqlx)?;
            let item = items::create(
                &mut tx,
                job.household_id,
                &actor,
                target_list_id,
                validated,
                assignee,
                now,
            )
            .await
            .map_err(into_sqlx)?;
            existing.push(item.clone());
            created.push(item);
        }

        let draft = if draft_items.is_empty() {
            None
        } else {
            let draft = draft::create(
                &mut tx,
                NewCapture {
                    id: None,
                    household_id: job.household_id,
                    user_id: Some(prepared.user.id),
                    chat_id: Some(job.chat_id),
                    source: job.source,
                    mode: CaptureMode::Draft,
                    idempotency_key: None,
                    request_hash: None,
                    raw_text: text.to_owned(),
                    parsed: ParsedResponse {
                        items: draft_items,
                        clarifications,
                    },
                    fallback: false,
                    prompt_version: Some(PROMPT_VERSION.to_owned()),
                },
                now,
            )
            .await
            .map_err(into_sqlx)?;
            Some(draft)
        };

        capture_jobs::complete(&mut tx, job.id, draft.as_ref().map(|draft| draft.id), now).await?;

        let active = active_screen(&mut tx, job.chat_id).await?;
        let added: Vec<String> = created.iter().map(position_label).collect();
        let mut buttons = duplicates
            .split_first()
            .map(|(first, rest)| first.buttons(target_list_id, rest.to_vec()))
            .unwrap_or_default();
        if !created.is_empty() {
            buttons.push(button(
                "↩️ Отменить",
                Action::AddedUndo {
                    item_ids: created.iter().map(|item| item.id).collect(),
                },
            ));
        }
        let on_list = matches!(
            &active,
            Some(Screen::List { list_id, .. }) if *list_id == target_list_id
        );
        let text = result_banner(&target_name, &added, &duplicates, on_list, draft.is_some());
        let banner = text.map(|text| {
            if buttons.is_empty() {
                Banner::text(text)
            } else {
                Banner::with_buttons(text, buttons)
            }
        });

        let screen = if let Some(draft) = &draft {
            Screen::Draft { draft_id: draft.id }
        } else {
            active.unwrap_or(Screen::Menu)
        };

        self.bot
            .present_to(
                &mut tx,
                now,
                prepared.recipient(job.chat_id),
                screen,
                banner,
            )
            .await?;
        tx.commit().await?;
        self.wake_outbox.notify_one();
        metrics::capture("telegram", "added");
        Ok(Pass::Drafted)
    }

    /// Добавить покупки без разбора в целевой список: воркер исчерпал
    /// попытки или получил ошибку, после которой разбор уже не поможет.
    /// `false` — целевой список больше не виден, работает обычная ошибка.
    async fn finish_direct_split(
        &self,
        tx: &mut PgConnection,
        job: &Job,
        prepared: &Prepared,
        text: &str,
        target_list_id: uuid::Uuid,
        now: DateTime<Utc>,
    ) -> Result<bool, sqlx::Error> {
        let context = &prepared.context;
        let Some(target_name) = context
            .lists
            .iter()
            .find(|list| list.id == target_list_id && list.kind == ListKind::Shopping)
            .map(|list| list.name.clone())
        else {
            return Ok(false);
        };
        let actor = Actor::user(prepared.user.id, prepared.user.role);
        let timezone = context.timezone.name();
        let today = context.today();
        let mut existing = items::list_by_list(&mut *tx, job.household_id, target_list_id)
            .await
            .map_err(into_sqlx)?;

        let mut created: Vec<Item> = Vec::new();
        let mut duplicates: Vec<DuplicateChoice> = Vec::new();
        for title in split_titles(text) {
            let draft = ItemDraft {
                title,
                quantity: Some(1.0),
                ..ItemDraft::default()
            };
            let mut new_item = draft
                .to_new_item(target_list_id, context.timezone, ListKind::Shopping)
                .map_err(into_sqlx)?;
            new_item.list_id = target_list_id;
            if let Some(existing_item) = Duplicate::find(&existing, &new_item.title) {
                let quantity = new_item.quantity.unwrap_or(1.0);
                let unit = new_item.unit.clone();
                let effective_unit = unit.as_deref().or(existing_item.unit.as_deref());
                if let Some(pending) = duplicates.iter_mut().find(|pending| {
                    pending.existing.item_id == existing_item.item_id
                        && same_unit(
                            pending.unit.as_deref().or(pending.existing.unit.as_deref()),
                            effective_unit,
                        )
                }) {
                    pending.quantity += quantity;
                } else {
                    duplicates.push(DuplicateChoice {
                        existing: existing_item,
                        title: new_item.title.clone(),
                        quantity,
                        unit,
                    });
                }
                continue;
            }
            let assignee = new_item.assignee_user_id;
            let validated = match validate_new_item(ListKind::Shopping, timezone, new_item, today) {
                Ok(validated) => validated,
                // Слишком длинную часть не теряем из-за остальных.
                Err(DomainError::Validation(_)) => continue,
                Err(error) => return Err(into_sqlx(error)),
            };
            let item = items::create(
                &mut *tx,
                job.household_id,
                &actor,
                target_list_id,
                validated,
                assignee,
                now,
            )
            .await
            .map_err(into_sqlx)?;
            existing.push(item.clone());
            created.push(item);
        }

        let active = active_screen(&mut *tx, job.chat_id).await?;
        let titles: Vec<String> = created.iter().map(|item| item.title.clone()).collect();
        let mut buttons = duplicates
            .split_first()
            .map(|(first, rest)| first.buttons(target_list_id, rest.to_vec()))
            .unwrap_or_default();
        if !created.is_empty() {
            buttons.push(button(
                "↩️ Отменить",
                Action::AddedUndo {
                    item_ids: created.iter().map(|item| item.id).collect(),
                },
            ));
        }
        let on_list = matches!(
            &active,
            Some(Screen::List { list_id, .. }) if *list_id == target_list_id
        );
        let text = split_banner(&target_name, &titles, &duplicates, on_list);
        let banner = if buttons.is_empty() {
            Banner::text(text)
        } else {
            Banner::with_buttons(text, buttons)
        };
        let screen = active.unwrap_or(Screen::Menu);
        self.bot
            .present_to(
                &mut *tx,
                now,
                prepared.recipient(job.chat_id),
                screen,
                Some(banner),
            )
            .await?;
        Ok(true)
    }

    /// Записать неудачу; когда повтор не нужен, показать редактор с
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
                if let Some(target) = job.target().filter(|target| target.direct)
                    && !matches!(issue.code, "no_items" | "too_many_items")
                    && self
                        .finish_direct_split(
                            &mut tx,
                            job,
                            prepared,
                            issue.text,
                            target.list_id,
                            now,
                        )
                        .await?
                {
                    tx.commit().await?;
                    self.wake_outbox.notify_one();
                    metrics::capture("telegram", "added");
                    return Ok(Pass::Dead);
                }
                if let Some(target) = job.target().filter(|target| target.direct)
                    && let Some(Screen::List {
                        list_id,
                        page,
                        edit,
                        from_lists,
                    }) = active_screen(&mut tx, job.chat_id).await?
                    && list_id == target.list_id
                {
                    self.bot
                        .present_to(
                            &mut tx,
                            now,
                            prepared.recipient(job.chat_id),
                            Screen::List {
                                list_id,
                                page,
                                edit,
                                from_lists,
                            },
                            Some(Banner::text(issue.message)),
                        )
                        .await?;
                    tx.commit().await?;
                    self.wake_outbox.notify_one();
                    metrics::capture("telegram", "failed");
                    return Ok(Pass::Dead);
                }
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
                metrics::capture("telegram", "failed");
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
        metrics::capture("telegram", "failed");
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
