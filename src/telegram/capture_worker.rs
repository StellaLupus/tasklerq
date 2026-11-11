//! Воркер разбора текста через LLM.
//!
//! Разбор через цепочку слотов занимает до 20 секунд, поэтому он идёт вне
//! транзакции и не держит ни соединение пула, ни блокировки: задание
//! захватывается арендой (`capture_jobs`), контекст читается коротким запросом,
//! затем идёт вызов, и только результат — черновик, завершение задания и экран
//! в `outbox` — фиксируется одной транзакцией. Если воркер упал посреди
//! вызова, аренда истечёт и задание подберёт другой проход.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::FutureExt;
use sqlx::{PgConnection, PgPool};
use tokio::sync::{Notify, watch};

use crate::capture::context::CaptureContext;
use crate::capture::draft::{self, NewCapture};
use crate::capture::llm::{self, Failure as LlmFailure};
use crate::capture::prompt::PROMPT_VERSION;
use crate::capture::schema::{Clarification, MAX_ITEMS, ParseError, ParsedResponse, RawItem};
use crate::capture::validate::validate_item;
use crate::domain::error::DomainError;
use crate::domain::models::{Actor, CaptureMode, Household, Item, ItemSource, ListKind, User};
use crate::domain::validation::validate_new_item;
use crate::llm::Chain;
use crate::metrics;
use crate::security_audit::{Audit, LlmValidation};
use crate::storage::repositories::capture_jobs::{self, Failure, Job};
use crate::storage::repositories::llm_runs::{self, NewRun, RunFields, runs_from_attempts};
use crate::storage::repositories::{households, items, users};
use crate::telegram::ru;

use super::bot::additions::add_parts;
use super::bot::capture::{split_banner, split_titles};
use super::bot::model::{
    Action, DuplicateChoice, Screen, addition_rows, duplicate_notice, merge_duplicate_choice,
    plan_additions,
};
use super::bot::{Banner, Bot, Recipient, into_sqlx};
use super::views;
use crate::telegram::rich::button;

/// Попыток на задание: повторяются только сбои транспорта и сервера слота.
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
    llm: Arc<Chain>,
    bot: Bot,
    wake_outbox: Arc<Notify>,
    only_chat: Option<i64>,
    quota: u32,
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
    /// Записи журнала по выполненным попыткам; пусто, если LLM не вызывалась.
    runs: Vec<NewRun>,
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
                let named_by_user = names_list(&text, &target);
                if autofilled && !named_by_user {
                    item.target_list = Some(list.name.clone());
                }
            }
            _ => item.target_list = Some(list.name.clone()),
        }
    }
}

/// Пересчитать номера уточнений после прямого добавления.
///
/// Часть элементов разбора создаётся сразу, а остальные попадают в черновик в
/// другом порядке; вопросы о созданных элементах отбрасываются, а номера
/// оставшихся указывают на новый порядок.
fn reindex_clarifications(
    parsed_len: usize,
    draft_items: &[(usize, RawItem)],
    clarifications: Vec<Clarification>,
) -> Vec<Clarification> {
    let mut positions = vec![None; parsed_len];
    for (new_index, (old_index, _)) in draft_items.iter().enumerate() {
        positions[*old_index] = Some(new_index);
    }
    clarifications
        .into_iter()
        .filter_map(|clarification| {
            let item = positions.get(clarification.item).copied().flatten()?;
            Some(Clarification {
                item,
                ..clarification
            })
        })
        .collect()
}

/// Сравнение названий списков без регистра и «ё».
fn fold(value: &str) -> String {
    value.trim().to_lowercase().replace('ё', "е")
}

/// Слова текста без пунктуации: название списка ищется по границам слов.
fn words(value: &str) -> Vec<String> {
    value
        .split(|ch: char| !ch.is_alphanumeric() && ch != '-')
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Пользователь назвал список явно: «в X», «в список X» или «list X».
///
/// Одного совпадения слова мало: «добавить задачи на выходные» — это обычное
/// значение слова «задачи», а не выбор списка «Задачи». Поэтому названием
/// списка считается только слово (или последовательность слов) после явной
/// конструкции выбора.
fn names_list(text: &str, name: &str) -> bool {
    let text = words(text);
    let name = words(name);
    if name.is_empty() || text.len() < name.len() {
        return false;
    }
    (0..=text.len() - name.len()).any(|start| {
        if text[start..start + name.len()] != name[..] {
            return false;
        }
        let before = &text[..start];
        match before.last().map(String::as_str) {
            Some("в" | "list") => true,
            Some("список") => before.len() >= 2 && before[before.len() - 2] == "в",
            _ => false,
        }
    })
}

/// Позиция в баннере: название и количество, как в списке.
fn position_label(item: &Item) -> String {
    ru::position_label(&item.title, item.quantity, item.unit.as_deref())
}

/// Что показывает результат прямого добавления: карточку, баннер с
/// черновиком или баннер без него.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectResult {
    /// Черновик открывается карточкой.
    Card,
    /// Экран остаётся, а баннер приглашает открыть черновик.
    DraftBanner,
    /// Черновика нет, экран остаётся с баннером добавления.
    PlainBanner,
}

/// Текст баннера после прямого добавления.
fn result_banner(
    target_name: &str,
    added: &[String],
    duplicates: &[DuplicateChoice],
    on_list: bool,
    result: DirectResult,
) -> Option<String> {
    let mut parts = Vec::new();
    if !added.is_empty() {
        if on_list {
            parts.push(format!("Добавлено: {}", added.join(", ")));
        } else {
            parts.push(format!("Добавлено в «{target_name}»: {}", added.join(", ")));
        }
    }
    if let Some(notice) = duplicate_notice(duplicates) {
        parts.push(notice);
    }
    if result == DirectResult::DraftBanner {
        parts.push("Черновик готов".to_owned());
    }
    if parts.is_empty() {
        if result == DirectResult::Card {
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

/// Активный экран принадлежит этому результату: статус этого задания или
/// карточка этого же черновика. Только на нём карточка заменяет экран.
fn on_own_screen(active: &Option<Screen>, job_id: uuid::Uuid, draft_id: uuid::Uuid) -> bool {
    match active {
        Some(Screen::Processing {
            job_id: Some(active_job),
        }) => *active_job == job_id,
        Some(Screen::Draft {
            draft_id: active_draft,
        }) => *active_draft == draft_id,
        _ => false,
    }
}

impl CaptureWorker {
    /// Создать воркер. `wake_outbox` будит доставку после коммита, `audit` —
    /// общий журнал событий процесса.
    pub fn new(
        pool: PgPool,
        llm: Arc<Chain>,
        bot: Bot,
        wake_outbox: Arc<Notify>,
        audit: Audit,
    ) -> Self {
        Self {
            audit,
            pool,
            llm,
            bot,
            wake_outbox,
            only_chat: None,
            quota: DEFAULT_DAILY_QUOTA,
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
                        runs: Vec::new(),
                    },
                    now,
                )
                .await;
        }

        let outcome = llm::generate(
            self.llm.as_ref(),
            &prepared.context,
            &text,
            now,
            llm::DEFAULT_BUDGET,
        )
        .await;
        let llm::Outcome { result, attempts } = outcome;

        // Предохранители всех слотов разомкнуты — попыток нет, и строки
        // `llm_runs` не пишется: LLM не вызывалась, а квота считает вызовы.
        let runs = runs_from_attempts(self.run_fields(&job, &prepared), &attempts);

        match result {
            Ok(generated) => {
                let now = generated.finished_at;
                let parsed = generated.parsed;
                if parsed.items.is_empty() {
                    self.fail(
                        &job,
                        &prepared,
                        Issue {
                            text: &text,
                            code: "no_items",
                            retryable: false,
                            message: NOTHING,
                            runs,
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
                    self.finish_direct(&job, &prepared, &text, parsed, target.list_id, now, runs)
                        .await
                } else {
                    self.finish(&job, &prepared, &text, parsed, now, runs).await
                }
            }
            Err(failed) => {
                let now = failed.finished_at;
                let failure = failed.failure;
                if let Some(code) = failure.validation_code() {
                    tracing::warn!(error = code, "llm capture response rejected");
                    self.audit
                        .llm_validation_failed(
                            LlmValidation {
                                source: "telegram",
                                stage: "parse",
                                code,
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
                } else {
                    tracing::warn!(
                        error = failure.code(),
                        attempt = job.attempts,
                        "llm capture request failed"
                    );
                }
                let message = match &failure {
                    LlmFailure::User(ParseError::TooManyItems { .. }) => TOO_MANY,
                    LlmFailure::Degraded {
                        reason: "circuit_open",
                        ..
                    } => CIRCUIT_OPEN,
                    _ => FAILED,
                };
                let code = failure.code();
                self.fail(
                    &job,
                    &prepared,
                    Issue {
                        text: &text,
                        code,
                        retryable: failure.is_retryable(),
                        message,
                        runs,
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

    /// Общие поля журнала одного разбора.
    fn run_fields(&self, job: &Job, prepared: &Prepared) -> RunFields {
        RunFields {
            household_id: job.household_id,
            user_id: Some(prepared.user.id),
            source: job.source,
            mode: CaptureMode::Draft,
            prompt_version: Some(PROMPT_VERSION),
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
        runs: Vec<NewRun>,
    ) -> Result<Pass, sqlx::Error> {
        let mut parsed = parsed;
        apply_default_list(job, &prepared.context, text, &mut parsed);
        let mut tx = self.pool.begin().await?;
        for run in runs {
            llm_runs::record(&mut tx, run, now).await?;
        }
        let created = draft::append_or_create(
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
        let draft = Screen::Draft {
            draft_id: created.id,
        };
        let active = active_screen(&mut tx, job.chat_id).await?;
        if on_own_screen(&active, job.id, created.id) {
            self.bot
                .present_to(&mut tx, now, prepared.recipient(job.chat_id), draft, None)
                .await?;
        } else if let Some(screen) = active {
            let banner = Banner::with_buttons(
                "Черновик готов",
                vec![button("Открыть", Action::Go { to: draft })],
            );
            self.bot
                .present_to(
                    &mut tx,
                    now,
                    prepared.recipient(job.chat_id),
                    screen,
                    Some(banner),
                )
                .await?;
        } else {
            // Активного экрана нет: показывать баннер не на чем.
            self.bot
                .present_to(&mut tx, now, prepared.recipient(job.chat_id), draft, None)
                .await?;
        }
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
        runs: Vec<NewRun>,
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
            ..
        } = parsed;
        let parsed_len = parsed_items.len();
        let mut direct = Vec::new();
        let mut draft_items = Vec::new();
        for (index, mut item) in parsed_items.into_iter().enumerate() {
            if item.target_kind == ListKind::Shopping {
                // Экран задаёт целевой список: разбор не должен уводить
                // покупку в другой список.
                item.target_list = Some(target_name.clone());
                direct.push((index, item));
            } else {
                draft_items.push((index, item));
            }
        }

        let mut tx = self.pool.begin().await?;
        for run in runs {
            llm_runs::record(&mut tx, run, now).await?;
        }
        let actor = Actor::user(prepared.user.id, prepared.user.role);
        let timezone = context.timezone.name();
        let today = context.today();
        let mut existing = items::list_by_list(&mut tx, job.household_id, target_list_id)
            .await
            .map_err(into_sqlx)?;

        let mut new_items = Vec::new();
        for (index, raw) in direct {
            let checked = validate_item(context, ItemSource::Telegram, raw.clone());
            let Some(mut new_item) = checked.item else {
                // Невалидную покупку не теряем: её можно поправить в черновике.
                draft_items.push((index, raw));
                continue;
            };
            // Имя списка могло совпасть с другим: экран задаёт идентификатор.
            new_item.list_id = target_list_id;
            new_items.push(new_item);
        }

        let mut created: Vec<Item> = Vec::new();
        let mut duplicates: Vec<DuplicateChoice> = Vec::new();
        for new_item in new_items {
            // Созданная позиция сразу становится существующей: повтор в том же
            // сообщении предлагается как дубликат, а не сохраняется молча.
            let (to_create, found) = plan_additions(&existing, vec![new_item]);
            for choice in found {
                merge_duplicate_choice(&mut duplicates, choice);
            }
            for new_item in to_create {
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
        }

        let draft = if draft_items.is_empty() {
            None
        } else {
            let clarifications = reindex_clarifications(parsed_len, &draft_items, clarifications);
            let draft_items = draft_items
                .into_iter()
                .map(|(_, item)| item)
                .collect::<Vec<_>>();
            let draft = draft::append_or_create(
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
        let on_list = matches!(
            &active,
            Some(Screen::List { list_id, .. }) if *list_id == target_list_id
        );
        // Карточка заменяет экран только на собственном статусе задания или
        // карточке этого же черновика; без экрана — тоже карточка. На списке
        // покупок и на чужом экране остаётся текущий экран с баннером.
        let show_card = draft
            .as_ref()
            .is_some_and(|draft| active.is_none() || on_own_screen(&active, job.id, draft.id));

        let result = match (&draft, show_card) {
            (_, true) => DirectResult::Card,
            (Some(_), false) => DirectResult::DraftBanner,
            (None, false) => DirectResult::PlainBanner,
        };
        let open_draft = draft
            .as_ref()
            .filter(|_| result == DirectResult::DraftBanner)
            .map(|draft| draft.id);
        let created_ids: Vec<uuid::Uuid> = created.iter().map(|item| item.id).collect();
        let rows = addition_rows(&duplicates, target_list_id, &created_ids, open_draft);
        let text = result_banner(&target_name, &added, &duplicates, on_list, result);
        let banner = text.map(|text| Banner::with_button_rows(text, rows));

        let screen = match &draft {
            Some(draft) if show_card => Screen::Draft { draft_id: draft.id },
            _ => active.unwrap_or(Screen::Menu),
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
        let active = active_screen(&mut *tx, job.chat_id).await?;

        let parts = split_titles(text);
        if parts.len() > MAX_ITEMS {
            self.bot
                .present_to(
                    &mut *tx,
                    now,
                    prepared.recipient(job.chat_id),
                    active.unwrap_or(Screen::Menu),
                    Some(Banner::text(format!(
                        "Слишком много позиций: не больше {MAX_ITEMS} за раз. \
                         Разделите текст"
                    ))),
                )
                .await?;
            return Ok(true);
        }

        let added = add_parts(
            &mut *tx,
            job.household_id,
            &actor,
            target_list_id,
            context.timezone,
            context.today(),
            now,
            parts,
        )
        .await
        .map_err(into_sqlx)?;

        let titles: Vec<String> = added
            .created
            .iter()
            .map(|item| item.title.clone())
            .collect();
        let created_ids: Vec<uuid::Uuid> = added.created.iter().map(|item| item.id).collect();
        let rows = addition_rows(&added.duplicates, target_list_id, &created_ids, None);
        let on_list = matches!(
            &active,
            Some(Screen::List { list_id, .. }) if *list_id == target_list_id
        );
        let text = split_banner(
            &target_name,
            &titles,
            &added.duplicates,
            &added.skipped,
            on_list,
        );
        let banner = Banner::with_button_rows(text, rows);
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
        for run in issue.runs {
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
                // Чужой активный экран не вытесняется: ошибка видна баннером
                // на нём, а исходный текст остаётся в кнопке ручного ввода.
                // Исключение — статус этого же разбора: его место занимает
                // редактор с исходным текстом.
                let active = active_screen(&mut tx, job.chat_id).await?;
                let on_own_processing = matches!(
                    &active,
                    Some(Screen::Processing { job_id: Some(job_id) }) if *job_id == job.id
                );
                if let Some(screen) = active.filter(|_| !on_own_processing) {
                    let banner = Banner::with_buttons(
                        issue.message,
                        vec![button(
                            "✍️ Ввести вручную",
                            Action::ManualEntry {
                                text: issue.text.to_owned(),
                            },
                        )],
                    );
                    self.bot
                        .present_to(
                            &mut tx,
                            now,
                            prepared.recipient(job.chat_id),
                            screen,
                            Some(banner),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::schema::ClarificationTopic;

    fn raw(title: &str) -> RawItem {
        RawItem {
            target_kind: ListKind::Tasks,
            target_list: None,
            title: title.to_owned(),
            notes: None,
            due_date: None,
            due_time: None,
            all_day: false,
            end_date: None,
            end_time: None,
            date_error: Vec::new(),
            priority: None,
            assignee: None,
            assignee_id: None,
            tags: Vec::new(),
            quantity: None,
            unit: None,
            subtasks: Vec::new(),
            recurrence: None,
        }
    }

    #[test]
    fn clarifications_follow_items_that_stay_in_the_draft() {
        let draft_items = vec![(1, raw("Второе")), (2, raw("Третье"))];
        let clarifications = vec![
            Clarification {
                item: 0,
                topic: ClarificationTopic::Date,
            },
            Clarification {
                item: 1,
                topic: ClarificationTopic::Time,
            },
            Clarification {
                item: 2,
                topic: ClarificationTopic::List,
            },
        ];

        let reindexed = reindex_clarifications(3, &draft_items, clarifications);

        assert_eq!(
            reindexed,
            [
                Clarification {
                    item: 0,
                    topic: ClarificationTopic::Time,
                },
                Clarification {
                    item: 1,
                    topic: ClarificationTopic::List,
                },
            ],
            "the item created directly loses its question, the rest are renumbered"
        );
    }
}
