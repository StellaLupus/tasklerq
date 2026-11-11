//! Статус проверки слота LLM.
//!
//! Статус вычисляется сравнением отпечатка действующей конфигурации слота,
//! контракта и smoke-набора с отпечатком, сохранённым командой `capture-eval`
//! ([`super::slot_check::fingerprint`]). Он не блокирует работу: попадает в лог
//! при старте, в метрику `llm_slot_verified{slot}` и в предупреждения
//! `/health/ready`.
//!
//! Readiness не опрашивает `llm_slot_checks` на каждый запрос: вычисленный
//! статус кешируется на [`CACHE_TTL`]. Ошибка чтения базы при старте — только
//! предупреждение, а в readiness используется прежний кеш.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;

use super::prompt;
use super::slot_check::{self, BUILTIN_SET};
use crate::config::{LlmConfig, SlotConfig};
use crate::llm::{Chain, FALLBACK_SLOT, PRIMARY_SLOT};
use crate::metrics;
use crate::storage::repositories::llm_slot_checks::{self, StoredCheck};

/// Как долго readiness использует вычисленный статус слотов.
pub const CACHE_TTL: Duration = Duration::minutes(1);

/// Статус проверки слота smoke-набором.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStatus {
    /// Результата проверки нет.
    Unverified,
    /// Отпечаток действующей конфигурации отличается от сохранённого.
    Stale,
    /// Отпечаток совпадает, пройдены все фразы.
    Passed,
    /// Отпечаток совпадает, пройдены не все фразы.
    Failed {
        /// Сколько фраз пройдено.
        passed: usize,
        /// Сколько фраз всего.
        total: usize,
    },
}

impl SlotStatus {
    /// Слот прошёл проверку; только этот статус даёт метрику `1`.
    pub fn is_passed(self) -> bool {
        matches!(self, Self::Passed)
    }

    /// Короткий код для журнала: без пользовательских данных.
    pub fn code(self) -> &'static str {
        match self {
            Self::Unverified => "unverified",
            Self::Stale => "stale",
            Self::Passed => "passed",
            Self::Failed { .. } => "failed",
        }
    }
}

/// Статус одного настроенного слота.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotStatusReport {
    /// Имя слота: `primary` или `fallback`.
    pub slot: &'static str,
    /// Статус проверки.
    pub status: SlotStatus,
}

/// Статус слота по сохранённой проверке.
///
/// Нет строки — «не проверен»; отпечаток отличается — «устарел»; отпечаток
/// совпадает, и пройдены все фразы — «прошёл»; иначе — «не прошёл» с числом
/// пройденных. Набор с нулём фраз не считается пройденным.
pub fn status(
    slot: &SlotConfig,
    check: Option<&StoredCheck>,
    contract_hash: &str,
    set_hash: &str,
) -> SlotStatus {
    let Some(check) = check else {
        return SlotStatus::Unverified;
    };
    let current = slot_check::fingerprint(slot, contract_hash, set_hash);
    if check.fingerprint != current.value {
        return SlotStatus::Stale;
    }

    let passed = usize::try_from(check.passed).unwrap_or(0);
    let total = usize::try_from(check.total).unwrap_or(0);
    if total > 0 && passed == total {
        SlotStatus::Passed
    } else {
        SlotStatus::Failed { passed, total }
    }
}

/// Состояние проверки слотов для старта и readiness.
pub struct LlmSlotHealth {
    chain: Arc<Chain>,
    slots: Vec<SlotEntry>,
    contract_hash: String,
    set_hash: String,
    cache: Mutex<Option<CachedStatuses>>,
}

struct SlotEntry {
    name: &'static str,
    config: SlotConfig,
}

struct CachedStatuses {
    at: DateTime<Utc>,
    statuses: Vec<SlotStatusReport>,
}

impl LlmSlotHealth {
    /// Собрать состояние из действующей цепочки и конфигурации.
    ///
    /// `None`, если разбор выключен или основной слот не задан: тогда и
    /// предупреждать не о чем. Цепочка передаётся снаружи, чтобы readiness
    /// видел те же предохранители, что и рабочий разбор.
    pub fn new(chain: Arc<Chain>, llm: &LlmConfig) -> Option<Self> {
        if !llm.enabled {
            return None;
        }
        let primary = llm.primary.clone()?;
        let mut slots = vec![SlotEntry {
            name: PRIMARY_SLOT,
            config: primary,
        }];
        if let Some(fallback) = &llm.fallback {
            slots.push(SlotEntry {
                name: FALLBACK_SLOT,
                config: fallback.clone(),
            });
        }

        Some(Self {
            chain,
            slots,
            contract_hash: prompt::contract_hash(),
            set_hash: slot_check::set_hash(BUILTIN_SET),
            cache: Mutex::new(None),
        })
    }

    /// Предохранители всех настроенных слотов разомкнуты.
    ///
    /// `retry_after` не меняет состояние предохранителя, в отличие от `allow`:
    /// проверка readiness не должна сбрасывать паузу перед новой попыткой.
    pub fn all_breakers_open(&self, now: DateTime<Utc>) -> bool {
        let mut configured = false;
        for slot in [Some(self.chain.primary()), self.chain.fallback()]
            .into_iter()
            .flatten()
        {
            configured = true;
            if slot.breaker().retry_after(now).is_none() {
                return false;
            }
        }
        configured
    }

    /// Сбросить кеш статусов.
    ///
    /// Нужен после ручного прогона `capture-eval` в том же процессе и тестам,
    /// которые проверяют срок кеша без реального ожидания минуты.
    pub fn invalidate(&self) {
        *self.lock_cache() = None;
    }

    /// Статусы для readiness: из кеша, если он моложе [`CACHE_TTL`].
    ///
    /// Ошибка чтения базы не делает readiness неуспешным: возвращается прежний
    /// кеш, а без него все слоты считаются непроверенными. Ошибка пишется в
    /// журнал без адресов и токенов.
    pub async fn statuses(&self, pool: &PgPool, now: DateTime<Utc>) -> Vec<SlotStatusReport> {
        if let Some(cached) = self.cached(now) {
            return cached;
        }

        match self.refresh(pool, now).await {
            Ok(statuses) => statuses,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "could not read llm slot check status; keeping the previous one"
                );
                if let Some(cached) = self.lock_cache().as_ref() {
                    return cached.statuses.clone();
                }
                let statuses = self.unverified();
                for report in &statuses {
                    metrics::llm_slot_verified(report.slot, false);
                }
                statuses
            }
        }
    }

    /// Перечитать статусы из базы, обновить метрику и кеш.
    pub async fn refresh(
        &self,
        pool: &PgPool,
        now: DateTime<Utc>,
    ) -> Result<Vec<SlotStatusReport>, sqlx::Error> {
        let mut conn = pool.acquire().await?;
        let mut statuses = Vec::with_capacity(self.slots.len());
        for entry in &self.slots {
            let check = llm_slot_checks::latest(&mut conn, entry.name).await?;
            statuses.push(SlotStatusReport {
                slot: entry.name,
                status: status(
                    &entry.config,
                    check.as_ref(),
                    &self.contract_hash,
                    &self.set_hash,
                ),
            });
        }
        drop(conn);

        for report in &statuses {
            metrics::llm_slot_verified(report.slot, report.status.is_passed());
        }
        *self.lock_cache() = Some(CachedStatuses {
            at: now,
            statuses: statuses.clone(),
        });
        Ok(statuses)
    }

    fn cached(&self, now: DateTime<Utc>) -> Option<Vec<SlotStatusReport>> {
        let cache = self.lock_cache();
        let cached = cache.as_ref()?;
        (now - cached.at < CACHE_TTL).then(|| cached.statuses.clone())
    }

    fn unverified(&self) -> Vec<SlotStatusReport> {
        self.slots
            .iter()
            .map(|entry| SlotStatusReport {
                slot: entry.name,
                status: SlotStatus::Unverified,
            })
            .collect()
    }

    fn lock_cache(&self) -> std::sync::MutexGuard<'_, Option<CachedStatuses>> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::Map;

    use super::*;
    use crate::config::SecretString;

    const CONTRACT: &str = "contract-hash";
    const SET: &str = "set-hash";

    fn slot() -> SlotConfig {
        SlotConfig {
            url: "https://api.example.test/v1".parse().expect("url"),
            model: "vendor/model".to_owned(),
            token: Some(SecretString::new("slot-token")),
            timeout: Duration::from_secs(10),
            extra: Map::new(),
        }
    }

    fn passed_check(slot: &SlotConfig, contract: &str) -> StoredCheck {
        let fingerprint = slot_check::fingerprint(slot, contract, SET);
        StoredCheck {
            fingerprint: fingerprint.value,
            passed: 22,
            total: 22,
        }
    }

    #[test]
    fn no_saved_check_is_unverified() {
        assert_eq!(status(&slot(), None, CONTRACT, SET), SlotStatus::Unverified);
    }

    #[test]
    fn a_model_change_makes_the_status_stale() {
        let stored = passed_check(&slot(), CONTRACT);
        let mut changed = slot();
        changed.model = "vendor/other".to_owned();

        assert_eq!(
            status(&changed, Some(&stored), CONTRACT, SET),
            SlotStatus::Stale
        );
    }

    #[test]
    fn a_token_change_keeps_the_passed_status() {
        let stored = passed_check(&slot(), CONTRACT);
        let mut changed = slot();
        changed.token = Some(SecretString::new("another-token"));

        assert_eq!(
            status(&changed, Some(&stored), CONTRACT, SET),
            SlotStatus::Passed
        );

        changed.token = None;
        assert_eq!(
            status(&changed, Some(&stored), CONTRACT, SET),
            SlotStatus::Passed
        );
    }

    #[test]
    fn a_new_contract_makes_the_status_stale() {
        let stored = passed_check(&slot(), CONTRACT);

        assert_eq!(
            status(&slot(), Some(&stored), "another-contract", SET),
            SlotStatus::Stale
        );
    }

    #[test]
    fn a_new_set_makes_the_status_stale() {
        let stored = passed_check(&slot(), CONTRACT);

        assert_eq!(
            status(&slot(), Some(&stored), CONTRACT, "another-set"),
            SlotStatus::Stale
        );
    }

    #[test]
    fn a_failed_check_reports_the_passed_count() {
        let stored = StoredCheck {
            fingerprint: slot_check::fingerprint(&slot(), CONTRACT, SET).value,
            passed: 17,
            total: 22,
        };

        assert_eq!(
            status(&slot(), Some(&stored), CONTRACT, SET),
            SlotStatus::Failed {
                passed: 17,
                total: 22
            }
        );
        assert!(
            !SlotStatus::Failed {
                passed: 17,
                total: 22
            }
            .is_passed()
        );
    }

    #[test]
    fn an_empty_set_never_passes() {
        let stored = StoredCheck {
            fingerprint: slot_check::fingerprint(&slot(), CONTRACT, SET).value,
            passed: 0,
            total: 0,
        };

        assert_eq!(
            status(&slot(), Some(&stored), CONTRACT, SET),
            SlotStatus::Failed {
                passed: 0,
                total: 0
            }
        );
    }

    #[test]
    fn only_a_passed_status_is_verified() {
        assert!(SlotStatus::Passed.is_passed());
        assert!(!SlotStatus::Unverified.is_passed());
        assert!(!SlotStatus::Stale.is_passed());
    }
}
