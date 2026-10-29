//! Состояние экранов и форм в базе.
//!
//! `telegram_views` помнит активное сообщение экрана чата, чтобы навигация
//! редактировала его, в том числе после перезапуска. `conversation_states`
//! хранит единственную активную форму чата с TTL 15 минут.

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use sqlx::PgConnection;

/// Срок жизни формы.
pub const FORM_TTL: Duration = Duration::minutes(15);

/// Активный экран чата.
#[derive(Debug, Clone)]
pub struct ActiveView {
    /// Сообщение с экраном; пусто, пока оно не доставлено.
    pub message_id: Option<i64>,
    /// Ключ экрана для диагностики.
    pub view_key: String,
    /// Состояние, достаточное для перерисовки экрана.
    pub state: Value,
}

/// Активный экран чата.
pub async fn get(conn: &mut PgConnection, chat_id: i64) -> Result<Option<ActiveView>, sqlx::Error> {
    let row: Option<(Option<i64>, String, Value)> =
        sqlx::query_as("SELECT message_id, view_key, state FROM telegram_views WHERE chat_id = $1")
            .bind(chat_id)
            .fetch_optional(conn)
            .await?;

    Ok(row.map(|(message_id, view_key, state)| ActiveView {
        message_id,
        view_key,
        state,
    }))
}

/// Запомнить текущий экран; идентификатор сообщения сохраняется.
pub async fn set(
    conn: &mut PgConnection,
    chat_id: i64,
    view_key: &str,
    state: &Value,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO telegram_views (chat_id, view_key, state, updated_at)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (chat_id)
         DO UPDATE SET view_key = EXCLUDED.view_key,
                       state = EXCLUDED.state,
                       updated_at = EXCLUDED.updated_at",
    )
    .bind(chat_id)
    .bind(view_key)
    .bind(state)
    .bind(now)
    .execute(conn)
    .await?;

    Ok(())
}

/// Записать сообщение, в котором показан экран.
pub async fn set_message(
    conn: &mut PgConnection,
    chat_id: i64,
    message_id: i64,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO telegram_views (chat_id, message_id, view_key, updated_at)
         VALUES ($1, $2, 'unknown', $3)
         ON CONFLICT (chat_id)
         DO UPDATE SET message_id = EXCLUDED.message_id, updated_at = EXCLUDED.updated_at",
    )
    .bind(chat_id)
    .bind(message_id)
    .bind(now)
    .execute(conn)
    .await?;

    Ok(())
}

/// Форма чата.
#[derive(Debug, Clone)]
pub struct Conversation {
    /// Пользователь, которому принадлежит форма.
    pub telegram_user_id: i64,
    /// Вид формы.
    pub kind: String,
    /// Введённые данные и текущий шаг.
    pub data: Value,
}

/// Результат поиска формы.
#[derive(Debug, Clone)]
pub enum Lookup {
    /// Формы нет.
    None,
    /// Форма активна.
    Active(Conversation),
    /// Форма была, но срок её жизни истёк.
    Expired(Conversation),
}

/// Форма чата с учётом срока жизни.
pub async fn conversation(
    conn: &mut PgConnection,
    chat_id: i64,
    now: DateTime<Utc>,
) -> Result<Lookup, sqlx::Error> {
    let row: Option<(i64, String, Value, DateTime<Utc>)> = sqlx::query_as(
        "SELECT telegram_user_id, kind, data, expires_at
         FROM conversation_states WHERE chat_id = $1",
    )
    .bind(chat_id)
    .fetch_optional(conn)
    .await?;

    Ok(match row {
        None => Lookup::None,
        Some((telegram_user_id, kind, data, expires_at)) => {
            let conversation = Conversation {
                telegram_user_id,
                kind,
                data,
            };
            if expires_at > now {
                Lookup::Active(conversation)
            } else {
                Lookup::Expired(conversation)
            }
        }
    })
}

/// Создать или заменить форму чата; срок жизни отсчитывается заново.
pub async fn put_conversation(
    conn: &mut PgConnection,
    chat_id: i64,
    telegram_user_id: i64,
    kind: &str,
    data: &Value,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO conversation_states (
             chat_id, telegram_user_id, kind, data, expires_at, created_at, updated_at
         )
         VALUES ($1, $2, $3, $4, $5, $6, $6)
         ON CONFLICT (chat_id)
         DO UPDATE SET telegram_user_id = EXCLUDED.telegram_user_id,
                       kind = EXCLUDED.kind,
                       data = EXCLUDED.data,
                       expires_at = EXCLUDED.expires_at,
                       updated_at = EXCLUDED.updated_at",
    )
    .bind(chat_id)
    .bind(telegram_user_id)
    .bind(kind)
    .bind(data)
    .bind(now + FORM_TTL)
    .bind(now)
    .execute(conn)
    .await?;

    Ok(())
}

/// Удалить форму чата.
pub async fn clear_conversation(conn: &mut PgConnection, chat_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM conversation_states WHERE chat_id = $1")
        .bind(chat_id)
        .execute(conn)
        .await?;

    Ok(())
}

/// Удалить формы, истёкшие до `cutoff`.
pub async fn purge_conversations(
    conn: &mut PgConnection,
    cutoff: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    Ok(
        sqlx::query("DELETE FROM conversation_states WHERE expires_at < $1")
            .bind(cutoff)
            .execute(conn)
            .await?
            .rows_affected(),
    )
}
