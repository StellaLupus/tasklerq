//! Команда `capture-eval`: прогон smoke-набора через настоящий слот-бэкенд.
//!
//! Бэкенд — локальный HTTP-сервер `wiremock`, на который указывает
//! `LLM_PRIMARY_URL`; команда собирает боевой запрос и судит ответ по
//! таймауту слота. Каждый тест получает отдельную базу: строка проверки одна
//! на слот.

mod support;

use std::process::{Output, Stdio};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use sqlx::postgres::PgPoolOptions;
use tasklerq::capture::eval::TitleMatch;
use tasklerq::capture::prompt::{self, PROMPT_VERSION};
use tasklerq::capture::slot_check;
use tasklerq::capture::slot_status::{self, SlotStatus};
use tasklerq::config::{SecretKey, SecretString, SlotConfig};
use tasklerq::llm::OpenAiBackend;
use tasklerq::settings;
use tasklerq::settings::crypto::SecretCipher;
use tasklerq::storage::migrations;
use tasklerq::storage::repositories::llm_slot_checks;
use tasklerq::storage::repositories::llm_slot_settings;
use tokio::process::Command;
use uuid::Uuid;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct CliDatabase {
    url: String,
    pool: sqlx::PgPool,
}

/// База без миграций: соединение проходит, таблиц нет.
async fn empty_database() -> CliDatabase {
    let name = format!("cli_capture_eval_{}", Uuid::now_v7().simple());
    support::create_database(&name).await;
    let url = support::database_url_for(&name).await;
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("the CLI database should accept connections");
    CliDatabase { url, pool }
}

async fn cli_database() -> CliDatabase {
    let db = empty_database().await;
    migrations::run(&db.pool)
        .await
        .expect("migrations should apply");
    db
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tasklerq"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("utf8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("utf8")
}

/// Команда с конфигурацией основного слота и без резервного.
fn capture_eval(url: &str, server: &MockServer, timeout_ms: u64) -> Command {
    let mut command = bin();
    command
        .env("DATABASE_URL", url)
        .env("LLM_CAPTURE_ENABLED", "false")
        .env("LLM_PRIMARY_URL", format!("{}/v1", server.uri()))
        .env("LLM_PRIMARY_MODEL", "test/model")
        .env("LLM_PRIMARY_TIMEOUT_MS", timeout_ms.to_string())
        .env("RUST_LOG", "error")
        .env_remove("LLM_FALLBACK_URL")
        .env_remove("LLM_FALLBACK_MODEL")
        .env_remove("LLM_FALLBACK_TOKEN")
        .env_remove("LLM_FALLBACK_TIMEOUT_MS")
        .env_remove("LLM_FALLBACK_EXTRA_JSON");
    command
}

/// Команда без параметров слота в окружении: слот задан только в `settings`.
fn capture_eval_without_slot_env(url: &str) -> Command {
    let mut command = bin();
    command
        .env("DATABASE_URL", url)
        .env("LLM_CAPTURE_ENABLED", "false")
        .env("RUST_LOG", "error");
    for name in [
        "LLM_PRIMARY_URL",
        "LLM_PRIMARY_MODEL",
        "LLM_PRIMARY_TOKEN",
        "LLM_PRIMARY_TIMEOUT_MS",
        "LLM_PRIMARY_EXTRA_JSON",
        "LLM_FALLBACK_URL",
        "LLM_FALLBACK_MODEL",
        "LLM_FALLBACK_TOKEN",
        "LLM_FALLBACK_TIMEOUT_MS",
        "LLM_FALLBACK_EXTRA_JSON",
    ] {
        command.env_remove(name);
    }
    command
}

/// Набор из фраз `(id, text, ожидаемое название)`.
fn smoke_set(phrases: &[(&str, &str, &str)]) -> Value {
    json!({
        "prompt_version": PROMPT_VERSION,
        "frozen_at": "2026-11-11",
        "context": {
            "now": "2026-11-10T14:32:00+03:00",
            "timezone": "Europe/Moscow",
            "lists": [
                {"name": "Задачи", "kind": "tasks", "default": true}
            ],
            "members": ["Анна"],
            "author": "Анна"
        },
        "phrases": phrases
            .iter()
            .map(|(id, text, title)| json!({
                "id": id,
                "category": "simple",
                "text": text,
                "expect": support::llm::v4::response(vec![
                    support::llm::v4::task(title),
                ]),
            }))
            .collect::<Vec<_>>(),
    })
}

/// Записать набор в отдельный файл и вернуть путь и каталог для уборки.
fn write_set(name: &str, set: &Value) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "tasklerq-capture-eval-{name}-{}",
        Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir).expect("temporary directory");
    let path = dir.join("smoke.json");
    std::fs::write(&path, serde_json::to_vec(set).expect("set serializes"))
        .expect("smoke set file");
    (path, dir)
}

/// Ответ `chat/completions` с готовым текстом модели.
fn completion(answer: &Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "model": "test/model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": answer.to_string()},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5},
    }))
}

async fn mount_answer(server: &MockServer, text: &str, title: &str) {
    // Ищем фразу в блоке пользовательского текста: та же строка встречается в
    // примерах системного сообщения, и совпадение только по тексту выбрало бы
    // чужой ответ.
    let marker = format!("USER_TEXT\\n{text}\\nUSER_TEXT");
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains(marker))
        .respond_with(completion(&support::llm::v4::response(vec![
            support::llm::v4::task(title),
        ])))
        .mount(server)
        .await;
}

async fn slot_rows(
    pool: &sqlx::PgPool,
) -> Vec<(String, String, i32, i32, i32, Value, DateTime<Utc>)> {
    sqlx::query_as(
        "SELECT slot, fingerprint, passed, total, p95_ms, report, checked_at
         FROM llm_slot_checks ORDER BY slot",
    )
    .fetch_all(pool)
    .await
    .expect("slot checks")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_slot_standard_round_trips_through_the_settings_table() {
    let db = cli_database().await;
    let mut conn = db.pool.acquire().await.expect("connection");

    assert_eq!(
        llm_slot_settings::titles(&mut conn, "primary")
            .await
            .expect("standard"),
        "strict",
        "no row means the strict standard"
    );

    llm_slot_settings::set_titles(&mut conn, "primary", "normalized")
        .await
        .expect("the standard should be stored");
    assert_eq!(
        llm_slot_settings::titles(&mut conn, "primary")
            .await
            .expect("standard"),
        "normalized",
        "the stored standard must be read back"
    );
    assert_eq!(
        llm_slot_settings::titles(&mut conn, "fallback")
            .await
            .expect("standard"),
        "strict",
        "the standard is per slot"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_check_without_a_saved_standard_reads_as_strict() {
    let db = cli_database().await;
    sqlx::query(
        "INSERT INTO llm_slot_checks (
             slot, fingerprint, fingerprint_parts, passed, total, p95_ms, report,
             checked_at
         )
         VALUES ('primary', 'legacy', '{\"model\":\"m\"}'::jsonb, 1, 1, 1000,
                 '{}'::jsonb, now())",
    )
    .execute(&db.pool)
    .await
    .expect("a check written before the standard existed");

    let mut conn = db.pool.acquire().await.expect("connection");
    let check = llm_slot_checks::latest(&mut conn, "primary")
        .await
        .expect("the saved check should be read")
        .expect("the check exists");

    assert_eq!(
        check.titles, "strict",
        "a check without a saved standard was strict"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_title_standard_is_saved_and_decides_the_verdict() {
    let db = cli_database().await;
    let server = MockServer::start().await;
    // Модель отвечает несовершенным видом на совершенный эталон: строгое
    // сравнение видит разные основы, нормализованное — одну пару.
    mount_answer(&server, "заплатить за квартиру", "Платить за квартиру").await;
    let (set_path, dir) = write_set(
        "titles",
        &smoke_set(&[("standard", "заплатить за квартиру", "Заплатить за квартиру")]),
    );

    let run = |titles: Option<&str>| {
        let mut command = capture_eval(&db.url, &server, 10_000);
        command.args([
            "capture-eval",
            "--slot",
            "primary",
            "--set",
            set_path.to_str().expect("path"),
        ]);
        if let Some(titles) = titles {
            command.args(["--titles", titles]);
        }
        command.stdin(Stdio::null()).output()
    };

    // Нормализованный стандарт: фраза проходит, печатаются оба числа, стандарт
    // сохраняется за слотом и входит в отпечаток.
    let normalized = run(Some("normalized")).await.expect("the CLI should run");
    assert_eq!(normalized.status.code(), Some(0), "{normalized:?}");
    let text = stdout(&normalized);
    assert!(
        text.contains("Названия: normalized — 1 из 1 (стандарт), strict — 0 из 1 (справочно)"),
        "both counts are printed from one run:\n{text}"
    );
    assert!(text.contains("Слот прошёл проверку"), "{text}");

    // Второй режим судит тот же прогон: слот вызван один раз на фразу.
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        1,
        "both standards must be judged from one slot call"
    );

    let mut conn = db.pool.acquire().await.expect("connection");
    assert_eq!(
        llm_slot_settings::titles(&mut conn, "primary")
            .await
            .expect("standard"),
        "normalized",
        "the flag must save the standard"
    );
    let parts_titles: String = sqlx::query_scalar(
        "SELECT fingerprint_parts->>'titles' FROM llm_slot_checks WHERE slot = 'primary'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("fingerprint parts");
    assert_eq!(
        parts_titles, "normalized",
        "the standard is part of the check"
    );

    // Без флага действует сохранённый стандарт.
    let default_run = run(None).await.expect("the CLI should run");
    assert_eq!(default_run.status.code(), Some(0), "{default_run:?}");
    let text = stdout(&default_run);
    assert!(
        text.contains("Названия: normalized — 1 из 1 (стандарт)"),
        "the saved standard is used without the flag:\n{text}"
    );

    // Строгий стандарт: та же фраза не проходит, стандарт перезаписан.
    let strict = run(Some("strict")).await.expect("the CLI should run");
    assert_eq!(strict.status.code(), Some(1), "{strict:?}");
    let text = stdout(&strict);
    assert!(
        text.contains("Названия: strict — 0 из 1 (стандарт), normalized — 1 из 1 (справочно)"),
        "{text}"
    );
    assert_eq!(
        llm_slot_settings::titles(&mut conn, "primary")
            .await
            .expect("standard"),
        "strict",
        "the strict flag must save the strict standard"
    );

    // Смена стандарта делает сохранённую проверку устаревшей.
    let check = llm_slot_checks::latest(&mut conn, "primary")
        .await
        .expect("the saved check")
        .expect("the check exists");
    assert_eq!(check.titles, "strict");
    llm_slot_settings::set_titles(&mut conn, "primary", "normalized")
        .await
        .expect("standard");
    let set_text = std::fs::read_to_string(&set_path).expect("the smoke set");
    let slot = SlotConfig {
        url: format!("{}/v1", server.uri()).parse().expect("url"),
        model: "test/model".to_owned(),
        token: Some(SecretString::new("slot-token")),
        timeout: Duration::from_secs(10),
        extra: Map::new(),
    };
    assert_eq!(
        slot_status::status(
            &slot,
            Some(&check),
            &prompt::contract_hash(),
            &slot_check::set_hash(&set_text),
            TitleMatch::Normalized,
        ),
        SlotStatus::Stale,
        "a strict check must not pass as a normalized one"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_phrases_passed_records_a_fingerprint_and_updates_one_row() {
    let db = cli_database().await;
    let server = MockServer::start().await;
    mount_answer(&server, "полить цветы", "Полить цветы").await;
    mount_answer(&server, "помыть посуду", "Помыть посуду").await;
    let (set_path, dir) = write_set(
        "pass",
        &smoke_set(&[
            ("first", "полить цветы", "Полить цветы"),
            ("second", "помыть посуду", "Помыть посуду"),
        ]),
    );

    let first = capture_eval(&db.url, &server, 10_000)
        .args([
            "capture-eval",
            "--slot",
            "primary",
            "--set",
            set_path.to_str().expect("path"),
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .expect("the CLI should run");

    assert_eq!(first.status.code(), Some(0), "{first:?}");
    let text = stdout(&first);
    assert!(text.contains("пройдено 2 из 2"), "{text}");
    assert!(text.contains("Слот прошёл проверку"), "{text}");
    assert!(
        text.contains("first (простые задачи)") && text.contains("second (простые задачи)"),
        "the report lists every phrase with its time:\n{text}"
    );
    assert!(!text.contains("timeout"), "{text}");

    let rows = slot_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "one row per slot");
    let (slot, fingerprint, passed, total, p95_ms, report, _) = &rows[0];
    assert_eq!(slot, "primary");
    assert_eq!(*passed, 2);
    assert_eq!(*total, 2);
    assert!(
        *p95_ms < 20_000,
        "the local server answers within the per-phrase wait: {p95_ms}"
    );
    assert_eq!(fingerprint.len(), 64, "SHA-256 hex");
    assert_eq!(report["passed"], 2);
    assert_eq!(report["phrases"].as_array().expect("phrases").len(), 2);
    assert!(
        !report.to_string().contains("Полить цветы"),
        "the report must not keep the model answer: {report}"
    );

    // Повторный прогон заменяет ту же строку, а не добавляет вторую.
    let second = capture_eval(&db.url, &server, 10_000)
        .args([
            "capture-eval",
            "--slot",
            "primary",
            "--set",
            set_path.to_str().expect("path"),
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .expect("the CLI should run");
    assert_eq!(second.status.code(), Some(0), "{second:?}");

    let rows = slot_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "a second run updates the slot row");
    assert_eq!(
        &rows[0].1, fingerprint,
        "the same configuration gives the same fingerprint"
    );

    let (parts_model, parts_kind, parts_host): (String, String, String) = sqlx::query_as(
        "SELECT fingerprint_parts->>'model', fingerprint_parts->>'kind',
                fingerprint_parts->>'host'
         FROM llm_slot_checks WHERE slot = 'primary'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("fingerprint parts");
    assert_eq!(parts_model, "test/model");
    assert_eq!(parts_kind, "openai");
    assert!(
        parts_host.starts_with("127.0.0.1"),
        "the host is stored without path or credentials: {parts_host}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_correct_but_slow_answer_fails_with_timeout_and_its_actual_time() {
    let db = cli_database().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("USER_TEXT\\nполить цветы\\nUSER_TEXT"))
        .respond_with(
            completion(&support::llm::v4::response(vec![support::llm::v4::task(
                "Полить цветы",
            )]))
            .set_delay(Duration::from_millis(1_200)),
        )
        .mount(&server)
        .await;
    let (set_path, dir) = write_set(
        "slow",
        &smoke_set(&[("slow", "полить цветы", "Полить цветы")]),
    );

    let output = capture_eval(&db.url, &server, 1_000)
        .args([
            "capture-eval",
            "--slot",
            "primary",
            "--set",
            set_path.to_str().expect("path"),
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .expect("the CLI should run");

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let text = stdout(&output);
    let line = text
        .lines()
        .find(|line| line.starts_with("slow (простые задачи): "))
        .unwrap_or_else(|| panic!("the phrase is missing from the report:\n{text}"));
    assert!(
        line.contains("с — timeout"),
        "the phrase fails with timeout and its actual time: {line}"
    );
    assert!(text.contains("Слот не прошёл проверку"), "{text}");

    let rows = slot_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let (_, _, passed, total, _, report, _) = &rows[0];
    assert_eq!(*passed, 0);
    assert_eq!(*total, 1);
    assert_eq!(report["phrases"][0]["reason"]["kind"], "timeout");
    assert!(
        report["phrases"][0]["reason"]["actual_ms"]
            .as_u64()
            .expect("actual_ms")
            >= 1_200
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_report_is_printed_even_when_the_result_cannot_be_saved() {
    // База есть, но миграции не применены: соединение проходит, запись — нет.
    let name = format!("cli_capture_eval_noschema_{}", Uuid::now_v7().simple());
    support::create_database(&name).await;
    let url = support::database_url_for(&name).await;
    let server = MockServer::start().await;
    mount_answer(&server, "полить цветы", "Полить цветы").await;
    let (set_path, dir) = write_set(
        "noschema",
        &smoke_set(&[("first", "полить цветы", "Полить цветы")]),
    );

    let output = capture_eval(&url, &server, 10_000)
        .args([
            "capture-eval",
            "--slot",
            "primary",
            "--titles",
            "strict",
            "--set",
            set_path.to_str().expect("path"),
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .expect("the CLI should run");

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let text = stdout(&output);
    assert!(
        text.contains("Слот прошёл проверку"),
        "the report is printed before saving:\n{text}"
    );
    assert!(
        stderr(&output).contains("failed to save the slot check"),
        "{}",
        stderr(&output)
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_standard_table_fails_before_calling_the_llm() {
    // База есть, но миграций нет: стандарт не прочитать, платные вызовы не нужны.
    let name = format!("cli_capture_eval_nostandard_{}", Uuid::now_v7().simple());
    support::create_database(&name).await;
    let url = support::database_url_for(&name).await;
    let server = MockServer::start().await;
    mount_answer(&server, "полить цветы", "Полить цветы").await;
    let (set_path, dir) = write_set(
        "nostandard",
        &smoke_set(&[("first", "полить цветы", "Полить цветы")]),
    );

    let output = capture_eval(&url, &server, 10_000)
        .args([
            "capture-eval",
            "--slot",
            "primary",
            "--set",
            set_path.to_str().expect("path"),
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .expect("the CLI should run");

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        stderr(&output).contains("failed to read the slot standard"),
        "{}",
        stderr(&output)
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty(),
        "a missing standard must not cost a paid slot call"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unconfigured_fallback_slot_fails_without_calling_the_llm() {
    let server = MockServer::start().await;

    let output = bin()
        .env(
            "DATABASE_URL",
            "postgres://tasklerq:tasklerq@127.0.0.1:1/tasklerq",
        )
        .env("LLM_CAPTURE_ENABLED", "true")
        .env("LLM_PRIMARY_URL", format!("{}/v1", server.uri()))
        .env("LLM_PRIMARY_MODEL", "test/model")
        .env("RUST_LOG", "error")
        .env_remove("LLM_FALLBACK_URL")
        .env_remove("LLM_FALLBACK_MODEL")
        .env_remove("LLM_FALLBACK_TOKEN")
        .env_remove("LLM_FALLBACK_TIMEOUT_MS")
        .env_remove("LLM_FALLBACK_EXTRA_JSON")
        .args(["capture-eval", "--slot", "fallback"])
        .stdin(Stdio::null())
        .output()
        .await
        .expect("the CLI should run");

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        stderr(&output).contains("слот fallback не настроен"),
        "{}",
        stderr(&output)
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty(),
        "an unconfigured slot must not call the LLM"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_set_for_another_contract_fails_before_calling_the_llm() {
    let server = MockServer::start().await;
    let mut set = smoke_set(&[("first", "полить цветы", "Полить цветы")]);
    set["prompt_version"] = json!("capture-v3");
    let (set_path, dir) = write_set("version", &set);

    let output = bin()
        .env(
            "DATABASE_URL",
            "postgres://tasklerq:tasklerq@127.0.0.1:1/tasklerq",
        )
        .env("LLM_CAPTURE_ENABLED", "true")
        .env("LLM_PRIMARY_URL", format!("{}/v1", server.uri()))
        .env("LLM_PRIMARY_MODEL", "test/model")
        .env("RUST_LOG", "error")
        .args([
            "capture-eval",
            "--slot",
            "primary",
            "--set",
            set_path.to_str().expect("path"),
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .expect("the CLI should run");

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let message = stderr(&output);
    assert!(message.contains("capture-v3"), "{message}");
    assert!(message.contains(PROMPT_VERSION), "{message}");
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty(),
        "another contract must not reach the LLM"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slot_configured_only_in_settings_is_checked_by_the_command() {
    let db = cli_database().await;
    let server = MockServer::start().await;
    mount_answer(&server, "полить цветы", "Полить цветы").await;
    let (set_path, dir) = write_set(
        "settings-slot",
        &smoke_set(&[("first", "полить цветы", "Полить цветы")]),
    );

    // Слот перенесён из `.env` в консоль: в окружении команды `LLM_*` нет, а
    // адрес, модель и токен лежат в `settings`. Токен шифруется ключом, который
    // команда получает из `TASKLERQ_SECRET_KEY`.
    let key = [7u8; 32];
    let secret_key = BASE64.encode(key);
    let cipher = SecretCipher::new(
        Some(SecretKey::parse(&secret_key).expect("32 bytes are a key")),
        None,
    );
    let mut conn = db.pool.acquire().await.expect("connection");
    settings::write_value(
        &mut conn,
        "llm.primary.url",
        json!(format!("{}/v1", server.uri())),
    )
    .await
    .expect("the slot url is written");
    settings::write_value(&mut conn, "llm.primary.model", json!("db/model"))
        .await
        .expect("the slot model is written");
    settings::write_secret(
        &mut conn,
        "llm.primary.token",
        &SecretString::new("db-token"),
        &cipher,
    )
    .await
    .expect("the slot token is written");
    drop(conn);

    let output = capture_eval_without_slot_env(&db.url)
        .env("TASKLERQ_SECRET_KEY", &secret_key)
        .args([
            "capture-eval",
            "--slot",
            "primary",
            "--set",
            set_path.to_str().expect("path"),
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .expect("the CLI should run");

    assert_eq!(output.status.code(), Some(0), "{output:?}");

    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1, "the phrase is checked once");
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer db-token"),
        "the token is decrypted from settings"
    );
    let body: Value = serde_json::from_slice(&requests[0].body).expect("request body");
    assert_eq!(body["model"], "db/model", "the model comes from settings");

    let model: String = sqlx::query_scalar(
        "SELECT fingerprint_parts->>'model' FROM llm_slot_checks WHERE slot = 'primary'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("the check is saved with the slot fingerprint");
    assert_eq!(model, "db/model");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_run_while_a_check_is_running_fails_without_calling_the_llm() {
    let db = cli_database().await;
    let server = MockServer::start().await;
    mount_answer(&server, "полить цветы", "Полить цветы").await;
    let (set_path, dir) = write_set(
        "busy",
        &smoke_set(&[("first", "полить цветы", "Полить цветы")]),
    );

    // Блокировку держит другой прогон, как идущая проверка из консоли.
    let mut conn = db.pool.acquire().await.expect("connection");
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(slot_check::CHECK_LOCK_KEY)
        .fetch_one(&mut *conn)
        .await
        .expect("the test takes the check lock");
    assert!(acquired);

    let run = || {
        capture_eval(&db.url, &server, 10_000)
            .args([
                "capture-eval",
                "--slot",
                "primary",
                "--set",
                set_path.to_str().expect("path"),
            ])
            .stdin(Stdio::null())
            .output()
    };

    let busy = run().await.expect("the CLI should run");
    assert_eq!(busy.status.code(), Some(2), "{busy:?}");
    assert!(
        stderr(&busy).contains("проверка уже идёт"),
        "{}",
        stderr(&busy)
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty(),
        "a concurrent check must not reach the LLM"
    );
    assert!(
        slot_rows(&db.pool).await.is_empty(),
        "a concurrent check must not write a result"
    );

    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(slot_check::CHECK_LOCK_KEY)
        .execute(&mut *conn)
        .await
        .expect("the test releases the check lock");
    drop(conn);

    // Блокировка не залипает: после снятия та же команда проходит.
    let retry = run().await.expect("the CLI should run");
    assert_eq!(retry.status.code(), Some(0), "{retry:?}");
    assert!(
        stdout(&retry).contains("Слот прошёл проверку"),
        "{}",
        stdout(&retry)
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_lock_is_released_when_the_result_cannot_be_saved() {
    // База без миграций: прогон проходит, запись результата — нет.
    let db = empty_database().await;
    let server = MockServer::start().await;
    mount_answer(&server, "полить цветы", "Полить цветы").await;
    let (set_path, dir) = write_set(
        "lock-release",
        &smoke_set(&[("first", "полить цветы", "Полить цветы")]),
    );

    let set_text = std::fs::read_to_string(&set_path).expect("the smoke set");
    let set = slot_check::load_set(&set_text).expect("the set matches the contract");
    let slot = SlotConfig {
        url: format!("{}/v1", server.uri()).parse().expect("url"),
        model: "test/model".to_owned(),
        token: None,
        timeout: Duration::from_secs(10),
        extra: Map::new(),
    };
    let backend = OpenAiBackend::from_slot(&slot, Duration::from_secs(5)).expect("backend");
    let prepared = slot_check::PreparedCheck {
        slot_name: "primary",
        slot,
        backend: Box::new(backend),
        set,
        set_text,
    };

    let result = slot_check::run_check(&db.pool, prepared, Some(TitleMatch::Strict))
        .await
        .expect("the report is ready even when the save fails");
    assert!(
        result.saved.is_err(),
        "a database without migrations cannot store the check"
    );

    let mut conn = db.pool.acquire().await.expect("connection");
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(slot_check::CHECK_LOCK_KEY)
        .fetch_one(&mut *conn)
        .await
        .expect("try the check lock");
    assert!(
        acquired,
        "the check lock must be released after a failed save"
    );
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(slot_check::CHECK_LOCK_KEY)
        .execute(&mut *conn)
        .await
        .expect("unlock");

    std::fs::remove_dir_all(&dir).ok();
}
