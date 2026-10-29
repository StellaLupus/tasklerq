//! Tasklerq — семейные задачи и покупки.

pub mod admin;
pub mod auth;
pub mod capture;
pub mod config;
pub mod domain;
pub mod home_assistant;
pub mod http;
pub mod jobs;
pub mod storage;
pub mod telegram;

/// Версия сервиса из `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
