//! Контекст household для разбора: часовой пояс, списки и участники.
//!
//! Один и тот же контекст идёт в промпт (только имена) и в серверную
//! валидацию (идентификаторы). Идентификаторы в промпт не попадают.

use chrono::{DateTime, DurationRound, NaiveDate, TimeDelta, Utc};
use chrono_tz::Tz;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{List, ListKind, User};
use crate::domain::recurrence::parse_timezone;

/// Список, доступный для разбора.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListRef {
    /// Идентификатор.
    pub id: Uuid,
    /// Название.
    pub name: String,
    /// Тип.
    pub kind: ListKind,
    /// Список по умолчанию для своего типа.
    pub is_default: bool,
}

/// Участник household, которому можно назначить элемент.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRef {
    /// Идентификатор.
    pub id: Uuid,
    /// Отображаемое имя.
    pub display_name: String,
    /// Telegram username без `@`.
    pub username: Option<String>,
}

/// Данные household, нужные для одного разбора.
#[derive(Debug, Clone)]
pub struct CaptureContext {
    /// Текущий момент.
    pub now: DateTime<Utc>,
    /// Часовой пояс household.
    pub timezone: Tz,
    /// Активные списки.
    pub lists: Vec<ListRef>,
    /// Активные участники.
    pub members: Vec<MemberRef>,
    /// Автор заметки, если он известен. Нужен, чтобы «мне»/«я» в тексте
    /// разрешались в него, а не оставались неизвестным исполнителем.
    pub author: Option<MemberRef>,
}

impl CaptureContext {
    /// Собрать контекст из моделей; архивные и удалённые списки и удалённые
    /// участники в него не попадают.
    ///
    /// Списки событий входят в контекст наравне с остальными: разбор создаёт
    /// события (7.2), поэтому они есть и в промпте, и среди целевых списков
    /// элементов черновика.
    pub fn new(
        timezone: &str,
        now: DateTime<Utc>,
        lists: &[List],
        members: &[User],
    ) -> Result<Self, DomainError> {
        let lists = lists
            .iter()
            .filter(|list| !list.is_archived && list.deleted_at.is_none())
            .map(|list| ListRef {
                id: list.id,
                name: list.name.clone(),
                kind: list.kind,
                is_default: list.is_default,
            })
            .collect();
        let members = members
            .iter()
            .filter(|user| user.deleted_at.is_none())
            .map(|user| MemberRef {
                id: user.id,
                display_name: user.display_name.clone(),
                username: user.username.clone(),
            })
            .collect();
        Ok(Self {
            now,
            timezone: parse_timezone(timezone)?,
            lists,
            members,
            author: None,
        })
    }

    /// Указать автора заметки. Автор обязан быть активным участником: иначе
    /// «мне» нельзя назначить задачу, и поле остаётся пустым.
    #[must_use]
    pub fn with_author(mut self, author_id: Option<Uuid>) -> Self {
        self.author =
            author_id.and_then(|id| self.members.iter().find(|member| member.id == id).cloned());
        self
    }

    /// Текущий момент с точностью до минуты: так время видит модель в промпте,
    /// поэтому «в 14:05» в 14:05:30 ещё не прошло.
    pub fn now_minute(&self) -> DateTime<Utc> {
        self.now
            .duration_trunc(TimeDelta::minutes(1))
            .unwrap_or(self.now)
    }

    /// Сегодняшняя дата в часовом поясе household.
    pub fn today(&self) -> NaiveDate {
        self.now.with_timezone(&self.timezone).date_naive()
    }

    /// Список по умолчанию для типа: помеченный `is_default`, иначе первый.
    pub fn default_list(&self, kind: ListKind) -> Option<&ListRef> {
        let mut of_kind = self.lists.iter().filter(|list| list.kind == kind);
        of_kind
            .clone()
            .find(|list| list.is_default)
            .or_else(|| of_kind.next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::test_support::{model_list, now};

    #[test]
    fn event_lists_are_part_of_the_capture_context() {
        let lists = vec![
            model_list("Задачи", ListKind::Tasks, true),
            model_list("Покупки", ListKind::Shopping, true),
            model_list("Календарь", ListKind::Events, true),
        ];

        let context =
            CaptureContext::new("Europe/Moscow", now(), &lists, &[]).expect("context is valid");

        assert_eq!(
            context
                .lists
                .iter()
                .map(|list| list.name.as_str())
                .collect::<Vec<_>>(),
            ["Задачи", "Покупки", "Календарь"]
        );
        assert_eq!(
            context
                .default_list(ListKind::Events)
                .map(|list| list.name.as_str()),
            Some("Календарь")
        );
    }
}
