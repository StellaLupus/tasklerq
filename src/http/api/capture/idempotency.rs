//! Ключ идемпотентности внешнего capture: разбор, хеш тела и резерв.

use axum::http::HeaderValue;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::models::{CaptureMode, CaptureSource};
use crate::http::error::ApiError;
use crate::storage::repositories::capture_requests;

use super::{RequestHint, source_name};

/// Заголовок с ключом повтора.
const IDEMPOTENCY_HEADER: &str = "idempotency-key";
/// Предельная длина ключа повтора.
const IDEMPOTENCY_KEY_MAX: usize = 128;

/// Заголовок с ключом повтора этого запроса.
///
/// Несколько заголовков неоднозначны, поэтому отклоняются до разбора ключа.
pub(super) fn header_key(
    headers: &axum::http::HeaderMap,
) -> Result<Option<&HeaderValue>, ApiError> {
    let mut values = headers.get_all(IDEMPOTENCY_HEADER).iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(ApiError::validation(
            "Idempotency-Key must be provided only once",
        ));
    }
    Ok(first)
}

/// Ключ повтора: заголовок сильнее поля, но различающиеся значения — ошибка.
pub(super) fn resolve_idempotency_key(
    header: Option<&HeaderValue>,
    field: Option<&str>,
) -> Result<Option<String>, ApiError> {
    let header = match header {
        Some(value) => match value.to_str() {
            Ok(value) => validate_idempotency_key(value)?,
            Err(_) => return Err(invalid_idempotency_key()),
        },
        None => None,
    };
    let field = field.map(validate_idempotency_key).transpose()?.flatten();
    match (header, field) {
        (Some(header), Some(field)) if header != field => Err(ApiError::validation(
            "Idempotency-Key header and idempotency_key field must match",
        )),
        (Some(key), _) | (None, Some(key)) => Ok(Some(key)),
        (None, None) => Ok(None),
    }
}

/// Пустой ключ означает «без ключа»; иначе допустимы 1..128 видимых ASCII.
fn validate_idempotency_key(raw: &str) -> Result<Option<String>, ApiError> {
    if raw.is_empty() {
        return Ok(None);
    }
    if raw.len() > IDEMPOTENCY_KEY_MAX || !raw.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(invalid_idempotency_key());
    }
    Ok(Some(raw.to_owned()))
}

fn invalid_idempotency_key() -> ApiError {
    ApiError::validation("Idempotency-Key must be 1..128 visible ASCII characters without spaces")
}

/// sha256 канонического представления значимых полей. Ключ сюда не входит.
pub(super) fn request_hash(
    text: &str,
    source: CaptureSource,
    mode: CaptureMode,
    hint: Option<RequestHint>,
    list_id: Option<Uuid>,
) -> String {
    let canonical = json!({
        "text": text,
        "source": source_name(source),
        "mode": mode_name(mode),
        "hint": hint.map(hint_name),
        "list_id": list_id,
    });
    sha256_hex(canonical.to_string().as_bytes())
}

fn sha256_hex(value: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    let digest = Sha256::digest(value);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn mode_name(mode: CaptureMode) -> &'static str {
    match mode {
        CaptureMode::Auto => "auto",
        CaptureMode::Draft => "draft",
    }
}

fn hint_name(hint: RequestHint) -> &'static str {
    match hint {
        RequestHint::Tasks => "tasks",
        RequestHint::Shopping => "shopping",
        RequestHint::Checklist => "checklist",
    }
}

/// Освобождает незавершённый резерв, если future запроса отброшена.
///
/// Обычная ветка ошибки освобождает ключ синхронно; `Drop` нужен только для
/// отмены запроса (например, клиент разорвал соединение), когда `capture_inner`
/// не успевает вернуть результат.
pub(super) struct ReservationGuard {
    pool: PgPool,
    reservation: Option<capture_requests::Reservation>,
}

impl ReservationGuard {
    pub(super) fn new(pool: PgPool) -> Self {
        Self {
            pool,
            reservation: None,
        }
    }

    pub(super) fn slot(&mut self) -> &mut Option<capture_requests::Reservation> {
        &mut self.reservation
    }

    pub(super) async fn release(&mut self) {
        let Some(reservation) = self.reservation.take() else {
            return;
        };
        release_reservation(self.pool.clone(), reservation).await;
    }
}

impl Drop for ReservationGuard {
    fn drop(&mut self) {
        let Some(reservation) = self.reservation.take() else {
            return;
        };
        if reservation.is_settled() {
            return;
        }
        let pool = self.pool.clone();
        tokio::spawn(release_reservation(pool, reservation));
    }
}

/// Взять соединение из пула и освободить резерв; используется и явным
/// `release`, и `Drop`.
async fn release_reservation(pool: PgPool, mut reservation: capture_requests::Reservation) {
    match pool.acquire().await {
        Ok(mut conn) => reservation.release(&mut conn).await,
        Err(error) => {
            tracing::error!(?error, "could not acquire a connection to release the key");
        }
    }
}
