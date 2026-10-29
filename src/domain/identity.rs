//! Типы идентичности: приглашения, аудит и уведомления об изменении состава.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use super::models::{ActorType, Household, User, UserRole};

/// Тип события аудита.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "audit_event_type", rename_all = "snake_case")]
pub enum AuditEventType {
    /// Household создан.
    HouseholdCreated,
    /// Участник присоединился.
    MemberJoined,
    /// Участник вышел.
    MemberLeft,
    /// Участника удалили.
    MemberRemoved,
    /// Изменена роль.
    RoleChanged,
    /// Приглашение создано.
    InviteCreated,
    /// Приглашение использовано.
    InviteUsed,
    /// Приглашение отозвано.
    InviteRevoked,
    /// API-токен выпущен.
    TokenCreated,
    /// API-токен отозван.
    TokenRevoked,
    /// Отказ аутентификации.
    AuthFailed,
    /// Сработал rate limit.
    RateLimited,
    /// Household удалён.
    HouseholdDeleted,
}

impl AuditEventType {
    /// Значение в базе и JSON.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HouseholdCreated => "household_created",
            Self::MemberJoined => "member_joined",
            Self::MemberLeft => "member_left",
            Self::MemberRemoved => "member_removed",
            Self::RoleChanged => "role_changed",
            Self::InviteCreated => "invite_created",
            Self::InviteUsed => "invite_used",
            Self::InviteRevoked => "invite_revoked",
            Self::TokenCreated => "token_created",
            Self::TokenRevoked => "token_revoked",
            Self::AuthFailed => "auth_failed",
            Self::RateLimited => "rate_limited",
            Self::HouseholdDeleted => "household_deleted",
        }
    }

    /// События, которые владелец видит в разделе «Журнал».
    pub const MEMBERSHIP: [Self; 9] = [
        Self::HouseholdCreated,
        Self::MemberJoined,
        Self::MemberLeft,
        Self::MemberRemoved,
        Self::RoleChanged,
        Self::InviteCreated,
        Self::InviteUsed,
        Self::InviteRevoked,
        Self::HouseholdDeleted,
    ];
}

/// Событие аудита.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct AuditEvent {
    /// Идентификатор.
    pub id: Uuid,
    /// Household; `None` для системных событий.
    pub household_id: Option<Uuid>,
    /// Пользователь-актор.
    pub actor_user_id: Option<Uuid>,
    /// Тип актора: источник действия.
    pub actor_type: ActorType,
    /// Тип события.
    pub event_type: AuditEventType,
    /// Данные без секретов.
    pub payload: serde_json::Value,
    /// Время события.
    pub created_at: DateTime<Utc>,
}

/// Приглашение без хеша кода.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Invite {
    /// Идентификатор.
    pub id: Uuid,
    /// Household.
    pub household_id: Uuid,
    /// Роль приглашённого.
    pub role: UserRole,
    /// Кто создал приглашение.
    pub created_by_user_id: Option<Uuid>,
    /// Срок действия.
    pub expires_at: DateTime<Utc>,
    /// Кто использовал приглашение.
    pub used_by_user_id: Option<Uuid>,
    /// Когда использовано.
    pub used_at: Option<DateTime<Utc>>,
    /// Когда отозвано.
    pub revoked_at: Option<DateTime<Utc>>,
    /// Время создания.
    pub created_at: DateTime<Utc>,
}

/// Новый участник из Telegram.
#[derive(Debug, Clone)]
pub struct NewMember {
    /// Telegram user id.
    pub telegram_user_id: i64,
    /// Telegram chat id.
    pub telegram_chat_id: i64,
    /// Отображаемое имя.
    pub display_name: String,
    /// Telegram username.
    pub username: Option<String>,
}

/// Уведомление об изменении состава, которое отправит Telegram-слой.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MembershipNotice {
    /// Получатель.
    pub recipient_user_id: Uuid,
    /// Чат получателя.
    pub chat_id: i64,
    /// Содержание.
    pub kind: NoticeKind,
}

/// Содержание уведомления об изменении состава.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NoticeKind {
    /// Владельцу: новый участник присоединился.
    MemberJoined {
        /// Имя участника.
        display_name: String,
    },
    /// Владельцу: участник вышел.
    MemberLeft {
        /// Имя участника.
        display_name: String,
    },
    /// Участнику: доступ прекращён. Сообщение не раскрывает данные household.
    AccessRevoked,
    /// Новому владельцу: ему передано владение.
    OwnershipReceived,
    /// Участникам: household удалён.
    HouseholdDeleted {
        /// Название household.
        household_name: String,
    },
}

/// Результат присоединения к household.
#[derive(Debug, Clone)]
pub struct Joined {
    /// Household.
    pub household: Household,
    /// Новый участник.
    pub user: User,
    /// Уведомления владельцу.
    pub notices: Vec<MembershipNotice>,
}

/// Результат онбординга.
#[derive(Debug, Clone)]
pub struct Onboarded {
    /// Household.
    pub household: Household,
    /// Пользователь.
    pub user: User,
    /// `false`, если пользователь уже состоял в household (повторный шаг или
    /// владелец, добавленный администратором).
    pub created: bool,
}

/// Результат изменения состава household.
#[derive(Debug, Clone, Default)]
pub struct MembershipChange {
    /// Уведомления, которые нужно отправить.
    pub notices: Vec<MembershipNotice>,
    /// Сколько элементов потеряло исполнителя.
    pub unassigned_items: u64,
}
