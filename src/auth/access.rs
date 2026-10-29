//! Проверки доступа доменных операций.

use crate::domain::error::DomainError;
use crate::domain::models::Actor;

/// Требовать роль владельца household.
pub fn require_owner(actor: &Actor) -> Result<(), DomainError> {
    if actor.is_owner() {
        Ok(())
    } else {
        Err(DomainError::Forbidden(
            "only the household owner can perform this action".into(),
        ))
    }
}
