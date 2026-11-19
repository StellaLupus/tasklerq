//! Первый контакт с ботом: состояние инсталляции и чистый классификатор.
//!
//! В `single` онбординг доступен только на пустой инсталляции и только
//! выбранному владельцу. Во всех остальных случаях пользователь получает
//! информационный экран, а не форму создания household.
//!
//! Ссылка-claim (design 0014, решение 5) расширяет правила: действительный код
//! claim на household без владельца даёт [`FirstContact::ClaimOwner`] в любом
//! режиме, а код на пустую инсталляцию открывает онбординг даже при чужом
//! `BOOTSTRAP_OWNER_TELEGRAM_ID` и созданной учётке администратора.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::TenancyMode;

/// Состояние инсталляции по активным household.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "installation", content = "value", rename_all = "snake_case")]
pub enum Installation {
    /// Активных household нет.
    Empty,
    /// Ровно один активный household.
    Single {
        /// Идентификатор единственного household: по нему назначается
        /// владелец из `BOOTSTRAP_OWNER_TELEGRAM_ID`.
        household_id: Uuid,
        /// Есть ли активный владелец.
        has_owner: bool,
    },
    /// Активных household больше одного.
    Multiple,
}

/// Цель действительного кода claim.
///
/// Проверку срока, назначения и отсутствия владельца делает вызывающий: к
/// классификатору попадает только уже подтверждённая цель.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimTarget {
    /// Пустая инсталляция в `single`: онбординг по ссылке-claim.
    EmptyInstallation,
    /// Конкретный household без владельца.
    Household(Uuid),
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
    AdminCommand {
        /// Telegram ID отправителя для подстановки в команду.
        telegram_id: i64,
        /// Учётка администратора уже создана: можно попросить ссылку-claim.
        admin_exists: bool,
    },
    /// Отправитель становится владельцем указанного household.
    ///
    /// Исход несёт цель: id из ссылки-claim или id единственного household,
    /// выбранного `BOOTSTRAP_OWNER_TELEGRAM_ID`.
    ClaimOwner(Uuid),
    /// Инсталляция ещё не настроена, подробностей нет.
    NotConfigured,
    /// В `single` больше одного household: вмешательство администратора.
    NeedsAdmin,
}

/// Отбросить claim, не согласующийся с состоянием инсталляции.
///
/// Вызывающий проверяет код и цель, но между проверкой и классификацией
/// состояние могло измениться. Claim на пустую инсталляцию при уже созданном
/// household, claim на household при пустой инсталляции и claim на единственный
/// household, у которого уже есть владелец, считаются отсутствующими: поведение
/// совпадает с `/start` без ссылки.
fn effective_claim(installation: Installation, claim: Option<ClaimTarget>) -> Option<ClaimTarget> {
    match (installation, claim) {
        (Installation::Empty, Some(ClaimTarget::EmptyInstallation)) => claim,
        (Installation::Empty, _) => None,
        (
            Installation::Single {
                has_owner: true, ..
            },
            Some(ClaimTarget::Household(_)),
        ) => None,
        (_, Some(ClaimTarget::Household(_))) => claim,
        (_, Some(ClaimTarget::EmptyInstallation)) => None,
        (_, None) => None,
    }
}

/// Выбрать исход первого контакта по таблице design 0014, решение 5.
///
/// В режиме [`TenancyMode::Multi`] онбординг доступен каждому, как раньше;
/// меняет его только claim на household без владельца. `admin_exists`
/// закрывает правило первого написавшего только в `single` на пустой
/// инсталляции без bootstrap-владельца и добавляет подсказку о ссылке к
/// CLI-команде у household без владельца.
pub fn classify(
    mode: TenancyMode,
    bootstrap_owner: Option<i64>,
    sender_id: i64,
    installation: Installation,
    claim: Option<ClaimTarget>,
    admin_exists: bool,
) -> FirstContact {
    let claim = effective_claim(installation, claim);

    if mode == TenancyMode::Multi {
        return match claim {
            Some(ClaimTarget::Household(household_id)) => FirstContact::ClaimOwner(household_id),
            _ => FirstContact::Onboard,
        };
    }

    match installation {
        Installation::Empty => match claim {
            Some(ClaimTarget::EmptyInstallation) => FirstContact::Onboard,
            _ => match bootstrap_owner {
                Some(owner) if owner != sender_id => FirstContact::NotConfigured,
                Some(_) => FirstContact::Onboard,
                None if admin_exists => FirstContact::NotConfigured,
                None => FirstContact::Onboard,
            },
        },
        Installation::Single {
            has_owner: true, ..
        } => FirstContact::AskInvite,
        Installation::Single {
            household_id,
            has_owner: false,
        } => match claim {
            Some(ClaimTarget::Household(claimed)) => FirstContact::ClaimOwner(claimed),
            _ => match bootstrap_owner {
                Some(owner) if owner != sender_id => FirstContact::NotConfigured,
                Some(_) => FirstContact::ClaimOwner(household_id),
                None => FirstContact::AdminCommand {
                    telegram_id: sender_id,
                    admin_exists,
                },
            },
        },
        Installation::Multiple => FirstContact::NeedsAdmin,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENDER: i64 = 111;
    const OTHER: i64 = 222;
    const HOUSEHOLD: Uuid = Uuid::from_u128(7);

    fn single(has_owner: bool) -> Installation {
        Installation::Single {
            household_id: HOUSEHOLD,
            has_owner,
        }
    }

    fn admin_command(admin_exists: bool) -> FirstContact {
        FirstContact::AdminCommand {
            telegram_id: SENDER,
            admin_exists,
        }
    }

    fn check(
        mode: TenancyMode,
        installation: Installation,
        claim: Option<ClaimTarget>,
        bootstrap: Option<i64>,
        admin_exists: bool,
        expected: FirstContact,
    ) {
        assert_eq!(
            classify(mode, bootstrap, SENDER, installation, claim, admin_exists),
            expected,
            "mode={mode:?}, installation={installation:?}, claim={claim:?}, \
             bootstrap={bootstrap:?}, admin_exists={admin_exists}"
        );
    }

    /// Каждая строка таблицы design 0014, решение 5.
    ///
    /// Входы, отмеченные в таблице «—», проверяются хотя бы в двух значениях:
    /// циклы перебирают все варианты bootstrap и `admin_exists`.
    #[test]
    fn design_table_decides_the_first_contact() {
        let bootstraps = [None, Some(SENDER), Some(OTHER)];
        let admin_states = [false, true];
        let all_installations = [
            Installation::Empty,
            single(true),
            single(false),
            Installation::Multiple,
        ];

        // multi + claim на дом без владельца -> ClaimOwner.
        for installation in [single(false), Installation::Multiple] {
            for bootstrap in bootstraps {
                for admin_exists in admin_states {
                    check(
                        TenancyMode::Multi,
                        installation,
                        Some(ClaimTarget::Household(HOUSEHOLD)),
                        bootstrap,
                        admin_exists,
                        FirstContact::ClaimOwner(HOUSEHOLD),
                    );
                }
            }
        }

        // multi без claim (и с несогласованным claim) -> Onboard.
        for installation in all_installations {
            for bootstrap in bootstraps {
                for admin_exists in admin_states {
                    check(
                        TenancyMode::Multi,
                        installation,
                        None,
                        bootstrap,
                        admin_exists,
                        FirstContact::Onboard,
                    );
                    check(
                        TenancyMode::Multi,
                        installation,
                        Some(ClaimTarget::EmptyInstallation),
                        bootstrap,
                        admin_exists,
                        FirstContact::Onboard,
                    );
                }
            }
        }
        check(
            TenancyMode::Multi,
            Installation::Empty,
            Some(ClaimTarget::Household(HOUSEHOLD)),
            None,
            false,
            FirstContact::Onboard,
        );
        // Claim на единственный household с владельцем — как без claim.
        check(
            TenancyMode::Multi,
            single(true),
            Some(ClaimTarget::Household(HOUSEHOLD)),
            None,
            false,
            FirstContact::Onboard,
        );

        // single Empty + claim на пустую инсталляцию -> Onboard при любых
        // bootstrap и admin_exists.
        for bootstrap in bootstraps {
            for admin_exists in admin_states {
                check(
                    TenancyMode::Single,
                    Installation::Empty,
                    Some(ClaimTarget::EmptyInstallation),
                    bootstrap,
                    admin_exists,
                    FirstContact::Onboard,
                );
            }
        }

        // single Empty без claim.
        for admin_exists in admin_states {
            check(
                TenancyMode::Single,
                Installation::Empty,
                None,
                Some(SENDER),
                admin_exists,
                FirstContact::Onboard,
            );
            check(
                TenancyMode::Single,
                Installation::Empty,
                None,
                Some(OTHER),
                admin_exists,
                FirstContact::NotConfigured,
            );
        }
        check(
            TenancyMode::Single,
            Installation::Empty,
            None,
            None,
            false,
            FirstContact::Onboard,
        );
        check(
            TenancyMode::Single,
            Installation::Empty,
            None,
            None,
            true,
            FirstContact::NotConfigured,
        );

        // Claim на household при пустой инсталляции — как без claim.
        for (bootstrap, admin_exists, expected) in [
            (Some(SENDER), false, FirstContact::Onboard),
            (Some(SENDER), true, FirstContact::Onboard),
            (Some(OTHER), false, FirstContact::NotConfigured),
            (Some(OTHER), true, FirstContact::NotConfigured),
            (None, false, FirstContact::Onboard),
            (None, true, FirstContact::NotConfigured),
        ] {
            check(
                TenancyMode::Single,
                Installation::Empty,
                Some(ClaimTarget::Household(HOUSEHOLD)),
                bootstrap,
                admin_exists,
                expected,
            );
        }

        // single без владельца + claim на household -> ClaimOwner.
        for bootstrap in bootstraps {
            for admin_exists in admin_states {
                check(
                    TenancyMode::Single,
                    single(false),
                    Some(ClaimTarget::Household(HOUSEHOLD)),
                    bootstrap,
                    admin_exists,
                    FirstContact::ClaimOwner(HOUSEHOLD),
                );
            }
        }

        // single без владельца + claim на пустую инсталляцию — как без claim.
        for (bootstrap, admin_exists, expected) in [
            (Some(SENDER), false, FirstContact::ClaimOwner(HOUSEHOLD)),
            (Some(SENDER), true, FirstContact::ClaimOwner(HOUSEHOLD)),
            (Some(OTHER), false, FirstContact::NotConfigured),
            (Some(OTHER), true, FirstContact::NotConfigured),
            (None, false, admin_command(false)),
            (None, true, admin_command(true)),
        ] {
            check(
                TenancyMode::Single,
                single(false),
                Some(ClaimTarget::EmptyInstallation),
                bootstrap,
                admin_exists,
                expected,
            );
        }

        // single без владельца без claim.
        for admin_exists in admin_states {
            check(
                TenancyMode::Single,
                single(false),
                None,
                Some(SENDER),
                admin_exists,
                FirstContact::ClaimOwner(HOUSEHOLD),
            );
            check(
                TenancyMode::Single,
                single(false),
                None,
                Some(OTHER),
                admin_exists,
                FirstContact::NotConfigured,
            );
        }
        check(
            TenancyMode::Single,
            single(false),
            None,
            None,
            false,
            admin_command(false),
        );
        check(
            TenancyMode::Single,
            single(false),
            None,
            None,
            true,
            admin_command(true),
        );

        // single с владельцем -> AskInvite, Multiple -> NeedsAdmin: как раньше,
        // независимо от claim, bootstrap и admin_exists.
        for claim in [
            None,
            Some(ClaimTarget::EmptyInstallation),
            Some(ClaimTarget::Household(HOUSEHOLD)),
        ] {
            for bootstrap in bootstraps {
                for admin_exists in admin_states {
                    check(
                        TenancyMode::Single,
                        single(true),
                        claim,
                        bootstrap,
                        admin_exists,
                        FirstContact::AskInvite,
                    );
                    check(
                        TenancyMode::Single,
                        Installation::Multiple,
                        claim,
                        bootstrap,
                        admin_exists,
                        FirstContact::NeedsAdmin,
                    );
                }
            }
        }
    }
}
