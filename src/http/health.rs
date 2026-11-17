//! Проверки живости и готовности.

use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sqlx::PgPool;

use crate::VERSION;
use crate::capture::slot_status::LlmSlotHealth;
use crate::config::TenancyMode;
use crate::settings::crypto::SecretCipher;
use crate::storage::repositories::households;
use crate::telegram::queue;

use super::error::ApiError;
use super::router::AppState;

/// Код предупреждения: в `single` активных household больше одного.
pub const WARNING_MULTIPLE_HOUSEHOLDS: &str = "multiple_households";
/// Код предупреждения: предохранители всех настроенных слотов LLM разомкнуты.
pub const WARNING_LLM_UNAVAILABLE: &str = "llm_unavailable";
/// Префикс кода предупреждения о непроверенном слоте LLM; за ним идёт `:slot`
/// и, для сохранённой проверки, `:режим`.
pub const WARNING_LLM_SLOT_UNVERIFIED: &str = "llm_slot_unverified";

/// Коды предупреждений, которые может вернуть [`readiness`] без параметров.
///
/// Параметрический `llm_slot_unverified:<slot>[:<режим>]` проверяется тестом
/// консоли отдельно, от [`WARNING_LLM_SLOT_UNVERIFIED`]: у каждого кода должна
/// быть понятная подпись, поэтому новый код без подписи не проходит проверку.
pub const WARNING_CODES: &[&str] = &[
    WARNING_MULTIPLE_HOUSEHOLDS,
    WARNING_LLM_UNAVAILABLE,
    crate::settings::SecretProblem::KeyMissing.code(),
    crate::settings::SecretProblem::Undecryptable.code(),
];

/// Ответ health-эндпоинта.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    /// `ok`, если проверка пройдена.
    pub status: &'static str,
    /// Версия сервиса.
    pub version: &'static str,
    /// Время проверки в UTC.
    pub time: DateTime<Utc>,
    /// Необязательные предупреждения о деградации; поле отсутствует, когда их
    /// нет. Имена предупреждений фиксированы: `multiple_households`,
    /// `llm_unavailable`, `llm_slot_unverified:<slot>` и
    /// `llm_slot_unverified:<slot>:<режим>` для сохранённой проверки,
    /// `secret_key_missing` и `secret_undecryptable` для сохранённых секретов.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Vec<String>>,
}

/// `GET /health/live`: процесс отвечает.
pub async fn live() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: VERSION,
        time: Utc::now(),
        warnings: None,
    })
}

/// Входные данные проверки готовности.
///
/// Значения передаются явно, чтобы `/health/ready` и главная консоли считали
/// одно и то же: обработчик берёт их из [`AppState`], консоль — из своего
/// состояния с тем же пулом, снимком и ключами.
pub struct ReadinessInput<'a> {
    /// Пул соединений.
    pub pool: &'a PgPool,
    /// Режим размещения.
    pub tenancy_mode: TenancyMode,
    /// Допустимый возраст необработанного обновления Telegram; `None` — очередь
    /// не проверяется.
    pub queue_lag_limit: Option<Duration>,
    /// Есть живое поколение Telegram: без него очередь никто не разбирает и
    /// порог не применяется.
    pub telegram_active: bool,
    /// Ключи для проверки сохранённых секретов.
    pub secret_cipher: &'a SecretCipher,
    /// Статус проверки слотов LLM.
    pub llm_slots: &'a LlmSlotHealth,
    /// Момент проверки.
    pub now: DateTime<Utc>,
}

/// Причина, по которой сервис не готов.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadinessFailure {
    /// База недоступна.
    Database,
    /// Миграции не применены.
    Migrations,
    /// Очередь обновлений Telegram отстаёт.
    QueueLag,
}

impl ReadinessFailure {
    /// Текст ответа `/health/ready`; он не меняется при выносе общей сборки.
    pub const fn message(self) -> &'static str {
        match self {
            Self::Database => "Database is unavailable",
            Self::Migrations => "Database migrations are not applied",
            Self::QueueLag => "Telegram update queue is lagging",
        }
    }

    /// Понятная подпись для главной консоли без технических подробностей.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Database => "база данных недоступна",
            Self::Migrations => "миграции не применены",
            Self::QueueLag => "очередь обновлений Telegram отстаёт",
        }
    }
}

/// `GET /health/ready`: база доступна и принимает запросы, миграции применены,
/// а очередь обновлений Telegram не застряла (пока живо поколение бота).
pub async fn ready(State(state): State<AppState>) -> Result<Json<HealthResponse>, ApiError> {
    let now = Utc::now();
    let warnings = readiness(ReadinessInput {
        pool: &state.pool,
        tenancy_mode: state.tenancy_mode,
        queue_lag_limit: state.queue_lag_limit,
        telegram_active: state.telegram.is_active(),
        secret_cipher: &state.secret_cipher,
        llm_slots: &state.llm_slots,
        now,
    })
    .await
    .map_err(|failure| ApiError::service_unavailable(failure.message()))?;

    Ok(Json(HealthResponse {
        status: "ok",
        version: VERSION,
        time: now,
        warnings: (!warnings.is_empty()).then_some(warnings),
    }))
}

/// Собрать предупреждения readiness или причину отказа.
///
/// Одна сборка для `/health/ready` и главной консоли: проверки базы, миграций,
/// секретов, очереди, household и слотов LLM не дублируются. Ошибки пишутся в
/// журнал без адресов и токенов.
pub async fn readiness(input: ReadinessInput<'_>) -> Result<Vec<String>, ReadinessFailure> {
    let ReadinessInput {
        pool,
        tenancy_mode,
        queue_lag_limit,
        telegram_active,
        secret_cipher,
        llm_slots,
        now,
    } = input;

    sqlx::query("SELECT 1")
        .execute(pool)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "readiness check failed");
            ReadinessFailure::Database
        })?;

    let migrations = crate::storage::migrations::all_applied(pool)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "readiness migration check failed");
            ReadinessFailure::Migrations
        })?;
    if !migrations {
        tracing::warn!("not all embedded migrations are applied");
        return Err(ReadinessFailure::Migrations);
    }

    let mut warnings: Vec<String> = Vec::new();

    // Секреты читаются при каждом запросе: настройка может появиться из консоли
    // без перезапуска. Ошибка базы здесь — та же недоступность, что и выше.
    let secrets = crate::settings::read_secret_health(pool, secret_cipher)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "readiness secret check failed");
            ReadinessFailure::Database
        })?;
    if let Some(problem) = secrets.problem() {
        warnings.push(problem.code().to_owned());
    }

    // Очередь и счёт household делят одно соединение. Счёт идёт после проверки
    // очереди: при 503 из-за лага он уже не нужен.
    if queue_lag_limit.is_some() || tenancy_mode == TenancyMode::Single {
        let mut conn = pool.acquire().await.map_err(|error| {
            tracing::warn!(error = %error, "readiness check failed");
            ReadinessFailure::Database
        })?;

        if let Some(limit) = queue_lag_limit.filter(|_| telegram_active) {
            // Упавший или остановленный воркер заметен по самому старому
            // обновлению, которое давно могло быть обработано. Без живого
            // поколения очередь никто не разбирает, поэтому порог не
            // применяется: выключение бота не делает сервис неготовым.
            let oldest = queue::oldest_ready(&mut conn, now).await.map_err(|error| {
                tracing::warn!(error = %error, "readiness check failed");
                ReadinessFailure::Database
            })?;
            if oldest.is_some_and(|oldest| now - oldest > limit) {
                tracing::warn!("telegram update queue is lagging");
                return Err(ReadinessFailure::QueueLag);
            }
        }

        if tenancy_mode == TenancyMode::Single {
            // Счёт читается при каждом запросе: после `household delete`
            // предупреждение исчезает без перезапуска.
            let active = households::count_active(&mut conn).await.map_err(|error| {
                tracing::warn!(error = %error, "readiness household count failed");
                ReadinessFailure::Database
            })?;
            if active > 1 {
                warnings.push(WARNING_MULTIPLE_HOUSEHOLDS.to_owned());
            }
        }
    }

    // Статус слотов LLM — предупреждение, а не отказ: без LLM сервис работает с
    // деградацией, и падение readiness вывело бы из работы то, что от него не
    // зависит.
    if llm_slots.all_breakers_open(now) {
        warnings.push(WARNING_LLM_UNAVAILABLE.to_owned());
    }
    for report in llm_slots.statuses(pool, now).await {
        if !report.status.is_passed() {
            match report.titles {
                Some(titles) => warnings.push(format!(
                    "{WARNING_LLM_SLOT_UNVERIFIED}:{}:{}",
                    report.slot,
                    titles.as_str()
                )),
                None => warnings.push(format!("{WARNING_LLM_SLOT_UNVERIFIED}:{}", report.slot)),
            }
        }
    }

    Ok(warnings)
}
