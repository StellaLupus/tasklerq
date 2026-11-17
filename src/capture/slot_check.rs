//! Проверка слота smoke-набором: встроенный набор, прогон фраз и отпечаток.
//!
//! [`run_check`] — общий прогон для команды `capture-eval` (9.3) и кнопки
//! проверки в консоли (6.4). Он гоняет каждую фразу напрямую через бэкенд слота
//! (без цепочки, перехода на резервный слот и предохранителя) и судит ответ по
//! таймауту слота ([`super::eval`]). Ожидание одной фразы ограничено
//! [`PHRASE_WAIT`]: верный, но медленный ответ должен попасть в отчёт с
//! фактическим временем, а не оборваться раньше срока.
//!
//! Одновременно выполняется не более одной проверки на инсталляцию:
//! [`run_check`] держит сеансовую advisory-блокировку [`CHECK_LOCK_KEY`] на
//! отдельном соединении всё время прогона и записи. Мьютекс в памяти не годится:
//! `capture-eval` в `docker exec` — другой процесс.
//!
//! Набор по умолчанию лежит рядом с шаблоном промпта и встроен в бинарник:
//! проверка идёт в контейнере, где исходников `tests/` нет. `--set PATH` —
//! переопределение для разработки.
//!
//! Отпечаток слота ([`fingerprint`]) переиспользуется статусом проверки в 9.4:
//! он сравнивает отпечаток действующей конфигурации с сохранённым. В отпечаток
//! входят вид слота, модель, хост адреса, хеш контракта, хеш набора, таймаут и
//! стандарт сравнения названий; токен и квота не входят.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::pool::PoolConnection;
use sqlx::{Connection, PgPool, Postgres};
use thiserror::Error;
use url::Url;

use super::eval::{
    Report, SlotRun, SmokeSet, TitleMatch, check_prompt_version, evaluate, evaluate_with,
};
use super::prompt;
use crate::config::SlotConfig;
use crate::llm::{ChatBackend, ChatRequest};
use crate::storage::repositories::llm_slot_checks::{self, NewCheck};
use crate::storage::repositories::llm_slot_settings;

/// Набор по умолчанию, встроенный в бинарник на момент сборки.
pub const BUILTIN_SET: &str = include_str!("prompts/smoke-v4.json");

/// Сколько ждать ответа на одну фразу: дольше любого разумного таймаута слота,
/// но всё же ограниченно, чтобы зависший слот не держал команду вечно.
pub const PHRASE_WAIT: Duration = Duration::from_secs(20);

/// Вид слота в отпечатке: единственная реализация бэкенда.
pub const SLOT_KIND: &str = "openai";

/// Прочитать набор и проверить версию контракта до любых вызовов LLM.
pub fn load_set(text: &str) -> Result<SmokeSet, String> {
    let set = SmokeSet::parse(text)?;
    check_prompt_version(&set)?;
    Ok(set)
}

/// Сколько фраз во встроенном наборе.
///
/// Кнопка проверки в консоли показывает число из набора, а не литерал: набор
/// может измениться без правки страницы. Разбор кэшируется: страница не должна
/// платить за него на каждый показ.
pub fn builtin_phrase_count() -> usize {
    static COUNT: OnceLock<usize> = OnceLock::new();
    *COUNT.get_or_init(|| load_set(BUILTIN_SET).map_or(0, |set| set.phrases.len()))
}

/// Прогнать все фразы набора через бэкенд слота.
///
/// Фразы идут по очереди. Запрос собирает та же функция, что и боевой разбор
/// ([`prompt::build_request`]), поэтому проверяется настоящий контракт. Ошибка
/// клиента становится исходом слота, а не ошибкой команды: провал фразы должен
/// попасть в отчёт.
pub async fn run_set(
    backend: &dyn ChatBackend,
    set: &SmokeSet,
    timeout: Duration,
) -> Result<Report, String> {
    let runs = collect_runs(backend, set).await?;
    evaluate(set, &runs, timeout)
}

/// Собрать исходы вызовов слота, не судя их.
///
/// Нужен, когда один прогон надо оценить в нескольких режимах сравнения
/// названий: вызовы слота платные и недетерминированные, повторять их нельзя.
pub async fn collect_runs(
    backend: &dyn ChatBackend,
    set: &SmokeSet,
) -> Result<BTreeMap<String, SlotRun>, String> {
    collect_runs_with(backend, set, |_, _| {}).await
}

/// Собрать исходы вызовов слота и сообщать о ходе прогона.
///
/// Наблюдатель получает число пройденных фраз и общее число после каждой
/// фразы. Нулевой вызов перед первой фразой сообщает общее число сразу:
/// страница проверки показывает «0 из N», пока ответа ещё нет.
pub async fn collect_runs_with(
    backend: &dyn ChatBackend,
    set: &SmokeSet,
    progress: impl Fn(usize, usize),
) -> Result<BTreeMap<String, SlotRun>, String> {
    let total = set.phrases.len();
    progress(0, total);
    let mut runs = BTreeMap::new();
    for (index, phrase) in set.phrases.iter().enumerate() {
        let context = phrase.context(&set.context)?;
        let request = prompt::build_request(&context, &phrase.text);
        let chat = ChatRequest {
            messages: &request.messages,
            schema_name: prompt::SCHEMA_NAME,
            schema: &request.schema,
        };
        let started = Instant::now();
        let answer = backend.complete(&chat, PHRASE_WAIT).await;
        let duration = started.elapsed();
        let run = match answer {
            Ok(completion) => SlotRun::text(completion.content, duration),
            Err(error) => SlotRun::error(error.code(), duration),
        };
        runs.insert(phrase.id.clone(), run);
        progress(index + 1, total);
    }
    Ok(runs)
}

/// Фиксированный ключ advisory-блокировки проверки слота.
///
/// Ключ общий для всех слотов и процессов: одновременно выполняется не более
/// одной проверки, поэтому повторный запуск не тратит платные вызовы. Значение
/// не пересекается с ключом retention (`RTNTLOCK`) и блокировками по Telegram ID.
pub const CHECK_LOCK_KEY: i64 = 0x0053_4C4F_5443_484B; // "SLOTCHK"

/// Проверка слота, подготовленная до первого запроса к LLM.
///
/// Конфигурацию слота собирает вызывающий: CLI — из `settings` и окружения,
/// консоль — из действующего снимка. Бэкенд — граница, за которой транспорт:
/// в тестах вместо [`crate::llm::OpenAiBackend`] может стоять двойник.
pub struct PreparedCheck {
    /// Имя слота для отчёта и `llm_slot_checks`.
    pub slot_name: &'static str,
    /// Конфигурация слота.
    pub slot: SlotConfig,
    /// Клиент выбранного слота.
    pub backend: Box<dyn ChatBackend>,
    /// Разобранный набор.
    pub set: SmokeSet,
    /// Исходный текст набора: из него считается хеш.
    pub set_text: String,
}

/// Итог прогона проверки — всё, что печатает CLI и показывает консоль.
///
/// Отчёт готов независимо от записи: `saved` несёт её итог отдельно, чтобы
/// отказ базы не скрывал результат проверки.
#[derive(Debug)]
pub struct CheckResult {
    /// Имя слота.
    pub slot_name: &'static str,
    /// Модель слота.
    pub model: String,
    /// Отпечаток конфигурации, контракта, набора и стандарта.
    pub fingerprint: SlotFingerprint,
    /// Отчёт по стандарту слота.
    pub report: Report,
    /// Отчёт по другому стандарту: тот же прогон, справочно.
    pub other: Option<Report>,
    /// Стандарт второго отчёта.
    pub other_titles: TitleMatch,
    /// Итог записи проверки и стандарта.
    pub saved: Result<(), SaveError>,
}

/// Ошибка проверки до готового отчёта.
#[derive(Debug, Error)]
pub enum CheckError {
    /// Проверку уже выполняет другой прогон: блокировку держит он.
    #[error("проверка уже идёт")]
    Busy,
    /// Не удалось подключиться к базе или взять соединение.
    #[error("database connection failed")]
    Database(#[source] sqlx::Error),
    /// Не удалось прочитать действующий стандарт слота.
    #[error("failed to read the slot standard")]
    Standard(#[source] sqlx::Error),
    /// Набор не разбирается или не оценивается: прогон не имеет вердикта.
    #[error("invalid smoke set: {0}")]
    Set(String),
}

/// Ошибка записи готового отчёта.
#[derive(Debug, Error)]
pub enum SaveError {
    /// Не удалось записать проверку.
    #[error("failed to save the slot check")]
    Check(#[source] sqlx::Error),
    /// Не удалось записать стандарт слота.
    #[error("failed to save the slot standard")]
    Standard(#[source] sqlx::Error),
}

/// Прогнать набор через слот, оценить его и записать проверку.
///
/// Одновременно выполняется не более одной проверки на инсталляцию:
/// сеансовая advisory-блокировка [`CHECK_LOCK_KEY`] держится на отдельном
/// соединении всё время прогона и записи и снимается в конце, в том числе при
/// ошибке. Без блокировки возвращается [`CheckError::Busy`] до первого вызова
/// LLM. Запись результата идёт своим соединением: долгий прогон не занимает
/// соединение блокировки под запросы.
pub async fn run_check(
    pool: &PgPool,
    prepared: PreparedCheck,
    titles: Option<TitleMatch>,
) -> Result<CheckResult, CheckError> {
    run_check_with_progress(pool, prepared, titles, |_, _| {}).await
}

/// То же, что [`run_check`], но с сообщением о ходе прогона.
///
/// Консоль передаёт наблюдателя, который обновляет ход в памяти процесса;
/// команда `capture-eval` — пустого. Наблюдатель не может отменить прогон.
pub async fn run_check_with_progress(
    pool: &PgPool,
    prepared: PreparedCheck,
    titles: Option<TitleMatch>,
    progress: impl Fn(usize, usize) + Sync,
) -> Result<CheckResult, CheckError> {
    let lock = CheckLock::acquire(pool).await?;
    let outcome = run_locked(pool, &prepared, titles, &progress).await;
    lock.release().await;
    outcome
}

/// Соединение с сеансовой advisory-блокировкой проверки.
///
/// Соединение не возвращается в пул с блокировкой: при выходе без явного
/// `pg_advisory_unlock` (отмена future, паника) или при ошибке снятия оно
/// закрывается. Иначе блокировка залипла бы до закрытия соединения, и все
/// следующие проверки, включая `capture-eval` в другом процессе, получали бы
/// «проверка уже идёт».
struct CheckLock {
    conn: Option<PoolConnection<Postgres>>,
}

impl CheckLock {
    /// Взять блокировку или вернуть [`CheckError::Busy`], не тревожа LLM.
    async fn acquire(pool: &PgPool) -> Result<Self, CheckError> {
        let mut conn = pool.acquire().await.map_err(CheckError::Database)?;
        // Соединение блокировки только держит блокировку: стандарт и запись
        // идут по своим соединениям из пула, поэтому обрыв простаивающего
        // сеанса не ломает результат. Такой обрыв снимет блокировку досрочно —
        // это цена сеансовой блокировки при долгом простое; серверный idle
        // timeout в конфигурации сервиса не задан.
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(CHECK_LOCK_KEY)
            .fetch_one(&mut *conn)
            .await
            .map_err(CheckError::Database)?;
        if !acquired {
            return Err(CheckError::Busy);
        }
        Ok(Self { conn: Some(conn) })
    }

    /// Снять блокировку и вернуть соединение в пул.
    ///
    /// При ошибке снятия соединение закрывается: `Drop` не даст вернуть его в
    /// пул с блокировкой.
    async fn release(mut self) {
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        let released = sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(CHECK_LOCK_KEY)
            .execute(&mut **conn)
            .await;
        match released {
            Ok(_) => {
                // Блокировка снята: соединение можно вернуть в пул.
                drop(self.conn.take());
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "could not release the llm slot check lock; closing its connection"
                );
                // Соединение закрывается здесь же: `Drop` не должен добавить
                // второе предупреждение о том же соединении.
                if let Some(mut conn) = self.conn.take() {
                    conn.close_on_drop();
                }
            }
        }
    }
}

impl Drop for CheckLock {
    fn drop(&mut self) {
        if let Some(mut conn) = self.conn.take() {
            tracing::warn!("llm slot check lock connection was dropped without unlock; closing it");
            conn.close_on_drop();
        }
    }
}

/// Прогон и запись под уже взятой блокировкой.
async fn run_locked(
    pool: &PgPool,
    prepared: &PreparedCheck,
    requested: Option<TitleMatch>,
    progress: &(impl Fn(usize, usize) + Sync),
) -> Result<CheckResult, CheckError> {
    let mode = match requested {
        Some(titles) => titles,
        None => {
            let mut conn = pool.acquire().await.map_err(CheckError::Database)?;
            let stored = llm_slot_settings::titles(&mut conn, prepared.slot_name)
                .await
                .map_err(CheckError::Standard)?;
            drop(conn);
            match TitleMatch::from_code(&stored) {
                Some(titles) => titles,
                None => {
                    tracing::warn!(
                        slot = prepared.slot_name,
                        titles = %stored,
                        "unknown llm slot title standard; treating it as strict"
                    );
                    TitleMatch::Strict
                }
            }
        }
    };

    // Прогон один: вызовы слота платные и недетерминированные, поэтому оба
    // режима судят одни и те же исходы.
    let runs = collect_runs_with(prepared.backend.as_ref(), &prepared.set, progress)
        .await
        .map_err(CheckError::Set)?;
    let report = evaluate_with(&prepared.set, &runs, prepared.slot.timeout, mode)
        .map_err(CheckError::Set)?;
    let other_titles = other_titles(mode);
    let other = evaluate_with(&prepared.set, &runs, prepared.slot.timeout, other_titles).ok();

    let set_hash = set_hash(&prepared.set_text);
    let contract_hash = prompt::contract_hash();
    let fingerprint = fingerprint(&prepared.slot, &contract_hash, &set_hash, mode);
    let saved = save(
        pool,
        prepared,
        &report,
        &fingerprint,
        requested.is_some(),
        mode,
    )
    .await;

    Ok(CheckResult {
        slot_name: prepared.slot_name,
        model: prepared.slot.model.clone(),
        fingerprint,
        report,
        other,
        other_titles,
        saved,
    })
}

/// Второй режим сравнения названий: по нему тот же прогон оценивается
/// справочно.
fn other_titles(titles: TitleMatch) -> TitleMatch {
    match titles {
        TitleMatch::Strict => TitleMatch::Normalized,
        TitleMatch::Normalized => TitleMatch::Strict,
    }
}

/// Записать проверку и стандарт одной транзакцией.
///
/// Иначе смена стандарта без новой проверки оставила бы слот «устарел» без
/// объяснения. Ошибка возвращается вызывающему: отчёт уже готов и печатается.
async fn save(
    pool: &PgPool,
    prepared: &PreparedCheck,
    report: &Report,
    fingerprint: &SlotFingerprint,
    save_standard: bool,
    mode: TitleMatch,
) -> Result<(), SaveError> {
    let report_json = report.to_json();
    let parts_json = fingerprint.parts.to_json();
    let mut conn = pool.acquire().await.map_err(SaveError::Check)?;
    let mut tx = conn.begin().await.map_err(SaveError::Check)?;
    llm_slot_checks::upsert(
        &mut tx,
        NewCheck {
            slot: prepared.slot_name,
            fingerprint: &fingerprint.value,
            fingerprint_parts: &parts_json,
            passed: report.passed_count(),
            total: report.total(),
            p95_ms: u64::try_from(report.p95().as_millis()).unwrap_or(u64::MAX),
            report: &report_json,
        },
        Utc::now(),
    )
    .await
    .map_err(SaveError::Check)?;
    if save_standard {
        llm_slot_settings::set_titles(&mut tx, prepared.slot_name, mode.as_str())
            .await
            .map_err(SaveError::Standard)?;
    }
    tx.commit().await.map_err(SaveError::Check)?;
    drop(conn);

    Ok(())
}

/// Части отпечатка слота; сам отпечаток — SHA-256 этих частей.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FingerprintParts {
    /// Вид слота; сейчас только `openai`.
    pub kind: &'static str,
    /// Имя модели.
    pub model: String,
    /// Хост адреса без пути, учётных данных и query.
    pub host: String,
    /// SHA-256 шаблона промпта и схемы контракта.
    pub contract_hash: String,
    /// SHA-256 содержимого файла smoke-набора.
    pub set_hash: String,
    /// Таймаут запроса слота, миллисекунды.
    pub timeout_ms: u64,
    /// Стандарт сравнения названий: часть проверки, как модель или таймаут.
    pub titles: TitleMatch,
}

impl FingerprintParts {
    /// Части в виде JSON для `llm_slot_checks.fingerprint_parts`.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("fingerprint parts serialize")
    }
}

/// Отпечаток слота и его части.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotFingerprint {
    /// SHA-256 канонической записи частей.
    pub value: String,
    /// Части, из которых посчитан отпечаток.
    pub parts: FingerprintParts,
}

/// Отпечаток конфигурации слота, контракта, набора и стандарта сравнения.
///
/// Токен и квота намеренно не входят: смена токена не делает проверку
/// устаревшей, а квота относится к household, а не к слоту. Стандарт входит:
/// смена режима сравнения названий — другая проверка, как смена модели.
pub fn fingerprint(
    slot: &SlotConfig,
    contract_hash: &str,
    set_hash: &str,
    titles: TitleMatch,
) -> SlotFingerprint {
    let parts = FingerprintParts {
        kind: SLOT_KIND,
        model: slot.model.clone(),
        host: host_of(&slot.url),
        contract_hash: contract_hash.to_owned(),
        set_hash: set_hash.to_owned(),
        timeout_ms: u64::try_from(slot.timeout.as_millis()).unwrap_or(u64::MAX),
        titles,
    };
    let serialized = serde_json::to_vec(&parts).expect("fingerprint parts serialize");
    let mut digest = Sha256::new();
    digest.update(&serialized);
    SlotFingerprint {
        value: hex(digest.finalize()),
        parts,
    }
}

/// SHA-256 содержимого файла smoke-набора.
pub fn set_hash(text: &str) -> String {
    hex(Sha256::digest(text.as_bytes()))
}

/// Хост адреса: без схемы, пути, учётных данных, query и fragment. Порт
/// добавляется, если он задан явно.
fn host_of(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::Map;

    use super::*;
    use crate::config::SecretString;

    const CONTRACT: &str = "contract-hash";
    const SET: &str = "set-hash";

    fn slot() -> SlotConfig {
        SlotConfig {
            url: "https://api.example.test/v1?api-version=1"
                .parse()
                .expect("url"),
            model: "vendor/model".to_owned(),
            token: Some(SecretString::new("slot-token")),
            timeout: Duration::from_secs(10),
            extra: Map::new(),
        }
    }

    /// Отпечаток в строгом стандарте: большинство тестов не про режим.
    fn fp(slot: &SlotConfig, contract_hash: &str, set_hash: &str) -> SlotFingerprint {
        fingerprint(slot, contract_hash, set_hash, TitleMatch::Strict)
    }

    #[test]
    fn a_token_change_keeps_the_fingerprint() {
        let mut first = slot();
        let mut second = slot();
        second.token = Some(SecretString::new("another-token"));

        assert_eq!(
            fp(&first, CONTRACT, SET).value,
            fp(&second, CONTRACT, SET).value
        );

        first.token = None;
        assert_eq!(
            fp(&first, CONTRACT, SET).value,
            fp(&slot(), CONTRACT, SET).value
        );
    }

    #[test]
    fn a_model_change_changes_the_fingerprint() {
        let mut other = slot();
        other.model = "vendor/other".to_owned();

        assert_ne!(
            fp(&slot(), CONTRACT, SET).value,
            fp(&other, CONTRACT, SET).value
        );
    }

    #[test]
    fn path_credentials_and_query_do_not_change_the_fingerprint() {
        let mut other = slot();
        other.url = "https://user:password@api.example.test/other?token=secret"
            .parse()
            .expect("url");

        assert_eq!(
            fp(&slot(), CONTRACT, SET).value,
            fp(&other, CONTRACT, SET).value
        );
    }

    #[test]
    fn host_port_timeout_contract_and_set_are_part_of_the_fingerprint() {
        let base = fp(&slot(), CONTRACT, SET);

        let mut other_host = slot();
        other_host.url = "https://api.other.test/v1".parse().expect("url");
        assert_ne!(base.value, fp(&other_host, CONTRACT, SET).value, "host");

        let mut other_port = slot();
        other_port.url = "https://api.example.test:8443/v1".parse().expect("url");
        assert_ne!(base.value, fp(&other_port, CONTRACT, SET).value, "port");

        let mut other_timeout = slot();
        other_timeout.timeout = Duration::from_secs(20);
        assert_ne!(
            base.value,
            fp(&other_timeout, CONTRACT, SET).value,
            "timeout"
        );

        assert_ne!(
            base.value,
            fp(&slot(), "another-contract", SET).value,
            "contract hash"
        );
        assert_ne!(
            base.value,
            fp(&slot(), CONTRACT, "another-set").value,
            "set hash"
        );
    }

    #[test]
    fn a_titles_change_changes_the_fingerprint() {
        assert_ne!(
            fingerprint(&slot(), CONTRACT, SET, TitleMatch::Strict).value,
            fingerprint(&slot(), CONTRACT, SET, TitleMatch::Normalized).value,
            "the title standard is part of the check, like the model"
        );
    }

    #[test]
    fn fingerprint_parts_have_the_documented_shape_and_no_token() {
        let slot = slot();
        let value = fp(&slot, CONTRACT, SET);
        let parts = value.parts.to_json();

        assert_eq!(parts["kind"], "openai");
        assert_eq!(parts["model"], "vendor/model");
        assert_eq!(parts["host"], "api.example.test");
        assert_eq!(parts["contract_hash"], CONTRACT);
        assert_eq!(parts["set_hash"], SET);
        assert_eq!(parts["timeout_ms"], 10_000);
        assert_eq!(parts["titles"], "strict");
        assert_eq!(
            parts.as_object().expect("object").len(),
            7,
            "only the documented parts, no token or quota: {parts}"
        );
        assert!(
            !value.value.contains("slot-token"),
            "the fingerprint must not contain the token"
        );
        assert_eq!(value.value.len(), 64, "SHA-256 hex");
    }

    #[test]
    fn the_set_hash_covers_the_file_contents() {
        assert_eq!(set_hash("a"), set_hash("a"));
        assert_ne!(set_hash("a"), set_hash("b"));
        assert_eq!(set_hash("a").len(), 64);
        assert_ne!(set_hash(BUILTIN_SET), set_hash(&format!("{BUILTIN_SET}\n")));
    }

    #[test]
    fn a_set_for_another_contract_is_rejected_before_any_call() {
        let other = BUILTIN_SET.replacen("\"capture-v4\"", "\"capture-v3\"", 1);

        let error = load_set(&other).expect_err("another contract is an error");

        assert!(error.contains("capture-v3"), "{error}");
        assert!(error.contains(prompt::PROMPT_VERSION), "{error}");
        load_set(BUILTIN_SET).expect("the built-in set matches the contract");
    }
}
