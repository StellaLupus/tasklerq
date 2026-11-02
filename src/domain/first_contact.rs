//! Первый контакт с ботом: состояние инсталляции и чистый классификатор.
//!
//! В `single` онбординг доступен только на пустой инсталляции и только
//! выбранному владельцу. Во всех остальных случаях пользователь получает
//! информационный экран, а не форму создания household.

use serde::{Deserialize, Serialize};

use crate::config::TenancyMode;

/// Состояние инсталляции по активным household.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "installation", content = "value", rename_all = "snake_case")]
pub enum Installation {
    /// Активных household нет.
    Empty,
    /// Ровно один активный household; `has_owner` — есть ли у него владелец.
    Single {
        /// Есть ли активный владелец.
        has_owner: bool,
    },
    /// Активных household больше одного.
    Multiple,
}

/// Что показать при первом контакте.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum FirstContact {
    /// Открыть форму онбординга.
    Onboard,
    /// Это частное пространство: попросить приглашение у владельца.
    AskInvite,
    /// Нужно назначить владельца командой CLI с этим Telegram ID.
    AdminCommand(i64),
    /// Инсталляция ещё не настроена, подробностей нет.
    NotConfigured,
    /// В `single` больше одного household: вмешательство администратора.
    NeedsAdmin,
}

/// Выбрать исход первого контакта по таблице design D3.
///
/// В режиме [`TenancyMode::Multi`] онбординг всегда доступен, как раньше.
pub fn classify(
    mode: TenancyMode,
    bootstrap_owner: Option<i64>,
    sender_id: i64,
    installation: Installation,
) -> FirstContact {
    if mode == TenancyMode::Multi {
        return FirstContact::Onboard;
    }

    match installation {
        Installation::Empty => match bootstrap_owner {
            Some(owner) if owner != sender_id => FirstContact::NotConfigured,
            _ => FirstContact::Onboard,
        },
        Installation::Single { has_owner: true } => FirstContact::AskInvite,
        Installation::Single { has_owner: false } => match bootstrap_owner {
            Some(owner) if owner != sender_id => FirstContact::NotConfigured,
            _ => FirstContact::AdminCommand(sender_id),
        },
        Installation::Multiple => FirstContact::NeedsAdmin,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENDER: i64 = 111;
    const OTHER: i64 = 222;

    fn single(has_owner: bool) -> Installation {
        Installation::Single { has_owner }
    }

    #[test]
    fn empty_installation_requires_bootstrap_owner_when_set() {
        assert_eq!(
            classify(TenancyMode::Single, None, SENDER, Installation::Empty),
            FirstContact::Onboard
        );
        assert_eq!(
            classify(
                TenancyMode::Single,
                Some(SENDER),
                SENDER,
                Installation::Empty
            ),
            FirstContact::Onboard
        );
        assert_eq!(
            classify(
                TenancyMode::Single,
                Some(OTHER),
                SENDER,
                Installation::Empty
            ),
            FirstContact::NotConfigured
        );
    }

    #[test]
    fn household_with_owner_always_asks_for_an_invite() {
        for bootstrap in [None, Some(SENDER), Some(OTHER)] {
            assert_eq!(
                classify(TenancyMode::Single, bootstrap, SENDER, single(true)),
                FirstContact::AskInvite
            );
        }
    }

    #[test]
    fn household_without_owner_shows_the_admin_command() {
        assert_eq!(
            classify(TenancyMode::Single, None, SENDER, single(false)),
            FirstContact::AdminCommand(SENDER)
        );
        assert_eq!(
            classify(TenancyMode::Single, Some(SENDER), SENDER, single(false)),
            FirstContact::AdminCommand(SENDER)
        );
        assert_eq!(
            classify(TenancyMode::Single, Some(OTHER), SENDER, single(false)),
            FirstContact::NotConfigured
        );
    }

    #[test]
    fn multiple_households_need_an_admin() {
        for bootstrap in [None, Some(SENDER), Some(OTHER)] {
            assert_eq!(
                classify(
                    TenancyMode::Single,
                    bootstrap,
                    SENDER,
                    Installation::Multiple
                ),
                FirstContact::NeedsAdmin
            );
        }
    }

    #[test]
    fn multi_always_starts_onboarding() {
        for bootstrap in [None, Some(SENDER), Some(OTHER)] {
            for installation in [
                Installation::Empty,
                single(true),
                single(false),
                Installation::Multiple,
            ] {
                assert_eq!(
                    classify(TenancyMode::Multi, bootstrap, SENDER, installation),
                    FirstContact::Onboard
                );
            }
        }
    }
}
