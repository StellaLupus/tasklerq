//! Черновики capture: создание, правка, подтверждение и отмена.
//!
//! В `capture_drafts.parsed` лежат элементы «как есть» после очистки от
//! управляющих символов, в том числе невалидные: пользователь должен видеть
//! ошибку и исправить её. Создать из них что-либо можно только через
//! [`validate_items`]: каждое чтение и подтверждение заново проверяет элементы
//! по актуальному контексту household, поэтому «дата уже прошла» срабатывает и
//! для черновика, пролежавшего до утра.
//!
//! Все операции сами открывают транзакцию (вложенную, если вызывающая сторона
//! уже в транзакции) и берут блокировку строки черновика, так что параллельные
//! подтверждения выстраиваются в очередь и второе видит результат первого.

use chrono::{DateTime, Days, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Connection, PgConnection};
use thiserror::Error;
use uuid::Uuid;

use super::context::CaptureContext;
use super::schema::{MAX_CLARIFICATIONS, ParsedResponse, RawItem, RawRecurrence};
use super::validate::{DraftItem, ValidatedDraft, validate_items};
use crate::domain::error::DomainError;
use crate::domain::models::{
    Actor, CaptureMode, CaptureSource, DraftStatus, Field, Item, ItemSource, ItemStatus, ListKind,
};
use crate::domain::validation::{sanitize, validate_new_item};
use crate::storage::repositories::drafts::{self, Draft, NewDraft};
use crate::storage::repositories::{capture_requests, households, items, lists, users};

/// Источник элементов, созданных из capture с данным источником.
pub fn item_source(source: CaptureSource) -> ItemSource {
    match source {
        CaptureSource::Telegram => ItemSource::Telegram,
        CaptureSource::HaVoice => ItemSource::HaVoice,
        CaptureSource::HaAutomation => ItemSource::HaAutomation,
        CaptureSource::Api | CaptureSource::Shortcut => ItemSource::Api,
    }
}

/// Ответ на вопрос о дате. Дата вычисляется в момент нажатия, а не при
/// отрисовке карточки: иначе карточка, пережившая полночь, записала бы вчерашний
/// день, и вопрос о прошедшей дате возник бы снова.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DueChoice {
    /// Сегодня по часовому поясу household.
    Today,
    /// Завтра по часовому поясу household.
    Tomorrow,
}

/// Причина, по которой операция над черновиком невозможна.
#[derive(Debug, Error)]
pub enum DraftError {
    /// Срок жизни черновика вышел.
    #[error("draft expired")]
    Expired,
    /// Черновик отменён.
    #[error("draft cancelled")]
    Cancelled,
    /// Черновик уже подтверждён.
    #[error("draft already applied")]
    Applied,
    /// Черновик ещё не подтверждён: отменять нечего.
    #[error("draft is not applied")]
    NotApplied,
    /// Окно отмены подтверждённого capture закрыто.
    #[error("undo window is closed")]
    UndoExpired,
    /// Элементы capture изменены после создания.
    #[error("capture items were changed")]
    Changed,
    /// В черновике нет элемента с таким номером.
    #[error("draft has no item {0}")]
    NoSuchItem(usize),
    /// Лимит уточняющих вопросов исчерпан: дальше только ручная правка.
    #[error("clarification limit is reached")]
    ClarificationLimit,
    /// Выбранный вариант не относится к текущей неоднозначности.
    #[error("clarification is not applicable")]
    NoClarification,
    /// Сохранить нельзя: черновик пуст или содержит невалидные элементы.
    /// Номера невалидных элементов; пустой список означает пустой черновик.
    #[error("draft cannot be saved")]
    Invalid(Vec<usize>),
    /// Ошибка домена или базы данных.
    #[error(transparent)]
    Domain(#[from] DomainError),
    /// Ошибка базы данных.
    #[error("database error")]
    Database(#[from] sqlx::Error),
}

/// Ошибка сохранения черновика вместе с ответом ключа идемпотентности.
///
/// Появляется только на пути с `responder`: ошибка сериализации ответа и
/// потеря резерва невозможны в [`create`], поэтому Telegram-код их не видит.
#[derive(Debug, Error)]
pub enum StoreError {
    /// Ошибка домена, базы данных или состояния черновика.
    #[error(transparent)]
    Draft(#[from] DraftError),
    /// Ответ capture не удалось сериализовать.
    #[error("capture response could not be serialized")]
    Serialize(#[from] serde_json::Error),
    /// Резерв ключа перехвачен другим запросом.
    #[error("the idempotency reservation was taken over by another request")]
    ReservationLost,
}

impl From<DomainError> for StoreError {
    fn from(error: DomainError) -> Self {
        Self::Draft(error.into())
    }
}

impl From<sqlx::Error> for StoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Draft(DraftError::Database(error))
    }
}

impl From<capture_requests::CompleteError> for StoreError {
    fn from(error: capture_requests::CompleteError) -> Self {
        match error {
            capture_requests::CompleteError::Lost => Self::ReservationLost,
            capture_requests::CompleteError::Database(error) => {
                Self::Draft(DraftError::Database(error))
            }
        }
    }
}

/// Содержимое `capture_drafts.parsed`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Stored {
    items: Vec<RawItem>,
    clarifications: Vec<String>,
    /// Сколько уточняющих вопросов пользователь уже закрыл. Счётчик живёт в
    /// `parsed`, поэтому переживает перерисовку карточки и не требует миграции.
    #[serde(default)]
    clarifications_answered: u8,
}

impl Stored {
    fn from_parsed(parsed: ParsedResponse) -> Self {
        let mut stored = Self {
            items: parsed.items,
            clarifications: parsed.clarifications,
            clarifications_answered: 0,
        };
        stored.items.iter_mut().for_each(scrub);
        stored
    }

    fn read(draft: &Draft) -> Result<Self, DomainError> {
        let value = draft
            .parsed
            .clone()
            .ok_or_else(|| DomainError::Validation("draft has no content".into()))?;
        serde_json::from_value(value)
            .map_err(|_| DomainError::Validation("draft content is corrupted".into()))
    }

    fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_default()
    }
}

/// Построитель сериализованного ответа, который сохраняется в резерве ключа
/// идемпотентности в той же транзакции, что и строки capture. Ошибка
/// сериализации не превращается в `null`: вызывающая сторона получает её как
/// ошибку запроса.
pub type Responder<'a, T> = Box<dyn FnOnce(&T) -> Result<Value, serde_json::Error> + Send + 'a>;

/// Данные нового черновика.
#[derive(Debug, Clone)]
pub struct NewCapture {
    /// Идентификатор, зарезервированный вызывающей стороной (ключ
    /// идемпотентности); иначе генерируется новый.
    pub id: Option<Uuid>,
    /// Household.
    pub household_id: Uuid,
    /// Автор; у голосового capture его нет.
    pub user_id: Option<Uuid>,
    /// Чат Telegram.
    pub chat_id: Option<i64>,
    /// Источник.
    pub source: CaptureSource,
    /// Режим.
    pub mode: CaptureMode,
    /// Ключ идемпотентности.
    pub idempotency_key: Option<String>,
    /// Хеш тела запроса.
    pub request_hash: Option<String>,
    /// Исходный текст.
    pub raw_text: String,
    /// Разобранный ответ LLM.
    pub parsed: ParsedResponse,
    /// Черновик собран без LLM.
    pub fallback: bool,
    /// Версия промпта.
    pub prompt_version: Option<String>,
}

/// Черновик вместе с проверкой его элементов.
#[derive(Debug, Clone)]
pub struct DraftView {
    /// Строка черновика.
    pub draft: Draft,
    /// Элементы, проверенные по актуальному контексту.
    pub validated: ValidatedDraft,
    /// Те же элементы без проверки, в том же порядке: по ним форма правки
    /// заполняется и для невалидного элемента.
    pub raw: Vec<RawItem>,
    /// Контекст, по которому шла проверка.
    pub context: CaptureContext,
    /// Сколько уточняющих вопросов уже закрыто.
    pub clarifications_answered: u8,
}

impl DraftView {
    /// Черновик ждёт подтверждения и не истёк.
    pub fn is_open(&self, now: DateTime<Utc>) -> bool {
        self.draft.status == DraftStatus::Pending && !self.draft.is_expired(now)
    }

    /// Сколько уточняющих вопросов ещё можно задать.
    pub fn clarifications_left(&self) -> usize {
        clarifications_left(self.clarifications_answered)
    }
}

fn clarifications_left(answered: u8) -> usize {
    MAX_CLARIFICATIONS.saturating_sub(usize::from(answered))
}

/// Результат подтверждения.
#[derive(Debug, Clone)]
pub struct Confirmed {
    /// Созданные элементы в порядке черновика.
    pub items: Vec<Item>,
    /// Черновик уже был подтверждён раньше: элементы не создавались заново.
    pub replayed: bool,
}

/// Результат применения внешнего `auto`.
#[derive(Debug, Clone)]
pub struct Applied {
    /// Строка `capture_drafts`; она же `capture_id` ответа.
    pub capture_id: Uuid,
    /// Созданные элементы в порядке разбора.
    pub items: Vec<Item>,
}

/// Правка элемента черновика. Строки — «как у LLM»: проверка идёт тем же
/// кодом, что и для ответа модели.
#[derive(Debug, Clone, Default)]
pub struct ItemEdit {
    /// Тип списка.
    pub target_kind: Option<ListKind>,
    /// Название списка.
    pub target_list: Field<String>,
    /// Название.
    pub title: Option<String>,
    /// Заметка.
    pub notes: Field<String>,
    /// Дата `YYYY-MM-DD`; `Null` снимает и время.
    pub due_date: Field<String>,
    /// Время `HH:MM`.
    pub due_time: Field<String>,
    /// Приоритет.
    pub priority: Field<String>,
    /// Исполнитель.
    pub assignee: Field<String>,
    /// Теги целиком.
    pub tags: Option<Vec<String>>,
    /// Количество.
    pub quantity: Field<f64>,
    /// Единица.
    pub unit: Field<String>,
    /// Подзадачи целиком.
    pub subtasks: Option<Vec<String>>,
    /// Повторение.
    pub recurrence: Field<RawRecurrence>,
}

/// Собрать контекст household для проверки: часовой пояс, списки, участники.
pub async fn load_context_for(
    conn: &mut PgConnection,
    household_id: Uuid,
    author_id: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<CaptureContext, DomainError> {
    let household = households::get(conn, household_id).await?;
    let all_lists = lists::list(conn, household_id).await?;
    let members = users::list_active(conn, household_id).await?;
    Ok(CaptureContext::new(&household.timezone, now, &all_lists, &members)?.with_author(author_id))
}

/// Сохранить черновик со сроком жизни 15 минут.
///
/// Без `responder` возможны только ошибки вставки (`Domain`), поэтому путь
/// возвращает [`DomainError`] и не маскирует недостижимые варианты под ошибку
/// валидации.
pub async fn create(
    conn: &mut PgConnection,
    capture: NewCapture,
    now: DateTime<Utc>,
) -> Result<Draft, DomainError> {
    drafts::insert(conn, new_draft(capture), now).await
}

/// Сохранить черновик и ответ для ключа идемпотентности одной транзакцией.
///
/// `responder` вызывается после вставки черновика, но до коммита, поэтому ответ
/// и `capture_drafts` коммитятся вместе. Вызывающая сторона должна передать
/// соединение внутри транзакции, чтобы обе записи были атомарны, и отметить
/// резерв через [`capture_requests::Reservation::settle`] после коммита.
pub async fn create_with_responder(
    conn: &mut PgConnection,
    capture: NewCapture,
    now: DateTime<Utc>,
    responder: Option<Responder<'_, Draft>>,
    mut reservation: Option<&mut capture_requests::Reservation>,
) -> Result<Draft, StoreError> {
    if let Some(reservation) = reservation.as_deref_mut() {
        reservation.verify(conn).await?;
    }
    let draft = drafts::insert(conn, new_draft(capture), now).await?;
    if let (Some(responder), Some(reservation)) = (responder, reservation) {
        let response = responder(&draft)?;
        reservation.complete(conn, &response, now).await?;
    }
    Ok(draft)
}

/// Строка `capture_drafts` из данных capture: общая часть [`create`],
/// [`create_with_responder`] и [`apply_auto`].
fn new_draft(capture: NewCapture) -> NewDraft {
    let stored = Stored::from_parsed(capture.parsed);
    NewDraft {
        id: capture.id.unwrap_or_else(Uuid::now_v7),
        household_id: capture.household_id,
        user_id: capture.user_id,
        chat_id: capture.chat_id,
        source: capture.source,
        mode: capture.mode,
        idempotency_key: capture.idempotency_key,
        request_hash: capture.request_hash,
        raw_text: sanitize(&capture.raw_text),
        parsed: stored.to_value(),
        fallback: capture.fallback,
        prompt_version: capture.prompt_version,
    }
}

/// Черновик с проверенными элементами; чужой черновик — `NotFound`.
pub async fn view(
    conn: &mut PgConnection,
    household_id: Uuid,
    draft_id: Uuid,
    now: DateTime<Utc>,
) -> Result<DraftView, DomainError> {
    let draft = drafts::get(conn, household_id, draft_id).await?;
    build_view(conn, draft, now).await
}

async fn build_view(
    conn: &mut PgConnection,
    draft: Draft,
    now: DateTime<Utc>,
) -> Result<DraftView, DomainError> {
    let context = load_context_for(conn, draft.household_id, draft.user_id, now).await?;
    let stored = Stored::read(&draft)?;
    let clarifications_answered = stored.clarifications_answered;
    let raw = stored.items.clone();
    let validated = validate_items(
        &context,
        item_source(draft.source),
        ParsedResponse {
            items: stored.items,
            clarifications: stored.clarifications,
        },
    );
    Ok(DraftView {
        draft,
        validated,
        raw,
        context,
        clarifications_answered,
    })
}

/// Исправить элемент черновика. Невалидный результат сохраняется: ошибки
/// покажутся в карточке, а сохранить черновик с ними нельзя.
pub async fn edit_item(
    conn: &mut PgConnection,
    household_id: Uuid,
    draft_id: Uuid,
    index: usize,
    edit: ItemEdit,
    now: DateTime<Utc>,
) -> Result<DraftView, DraftError> {
    let mut tx = conn.begin().await?;
    let draft = open_for_change(&mut tx, household_id, draft_id, now).await?;
    let mut stored = Stored::read(&draft)?;
    let raw = stored
        .items
        .get_mut(index)
        .ok_or(DraftError::NoSuchItem(index))?;
    apply(raw, edit);
    let view = save_and_view(&mut tx, draft, stored, now).await?;
    tx.commit().await?;
    Ok(view)
}

/// Удалить элемент из черновика.
pub async fn remove_item(
    conn: &mut PgConnection,
    household_id: Uuid,
    draft_id: Uuid,
    index: usize,
    now: DateTime<Utc>,
) -> Result<DraftView, DraftError> {
    let mut tx = conn.begin().await?;
    let draft = open_for_change(&mut tx, household_id, draft_id, now).await?;
    let mut stored = Stored::read(&draft)?;
    if index >= stored.items.len() {
        return Err(DraftError::NoSuchItem(index));
    }
    stored.items.remove(index);
    let view = save_and_view(&mut tx, draft, stored, now).await?;
    tx.commit().await?;
    Ok(view)
}

/// Заменить элемент черновика целиком (результат формы правки).
pub async fn replace_item(
    conn: &mut PgConnection,
    household_id: Uuid,
    draft_id: Uuid,
    index: usize,
    mut item: RawItem,
    now: DateTime<Utc>,
) -> Result<DraftView, DraftError> {
    let mut tx = conn.begin().await?;
    let draft = open_for_change(&mut tx, household_id, draft_id, now).await?;
    let mut stored = Stored::read(&draft)?;
    let slot = stored
        .items
        .get_mut(index)
        .ok_or(DraftError::NoSuchItem(index))?;
    scrub(&mut item);
    *slot = item;
    let view = save_and_view(&mut tx, draft, stored, now).await?;
    tx.commit().await?;
    Ok(view)
}

/// Ответ на вопрос «какой исполнитель?»: выбрать одного из кандидатов.
///
/// Ответ обновляет только черновик: LLM повторно не вызывается. Выбор
/// принимается лишь для участника, который сейчас действительно входит в
/// неоднозначный набор, и расходует одну из трёх попыток уточнения.
pub async fn answer_assignee(
    conn: &mut PgConnection,
    household_id: Uuid,
    draft_id: Uuid,
    index: usize,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> Result<DraftView, DraftError> {
    answer(
        conn,
        household_id,
        draft_id,
        index,
        now,
        |item| item.has_assignee_question() && item.assignee_candidates.contains(&user_id),
        |_, raw| raw.assignee_id = Some(user_id),
    )
    .await
}

/// Ответ на вопрос о дате: выбрать «сегодня» или «завтра». Точную дату
/// пользователь задаёт в обычной форме правки.
///
/// Время не сбрасывается: ответ меняет только дату. Если время уже прошло,
/// карточка покажет отдельную ошибку `time_past`, а не молча сделает задачу
/// вседневной.
pub async fn answer_due_date(
    conn: &mut PgConnection,
    household_id: Uuid,
    draft_id: Uuid,
    index: usize,
    choice: DueChoice,
    now: DateTime<Utc>,
) -> Result<DraftView, DraftError> {
    answer(
        conn,
        household_id,
        draft_id,
        index,
        now,
        DraftItem::has_date_question,
        |context, raw| {
            let today = context.today();
            let date = match choice {
                DueChoice::Today => today,
                DueChoice::Tomorrow => today.checked_add_days(Days::new(1)).unwrap_or(today),
            };
            raw.due_date = Some(date.format("%Y-%m-%d").to_string());
        },
    )
    .await
}

/// Общая часть ответа на уточняющий вопрос: открыть черновик, проверить лимит
/// и актуальность вопроса, применить изменение и перечитать карточку.
async fn answer(
    conn: &mut PgConnection,
    household_id: Uuid,
    draft_id: Uuid,
    index: usize,
    now: DateTime<Utc>,
    is_question: impl FnOnce(&DraftItem) -> bool,
    apply: impl FnOnce(&CaptureContext, &mut RawItem),
) -> Result<DraftView, DraftError> {
    let mut tx = conn.begin().await?;
    let draft = open_for_change(&mut tx, household_id, draft_id, now).await?;
    let mut stored = Stored::read(&draft)?;
    ensure_clarification_left(&stored)?;

    let view = build_view(&mut tx, draft.clone(), now).await?;
    let checked = view
        .validated
        .items
        .get(index)
        .ok_or(DraftError::NoSuchItem(index))?;
    if !is_question(checked) {
        return Err(DraftError::NoClarification);
    }

    let raw = stored
        .items
        .get_mut(index)
        .ok_or(DraftError::NoSuchItem(index))?;
    apply(&view.context, raw);
    stored.clarifications_answered += 1;
    let view = save_and_view(&mut tx, draft, stored, now).await?;
    tx.commit().await?;
    Ok(view)
}

fn ensure_clarification_left(stored: &Stored) -> Result<(), DraftError> {
    if clarifications_left(stored.clarifications_answered) == 0 {
        Err(DraftError::ClarificationLimit)
    } else {
        Ok(())
    }
}

/// Записать изменённое содержимое и вернуть черновик с ним же, чтобы не
/// сериализовать `parsed` повторно.
async fn persist(
    conn: &mut PgConnection,
    draft: Draft,
    stored: &Stored,
) -> Result<Draft, DraftError> {
    let parsed = stored.to_value();
    drafts::set_parsed(conn, draft.household_id, draft.id, &parsed).await?;
    Ok(Draft {
        parsed: Some(parsed),
        ..draft
    })
}

async fn save_and_view(
    conn: &mut PgConnection,
    draft: Draft,
    stored: Stored,
    now: DateTime<Utc>,
) -> Result<DraftView, DraftError> {
    let draft = persist(conn, draft, &stored).await?;
    Ok(build_view(conn, draft, now).await?)
}

/// Подтвердить черновик: все элементы создаются одной транзакцией.
///
/// Повторное подтверждение возвращает элементы первого и ничего не создаёт.
/// При любой ошибке — включая отказ репозитория на середине — не создаётся ни
/// один элемент, а черновик остаётся ожидающим.
pub async fn confirm(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    draft_id: Uuid,
    now: DateTime<Utc>,
) -> Result<Confirmed, DraftError> {
    let mut tx = conn.begin().await?;
    let draft = drafts::get_for_update(&mut tx, household_id, draft_id).await?;

    match draft.status {
        DraftStatus::Applied => {
            let items = linked_items(&mut tx, household_id, draft_id).await?;
            return Ok(Confirmed {
                items,
                replayed: true,
            });
        }
        DraftStatus::Cancelled | DraftStatus::Rejected => return Err(DraftError::Cancelled),
        DraftStatus::Expired => return Err(DraftError::Expired),
        DraftStatus::Pending if draft.is_expired(now) => return Err(DraftError::Expired),
        DraftStatus::Pending => {}
    }

    let view = build_view(&mut tx, draft, now).await?;
    if !view.validated.is_valid() {
        let invalid = view
            .validated
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| !item.is_valid())
            .map(|(index, _)| index)
            .collect();
        return Err(DraftError::Invalid(invalid));
    }

    let mut created = create_items(
        &mut tx,
        household_id,
        &view.context,
        view.validated.items,
        actor,
        now,
    )
    .await?;

    let ids: Vec<Uuid> = created.iter().map(|item| item.id).collect();
    drafts::link_items(&mut tx, household_id, draft_id, &ids).await?;
    drafts::finish(&mut tx, household_id, draft_id, DraftStatus::Applied, now).await?;
    tx.commit().await?;

    // `capture_id` проставлен после чтения: возвращаем актуальные строки.
    for item in &mut created {
        item.capture_id = Some(draft_id);
    }
    Ok(Confirmed {
        items: created,
        replayed: false,
    })
}

/// Применить внешний `auto` одной транзакцией.
///
/// Строка `capture_drafts` сразу пишется в статусе `applied`, затем в той же
/// транзакции создаются элементы (вместе с их событиями) и привязываются к
/// capture. Любая ошибка откатывает всё: не остаётся ни элементов, ни событий,
/// ни строки capture. Ответ для ключа идемпотентности пишется в той же
/// транзакции через `responder`.
#[allow(clippy::too_many_arguments)]
pub async fn apply_auto(
    conn: &mut PgConnection,
    capture: NewCapture,
    actor: &Actor,
    context: &CaptureContext,
    items: Vec<DraftItem>,
    now: DateTime<Utc>,
    responder: Option<Responder<'_, Applied>>,
    mut reservation: Option<&mut capture_requests::Reservation>,
) -> Result<Applied, StoreError> {
    let household_id = capture.household_id;
    let current = new_draft(capture);

    let mut tx = conn.begin().await?;
    if let Some(reservation) = reservation.as_deref_mut() {
        reservation.verify(&mut tx).await?;
    }
    let draft = drafts::insert_applied(&mut tx, current, now).await?;

    let created = create_items(&mut tx, household_id, context, items, actor, now).await?;
    let ids: Vec<Uuid> = created.iter().map(|item| item.id).collect();
    drafts::link_items(&mut tx, household_id, draft.id, &ids).await?;

    let mut applied = Applied {
        capture_id: draft.id,
        items: created,
    };
    let mut completed = false;
    if let (Some(responder), Some(reservation)) = (responder, reservation.as_deref_mut()) {
        let response = responder(&applied)?;
        reservation.complete(&mut tx, &response, now).await?;
        completed = true;
    }

    tx.commit().await?;

    // Ответ и элементы закоммичены вместе: освобождать резерв нельзя, даже
    // если вызывающая сторона потом обнаружит расхождение в ответе.
    if completed && let Some(reservation) = reservation {
        reservation.settle();
    }

    for item in &mut applied.items {
        item.capture_id = Some(draft.id);
    }
    Ok(applied)
}

/// Создать элементы проверенного черновика в уже открытой транзакции.
///
/// Общая часть [`confirm`] и [`apply_auto`]: доменная проверка повторяется,
/// чтобы репозиторий и валидация не разошлись.
async fn create_items(
    conn: &mut PgConnection,
    household_id: Uuid,
    context: &CaptureContext,
    items: Vec<DraftItem>,
    actor: &Actor,
    now: DateTime<Utc>,
) -> Result<Vec<Item>, DraftError> {
    let today = context.today();
    let timezone = context.timezone.name();
    let mut created = Vec::with_capacity(items.len());
    for draft_item in items {
        let Some(new_item) = draft_item.item else {
            return Err(DraftError::Invalid(Vec::new()));
        };
        let list_kind = context
            .lists
            .iter()
            .find(|list| list.id == new_item.list_id)
            .map(|list| list.kind)
            .ok_or(DomainError::NotFound)?;
        let list_id = new_item.list_id;
        let assignee = new_item.assignee_user_id;
        let validated = validate_new_item(list_kind, timezone, new_item, today)?;
        let item =
            items::create(conn, household_id, actor, list_id, validated, assignee, now).await?;
        created.push(item);
    }
    Ok(created)
}

/// Отменить черновик. Повторная отмена и отмена истёкшего — не ошибка;
/// подтверждённый черновик отменить нельзя (для этого есть undo).
pub async fn cancel(
    conn: &mut PgConnection,
    household_id: Uuid,
    draft_id: Uuid,
    now: DateTime<Utc>,
) -> Result<(), DraftError> {
    let mut tx = conn.begin().await?;
    let draft = drafts::get_for_update(&mut tx, household_id, draft_id).await?;
    match draft.status {
        DraftStatus::Applied => return Err(DraftError::Applied),
        DraftStatus::Pending => {
            drafts::finish(&mut tx, household_id, draft_id, DraftStatus::Cancelled, now).await?;
        }
        DraftStatus::Cancelled | DraftStatus::Expired | DraftStatus::Rejected => {}
    }
    tx.commit().await?;
    Ok(())
}

/// Отменить подтверждённый capture: мягко удалить все его элементы.
///
/// Возможно в течение [`items::UNDO_WINDOW_MINUTES`] после подтверждения и
/// только пока элементы не менялись (версия 1, не завершены). Повторная
/// отмена ничего не делает и возвращает пустой список.
pub async fn undo(
    conn: &mut PgConnection,
    household_id: Uuid,
    actor: &Actor,
    draft_id: Uuid,
    now: DateTime<Utc>,
) -> Result<Vec<Item>, DraftError> {
    let mut tx = conn.begin().await?;
    let draft = drafts::get_for_update(&mut tx, household_id, draft_id).await?;
    let applied_at = match (draft.status, draft.applied_at) {
        (DraftStatus::Applied, Some(applied_at)) => applied_at,
        _ => return Err(DraftError::NotApplied),
    };

    let live = linked_items(&mut tx, household_id, draft_id).await?;
    if live.is_empty() {
        return Ok(Vec::new());
    }
    if now - applied_at > chrono::Duration::minutes(items::UNDO_WINDOW_MINUTES) {
        return Err(DraftError::UndoExpired);
    }
    if live
        .iter()
        .any(|item| item.version != 1 || item.status != ItemStatus::Open)
    {
        return Err(DraftError::Changed);
    }

    let mut removed = Vec::with_capacity(live.len());
    for item in live {
        removed.push(items::soft_delete(&mut tx, household_id, actor, item.id, now).await?);
    }
    tx.commit().await?;
    Ok(removed)
}

/// Заблокировать черновик для правки: он должен ждать подтверждения.
async fn open_for_change(
    conn: &mut PgConnection,
    household_id: Uuid,
    draft_id: Uuid,
    now: DateTime<Utc>,
) -> Result<Draft, DraftError> {
    let draft = drafts::get_for_update(conn, household_id, draft_id).await?;
    match draft.status {
        DraftStatus::Applied => Err(DraftError::Applied),
        DraftStatus::Cancelled | DraftStatus::Rejected => Err(DraftError::Cancelled),
        DraftStatus::Expired => Err(DraftError::Expired),
        DraftStatus::Pending if draft.is_expired(now) => Err(DraftError::Expired),
        DraftStatus::Pending => Ok(draft),
    }
}

/// Элементы подтверждённого capture; удалённые после создания пропускаются.
async fn linked_items(
    conn: &mut PgConnection,
    household_id: Uuid,
    draft_id: Uuid,
) -> Result<Vec<Item>, DraftError> {
    let mut found = Vec::new();
    for id in drafts::linked_item_ids(conn, household_id, draft_id).await? {
        match items::get(conn, household_id, id).await {
            Ok(item) => found.push(item),
            Err(DomainError::NotFound) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(found)
}

fn apply(raw: &mut RawItem, edit: ItemEdit) {
    if let Some(kind) = edit.target_kind {
        raw.target_kind = kind;
    }
    if let Some(title) = edit.title {
        raw.title = title;
    }
    set(&mut raw.target_list, edit.target_list);
    set(&mut raw.notes, edit.notes);
    set(&mut raw.priority, edit.priority);
    set(&mut raw.assignee, edit.assignee);
    set(&mut raw.unit, edit.unit);
    set(&mut raw.recurrence, edit.recurrence);
    // Срок без даты не бывает: сняв дату, снимаем и время.
    if matches!(edit.due_date, Field::Null) {
        raw.due_time = None;
    }
    set(&mut raw.due_date, edit.due_date);
    set(&mut raw.due_time, edit.due_time);
    set(&mut raw.quantity, edit.quantity);
    if let Some(tags) = edit.tags {
        raw.tags = tags;
    }
    if let Some(subtasks) = edit.subtasks {
        raw.subtasks = subtasks;
    }
    scrub(raw);
}

fn set<T>(target: &mut Option<T>, field: Field<T>) {
    match field {
        Field::Missing => {}
        Field::Null => *target = None,
        Field::Value(value) => *target = Some(value),
    }
}

/// Убрать управляющие символы: `\u{0}` не принимает ни `text`, ни `jsonb`.
fn scrub(raw: &mut RawItem) {
    let clean = |value: &mut String| *value = sanitize(value).trim().to_owned();
    let clean_opt = |value: &mut Option<String>| {
        if let Some(value) = value {
            *value = sanitize(value).trim().to_owned();
        }
    };
    clean(&mut raw.title);
    clean_opt(&mut raw.target_list);
    clean_opt(&mut raw.notes);
    clean_opt(&mut raw.due_date);
    clean_opt(&mut raw.due_time);
    clean_opt(&mut raw.priority);
    clean_opt(&mut raw.assignee);
    clean_opt(&mut raw.unit);
    raw.tags.iter_mut().for_each(clean);
    raw.subtasks.iter_mut().for_each(clean);
    if let Some(recurrence) = &mut raw.recurrence {
        clean_opt(&mut recurrence.freq);
        clean_opt(&mut recurrence.month_rule);
        clean_opt(&mut recurrence.until);
    }
}
