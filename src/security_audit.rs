//! События безопасности: отказы аутентификации, превышения лимитов, ошибки
//! LLM-валидации и порог отказов webhook.
//!
//! Запись идёт отдельной короткой операцией пула, а не в транзакции запроса:
//! отказ webhook или API не должен откатывать уже записанное событие. Ошибка
//! записи только журналируется — основной путь обрабатывает запрос как раньше.
//! Payload собирается из явных полей: токены, их значения, префиксы и хеши,
//! пользовательский текст и ответы LLM в журнал не попадают.
//!
//! Флуд не должен раздувать журнал: на пару (тип события, ключ) пишется не
//! больше одного события за [`SUPPRESSION_WINDOW_SECONDS`]; сколько повторов
//! подавлено, видно в поле `suppressed`. Метрики при этом считают все случаи.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::identity::AuditEventType;
use crate::domain::models::{Actor, ActorType, CaptureSource};
use crate::metrics;
use crate::storage::repositories::audit;

/// Окно подавления повторов, секунды.
pub const SUPPRESSION_WINDOW_SECONDS: i64 = 60;
/// Ключ без событий дольше этого времени удаляется из памяти, секунды.
pub const SUPPRESSION_KEY_TTL_SECONDS: i64 = 3_600;
/// Окно подсчёта отказов webhook, секунды.
pub const WEBHOOK_FAILURE_WINDOW_SECONDS: i64 = 300;
/// Порог отказов webhook в окне: превышение создаёт событие и метрику.
pub const WEBHOOK_FAILURE_THRESHOLD: u64 = 50;

/// Имя лимита capture в событиях.
pub const CAPTURE_LIMIT: &str = "capture_per_minute";
/// Имя лимита запросов webhook с одного IP.
pub const WEBHOOK_LIMIT: &str = "webhook_per_second";
/// Имя лимита обновлений Telegram на пользователя.
pub const TELEGRAM_LIMIT: &str = "telegram_updates_per_minute";
/// Имя лимита создания приглашений на household.
pub const INVITES_LIMIT: &str = "invites_per_hour";

/// Тип события для подавления повторов.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SuppressKind {
    AuthFailed,
    RateLimited,
    LlmValidationFailed,
}

#[derive(Debug, Clone, Copy)]
struct Suppressed {
    last: DateTime<Utc>,
    suppressed: u64,
}

/// Счётчики подавления: сколько одинаковых событий пришло после записанного.
#[derive(Debug, Default)]
struct Suppression {
    entries: HashMap<(SuppressKind, String), Suppressed>,
}

impl Suppression {
    /// Проверить, нужно ли писать событие. `Some(n)` — писать, `n` повторов
    /// подавлено; `None` — событие подавлено.
    fn allow(&mut self, kind: SuppressKind, key: &str, now: DateTime<Utc>) -> Option<u64> {
        let ttl = Duration::seconds(SUPPRESSION_KEY_TTL_SECONDS);
        self.entries.retain(|_, entry| now - entry.last < ttl);

        let window = Duration::seconds(SUPPRESSION_WINDOW_SECONDS);
        match self.entries.get_mut(&(kind, key.to_owned())) {
            Some(entry) if now - entry.last < window => {
                entry.suppressed += 1;
                None
            }
            Some(entry) => {
                let suppressed = entry.suppressed;
                entry.last = now;
                entry.suppressed = 0;
                Some(suppressed)
            }
            None => {
                self.entries.insert(
                    (kind, key.to_owned()),
                    Suppressed {
                        last: now,
                        suppressed: 0,
                    },
                );
                Some(0)
            }
        }
    }

    /// Хранится ли ключ; только для тестов.
    #[cfg(test)]
    fn tracks(&self, key: &str) -> bool {
        self.entries.keys().any(|(_, stored)| stored == key)
    }
}

/// Скользящее окно отказов аутентификации webhook.
#[derive(Debug, Default)]
struct WebhookFailures {
    hits: VecDeque<DateTime<Utc>>,
    exceeded: bool,
}

impl WebhookFailures {
    /// Учесть отказ. `Some(count)` — порог только что превышен; пока
    /// превышение длится, событие не повторяется.
    fn record(&mut self, now: DateTime<Utc>) -> Option<u64> {
        let cutoff = now - Duration::seconds(WEBHOOK_FAILURE_WINDOW_SECONDS);
        while self.hits.front().is_some_and(|at| *at <= cutoff) {
            self.hits.pop_front();
        }
        self.hits.push_back(now);

        let count = u64::try_from(self.hits.len()).unwrap_or(u64::MAX);
        if count > WEBHOOK_FAILURE_THRESHOLD {
            if self.exceeded {
                None
            } else {
                self.exceeded = true;
                Some(count)
            }
        } else {
            self.exceeded = false;
            None
        }
    }
}

/// Ключ сработавшего лимита без секретов.
#[derive(Debug, Clone)]
pub enum LimitKey {
    /// API-токен capture.
    Token(Uuid),
    /// Telegram-пользователь.
    User(i64),
    /// Household.
    Household(Uuid),
    /// IP-адрес webhook.
    Ip(IpAddr),
}

impl LimitKey {
    /// Строка для карты подавления.
    fn serialized(&self) -> String {
        match self {
            Self::Token(id) => format!("token:{id}"),
            Self::User(id) => format!("user:{id}"),
            Self::Household(id) => format!("household:{id}"),
            Self::Ip(ip) => format!("ip:{ip}"),
        }
    }

    /// Имя поля в payload.
    const fn payload_field(&self) -> &'static str {
        match self {
            Self::Token(_) => "token_id",
            Self::User(_) => "user_id",
            Self::Household(_) => "household_id",
            Self::Ip(_) => "ip",
        }
    }

    fn payload_value(&self) -> Value {
        match self {
            Self::Token(id) | Self::Household(id) => json!(id),
            Self::User(id) => json!(id),
            Self::Ip(ip) => json!(ip.to_string()),
        }
    }
}

/// Имя источника capture в событиях безопасности.
pub const fn capture_source_label(source: CaptureSource) -> &'static str {
    match source {
        CaptureSource::Telegram => "telegram",
        CaptureSource::HaVoice => "voice",
        CaptureSource::HaAutomation | CaptureSource::Api | CaptureSource::Shortcut => "api",
    }
}

/// Сработавший лимит.
#[derive(Debug)]
pub struct RateLimitEvent {
    /// Имя лимита.
    pub limit: &'static str,
    /// Ключ без секретов.
    pub key: LimitKey,
    /// Через сколько секунд повторять.
    pub retry_after: u64,
    /// Household, если известен.
    pub household_id: Option<Uuid>,
    /// Актор действия.
    pub actor: Actor,
}

/// Ошибка LLM-валидации.
#[derive(Debug)]
pub struct LlmValidation {
    /// Источник: `telegram`, `voice` или `api`.
    pub source: &'static str,
    /// Стадия ошибки.
    pub stage: &'static str,
    /// Код ошибки без пользовательских данных.
    pub code: &'static str,
    /// Версия промпта.
    pub prompt_version: &'static str,
    /// Household.
    pub household_id: Uuid,
    /// Актор действия.
    pub actor: Actor,
    /// Идентификатор попытки capture.
    pub capture_id: Option<Uuid>,
    /// Идентификатор задания разбора.
    pub job_id: Option<Uuid>,
}

#[derive(Debug)]
struct Inner {
    pool: PgPool,
    suppression: Mutex<Suppression>,
    webhook_failures: Mutex<WebhookFailures>,
}

/// Журнал событий безопасности. Клоны делят состояние подавления.
#[derive(Debug, Clone)]
pub struct Audit {
    inner: Arc<Inner>,
}

impl Audit {
    /// Создать журнал поверх пула.
    pub fn new(pool: PgPool) -> Self {
        Self {
            inner: Arc::new(Inner {
                pool,
                suppression: Mutex::new(Suppression::default()),
                webhook_failures: Mutex::new(WebhookFailures::default()),
            }),
        }
    }

    fn check(&self, kind: SuppressKind, key: &str, now: DateTime<Utc>) -> Option<u64> {
        self.inner
            .suppression
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .allow(kind, key, now)
    }

    /// Записать событие отдельным соединением. Ошибка только журналируется.
    async fn record(
        &self,
        household_id: Option<Uuid>,
        actor: &Actor,
        event_type: AuditEventType,
        payload: Value,
        now: DateTime<Utc>,
    ) {
        let written = async {
            let mut conn = self.inner.pool.acquire().await?;
            audit::record(&mut conn, household_id, actor, event_type, payload, now)
                .await
                .map_err(|error| match error {
                    crate::domain::error::DomainError::Database(error) => error,
                    other => sqlx::Error::Protocol(format!("audit record failed: {other}")),
                })
        }
        .await;

        if let Err(error) = written {
            tracing::warn!(
                %error,
                event_type = event_type.as_str(),
                "security audit event was not recorded"
            );
        }
    }

    /// Отказ аутентификации webhook. Метрика считает каждый отказ, событие —
    /// не чаще раза в окно на IP.
    pub async fn webhook_auth_failed(&self, reason: &'static str, ip: IpAddr, now: DateTime<Utc>) {
        metrics::webhook_auth_failure();

        if let Some(suppressed) = self.check(SuppressKind::AuthFailed, &format!("ip:{ip}"), now) {
            self.record(
                None,
                &Actor::system(ActorType::System),
                AuditEventType::AuthFailed,
                json!({
                    "source": "webhook",
                    "reason": reason,
                    "ip": ip.to_string(),
                    "suppressed": suppressed,
                }),
                now,
            )
            .await;
        }

        let exceeded = self
            .inner
            .webhook_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record(now);
        if let Some(count) = exceeded {
            metrics::webhook_auth_failure_threshold_exceeded();
            self.record(
                None,
                &Actor::system(ActorType::System),
                AuditEventType::AuthFailureThresholdExceeded,
                json!({
                    "window_seconds": WEBHOOK_FAILURE_WINDOW_SECONDS,
                    "threshold": WEBHOOK_FAILURE_THRESHOLD,
                    "count": count,
                }),
                now,
            )
            .await;
        }
    }

    /// Отказ аутентификации API. `token` — идентификатор токена и household,
    /// если токен опознан (отозванный или просроченный тоже опознаётся).
    pub async fn api_auth_failed(
        &self,
        reason: &'static str,
        route: &str,
        token: Option<(Uuid, Uuid)>,
        now: DateTime<Utc>,
    ) {
        let key = token.map_or_else(
            || "api:unknown".to_owned(),
            |(token_id, _)| format!("api:token:{token_id}"),
        );
        let Some(suppressed) = self.check(SuppressKind::AuthFailed, &key, now) else {
            return;
        };

        let (token_id, household_id) = match token {
            Some((token_id, household_id)) => (Some(token_id), Some(household_id)),
            None => (None, None),
        };
        let actor_type = if token_id.is_some() {
            ActorType::HomeAssistant
        } else {
            ActorType::System
        };
        let mut payload = json!({
            "source": "api",
            "reason": reason,
            "route": route,
            "suppressed": suppressed,
        });
        if let Some(token_id) = token_id {
            payload["token_id"] = json!(token_id);
        }

        self.record(
            household_id,
            &Actor::system(actor_type),
            AuditEventType::AuthFailed,
            payload,
            now,
        )
        .await;
    }

    /// Сработавший лимит частоты.
    pub async fn rate_limited(&self, event: RateLimitEvent, now: DateTime<Utc>) {
        let key = format!("{}:{}", event.limit, event.key.serialized());
        let Some(suppressed) = self.check(SuppressKind::RateLimited, &key, now) else {
            return;
        };

        let mut payload = json!({
            "limit": event.limit,
            "retry_after": event.retry_after,
            "suppressed": suppressed,
        });
        payload[event.key.payload_field()] = event.key.payload_value();

        self.record(
            event.household_id,
            &event.actor,
            AuditEventType::RateLimited,
            payload,
            now,
        )
        .await;
    }

    /// Ответ LLM не прошёл валидацию. Ни текста пользователя, ни ответа LLM.
    pub async fn llm_validation_failed(&self, event: LlmValidation, now: DateTime<Utc>) {
        let key = format!("household:{}", event.household_id);
        let Some(suppressed) = self.check(SuppressKind::LlmValidationFailed, &key, now) else {
            return;
        };

        let mut payload = json!({
            "source": event.source,
            "stage": event.stage,
            "code": event.code,
            "prompt_version": event.prompt_version,
            "suppressed": suppressed,
        });
        if let Some(capture_id) = event.capture_id {
            payload["capture_id"] = json!(capture_id);
        }
        if let Some(job_id) = event.job_id {
            payload["job_id"] = json!(job_id);
        }

        self.record(
            Some(event.household_id),
            &event.actor,
            AuditEventType::LlmValidationFailed,
            payload,
            now,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use chrono::{DateTime, Duration};

    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("timestamp")
    }

    #[test]
    fn one_event_per_key_is_written_within_the_suppression_window() {
        let mut suppression = Suppression::default();
        let start = at(10_000);
        let key = "ip:203.0.113.7";

        assert_eq!(
            suppression.allow(SuppressKind::AuthFailed, key, start),
            Some(0),
            "the first event in a window is written"
        );
        for offset in 1..=3 {
            assert_eq!(
                suppression.allow(
                    SuppressKind::AuthFailed,
                    key,
                    start + Duration::seconds(offset)
                ),
                None,
                "repeats inside the window are only counted"
            );
        }
        assert_eq!(
            suppression.allow(
                SuppressKind::AuthFailed,
                key,
                start + Duration::seconds(SUPPRESSION_WINDOW_SECONDS + 1)
            ),
            Some(3),
            "the next event after the window reports how many were suppressed"
        );
        assert_eq!(
            suppression.allow(
                SuppressKind::AuthFailed,
                key,
                start + Duration::seconds(SUPPRESSION_WINDOW_SECONDS + 2)
            ),
            None
        );
    }

    #[test]
    fn different_event_types_and_keys_are_counted_separately() {
        let mut suppression = Suppression::default();
        let now = at(20_000);

        assert_eq!(
            suppression.allow(SuppressKind::AuthFailed, "ip:1.1.1.1", now),
            Some(0)
        );
        assert_eq!(
            suppression.allow(SuppressKind::AuthFailed, "ip:2.2.2.2", now),
            Some(0)
        );
        assert_eq!(
            suppression.allow(SuppressKind::RateLimited, "ip:1.1.1.1", now),
            Some(0)
        );
    }

    #[test]
    fn stale_keys_are_removed_from_the_suppression_map() {
        let mut suppression = Suppression::default();
        let start = at(30_000);

        assert_eq!(
            suppression.allow(SuppressKind::AuthFailed, "ip:1.1.1.1", start),
            Some(0)
        );
        assert_eq!(
            suppression.allow(
                SuppressKind::AuthFailed,
                "ip:2.2.2.2",
                start + Duration::seconds(SUPPRESSION_KEY_TTL_SECONDS + 1)
            ),
            Some(0)
        );
        assert!(
            !suppression.tracks("ip:1.1.1.1"),
            "a key without recent events must not stay in the map"
        );
        assert!(suppression.tracks("ip:2.2.2.2"));
    }

    #[test]
    fn the_webhook_threshold_fires_once_per_exceeded_window() {
        let mut window = WebhookFailures::default();
        let start = at(40_000);

        for offset in 0..WEBHOOK_FAILURE_THRESHOLD {
            assert_eq!(
                window.record(start + Duration::seconds(i64::try_from(offset).expect("offset"))),
                None,
                "the threshold itself is not an excess"
            );
        }
        assert_eq!(
            window.record(start + Duration::seconds(60)),
            Some(WEBHOOK_FAILURE_THRESHOLD + 1)
        );
        assert_eq!(
            window.record(start + Duration::seconds(61)),
            None,
            "while the excess lasts the event is not repeated"
        );
    }

    #[test]
    fn the_webhook_threshold_can_fire_again_after_the_window_drains() {
        let mut window = WebhookFailures::default();
        let start = at(50_000);
        for offset in 0..=WEBHOOK_FAILURE_THRESHOLD {
            window.record(start + Duration::seconds(i64::try_from(offset).expect("offset")));
        }

        // Спустя окно от последнего отказа старые отметки уже не учитываются.
        let drained = start + Duration::seconds(WEBHOOK_FAILURE_WINDOW_SECONDS + 60);
        assert_eq!(
            window.record(drained),
            None,
            "a single event after the old window is below the threshold again"
        );
        let mut fired = None;
        for _ in 1..=WEBHOOK_FAILURE_THRESHOLD {
            fired = window.record(drained);
        }
        assert_eq!(
            fired,
            Some(WEBHOOK_FAILURE_THRESHOLD + 1),
            "a new burst is reported again"
        );
    }

    #[test]
    fn limit_keys_render_without_secrets() {
        let ip: IpAddr = "203.0.113.9".parse().expect("ip");
        let key = LimitKey::Ip(ip);
        assert_eq!(key.serialized(), "ip:203.0.113.9");
        assert_eq!(key.payload_field(), "ip");
    }
}
