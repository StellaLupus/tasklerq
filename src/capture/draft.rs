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

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Connection, PgConnection};
use thiserror::Error;
use uuid::Uuid;

use super::context::CaptureContext;
use super::schema::{ParsedResponse, RawItem, RawRecurrence};
use super::validate::{ValidatedDraft, validate_items};
use crate::domain::error::DomainError;
use crate::domain::models::{
    Actor, CaptureMode, CaptureSource, DraftStatus, Field, Item, ItemSource, ItemStatus, ListKind,
};
use crate::domain::validation::{sanitize, validate_new_item};
use crate::storage::repositories::drafts::{self, Draft, NewDraft};
use crate::storage::repositories::{households, items, lists, users};

/// Источник элементов, созданных из capture с данным источником.
pub fn item_source(source: CaptureSource) -> ItemSource {
    match source {
        CaptureSource::Telegram => ItemSource::Telegram,
        CaptureSource::HaVoice => ItemSource::HaVoice,
        CaptureSource::HaAutomation => ItemSource::HaAutomation,
        CaptureSource::Api | CaptureSource::Shortcut => ItemSource::Api,
    }
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

/// Содержимое `capture_drafts.parsed`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Stored {
    items: Vec<RawItem>,
    clarifications: Vec<String>,
}

impl Stored {
    fn from_parsed(parsed: ParsedResponse) -> Self {
        let mut stored = Self {
            items: parsed.items,
            clarifications: parsed.clarifications,
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

/// Данные нового черновика.
#[derive(Debug, Clone)]
pub struct NewCapture {
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
}

impl DraftView {
    /// Черновик ждёт подтверждения и не истёк.
    pub fn is_open(&self, now: DateTime<Utc>) -> bool {
        self.draft.status == DraftStatus::Pending && !self.draft.is_expired(now)
    }
}

/// Результат подтверждения.
#[derive(Debug, Clone)]
pub struct Confirmed {
    /// Созданные элементы в порядке черновика.
    pub items: Vec<Item>,
    /// Черновик уже был подтверждён раньше: элементы не создавались заново.
    pub replayed: bool,
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
pub async fn load_context(
    conn: &mut PgConnection,
    household_id: Uuid,
    now: DateTime<Utc>,
) -> Result<CaptureContext, DomainError> {
    let household = households::get(conn, household_id).await?;
    let all_lists = lists::list(conn, household_id).await?;
    let members = users::list_active(conn, household_id).await?;
    CaptureContext::new(&household.timezone, now, &all_lists, &members)
}

/// Сохранить черновик со сроком жизни 15 минут.
pub async fn create(
    conn: &mut PgConnection,
    capture: NewCapture,
    now: DateTime<Utc>,
) -> Result<Draft, DomainError> {
    let stored = Stored::from_parsed(capture.parsed);
    drafts::insert(
        conn,
        NewDraft {
            id: Uuid::now_v7(),
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
        },
        now,
    )
    .await
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
    let context = load_context(conn, draft.household_id, now).await?;
    let stored = Stored::read(&draft)?;
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
    drafts::set_parsed(&mut tx, household_id, draft_id, stored.to_value()).await?;
    let draft = Draft {
        parsed: Some(stored.to_value()),
        ..draft
    };
    let view = build_view(&mut tx, draft, now).await?;
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
    drafts::set_parsed(&mut tx, household_id, draft_id, stored.to_value()).await?;
    let draft = Draft {
        parsed: Some(stored.to_value()),
        ..draft
    };
    let view = build_view(&mut tx, draft, now).await?;
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
    drafts::set_parsed(&mut tx, household_id, draft_id, stored.to_value()).await?;
    let draft = Draft {
        parsed: Some(stored.to_value()),
        ..draft
    };
    let view = build_view(&mut tx, draft, now).await?;
    tx.commit().await?;
    Ok(view)
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

    let today = view.context.today();
    let timezone = view.context.timezone.name();
    let mut created = Vec::with_capacity(view.validated.items.len());
    for draft_item in view.validated.items {
        let Some(new_item) = draft_item.item else {
            return Err(DraftError::Invalid(Vec::new()));
        };
        let list_kind = view
            .context
            .lists
            .iter()
            .find(|list| list.id == new_item.list_id)
            .map(|list| list.kind)
            .ok_or(DomainError::NotFound)?;
        let list_id = new_item.list_id;
        let assignee = new_item.assignee_user_id;
        let validated = validate_new_item(list_kind, timezone, new_item, today)?;
        let item = items::create(
            &mut tx,
            household_id,
            actor,
            list_id,
            validated,
            assignee,
            now,
        )
        .await?;
        created.push(item);
    }

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
    match edit.quantity {
        Field::Missing => {}
        Field::Null => raw.quantity = None,
        Field::Value(value) => raw.quantity = Some(value),
    }
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
