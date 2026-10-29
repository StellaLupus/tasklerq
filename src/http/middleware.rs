//! Middleware HTTP: идентификатор запроса и журналирование.

use std::time::Instant;

use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

/// Заголовок с идентификатором запроса.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

tokio::task_local! {
    static REQUEST_ID: String;
}

/// Вернуть идентификатор текущего запроса, если middleware уже выполнился.
pub fn current_request_id() -> Option<String> {
    REQUEST_ID.try_with(Clone::clone).ok()
}

/// Middleware: принимает `X-Request-Id` от клиента или генерирует UUIDv7,
/// добавляет заголовок в ответ и пишет строку журнала без пользовательских данных.
pub async fn request_id(request: Request, next: Next) -> Response {
    let id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| is_valid_request_id(value))
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| Uuid::now_v7().to_string());

    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let started = Instant::now();

    let mut response = REQUEST_ID.scope(id.clone(), next.run(request)).await;

    if let Ok(value) = HeaderValue::from_str(&id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(REQUEST_ID_HEADER), value);
    }

    let status = response.status().as_u16();
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::info!(
        request_id = %id,
        %method,
        path = %path,
        status,
        duration_ms,
        "request completed"
    );

    response
}

/// Клиентский идентификатор принимается, только если он короткий и состоит из
/// безопасных символов; иначе генерируется новый.
fn is_valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

#[cfg(test)]
mod tests {
    use super::is_valid_request_id;

    #[test]
    fn accepts_reasonable_request_ids() {
        assert!(is_valid_request_id("01J2Z3X4Y5"));
        assert!(is_valid_request_id("req-123_abc.def:1"));
    }

    #[test]
    fn rejects_empty_long_and_unsafe_ids() {
        assert!(!is_valid_request_id(""));
        assert!(!is_valid_request_id("id with spaces"));
        assert!(!is_valid_request_id("id\ninjection"));
        assert!(!is_valid_request_id(&"a".repeat(129)));
    }
}
