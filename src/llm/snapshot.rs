//! Снимок действующей конфигурации LLM.
//!
//! [`LlmSnapshot`] собирает в одном месте всё, что нужно разбору: разрешённые
//! настройки ([`EffectiveLlm`]), готовую [`Chain`] и суточную квоту. Каждый
//! разбор берёт снимок один раз в начале, поэтому уже начатый разбор
//! завершается с прежней цепочкой, а следующий видит новую.
//!
//! [`LlmHandle`] — дешёвая для клонирования обёртка над `ArcSwap`: заменять
//! снимок может консоль администратора, а читают все потребители без
//! блокировок. Версия растёт при каждой записи.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;

use super::Chain;
use crate::config::{DEFAULT_CONNECT_TIMEOUT_MS, DEFAULT_DAILY_QUOTA, LlmConfig};
use crate::settings::crypto::SecretCipher;
use crate::settings::{self, EffectiveLlm};

/// Действующая конфигурация LLM на момент разбора.
#[derive(Debug)]
pub struct LlmSnapshot {
    version: u64,
    effective: EffectiveLlm,
    chain: Option<Arc<Chain>>,
    quota: u32,
}

impl LlmSnapshot {
    /// Собрать снимок из действующей конфигурации.
    ///
    /// Цепочка есть, только если разбор включён и основной слот настроен.
    /// Ошибка сборки не роняет сервис: цепочки нет, а в журнал уходит код
    /// ошибки без адреса и токена.
    pub fn from_effective(effective: EffectiveLlm) -> Self {
        let chain = build_chain(&effective);
        Self::from_effective_and_chain(effective, chain)
    }

    /// Снимок из готовых частей.
    ///
    /// Нужен тестам с двойниками слотов и будущей замене настроек, которая
    /// переносит предохранители между цепочками.
    pub fn from_effective_and_chain(effective: EffectiveLlm, chain: Option<Arc<Chain>>) -> Self {
        let quota = effective.daily_quota();
        Self {
            version: 0,
            effective,
            chain,
            quota,
        }
    }

    /// Снимок с готовой цепочкой и квотой без настроек консоли.
    ///
    /// Только для тестов потребителей снимка: проверяется цепочка и квота, а не
    /// разбор `settings`. Действующая конфигурация в нём пустая, разбор
    /// выключен, поэтому readiness по такому снимку не видит ни одного слота.
    #[doc(hidden)]
    pub fn from_chain(chain: Option<Arc<Chain>>, quota: u32) -> Self {
        Self::from_effective_and_chain(empty_effective(quota), chain)
    }

    /// Версия снимка; растёт при каждой записи в [`LlmHandle`].
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// Разрешённые настройки, из которых собран снимок.
    pub const fn effective(&self) -> &EffectiveLlm {
        &self.effective
    }

    /// Цепочка слотов; `None`, если разбор выключен или слоты не настроены.
    pub fn chain(&self) -> Option<&Arc<Chain>> {
        self.chain.as_ref()
    }

    /// Суточная квота разборов на household.
    pub const fn quota(&self) -> u32 {
        self.quota
    }
}

/// Собрать цепочку из действующей конфигурации с предупреждением об ошибке.
fn build_chain(effective: &EffectiveLlm) -> Option<Arc<Chain>> {
    match Chain::from_effective(effective) {
        Ok(Some(chain)) => Some(Arc::new(chain)),
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(
                error = error.code(),
                "LLM capture is disabled: slot configuration is invalid"
            );
            None
        }
    }
}

/// Пустая действующая конфигурация с заданной квотой.
fn empty_effective(quota: u32) -> EffectiveLlm {
    let env = LlmConfig {
        enabled: Some(false),
        primary: None,
        fallback: None,
        connect_timeout: std::time::Duration::from_millis(u64::from(DEFAULT_CONNECT_TIMEOUT_MS)),
        connect_timeout_from_env: true,
        daily_quota: quota,
        daily_quota_from_env: true,
        ignored_env: Vec::new(),
    };
    settings::effective_llm(&env, &[], &SecretCipher::new(None, None))
}

/// Разделяемый снимок LLM с монотонной версией.
#[derive(Clone)]
pub struct LlmHandle {
    snapshot: Arc<ArcSwap<LlmSnapshot>>,
    next_version: Arc<AtomicU64>,
}

impl LlmHandle {
    /// Создать handle с первым снимком.
    pub fn new(mut snapshot: LlmSnapshot) -> Self {
        snapshot.version = 1;
        Self {
            snapshot: Arc::new(ArcSwap::from_pointee(snapshot)),
            next_version: Arc::new(AtomicU64::new(2)),
        }
    }

    /// Handle без настроенных слотов: HTTP API и тесты без LLM.
    pub fn empty() -> Self {
        Self::new(LlmSnapshot::from_chain(None, DEFAULT_DAILY_QUOTA))
    }

    /// Текущий снимок целиком.
    pub fn load(&self) -> Arc<LlmSnapshot> {
        self.snapshot.load_full()
    }

    /// Заменить снимок; версия новой записи монотонно растёт.
    ///
    /// Присвоение версии и публикация снимка — два отдельных шага, поэтому
    /// вызывающий сериализует записи (в консоли это делает мьютекс сохранения
    /// настроек): иначе две конкурентные записи могут опубликовать меньшую
    /// версию последней.
    pub fn store(&self, mut snapshot: LlmSnapshot) {
        snapshot.version = self.next_version.fetch_add(1, Ordering::SeqCst);
        self.snapshot.store(Arc::new(snapshot));
    }
}

impl fmt::Debug for LlmHandle {
    /// Печатает только версию и признак цепочки: настройки и токены слотов
    /// журналу не нужны.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let snapshot = self.load();
        formatter
            .debug_struct("LlmHandle")
            .field("version", &snapshot.version())
            .field("has_chain", &snapshot.chain().is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::Map;

    use super::*;
    use crate::config::{SecretString, SlotConfig};
    use crate::settings::crypto::SecretCipher;

    fn slot() -> SlotConfig {
        SlotConfig {
            url: "http://127.0.0.1:8080/v1".parse().expect("url"),
            model: "local/model".to_owned(),
            token: Some(SecretString::new("token")),
            timeout: Duration::from_secs(10),
            extra: Map::new(),
        }
    }

    fn effective(enabled: bool, primary: Option<SlotConfig>) -> EffectiveLlm {
        let env = LlmConfig {
            enabled: Some(enabled),
            primary,
            fallback: None,
            connect_timeout: Duration::from_millis(1_000),
            connect_timeout_from_env: true,
            daily_quota: 200,
            daily_quota_from_env: true,
            ignored_env: Vec::new(),
        };
        settings::effective_llm(&env, &[], &SecretCipher::new(None, None))
    }

    #[test]
    fn disabled_capture_builds_no_chain() {
        let snapshot = LlmSnapshot::from_effective(effective(false, Some(slot())));
        assert!(snapshot.chain().is_none());
    }

    #[test]
    fn a_missing_primary_slot_builds_no_chain() {
        let snapshot = LlmSnapshot::from_effective(effective(true, None));
        assert!(snapshot.chain().is_none());
    }

    #[test]
    fn configured_slots_build_a_chain() {
        let snapshot = LlmSnapshot::from_effective(effective(true, Some(slot())));
        let chain = snapshot.chain().expect("configured slot builds a chain");
        assert_eq!(chain.primary().name(), "primary");
        assert_eq!(snapshot.quota(), 200);
    }

    #[test]
    fn an_absent_flag_builds_a_chain_when_the_primary_slot_is_set() {
        // Прежнее умолчание окружения: слот задан, флага нет — разбор включён.
        let env = LlmConfig {
            enabled: None,
            primary: Some(slot()),
            fallback: None,
            connect_timeout: Duration::from_millis(1_000),
            connect_timeout_from_env: true,
            daily_quota: 200,
            daily_quota_from_env: true,
            ignored_env: Vec::new(),
        };
        let effective = settings::effective_llm(&env, &[], &SecretCipher::new(None, None));

        assert!(LlmSnapshot::from_effective(effective).chain().is_some());
    }

    #[test]
    fn every_store_increments_the_version() {
        let handle = LlmHandle::new(LlmSnapshot::from_chain(None, 200));
        assert_eq!(handle.load().version(), 1);

        handle.store(LlmSnapshot::from_chain(None, 300));
        assert_eq!(handle.load().version(), 2);
        assert_eq!(handle.load().quota(), 300);

        handle.store(LlmSnapshot::from_chain(None, 400));
        assert_eq!(handle.load().version(), 3);
    }

    #[test]
    fn an_empty_handle_has_no_chain_and_keeps_its_quota() {
        let handle = LlmHandle::empty();
        let snapshot = handle.load();
        assert!(snapshot.chain().is_none());
        assert_eq!(snapshot.quota(), 200);
    }

    #[test]
    fn a_snapshot_from_a_ready_chain_is_shared_cheaply() {
        let chain = Arc::new(Chain::new(
            crate::llm::Slot::new("primary", Arc::new(UnusedBackend), Duration::from_secs(1)),
            None,
        ));
        let handle = LlmHandle::new(LlmSnapshot::from_chain(Some(chain.clone()), 50));
        assert!(Arc::ptr_eq(handle.load().chain().expect("chain"), &chain));
        assert_eq!(handle.load().quota(), 50);
    }

    struct UnusedBackend;

    #[async_trait::async_trait]
    impl crate::llm::ChatBackend for UnusedBackend {
        async fn complete(
            &self,
            _request: &crate::llm::ChatRequest<'_>,
            _timeout: Duration,
        ) -> Result<crate::llm::ChatCompletion, crate::llm::LlmError> {
            Err(crate::llm::LlmError::Transport)
        }
    }
}
