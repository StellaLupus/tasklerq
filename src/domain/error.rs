//! Ошибки доменного слоя.

use serde_json::Value;
use thiserror::Error;

/// Ошибка доменной операции.
#[derive(Debug, Error)]
pub enum DomainError {
    /// Некорректные входные данные.
    #[error("{0}")]
    Validation(String),
    /// Ресурс не найден или принадлежит другому household.
    #[error("resource not found")]
    NotFound,
    /// Действие запрещено.
    #[error("{0}")]
    Forbidden(String),
    /// Конфликт состояния или версии.
    #[error("{message}")]
    Conflict {
        /// Сообщение.
        message: String,
        /// Машиночитаемые детали.
        details: Value,
    },
    /// Ошибка базы данных.
    #[error("database error")]
    Database(#[from] sqlx::Error),
}

impl DomainError {
    /// Конфликт без деталей.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict {
            message: message.into(),
            details: Value::Object(Default::default()),
        }
    }

    /// Конфликт с деталями.
    pub fn conflict_with(message: impl Into<String>, details: Value) -> Self {
        Self::Conflict {
            message: message.into(),
            details,
        }
    }
}
