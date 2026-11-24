//! Фоновые воркеры: планировщик уведомлений, доменный тикер и retention.

pub mod assignment;
pub(crate) mod batch;
pub(crate) mod delivery;
pub mod digest;
pub(crate) mod digest_sync;
pub mod domain;
pub(crate) mod keys;
pub(crate) mod notice;
pub(crate) mod notice_sync;
pub(crate) mod quiet;
pub mod retention;
pub mod scheduler;

/// Ошибка БД доменного слоя как ошибка sqlx; прочие варианты не возникают у
/// запросов выборки выполнений и участников. Общая для шагов синхронизации
/// уведомлений ([`notice_sync`]) и сводки ([`digest`]).
pub(crate) fn domain_db(error: crate::domain::error::DomainError) -> sqlx::Error {
    match error {
        crate::domain::error::DomainError::Database(error) => error,
        other => sqlx::Error::Protocol(other.to_string()),
    }
}
