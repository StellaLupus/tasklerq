//! Проверка слота smoke-набором: встроенный набор, прогон фраз и отпечаток.
//!
//! Команда `capture-eval` (9.3) берёт конфигурацию слота из окружения, гоняет
//! каждую фразу напрямую через бэкенд слота (без цепочки, перехода на резервный
//! слот и предохранителя) и судит ответ по таймауту слота ([`super::eval`]).
//! Ожидание одной фразы ограничено [`PHRASE_WAIT`]: верный, но медленный ответ
//! должен попасть в отчёт с фактическим временем, а не оборваться раньше
//! срока.
//!
//! Набор по умолчанию лежит рядом с шаблоном промпта и встроен в бинарник:
//! проверка идёт в контейнере, где исходников `tests/` нет. `--set PATH` —
//! переопределение для разработки.
//!
//! Отпечаток слота ([`fingerprint`]) переиспользуется статусом проверки в 9.4:
//! он сравнивает отпечаток действующей конфигурации с сохранённым. В отпечаток
//! входят вид слота, модель, хост адреса, хеш контракта, хеш набора и таймаут;
//! токен и квота не входят.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

use super::eval::{Report, SlotRun, SmokeSet, check_prompt_version, evaluate};
use super::prompt;
use crate::config::SlotConfig;
use crate::llm::{ChatBackend, ChatRequest};

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
    let mut runs = BTreeMap::new();
    for phrase in &set.phrases {
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
    }
    Ok(runs)
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

/// Отпечаток конфигурации слота, контракта и набора.
///
/// Токен и квота намеренно не входят: смена токена не делает проверку
/// устаревшей, а квота относится к household, а не к слоту.
pub fn fingerprint(slot: &SlotConfig, contract_hash: &str, set_hash: &str) -> SlotFingerprint {
    let parts = FingerprintParts {
        kind: SLOT_KIND,
        model: slot.model.clone(),
        host: host_of(&slot.url),
        contract_hash: contract_hash.to_owned(),
        set_hash: set_hash.to_owned(),
        timeout_ms: u64::try_from(slot.timeout.as_millis()).unwrap_or(u64::MAX),
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

    #[test]
    fn a_token_change_keeps_the_fingerprint() {
        let mut first = slot();
        let mut second = slot();
        second.token = Some(SecretString::new("another-token"));

        assert_eq!(
            fingerprint(&first, CONTRACT, SET).value,
            fingerprint(&second, CONTRACT, SET).value
        );

        first.token = None;
        assert_eq!(
            fingerprint(&first, CONTRACT, SET).value,
            fingerprint(&slot(), CONTRACT, SET).value
        );
    }

    #[test]
    fn a_model_change_changes_the_fingerprint() {
        let mut other = slot();
        other.model = "vendor/other".to_owned();

        assert_ne!(
            fingerprint(&slot(), CONTRACT, SET).value,
            fingerprint(&other, CONTRACT, SET).value
        );
    }

    #[test]
    fn path_credentials_and_query_do_not_change_the_fingerprint() {
        let mut other = slot();
        other.url = "https://user:password@api.example.test/other?token=secret"
            .parse()
            .expect("url");

        assert_eq!(
            fingerprint(&slot(), CONTRACT, SET).value,
            fingerprint(&other, CONTRACT, SET).value
        );
    }

    #[test]
    fn host_port_timeout_contract_and_set_are_part_of_the_fingerprint() {
        let base = fingerprint(&slot(), CONTRACT, SET);

        let mut other_host = slot();
        other_host.url = "https://api.other.test/v1".parse().expect("url");
        assert_ne!(
            base.value,
            fingerprint(&other_host, CONTRACT, SET).value,
            "host"
        );

        let mut other_port = slot();
        other_port.url = "https://api.example.test:8443/v1".parse().expect("url");
        assert_ne!(
            base.value,
            fingerprint(&other_port, CONTRACT, SET).value,
            "port"
        );

        let mut other_timeout = slot();
        other_timeout.timeout = Duration::from_secs(20);
        assert_ne!(
            base.value,
            fingerprint(&other_timeout, CONTRACT, SET).value,
            "timeout"
        );

        assert_ne!(
            base.value,
            fingerprint(&slot(), "another-contract", SET).value,
            "contract hash"
        );
        assert_ne!(
            base.value,
            fingerprint(&slot(), CONTRACT, "another-set").value,
            "set hash"
        );
    }

    #[test]
    fn fingerprint_parts_have_the_documented_shape_and_no_token() {
        let slot = slot();
        let value = fingerprint(&slot, CONTRACT, SET);
        let parts = value.parts.to_json();

        assert_eq!(parts["kind"], "openai");
        assert_eq!(parts["model"], "vendor/model");
        assert_eq!(parts["host"], "api.example.test");
        assert_eq!(parts["contract_hash"], CONTRACT);
        assert_eq!(parts["set_hash"], SET);
        assert_eq!(parts["timeout_ms"], 10_000);
        assert_eq!(
            parts.as_object().expect("object").len(),
            6,
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
