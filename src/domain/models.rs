//! Доменные типы Tasklerq.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

/// Роль участника household.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "user_role", rename_all = "lowercase")]
pub enum UserRole {
    /// Владелец: управляет household, участниками и списками.
    Owner,
    /// Участник: работает с контентом.
    Member,
}

/// Тип списка.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "list_kind", rename_all = "lowercase")]
pub enum ListKind {
    /// Задачи.
    Tasks,
    /// Покупки.
    Shopping,
    /// Чек-лист.
    Checklist,
}

/// Статус элемента или подзадачи.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "item_status", rename_all = "lowercase")]
pub enum ItemStatus {
    /// Открыт.
    Open,
    /// Выполнен.
    Done,
}

/// Приоритет задачи.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "item_priority", rename_all = "lowercase")]
pub enum ItemPriority {
    /// Низкий.
    Low,
    /// Обычный.
    #[default]
    Normal,
    /// Высокий.
    High,
    /// Срочный.
    Urgent,
}

/// Частота повторения.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "recurrence_freq", rename_all = "lowercase")]
pub enum RecurrenceFreq {
    /// Каждый день.
    Daily,
    /// Каждую неделю.
    Weekly,
    /// Каждый месяц.
    Monthly,
    /// Каждый год.
    Yearly,
}

/// Правило для месячного повторения.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "month_rule", rename_all = "snake_case")]
pub enum MonthRule {
    /// День из якорной даты; месяцы без такого дня пропускаются.
    AnchorDay,
    /// Последний день месяца.
    LastDay,
}

/// Тип актора действия.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "actor_type", rename_all = "snake_case")]
pub enum ActorType {
    /// Пользователь Telegram.
    User,
    /// Система.
    System,
    /// Home Assistant.
    HomeAssistant,
    /// Голосовой ассистент.
    Voice,
}

/// Тип события журнала.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "event_type", rename_all = "snake_case")]
pub enum EventType {
    /// Элемент создан.
    Created,
    /// Элемент изменён.
    Updated,
    /// Элемент завершён.
    Completed,
    /// Элемент открыт повторно.
    Reopened,
    /// Элемент мягко удалён.
    Deleted,
    /// Элемент восстановлен.
    Restored,
    /// Назначен исполнитель.
    Assigned,
    /// Снят исполнитель.
    Unassigned,
    /// Изменено правило повторения.
    RecurrenceChanged,
    /// Подзадача завершена.
    SubtaskCompleted,
    /// Подзадача открыта повторно.
    SubtaskReopened,
}

/// Источник создания элемента.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "item_source", rename_all = "snake_case")]
pub enum ItemSource {
    /// Telegram-бот.
    Telegram,
    /// Сущность `todo` Home Assistant.
    HaTodo,
    /// Голосовой capture Home Assistant.
    HaVoice,
    /// Автоматизация Home Assistant.
    HaAutomation,
    /// HTTP API.
    Api,
    /// Импорт.
    Import,
}

/// Источник внешнего capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "capture_source", rename_all = "snake_case")]
pub enum CaptureSource {
    /// Telegram.
    Telegram,
    /// Голосовой Assist.
    HaVoice,
    /// Автоматизация Home Assistant.
    HaAutomation,
    /// HTTP API.
    Api,
    /// Ярлык на телефоне.
    Shortcut,
}

/// Режим capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "capture_mode", rename_all = "lowercase")]
pub enum CaptureMode {
    /// Применить сразу.
    Auto,
    /// Сохранить черновик.
    Draft,
}

/// Статус черновика capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "draft_status", rename_all = "lowercase")]
pub enum DraftStatus {
    /// Ожидает подтверждения.
    Pending,
    /// Применён.
    Applied,
    /// Отменён.
    Cancelled,
    /// Истёк.
    Expired,
    /// Отклонён.
    Rejected,
}

/// Household — семья, владеющая списками.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Household {
    /// Идентификатор.
    pub id: Uuid,
    /// Название.
    pub name: String,
    /// Часовой пояс IANA.
    pub timezone: String,
    /// Время создания.
    pub created_at: DateTime<Utc>,
    /// Время изменения.
    pub updated_at: DateTime<Utc>,
    /// Время мягкого удаления.
    pub deleted_at: Option<DateTime<Utc>>,
}

/// Участник household.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct User {
    /// Идентификатор.
    pub id: Uuid,
    /// Household.
    pub household_id: Uuid,
    /// Telegram user id.
    pub telegram_user_id: i64,
    /// Telegram chat id.
    pub telegram_chat_id: i64,
    /// Отображаемое имя.
    pub display_name: String,
    /// Telegram username.
    pub username: Option<String>,
    /// Роль.
    pub role: UserRole,
    /// Личный часовой пояс.
    pub timezone: Option<String>,
    /// Настройки уведомлений.
    pub settings: serde_json::Value,
    /// Время создания.
    pub created_at: DateTime<Utc>,
    /// Время изменения.
    pub updated_at: DateTime<Utc>,
    /// Время мягкого удаления.
    pub deleted_at: Option<DateTime<Utc>>,
}

/// Список задач, покупок или чек-лист.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct List {
    /// Идентификатор.
    pub id: Uuid,
    /// Household.
    pub household_id: Uuid,
    /// Тип.
    pub kind: ListKind,
    /// Название.
    pub name: String,
    /// Список по умолчанию для своего kind.
    pub is_default: bool,
    /// Архивирован.
    pub is_archived: bool,
    /// Позиция в меню.
    pub position: i32,
    /// Время создания.
    pub created_at: DateTime<Utc>,
    /// Время изменения.
    pub updated_at: DateTime<Utc>,
    /// Время мягкого удаления.
    pub deleted_at: Option<DateTime<Utc>>,
}

/// Подзадача.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Subtask {
    /// Идентификатор.
    pub id: Uuid,
    /// Родительский элемент.
    pub item_id: Uuid,
    /// Название.
    pub title: String,
    /// Статус.
    pub status: ItemStatus,
    /// Позиция.
    pub position: i32,
    /// Время создания.
    pub created_at: DateTime<Utc>,
    /// Время завершения.
    pub completed_at: Option<DateTime<Utc>>,
}

/// Тег household.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Tag {
    /// Идентификатор.
    pub id: Uuid,
    /// Household.
    pub household_id: Uuid,
    /// Название в нижнем регистре.
    pub name: String,
    /// Время создания.
    pub created_at: DateTime<Utc>,
}

/// Событие журнала элемента.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ItemEvent {
    /// Идентификатор.
    pub id: Uuid,
    /// Household.
    pub household_id: Uuid,
    /// Элемент.
    pub item_id: Uuid,
    /// Пользователь-актор; NULL для системных действий.
    pub actor_user_id: Option<Uuid>,
    /// Тип актора.
    pub actor_type: ActorType,
    /// Тип события.
    pub event_type: EventType,
    /// Данные до/после.
    pub payload: serde_json::Value,
    /// Ссылка на отменяемое событие.
    pub undo_of: Option<Uuid>,
    /// Время события.
    pub created_at: DateTime<Utc>,
}

/// Правило повторения, сохранённое на элементе.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recurrence {
    /// Частота.
    pub freq: RecurrenceFreq,
    /// Интервал 1..365.
    pub interval: i16,
    /// Дни недели ISO 1..7 (только weekly).
    pub byweekday: Vec<i16>,
    /// Дата окончания серии.
    pub until: Option<NaiveDate>,
    /// Часовой пояс IANA.
    pub timezone: String,
    /// Якорная дата серии.
    pub anchor_date: NaiveDate,
    /// Правило месячного повторения.
    pub month_rule: Option<MonthRule>,
}

/// Входные данные правила повторения до валидации.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecurrenceInput {
    /// Частота.
    pub freq: RecurrenceFreq,
    /// Интервал; по умолчанию 1.
    pub interval: Option<i16>,
    /// Дни недели ISO.
    #[serde(default)]
    pub byweekday: Vec<i16>,
    /// Дата окончания.
    pub until: Option<NaiveDate>,
    /// Часовой пояс; по умолчанию timezone household.
    pub timezone: Option<String>,
    /// Якорная дата; по умолчанию срок или сегодня.
    pub anchor_date: Option<NaiveDate>,
    /// Правило месяца.
    pub month_rule: Option<MonthRule>,
}

/// Задача или позиция списка.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    /// Идентификатор.
    pub id: Uuid,
    /// Household.
    pub household_id: Uuid,
    /// Список.
    pub list_id: Uuid,
    /// Название.
    pub title: String,
    /// Описание.
    pub notes: Option<String>,
    /// Статус.
    pub status: ItemStatus,
    /// Приоритет.
    pub priority: ItemPriority,
    /// Срок со временем.
    pub due_at: Option<DateTime<Utc>>,
    /// Вседневный срок.
    pub due_date: Option<NaiveDate>,
    /// Исполнитель.
    pub assignee_user_id: Option<Uuid>,
    /// Автор-пользователь.
    pub created_by_user_id: Option<Uuid>,
    /// Источник.
    pub source: ItemSource,
    /// Внешний capture.
    pub capture_id: Option<Uuid>,
    /// Время завершения.
    pub completed_at: Option<DateTime<Utc>>,
    /// Кто завершил.
    pub completed_by_user_id: Option<Uuid>,
    /// Количество.
    pub quantity: Option<f64>,
    /// Единица измерения.
    pub unit: Option<String>,
    /// Позиция в списке.
    pub position: i32,
    /// Версия для оптимистической блокировки.
    pub version: i32,
    /// Правило повторения.
    pub recurrence: Option<Recurrence>,
    /// Время создания.
    pub created_at: DateTime<Utc>,
    /// Время изменения.
    pub updated_at: DateTime<Utc>,
    /// Время мягкого удаления.
    pub deleted_at: Option<DateTime<Utc>>,
    /// Теги.
    pub tags: Vec<Tag>,
    /// Подзадачи.
    pub subtasks: Vec<Subtask>,
}

impl Item {
    /// Прогресс подзадач: (выполнено, всего).
    pub fn subtask_progress(&self) -> (usize, usize) {
        let done = self
            .subtasks
            .iter()
            .filter(|subtask| subtask.status == ItemStatus::Done)
            .count();
        (done, self.subtasks.len())
    }
}

/// Данные для создания элемента.
#[derive(Debug, Clone)]
pub struct NewItem {
    /// Список.
    pub list_id: Uuid,
    /// Название.
    pub title: String,
    /// Описание.
    pub notes: Option<String>,
    /// Приоритет.
    pub priority: ItemPriority,
    /// Срок со временем.
    pub due_at: Option<DateTime<Utc>>,
    /// Вседневный срок.
    pub due_date: Option<NaiveDate>,
    /// Исполнитель.
    pub assignee_user_id: Option<Uuid>,
    /// Источник.
    pub source: ItemSource,
    /// Количество.
    pub quantity: Option<f64>,
    /// Единица.
    pub unit: Option<String>,
    /// Теги.
    pub tags: Vec<String>,
    /// Подзадачи.
    pub subtasks: Vec<SubtaskInput>,
    /// Повторение.
    pub recurrence: Option<RecurrenceInput>,
}

/// Подзадача при создании/замене.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtaskInput {
    /// Название.
    pub title: String,
    /// Статус; по умолчанию open.
    #[serde(default)]
    pub status: Option<ItemStatus>,
}

/// Поле PATCH-запроса: отсутствует, null или значение.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Field<T> {
    /// Поле не передано — не менять.
    #[default]
    Missing,
    /// Передан null — очистить.
    Null,
    /// Передано значение.
    Value(T),
}

impl<T> Field<T> {
    /// Поле отсутствует.
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }

    /// Поле задано (null или значение).
    pub fn is_set(&self) -> bool {
        !self.is_missing()
    }
}

impl<'de, T> Deserialize<'de> for Field<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Option::<T>::deserialize(deserializer)?;
        Ok(match value {
            Some(value) => Self::Value(value),
            None => Self::Null,
        })
    }
}

/// Изменения элемента.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ItemPatch {
    /// Название.
    pub title: Option<String>,
    /// Описание.
    #[serde(default)]
    pub notes: Field<String>,
    /// Приоритет.
    pub priority: Option<ItemPriority>,
    /// Срок со временем.
    #[serde(default)]
    pub due_at: Field<DateTime<Utc>>,
    /// Вседневный срок.
    #[serde(default)]
    pub due_date: Field<NaiveDate>,
    /// Исполнитель.
    #[serde(default)]
    pub assignee_user_id: Field<Uuid>,
    /// Количество.
    #[serde(default)]
    pub quantity: Field<f64>,
    /// Единица.
    #[serde(default)]
    pub unit: Field<String>,
    /// Полный набор тегов.
    pub tags: Option<Vec<String>>,
    /// Полный набор подзадач.
    pub subtasks: Option<Vec<SubtaskInput>>,
    /// Правило повторения.
    #[serde(default)]
    pub recurrence: Field<RecurrenceInput>,
    /// Перенос в другой список.
    pub list_id: Option<Uuid>,
}

/// Актор доменного действия.
#[derive(Debug, Clone)]
pub struct Actor {
    /// Пользователь, если действие от участника.
    pub user_id: Option<Uuid>,
    /// Роль пользователя.
    pub role: Option<UserRole>,
    /// Тип актора.
    pub actor_type: ActorType,
}

impl Actor {
    /// Системный актор без пользователя.
    pub fn system(actor_type: ActorType) -> Self {
        Self {
            user_id: None,
            role: None,
            actor_type,
        }
    }

    /// Действие участника household.
    pub fn user(user_id: Uuid, role: UserRole) -> Self {
        Self {
            user_id: Some(user_id),
            role: Some(role),
            actor_type: ActorType::User,
        }
    }

    /// Является ли актор владельцем household.
    pub fn is_owner(&self) -> bool {
        self.role == Some(UserRole::Owner)
    }
}
