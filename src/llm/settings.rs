//! Применение сохранённых настроек LLM к действующему снимку без перезапуска.
//!
//! [`LlmSettings`] владеет снимком ([`LlmHandle`]), пулом, ключами шифрования и
//! конфигурацией окружения. Один вызов [`LlmSettings::apply`] применяет группу
//! изменений целиком, как сохраняет форму консоли: установить значение,
//! установить секрет, удалить ключ.
//!
//! Порядок задан design, решение 9: под мьютексом транзакция пишет изменения,
//! в той же транзакции читаются настройки инсталляции, собираются действующая
//! конфигурация и цепочка; только после этого commit и `store` в handle. Если
//! конфигурация невалидна или цепочка не собирается, транзакция откатывается, а
//! снимок остаётся прежним. Мьютекс исключает гонку двух сохранений, при которой
//! более раннее попало бы в handle последним.
//!
//! Предохранители слотов переносит [`LlmSnapshot::rebuild`]: слот с неизменным
//! отпечатком продолжает с прежним состоянием, изменённый начинает с замкнутым.

use std::sync::Arc;

use serde_json::Value;
use sqlx::{PgConnection, PgPool};
use thiserror::Error;
use tokio::sync::Mutex;

use super::{LlmError, LlmHandle, LlmSnapshot};
use crate::config::{LlmConfig, SecretString};
use crate::settings::crypto::{SecretCipher, SettingScope};
use crate::settings::{self, EffectiveSlot, SettingError, SlotProblem};
use crate::storage::repositories::settings as repository;
use crate::storage::repositories::settings::StoredSetting;

/// Одно изменение настроек LLM в группе сохранения.
///
/// Секрет и обычное значение разделены: форма консоли присылает их разными
/// полями, а запись проверяет вид ключа.
#[derive(Debug, Clone)]
pub enum LlmChange {
    /// Установить обычное значение известной настройки.
    Set {
        /// Имя настройки.
        key: String,
        /// Значение; проверяется теми же парсерами, что и при запуске.
        value: Value,
    },
    /// Установить секрет известной настройки текущим ключом.
    SetSecret {
        /// Имя настройки.
        key: String,
        /// Открытый текст: в журнал и ошибки не попадает.
        value: SecretString,
    },
    /// Удалить известную настройку.
    Delete {
        /// Имя настройки.
        key: String,
    },
}

impl LlmChange {
    /// Установить обычное значение.
    pub fn set(key: impl Into<String>, value: Value) -> Self {
        Self::Set {
            key: key.into(),
            value,
        }
    }

    /// Установить секрет.
    pub fn secret(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self::SetSecret {
            key: key.into(),
            value: SecretString::new(value),
        }
    }

    /// Удалить настройку.
    pub fn delete(key: impl Into<String>) -> Self {
        Self::Delete { key: key.into() }
    }

    /// Имя изменяемой настройки.
    fn key(&self) -> &str {
        match self {
            Self::Set { key, .. } | Self::SetSecret { key, .. } | Self::Delete { key } => key,
        }
    }
}

/// Ошибка применения группы настроек.
///
/// Ни один вариант не содержит сохранённого значения или секрета: только имя
/// настройки и короткую причину. Любая ошибка означает, что транзакция
/// откатилась и снимок не менялся.
#[derive(Debug, Error)]
pub enum LlmApplyError {
    /// В сохранённом слоте не хватает адреса или модели: слот остался бы
    /// ненастроенным, а форма должна назвать недостающее поле.
    #[error(
        "llm setting `{key}` is not set; a slot needs both an address and a model, so nothing was saved"
    )]
    MissingField {
        /// Имя недостающей настройки.
        key: &'static str,
    },
    /// Значение сохранённого поля не разобралось.
    #[error("llm setting `{key}` has an invalid value, so nothing was saved")]
    InvalidField {
        /// Имя настройки.
        key: &'static str,
    },
    /// Цепочка не собирается из новой конфигурации.
    #[error("cannot build the llm chain: {source}, so nothing was saved")]
    Chain {
        /// Причина без адреса и токена.
        #[from]
        source: LlmError,
    },
    /// Ошибка записи настройки.
    #[error(transparent)]
    Setting(#[from] SettingError),
    /// Ошибка чтения настроек в транзакции.
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    /// Фоновая задача применения не завершилась; подробности не содержат
    /// значений настроек.
    #[error("llm settings task failed")]
    Task(#[from] tokio::task::JoinError),
}

/// Применение сохранённых настроек LLM к снимку.
#[derive(Clone)]
pub struct LlmSettings {
    pool: PgPool,
    env: LlmConfig,
    cipher: SecretCipher,
    handle: LlmHandle,
    write_lock: Arc<Mutex<()>>,
}

impl LlmSettings {
    /// Создать компонент поверх готового снимка.
    pub fn new(pool: PgPool, env: LlmConfig, cipher: SecretCipher, handle: LlmHandle) -> Self {
        Self {
            pool,
            env,
            cipher,
            handle,
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Действующий снимок: через него компонент читают потребители.
    pub fn handle(&self) -> &LlmHandle {
        &self.handle
    }

    /// Действующие значения всех известных ключей для страницы настроек.
    ///
    /// Чтение не берёт мьютекс записи: конкурентное сохранение публикует новый
    /// снимок и строки одной транзакцией, поэтому страница увидит либо прежнее,
    /// либо новое состояние.
    pub async fn read_settings(&self) -> Result<Vec<settings::ResolvedSetting>, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        let stored = repository::all(&mut conn, SettingScope::Installation.as_str(), None).await?;
        Ok(settings::resolve_settings(&self.env, &stored, &self.cipher))
    }

    /// Применить группу изменений целиком.
    ///
    /// Ошибка любого шага откатывает транзакцию, поэтому частично сохранённой
    /// группы не бывает. Снимок заменяется только после commit.
    ///
    /// Применение выполняется отдельной задачей: между commit и `store` нет
    /// точки, где отмена future обработчика оставила бы базу записанной, а
    /// снимок прежним. Отмена ожидания результата (обрыв запроса) не
    /// прерывает применение.
    pub async fn apply(&self, changes: &[LlmChange]) -> Result<Arc<LlmSnapshot>, LlmApplyError> {
        let this = self.clone();
        let changes = changes.to_vec();
        tokio::spawn(async move { this.apply_inner(&changes).await }).await?
    }

    /// Критическая часть [`Self::apply`], переживающая отмену вызывающего.
    async fn apply_inner(&self, changes: &[LlmChange]) -> Result<Arc<LlmSnapshot>, LlmApplyError> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await?;

        for change in changes {
            write_change(&mut tx, change, &self.cipher).await?;
        }

        // Читаем в той же транзакции: сборка видит ровно те значения, которые
        // будут закоммичены.
        let stored = repository::all(&mut tx, SettingScope::Installation.as_str(), None).await?;
        let effective = settings::effective_llm(&self.env, &stored, &self.cipher);
        check_slots(&effective, &stored, changes)?;

        let previous = self.handle.load();
        let snapshot = LlmSnapshot::rebuild(effective, &previous)?;

        tx.commit().await?;
        self.handle.store(snapshot);
        Ok(self.handle.load())
    }
}

impl std::fmt::Debug for LlmSettings {
    /// Печатает только версию снимка: окружение и ключи журналу не нужны.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LlmSettings")
            .field("snapshot", &self.handle)
            .finish_non_exhaustive()
    }
}

/// Записать одно изменение через существующие проверки значений.
async fn write_change(
    conn: &mut PgConnection,
    change: &LlmChange,
    cipher: &SecretCipher,
) -> Result<(), SettingError> {
    match change {
        LlmChange::Set { key, value } => settings::write_value(conn, key, value.clone()).await,
        LlmChange::SetSecret { key, value } => {
            settings::write_secret(conn, key, value, cipher).await
        }
        LlmChange::Delete { key } => {
            settings::delete(conn, key).await?;
            Ok(())
        }
    }
}

/// Отклонить слот из базы, который остался бы ненастроенным.
///
/// Проверяются только слоты, ключи которых есть в наборе изменений: неполный
/// слот из старой строки или прямого SQL не должен блокировать сохранение
/// квоты. Неполный слот (адрес без модели или наоборот) и неразобранное поле в
/// затронутом слоте — ошибка формы: иначе сохранение прошло бы, а действующие
/// слоты молча не изменились. Остальные причины (слот не задан, не читается
/// секрет) сохранение не отклоняют.
fn check_slots(
    effective: &settings::EffectiveLlm,
    stored: &[StoredSetting],
    changes: &[LlmChange],
) -> Result<(), LlmApplyError> {
    if touches_slot(changes, "llm.primary.") {
        check_slot(
            effective.primary(),
            "llm.primary.url",
            "llm.primary.model",
            stored,
        )?;
    }
    if touches_slot(changes, "llm.fallback.") {
        check_slot(
            effective.fallback(),
            "llm.fallback.url",
            "llm.fallback.model",
            stored,
        )?;
    }
    Ok(())
}

/// Набор изменений затрагивает поля слота с заданным префиксом.
fn touches_slot(changes: &[LlmChange], prefix: &str) -> bool {
    changes
        .iter()
        .any(|change| change.key().starts_with(prefix))
}

fn check_slot(
    slot: &EffectiveSlot,
    url_key: &'static str,
    model_key: &'static str,
    stored: &[StoredSetting],
) -> Result<(), LlmApplyError> {
    match slot.problem() {
        Some(SlotProblem::Incomplete) => {
            let missing = if has_value(stored, url_key) {
                model_key
            } else {
                url_key
            };
            Err(LlmApplyError::MissingField { key: missing })
        }
        Some(SlotProblem::InvalidField { field }) => {
            Err(LlmApplyError::InvalidField { key: field })
        }
        Some(
            SlotProblem::NotSet | SlotProblem::SecretKeyMissing | SlotProblem::SecretUndecryptable,
        )
        | None => Ok(()),
    }
}

fn has_value(stored: &[StoredSetting], key: &str) -> bool {
    stored
        .iter()
        .any(|row| row.key == key && row.value.is_some())
}
