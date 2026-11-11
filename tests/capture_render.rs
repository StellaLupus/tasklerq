//! Команда `capture-render`: JSONL из stdin, запрос `capture-v4` в stdout.
//!
//! Тесты запускают настоящий бинарник без базы: окружение очищено, поэтому
//! `DATABASE_URL` и прочие переменные сервиса недоступны. Совпадение с боевым
//! запросом проверяется на локальном HTTP-сервере, который ловит тело
//! `chat/completions` от [`OpenAiBackend`] через [`Chain`].

use std::process::{Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use serde_json::{Map, Value, json};
use tasklerq::capture::context::{CaptureContext, ListRef, MemberRef};
use tasklerq::capture::prompt;
use tasklerq::domain::models::ListKind;
use tasklerq::domain::recurrence::parse_timezone;
use tasklerq::llm::{Chain, ChatRequest, OpenAiBackend, ResponseFailure, Slot, SystemClock};
use tokio::process::Command;
use uuid::Uuid;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tasklerq"))
}

/// Запустить `capture-render` с очищенным окружением и заданным stdin.
async fn render(input: &str) -> Output {
    use tokio::io::AsyncWriteExt;

    let mut child = bin()
        .arg("capture-render")
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the CLI should start");
    let mut stdin = child.stdin.take().expect("stdin");
    stdin
        .write_all(input.as_bytes())
        .await
        .expect("input is written");
    drop(stdin);
    child
        .wait_with_output()
        .await
        .expect("the CLI should finish")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("utf8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("utf8")
}

/// Описание контекста для команды: момент с поясом, IANA-пояс household,
/// списки всех видов, участники и автор.
fn input_context() -> Value {
    json!({
        "now": "2026-10-30T11:05:00+03:00",
        "timezone": "Europe/Moscow",
        "lists": [
            {"name": "Задачи", "kind": "tasks", "default": true},
            {"name": "Покупки", "kind": "shopping", "default": true},
            {"name": "Уборка", "kind": "checklist"},
            {"name": "Календарь", "kind": "events", "default": true},
        ],
        "members": ["Анна", "Пётр"],
        "author": "Анна",
    })
}

/// Тот же контекст, что и [`input_context`], в виде [`CaptureContext`]: по нему
/// собирается «боевой» запрос.
fn capture_context() -> CaptureContext {
    let member = |name: &str| MemberRef {
        id: Uuid::nil(),
        display_name: name.to_owned(),
        username: None,
    };
    let list = |name: &str, kind: ListKind, is_default: bool| ListRef {
        id: Uuid::nil(),
        name: name.to_owned(),
        kind,
        is_default,
    };
    CaptureContext {
        now: Utc.with_ymd_and_hms(2026, 10, 30, 8, 5, 0).unwrap(),
        timezone: parse_timezone("Europe/Moscow").expect("timezone"),
        lists: vec![
            list("Задачи", ListKind::Tasks, true),
            list("Покупки", ListKind::Shopping, true),
            list("Уборка", ListKind::Checklist, false),
            list("Календарь", ListKind::Events, true),
        ],
        members: vec![member("Анна"), member("Пётр")],
        author: Some(member("Анна")),
    }
}

fn record(id: Value, text: &str) -> Value {
    json!({"id": id, "context": input_context(), "text": text})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_records_are_rendered_with_the_contract_version_and_hash() {
    let records = [
        record(json!("первый"), "купить молоко"),
        record(json!(7), "позвонить маме в 18:00"),
        record(json!("третий"), "маникюр завтра с 14 до 15:30"),
    ];
    let input = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    let output = render(&input).await;
    assert!(output.status.success(), "{output:?}");

    let lines = stdout(&output);
    let lines = lines
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("output is JSONL"))
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 3, "one output record per input record");

    for (record, line) in records.iter().zip(&lines) {
        assert_eq!(line["id"], record["id"], "the id is returned as is");
        assert_eq!(line["prompt_version"], "capture-v4");
        assert_eq!(line["contract_hash"], prompt::contract_hash());
        assert_eq!(
            line["contract_hash"].as_str().map(str::len),
            Some(64),
            "the contract hash is a SHA-256 hex string"
        );
        assert!(line["messages"].is_array(), "{line}");
        assert!(line["schema"].is_object(), "{line}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_lines_stop_the_command_with_a_line_number_and_no_phrase_text() {
    const PHRASE: &str = "секретная фраза про молоко и хлеб";

    let good = record(json!("good"), "купить хлеб").to_string();
    let cases = [
        (
            "не JSON",
            json!({"id": "bad-json", "text": PHRASE}).to_string(),
        ),
        (
            "нет поля context",
            json!({"id": "bad-missing", "text": PHRASE}).to_string(),
        ),
        (
            "неизвестный вид списка",
            json!({
                "id": "bad-kind",
                "context": {
                    "now": "2026-10-30T11:05:00+03:00",
                    "timezone": "Europe/Moscow",
                    "lists": [{"name": "Задачи", "kind": "notes"}],
                },
                "text": PHRASE,
            })
            .to_string(),
        ),
        (
            "невалидный пояс",
            json!({
                "id": "bad-zone",
                "context": {
                    "now": "2026-10-30T11:05:00+03:00",
                    "timezone": "Mars/Olympus",
                    "lists": [],
                },
                "text": PHRASE,
            })
            .to_string(),
        ),
    ];

    for (case, bad) in cases {
        let input = format!("{good}\n{bad}\n");
        let output = render(&input).await;

        assert!(!output.status.success(), "{case}: {output:?}");
        let errors = stderr(&output);
        assert!(errors.contains("строка 2"), "{case}: {errors}");
        assert!(
            !errors.contains(PHRASE),
            "{case}: the phrase must not reach stderr: {errors}"
        );
        assert!(
            !errors.contains("купить хлеб"),
            "{case}: another phrase must not reach stderr: {errors}"
        );
        assert!(
            stdout(&output).lines().count() == 1,
            "{case}: the command stops after the first bad line"
        );
    }

    // Идентификатор ошибочной строки попадает в сообщение, если он есть.
    let input = format!("{}\n", json!({"id": "bad-id", "text": PHRASE}));
    let output = render(&input).await;
    assert!(!output.status.success(), "{output:?}");
    assert!(
        stderr(&output).contains("\"bad-id\""),
        "the id is reported: {}",
        stderr(&output)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn help_explains_the_input_format_and_the_stop_rule() {
    let output = bin()
        .args(["capture-render", "--help"])
        .env_clear()
        .output()
        .await
        .expect("the CLI should run");

    assert!(output.status.success(), "{output:?}");
    let help = stdout(&output);
    for expected in [
        "stdin",
        "messages",
        "schema",
        "prompt_version",
        "contract_hash",
        "останавливается",
    ] {
        assert!(help.contains(expected), "help lacks {expected}:\n{help}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rendered_messages_and_schema_match_the_backend_request() {
    let phrase = "завтра купить молоко 2 л и позвонить маме в 18:00";
    let input = format!("{}\n", record(json!(42), phrase));

    let output = render(&input).await;
    assert!(output.status.success(), "{output:?}");
    let rendered: Value = serde_json::from_str(stdout(&output).trim()).expect("output JSONL");
    assert_eq!(rendered["id"], 42);

    // Двойник бэкенда ловит ровно то тело, которое уходит в chat/completions.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "test/model",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "{\"items\":[],\"clarifications\":[]}",
                },
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1},
        })))
        .mount(&server)
        .await;

    let request = prompt::build_request(&capture_context(), phrase);
    let chat = ChatRequest {
        messages: &request.messages,
        schema_name: "capture_v4",
        schema: &request.schema,
    };
    let backend = OpenAiBackend::new(
        &format!("{}/v1", server.uri()),
        "test/model",
        None,
        Duration::from_millis(500),
        Map::new(),
    )
    .expect("backend");
    let chain = Chain::new(
        Slot::new("primary", Arc::new(backend), Duration::from_secs(5)),
        None,
    );

    let outcome = chain
        .run(
            &chat,
            Duration::from_secs(5),
            Utc::now(),
            &SystemClock::new(),
            |content: &str| Ok::<String, ResponseFailure<()>>(content.to_owned()),
        )
        .await;
    assert!(outcome.result.is_ok(), "{:?}", outcome.result);

    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).expect("request body");

    assert_eq!(
        serde_json::to_string(&rendered["messages"]).expect("messages"),
        serde_json::to_string(&body["messages"]).expect("body messages"),
        "messages must be byte-identical to the production request"
    );
    assert_eq!(
        serde_json::to_string(&rendered["schema"]).expect("schema"),
        serde_json::to_string(&body["response_format"]["json_schema"]["schema"])
            .expect("body schema"),
        "schema must be byte-identical to the production request"
    );
}
