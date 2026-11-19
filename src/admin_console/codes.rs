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

/// Срок жизни ссылки-claim владельца, часы.
pub const OWNER_CLAIM_TTL_HOURS: i64 = 24;

/// Срок жизни ссылки привязки Telegram, минуты.
pub const TELEGRAM_LINK_TTL_MINUTES: i64 = 15;

/// Назначение одноразового кода.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Первый вход: пока учётной записи администратора нет.
    Setup,
    /// Восстановление доступа.
    Recovery,
    /// Ссылка-claim владельца.
    OwnerClaim,
    /// Привязка Telegram к учётной записи администратора.
    TelegramLink,
}

impl Purpose {
    /// Значение колонки `purpose`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Setup => repository::PURPOSE_SETUP,
            Self::Recovery => repository::PURPOSE_RECOVERY,
            Self::OwnerClaim => repository::PURPOSE_OWNER_CLAIM,
            Self::TelegramLink => repository::PURPOSE_TELEGRAM_LINK,
        }
    }

    /// Срок жизни; `None` — код без срока, пока его не погасит новый выпуск.
    fn ttl(self) -> Option<Duration> {
        match self {
            Self::Setup => None,
            Self::Recovery => Some(Duration::minutes(RECOVERY_TTL_MINUTES)),
            Self::OwnerClaim => Some(Duration::hours(OWNER_CLAIM_TTL_HOURS)),
            Self::TelegramLink => Some(Duration::minutes(TELEGRAM_LINK_TTL_MINUTES)),
        }
    }
}

/// Выпустить код в собственной транзакции.
///
/// Прежние неиспользованные коды той же пары `(purpose, household_id)`
/// гасятся в той же транзакции, поэтому после выпуска действует только новый
/// код. Открытое значение возвращается один раз.
///
/// Транзакция берёт `pg_advisory_xact_lock` от назначения (и household):
/// два параллельных выпуска одной пары сериализуются, и «погасить прежние,
/// вставить новый» не оставляет двух действующих кодов.
pub async fn issue(
    pool: &PgPool,
    purpose: Purpose,
    household_id: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<IssuedCode, DomainError> {
    let mut tx = pool.begin().await?;
    let issued = issue_locked(&mut tx, purpose, household_id, now).await?;
    tx.commit().await?;

    Ok(issued)
}

/// Выпустить код в транзакции вызывающего.
///
/// Нужен путям, которые проверяют цель и выпускают код одной транзакцией:
/// ссылка-claim держит блокировку household, пока не увидит отсутствие
/// владельца и не вставит новый код. Политика срока остаётся здесь же, поэтому
/// [`issue`] и [`issue_locked`] не расходятся.
pub async fn issue_locked(
    conn: &mut PgConnection,
    purpose: Purpose,
    household_id: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<IssuedCode, DomainError> {
    let expires_at = purpose.ttl().and_then(|ttl| now.checked_add_signed(ttl));
    lock_purpose(conn, purpose, household_id).await?;
    repository::issue(conn, purpose.as_str(), household_id, expires_at, now).await
}

/// Взять блокировку выпуска кодов назначения в транзакции вызывающего.
///
/// Блокировка транзакционная: снимается коммитом или откатом. Создание учётки
/// по setup-коду берёт ту же блокировку, поэтому выпуск setup-кода не может
/// пройти между проверкой «учётки нет» и коммитом создания.
pub async fn lock_purpose(
    conn: &mut PgConnection,
    purpose: Purpose,
    household_id: Option<Uuid>,
) -> Result<(), DomainError> {
    repository::lock_purpose(conn, purpose.as_str(), household_id).await
}

/// Погасить код атомарным `UPDATE … RETURNING`.
///
/// Работает в транзакции вызывающего: страница setup или восстановления
/// создаёт учётную запись либо меняет пароль и гасит код одной транзакцией.
/// Повторный, истёкший, погашенный и код другого назначения дают
/// [`DomainError::NotFound`] без различения причины.
///
/// Пробелы по краям ввода отбрасываются: код копируют из строки журнала, где
/// он окружён текстом, и лишний пробел не должен превращать верный код в
/// неверный. [`issue`] пробелов не добавляет, поэтому сравнение идёт по
/// значению без изменений.
pub async fn use_code(
    conn: &mut PgConnection,
    purpose: Purpose,
    code: &str,
    now: DateTime<Utc>,
) -> Result<OneTimeCode, DomainError> {
    repository::use_code(conn, purpose.as_str(), code, now).await
}

/// Найти действующий код по открытому значению, не гася его.
pub async fn find_active(
    conn: &mut PgConnection,
    purpose: Purpose,
    code: &str,
    now: DateTime<Utc>,
) -> Result<Option<OneTimeCode>, DomainError> {
    repository::find_active(conn, purpose.as_str(), code, now).await
}

/// Найти действующий код по идентификатору, не гася его.
///
/// Онбординг по ссылке-claim хранит в состоянии диалога только id кода:
/// открытое значение в базе не сохраняется. Проверка нужна перед показом
/// подтверждения и повторно перед созданием household.
pub async fn find_active_by_id(
    conn: &mut PgConnection,
    purpose: Purpose,
    id: Uuid,
    now: DateTime<Utc>,
) -> Result<Option<OneTimeCode>, DomainError> {
    repository::find_active_by_id(conn, purpose.as_str(), id, now).await
}

/// Погасить код по идентификатору атомарным `UPDATE … RETURNING`.
///
/// Работает в транзакции вызывающего: создание household по ссылке-claim
/// гасит код и вставляет household одной транзакцией. Недействительный код
/// даёт [`DomainError::NotFound`].
pub async fn use_code_by_id(
    conn: &mut PgConnection,
    purpose: Purpose,
    id: Uuid,
    now: DateTime<Utc>,
) -> Result<OneTimeCode, DomainError> {
    repository::use_code_by_id(conn, purpose.as_str(), id, now).await
}

/// Выпустить setup-код, если учётной записи администратора ещё нет.
///
/// `Ok(None)` — учётка уже создана, код не выпускается. Проверка и выпуск идут
/// в одной транзакции под блокировкой назначения `setup`: создание учётки по
/// setup-коду берёт ту же блокировку, поэтому код не выпустится параллельно с
/// ним.
pub async fn issue_setup_code(
    pool: &PgPool,
    now: DateTime<Utc>,
) -> Result<Option<IssuedCode>, DomainError> {
    let mut tx = pool.begin().await?;
    repository::lock_purpose(&mut tx, Purpose::Setup.as_str(), None).await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM admin_accounts)")
        .fetch_one(&mut *tx)
        .await?;
    if exists {
        tx.rollback().await?;
        return Ok(None);
    }

    let issued = repository::issue(&mut tx, Purpose::Setup.as_str(), None, None, now).await?;
    tx.commit().await?;

    Ok(Some(issued))
}

/// Выпустить `recovery`-код, если учётная запись администратора существует.
///
/// `Ok(None)` — учётки нет: выпускать ссылку некуда, CLI подскажет
/// `tasklerq admin setup-code`. Проверка и выпуск идут в одной транзакции.
pub async fn issue_recovery_code(
    pool: &PgPool,
    now: DateTime<Utc>,
) -> Result<Option<IssuedCode>, DomainError> {
    let mut tx = pool.begin().await?;
    repository::lock_purpose(&mut tx, Purpose::Recovery.as_str(), None).await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM admin_accounts)")
        .fetch_one(&mut *tx)
        .await?;
    if !exists {
        tx.rollback().await?;
        return Ok(None);
    }

    let expires_at = now.checked_add_signed(Duration::minutes(RECOVERY_TTL_MINUTES));
    let issued =
        repository::issue(&mut tx, Purpose::Recovery.as_str(), None, expires_at, now).await?;
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
