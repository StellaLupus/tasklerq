//! DTO REST API.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::calendar::CalendarEvent;
use crate::domain::models::{
    Household, Item, ItemPatch, ItemPriority, ItemSource, List, ListKind, RecurrenceInput,
    SubtaskInput, User,
};

/// Прогресс подзадач.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Progress {
    /// Выполнено.
    pub done: usize,
    /// Всего.
    pub total: usize,
}

/// Элемент в ответе API.
#[derive(Debug, Clone, Serialize)]
pub struct ItemResponse {
    /// Поля элемента.
    #[serde(flatten)]
    pub item: Item,
    /// Прогресс подзадач.
    pub subtask_progress: Progress,
}

impl From<Item> for ItemResponse {
    fn from(item: Item) -> Self {
        let (done, total) = item.subtask_progress();
        Self {
            item,
            subtask_progress: Progress { done, total },
        }
    }
}

/// Household и участники.
#[derive(Debug, Serialize)]
pub struct HouseholdResponse {
    /// Household.
    pub household: Household,
    /// Участники.
    pub members: Vec<User>,
}

/// Создание списка.
#[derive(Debug, Deserialize)]
pub struct CreateListRequest {
    /// Тип списка.
    pub kind: ListKind,
    /// Название.
    pub name: String,
}

/// Изменение списка.
#[derive(Debug, Deserialize)]
pub struct UpdateListRequest {
    /// Новое название.
    pub name: Option<String>,
    /// Архивировать или вернуть из архива.
    pub is_archived: Option<bool>,
}

/// Создание элемента.
#[derive(Debug, Deserialize)]
pub struct CreateItemRequest {
    /// Название.
    pub title: String,
    /// Описание.
    pub notes: Option<String>,
    /// Приоритет.
    pub priority: Option<ItemPriority>,
    /// Срок со временем.
    pub due_at: Option<DateTime<Utc>>,
    /// Вседневный срок.
    pub due_date: Option<NaiveDate>,
    /// Исполнитель.
    pub assignee_user_id: Option<Uuid>,
    /// Количество.
    pub quantity: Option<f64>,
    /// Единица измерения.
    pub unit: Option<String>,
    /// Теги.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Подзадачи.
    #[serde(default)]
    pub subtasks: Vec<SubtaskInput>,
    /// Повторение.
    pub recurrence: Option<RecurrenceInput>,
    /// Источник; по умолчанию `api`.
    pub source: Option<ItemSource>,
}

/// Изменение элемента с обязательной версией.
#[derive(Debug, Deserialize)]
pub struct UpdateItemRequest {
    /// Ожидаемая версия.
    pub version: i32,
    /// Остальные поля.
    #[serde(flatten)]
    pub patch: ItemPatch,
}

/// Необязательная версия для complete/reopen.
#[derive(Debug, Default, Deserialize)]
pub struct VersionRequest {
    /// Ожидаемая версия.
    #[serde(default)]
    pub version: Option<i32>,
}

/// Ответ со списком элементов.
#[derive(Debug, Serialize)]
pub struct ItemsResponse {
    /// Элементы.
    pub items: Vec<ItemResponse>,
}

/// Результат поиска.
#[derive(Debug, Serialize)]
pub struct SearchItemResponse {
    /// Поля элемента.
    #[serde(flatten)]
    pub item: ItemResponse,
    /// Название списка.
    pub list_name: String,
    /// Тип списка.
    pub list_kind: ListKind,
    /// Релевантность.
    pub rank: f64,
}

/// Страница поиска.
#[derive(Debug, Serialize)]
pub struct SearchResponse {
    /// Найденные элементы.
    pub items: Vec<SearchItemResponse>,
    /// Курсор следующей страницы.
    pub next_cursor: Option<String>,
}

/// Календарные вхождения.
#[derive(Debug, Serialize)]
pub struct CalendarResponse {
    /// События.
    pub events: Vec<CalendarEvent>,
}

/// Инкрементальный снапшот для Home Assistant.
#[derive(Debug, Serialize)]
pub struct SnapshotResponse {
    /// Время сервера.
    pub server_time: DateTime<Utc>,
    /// Курсор следующего запроса.
    pub next_since: DateTime<Utc>,
    /// Списки.
    pub lists: Vec<List>,
    /// Изменённые элементы.
    pub items: Vec<ItemResponse>,
    /// Идентификаторы удалённых элементов.
    pub deleted_item_ids: Vec<Uuid>,
    /// Календарные события.
    pub calendar_events: Vec<CalendarEvent>,
}
