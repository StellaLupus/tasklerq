//! Единый формат ошибок API.
//!
//! Ответ всегда имеет вид:
//! `{"error":{"code","message","details","request_id"}}`.

use axum::Json;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::domain::error::DomainError;

use super::middleware::{REQUEST_ID_HEADER, current_request_id};

/// Машиночитаемый код ошибки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    /// Некорректные входные данные.
    ValidationError,
    /// Нет или недействителен токен.
    Unauthorized,
    /// Недостаточно прав.
    Forbidden,
    /// Ресурс не найден (в том числе чужой household).
    NotFound,
    /// Конфликт версий или состояния.
    Conflict,
    /// Ресурс больше не доступен: например, срок жизни черновика истёк.
    Gone,
    /// Превышен лимит частоты.
    RateLimited,
    /// Внутренняя ошибка.
    InternalError,
    /// Временная недоступность зависимости.
    ServiceUnavailable,
    /// Тело запроса слишком велико.
    PayloadTooLarge,
    /// Неподдерживаемый тип тела запроса.
    UnsupportedMediaType,
    /// Метод не поддерживается маршрутом.
    MethodNotAllowed,
}

impl ErrorCode {
    /// Строковое значение для JSON.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ValidationError => "validation_error",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::Gone => "gone",
            Self::RateLimited => "rate_limited",
            Self::InternalError => "internal_error",
            Self::ServiceUnavailable => "service_unavailable",
            Self::PayloadTooLarge => "payload_too_large",
            Self::UnsupportedMediaType => "unsupported_media_type",
            Self::MethodNotAllowed => "method_not_allowed",
        }
    }
}

/// Ошибка API, сериализуемая в единый конверт.
#[derive(Clone, Debug)]
pub struct ApiError {
    status: StatusCode,
    code: ErrorCode,
    message: String,
    details: Value,
    retry_after: Option<u64>,
}

impl ApiError {
    /// Создать ошибку с произвольным статусом и кодом.
    pub fn new(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: json!({}),
            retry_after: None,
        }
    }

    /// Добавить заголовок `Retry-After` (секунды).
    pub fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds);
        self
    }

    /// Добавить машиночитаемые детали.
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    /// `422 validation_error`.
    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::ValidationError,
            message,
        )
    }

    /// `401 unauthorized`.
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, ErrorCode::Unauthorized, message)
    }

    /// `403 forbidden`.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, ErrorCode::Forbidden, message)
    }

    /// `404 not_found`.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, message)
    }

    /// `409 conflict`.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, ErrorCode::Conflict, message)
    }

    /// `410 gone`.
    pub fn gone(message: impl Into<String>) -> Self {
        Self::new(StatusCode::GONE, ErrorCode::Gone, message)
    }

    /// `429 rate_limited`.
    pub fn rate_limited(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::RateLimited,
            message,
        )
    }

    /// `500 internal_error`.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::InternalError,
            message,
        )
    }

    /// `503 service_unavailable`.
    pub fn service_unavailable(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::ServiceUnavailable,
            message,
        )
    }

    /// `413 payload_too_large`.
    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            ErrorCode::PayloadTooLarge,
            message,
        )
    }

    /// `415 unsupported_media_type`.
    pub fn unsupported_media_type(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ErrorCode::UnsupportedMediaType,
            message,
        )
    }

    /// `405 method_not_allowed`.
    pub fn method_not_allowed(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            ErrorCode::MethodNotAllowed,
            message,
        )
    }

    /// HTTP-статус ошибки.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// Машиночитаемый код.
    pub fn code(&self) -> ErrorCode {
        self.code
    }
}

impl From<DomainError> for ApiError {
    fn from(error: DomainError) -> Self {
        match error {
            DomainError::Validation(message) => Self::validation(message),
            DomainError::NotFound => Self::not_found("Resource not found"),
            DomainError::Forbidden(message) => Self::forbidden(message),
            DomainError::Conflict { message, details } => {
                Self::conflict(message).with_details(details)
            }
            DomainError::Database(error) => {
                tracing::error!(error = %error, "database operation failed");
                Self::internal("Database error")
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let request_id = current_request_id();

        if self.status.is_server_error() {
            tracing::error!(
                error_code = self.code.as_str(),
                status = self.status.as_u16(),
                request_id = request_id.as_deref().unwrap_or(""),
                "request failed"
            );
        }

        let body = json!({
            "error": {
                "code": self.code.as_str(),
                "message": self.message,
                "details": self.details,
                "request_id": request_id,
            }
        });

        let mut response = (self.status, Json(body)).into_response();
        if let Some(seconds) = self.retry_after
            && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
        {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
        }
        if let Some(id) = request_id
            && let Ok(value) = HeaderValue::from_str(&id)
        {
            response
                .headers_mut()
                .insert(HeaderName::from_static(REQUEST_ID_HEADER), value);
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_match_documented_wire_values() {
        assert_eq!(ErrorCode::ValidationError.as_str(), "validation_error");
        assert_eq!(ErrorCode::Unauthorized.as_str(), "unauthorized");
        assert_eq!(ErrorCode::Forbidden.as_str(), "forbidden");
        assert_eq!(ErrorCode::NotFound.as_str(), "not_found");
        assert_eq!(ErrorCode::Conflict.as_str(), "conflict");
        assert_eq!(ErrorCode::Gone.as_str(), "gone");
        assert_eq!(ErrorCode::RateLimited.as_str(), "rate_limited");
        assert_eq!(ErrorCode::InternalError.as_str(), "internal_error");
        assert_eq!(
            ErrorCode::ServiceUnavailable.as_str(),
            "service_unavailable"
        );
        assert_eq!(ErrorCode::PayloadTooLarge.as_str(), "payload_too_large");
        assert_eq!(
            ErrorCode::UnsupportedMediaType.as_str(),
            "unsupported_media_type"
        );
        assert_eq!(ErrorCode::MethodNotAllowed.as_str(), "method_not_allowed");
    }

    #[test]
    fn constructors_set_expected_statuses() {
        assert_eq!(ApiError::validation("x").status().as_u16(), 422);
        assert_eq!(ApiError::unauthorized("x").status().as_u16(), 401);
        assert_eq!(ApiError::forbidden("x").status().as_u16(), 403);
        assert_eq!(ApiError::not_found("x").status().as_u16(), 404);
        assert_eq!(ApiError::conflict("x").status().as_u16(), 409);
        assert_eq!(ApiError::gone("x").status().as_u16(), 410);
        assert_eq!(ApiError::rate_limited("x").status().as_u16(), 429);
        assert_eq!(ApiError::internal("x").status().as_u16(), 500);
        assert_eq!(ApiError::service_unavailable("x").status().as_u16(), 503);
        assert_eq!(ApiError::payload_too_large("x").status().as_u16(), 413);
        assert_eq!(ApiError::unsupported_media_type("x").status().as_u16(), 415);
        assert_eq!(ApiError::method_not_allowed("x").status().as_u16(), 405);
    }
}
