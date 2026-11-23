//! Фоновые воркеры: планировщик уведомлений, доменный тикер и retention.

pub mod assignment;
pub(crate) mod batch;
pub(crate) mod delivery;
pub mod digest;
pub mod domain;
pub(crate) mod keys;
pub(crate) mod notice;
pub(crate) mod notice_sync;
pub(crate) mod quiet;
pub mod retention;
pub mod scheduler;
