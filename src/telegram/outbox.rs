//! Исходящие сообщения: очередь, лимиты и доставка.
//!
//! Всё, что бот показывает пользователю вне мгновенного ответа на callback,
//! сначала записывается в `outbox` вместе с доменными эффектами, а доставляет
//! его отдельный воркер. Он соблюдает порядок сообщений чата, лимиты Telegram
//! (1 сообщение/сек на чат, 30/сек суммарно) и `retry_after` при `429`.
//!
//! Доставка — at-least-once: если Telegram принял сообщение, а ответ не дошёл
//! (таймаут) или транзакция не зафиксировалась, строка остаётся `pending` и
//! сообщение уйдёт повторно. `message_id` появляется только после успешной
//! отправки, поэтому защитить от такой копии нельзя; экраны при этом
//! выправляются сами: следующий экран редактирует активное сообщение чата.
//! Текст сообщения живёт в `payload` только до отправки: в `sent`, `skipped`
//! и `dead` он затирается.

use std::collections::{HashMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, Utc};
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use tokio::sync::{Notify, watch};

use super::api::{RichMessage, TelegramApi, TelegramError};
use super::queue::backoff;
use super::views;

/// Не больше одного сообщения в секунду в один чат.
pub const CHAT_INTERVAL: StdDuration = StdDuration::from_secs(1);
/// Не больше 30 сообщений в секунду суммарно.
pub const GLOBAL_PER_SECOND: usize = 30;
/// Максимум попыток доставки.
pub const MAX_ATTEMPTS: i32 = 5;
/// Сколько кандидатов рассматривается за один проход.
const BATCH: i64 = 50;

/// Что отправить.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outgoing {
    /// Экран навигации: редактирует активное сообщение чата или заменяет его.
    Screen {
        /// HTML RichMessage.
        html: String,
    },
    /// Новое самостоятельное сообщение.
    Message {
        /// HTML.
        html: String,
        /// RichMessage с кнопками (иначе обычное HTML-сообщение).
        rich: bool,
    },
    /// Редактирование конкретного сообщения (например, уведомления).
    Edit {
        /// Сообщение.
        message_id: i64,
        /// HTML RichMessage.
        html: String,
    },
}

impl Outgoing {
    fn kind(&self) -> &'static str {
        match self {
            Self::Screen { .. } => "screen",
            Self::Message { .. } => "message",
            Self::Edit { .. } => "edit",
        }
    }

    fn payload(&self) -> Value {
        match self {
            Self::Screen { html } => json!({"html": html}),
            Self::Message { html, rich } => json!({"html": html, "rich": rich}),
            Self::Edit { message_id, html } => json!({"html": html, "message_id": message_id}),
        }
    }

    fn from_row(kind: &str, payload: &Value) -> Option<Self> {
        let html = payload.get("html")?.as_str()?.to_owned();
        match kind {
            "screen" => Some(Self::Screen { html }),
            "message" => Some(Self::Message {
                html,
                rich: payload
                    .get("rich")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }),
            "edit" => Some(Self::Edit {
                message_id: payload.get("message_id")?.as_i64()?,
                html,
            }),
            _ => None,
        }
    }
}

/// Поставить сообщение в очередь. Возвращает `seq` или `None`, если сообщение с
/// таким `dedupe_key` уже есть.
///
/// Экран заменяет все ещё не доставленные экраны того же чата: пользователю
/// нужно только актуальное состояние.
pub async fn enqueue(
    conn: &mut PgConnection,
    chat_id: i64,
    outgoing: &Outgoing,
    dedupe_key: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<i64>, sqlx::Error> {
    if matches!(outgoing, Outgoing::Screen { .. }) {
        sqlx::query(
            "UPDATE outbox SET status = 'skipped', payload = jsonb_build_object('redacted', true)
             WHERE chat_id = $1 AND status = 'pending' AND kind = 'screen'
               AND message_id IS NULL",
        )
        .bind(chat_id)
        .execute(&mut *conn)
        .await?;
    }

    let seq: Option<i64> = sqlx::query_scalar(
        "INSERT INTO outbox (chat_id, kind, payload, dedupe_key, next_attempt_at, created_at)
         VALUES ($1, $2::outbox_kind, $3, $4, $5, $5)
         ON CONFLICT (dedupe_key) DO NOTHING
         RETURNING seq",
    )
    .bind(chat_id)
    .bind(outgoing.kind())
    .bind(outgoing.payload())
    .bind(dedupe_key)
    .bind(now)
    .fetch_optional(conn)
    .await?;

    Ok(seq)
}

/// Состояние outbox.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Ждут доставки.
    pub pending: i64,
    /// Доставлены.
    pub sent: i64,
    /// Не доставлены после всех попыток.
    pub dead: i64,
}

/// Количество сообщений по статусам.
pub async fn counts(conn: &mut PgConnection) -> Result<Counts, sqlx::Error> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT status::text, count(*) FROM outbox GROUP BY status")
            .fetch_all(conn)
            .await?;

    let mut counts = Counts::default();
    for (status, count) in rows {
        match status.as_str() {
            "pending" => counts.pending = count,
            "sent" => counts.sent = count,
            "dead" => counts.dead = count,
            _ => {}
        }
    }
    Ok(counts)
}

/// Удалить завершённые сообщения (отправленные, пропущенные и мёртвые) старше
/// `cutoff`. Ожидающие доставки остаются.
pub async fn purge(conn: &mut PgConnection, cutoff: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "DELETE FROM outbox WHERE status IN ('sent', 'skipped', 'dead') AND created_at < $1",
    )
    .bind(cutoff)
    .execute(conn)
    .await?
    .rows_affected())
}

/// Ограничитель частоты исходящих сообщений.
///
/// Время передаётся снаружи, поэтому поведение проверяется без пауз.
#[derive(Debug, Default)]
pub struct RateLimiter {
    per_chat: HashMap<i64, Instant>,
    global: VecDeque<Instant>,
    paused_until: Option<Instant>,
}

impl RateLimiter {
    /// Занять слот для отправки в чат; `false` — отправлять пока нельзя.
    pub fn try_acquire(&mut self, chat_id: i64, now: Instant) -> bool {
        if self.paused_until.is_some_and(|until| now < until) {
            return false;
        }
        while self
            .global
            .front()
            .is_some_and(|sent| now.duration_since(*sent) >= StdDuration::from_secs(1))
        {
            self.global.pop_front();
        }
        if self.global.len() >= GLOBAL_PER_SECOND {
            return false;
        }
        if self
            .per_chat
            .get(&chat_id)
            .is_some_and(|last| now.duration_since(*last) < CHAT_INTERVAL)
        {
            return false;
        }

        self.per_chat.insert(chat_id, now);
        self.global.push_back(now);
        // Карта не растёт без границ: давно неактивные чаты забываются.
        if self.per_chat.len() > 4_096 {
            self.per_chat
                .retain(|_, last| now.duration_since(*last) < CHAT_INTERVAL);
        }
        true
    }

    /// Остановить все отправки до `until` (ответ `429`).
    pub fn pause_until(&mut self, until: Instant) {
        self.paused_until = Some(
            self.paused_until
                .map_or(until, |current| current.max(until)),
        );
    }
}

/// Итог обработки одного сообщения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Доставлено.
    Sent,
    /// Отложено: `429` или временная ошибка.
    Deferred,
    /// Попытки исчерпаны или ошибка постоянная.
    Dead,
    /// Нечего отправлять.
    Idle,
}

/// Воркер доставки.
pub struct OutboxWorker {
    pool: PgPool,
    api: Arc<dyn TelegramApi>,
    limiter: RateLimiter,
    only_chat: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct Row {
    seq: i64,
    chat_id: i64,
    kind: String,
    payload: Value,
    attempts: i32,
}

impl OutboxWorker {
    /// Создать воркер.
    pub fn new(pool: PgPool, api: Arc<dyn TelegramApi>) -> Self {
        Self {
            pool,
            api,
            limiter: RateLimiter::default(),
            only_chat: None,
        }
    }

    /// Доставлять только сообщения одного чата (для тестов).
    #[must_use]
    pub fn only_chat(mut self, chat_id: i64) -> Self {
        self.only_chat = Some(chat_id);
        self
    }

    /// Один проход: доставить всё, что готово и разрешено лимитами.
    /// Возвращает число доставленных сообщений.
    pub async fn run_once(
        &mut self,
        now: DateTime<Utc>,
        clock: Instant,
    ) -> Result<usize, sqlx::Error> {
        // Головы очередей по чатам: следующее сообщение чата не обгоняет
        // предыдущее, даже если оно ждёт повтора после ошибки. Сообщение,
        // изначально запланированное на будущее (отложенное напоминание),
        // очередь не блокирует.
        let candidates: Vec<i64> = sqlx::query_scalar(
            "SELECT o.seq FROM outbox o
             WHERE o.status = 'pending' AND o.next_attempt_at <= $1
               AND ($3::bigint IS NULL OR o.chat_id = $3)
               AND NOT EXISTS (
                   SELECT 1 FROM outbox p
                   WHERE p.chat_id = o.chat_id AND p.status = 'pending' AND p.seq < o.seq
                     AND (p.next_attempt_at <= $1 OR p.last_error IS NOT NULL)
               )
             ORDER BY o.seq
             LIMIT $2",
        )
        .bind(now)
        .bind(BATCH)
        .bind(self.only_chat)
        .fetch_all(&self.pool)
        .await?;

        let mut sent = 0;
        for seq in candidates {
            match self.deliver_one(seq, now, clock).await? {
                Delivery::Sent => sent += 1,
                Delivery::Deferred | Delivery::Dead | Delivery::Idle => {}
            }
        }
        Ok(sent)
    }

    async fn deliver_one(
        &mut self,
        seq: i64,
        now: DateTime<Utc>,
        clock: Instant,
    ) -> Result<Delivery, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // Строка блокируется на время вызова API: второй воркер её пропустит.
        let row: Option<Row> = sqlx::query_as(
            "SELECT seq, chat_id, kind::text AS kind, payload, attempts
             FROM outbox
             WHERE seq = $1 AND status = 'pending' AND next_attempt_at <= $2
             FOR UPDATE SKIP LOCKED",
        )
        .bind(seq)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Ok(Delivery::Idle);
        };
        if !self.limiter.try_acquire(row.chat_id, clock) {
            return Ok(Delivery::Idle);
        }

        let Some(outgoing) = Outgoing::from_row(&row.kind, &row.payload) else {
            mark_dead(&mut tx, row.seq, "invalid_payload").await?;
            tx.commit().await?;
            return Ok(Delivery::Dead);
        };

        let result = deliver(self.api.as_ref(), &mut tx, &row, &outgoing, now).await;
        let outcome = match result {
            Ok(message_id) => {
                sqlx::query(
                    "UPDATE outbox
                     SET status = 'sent', message_id = $2, sent_at = $3, last_error = NULL,
                         payload = jsonb_build_object('redacted', true)
                     WHERE seq = $1",
                )
                .bind(row.seq)
                .bind(message_id)
                .bind(now)
                .execute(&mut *tx)
                .await?;
                Delivery::Sent
            }
            Err(TelegramError::RetryAfter(seconds)) => {
                let wait = i64::try_from(seconds).unwrap_or(i64::MAX).min(3_600);
                // `429` не считается неудачной попыткой: сообщение не теряется.
                sqlx::query(
                    "UPDATE outbox SET next_attempt_at = $2, last_error = 'retry_after'
                     WHERE seq = $1",
                )
                .bind(row.seq)
                .bind(now + Duration::seconds(wait))
                .execute(&mut *tx)
                .await?;
                self.limiter
                    .pause_until(clock + StdDuration::from_secs(wait.unsigned_abs()));
                Delivery::Deferred
            }
            Err(error) if error.is_retryable() => {
                let attempts = row.attempts + 1;
                if attempts >= MAX_ATTEMPTS {
                    mark_dead(&mut tx, row.seq, error.code()).await?;
                    tracing::error!(
                        seq = row.seq,
                        error = error.code(),
                        "outbox message is dead"
                    );
                    Delivery::Dead
                } else {
                    sqlx::query(
                        "UPDATE outbox
                         SET attempts = $2, next_attempt_at = $3, last_error = $4
                         WHERE seq = $1",
                    )
                    .bind(row.seq)
                    .bind(attempts)
                    .bind(now + backoff(attempts))
                    .bind(error.code())
                    .execute(&mut *tx)
                    .await?;
                    Delivery::Deferred
                }
            }
            Err(error) => {
                mark_dead(&mut tx, row.seq, error.code()).await?;
                tracing::warn!(
                    seq = row.seq,
                    error = error.code(),
                    "outbox message rejected"
                );
                Delivery::Dead
            }
        };

        tx.commit().await?;
        Ok(outcome)
    }

    /// Цикл воркера до сигнала остановки.
    pub async fn run(mut self, wake: Arc<Notify>, mut shutdown: watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow() {
                return;
            }
            // Паника не должна молча останавливать доставку.
            let delivered = match AssertUnwindSafe(self.run_once(Utc::now(), Instant::now()))
                .catch_unwind()
                .await
            {
                Ok(Ok(delivered)) => delivered,
                Ok(Err(error)) => {
                    tracing::error!(%error, "outbox pass failed");
                    0
                }
                Err(_) => {
                    tracing::error!("outbox pass panicked");
                    0
                }
            };
            if delivered > 0 {
                continue;
            }
            tokio::select! {
                _ = shutdown.changed() => {}
                () = wake.notified() => {}
                () = tokio::time::sleep(StdDuration::from_millis(500)) => {}
            }
        }
    }
}

async fn mark_dead(conn: &mut PgConnection, seq: i64, code: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE outbox
         SET status = 'dead', last_error = $2, payload = jsonb_build_object('redacted', true)
         WHERE seq = $1",
    )
    .bind(seq)
    .bind(code)
    .execute(conn)
    .await?;
    Ok(())
}

/// Выполнить доставку. Возвращает `message_id` сообщения, в котором показан
/// результат.
async fn deliver(
    api: &dyn TelegramApi,
    conn: &mut PgConnection,
    row: &Row,
    outgoing: &Outgoing,
    now: DateTime<Utc>,
) -> Result<i64, TelegramError> {
    let db = |error: sqlx::Error| TelegramError::Transient(format!("database: {error}"));

    match outgoing {
        Outgoing::Screen { html } => {
            let rich = RichMessage { html: html.clone() };
            let active = views::get(conn, row.chat_id).await.map_err(db)?;
            let target = active.and_then(|view| view.message_id);
            let message_id = replace_or_send(api, row.chat_id, target, &rich).await?;
            views::set_message(conn, row.chat_id, message_id, now)
                .await
                .map_err(db)?;
            Ok(message_id)
        }
        Outgoing::Message { html, rich } => {
            if *rich {
                api.send_rich_message(row.chat_id, &RichMessage { html: html.clone() })
                    .await
            } else {
                api.send_message(row.chat_id, html).await
            }
        }
        Outgoing::Edit { message_id, html } => {
            let rich = RichMessage { html: html.clone() };
            replace_or_send(api, row.chat_id, Some(*message_id), &rich).await
        }
    }
}

/// Отредактировать сообщение; если это невозможно — удалить старое и отправить
/// новое.
async fn replace_or_send(
    api: &dyn TelegramApi,
    chat_id: i64,
    target: Option<i64>,
    message: &RichMessage,
) -> Result<i64, TelegramError> {
    let Some(message_id) = target else {
        return api.send_rich_message(chat_id, message).await;
    };

    match api.edit_rich_message(chat_id, message_id, message).await {
        Ok(()) | Err(TelegramError::NotModified) => Ok(message_id),
        Err(TelegramError::MessageGone) => {
            // Старое сообщение уже могло исчезнуть; ошибка удаления не мешает.
            let _ = api.delete_message(chat_id, message_id).await;
            api.send_rich_message(chat_id, message).await
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_allows_one_message_per_second_per_chat() {
        let mut limiter = RateLimiter::default();
        let start = Instant::now();

        assert!(limiter.try_acquire(1, start));
        assert!(!limiter.try_acquire(1, start + StdDuration::from_millis(900)));
        assert!(limiter.try_acquire(2, start + StdDuration::from_millis(900)));
        assert!(limiter.try_acquire(1, start + StdDuration::from_millis(1_000)));
    }

    #[test]
    fn limiter_caps_global_rate_at_thirty_per_second() {
        let mut limiter = RateLimiter::default();
        let start = Instant::now();

        for chat in 0..30 {
            assert!(limiter.try_acquire(chat, start));
        }
        assert!(!limiter.try_acquire(100, start + StdDuration::from_millis(500)));
        assert!(limiter.try_acquire(100, start + StdDuration::from_secs(1)));
    }

    #[test]
    fn limiter_pause_blocks_every_chat() {
        let mut limiter = RateLimiter::default();
        let start = Instant::now();

        limiter.pause_until(start + StdDuration::from_secs(5));
        assert!(!limiter.try_acquire(1, start + StdDuration::from_secs(4)));
        assert!(limiter.try_acquire(1, start + StdDuration::from_secs(5)));
    }

    #[test]
    fn outgoing_round_trips_through_rows() {
        for outgoing in [
            Outgoing::Screen { html: "a".into() },
            Outgoing::Message {
                html: "b".into(),
                rich: true,
            },
            Outgoing::Edit {
                message_id: 7,
                html: "c".into(),
            },
        ] {
            assert_eq!(
                Outgoing::from_row(outgoing.kind(), &outgoing.payload()),
                Some(outgoing)
            );
        }
    }
}
