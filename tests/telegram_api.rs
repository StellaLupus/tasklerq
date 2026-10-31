//! Контракт `HttpTelegramApi` с Bot API на mock-сервере.

use serde_json::json;
use tasklerq::config::SecretString;
use tasklerq::telegram::api::{HttpTelegramApi, RichMessage, TelegramApi, TelegramError};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "123456:SECRET-bot-token";

fn client(server: &MockServer) -> HttpTelegramApi {
    HttpTelegramApi::new(SecretString::new(TOKEN), &server.uri()).expect("client")
}

fn rich(html: &str) -> RichMessage {
    RichMessage {
        html: html.to_owned(),
    }
}

#[tokio::test]
async fn send_rich_message_posts_html_and_returns_message_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/sendRichMessage")))
        .and(body_json(
            json!({"chat_id": 42, "rich_message": {"html": "<h3>Привет</h3>"}}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {"message_id": 77, "chat": {"id": 42}}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let id = client(&server)
        .send_rich_message(42, &rich("<h3>Привет</h3>"))
        .await
        .expect("sent");

    assert_eq!(id, 77);
}

#[tokio::test]
async fn edit_rich_and_plain_messages_use_matching_methods() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/editMessageText")))
        .and(body_json(json!({
            "chat_id": 42,
            "message_id": 7,
            "rich_message": {"html": "<p>x</p>"}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": true})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/sendMessage")))
        .and(body_json(
            json!({"chat_id": 42, "text": "a &amp; b", "parse_mode": "HTML"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "result": {"message_id": 8}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/editMessageText")))
        .and(body_json(json!({
            "chat_id": 42,
            "message_id": 8,
            "text": "новый текст",
            "parse_mode": "HTML"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": true})))
        .expect(1)
        .mount(&server)
        .await;

    let api = client(&server);
    api.edit_rich_message(42, 7, &rich("<p>x</p>"))
        .await
        .expect("edited");
    assert_eq!(api.send_message(42, "a &amp; b").await.expect("sent"), 8);
    api.edit_message_text(42, 8, "новый текст")
        .await
        .expect("edited text");
}

#[tokio::test]
async fn answer_callback_and_delete_send_expected_payloads() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/answerCallbackQuery")))
        .and(body_json(json!({
            "callback_query_id": "cb1", "text": "Готово", "show_alert": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": true})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/deleteMessage")))
        .and(body_json(json!({"chat_id": 42, "message_id": 5})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": true})))
        .expect(1)
        .mount(&server)
        .await;

    let api = client(&server);
    api.answer_callback_query("cb1", Some("Готово"), false)
        .await
        .expect("answered");
    api.delete_message(42, 5).await.expect("deleted");
}

#[tokio::test]
async fn webhook_is_registered_with_secret_and_inspected() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/setWebhook")))
        .and(body_json(json!({
            "url": "https://example.test/webhooks/telegram",
            "secret_token": "s3cret",
            "allowed_updates": ["message", "callback_query"],
            "drop_pending_updates": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": true})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/getWebhookInfo")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {"url": "https://example.test/webhooks/telegram", "pending_update_count": 3}
        })))
        .mount(&server)
        .await;

    let api = client(&server);
    api.set_webhook("https://example.test/webhooks/telegram", "s3cret")
        .await
        .expect("registered");
    let info = api.get_webhook_info().await.expect("info");
    assert_eq!(info.pending_update_count, 3);
    assert_eq!(info.url, "https://example.test/webhooks/telegram");
}

#[tokio::test]
async fn too_many_requests_carries_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "ok": false,
            "error_code": 429,
            "description": "Too Many Requests: retry after 5",
            "parameters": {"retry_after": 5}
        })))
        .mount(&server)
        .await;

    let error = client(&server)
        .send_rich_message(1, &rich("x"))
        .await
        .unwrap_err();

    assert_eq!(error, TelegramError::RetryAfter(5));
    assert!(error.is_retryable());
}

#[tokio::test]
async fn edit_errors_are_classified() {
    let cases = [
        (
            400,
            "Bad Request: message is not modified: specified new message content",
            TelegramError::NotModified,
        ),
        (
            400,
            "Bad Request: message to edit not found",
            TelegramError::MessageGone,
        ),
        (
            400,
            "Bad Request: message can't be edited",
            TelegramError::MessageGone,
        ),
    ];
    for (code, description, expected) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(code).set_body_json(json!({
                "ok": false, "error_code": code, "description": description
            })))
            .mount(&server)
            .await;

        let error = client(&server)
            .edit_rich_message(1, 2, &rich("x"))
            .await
            .unwrap_err();
        assert_eq!(error, expected, "{description}");
    }
}

#[tokio::test]
async fn forbidden_bad_request_and_server_errors_are_distinguished() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/sendMessage")))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "ok": false, "error_code": 403, "description": "Forbidden: bot was blocked by the user"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/sendRichMessage")))
        .respond_with(ResponseTemplate::new(502).set_body_string("<html>Bad Gateway</html>"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/deleteMessage")))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "ok": false, "error_code": 400, "description": "Bad Request: chat not found"
        })))
        .mount(&server)
        .await;

    let api = client(&server);
    assert!(matches!(
        api.send_message(1, "x").await,
        Err(TelegramError::Forbidden(_))
    ));
    let gateway = api.send_rich_message(1, &rich("x")).await.unwrap_err();
    assert!(matches!(gateway, TelegramError::Transient(_)));
    assert!(gateway.is_retryable());
    let bad = api.delete_message(1, 1).await.unwrap_err();
    assert!(matches!(bad, TelegramError::BadRequest(_)));
    assert!(!bad.is_retryable());
}

#[tokio::test]
async fn unreachable_server_never_leaks_the_bot_token() {
    let api = HttpTelegramApi::new(SecretString::new(TOKEN), "http://127.0.0.1:9").expect("client");

    let error = api.send_message(1, "x").await.unwrap_err();

    let rendered = format!("{error} {error:?}");
    assert!(matches!(error, TelegramError::Transient(_)));
    assert!(!rendered.contains("SECRET-bot-token"), "{rendered}");
}

#[tokio::test]
async fn get_updates_returns_raw_updates() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/getUpdates")))
        .and(body_json(json!({
            "offset": 10, "timeout": 25, "allowed_updates": ["message", "callback_query"]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "result": [{"update_id": 10}, {"update_id": 11}]
        })))
        .mount(&server)
        .await;

    let updates = client(&server).get_updates(10, 25).await.expect("updates");

    assert_eq!(updates.len(), 2);
}
