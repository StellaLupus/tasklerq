//! LLM-capture: оркестрация разбора, черновики, валидация и подтверждение.

pub mod context;
pub mod draft;
pub mod prompt;
pub mod schema;
pub mod validate;

#[cfg(test)]
mod test_support;
