//! Одноразовые коды консоли: выпуск, гашение и использование.
//!
//! Политика назначений живёт здесь, хранение и SQL — в
//! [`crate::storage::repositories::admin_one_time_codes`]. `setup` выпускается
//! без срока и гасится следующим выпуском, `recovery` живёт 15 минут.
//!
//! Проверка кода идёт по SHA-256 хешу в базе, поэтому открытый текст не
//! сравнивается в памяти (подробности — в doc-комментарии репозитория).

use chrono::{DateTime, Duration, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::config::Config;
use crate::domain::error::DomainError;
use crate::storage::repositories::admin_one_time_codes::{
    self as repository, IssuedCode, OneTimeCode,
};

/// Срок жизни `recovery`-кода, минуты.
pub const RECOVERY_TTL_MINUTES: i64 = 15;

/// Назначение одноразового кода.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Первый вход: пока учётной записи администратора нет.
    Setup,
    /// Восстановление доступа.
    Recovery,
}

impl Purpose {
    /// Значение колонки `purpose`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Setup => repository::PURPOSE_SETUP,
            Self::Recovery => repository::PURPOSE_RECOVERY,
        }
    }

    /// Срок жизни; `None` — код без срока, пока его не погасит новый выпуск.
    fn ttl(self) -> Option<Duration> {
        match self {
            Self::Setup => None,
            Self::Recovery => Some(Duration::minutes(RECOVERY_TTL_MINUTES)),
        }
    }
}

/// Выпустить код в собственной транзакции.
///
/// Прежние неиспользованные коды той же пары `(purpose, household_id)`
/// гасятся в той же транзакции, поэтому после выпуска действует только новый
/// код. Открытое значение возвращается один раз.
pub async fn issue(
    pool: &PgPool,
    purpose: Purpose,
    household_id: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<IssuedCode, DomainError> {
    let expires_at = purpose.ttl().and_then(|ttl| now.checked_add_signed(ttl));

    let mut tx = pool.begin().await?;
    let issued =
        repository::issue(&mut tx, purpose.as_str(), household_id, expires_at, now).await?;
    tx.commit().await?;

    Ok(issued)
}

/// Погасить код атомарным `UPDATE … RETURNING`.
///
/// Работает в транзакции вызывающего: страница setup или восстановления
/// создаёт учётную запись либо меняет пароль и гасит код одной транзакцией.
/// Повторный, истёкший, погашенный и код другого назначения дают
/// [`DomainError::NotFound`] без различения причины.
pub async fn use_code(
    conn: &mut PgConnection,
    purpose: Purpose,
    code: &str,
    now: DateTime<Utc>,
) -> Result<OneTimeCode, DomainError> {
    repository::use_code(conn, purpose.as_str(), code, now).await
}

/// Выпустить setup-код, если учётной записи администратора ещё нет.
///
/// `Ok(None)` — учётка уже создана, код не выпускается. Проверка и выпуск идут
/// в одной транзакции.
pub async fn issue_setup_code(
    pool: &PgPool,
    now: DateTime<Utc>,
) -> Result<Option<IssuedCode>, DomainError> {
    let mut tx = pool.begin().await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM admin_accounts)")
        .fetch_one(&mut *tx)
        .await?;
    if exists {
        return Ok(None);
    }

    let issued = repository::issue(&mut tx, Purpose::Setup.as_str(), None, None, now).await?;
    tx.commit().await?;

    Ok(Some(issued))
}

/// Объявить setup-код при старте: одна строка `warn` с кодом и портом.
///
/// Код выпускается, только когда консоль включена (`ADMIN_BIND_ADDR` не
/// `disabled`), listener привязан и учётной записи администратора ещё нет.
/// Вызывать после привязки listener'а, чтобы в лог попал фактический порт:
/// внешний адрес сервер не знает, поэтому строка подсказывает открыть
/// `http://<адрес хоста>:<порт>/admin/setup`.
///
/// Сбой базы не роняет сервис: в лог уходит предупреждение без кода.
pub async fn announce_setup_code(
    pool: &PgPool,
    config: &Config,
    admin_port: Option<u16>,
    now: DateTime<Utc>,
) {
    if config.admin_bind_addr.is_none() {
        return;
    }
    let Some(port) = admin_port else {
        return;
    };

    match issue_setup_code(pool, now).await {
        Ok(Some(issued)) => tracing::warn!(
            port,
            code = %issued.code,
            "admin console setup code issued: open http://<host>:{}/admin/setup to create the administrator account",
            port
        ),
        Ok(None) => {}
        Err(error) => tracing::warn!(
            error = %error,
            "failed to issue the admin console setup code; no code is available for the first sign-in"
        ),
    }
}
