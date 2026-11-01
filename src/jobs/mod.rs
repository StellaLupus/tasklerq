//! Фоновые воркеры: планировщик уведомлений и retention.

pub mod assignment;
pub(crate) mod batch;
pub(crate) mod delivery;
pub mod digest;
pub(crate) mod keys;
pub(crate) mod notice;
pub(crate) mod quiet;
pub mod retention;
pub mod scheduler;
