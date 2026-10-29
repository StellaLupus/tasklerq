//! Проверки прав на уровне сервиса: роли owner/member и системные акторы.
//!
//! Проверки выполняются на сервере, а не скрытием кнопок. Системный актор
//! (токен Home Assistant, голос, автоматизация) не имеет роли и не может
//! управлять household, даже если токену выдан любой scope.

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{Actor, ActorType, UserRole};

/// Требуется владелец-пользователь. Возвращает его идентификатор.
pub fn require_owner(actor: &Actor) -> Result<Uuid, DomainError> {
    match (actor.actor_type, actor.user_id, actor.role) {
        (ActorType::User, Some(user_id), Some(UserRole::Owner)) => Ok(user_id),
        (ActorType::User, Some(_), _) => Err(DomainError::Forbidden(
            "only the household owner can perform this action".into(),
        )),
        _ => Err(DomainError::Forbidden(
            "system actors cannot manage the household".into(),
        )),
    }
}

/// Требуется участник-пользователь (любая роль). Возвращает его идентификатор.
pub fn require_user(actor: &Actor) -> Result<Uuid, DomainError> {
    match (actor.actor_type, actor.user_id) {
        (ActorType::User, Some(user_id)) if actor.role.is_some() => Ok(user_id),
        _ => Err(DomainError::Forbidden(
            "system actors cannot manage the household".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_passes_member_and_system_do_not() {
        let owner = Actor::user(Uuid::now_v7(), UserRole::Owner);
        let member = Actor::user(Uuid::now_v7(), UserRole::Member);
        assert_eq!(require_owner(&owner).ok(), owner.user_id);
        assert!(matches!(
            require_owner(&member),
            Err(DomainError::Forbidden(_))
        ));
        for actor_type in [
            ActorType::HomeAssistant,
            ActorType::Voice,
            ActorType::System,
        ] {
            assert!(matches!(
                require_owner(&Actor::system(actor_type)),
                Err(DomainError::Forbidden(_))
            ));
            assert!(matches!(
                require_user(&Actor::system(actor_type)),
                Err(DomainError::Forbidden(_))
            ));
        }
    }

    #[test]
    fn token_actor_with_owner_role_is_still_rejected() {
        // Токен интеграции получает роль owner для операций со списками, но
        // user_id у него нет: управлять household он не может.
        let integration = Actor {
            user_id: None,
            role: Some(UserRole::Owner),
            actor_type: ActorType::HomeAssistant,
        };
        assert!(require_owner(&integration).is_err());
        assert!(require_user(&integration).is_err());
    }

    #[test]
    fn members_are_users() {
        let member = Actor::user(Uuid::now_v7(), UserRole::Member);
        assert_eq!(require_user(&member).ok(), member.user_id);
    }
}
