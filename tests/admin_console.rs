//! Консоль администратора: отдельный listener, роутер и определение клиента.

mod support;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use support::telegram::{FakeTelegram, Harness};
use tasklerq::admin_console::assets;
use tasklerq::admin_console::change::{self, ChangeOutcome};
use tasklerq::admin_console::codes::{self, Purpose};
use tasklerq::admin_console::login;
use tasklerq::admin_console::password::{self, Password};
use tasklerq::admin_console::router::{AdminState, build_router as build_admin_router};
use tasklerq::admin_console::session::AdminSession;
use tasklerq::admin_console::setup::{self, SetupOutcome};
use tasklerq::admin_console::wizard::WizardSettings;
use tasklerq::app::{Application, build_application};
use tasklerq::auth::tokens::hash_token;
use tasklerq::capture::eval::SmokeSet;
use tasklerq::capture::prompt;
use tasklerq::capture::slot_check;
use tasklerq::capture::slot_status::LlmSlotHealth;
use tasklerq::config::{
    CaptureConfig, Config, IpNet, LlmConfig, SecretKey, SecretString, TenancyConfig, TenancyMode,
};
use tasklerq::domain::error::DomainError;
use tasklerq::http::api::capture::CaptureState;
use tasklerq::http::client::{Client, Scheme};
use tasklerq::http::router::{AppState, build_router};
use tasklerq::http::server::{bind_listeners, serve_listeners};
use tasklerq::llm::{LlmChange, LlmHandle, LlmSettings, LlmSnapshot};
use tasklerq::rate_limit::{Reservation, ip_key};
use tasklerq::security_audit::Audit;
use tasklerq::settings;
use tasklerq::settings::crypto::SecretCipher;
use tasklerq::storage::repositories::admin_accounts;
use tasklerq::storage::repositories::households;
use tasklerq::storage::repositories::llm_slot_checks::{self, NewCheck};
use tasklerq::telegram::bot::Bot;
use tasklerq::telegram::outbox::{self, Outgoing};
use tokio::sync::watch;
use tower::ServiceExt;
use url::form_urlencoded;
use uuid::Uuid;
use wiremock::matchers::{body_string_contains, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const REQUEST_ID: &str = "test-request-id";

/// Пароль с хешем [`TEST_PASSWORD_HASH`]; хеш посчитан боевыми параметрами
/// argon2id один раз, чтобы тесты входа не платили за настройку учётки.
const TEST_PASSWORD: &str = "correct horse battery staple";
const TEST_PASSWORD_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$BwcHBwcHBwcHBwcHBwcHBw$\
    eZ8SueF3EIJEgtgpg1rLafWpNVv3dMTwc0KCOxG5CSg";

async fn test_config(admin_bind_addr: &str) -> Config {
    let database_url = support::database_url().await;
    Config::from_map([
        ("ENV", "development"),
        ("PUBLIC_BASE_URL", "http://127.0.0.1:18080"),
        ("DATABASE_URL", database_url.as_str()),
        ("TELEGRAM_MODE", "disabled"),
        ("LLM_CAPTURE_ENABLED", "false"),
        ("BIND_ADDR", "127.0.0.1:0"),
        ("ADMIN_BIND_ADDR", admin_bind_addr),
    ])
    .expect("test config should be valid")
}

async fn send_from(
    app: &Router,
    method: Method,
    path: &str,
    peer: SocketAddr,
    headers: HeaderMap,
    body: Body,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("x-request-id", REQUEST_ID);
    for (name, value) in &headers {
        builder = builder.header(name, value);
    }
    let mut request = builder.body(body).expect("request should build");
    // Роутер консоли читает адрес пира так же, как боевой listener через
    // `ConnectInfo`; в тесте его подставляет обвязка `oneshot`.
    request.extensions_mut().insert(ConnectInfo(peer));

    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("router should respond");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body should be readable")
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

async fn send(app: &Router, method: Method, path: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    send_from(
        app,
        method,
        path,
        test_peer(),
        HeaderMap::new(),
        Body::empty(),
    )
    .await
}

/// Пир по умолчанию: loopback, как запрос с самого хоста.
fn test_peer() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 4_040))
}

/// POST обычной формы без JavaScript.
async fn send_form(
    app: &Router,
    path: &str,
    fields: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Vec<u8>) {
    send_form_from(app, path, test_peer(), HeaderMap::new(), fields).await
}

/// POST формы с заголовками браузера своей страницы: `Host` и совпадающий с
/// ним `Origin`. Явно переданные значения не перекрываются.
async fn send_form_from(
    app: &Router,
    path: &str,
    peer: SocketAddr,
    extra_headers: HeaderMap,
    fields: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let headers = with_same_origin(extra_headers);
    send_form_raw(app, path, peer, headers, fields).await
}

/// POST формы без подставленного origin — для проверок самого отказа.
async fn send_form_raw(
    app: &Router,
    path: &str,
    peer: SocketAddr,
    headers: HeaderMap,
    fields: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let body = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish();
    let mut headers = headers;
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    send_from(app, Method::POST, path, peer, headers, Body::from(body)).await
}

/// Заголовки `Host` и `Origin` одной и той же страницы, как их шлёт браузер.
///
/// Без них изменяющий запрос отклоняется ещё до обработчика.
fn with_same_origin(mut headers: HeaderMap) -> HeaderMap {
    headers
        .entry(header::HOST)
        .or_insert(HeaderValue::from_static("nas:18081"));
    headers
        .entry(header::ORIGIN)
        .or_insert(HeaderValue::from_static("http://nas:18081"));
    headers
}

/// Заголовки из пар; повторяющиеся имена сохраняются.
fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.append(
            HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            HeaderValue::from_str(value).expect("header value"),
        );
    }
    headers
}

/// Тег `<input>` с заданным `name`.
fn input_with_name<'a>(html: &'a str, name: &str) -> &'a str {
    let needle = format!("name=\"{name}\"");
    let position = html
        .find(&needle)
        .unwrap_or_else(|| panic!("no input named {name}: {html}"));
    let start = html[..position]
        .rfind("<input")
        .unwrap_or_else(|| panic!("no input tag around {name}: {html}"));
    let end = html[position..]
        .find('>')
        .map(|offset| position + offset)
        .unwrap_or_else(|| panic!("unclosed input named {name}: {html}"));
    &html[start..=end]
}

/// Запрос с cookie сессии.
async fn send_with_cookie(
    app: &Router,
    method: Method,
    path: &str,
    cookie: &str,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut headers = with_same_origin(HeaderMap::new());
    headers.insert(
        header::COOKIE,
        HeaderValue::from_str(cookie).expect("cookie header"),
    );
    send_from(app, method, path, test_peer(), headers, Body::empty()).await
}

/// Настройки LLM для тестов, которым разбор не нужен.
fn empty_llm_settings(pool: &PgPool) -> LlmSettings {
    LlmSettings::new(
        pool.clone(),
        LlmConfig::empty(),
        SecretCipher::new(None, None),
        LlmHandle::empty(),
    )
}

/// Состояние консоли без настроек разбора.
fn console_state(pool: &PgPool, trusted_proxies: Vec<IpNet>) -> AdminState {
    AdminState::new(
        pool.clone(),
        trusted_proxies,
        Audit::new(pool.clone()),
        empty_llm_settings(pool),
    )
}

fn console(pool: &PgPool, trusted_proxies: Vec<IpNet>) -> Router {
    build_admin_router(console_state(pool, trusted_proxies))
}

/// Консоль с явным режимом, признаком Telegram и параметрами LLM.
fn console_with(pool: &PgPool, tenancy: TenancyMode, telegram: bool, llm: LlmConfig) -> Router {
    console_with_llm_and_mode(pool, tenancy, telegram, llm, SecretCipher::new(None, None)).0
}

/// Ключ шифрования из повторяющегося байта.
fn secret_key(byte: u8) -> SecretKey {
    SecretKey::parse(&BASE64.encode([byte; 32])).expect("32 bytes are a key")
}

fn cipher_with_key(byte: u8) -> SecretCipher {
    SecretCipher::new(Some(secret_key(byte)), None)
}

/// Параметры LLM из окружения для тестов консоли.
fn llm_env(pairs: &[(&str, &str)]) -> LlmConfig {
    let mut vars: Vec<(String, String)> = [
        ("ENV", "development"),
        ("PUBLIC_BASE_URL", "http://127.0.0.1:18080"),
        (
            "DATABASE_URL",
            "postgres://tasklerq:secret@127.0.0.1:5432/tasklerq",
        ),
        ("TELEGRAM_MODE", "disabled"),
        ("LLM_CAPTURE_ENABLED", "false"),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value.to_owned()))
    .collect();
    vars.extend(
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    );
    Config::from_map(vars)
        .expect("the test environment is valid")
        .llm
}

/// Консоль с тем же экземпляром настроек LLM, что у рантайма.
fn console_with_llm(pool: &PgPool, llm: LlmConfig, cipher: SecretCipher) -> (Router, LlmSettings) {
    console_with_llm_and_mode(pool, TenancyMode::Single, false, llm, cipher)
}

/// Консоль с явным режимом, Telegram и тем же экземпляром настроек LLM.
fn console_with_llm_and_mode(
    pool: &PgPool,
    tenancy: TenancyMode,
    telegram: bool,
    llm: LlmConfig,
    cipher: SecretCipher,
) -> (Router, LlmSettings) {
    // Как в `build_application`: снимок стартует из окружения, базы и умолчаний.
    let handle = LlmHandle::new(LlmSnapshot::from_effective(settings::effective_llm(
        &llm,
        &[],
        &cipher,
    )));
    let settings = LlmSettings::new(pool.clone(), llm, cipher, handle);
    let app = build_admin_router(
        AdminState::new(
            pool.clone(),
            Vec::new(),
            Audit::new(pool.clone()),
            settings.clone(),
        )
        .with_wizard(WizardSettings::new(tenancy, telegram)),
    );
    (app, settings)
}

/// Встроенный набор проверки слота.
fn builtin_smoke_set() -> SmokeSet {
    SmokeSet::parse(slot_check::BUILTIN_SET).expect("the built-in smoke set parses")
}

/// Ответ `chat/completions` с готовым ответом модели.
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

/// Ответить на каждую фразу встроенного набора; `slow` — задержка одной фразы.
async fn mount_smoke_answers(server: &MockServer, delay: Duration, slow: Option<(&str, Duration)>) {
    for phrase in builtin_smoke_set().phrases {
        let marker = format!("USER_TEXT\\n{}\\nUSER_TEXT", phrase.text);
        let mut response = completion(&phrase.expect);
        if let Some((slow_id, slow_delay)) = slow
            && slow_id == phrase.id
        {
            response = response.set_delay(slow_delay);
        } else if !delay.is_zero() {
            response = response.set_delay(delay);
        }
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_string_contains(marker))
            .respond_with(response)
            .mount(server)
            .await;
    }
}

/// Консоль с основным слотом на подменном сервере.
fn console_with_server(
    pool: &PgPool,
    server: &MockServer,
    timeout_ms: u64,
) -> (Router, LlmSettings) {
    let url = format!("{}/v1", server.uri());
    let timeout = timeout_ms.to_string();
    let llm = llm_env(&[
        ("LLM_CAPTURE_ENABLED", "true"),
        ("LLM_PRIMARY_URL", &url),
        ("LLM_PRIMARY_MODEL", "test/model"),
        ("LLM_PRIMARY_TIMEOUT_MS", &timeout),
    ]);
    console_with_llm(pool, llm, SecretCipher::new(None, None))
}

/// POST кнопки проверки слота.
async fn start_check(
    app: &Router,
    cookie: &str,
    slot: &str,
    titles: &str,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    send_form_from(
        app,
        &format!("/admin/llm/check/{slot}"),
        test_peer(),
        headers_with(&[("cookie", cookie)]),
        &[("titles", titles)],
    )
    .await
}

/// Ждать отчёт проверки на странице хода.
async fn wait_for_check_report(app: &Router, cookie: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let (status, headers, body) =
            send_with_cookie(app, Method::GET, "/admin/llm/check", cookie).await;
        assert_eq!(status, StatusCode::OK);
        assert_security_headers(&headers, "GET /admin/llm/check");
        let text = String::from_utf8_lossy(&body).into_owned();
        if text.contains("Задержка p95") {
            return text;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the check never finished: {text}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// URL изолированной базы теста для запуска команды.
async fn command_database_url(pool: &PgPool) -> String {
    let name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(pool)
        .await
        .expect("the database name");
    support::database_url_for(&name).await
}

/// Отпечаток сохранённой проверки слота.
async fn saved_fingerprint(pool: &PgPool, slot: &str) -> String {
    sqlx::query_scalar("SELECT fingerprint FROM llm_slot_checks WHERE slot = $1")
        .bind(slot)
        .fetch_one(pool)
        .await
        .expect("the saved check")
}

/// Время сохранения проверки слота.
async fn saved_checked_at(pool: &PgPool, slot: &str) -> DateTime<Utc> {
    sqlx::query_scalar("SELECT checked_at FROM llm_slot_checks WHERE slot = $1")
        .bind(slot)
        .fetch_one(pool)
        .await
        .expect("the saved check time")
}

/// Записать проверку слота с текущим отпечатком конфигурации.
async fn store_current_check(pool: &PgPool, slot: &str, config: &tasklerq::config::SlotConfig) {
    let fingerprint = slot_check::fingerprint(
        config,
        &prompt::contract_hash(),
        &slot_check::set_hash(slot_check::BUILTIN_SET),
        tasklerq::capture::eval::TitleMatch::Strict,
    );
    let parts = fingerprint.parts.to_json();
    let report = json!({"total": 22, "passed": 22});
    let mut conn = pool.acquire().await.expect("connection");
    llm_slot_checks::upsert(
        &mut conn,
        NewCheck {
            slot,
            fingerprint: &fingerprint.value,
            fingerprint_parts: &parts,
            passed: 22,
            total: 22,
            p95_ms: 1_000,
            report: &report,
        },
        Utc::now(),
    )
    .await
    .expect("the slot check is stored");
}

/// Число строк `settings` с указанными ключами.
async fn setting_rows(pool: &PgPool, keys: &[&str]) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM settings WHERE key = ANY($1)")
        .bind(keys)
        .fetch_one(pool)
        .await
        .expect("setting rows")
}

/// Значение настройки инсталляции из `settings`.
async fn setting_value(pool: &PgPool, key: &str) -> Option<Value> {
    sqlx::query_scalar(
        "SELECT value FROM settings
         WHERE scope = 'installation' AND household_id IS NULL AND key = $1",
    )
    .bind(key)
    .fetch_optional(pool)
    .await
    .expect("setting value")
}

/// Household, созданный до входа администратора.
async fn insert_household(pool: &PgPool) {
    let mut tx = pool.begin().await.expect("household transaction");
    households::create_empty(
        &mut tx,
        TenancyMode::Single,
        "Дом",
        "Europe/Moscow",
        &tasklerq::admin::admin_actor(),
        Utc::now(),
    )
    .await
    .expect("household is created");
    tx.commit().await.expect("commit the household");
}

fn location(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
}

fn set_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Пара `имя=значение` из `Set-Cookie` для следующего запроса.
fn cookie_pair(set_cookie: &str) -> String {
    set_cookie
        .split(';')
        .next()
        .expect("a set-cookie value")
        .to_owned()
}

async fn insert_account(pool: &PgPool, username: &str) {
    sqlx::query(
        "INSERT INTO admin_accounts (id, username, password_hash)
         VALUES ($1, $2, $3)",
    )
    .bind(Uuid::now_v7())
    .bind(username)
    .bind(TEST_PASSWORD_HASH)
    .execute(pool)
    .await
    .expect("administrator account");
}

async fn insert_session(
    pool: &PgPool,
    token: &str,
    created_at: DateTime<Utc>,
    last_seen_at: DateTime<Utc>,
) -> Uuid {
    assert!(
        last_seen_at >= created_at,
        "the schema requires last_seen_at >= created_at"
    );
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO admin_sessions (id, token_hash, created_at, last_seen_at)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(hash_token(token))
    .bind(created_at)
    .bind(last_seen_at)
    .execute(pool)
    .await
    .expect("session row");
    id
}

async fn account_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM admin_accounts")
        .fetch_one(pool)
        .await
        .expect("account count")
}

async fn session_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM admin_sessions")
        .fetch_one(pool)
        .await
        .expect("session count")
}

/// Payload событий аудита одного типа в порядке записи.
async fn audit_payloads(pool: &PgPool, event_type: &str) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT payload FROM audit_events
         WHERE event_type::text = $1
         ORDER BY created_at, id",
    )
    .bind(event_type)
    .fetch_all(pool)
    .await
    .expect("audit payloads")
}

/// Актор и household событий аудита одного типа.
async fn audit_actor(pool: &PgPool, event_type: &str) -> Vec<(String, Option<Uuid>)> {
    sqlx::query_as(
        "SELECT actor_type::text, household_id FROM audit_events
         WHERE event_type::text = $1
         ORDER BY created_at, id",
    )
    .bind(event_type)
    .fetch_all(pool)
    .await
    .expect("audit actors")
}

/// Время с точностью до микросекунды: `timestamptz` хранит именно её.
fn precise_now() -> DateTime<Utc> {
    DateTime::from_timestamp_micros(Utc::now().timestamp_micros()).expect("a valid timestamp")
}

fn assert_security_headers(headers: &HeaderMap, context: &str) {
    assert_eq!(
        headers
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("no-store"),
        "Cache-Control on {context}"
    );
    assert_frame_headers(headers, context);
}

/// Запрет встраивания во фреймы: общий для HTML и встроенных ресурсов.
fn assert_frame_headers(headers: &HeaderMap, context: &str) {
    assert_eq!(
        headers
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|value| value.to_str().ok()),
        Some("frame-ancestors 'none'"),
        "Content-Security-Policy on {context}"
    );
    assert_eq!(
        headers
            .get(header::X_FRAME_OPTIONS)
            .and_then(|value| value.to_str().ok()),
        Some("DENY"),
        "X-Frame-Options on {context}"
    );
}

/// Заголовки, которые не меняются от запроса к запросу.
///
/// `x-request-id` фиксирован в запросе, поэтому сравнивается вместе с телом:
/// в теле ошибки тот же идентификатор.
fn stable_headers(headers: &HeaderMap) -> BTreeMap<String, Vec<Vec<u8>>> {
    let mut stable = BTreeMap::new();
    for (name, value) in headers {
        if name == header::DATE {
            continue;
        }
        stable
            .entry(name.as_str().to_owned())
            .or_insert_with(Vec::new)
            .push(value.as_bytes().to_vec());
    }
    stable
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_router_does_not_serve_console_paths() {
    let db = support::db().await;
    let app = build_router(AppState::new(db.pool.clone(), Audit::new(db.pool.clone())));

    let (status, headers, body) = send(&app, Method::GET, "/no-such-path").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let unknown_headers = stable_headers(&headers);

    for (method, path) in [
        (Method::GET, "/admin"),
        (Method::GET, "/admin/login"),
        (Method::GET, "/admin/setup"),
        (Method::GET, "/admin/recover/x"),
        (Method::GET, "/admin/password"),
        (Method::POST, "/admin/login"),
    ] {
        let (console_status, console_headers, console_body) =
            send(&app, method.clone(), path).await;
        assert_eq!(
            console_status,
            StatusCode::NOT_FOUND,
            "{method} {path} must look like an unknown path"
        );
        assert_eq!(
            stable_headers(&console_headers),
            unknown_headers,
            "{method} {path} must return the same headers as an unknown path"
        );
        assert_eq!(
            console_body, body,
            "{method} {path} must return the same body as an unknown path"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn console_router_redirects_to_login_without_a_session() {
    let db = support::db().await;
    let app = console(&db.pool, Vec::new());

    let (status, headers, _) = send(&app, Method::GET, "/admin").await;

    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_security_headers(&headers, "GET /admin without a session");
    assert_eq!(
        headers
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok()),
        Some("/admin/login")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_application_exposes_the_console_router_only_when_enabled() {
    let db = support::db().await;

    let disabled = test_config("disabled").await;
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let application = build_application(&disabled, db.pool.clone(), shutdown_rx)
        .await
        .expect("application should build with the console disabled");
    assert!(
        application.admin_router.is_none(),
        "ADMIN_BIND_ADDR=disabled must not expose a console router"
    );
    stop_workers(shutdown_tx, application.workers).await;

    let enabled = test_config("127.0.0.1:0").await;
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let application = build_application(&enabled, db.pool.clone(), shutdown_rx)
        .await
        .expect("application should build with the console enabled");
    let router = application
        .admin_router
        .expect("an explicit ADMIN_BIND_ADDR must expose a console router");

    let (status, headers, _) = send(&router, Method::GET, "/admin").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok()),
        Some("/admin/login")
    );

    stop_workers(shutdown_tx, application.workers).await;
}

async fn stop_workers(
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    workers: Vec<tokio::task::JoinHandle<()>>,
) {
    shutdown_tx.send(true).expect("shutdown signal");
    for worker in workers {
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("worker must stop on shutdown")
            .expect("worker task must join");
    }
}

fn no_redirect_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("http client")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_console_does_not_open_a_listener_and_public_api_works() {
    let db = support::db().await;
    let config = test_config("disabled").await;

    let listeners = bind_listeners(&config)
        .await
        .expect("listeners should bind on free ports");
    assert!(
        listeners.admin.is_none(),
        "ADMIN_BIND_ADDR=disabled must not bind a console listener"
    );
    let public_address = listeners
        .public
        .local_addr()
        .expect("public listener address");

    let public = build_router(AppState::new(db.pool.clone(), Audit::new(db.pool.clone())));
    let server = spawn_server(listeners.public, public, None);

    let response = reqwest::get(format!("http://{public_address}/health/live"))
        .await
        .expect("public request should complete");
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enabled_console_serves_on_its_own_listener_only() {
    let db = support::db().await;
    let config = test_config("127.0.0.1:0").await;

    let listeners = bind_listeners(&config)
        .await
        .expect("listeners should bind on free ports");
    let public_address = listeners
        .public
        .local_addr()
        .expect("public listener address");
    let admin = listeners
        .admin
        .expect("an explicit ADMIN_BIND_ADDR must bind a console listener");
    let admin_address = admin.local_addr().expect("admin listener address");

    let public = build_router(AppState::new(db.pool.clone(), Audit::new(db.pool.clone())));
    let console = console(&db.pool, Vec::new());
    let server = spawn_server(listeners.public, public, Some((admin, console)));

    let client = no_redirect_client();
    let response = client
        .get(format!("http://{admin_address}/admin"))
        .send()
        .await
        .expect("console request should complete");
    assert_eq!(response.status(), reqwest::StatusCode::SEE_OTHER);
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok()),
        Some("/admin/login")
    );

    let response = client
        .get(format!("http://{public_address}/admin"))
        .send()
        .await
        .expect("public request should complete");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);

    server.stop().await;

    for address in [public_address, admin_address] {
        assert!(
            reqwest::get(format!("http://{address}/health/live"))
                .await
                .is_err(),
            "the listener at {address} must stop after the shared shutdown"
        );
    }
}

struct RunningServer {
    shutdown: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl RunningServer {
    async fn stop(self) {
        self.shutdown
            .send(())
            .expect("server should await shutdown");
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("server should stop promptly after the signal")
            .expect("server task should not panic")
            .expect("server should exit cleanly");
    }
}

fn spawn_server(
    public: tokio::net::TcpListener,
    public_router: Router,
    admin: Option<(tokio::net::TcpListener, Router)>,
) -> RunningServer {
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(serve_listeners(public, public_router, admin, async move {
        let _ = shutdown_rx.await;
    }));
    RunningServer {
        shutdown: shutdown_tx,
        task,
    }
}

/// Код хранится только хешем, а использование возвращает строку один раз.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_time_code_is_stored_as_a_hash_and_used_once() {
    let db = support::isolated_db().await;
    let now = Utc::now();

    let issued = codes::issue(&db.pool, Purpose::Setup, None, now)
        .await
        .expect("a setup code should be issued");
    assert!(
        issued.code.len() >= 22,
        "the code must carry at least 128 bits: {}",
        issued.code
    );
    assert_eq!(issued.record.purpose, "setup");
    assert!(
        issued.record.expires_at.is_none(),
        "a setup code lives until the next issue"
    );

    let raw_matches: i64 =
        sqlx::query_scalar("SELECT count(*) FROM admin_one_time_codes WHERE code_hash = $1")
            .bind(&issued.code)
            .fetch_one(&db.pool)
            .await
            .expect("code count");
    assert_eq!(raw_matches, 0, "the table must not contain the open code");

    let stored: Vec<String> = sqlx::query_scalar("SELECT code_hash FROM admin_one_time_codes")
        .fetch_all(&db.pool)
        .await
        .expect("stored hashes");
    assert_eq!(
        stored,
        vec![hash_token(&issued.code)],
        "only the SHA-256 hash is stored"
    );
    assert!(
        !format!("{issued:?}").contains(&issued.code),
        "Debug must not print the open code"
    );

    let mut conn = db.pool.acquire().await.expect("connection");
    let consumed = codes::use_code(&mut conn, Purpose::Setup, &issued.code, now)
        .await
        .expect("the first use must succeed");
    assert_eq!(consumed.id, issued.record.id);
    assert!(consumed.used_at.is_some(), "the code is marked as used");

    let second = codes::use_code(&mut conn, Purpose::Setup, &issued.code, now).await;
    assert!(
        matches!(second, Err(DomainError::NotFound)),
        "the second use must be indistinguishable from an unknown code"
    );
}

/// Истёкший, чужой по назначению и неизвестный код — «не найден».
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_and_foreign_codes_are_not_found() {
    let db = support::isolated_db().await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");

    let recovery = codes::issue(&db.pool, Purpose::Recovery, None, now)
        .await
        .expect("a recovery code should be issued");

    let wrong_purpose = codes::use_code(&mut conn, Purpose::Setup, &recovery.code, now).await;
    assert!(
        matches!(wrong_purpose, Err(DomainError::NotFound)),
        "a recovery code must not be accepted as a setup code"
    );
    codes::use_code(&mut conn, Purpose::Recovery, &recovery.code, now)
        .await
        .expect("the wrong purpose must not consume the recovery code");

    // CHECK `expires_at > created_at` требует created_at в прошлом.
    let expired_code = "expired-recovery-code";
    sqlx::query(
        "INSERT INTO admin_one_time_codes (id, purpose, code_hash, created_at, expires_at)
         VALUES ($1, 'recovery', $2, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(hash_token(expired_code))
    .bind(now - ChronoDuration::minutes(20))
    .bind(now - ChronoDuration::minutes(5))
    .execute(&db.pool)
    .await
    .expect("an expired code is storable");

    let expired = codes::use_code(&mut conn, Purpose::Recovery, expired_code, now).await;
    assert!(
        matches!(expired, Err(DomainError::NotFound)),
        "an expired code must be indistinguishable from an unknown code"
    );

    let unknown = codes::use_code(&mut conn, Purpose::Setup, "no-such-code", now).await;
    assert!(
        matches!(unknown, Err(DomainError::NotFound)),
        "an unknown code must not be distinguished"
    );
}

/// Новый выпуск гасит прежний код той же пары и не трогает чужое назначение.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issuing_a_code_revokes_the_previous_one_of_the_same_purpose() {
    let db = support::isolated_db().await;
    let now = Utc::now();

    let first = codes::issue(&db.pool, Purpose::Setup, None, now)
        .await
        .expect("the first setup code");
    let recovery = codes::issue(&db.pool, Purpose::Recovery, None, now)
        .await
        .expect("a recovery code");
    let second = codes::issue(&db.pool, Purpose::Setup, None, now)
        .await
        .expect("the second setup code");
    assert_ne!(first.code, second.code, "every issue makes a new code");

    let mut conn = db.pool.acquire().await.expect("connection");
    let revoked = codes::use_code(&mut conn, Purpose::Setup, &first.code, now).await;
    assert!(
        matches!(revoked, Err(DomainError::NotFound)),
        "the previous setup code must be revoked by the new issue"
    );

    codes::use_code(&mut conn, Purpose::Recovery, &recovery.code, now)
        .await
        .expect("a code of another purpose must stay active");
    codes::use_code(&mut conn, Purpose::Setup, &second.code, now)
        .await
        .expect("the newest code must work");
}

/// `recovery` живёт 15 минут, `setup` — без срока.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_code_lives_fifteen_minutes() {
    let db = support::isolated_db().await;
    let now = Utc::now();

    let setup = codes::issue(&db.pool, Purpose::Setup, None, now)
        .await
        .expect("setup code");
    assert!(setup.record.expires_at.is_none());

    let recovery = codes::issue(&db.pool, Purpose::Recovery, None, now)
        .await
        .expect("recovery code");
    let expires_at = recovery.record.expires_at.expect("recovery has an expiry");
    let ttl = (expires_at - now).num_seconds();
    assert!(
        (899..=900).contains(&ttl),
        "a recovery code must live 15 minutes, got {ttl} seconds"
    );
}

/// Настройка создаёт учётку и сессию; код с пробелами по краям принимается, а
/// повторный ввод того же кода отказывает и страницу больше не открывает.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_creates_the_account_and_the_code_is_single_use() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());
    let issued = codes::issue_setup_code(&db.pool, Utc::now())
        .await
        .expect("the setup code lookup succeeds")
        .expect("a setup code should be issued");
    // Код копируют из строки журнала: пробелы вокруг не должны мешать.
    let spaced_code = format!("  {}  ", issued.code);
    let password = "setup-password-123";

    let (status, headers, _) = send_form(
        &app,
        "/admin/setup",
        &[
            ("code", &spaced_code),
            ("username", " admin "),
            ("password", password),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/wizard"));
    let cookie = set_cookie(&headers).expect("the setup response sets a session cookie");
    assert!(cookie.starts_with("tlq_admin="), "{cookie}");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    assert!(cookie.contains("Path=/"), "{cookie}");

    let (status, _, _) = send_with_cookie(&app, Method::GET, "/admin", &cookie_pair(&cookie)).await;
    assert_eq!(status, StatusCode::OK, "the session opens the main page");

    let token = cookie_pair(&cookie)
        .strip_prefix("tlq_admin=")
        .expect("the session cookie value")
        .to_owned();
    let stored_hashes: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM admin_sessions")
        .fetch_all(&db.pool)
        .await
        .expect("session hashes");
    assert_eq!(
        stored_hashes,
        vec![hash_token(&token)],
        "only the SHA-256 hash of the session token is stored"
    );

    let (username, password_hash): (String, String) =
        sqlx::query_as("SELECT username, password_hash FROM admin_accounts")
            .fetch_one(&db.pool)
            .await
            .expect("the account row");
    assert_eq!(username, "admin", "the name is trimmed");
    assert_eq!(account_count(&db.pool).await, 1);
    assert!(
        password_hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
        "argon2id with the design parameters: {password_hash}"
    );
    assert!(
        !password_hash.contains(password),
        "the password itself must not be stored: {password_hash}"
    );

    let (status, repeat_headers, _) = send_form(
        &app,
        "/admin/setup",
        &[
            ("code", &issued.code),
            ("username", "second"),
            ("password", "another-password-123"),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "the setup page is gone once the account exists"
    );
    assert_eq!(location(&repeat_headers), Some("/admin/login"));
    assert_eq!(
        account_count(&db.pool).await,
        1,
        "no second account appears"
    );
    let username: String = sqlx::query_scalar("SELECT username FROM admin_accounts")
        .fetch_one(&db.pool)
        .await
        .expect("the account row");
    assert_eq!(username, "admin", "the existing account is unchanged");

    let (status, headers, _) = send(&app, Method::GET, "/admin/setup").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/login"));
}

/// Неверный код ничего не создаёт и не гасит действующий код.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_code_creates_nothing_and_keeps_the_valid_code() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());
    let issued = codes::issue(&db.pool, Purpose::Setup, None, Utc::now())
        .await
        .expect("a setup code should be issued");

    let (status, _, body) = send_form(
        &app,
        "/admin/setup",
        &[
            ("code", "no-such-code"),
            ("username", "admin"),
            ("password", "valid-password-12"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&body).contains("Код недействителен"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(account_count(&db.pool).await, 0, "no account is created");

    let (status, headers, _) = send_form(
        &app,
        "/admin/setup",
        &[
            ("code", &issued.code),
            ("username", "admin"),
            ("password", "valid-password-12"),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "the valid code still works after the failed attempt"
    );
    assert_eq!(location(&headers), Some("/admin/wizard"));
    assert_eq!(account_count(&db.pool).await, 1);
}

/// Существующая учётка не даёт создать вторую, а откат оставляет код целым.
///
/// Через HTTP эта ветка недостижима: форма сначала видит учётку и уводит на
/// вход. Здесь функция вызывается напрямую, как при параллельной настройке.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_existing_account_keeps_the_setup_code_unused() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let issued = codes::issue(&db.pool, Purpose::Setup, None, Utc::now())
        .await
        .expect("a setup code should be issued");

    let state = console_state(&db.pool, Vec::new());
    let result = setup::create_account(
        &state,
        &Client {
            ip: "127.0.0.1".parse().expect("ip"),
            scheme: Scheme::Http,
            host: Some("nas:18081".into()),
        },
        &issued.code,
        "second",
        Password::new("unused-password-12"),
        Utc::now(),
    )
    .await;
    assert!(
        matches!(result, Ok(SetupOutcome::AlreadyConfigured)),
        "a second account must be refused: {result:?}"
    );

    let used_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT used_at FROM admin_one_time_codes WHERE id = $1")
            .bind(issued.record.id)
            .fetch_one(&db.pool)
            .await
            .expect("the setup code row");
    assert!(used_at.is_none(), "the rollback must keep the code unused");
    assert_eq!(
        account_count(&db.pool).await,
        1,
        "the existing account is unchanged"
    );
}

/// Длина пароля проверяется до хеширования и не расходует код.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_password_length_is_checked_before_hashing() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());
    let issued = codes::issue(&db.pool, Purpose::Setup, None, Utc::now())
        .await
        .expect("a setup code should be issued");

    let too_long = "a".repeat(1025);
    for (password, expected) in [
        ("12345678", "12"),
        (&too_long, "1024"),
        // 11 кириллических букв — 11 символов, хотя байтов 22.
        ("парольпарол", "12"),
    ] {
        let (status, _, body) = send_form(
            &app,
            "/admin/setup",
            &[
                ("code", &issued.code),
                ("username", "admin"),
                ("password", password),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "a bad length is a form error");
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains(expected),
            "the message names {expected}: {text}"
        );
        assert_eq!(account_count(&db.pool).await, 0, "no account is created");
    }

    let (status, _, _) = send_form(
        &app,
        "/admin/setup",
        &[
            ("code", &issued.code),
            ("username", "admin"),
            ("password", "123456789012"),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "the same code still works and 12 characters pass"
    );
    assert_eq!(account_count(&db.pool).await, 1);
}

/// Пять неудач закрывают вход: шестая попытка — `429` без проверки пароля.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn five_failed_logins_turn_the_sixth_attempt_into_too_many_requests() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(&db.pool, Vec::new());

    for attempt in 0..5 {
        let (status, headers, _) = send_form(
            &app,
            "/admin/login",
            &[("username", "admin"), ("password", "wrong-password-123")],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "failure {attempt} is a form error");
        assert!(
            set_cookie(&headers).is_none(),
            "failure {attempt} must not set a cookie"
        );
    }

    // Пароль верный, но лимит уже исчерпан: проверка до argon2 не доходит,
    // поэтому сессии нет.
    let (status, headers, _) = send_form(
        &app,
        "/admin/login",
        &[("username", "admin"), ("password", TEST_PASSWORD)],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the sixth attempt must be refused before checking the password"
    );
    assert!(set_cookie(&headers).is_none());
    let retry_after: u64 = headers
        .get(header::RETRY_AFTER)
        .expect("a rate-limited response names the retry moment")
        .to_str()
        .expect("an ASCII Retry-After")
        .parse()
        .expect("Retry-After is a number of seconds");
    assert!(
        (1..=900).contains(&retry_after),
        "Retry-After stays inside the window: {retry_after}"
    );
    assert_security_headers(&headers, "a rate-limited login");
    assert_eq!(
        session_count(&db.pool).await,
        0,
        "a rate-limited attempt must not issue a session"
    );

    let limits = audit_payloads(&db.pool, "rate_limited").await;
    let limit = limits
        .iter()
        .find(|payload| payload["limit"] == "admin_login_per_15_minutes")
        .expect("the login limit is audited");
    assert_eq!(limit["ip"], "127.0.0.1");
    assert!(limit["retry_after"].as_u64().unwrap_or(0) >= 1);

    let failed = audit_payloads(&db.pool, "admin_login_failed").await;
    assert!(!failed.is_empty(), "failed logins are audited");
    assert!(
        failed
            .iter()
            .all(|payload| payload["reason"] == "bad_credentials"),
        "{failed:?}"
    );
}

/// Параллельные неудачи не обходят лимит: проверок не больше пяти.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_failures_do_not_bypass_the_login_limit() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(&db.pool, Vec::new());

    let mut tasks = Vec::new();
    for _ in 0..10 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            let (status, _, _) = send_form(
                &app,
                "/admin/login",
                &[("username", "admin"), ("password", "wrong-password-123")],
            )
            .await;
            status
        }));
    }

    let mut checked = 0;
    let mut limited = 0;
    for task in tasks {
        match task.await.expect("login task") {
            StatusCode::OK => checked += 1,
            StatusCode::TOO_MANY_REQUESTS => limited += 1,
            other => panic!("unexpected login status: {other}"),
        }
    }
    assert_eq!(checked, 5, "only five password checks may run");
    assert_eq!(limited, 5, "the rest is refused before argon2");
    assert_eq!(session_count(&db.pool).await, 0);
}

/// Успешные входы не расходуют лимит неудач.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_logins_do_not_spend_the_failure_limit() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(&db.pool, Vec::new());
    let fields = [("username", "admin"), ("password", TEST_PASSWORD)];

    for attempt in 0..4 {
        let (status, headers, _) = send_form(&app, "/admin/login", &fields).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "success {attempt}");
        assert!(
            set_cookie(&headers).is_some(),
            "success {attempt} sets a cookie"
        );
    }

    let (status, headers, _) = send_form(&app, "/admin/login", &fields).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "the fifth successful login still passes"
    );
    assert!(set_cookie(&headers).is_some());
    assert_eq!(session_count(&db.pool).await, 5);

    let logins = audit_payloads(&db.pool, "admin_login").await;
    assert_eq!(
        logins.len(),
        5,
        "every successful login leaves an audit event: {logins:?}"
    );
}

/// `X-Forwarded-For` учитывается только от доверенного прокси.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn x_forwarded_for_does_not_bypass_the_login_limit() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let fields = [("username", "admin"), ("password", "wrong-password-123")];

    // Пустой ADMIN_TRUSTED_PROXIES: адрес клиента — адрес соединения, смена
    // X-Forwarded-For не создаёт новый бакет.
    let untrusted = console(&db.pool, Vec::new());
    for attempt in 0..5 {
        let mut forwarded = HeaderMap::new();
        forwarded.insert(
            "x-forwarded-for",
            HeaderValue::from_str(&format!("203.0.113.{}", attempt + 1))
                .expect("a forwarded address"),
        );
        let (status, _, _) =
            send_form_from(&untrusted, "/admin/login", test_peer(), forwarded, &fields).await;
        assert_eq!(status, StatusCode::OK, "failure {attempt}");
    }
    let mut forwarded = HeaderMap::new();
    forwarded.insert("x-forwarded-for", HeaderValue::from_static("198.51.100.99"));
    let (status, _, _) =
        send_form_from(&untrusted, "/admin/login", test_peer(), forwarded, &fields).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "an ignored X-Forwarded-For must not reset the bucket"
    );

    // Доверенный прокси: разные X-Forwarded-For — разные клиенты.
    let trusted = console(
        &db.pool,
        vec![IpNet::parse("127.0.0.1").expect("a loopback network")],
    );
    for attempt in 0..5 {
        let mut forwarded = HeaderMap::new();
        forwarded.insert(
            "x-forwarded-for",
            HeaderValue::from_str(&format!("198.51.100.{}", attempt + 1))
                .expect("a forwarded address"),
        );
        let (status, _, _) =
            send_form_from(&trusted, "/admin/login", test_peer(), forwarded, &fields).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "client {attempt} has its own bucket"
        );
    }
    let mut forwarded = HeaderMap::new();
    forwarded.insert("x-forwarded-for", HeaderValue::from_static("198.51.100.99"));
    let (status, _, _) =
        send_form_from(&trusted, "/admin/login", test_peer(), forwarded, &fields).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a new forwarded client is not refused because of its neighbours"
    );
}

/// Setup-код и вход считают неудачи в разных бакетах.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_code_failures_do_not_block_a_later_login() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());
    let issued = codes::issue(&db.pool, Purpose::Setup, None, Utc::now())
        .await
        .expect("a setup code should be issued");
    let password = "valid-password-12";

    for attempt in 0..5 {
        let (status, _, body) = send_form(
            &app,
            "/admin/setup",
            &[
                ("code", "no-such-code"),
                ("username", "admin"),
                ("password", password),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "failure {attempt}");
        assert!(
            String::from_utf8_lossy(&body).contains("Код недействителен"),
            "{}",
            String::from_utf8_lossy(&body)
        );
    }

    // Шестая попытка отклоняется до argon2 и до проверки кода: даже верный
    // код не создаёт учётку.
    let (status, headers, _) = send_form(
        &app,
        "/admin/setup",
        &[
            ("code", &issued.code),
            ("username", "admin"),
            ("password", password),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_security_headers(&headers, "a rate-limited setup");
    assert_eq!(
        account_count(&db.pool).await,
        0,
        "the sixth request must not create an account"
    );

    let limits = audit_payloads(&db.pool, "rate_limited").await;
    assert!(
        limits
            .iter()
            .any(|payload| payload["limit"] == "admin_setup_code_per_15_minutes"),
        "{limits:?}"
    );

    // Вход не делит бакет с setup-кодом: учётка, созданная вне консоли, даёт
    // обычный вход с того же адреса.
    insert_account(&db.pool, "admin").await;
    let (status, headers, _) = send_form(
        &app,
        "/admin/login",
        &[("username", "admin"), ("password", TEST_PASSWORD)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(set_cookie(&headers).is_some());
}

/// Ошибка валидации на setup — не попытка кода и лимит не расходует.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_validation_errors_do_not_spend_the_code_limit() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());
    let issued = codes::issue(&db.pool, Purpose::Setup, None, Utc::now())
        .await
        .expect("a setup code should be issued");

    for attempt in 0..5 {
        let (status, _, body) = send_form(
            &app,
            "/admin/setup",
            &[
                ("code", &issued.code),
                ("username", "admin"),
                ("password", "short"),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "invalid input {attempt}");
        assert!(
            String::from_utf8_lossy(&body).contains("не менее 12"),
            "the length error is shown: {}",
            String::from_utf8_lossy(&body)
        );
    }

    let (status, headers, _) = send_form(
        &app,
        "/admin/setup",
        &[
            ("code", &issued.code),
            ("username", "admin"),
            ("password", "valid-password-12"),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "five invalid lengths must not spend the code limit"
    );
    assert!(set_cookie(&headers).is_some());
    assert_eq!(account_count(&db.pool).await, 1);
}

/// Все четыре бакета живут в состоянии и не делят счётчики.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_four_failure_buckets_are_independent() {
    use tasklerq::rate_limit::Reservation;

    fn fail(
        limiter: &std::sync::Arc<tasklerq::rate_limit::RateLimiter<std::net::IpAddr>>,
        ip: &std::net::IpAddr,
        now: DateTime<Utc>,
    ) {
        let Reservation::Allowed(guard) = limiter.reserve(ip, now) else {
            panic!("a failure inside the limit must reserve a slot");
        };
        guard.failed();
    }

    fn allowed(
        limiter: &std::sync::Arc<tasklerq::rate_limit::RateLimiter<std::net::IpAddr>>,
        ip: &std::net::IpAddr,
        now: DateTime<Utc>,
    ) -> bool {
        matches!(limiter.reserve(ip, now), Reservation::Allowed(_))
    }

    let db = support::db().await;
    let state = console_state(&db.pool, Vec::new());
    let ip: std::net::IpAddr = "203.0.113.5".parse().expect("an IP address");
    let now = Utc::now();

    for _ in 0..5 {
        fail(&state.setup_limiter(), &ip, now);
    }
    assert!(!allowed(&state.setup_limiter(), &ip, now));
    assert!(allowed(&state.login_limiter(), &ip, now));
    assert!(allowed(&state.current_password_limiter(), &ip, now));
    assert!(allowed(&state.recovery_limiter(), &ip, now));

    for _ in 0..5 {
        fail(&state.login_limiter(), &ip, now);
    }
    assert!(!allowed(&state.login_limiter(), &ip, now));
    assert!(
        allowed(&state.recovery_limiter(), &ip, now),
        "the recovery bucket stays untouched"
    );
}

/// События консоли пишутся с системным актором и без секретов.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn console_audit_events_are_written_without_secrets() {
    const SETUP_PASSWORD: &str = "setup-password-123";
    const HIDDEN_NAME: &str = "hidden-user-name";
    const HIDDEN_PASSWORD: &str = "hidden-password-987";

    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());
    let issued = codes::issue(&db.pool, Purpose::Setup, None, Utc::now())
        .await
        .expect("a setup code should be issued");

    let (status, _, _) = send_form(
        &app,
        "/admin/setup",
        &[
            ("code", "no-such-code"),
            ("username", HIDDEN_NAME),
            ("password", SETUP_PASSWORD),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, _) = send_form(
        &app,
        "/admin/setup",
        &[
            ("code", &issued.code),
            ("username", "admin"),
            ("password", SETUP_PASSWORD),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (status, _, _) = send_form(
        &app,
        "/admin/login",
        &[("username", HIDDEN_NAME), ("password", HIDDEN_PASSWORD)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, _) = send_form(
        &app,
        "/admin/login",
        &[("username", "admin"), ("password", SETUP_PASSWORD)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    for event_type in [
        "admin_setup_code_failed",
        "admin_account_created",
        "admin_login_failed",
        "admin_login",
    ] {
        let payloads = audit_payloads(&db.pool, event_type).await;
        assert!(!payloads.is_empty(), "event {event_type} is recorded");
        for (actor, household) in audit_actor(&db.pool, event_type).await {
            assert_eq!(actor, "system", "event {event_type} has the system actor");
            assert_eq!(
                household, None,
                "console events have no household: {event_type}"
            );
        }
        for payload in payloads {
            let text = payload.to_string();
            for secret in [
                SETUP_PASSWORD,
                HIDDEN_PASSWORD,
                issued.code.as_str(),
                HIDDEN_NAME,
            ] {
                assert!(
                    !text.contains(secret),
                    "{event_type} payload leaked {secret:?}: {text}"
                );
            }
        }
    }

    let created = audit_payloads(&db.pool, "admin_account_created").await;
    assert_eq!(
        created[0]["username"], "admin",
        "the created account name is allowed"
    );
    let failed = audit_payloads(&db.pool, "admin_login_failed").await;
    assert_eq!(failed[0]["reason"], "bad_credentials");
    assert_eq!(failed[0]["ip"], "127.0.0.1");
}

/// POST без `Host` или с мусором в нём не даёт определить origin запроса.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_post_without_a_valid_host_header_is_refused() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(&db.pool, Vec::new());

    // Нет Host; пустой порт; обратный слэш; недопустимый символ — всё это не
    // authority, а не значение, которое можно нормализовать в origin.
    for host in [None, Some("nas:"), Some("nas\\"), Some("na_s:18081")] {
        let mut headers = headers_with(&[("origin", "http://nas:18081")]);
        if let Some(host) = host {
            headers.insert(
                header::HOST,
                HeaderValue::from_str(host).expect("a header value"),
            );
        }
        let (status, response_headers, _) = send_form_raw(
            &app,
            "/admin/login",
            test_peer(),
            headers,
            &[("username", "admin"), ("password", TEST_PASSWORD)],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "Host {host:?}");
        assert_security_headers(&response_headers, "a request with a bad Host");
    }
    assert_eq!(
        session_count(&db.pool).await,
        0,
        "a request without a valid Host must not log in"
    );
}

/// Верные данные дают сессию; неверный пароль и неверное имя неотличимы.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_accepts_the_right_credentials_and_hides_the_reason_on_failure() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(&db.pool, Vec::new());

    let (status, headers, _) = send_form(
        &app,
        "/admin/login",
        &[("username", "admin"), ("password", TEST_PASSWORD)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/wizard"));
    let cookie = set_cookie(&headers).expect("a successful login sets a cookie");
    assert!(cookie.starts_with("tlq_admin="), "{cookie}");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    assert!(cookie.contains("Path=/"), "{cookie}");

    let (status, headers, _) =
        send_with_cookie(&app, Method::GET, "/admin/login", &cookie_pair(&cookie)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location(&headers),
        Some("/admin/wizard"),
        "an active session goes to the same wizard step as after the login itself"
    );

    let (bad_password_status, bad_password_headers, bad_password_body) = send_form(
        &app,
        "/admin/login",
        &[("username", "admin"), ("password", "wrong-password-123")],
    )
    .await;
    assert_eq!(bad_password_status, StatusCode::OK);
    assert!(
        set_cookie(&bad_password_headers).is_none(),
        "a failed login must not set a cookie"
    );

    let (bad_name_status, bad_name_headers, bad_name_body) = send_form(
        &app,
        "/admin/login",
        &[("username", "someone-else"), ("password", TEST_PASSWORD)],
    )
    .await;
    assert_eq!(bad_name_status, bad_password_status);
    assert_eq!(
        bad_name_body, bad_password_body,
        "the response must not reveal whether the name or the password was wrong"
    );
    assert!(
        set_cookie(&bad_name_headers).is_none(),
        "a failed login must not set a cookie"
    );
}

/// Чужой `Origin` на входе — `403` до обработчика: ни сессии, ни cookie.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_with_a_foreign_origin_is_refused_before_the_handler() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(&db.pool, Vec::new());
    let fields = [("username", "admin"), ("password", TEST_PASSWORD)];

    for origin in ["http://nas:8123", "https://evil.example"] {
        let headers = headers_with(&[("host", "nas:18081"), ("origin", origin)]);
        let (status, response_headers, body) =
            send_form_from(&app, "/admin/login", test_peer(), headers, &fields).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "Origin {origin}");
        assert!(
            set_cookie(&response_headers).is_none(),
            "Origin {origin} must not receive a session cookie"
        );
        assert!(
            !String::from_utf8_lossy(&body).contains("Origin"),
            "the refusal must not explain the reason: {}",
            String::from_utf8_lossy(&body)
        );
        assert_security_headers(&response_headers, "a login with a foreign origin");
    }
    assert_eq!(
        session_count(&db.pool).await,
        0,
        "a foreign origin must not create a session"
    );

    let (status, response_headers, _) = send_form(&app, "/admin/login", &fields).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "a request with its own origin logs in"
    );
    assert!(set_cookie(&response_headers).is_some());
    assert_eq!(session_count(&db.pool).await, 1);
}

/// `Referer` заменяет отсутствующий `Origin`; чужой `Referer` и отсутствие
/// обоих заголовков — `403`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_uses_the_referer_when_the_origin_is_missing() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(&db.pool, Vec::new());
    let fields = [("username", "admin"), ("password", TEST_PASSWORD)];

    let headers = headers_with(&[
        ("host", "nas:18081"),
        ("referer", "http://nas:18081/admin/login?next=%2Fadmin"),
    ]);
    let (status, response_headers, _) =
        send_form_raw(&app, "/admin/login", test_peer(), headers, &fields).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "a matching Referer without Origin logs in"
    );
    assert!(set_cookie(&response_headers).is_some());

    let headers = headers_with(&[
        ("host", "nas:18081"),
        ("referer", "http://evil.example/admin/login"),
    ]);
    let (status, _, _) = send_form_raw(&app, "/admin/login", test_peer(), headers, &fields).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a foreign Referer is refused"
    );

    let headers = headers_with(&[
        ("host", "nas:18081"),
        ("origin", "null"),
        ("referer", "http://nas:18081/admin/login"),
    ]);
    let (status, _, _) = send_form_raw(&app, "/admin/login", test_peer(), headers, &fields).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "Origin: null must not fall back to a matching Referer"
    );

    let headers = headers_with(&[("host", "nas:18081")]);
    let (status, _, _) = send_form_raw(&app, "/admin/login", test_peer(), headers, &fields).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a request without Origin and Referer is refused"
    );

    assert_eq!(session_count(&db.pool).await, 1);
}

/// `GET` без `Origin` не проверяется: страницы отвечают как раньше.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_pages_do_not_need_an_origin() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());

    let (status, _, _) = send(&app, Method::GET, "/admin/setup").await;
    assert_eq!(status, StatusCode::OK);

    let (status, headers, _) = send(&app, Method::GET, "/admin").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/login"));
}

/// Сверка покрывает не только POST: любой изменяющий метод без своего
/// origin отклоняется до маршрутизации.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_changing_method_is_checked() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());

    for method in [Method::PUT, Method::PATCH, Method::DELETE] {
        let headers = headers_with(&[("host", "nas:18081"), ("origin", "http://evil.example")]);
        let (status, _, _) = send_from(
            &app,
            method.clone(),
            "/admin/login",
            test_peer(),
            headers,
            Body::empty(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} with a foreign origin"
        );
    }

    let headers = headers_with(&[("host", "nas:18081"), ("origin", "http://nas:18081")]);
    let (status, _, _) = send_from(
        &app,
        Method::PUT,
        "/admin/login",
        test_peer(),
        headers,
        Body::empty(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::METHOD_NOT_ALLOWED,
        "a matching origin reaches the router"
    );
}

/// Setup с чужим `Origin` не создаёт учётку и не расходует код.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_with_a_foreign_origin_keeps_the_code_for_the_right_request() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());
    let issued = codes::issue(&db.pool, Purpose::Setup, None, Utc::now())
        .await
        .expect("a setup code should be issued");
    let fields = [
        ("code", issued.code.as_str()),
        ("username", "admin"),
        ("password", "valid-password-12"),
    ];

    let headers = headers_with(&[("host", "nas:18081"), ("origin", "http://evil.example")]);
    let (status, _, _) = send_form_from(&app, "/admin/setup", test_peer(), headers, &fields).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(account_count(&db.pool).await, 0, "no account is created");
    let used_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT used_at FROM admin_one_time_codes WHERE id = $1")
            .bind(issued.record.id)
            .fetch_one(&db.pool)
            .await
            .expect("the setup code row");
    assert!(used_at.is_none(), "the code is not consumed");

    let (status, response_headers, _) = send_form(&app, "/admin/setup", &fields).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "the code still works from its own origin"
    );
    assert_eq!(location(&response_headers), Some("/admin/wizard"));
    assert_eq!(account_count(&db.pool).await, 1);
}

/// Выход с чужим `Origin` не завершает сессию.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_with_a_foreign_origin_keeps_the_session() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());
    let now = precise_now();
    insert_session(&db.pool, "foreign-origin-token", now, now).await;
    let cookie = "tlq_admin=foreign-origin-token";

    let headers = headers_with(&[
        ("host", "nas:18081"),
        ("origin", "http://nas:8123"),
        ("cookie", cookie),
    ]);
    let (status, response_headers, _) = send_from(
        &app,
        Method::POST,
        "/admin/logout",
        test_peer(),
        headers,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        set_cookie(&response_headers).is_none(),
        "a rejected logout must not clear the cookie"
    );

    let (status, _, _) = send_with_cookie(&app, Method::GET, "/admin", cookie).await;
    assert_eq!(status, StatusCode::OK, "the session is still valid");
    assert_eq!(session_count(&db.pool).await, 1);
}

/// Доверенный прокси задаёт origin через `X-Forwarded-*`; от недоверенного
/// пира те же заголовки игнорируются.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_origin_comes_from_forwarded_headers_only_of_a_trusted_proxy() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(
        &db.pool,
        vec![IpNet::parse("127.0.0.1").expect("a loopback network")],
    );
    let fields = [("username", "admin"), ("password", TEST_PASSWORD)];

    let headers = headers_with(&[
        ("host", "nas:18081"),
        ("x-forwarded-proto", "https"),
        ("x-forwarded-host", "console.example"),
        ("origin", "https://console.example"),
    ]);
    let (status, response_headers, _) =
        send_form_from(&app, "/admin/login", test_peer(), headers.clone(), &fields).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "the trusted proxy defines a matching origin"
    );
    assert!(set_cookie(&response_headers).is_some());

    let untrusted: SocketAddr = SocketAddr::from(([203, 0, 113, 9], 4_040));
    let (status, response_headers, _) =
        send_form_from(&app, "/admin/login", untrusted, headers, &fields).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an untrusted peer falls back to http and Host"
    );
    assert!(set_cookie(&response_headers).is_none());
    assert_eq!(session_count(&db.pool).await, 1);
}

/// Без учётки страница входа уступает место setup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_without_an_account_redirects_to_setup() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());

    for method in [Method::GET, Method::POST] {
        let (status, headers, _) = if method == Method::GET {
            send(&app, method.clone(), "/admin/login").await
        } else {
            send_form(
                &app,
                "/admin/login",
                &[("username", "admin"), ("password", "anything-at-all")],
            )
            .await
        };
        assert_eq!(status, StatusCode::SEE_OTHER, "{method} /admin/login");
        assert_eq!(location(&headers), Some("/admin/setup"), "{method}");
    }
}

/// 7 дней без активности и 30 дней после входа завершают сессию.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_expire_after_idle_or_absolute_time() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());
    let now = Utc::now();

    let idle_token = "idle-session-token";
    let idle_id = insert_session(
        &db.pool,
        idle_token,
        now - ChronoDuration::days(9),
        now - ChronoDuration::days(8),
    )
    .await;
    let idle_cookie = format!("tlq_admin={idle_token}");
    let (status, headers, _) = send_with_cookie(&app, Method::GET, "/admin", &idle_cookie).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "8 idle days invalidate a session"
    );
    assert_eq!(location(&headers), Some("/admin/login"));
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_sessions WHERE id = $1")
        .bind(idle_id)
        .fetch_one(&db.pool)
        .await
        .expect("session count");
    assert_eq!(remaining, 0, "the expired row is deleted");

    let absolute_token = "absolute-session-token";
    let absolute_id = insert_session(
        &db.pool,
        absolute_token,
        now - ChronoDuration::days(31),
        now - ChronoDuration::minutes(1),
    )
    .await;
    let absolute_cookie = format!("tlq_admin={absolute_token}");
    let (status, _, _) = send_with_cookie(&app, Method::GET, "/admin", &absolute_cookie).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "31 days after login invalidate a session"
    );
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_sessions WHERE id = $1")
        .bind(absolute_id)
        .fetch_one(&db.pool)
        .await
        .expect("session count");
    assert_eq!(remaining, 0, "the expired row is deleted");

    let fresh_token = "fresh-session-token";
    insert_session(
        &db.pool,
        fresh_token,
        now - ChronoDuration::days(10),
        now - ChronoDuration::days(6),
    )
    .await;
    let fresh_cookie = format!("tlq_admin={fresh_token}");
    let (status, _, _) = send_with_cookie(&app, Method::GET, "/admin", &fresh_cookie).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "6 idle days and 10 days after login are still valid"
    );
}

/// `last_seen_at` продлевается не чаще раза в минуту.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_is_recorded_at_most_once_a_minute() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());
    let now = precise_now();
    let created_at = now - ChronoDuration::minutes(10);
    let id = insert_session(&db.pool, "touch-session-token", created_at, now).await;
    let cookie = "tlq_admin=touch-session-token";

    let (status, _, _) = send_with_cookie(&app, Method::GET, "/admin", cookie).await;
    assert_eq!(status, StatusCode::OK);
    let last_seen: DateTime<Utc> =
        sqlx::query_scalar("SELECT last_seen_at FROM admin_sessions WHERE id = $1")
            .bind(id)
            .fetch_one(&db.pool)
            .await
            .expect("last_seen_at");
    assert_eq!(
        last_seen, now,
        "a request right after the login must not extend the session"
    );

    let earlier = now - ChronoDuration::minutes(2);
    sqlx::query("UPDATE admin_sessions SET last_seen_at = $2 WHERE id = $1")
        .bind(id)
        .bind(earlier)
        .execute(&db.pool)
        .await
        .expect("age the session");

    let (status, _, _) = send_with_cookie(&app, Method::GET, "/admin", cookie).await;
    assert_eq!(status, StatusCode::OK);
    let renewed: DateTime<Utc> =
        sqlx::query_scalar("SELECT last_seen_at FROM admin_sessions WHERE id = $1")
            .bind(id)
            .fetch_one(&db.pool)
            .await
            .expect("last_seen_at");
    assert!(renewed > earlier, "two idle minutes are recorded");
    assert!(renewed > now, "the update uses the request time");
}

/// Выход удаляет сессию на сервере и сбрасывает cookie.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_deletes_the_session_and_clears_the_cookie() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());
    let now = precise_now();
    let id = insert_session(&db.pool, "logout-session-token", now, now).await;

    let cookie = "tlq_admin=logout-session-token";
    let (status, headers, _) = send_with_cookie(&app, Method::POST, "/admin/logout", cookie).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/login"));
    let cleared = set_cookie(&headers).expect("logout clears the cookie");
    assert!(cleared.starts_with("tlq_admin=;"), "{cleared}");
    assert!(cleared.contains("Max-Age=0"), "{cleared}");
    assert!(!cleared.contains("logout-session-token"), "{cleared}");

    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_sessions WHERE id = $1")
        .bind(id)
        .fetch_one(&db.pool)
        .await
        .expect("session count");
    assert_eq!(remaining, 0, "the session row is deleted");

    let (status, headers, _) = send_with_cookie(&app, Method::GET, "/admin", cookie).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "the old cookie no longer works"
    );
    assert_eq!(location(&headers), Some("/admin/login"));
    assert_eq!(session_count(&db.pool).await, 0);

    // Мёртвая cookie тоже должна исчезнуть из браузера.
    let (status, headers, _) = send_with_cookie(
        &app,
        Method::POST,
        "/admin/logout",
        "tlq_admin=already-gone",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/login"));
    let cleared = set_cookie(&headers).expect("logout always clears the cookie");
    assert!(cleared.contains("Max-Age=0"), "{cleared}");
}

/// `Secure` ставится только по HTTPS от доверенного прокси.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secure_cookie_follows_the_trusted_proxy() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(
        &db.pool,
        vec![IpNet::parse("127.0.0.1").expect("a loopback network")],
    );

    let fields = [("username", "admin"), ("password", TEST_PASSWORD)];
    let mut forwarded = HeaderMap::new();
    forwarded.insert("x-forwarded-proto", HeaderValue::from_static("https"));
    // Origin доверенного прокси: схема https из X-Forwarded-Proto, хост из
    // заголовка Host (X-Forwarded-Host в тесте не задан).
    forwarded.insert(
        header::ORIGIN,
        HeaderValue::from_static("https://nas:18081"),
    );

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/login",
        test_peer(),
        forwarded.clone(),
        &fields,
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let cookie = set_cookie(&headers).expect("a trusted https request sets a cookie");
    assert!(
        cookie.contains("; Secure"),
        "a proxy from ADMIN_TRUSTED_PROXIES may report https: {cookie}"
    );

    let untrusted: SocketAddr = SocketAddr::from(([203, 0, 113, 9], 4_040));
    // Недоверенный пир остаётся на http и `Host`, поэтому origin другой.
    forwarded.insert(header::ORIGIN, HeaderValue::from_static("http://nas:18081"));
    let (status, headers, _) =
        send_form_from(&app, "/admin/login", untrusted, forwarded, &fields).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let cookie = set_cookie(&headers).expect("the login still sets a cookie");
    assert!(
        !cookie.contains("Secure"),
        "an untrusted peer cannot force the flag: {cookie}"
    );
}

/// Заголовки запрета кеша и фреймов стоят на всех ответах консоли.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_console_response_forbids_caching_and_framing() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());

    let (status, headers, _) = send(&app, Method::GET, "/admin/login").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/setup"));
    assert_security_headers(&headers, "GET /admin/login without an account");

    let (status, headers, _) = send(&app, Method::GET, "/admin/setup").await;
    assert_eq!(status, StatusCode::OK);
    assert_security_headers(&headers, "GET /admin/setup");

    let (status, headers, _) = send(&app, Method::GET, "/admin").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_security_headers(&headers, "GET /admin without a session");

    let (status, headers, _) = send_form(
        &app,
        "/admin/setup",
        &[
            ("code", "some-code"),
            ("username", "admin"),
            ("password", "short"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a form error");
    assert_security_headers(&headers, "a setup form error");

    let now = precise_now();
    insert_session(&db.pool, "headers-session-token", now, now).await;
    let (status, headers, _) = send_with_cookie(
        &app,
        Method::GET,
        "/admin",
        "tlq_admin=headers-session-token",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_security_headers(&headers, "GET /admin with a session");
}

/// Сбой базы в экстракторе — `500`, а не вход без сессии.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_database_failure_is_an_error_not_an_anonymous_session() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());
    db.pool.close().await;

    let (status, _, _) =
        send_with_cookie(&app, Method::GET, "/admin", "tlq_admin=some-token").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

/// Вход с паролем и cookie-пара для следующих запросов.
async fn login_cookie(app: &Router, username: &str, password: &str) -> String {
    let (status, headers, _) = send_form(
        app,
        "/admin/login",
        &[("username", username), ("password", password)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "a valid login must succeed");
    cookie_pair(&set_cookie(&headers).expect("a successful login sets a cookie"))
}

/// Текущий хеш и время смены пароля.
async fn account_password(pool: &PgPool) -> (String, DateTime<Utc>) {
    sqlx::query_as("SELECT password_hash, password_changed_at FROM admin_accounts")
        .fetch_one(pool)
        .await
        .expect("the account row")
}

/// `GET` ссылки восстановления не расходует код, `POST` меняет пароль,
/// завершает все сессии и аудируется без секретов.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_link_previews_without_spending_and_resets_the_password_on_post() {
    const NEW_PASSWORD: &str = "recovered-password-1";

    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(&db.pool, Vec::new());
    let issued = codes::issue(&db.pool, Purpose::Recovery, None, Utc::now())
        .await
        .expect("a recovery code");
    let path = format!("/admin/recover/{}", issued.code);

    let now = precise_now();
    insert_session(&db.pool, "recovery-old-session", now, now).await;
    insert_session(&db.pool, "recovery-other-session", now, now).await;

    for attempt in 0..2 {
        let (status, headers, body) = send(&app, Method::GET, &path).await;
        assert_eq!(status, StatusCode::OK, "preview {attempt}");
        assert!(
            String::from_utf8_lossy(&body).contains("Новый пароль"),
            "the preview shows the form: {}",
            String::from_utf8_lossy(&body)
        );
        assert_security_headers(&headers, "a recovery preview");
    }
    let used_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT used_at FROM admin_one_time_codes WHERE id = $1")
            .bind(issued.record.id)
            .fetch_one(&db.pool)
            .await
            .expect("the code row");
    assert!(used_at.is_none(), "GET must not consume the link");
    assert_eq!(session_count(&db.pool).await, 2, "GET deletes no sessions");

    let (status, headers, _) = send_form(&app, &path, &[("password", NEW_PASSWORD)]).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/login"));
    assert_eq!(
        session_count(&db.pool).await,
        0,
        "recovery ends every session"
    );

    let new_cookie = login_cookie(&app, "admin", NEW_PASSWORD).await;
    let (status, _, _) = send_with_cookie(&app, Method::GET, "/admin", &new_cookie).await;
    assert_eq!(status, StatusCode::OK, "the new password logs in");

    let (status, headers, _) = send_form(
        &app,
        "/admin/login",
        &[("username", "admin"), ("password", TEST_PASSWORD)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the old password no longer works");
    assert!(set_cookie(&headers).is_none());

    // Повторный POST по сохранённой ссылке — тот же ответ, что у истёкшей.
    let (status, _, body) = send_form(&app, &path, &[("password", "another-password-2")]).await;
    assert_eq!(status, StatusCode::GONE);
    assert!(
        String::from_utf8_lossy(&body).contains("недействительна"),
        "{}",
        String::from_utf8_lossy(&body)
    );

    let changed = audit_payloads(&db.pool, "admin_password_changed").await;
    assert_eq!(changed.len(), 1, "one password change is audited");
    assert_eq!(changed[0]["reason"], "recovery");
    assert_eq!(changed[0]["ip"], "127.0.0.1");
    let payload = changed[0].to_string();
    for secret in [NEW_PASSWORD, TEST_PASSWORD, issued.code.as_str()] {
        assert!(
            !payload.contains(secret),
            "the audit must not contain {secret:?}: {payload}"
        );
    }
    for (actor, household) in audit_actor(&db.pool, "admin_password_changed").await {
        assert_eq!(actor, "system");
        assert_eq!(household, None);
    }
}

/// Истёкшая ссылка не меняет пароль и даёт единый ответ.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_recovery_link_is_invalid_and_keeps_the_password() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(&db.pool, Vec::new());
    let before = account_password(&db.pool).await;
    let now = Utc::now();
    let code = "expired-recovery-link";

    // CHECK `expires_at > created_at` требует created_at в прошлом.
    sqlx::query(
        "INSERT INTO admin_one_time_codes (id, purpose, code_hash, created_at, expires_at)
         VALUES ($1, 'recovery', $2, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(hash_token(code))
    .bind(now - ChronoDuration::minutes(20))
    .bind(now - ChronoDuration::minutes(5))
    .execute(&db.pool)
    .await
    .expect("an expired code is storable");

    let path = format!("/admin/recover/{code}");
    let (status, _, body) = send(&app, Method::GET, &path).await;
    assert_eq!(status, StatusCode::GONE);
    assert!(
        String::from_utf8_lossy(&body).contains("недействительна"),
        "{}",
        String::from_utf8_lossy(&body)
    );

    let (status, _, _) = send_form(&app, &path, &[("password", "unused-password-12")]).await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(
        account_password(&db.pool).await,
        before,
        "an expired link must not change the password"
    );
    assert!(
        audit_payloads(&db.pool, "admin_password_changed")
            .await
            .is_empty()
    );
}

/// Слишком короткий пароль — ошибка формы, ссылка остаётся действующей.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_short_recovery_password_does_not_spend_the_link() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(&db.pool, Vec::new());
    let issued = codes::issue(&db.pool, Purpose::Recovery, None, Utc::now())
        .await
        .expect("a recovery code");
    let path = format!("/admin/recover/{}", issued.code);

    let (status, _, body) = send_form(&app, &path, &[("password", "short")]).await;
    assert_eq!(status, StatusCode::OK, "a bad length is a form error");
    assert!(
        String::from_utf8_lossy(&body).contains("не менее 12"),
        "{}",
        String::from_utf8_lossy(&body)
    );

    let (status, _, _) = send(&app, Method::GET, &path).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the link is still valid after a form error"
    );

    let (status, headers, _) = send_form(&app, &path, &[("password", "valid-password-12")]).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/login"));
}

/// Шестая неудачная ссылка восстановления — `429` до проверки кода.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_failures_are_rate_limited() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(&db.pool, Vec::new());
    let valid = codes::issue(&db.pool, Purpose::Recovery, None, Utc::now())
        .await
        .expect("a recovery code");

    for attempt in 0..5 {
        let (status, _, _) = send(&app, Method::GET, "/admin/recover/no-such-code").await;
        assert_eq!(status, StatusCode::GONE, "failure {attempt} is a dead link");
    }

    let (status, headers, _) =
        send(&app, Method::GET, &format!("/admin/recover/{}", valid.code)).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the sixth attempt is refused before the code check"
    );
    assert!(headers.contains_key(header::RETRY_AFTER));
    assert_security_headers(&headers, "a rate-limited recovery");
    let used_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT used_at FROM admin_one_time_codes WHERE id = $1")
            .bind(valid.record.id)
            .fetch_one(&db.pool)
            .await
            .expect("the code row");
    assert!(
        used_at.is_none(),
        "a rate-limited request must not consume the valid link"
    );

    let limits = audit_payloads(&db.pool, "rate_limited").await;
    assert!(
        limits
            .iter()
            .any(|payload| payload["limit"] == "admin_recovery_per_15_minutes"),
        "{limits:?}"
    );
}

/// POST восстановления с чужим `Origin` отклоняется слоем роутера.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recovery_post_with_a_foreign_origin_is_refused_before_the_handler() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(&db.pool, Vec::new());
    let issued = codes::issue(&db.pool, Purpose::Recovery, None, Utc::now())
        .await
        .expect("a recovery code");
    let path = format!("/admin/recover/{}", issued.code);

    let headers = headers_with(&[("host", "nas:18081"), ("origin", "http://evil.example")]);
    let (status, response_headers, _) = send_form_from(
        &app,
        &path,
        test_peer(),
        headers,
        &[("password", "unused-password-12")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_security_headers(&response_headers, "a recovery POST with a foreign origin");

    let (status, _, _) = send(&app, Method::GET, &path).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the refused POST keeps the link valid"
    );
    let (status, _, _) = send_form(&app, &path, &[("password", "valid-password-12")]).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
}

/// Смена пароля из одной сессии завершает другую и сохраняет текущую.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changing_the_password_ends_other_sessions_but_keeps_the_current_one() {
    const NEW_PASSWORD: &str = "changed-password-123";

    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(&db.pool, Vec::new());
    let first = login_cookie(&app, "admin", TEST_PASSWORD).await;
    let second = login_cookie(&app, "admin", TEST_PASSWORD).await;
    assert_eq!(session_count(&db.pool).await, 2);

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/password", &first).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&body).contains("Текущий пароль"),
        "{}",
        String::from_utf8_lossy(&body)
    );

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/password",
        test_peer(),
        {
            let mut headers = with_same_origin(HeaderMap::new());
            headers.insert(
                header::COOKIE,
                HeaderValue::from_str(&first).expect("cookie"),
            );
            headers
        },
        &[
            ("current_password", TEST_PASSWORD),
            ("new_password", NEW_PASSWORD),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location(&headers),
        Some("/admin/password?changed=1"),
        "the response confirms the change"
    );

    let (status, _, body) =
        send_with_cookie(&app, Method::GET, "/admin/password?changed=1", &first).await;
    assert_eq!(status, StatusCode::OK, "the current session survives");
    assert!(
        String::from_utf8_lossy(&body).contains("Пароль изменён"),
        "{}",
        String::from_utf8_lossy(&body)
    );

    let (status, headers, _) = send_with_cookie(&app, Method::GET, "/admin", &second).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "the other device loses access"
    );
    assert_eq!(location(&headers), Some("/admin/login"));
    assert_eq!(
        session_count(&db.pool).await,
        1,
        "only the current session stays"
    );

    let changed = audit_payloads(&db.pool, "admin_password_changed").await;
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0]["reason"], "change");
    let payload = changed[0].to_string();
    for secret in [TEST_PASSWORD, NEW_PASSWORD] {
        assert!(
            !payload.contains(secret),
            "the audit must not contain {secret:?}: {payload}"
        );
    }
}

/// Неверный текущий пароль не меняет пароль; шестая попытка — `429`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_current_password_keeps_the_password_and_is_rate_limited() {
    const NEW_PASSWORD: &str = "unreachable-password-1";

    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(&db.pool, Vec::new());
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;
    let before = account_password(&db.pool).await;

    let mut request_headers = with_same_origin(HeaderMap::new());
    request_headers.insert(
        header::COOKIE,
        HeaderValue::from_str(&cookie).expect("cookie"),
    );

    for attempt in 0..5 {
        let (status, _, body) = send_form_from(
            &app,
            "/admin/password",
            test_peer(),
            request_headers.clone(),
            &[
                ("current_password", "wrong-current-password"),
                ("new_password", NEW_PASSWORD),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "failure {attempt} is a form error");
        assert!(
            String::from_utf8_lossy(&body).contains("Неверный текущий пароль"),
            "{}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(
            account_password(&db.pool).await,
            before,
            "a wrong current password must not change the password"
        );
    }

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/password",
        test_peer(),
        request_headers,
        &[
            ("current_password", TEST_PASSWORD),
            ("new_password", NEW_PASSWORD),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the sixth attempt is refused before argon2"
    );
    assert_security_headers(&headers, "a rate-limited password change");
    assert_eq!(
        account_password(&db.pool).await,
        before,
        "a rate-limited attempt must not change the password"
    );

    let limits = audit_payloads(&db.pool, "rate_limited").await;
    assert!(
        limits
            .iter()
            .any(|payload| payload["limit"] == "admin_current_password_per_15_minutes"),
        "{limits:?}"
    );
}

/// Отмена обработчика входа не освобождает слот и не теряет неудачу.
///
/// Таблица `admin_accounts` заблокирована, поэтому проверка пароля заведомо не
/// завершится до отмены: слот должен остаться занятым до конца фоновой
/// проверки, а пять отменённых попыток — закрыть вход. Вызывается сам сервис
/// входа: страница до резервирования слота успела бы упереться в ту же
/// блокировку на чтении учётки.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_login_still_counts_as_a_failure() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let state = console_state(&db.pool, Vec::new());
    let app = build_admin_router(state.clone());

    let mut lock = db.pool.begin().await.expect("lock transaction");
    sqlx::query("LOCK TABLE admin_accounts IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .expect("the account table is locked");

    let client = Client {
        ip: "127.0.0.1".parse().expect("ip"),
        scheme: Scheme::Http,
        host: Some("nas:18081".into()),
    };

    for attempt in 0..5 {
        let cancelled = tokio::time::timeout(
            Duration::from_millis(500),
            login::authenticate(
                &state,
                &client,
                "admin",
                Password::new("wrong-password-123"),
                Utc::now(),
            ),
        )
        .await;
        assert!(
            cancelled.is_err(),
            "attempt {attempt} must be cancelled before the blocked check finishes"
        );
    }

    lock.commit().await.expect("release the account table");

    // Ждём, пока все пять фоновых проверок запишут неудачи: отказ только из-за
    // идущих проверок отдаёт `Retry-After = 1`, а отказ по пяти отметкам —
    // остаток окна. Поэтому большое значение и есть признак готовности.
    let key = ip_key(client.ip);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match state.login_limiter().reserve(&key, Utc::now()) {
            Reservation::Allowed(guard) => guard.succeeded(),
            Reservation::Denied(decision) if decision.retry_after.unwrap_or(0) > 1 => break,
            _ => {}
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the cancelled checks did not record five failures in time"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let (status, headers, _) = send_form(
        &app,
        "/admin/login",
        &[("username", "admin"), ("password", TEST_PASSWORD)],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "five cancelled failures must close the login"
    );
    assert!(set_cookie(&headers).is_none());
}

/// Два параллельных выпуска одной пары сериализуются: действует один код.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_code_issues_leave_exactly_one_active_code() {
    let db = support::isolated_db().await;
    let now = Utc::now();

    let (first, second) = tokio::join!(
        codes::issue(&db.pool, Purpose::Recovery, None, now),
        codes::issue(&db.pool, Purpose::Recovery, None, now),
    );
    first.expect("the first issue succeeds");
    second.expect("the second issue waits and succeeds");

    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM admin_one_time_codes
         WHERE purpose = 'recovery' AND used_at IS NULL",
    )
    .fetch_one(&db.pool)
    .await
    .expect("active code count");
    assert_eq!(active, 1, "only one recovery code may stay active");
}

/// Выпуск setup-кода ждёт транзакцию создания учётки и после неё не выпускает
/// код.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn setup_code_issue_waits_for_the_account_transaction() {
    let db = support::isolated_db().await;
    let now = Utc::now();

    let mut tx = db.pool.begin().await.expect("account transaction");
    codes::lock_purpose(&mut tx, Purpose::Setup, None)
        .await
        .expect("the setup lock");
    sqlx::query(
        "INSERT INTO admin_accounts (id, username, password_hash)
         VALUES ($1, 'admin', '$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA')",
    )
    .bind(Uuid::now_v7())
    .execute(&mut *tx)
    .await
    .expect("the account is created in the transaction");

    let pool = db.pool.clone();
    let task = tokio::spawn(async move { codes::issue_setup_code(&pool, now).await });

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !task.is_finished(),
        "the setup code issue must wait for the account transaction"
    );

    tx.commit().await.expect("commit the account transaction");
    let issued = task
        .await
        .expect("the issue task")
        .expect("the issue result");
    assert!(
        issued.is_none(),
        "the committed account must prevent a setup code"
    );

    let codes: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_one_time_codes")
        .fetch_one(&db.pool)
        .await
        .expect("code count");
    assert_eq!(codes, 0, "no setup code is issued after the account exists");
}

/// Запись пароля по устаревшему хешу не затирает более новый пароль.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_current_password_does_not_overwrite_a_newer_one() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");

    let original: String = sqlx::query_scalar("SELECT password_hash FROM admin_accounts")
        .fetch_one(&mut *conn)
        .await
        .expect("the original hash");

    // Параллельное восстановление уже сменило пароль.
    admin_accounts::update_password(&mut conn, "newer-password-hash", now)
        .await
        .expect("the newer password is written");

    let stale = admin_accounts::update_password_if_unchanged(
        &mut conn,
        &original,
        "stale-password-hash",
        now,
    )
    .await
    .expect("the stale update runs");
    assert!(
        stale.is_none(),
        "a hash changed after the check must reject the stale write"
    );

    let stored: String = sqlx::query_scalar("SELECT password_hash FROM admin_accounts")
        .fetch_one(&mut *conn)
        .await
        .expect("the stored hash");
    assert_eq!(stored, "newer-password-hash", "the newer password survives");

    // С актуальным ожидаемым хешем запись проходит.
    let updated = admin_accounts::update_password_if_unchanged(
        &mut conn,
        &stored,
        "final-password-hash",
        now,
    )
    .await
    .expect("the current update runs");
    assert!(updated.is_some());
    let stored: String = sqlx::query_scalar("SELECT password_hash FROM admin_accounts")
        .fetch_one(&mut *conn)
        .await
        .expect("the stored hash");
    assert_eq!(stored, "final-password-hash");
}

/// Ждать, пока `UPDATE admin_accounts` встанет на блокировке строки.
async fn wait_for_blocked_admin_account_update(pool: &PgPool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let blocked: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database()
               AND wait_event_type = 'Lock'
               AND query LIKE 'UPDATE admin_accounts%'",
        )
        .fetch_one(pool)
        .await
        .expect("pg_stat_activity");
        if blocked > 0 {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the password update did not block on the racing transaction"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Смена пароля, проигравшая гонку восстановлению, не считается неудачей.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_password_race_with_recovery_is_not_a_failed_attempt() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let now = precise_now();
    let session_id = insert_session(&db.pool, "race-session", now, now).await;

    let state = console_state(&db.pool, Vec::new());
    let client = Client {
        ip: "127.0.0.1".parse().expect("ip"),
        scheme: Scheme::Http,
        host: Some("nas:18081".into()),
    };

    // Параллельное восстановление уже пишет новый хеш, но транзакция не
    // закоммичена: проверка текущего пароля видит прежний хеш, а запись встаёт
    // на блокировке строки. После коммита условие `password_hash = $3` уже не
    // совпадает — это детерминированная гонка без таймингов.
    let newer_hash = password::hash(Password::new("newer-password-12"))
        .await
        .expect("the newer password hash");
    let mut racing = db.pool.begin().await.expect("racing transaction");
    sqlx::query("UPDATE admin_accounts SET password_hash = $1")
        .bind(&newer_hash)
        .execute(&mut *racing)
        .await
        .expect("the newer hash is written in the racing transaction");

    let task_state = state.clone();
    let task_client = client.clone();
    let task = tokio::spawn(async move {
        change::change(
            &task_state,
            AdminSession { id: session_id },
            &task_client,
            Password::new(TEST_PASSWORD),
            Password::new("changed-password-12"),
            Utc::now(),
        )
        .await
    });

    wait_for_blocked_admin_account_update(&db.pool).await;
    racing.commit().await.expect("commit the newer hash");

    let outcome = task
        .await
        .expect("the change task must join")
        .expect("the change must not fail");
    assert!(
        matches!(outcome, ChangeOutcome::AlreadyChanged),
        "a lost race is a separate outcome: {outcome:?}"
    );

    // Неудача в бакет не попала: пять неверных текущих паролей ещё проходят,
    // шестой упирается в лимит.
    for attempt in 1..=5 {
        let outcome = change::change(
            &state,
            AdminSession { id: session_id },
            &client,
            Password::new("wrong-password-12"),
            Password::new("another-password-12"),
            Utc::now(),
        )
        .await
        .expect("a wrong current password must not fail the request");
        assert!(
            matches!(outcome, ChangeOutcome::WrongPassword),
            "attempt {attempt} inside the limit: {outcome:?}"
        );
    }
    let limited = change::change(
        &state,
        AdminSession { id: session_id },
        &client,
        Password::new("wrong-password-12"),
        Password::new("another-password-12"),
        Utc::now(),
    )
    .await
    .expect("a rate-limited request is still a result");
    assert!(
        matches!(limited, ChangeOutcome::RateLimited(_)),
        "the race must not consume the failure bucket: {limited:?}"
    );
}

/// Значение атрибута из HTML, начинающееся с пути встроенного ресурса.
fn linked_asset(body: &[u8], ext: &str) -> String {
    let text = String::from_utf8_lossy(body);
    let suffix = format!(".{ext}");
    text.split('"')
        .find(|value| value.starts_with("/admin/assets/") && value.ends_with(&suffix))
        .unwrap_or_else(|| panic!("no /admin/assets/*{suffix} link in {text}"))
        .to_owned()
}

/// В ответе нет внешних адресов: ни схем, ни протокол-относительных ссылок.
fn assert_no_external_urls(body: &[u8], context: &str) {
    let text = String::from_utf8_lossy(body);
    for forbidden in ["http://", "https://", "src=\"//", "href=\"//"] {
        assert!(
            !text.contains(forbidden),
            "{context} must not reference {forbidden}: {text}"
        );
    }
}

/// Скрипты и стили страницы — только встроенные ресурсы консоли.
fn assert_local_assets(body: &[u8], context: &str) {
    let text = String::from_utf8_lossy(body);
    assert!(
        text.contains("href=\"/admin/assets/console-"),
        "{context} must link the console CSS: {text}"
    );
    assert!(
        text.contains("src=\"/admin/assets/htmx-"),
        "{context} must link htmx: {text}"
    );
    for (index, _) in text.match_indices("src=\"") {
        let value = &text[index + "src=\"".len()..];
        let value = value.split('"').next().unwrap_or_default();
        assert!(
            value.starts_with("/admin/assets/"),
            "{context} loads a script from {value}: {text}"
        );
    }
}

/// Короткий hex SHA-256 — та же часть имени, что у ресурса с хешем.
fn content_hash(body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_page_is_self_contained_and_uses_plain_forms() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());

    let (status, headers, body) = send(&app, Method::GET, "/admin/setup").await;
    assert_eq!(status, StatusCode::OK);
    assert_security_headers(&headers, "GET /admin/setup");
    assert_no_external_urls(&body, "GET /admin/setup");
    assert_local_assets(&body, "GET /admin/setup");
    assert!(
        String::from_utf8_lossy(&body).contains("<form method=\"post\" action=\"/admin/setup\">"),
        "the form works without JavaScript: {}",
        String::from_utf8_lossy(&body)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_and_recovery_pages_are_self_contained_and_use_plain_forms() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(&db.pool, Vec::new());
    let issued = codes::issue(&db.pool, Purpose::Recovery, None, Utc::now())
        .await
        .expect("a recovery code");

    let (status, headers, body) = send(&app, Method::GET, "/admin/login").await;
    assert_eq!(status, StatusCode::OK);
    assert_security_headers(&headers, "GET /admin/login");
    assert_no_external_urls(&body, "GET /admin/login");
    assert_local_assets(&body, "GET /admin/login");
    assert!(
        String::from_utf8_lossy(&body).contains("<form method=\"post\" action=\"/admin/login\">"),
        "{}",
        String::from_utf8_lossy(&body)
    );

    let path = format!("/admin/recover/{}", issued.code);
    let (status, headers, body) = send(&app, Method::GET, &path).await;
    assert_eq!(status, StatusCode::OK, "the valid link shows the form");
    assert_security_headers(&headers, "GET a recovery page");
    assert_no_external_urls(&body, "GET a recovery page");
    assert_local_assets(&body, "GET a recovery page");
    assert!(
        String::from_utf8_lossy(&body).contains(&format!(
            "<form method=\"post\" action=\"/admin/recover/{}\">",
            issued.code
        )),
        "{}",
        String::from_utf8_lossy(&body)
    );

    let (status, headers, body) = send(&app, Method::GET, "/admin/recover/no-such-code").await;
    assert_eq!(status, StatusCode::GONE);
    assert_security_headers(&headers, "an invalid recovery link");
    assert_no_external_urls(&body, "an invalid recovery link");
    assert!(String::from_utf8_lossy(&body).contains("недействительна"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_pages_show_navigation_and_plain_forms() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(&db.pool, Vec::new());
    let now = precise_now();
    insert_session(&db.pool, "navigation-session", now, now).await;

    for path in ["/admin", "/admin/password"] {
        let (status, headers, body) =
            send_with_cookie(&app, Method::GET, path, "tlq_admin=navigation-session").await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_security_headers(&headers, path);
        assert_no_external_urls(&body, path);
        assert_local_assets(&body, path);

        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("href=\"/admin\""), "{path}: {text}");
        assert!(text.contains("href=\"/admin/password\""), "{path}: {text}");
        assert!(
            text.contains("<form method=\"post\" action=\"/admin/logout\">"),
            "logout is a plain form on {path}: {text}"
        );
    }

    let (_, _, body) = send_with_cookie(
        &app,
        Method::GET,
        "/admin/password",
        "tlq_admin=navigation-session",
    )
    .await;
    assert!(
        String::from_utf8_lossy(&body)
            .contains("<form method=\"post\" action=\"/admin/password\">"),
        "{}",
        String::from_utf8_lossy(&body)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusals_and_form_errors_stay_local() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());
    let fields = [
        ("code", "no-such-code"),
        ("username", "admin"),
        ("password", "valid-password-12"),
    ];

    let (status, headers, body) = send_form(&app, "/admin/setup", &fields).await;
    assert_eq!(status, StatusCode::OK, "an unknown code is a form error");
    assert_security_headers(&headers, "a setup form error");
    assert_no_external_urls(&body, "a setup form error");
    assert!(String::from_utf8_lossy(&body).contains("Код недействителен"));

    // Пять неудач разрешены; шестая упирается в лимит.
    for _ in 1..5 {
        let (status, _, _) = send_form(&app, "/admin/setup", &fields).await;
        assert_eq!(status, StatusCode::OK, "a failure inside the limit");
    }
    let (status, headers, body) = send_form(&app, "/admin/setup", &fields).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_security_headers(&headers, "a rate-limited setup");
    assert_no_external_urls(&body, "a rate-limited setup");

    let headers = headers_with(&[("host", "nas:18081"), ("origin", "http://evil.example")]);
    let (status, headers, body) =
        send_form_raw(&app, "/admin/setup", test_peer(), headers, &[("code", "x")]).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_security_headers(&headers, "a request with a foreign origin");
    assert_no_external_urls(&body, "a request with a foreign origin");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedded_assets_are_served_by_content_hash() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());

    // Страница без сессии уже ссылается на встроенные ресурсы.
    let (status, _, body) = send(&app, Method::GET, "/admin/setup").await;
    assert_eq!(status, StatusCode::OK);
    assert_local_assets(&body, "GET /admin/setup");
    let css = linked_asset(&body, "css");
    let htmx = linked_asset(&body, "js");

    let (status, headers, css_body) = send(&app, Method::GET, &css).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/css; charset=utf-8")
    );
    let cache = headers
        .get(header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .expect("a Cache-Control header");
    assert!(cache.contains("immutable"), "{cache}");
    assert!(!cache.contains("no-store"), "{cache}");
    assert_frame_headers(&headers, "the console CSS");
    assert_eq!(css_body, assets::CONSOLE_CSS);
    let css_text = String::from_utf8_lossy(&css_body);
    for forbidden in ["http://", "https://", "//", "url(", "@import"] {
        assert!(
            !css_text.contains(forbidden),
            "the console CSS must not reference {forbidden}: {css_text}"
        );
    }
    assert!(
        css.contains(&content_hash(assets::CONSOLE_CSS)),
        "the CSS path carries the content hash: {css}"
    );

    let (status, headers, js_body) = send(&app, Method::GET, &htmx).await;
    assert_eq!(status, StatusCode::OK);
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .expect("a Content-Type header");
    assert!(
        content_type.starts_with("text/javascript"),
        "{content_type}"
    );
    assert_frame_headers(&headers, "htmx");
    assert_eq!(
        js_body,
        assets::HTMX_JS,
        "the embedded htmx is served as is"
    );
    assert!(
        htmx.contains(&content_hash(assets::HTMX_JS)),
        "the htmx path carries the content hash: {htmx}"
    );

    let unknown_hash = css.replace(&content_hash(assets::CONSOLE_CSS), "0000000000000000");
    assert_ne!(unknown_hash, css);
    let (status, headers, _) = send(&app, Method::GET, &unknown_hash).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an unknown hash is not served"
    );
    assert_security_headers(&headers, "an unknown asset");

    let (status, _, _) = send(&app, Method::GET, "/admin/assets/htmx.min.js").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the hash is part of the path"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_whole_flow_works_with_plain_form_posts() {
    const NEW_PASSWORD: &str = "changed-password-1";

    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());
    let issued = codes::issue(&db.pool, Purpose::Setup, None, Utc::now())
        .await
        .expect("a setup code");

    // Ни один запрос не несёт HX-Request: это обычные form-urlencoded POST.
    let (status, headers, _) = send_form(
        &app,
        "/admin/setup",
        &[
            ("code", &issued.code),
            ("username", "admin"),
            ("password", TEST_PASSWORD),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/wizard"));
    let cookie = cookie_pair(&set_cookie(&headers).expect("setup starts a session"));

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/logout",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/login"));
    assert_eq!(session_count(&db.pool).await, 0, "logout ends the session");

    let (status, headers, _) = send_form(
        &app,
        "/admin/login",
        &[("username", "admin"), ("password", TEST_PASSWORD)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/wizard"));
    let cookie = cookie_pair(&set_cookie(&headers).expect("login starts a session"));

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/password",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("current_password", TEST_PASSWORD),
            ("new_password", NEW_PASSWORD),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/password?changed=1"));

    let (status, _, body) =
        send_with_cookie(&app, Method::GET, "/admin/password?changed=1", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("Пароль изменён."));
    assert_eq!(
        session_count(&db.pool).await,
        1,
        "the current session survives the change"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recovery_code_from_the_address_is_percent_encoded() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());
    let code = "<script>";
    let now = precise_now();
    sqlx::query(
        "INSERT INTO admin_one_time_codes (id, purpose, code_hash, created_at, expires_at)
         VALUES ($1, 'recovery', $2, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(hash_token(code))
    .bind(now)
    .bind(now + ChronoDuration::minutes(15))
    .execute(&db.pool)
    .await
    .expect("a recovery code");

    let (status, headers, body) = send(&app, Method::GET, "/admin/recover/%3Cscript%3E").await;
    assert_eq!(status, StatusCode::OK);
    assert_security_headers(&headers, "a recovery form");
    assert_no_external_urls(&body, "a recovery form");

    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("action=\"/admin/recover/%3Cscript%3E\""),
        "the code is percent-encoded in the form action: {text}"
    );
    assert!(
        !text.contains("\"/admin/recover/<script>\""),
        "the raw code must not reach the markup: {text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recovery_code_with_path_separators_stays_one_segment() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());

    // Короткий пароль возвращает форму с кодом из адреса до проверки кода и
    // лимита. `?` и `#` в action превратили бы код в query/fragment.
    let (status, _, body) =
        send_form(&app, "/admin/recover/a%3Fb%23c", &[("password", "short")]).await;
    assert_eq!(status, StatusCode::OK);

    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("action=\"/admin/recover/a%3Fb%23c\""),
        "path separators are percent-encoded in the form action: {text}"
    );
    assert!(
        !text.contains("action=\"/admin/recover/a?b#c\""),
        "the raw separators must not reach the markup: {text}"
    );
}

/// Household, созданный до входа, снимает шаг household: мастер начинается с LLM.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_household_created_before_login_starts_the_wizard_at_llm() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    insert_household(&db.pool).await;
    let app = console(&db.pool, Vec::new());

    let (status, headers, _) = send_form(
        &app,
        "/admin/login",
        &[("username", "admin"), ("password", TEST_PASSWORD)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location(&headers),
        Some("/admin/wizard"),
        "an unfinished wizard comes before the main page"
    );

    let cookie = cookie_pair(&set_cookie(&headers).expect("a successful login sets a cookie"));
    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("Разбор записей"), "{text}");
    assert!(
        !text.contains("Токен Home Assistant"),
        "the token step must wait for LLM: {text}"
    );
}

/// После пропуска LLM повторный вход открывает следующий незавершённый шаг.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_wizard_resumes_at_the_next_unfinished_step_after_a_new_login() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    insert_household(&db.pool).await;
    let app = console(&db.pool, Vec::new());
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("Разбор записей"));

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/wizard/skip",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("step", "llm")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/wizard"));

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&body).contains("Токен Home Assistant"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        setting_value(&db.pool, "wizard.llm_skipped").await,
        Some(Value::Bool(true))
    );
    let events = audit_payloads(&db.pool, "admin_setting_changed").await;
    assert_eq!(
        events.len(),
        1,
        "the skip leaves one audit event: {events:?}"
    );
    assert_eq!(events[0]["key"], "wizard.llm_skipped");
    for (actor, household) in audit_actor(&db.pool, "admin_setting_changed").await {
        assert_eq!(actor, "system");
        assert_eq!(household, None);
    }

    let (status, _, _) = send_form_from(
        &app,
        "/admin/logout",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (status, headers, _) = send_form(
        &app,
        "/admin/login",
        &[("username", "admin"), ("password", TEST_PASSWORD)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/wizard"));
    let cookie = cookie_pair(&set_cookie(&headers).expect("a successful login sets a cookie"));

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("Токен Home Assistant"), "{text}");
    assert!(
        !text.contains("Разбор записей"),
        "a finished step is not repeated: {text}"
    );
}

/// Без Telegram шаг household не пропускается; с Telegram — пропускается.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_household_step_can_only_be_skipped_with_telegram() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let without = console(&db.pool, Vec::new());
    let cookie = login_cookie(&without, "admin", TEST_PASSWORD).await;

    let (status, _, body) = send_with_cookie(&without, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("Дом"), "{text}");
    assert!(
        !text.contains("Пропустить"),
        "the household step has no skip button without Telegram: {text}"
    );

    let (status, _, body) = send_form_from(
        &without,
        "/admin/wizard/skip",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("step", "household")],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "skipping the household without Telegram is refused"
    );
    assert!(
        String::from_utf8_lossy(&body).contains("Дом"),
        "the current step is shown again: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        setting_value(&db.pool, "wizard.household_skipped").await,
        None,
        "a refused skip writes no key"
    );

    let (status, _, body) = send_with_cookie(&without, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("Дом"));

    // Тот же сеанс, но Telegram включён: пропуск работает.
    let with = console_with(&db.pool, TenancyMode::Single, true, LlmConfig::empty());
    let (status, _, body) = send_with_cookie(&with, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&body).contains("Пропустить"),
        "Telegram enables the household skip: {}",
        String::from_utf8_lossy(&body)
    );

    let (status, headers, _) = send_form_from(
        &with,
        "/admin/wizard/skip",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("step", "household")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/wizard"));
    assert_eq!(
        setting_value(&db.pool, "wizard.household_skipped").await,
        Some(Value::Bool(true))
    );

    let (status, _, body) = send_with_cookie(&with, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&body).contains("Разбор записей"),
        "{}",
        String::from_utf8_lossy(&body)
    );
}

/// В `multi` мастер не показывает шаги household и токена.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_mode_hides_the_household_and_token_steps() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console_with(&db.pool, TenancyMode::Multi, false, LlmConfig::empty());

    let (status, headers, _) = send_form(
        &app,
        "/admin/login",
        &[("username", "admin"), ("password", TEST_PASSWORD)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/wizard"));
    let cookie = cookie_pair(&set_cookie(&headers).expect("a successful login sets a cookie"));

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("Разбор записей"), "{text}");
    assert!(
        !text.contains("Дом") && !text.contains("Токен Home Assistant"),
        "multi hides the household and token steps: {text}"
    );

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/wizard/skip",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("step", "llm")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location(&headers),
        Some("/admin"),
        "the last step leads to the main page"
    );

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("Консоль администратора"), "{text}");
    assert!(
        !text.contains("Продолжить настройку"),
        "the wizard is finished: {text}"
    );
}

/// Чужой `Origin` на пропуске — `403`; без сессии — `303` на вход.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wizard_skip_checks_the_origin_and_the_session() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(&db.pool, Vec::new());

    let (status, headers, _) =
        send_form(&app, "/admin/wizard/skip", &[("step", "household")]).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "an anonymous POST goes to the login page"
    );
    assert_eq!(location(&headers), Some("/admin/login"));

    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;
    let headers = headers_with(&[
        ("host", "nas:18081"),
        ("origin", "http://evil.example"),
        ("cookie", &cookie),
    ]);
    let (status, response_headers, _) = send_form_from(
        &app,
        "/admin/wizard/skip",
        test_peer(),
        headers,
        &[("step", "household")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_security_headers(&response_headers, "a wizard skip with a foreign origin");
    assert_eq!(
        setting_value(&db.pool, "wizard.household_skipped").await,
        None,
        "a refused origin writes no key"
    );

    // Не текущий шаг тоже ничего не пишет.
    let (status, _, _) = send_form_from(
        &app,
        "/admin/wizard/skip",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("step", "llm")],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(setting_value(&db.pool, "wizard.llm_skipped").await, None);
}

/// Страницы мастера проходят общие проверки: локальные ресурсы, POST-форма,
/// запрет кеша и фреймов; главная остаётся доступной со ссылкой на мастер.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wizard_pages_are_self_contained_and_keep_the_home_reachable() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console_with(&db.pool, TenancyMode::Single, true, LlmConfig::empty());
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let (status, headers, body) =
        send_with_cookie(&app, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_security_headers(&headers, "GET /admin/wizard");
    assert_no_external_urls(&body, "GET /admin/wizard");
    assert_local_assets(&body, "GET /admin/wizard");
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("<form method=\"post\" action=\"/admin/wizard/skip\">"),
        "the skip is a plain POST form: {text}"
    );
    assert!(
        text.contains("<form method=\"post\" action=\"/admin/wizard/household\">"),
        "the household step has its own plain POST form: {text}"
    );
    assert!(
        text.contains("value=\"Europe/Moscow\""),
        "the timezone defaults to the CLI default: {text}"
    );

    let (status, headers, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK, "the main page stays reachable");
    assert_security_headers(&headers, "GET /admin during the wizard");
    assert_no_external_urls(&body, "GET /admin during the wizard");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("Продолжить настройку"), "{text}");
    assert!(text.contains("href=\"/admin/wizard\""), "{text}");
    assert!(text.contains("href=\"/admin/password\""), "{text}");
    assert!(text.contains("action=\"/admin/logout\""), "{text}");
}

/// Пропуск токена завершает мастер; повторный пропуск уводит на главную.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_token_step_can_be_skipped_and_finishes_the_wizard() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    insert_household(&db.pool).await;
    let app = console(&db.pool, Vec::new());
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let (status, _, _) = send_form_from(
        &app,
        "/admin/wizard/skip",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("step", "llm")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&body).contains("Токен Home Assistant"),
        "{}",
        String::from_utf8_lossy(&body)
    );

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/wizard/skip",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("step", "token")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin"));
    assert_eq!(
        setting_value(&db.pool, "wizard.token_skipped").await,
        Some(Value::Bool(true))
    );

    let (status, headers, _) = send_with_cookie(&app, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin"));

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !String::from_utf8_lossy(&body).contains("Продолжить настройку"),
        "{}",
        String::from_utf8_lossy(&body)
    );

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/wizard/skip",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("step", "token")],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "a repeated skip after the wizard is finished goes home"
    );
    assert_eq!(location(&headers), Some("/admin"));
}

/// Два одновременных POST шага токена выпускают ровно один токен.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_double_token_post_issues_exactly_one_token() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    insert_household(&db.pool).await;
    let app = console(&db.pool, Vec::new());
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    // Шаг LLM пропущен: мастер стоит на токене.
    let (status, _, _) = send_form_from(
        &app,
        "/admin/wizard/skip",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("step", "llm")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("Токен Home Assistant"));

    let first = send_form_from(
        &app,
        "/admin/wizard/token",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("name", "Home Assistant")],
    );
    let second = send_form_from(
        &app,
        "/admin/wizard/token",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("name", "Home Assistant")],
    );
    let (first, second) = tokio::join!(first, second);

    assert_eq!(
        token_count(&db.pool).await,
        1,
        "a double click must not issue two tokens"
    );
    let issued_pages = [&first, &second]
        .iter()
        .filter(|(_, _, body)| String::from_utf8_lossy(body).contains("id=\"issued-token\""))
        .count();
    assert_eq!(issued_pages, 1, "the open token is shown exactly once");
    for (status, headers, _) in [first, second] {
        assert!(
            matches!(
                status,
                StatusCode::OK | StatusCode::SEE_OTHER | StatusCode::BAD_REQUEST
            ),
            "a concurrent request is refused without a server error: {status}"
        );
        assert_security_headers(&headers, "POST /admin/wizard/token");
    }
}

/// В `single` с несколькими household шаг токена — ошибка формы `400`, не `500`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_token_step_with_several_households_is_a_form_error() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    // Два household: `admin household create` в `single` второй бы отклонил,
    // но такая база — штатное состояние с предупреждением `multiple_households`.
    for name in ["Первый дом", "Второй дом"] {
        tasklerq::admin::create_household(
            &db.pool,
            TenancyMode::Multi,
            name,
            "Europe/Moscow",
            Utc::now(),
        )
        .await
        .expect("a household");
    }
    let app = console(&db.pool, Vec::new());
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    // Шаг LLM пропущен: мастер стоит на токене.
    let (status, _, _) = send_form_from(
        &app,
        "/admin/wizard/skip",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("step", "llm")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("Токен Home Assistant"));

    let (status, headers, body) = send_form_from(
        &app,
        "/admin/wizard/token",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("name", "Мой токен HA")],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an ambiguous household is a form error: {}",
        String::from_utf8_lossy(&body)
    );
    assert_security_headers(&headers, "POST /admin/wizard/token");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("Токен Home Assistant"), "{text}");
    assert!(text.contains("больше одного household"), "{text}");
    assert!(
        text.contains("value=\"Мой токен HA\""),
        "the submitted token name is kept: {text}"
    );
    assert!(text.contains("action=\"/admin/wizard/token\""), "{text}");
    assert!(!text.contains("id=\"issued-token\""), "{text}");
    assert_eq!(token_count(&db.pool).await, 0, "no token is issued");
}

/// Дом удалён между шагом токена и POST: `Validation` — про отсутствие
/// household, а не про несколько. Введённое имя токена сохраняется.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_token_step_without_a_household_says_so() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    insert_household(&db.pool).await;
    let app = console(&db.pool, Vec::new());
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    // Шаг LLM пропущен: мастер стоит на токене.
    let (status, _, _) = send_form_from(
        &app,
        "/admin/wizard/skip",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("step", "llm")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // Удаление дома держим незакоммиченным: `current_step` ещё видит household
    // по старой версии строки, а выпуск токена ждёт блокировку строки. После
    // commit дом исчезает, и `create_wizard_token` получает ноль active.
    let mut tx = db.pool.begin().await.expect("delete transaction");
    sqlx::query("UPDATE households SET deleted_at = now() WHERE deleted_at IS NULL")
        .execute(&mut *tx)
        .await
        .expect("the household is marked deleted");

    let post = {
        let app = app.clone();
        let cookie = cookie.clone();
        tokio::spawn(async move {
            send_form_from(
                &app,
                "/admin/wizard/token",
                test_peer(),
                headers_with(&[("cookie", &cookie)]),
                &[("name", "Мой токен HA")],
            )
            .await
        })
    };

    // Дождаться, пока POST встанет на блокировке строки household.
    let mut blocked = false;
    for _ in 0..200 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(&db.pool)
        .await
        .expect("pg_stat_activity is readable");
        if waiting > 0 {
            blocked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(blocked, "the token POST waits for the household row lock");
    tx.commit().await.expect("the household deletion commits");

    let (status, headers, body) = post.await.expect("the token POST finishes");
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a missing household is a form error: {}",
        String::from_utf8_lossy(&body)
    );
    assert_security_headers(&headers, "POST /admin/wizard/token");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("нет активного household"), "{text}");
    assert!(!text.contains("больше одного household"), "{text}");
    assert!(
        text.contains("value=\"Мой токен HA\""),
        "the submitted token name is kept: {text}"
    );
    assert!(!text.contains("id=\"issued-token\""), "{text}");
    assert_eq!(token_count(&db.pool).await, 0, "no token is issued");
}

/// Главная показывает версию, готовность, режим, household, Telegram и слоты.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_home_page_shows_the_installation_state_and_readiness() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;

    // Два household в режиме `single`: readiness предупреждает, главная
    // показывает оба кода, затем одно удаляется и счёт уменьшается.
    let first = tasklerq::admin::create_household(
        &db.pool,
        TenancyMode::Multi,
        "Первый дом",
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("the first household");
    let second = tasklerq::admin::create_household(
        &db.pool,
        TenancyMode::Multi,
        "Второй дом",
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("the second household");
    assert_ne!(first.id, second.id);

    let llm = llm_env(&[
        ("LLM_CAPTURE_ENABLED", "true"),
        ("LLM_PRIMARY_URL", "https://llm.example.test/v1"),
        ("LLM_PRIMARY_MODEL", "vendor/primary"),
    ]);
    let cipher = SecretCipher::new(None, None);
    let effective = settings::effective_llm(&llm, &[], &cipher);
    let handle = LlmHandle::new(LlmSnapshot::from_effective(effective.clone()));
    let llm_settings = LlmSettings::new(db.pool.clone(), llm, cipher.clone(), handle.clone());
    let slots = Arc::new(LlmSlotHealth::new(handle));

    // Публичный роутер и консоль делят снимок, ключи и статус слотов: их
    // предупреждения readiness должны совпадать.
    let public = build_router(
        AppState::new(db.pool.clone(), Audit::new(db.pool.clone()))
            .with_tenancy_mode(TenancyMode::Single)
            .with_secret_cipher(cipher.clone())
            .with_llm_slots(slots.clone()),
    );
    let app = build_admin_router(
        AdminState::new(
            db.pool.clone(),
            Vec::new(),
            Audit::new(db.pool.clone()),
            llm_settings,
        )
        .with_wizard(WizardSettings::new(TenancyMode::Single, false))
        .with_llm_slots(slots.clone()),
    );

    let (status, _, ready) = send(&public, Method::GET, "/health/ready").await;
    assert_eq!(status, StatusCode::OK);
    let ready: Value = serde_json::from_slice(&ready).expect("readiness JSON");
    assert_eq!(
        ready["warnings"],
        json!(["multiple_households", "llm_slot_unverified:primary"]),
        "{ready}"
    );

    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;
    let (status, headers, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_security_headers(&headers, "GET /admin");
    assert_no_external_urls(&body, "GET /admin");
    assert_local_assets(&body, "GET /admin");
    let text = String::from_utf8_lossy(&body);

    assert!(text.contains(env!("CARGO_PKG_VERSION")), "{text}");
    assert!(
        text.contains("<dt>Режим размещения</dt><dd>single</dd>"),
        "{text}"
    );
    assert!(text.contains("<dt>Household</dt><dd>2</dd>"), "{text}");
    assert!(
        text.contains("<dt>Telegram</dt><dd><span class=\"status\">выключен</span></dd>"),
        "{text}"
    );
    assert!(text.contains("multiple_households"), "{text}");
    assert!(
        text.contains("в режиме single больше одного household"),
        "{text}"
    );
    assert!(text.contains("llm_slot_unverified:primary"), "{text}");
    assert!(text.contains("слот LLM primary не проверен"), "{text}");
    assert!(text.contains("Основной слот: не проверен"), "{text}");
    assert!(text.contains("Резервный слот: не настроен"), "{text}");
    assert!(
        text.contains("Основной слот: не проверен — <a href=\"/admin/llm\">Проверить</a>"),
        "an unchecked slot links to the check page: {text}"
    );

    // Активный household остаётся один, предупреждение исчезает.
    tasklerq::admin::delete_household(&db.pool, second.id, Utc::now())
        .await
        .expect("the second household is deleted");
    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("<dt>Household</dt><dd>1</dd>"), "{text}");
    assert!(!text.contains("multiple_households"), "{text}");

    // Текущий отпечаток делает слот проверенным, ненастроенный резервный
    // остаётся «не настроен».
    let primary = effective
        .primary()
        .config()
        .expect("the primary slot is configured")
        .clone();
    store_current_check(&db.pool, "primary", &primary).await;
    slots.invalidate();

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("Основной слот: проверен"), "{text}");
    assert!(!text.contains("Основной слот: не проверен"), "{text}");
    assert!(text.contains("Резервный слот: не настроен"), "{text}");
}

/// Главная не показывает секреты и пользовательское содержимое.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_home_page_never_shows_secrets_or_user_content() {
    const TELEGRAM_TOKEN: &str = "SECRET-telegram-token";
    const WEBHOOK_SECRET: &str = "SECRET-webhook-secret";
    const ENV_SLOT_TOKEN: &str = "SECRET-env-slot-token";
    const STORED_SLOT_TOKEN: &str = "SECRET-stored-slot-token";
    const HOUSEHOLD_NAME: &str = "Секретный дом";
    const MEMBER_NAME: &str = "Иван Секретов";

    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let database_url = support::database_url().await;
    let key_bytes = [0x42_u8; 32];
    let key_text = BASE64.encode(key_bytes);
    let config = Config::from_map([
        ("ENV", "development"),
        ("PUBLIC_BASE_URL", "http://127.0.0.1:18080"),
        ("DATABASE_URL", database_url.as_str()),
        ("TELEGRAM_MODE", "webhook"),
        ("TELEGRAM_BOT_TOKEN", TELEGRAM_TOKEN),
        ("TELEGRAM_WEBHOOK_SECRET", WEBHOOK_SECRET),
        (
            "TELEGRAM_WEBHOOK_URL",
            "http://127.0.0.1:18080/webhooks/telegram",
        ),
        ("TENANCY_MODE", "multi"),
        ("TASKLERQ_SECRET_KEY", key_text.as_str()),
        ("LLM_CAPTURE_ENABLED", "true"),
        ("LLM_PRIMARY_URL", "https://llm.example.test/v1"),
        ("LLM_PRIMARY_MODEL", "vendor/primary"),
        ("LLM_PRIMARY_TOKEN", ENV_SLOT_TOKEN),
    ])
    .expect("test config is valid");
    let cipher = SecretCipher::from_config(&config);

    let household = tasklerq::admin::create_household(
        &db.pool,
        TenancyMode::Multi,
        HOUSEHOLD_NAME,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("the household is created");
    tasklerq::admin::add_user(
        &db.pool,
        TenancyMode::Multi,
        Some(household.id),
        tasklerq::domain::models::UserRole::Owner,
        424_242,
        Some(MEMBER_NAME),
        Utc::now(),
    )
    .await
    .expect("the member is added");
    let issued = tasklerq::admin::create_token(
        &db.pool,
        TenancyMode::Multi,
        Some(household.id),
        "API token",
        &["capture:write".to_owned()],
        None,
        Utc::now(),
    )
    .await
    .expect("the API token is issued");

    let mut conn = db.pool.acquire().await.expect("connection");
    settings::write_secret(
        &mut conn,
        "llm.primary.token",
        &SecretString::new(STORED_SLOT_TOKEN),
        &cipher,
    )
    .await
    .expect("the stored slot secret");
    drop(conn);

    let effective = settings::effective_llm(&config.llm, &[], &cipher);
    let handle = LlmHandle::new(LlmSnapshot::from_effective(effective));
    let llm_settings =
        LlmSettings::new(db.pool.clone(), config.llm.clone(), cipher.clone(), handle);
    let app = build_admin_router(
        AdminState::new(
            db.pool.clone(),
            Vec::new(),
            Audit::new(db.pool.clone()),
            llm_settings,
        )
        .with_wizard(WizardSettings::new(
            config.tenancy.mode,
            config
                .telegram
                .as_ref()
                .expect("telegram is configured")
                .mode
                .is_enabled(),
        )),
    );
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let (status, headers, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_security_headers(&headers, "GET /admin with secrets");
    assert_no_external_urls(&body, "GET /admin with secrets");
    let text = String::from_utf8_lossy(&body);

    assert!(
        text.contains("<dt>Telegram</dt><dd><span class=\"status status-ok\">включён</span></dd>"),
        "{text}"
    );
    assert!(
        text.contains("<dt>Режим размещения</dt><dd>multi</dd>"),
        "{text}"
    );
    for secret in [
        TELEGRAM_TOKEN,
        WEBHOOK_SECRET,
        ENV_SLOT_TOKEN,
        STORED_SLOT_TOKEN,
        key_text.as_str(),
        issued.token.as_str(),
        HOUSEHOLD_NAME,
        MEMBER_NAME,
        "llm.example.test",
    ] {
        assert!(
            !text.contains(secret),
            "the home page leaked `{secret}`: {text}"
        );
    }
}

/// Число неудалённых household.
async fn household_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM households WHERE deleted_at IS NULL")
        .fetch_one(pool)
        .await
        .expect("household count")
}

/// Число действующих участников household.
async fn member_count(pool: &PgPool, household_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM users WHERE household_id = $1 AND deleted_at IS NULL")
        .bind(household_id)
        .fetch_one(pool)
        .await
        .expect("member count")
}

/// Число неотозванных токенов household.
async fn token_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM api_tokens WHERE revoked_at IS NULL")
        .fetch_one(pool)
        .await
        .expect("token count")
}

/// Открытый токен со страницы выпуска: единственное поле `issued-token`.
fn issued_token(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let field = text
        .find("id=\"issued-token\"")
        .map(|start| &text[start..])
        .expect("the page shows the token field");
    let value = field
        .find("value=\"")
        .map(|start| &field[start + "value=\"".len()..])
        .expect("the token field carries a value");
    value
        .split('"')
        .next()
        .expect("the value is quoted")
        .to_owned()
}

/// Публичный роутер с включённым разбором и подменным слотом.
fn capture_router(pool: &PgPool, ai: Arc<support::llm::BackendDouble>) -> Router {
    build_router(
        AppState::new(pool.clone(), Audit::new(pool.clone())).with_capture(CaptureState::new(
            CaptureConfig {
                enabled: true,
                voice_timeout_ms: 8_000,
                voice_fallback_raw: false,
                rate_limit_per_min: 10,
                max_text_chars: 2_000,
            },
            LlmHandle::new(LlmSnapshot::for_tests_without_settings(
                Some(Arc::new(support::llm::chain(ai, None))),
                200,
            )),
        )),
    )
}

/// Бот в режиме `single` на базе теста.
fn single_bot(pool: &PgPool, telegram_id: i64, bootstrap_owner: Option<i64>) -> Harness {
    let bot = Bot::new("tasklerqbot")
        .with_tenancy_config(TenancyConfig {
            mode: TenancyMode::Single,
            bootstrap_owner,
        })
        .with_audit(Audit::new(pool.clone()));
    Harness::with_handler(pool, telegram_id, Arc::new(bot), FakeTelegram::new())
}

/// Сценарий «только Home Assistant»: setup, household из формы, пропуск LLM и
/// однократно показанный токен, который сразу работает в `POST /api/v1/capture`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_home_assistant_only_scenario_creates_a_household_and_shows_the_token_once() {
    let db = support::isolated_db().await;
    let app = console_with(&db.pool, TenancyMode::Single, false, LlmConfig::empty());
    let setup_code = codes::issue(&db.pool, Purpose::Setup, None, Utc::now())
        .await
        .expect("a setup code");

    let (status, headers, _) = send_form(
        &app,
        "/admin/setup",
        &[
            ("code", &setup_code.code),
            ("username", "admin"),
            ("password", TEST_PASSWORD),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/wizard"));
    let cookie = cookie_pair(&set_cookie(&headers).expect("setup starts a session"));

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("action=\"/admin/wizard/household\""),
        "the household step is a form now: {text}"
    );

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/wizard/household",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("name", "Дом"), ("timezone", "Europe/Moscow")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/wizard"));

    let household_id: Uuid =
        sqlx::query_scalar("SELECT id FROM households WHERE deleted_at IS NULL")
            .fetch_one(&db.pool)
            .await
            .expect("the household row");
    assert_eq!(
        member_count(&db.pool, household_id).await,
        0,
        "the console creates the household without an owner"
    );

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/wizard/skip",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("step", "llm")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/wizard"));

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("action=\"/admin/wizard/token\""),
        "the token step is a form now: {text}"
    );
    assert!(text.contains("Токен Home Assistant"), "{text}");

    let (status, headers, body) = send_form_from(
        &app,
        "/admin/wizard/token",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("name", "")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the token is shown in the response");
    assert_security_headers(&headers, "the issued token page");
    assert_no_external_urls(&body, "the issued token page");

    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("показывается один раз"), "{text}");
    assert!(text.contains("readonly"), "{text}");
    assert!(text.contains("href=\"/admin/wizard\""), "{text}");
    let token = issued_token(&body);
    assert!(token.starts_with("tlq_"), "{token}");
    assert_eq!(
        text.matches(&token).count(),
        1,
        "the open token is shown exactly once: {text}"
    );

    let (stored_hash, scopes): (String, Vec<String>) =
        sqlx::query_as("SELECT token_hash, scopes FROM api_tokens WHERE revoked_at IS NULL")
            .fetch_one(&db.pool)
            .await
            .expect("the token row");
    assert_eq!(stored_hash, hash_token(&token), "only the hash is stored");
    assert_eq!(
        scopes,
        vec!["ha:read", "ha:write", "capture:write"],
        "the Home Assistant token carries exactly the required scopes"
    );
    let leaked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM api_tokens
          WHERE token_hash = $1 OR name = $1 OR prefix = $1",
    )
    .bind(&token)
    .fetch_one(&db.pool)
    .await
    .expect("the open token must not be stored");
    assert_eq!(leaked, 0, "the open token is not written to the table");

    let created = audit_payloads(&db.pool, "household_created").await;
    assert_eq!(created.len(), 1, "one household event: {created:?}");
    for (actor, household) in audit_actor(&db.pool, "household_created").await {
        assert_eq!(actor, "system");
        assert_eq!(household, Some(household_id));
    }
    let issued_events = audit_payloads(&db.pool, "token_created").await;
    assert_eq!(issued_events.len(), 1, "one token event: {issued_events:?}");
    for (actor, household) in audit_actor(&db.pool, "token_created").await {
        assert_eq!(actor, "system");
        assert_eq!(household, Some(household_id));
    }
    assert!(
        !issued_events[0].to_string().contains(&token),
        "the audit event must not carry the open token"
    );

    let ai = support::llm::BackendDouble::new(vec![Ok(support::llm::v4::response(vec![
        support::llm::v4::shopping("молоко"),
    ]))]);
    let public = capture_router(&db.pool, ai);
    let (status, json) = support::request(
        &public,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(serde_json::json!({
            "text": "купить молоко",
            "source": "ha_voice",
            "mode": "auto"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the issued token captures: {json}");
    assert_eq!(json["status"], "applied", "{json}");

    let (status, headers, _) = send_with_cookie(&app, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location(&headers),
        Some("/admin"),
        "the token stays issued and the wizard is finished"
    );
    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(
        !text.contains(&token),
        "the token is not shown again: {text}"
    );
    assert!(!text.contains("Продолжить настройку"), "{text}");
}

/// Шаг household: ошибка валидации сохраняет ввод, повторный POST не создаёт
/// второй household, чужой `Origin` и анонимный POST ничего не пишут.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_household_step_validates_and_creates_only_one_household() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(&db.pool, Vec::new());

    let (status, headers, _) = send_form(
        &app,
        "/admin/wizard/household",
        &[("name", "Дом"), ("timezone", "Europe/Moscow")],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "an anonymous POST goes to the login page"
    );
    assert_eq!(location(&headers), Some("/admin/login"));
    assert_eq!(household_count(&db.pool).await, 0);

    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let (status, response_headers, _) = send_form_from(
        &app,
        "/admin/wizard/household",
        test_peer(),
        headers_with(&[
            ("host", "nas:18081"),
            ("origin", "http://evil.example"),
            ("cookie", &cookie),
        ]),
        &[("name", "Дом"), ("timezone", "Europe/Moscow")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_security_headers(&response_headers, "a household form with a foreign origin");
    assert_eq!(household_count(&db.pool).await, 0);

    let (status, _, body) = send_form_from(
        &app,
        "/admin/wizard/household",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("name", "Дом <b>"), ("timezone", "Not/AZone")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a bad timezone is a form error");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("role=\"alert\""), "{text}");
    assert!(
        text.contains("Дом &#60;b&#62;"),
        "the entered name is kept and escaped: {text}"
    );
    assert!(
        !text.contains("Дом <b>"),
        "the entered name must not reach the markup raw: {text}"
    );
    assert!(
        text.contains("value=\"Not/AZone\""),
        "the entered timezone is kept: {text}"
    );
    assert_eq!(household_count(&db.pool).await, 0);

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/wizard/household",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("name", "Дом"), ("timezone", "Europe/Moscow")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/wizard"));
    assert_eq!(household_count(&db.pool).await, 1);

    let (status, _, _) = send_form_from(
        &app,
        "/admin/wizard/household",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("name", "Второй"), ("timezone", "Europe/Moscow")],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "the household step is no longer current"
    );
    assert_eq!(
        household_count(&db.pool).await,
        1,
        "a repeated POST must not create a second household"
    );
}

/// Telegram включён и household пропущен: после создания учётки дом создаёт
/// только `BOOTSTRAP_OWNER_TELEGRAM_ID`, а не первый написавший.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_household_skip_needs_the_bootstrap_owner_after_the_account_exists() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console_with(&db.pool, TenancyMode::Single, true, LlmConfig::empty());
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/wizard/skip",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("step", "household")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/wizard"));
    assert_eq!(
        household_count(&db.pool).await,
        0,
        "the skip leaves the household to the bot"
    );

    // Без bootstrap-владельца учётка закрывает правило первого написавшего.
    let stranger = support::next_telegram_id();
    let mut first = single_bot(&db.pool, stranger, None);
    first.text("/start").await;
    first.assert_screen_contains("Обратитесь к администратору");
    assert!(
        !first.screen().contains("Как назовём ваше пространство"),
        "the first sender must not onboard after the account exists:\n{}",
        first.screen()
    );
    assert_eq!(household_count(&db.pool).await, 0);

    // Явный bootstrap-владелец проходит онбординг, как и раньше.
    let telegram_id = support::next_telegram_id();
    let mut bot = single_bot(&db.pool, telegram_id, Some(telegram_id));
    bot.text("/start").await;
    bot.assert_screen_contains("Как назовём ваше пространство");
    bot.text("Семья").await;
    bot.press("Москва").await;
    bot.assert_screen_contains("Созданы списки");

    assert_eq!(household_count(&db.pool).await, 1);
    let household_id: Uuid =
        sqlx::query_scalar("SELECT id FROM households WHERE deleted_at IS NULL")
            .fetch_one(&db.pool)
            .await
            .expect("the household row");
    let owner: Option<String> = sqlx::query_scalar(
        "SELECT role::text FROM users
          WHERE household_id = $1 AND telegram_user_id = $2 AND deleted_at IS NULL",
    )
    .bind(household_id)
    .bind(telegram_id)
    .fetch_optional(&db.pool)
    .await
    .expect("the onboarding owner");
    assert_eq!(owner.as_deref(), Some("owner"));
}

/// В `multi` шагов household и токена нет: POST отклоняется без записи.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_mode_refuses_the_household_and_token_steps() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console_with(&db.pool, TenancyMode::Multi, false, LlmConfig::empty());
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let (status, _, _) = send_form_from(
        &app,
        "/admin/wizard/household",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("name", "Дом"), ("timezone", "Europe/Moscow")],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(household_count(&db.pool).await, 0);

    let (status, _, _) = send_form_from(
        &app,
        "/admin/wizard/token",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("name", "Home Assistant")],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(token_count(&db.pool).await, 0);
}

/// Сбой вычисления адреса мастера после состоявшегося входа не отнимает
/// сессию: ответ — `303` на `/admin` с cookie.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_wizard_target_after_login_still_sets_the_cookie() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(&db.pool, Vec::new());

    // Снимок мастера читает `settings`; без таблицы он падает уже после
    // проверки пароля и создания сессии.
    sqlx::query("ALTER TABLE settings RENAME TO settings_unavailable")
        .execute(&db.pool)
        .await
        .expect("the settings table is hidden from the snapshot");

    let (status, headers, _) = send_form(
        &app,
        "/admin/login",
        &[("username", "admin"), ("password", TEST_PASSWORD)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location(&headers),
        Some("/admin"),
        "the failed snapshot must not replace the session with a 500"
    );
    assert!(
        set_cookie(&headers).is_some(),
        "the login already succeeded and the cookie must be set"
    );
    assert_eq!(session_count(&db.pool).await, 1);
}

/// Тот же хвост у setup: учётка создана, сессия выдана, адрес мастера не
/// вычислился — ведём на главную.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_wizard_target_after_setup_still_sets_the_cookie() {
    let db = support::isolated_db().await;
    let app = console(&db.pool, Vec::new());
    let setup_code = codes::issue(&db.pool, Purpose::Setup, None, Utc::now())
        .await
        .expect("a setup code");

    sqlx::query("ALTER TABLE settings RENAME TO settings_unavailable")
        .execute(&db.pool)
        .await
        .expect("the settings table is hidden from the snapshot");

    let (status, headers, _) = send_form(
        &app,
        "/admin/setup",
        &[
            ("code", &setup_code.code),
            ("username", "admin"),
            ("password", TEST_PASSWORD),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin"));
    assert!(set_cookie(&headers).is_some());
    assert_eq!(account_count(&db.pool).await, 1);
    assert_eq!(session_count(&db.pool).await, 1);
}

/// Активный токен household снимает шаг токена.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_active_token_removes_the_token_step() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    insert_household(&db.pool).await;
    tasklerq::admin::create_token(
        &db.pool,
        TenancyMode::Single,
        None,
        "home-assistant",
        &["ha:read".to_owned()],
        None,
        Utc::now(),
    )
    .await
    .expect("a household token is issued");

    let app = console(&db.pool, Vec::new());
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/wizard/skip",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("step", "llm")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location(&headers),
        Some("/admin"),
        "the token step is already complete"
    );

    let (status, headers, _) = send_with_cookie(&app, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin"));
}

/// Слот наполовину: адрес резервного без модели отклоняется, недостающее поле
/// названо, снимок и строки `settings` не меняются.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_incomplete_slot_is_rejected_without_changing_the_snapshot() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let (app, settings) =
        console_with_llm(&db.pool, LlmConfig::empty(), SecretCipher::new(None, None));
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;
    let before = settings.handle().load();

    let (status, _, body) = send_form_from(
        &app,
        "/admin/llm",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("fallback_url", "http://127.0.0.1:9/v1")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "an incomplete slot is a form error");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("role=\"alert\""), "{text}");
    assert!(
        text.contains("llm.fallback.model"),
        "the error must name the missing field: {text}"
    );
    assert!(
        text.contains("value=\"http://127.0.0.1:9/v1\""),
        "the entered address is kept: {text}"
    );

    assert_eq!(
        settings.handle().load().version(),
        before.version(),
        "a rejected save must not replace the snapshot"
    );
    assert_eq!(
        setting_rows(&db.pool, &["llm.fallback.url", "llm.fallback.model"]).await,
        0,
        "no row of the rejected slot is written"
    );
}

/// Поля слота из окружения показываются только для чтения, а POST их не пишет.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_environment_slot_is_read_only_and_post_cannot_change_it() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let llm = llm_env(&[
        ("LLM_PRIMARY_URL", "https://env.example.test/v1"),
        ("LLM_PRIMARY_MODEL", "env/model"),
        ("LLM_PRIMARY_TOKEN", "env-secret-token"),
    ]);
    let (app, settings) = console_with_llm(&db.pool, llm, SecretCipher::new(None, None));
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/llm", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("https://env.example.test/v1"), "{text}");
    assert!(text.contains("env/model"), "{text}");
    assert!(text.contains("источник: окружение"), "{text}");
    for name in [
        "primary_url",
        "primary_model",
        "primary_timeout_ms",
        "primary_extra_json",
    ] {
        assert!(
            input_with_name(&text, name).contains("readonly"),
            "the environment field {name} is read only: {text}"
        );
    }
    assert!(
        !text.contains("name=\"primary_token\""),
        "a token from the environment has no replacement field: {text}"
    );
    assert!(
        !text.contains("name=\"primary_token_delete\""),
        "a token from the environment has no delete field: {text}"
    );
    assert!(text.contains("Токен: <strong>задан</strong>"), "{text}");

    let before = settings.handle().load();
    let (status, headers, _) = send_form_from(
        &app,
        "/admin/llm",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("primary_url", "https://env.example.test/v1"),
            ("primary_model", "db/model"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location(&headers),
        Some("/admin/llm"),
        "nothing changed, so no save marker is shown"
    );

    assert_eq!(
        setting_value(&db.pool, "llm.primary.model").await,
        None,
        "a model from the form must not be written over an environment slot"
    );
    let after = settings.handle().load();
    assert_eq!(after.version(), before.version());
    assert_eq!(
        after
            .effective()
            .primary()
            .config()
            .map(|config| config.model.as_str()),
        Some("env/model")
    );
}

/// Токен: в HTML только состояние, замена и удаление работают, значения нет.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_saved_token_is_never_rendered_and_can_be_replaced_or_deleted() {
    const TOKEN: &str = "tlq-console-secret-token";
    const NEW_TOKEN: &str = "tlq-replacement-secret-token";

    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let (app, settings) = console_with_llm(&db.pool, LlmConfig::empty(), cipher_with_key(7));
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let headers = headers_with(&[("cookie", cookie.as_str())]);
    let base_fields = [
        ("enabled_present", "1"),
        ("enabled", "1"),
        ("primary_url", "https://llm.example.test/v1"),
        ("primary_model", "vendor/model"),
    ];
    let mut fields = base_fields.to_vec();
    fields.push(("primary_token", TOKEN));
    let (status, response_headers, _) =
        send_form_from(&app, "/admin/llm", test_peer(), headers.clone(), &fields).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&response_headers), Some("/admin/llm?saved=1"));

    let (status, _, body) =
        send_with_cookie(&app, Method::GET, "/admin/llm?saved=1", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(
        !text.contains(TOKEN),
        "the token leaks into the page: {text}"
    );
    assert!(text.contains("Токен: <strong>задан</strong>"), "{text}");
    assert!(
        text.contains("Настройки LLM сохранены."),
        "the save marker is shown: {text}"
    );

    let blob: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT secret FROM settings WHERE key = 'llm.primary.token'")
            .fetch_one(&db.pool)
            .await
            .expect("the secret row");
    let blob = blob.expect("the token is stored encrypted");
    assert!(
        !blob
            .windows(TOKEN.len())
            .any(|window| window == TOKEN.as_bytes()),
        "the open token must not be stored"
    );
    assert_eq!(
        settings
            .handle()
            .load()
            .effective()
            .primary()
            .config()
            .and_then(|config| config.token.as_ref())
            .map(|token| token.expose().to_owned()),
        Some(TOKEN.to_owned()),
        "the snapshot uses the saved token"
    );

    let mut fields = base_fields.to_vec();
    fields.push(("primary_token", NEW_TOKEN));
    let (status, response_headers, _) =
        send_form_from(&app, "/admin/llm", test_peer(), headers.clone(), &fields).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&response_headers), Some("/admin/llm?saved=1"));
    assert_eq!(
        settings
            .handle()
            .load()
            .effective()
            .primary()
            .config()
            .and_then(|config| config.token.as_ref())
            .map(|token| token.expose().to_owned()),
        Some(NEW_TOKEN.to_owned()),
        "the replacement token is applied without a restart"
    );
    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/llm", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(!text.contains(NEW_TOKEN), "{text}");

    let mut fields = base_fields.to_vec();
    fields.push(("primary_token_delete", "1"));
    let (status, response_headers, _) =
        send_form_from(&app, "/admin/llm", test_peer(), headers.clone(), &fields).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&response_headers), Some("/admin/llm?saved=1"));
    assert_eq!(
        settings.handle().load().effective().primary().token_state(),
        tasklerq::settings::SecretState::NotSet,
        "the deleted token is gone from the snapshot"
    );
    assert_eq!(
        setting_rows(&db.pool, &["llm.primary.token"]).await,
        0,
        "the deleted secret row is gone"
    );

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/llm", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&body).contains("Токен: <strong>не задан</strong>"),
        "{}",
        String::from_utf8_lossy(&body)
    );
}

/// Без ключа шифрования токен отклоняется, остальные поля сохраняются.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_secret_key_rejects_the_token_but_saves_the_other_fields() {
    const TOKEN: &str = "tlq-token-without-a-key";

    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let (app, settings) =
        console_with_llm(&db.pool, LlmConfig::empty(), SecretCipher::new(None, None));
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/llm",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("enabled_present", "1"),
            ("enabled", "1"),
            ("primary_url", "https://llm.example.test/v1"),
            ("primary_model", "vendor/model"),
            ("primary_token", TOKEN),
            ("daily_quota", "321"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location(&headers),
        Some("/admin/llm?saved=1&secret=key_missing"),
        "the other fields are saved and the token is rejected"
    );

    assert_eq!(
        setting_value(&db.pool, "llm.daily_quota").await,
        Some(serde_json::json!("321")),
        "the quota is saved without a key"
    );
    assert_eq!(
        setting_rows(&db.pool, &["llm.primary.token"]).await,
        0,
        "the token is not written without a key"
    );
    let snapshot = settings.handle().load();
    assert_eq!(snapshot.quota(), 321, "the quota is applied");
    assert_eq!(
        snapshot.effective().primary().token_state(),
        tasklerq::settings::SecretState::NotSet
    );

    let (status, _, body) = send_with_cookie(
        &app,
        Method::GET,
        "/admin/llm?saved=1&secret=key_missing",
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("TASKLERQ_SECRET_KEY"), "{text}");
    assert!(text.contains("Токен не сохранён"), "{text}");
    assert!(text.contains("Настройки LLM сохранены."), "{text}");
    assert!(
        !text.contains(TOKEN),
        "the rejected token is not echoed: {text}"
    );
}

/// Смена модели и квоты действует на следующий снимок без перезапуска.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_and_quota_change_take_effect_without_a_restart() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let (app, settings) =
        console_with_llm(&db.pool, LlmConfig::empty(), SecretCipher::new(None, None));
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/llm",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("enabled_present", "1"),
            ("enabled", "1"),
            ("primary_url", "https://llm.example.test/v1"),
            ("primary_model", "first/model"),
            ("daily_quota", "200"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/llm?saved=1"));
    let first = settings.handle().load();
    assert_eq!(
        first
            .effective()
            .primary()
            .config()
            .map(|config| config.model.clone()),
        Some("first/model".to_owned())
    );
    assert_eq!(first.quota(), 200);

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/llm",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("enabled_present", "1"),
            ("enabled", "1"),
            ("primary_model", "second/model"),
            ("daily_quota", "300"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/llm?saved=1"));

    let second = settings.handle().load();
    assert!(
        second.version() > first.version(),
        "the snapshot version grows"
    );
    assert!(second.chain().is_some(), "the chain is rebuilt");
    assert_eq!(
        second
            .effective()
            .primary()
            .config()
            .map(|config| config.model.clone()),
        Some("second/model".to_owned()),
        "the next snapshot uses the new model"
    );
    assert_eq!(second.quota(), 300, "the new quota is in the snapshot");
    assert_eq!(
        setting_value(&db.pool, "llm.primary.model").await,
        Some(serde_json::json!("second/model"))
    );
}

/// Отменённое ожидание сохранения не оставляет базу и снимок расходящимися.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_save_still_finishes_with_a_matching_snapshot() {
    let db = support::isolated_db().await;
    let handle = LlmHandle::empty();
    let settings = LlmSettings::new(
        db.pool.clone(),
        LlmConfig::empty(),
        SecretCipher::new(None, None),
        handle.clone(),
    );

    let result = tokio::time::timeout(
        Duration::from_millis(1),
        settings.apply(&[LlmChange::set("llm.daily_quota", serde_json::json!(300))]),
    )
    .await;
    assert!(result.is_err(), "the caller's wait is cancelled");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if setting_value(&db.pool, "llm.daily_quota").await == Some(serde_json::json!(300))
            && handle.load().quota() == 300
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the detached save never reached the database and the snapshot"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(
        setting_value(&db.pool, "llm.daily_quota").await,
        Some(serde_json::json!(300)),
        "the snapshot matches the committed row"
    );
    assert_eq!(handle.load().quota(), 300);
}

/// Успешное сохранение пишет `admin_setting_changed` без значений секретов.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_saved_llm_form_is_audited_without_secret_values() {
    const TOKEN: &str = "tlq-audited-secret-token";

    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let (app, _) = console_with_llm(&db.pool, LlmConfig::empty(), cipher_with_key(9));
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let (status, _, _) = send_form_from(
        &app,
        "/admin/llm",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("enabled_present", "1"),
            ("enabled", "1"),
            ("primary_url", "https://llm.example.test/v1"),
            ("primary_model", "vendor/model"),
            ("primary_token", TOKEN),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let payloads = audit_payloads(&db.pool, "admin_setting_changed").await;
    let keys: Vec<&str> = payloads
        .iter()
        .filter_map(|payload| payload["key"].as_str())
        .collect();
    for expected in [
        "llm.enabled",
        "llm.primary.url",
        "llm.primary.model",
        "llm.primary.token",
    ] {
        assert!(
            keys.contains(&expected),
            "missing audit for {expected}: {payloads:?}"
        );
    }
    let joined = payloads.iter().map(Value::to_string).collect::<String>();
    assert!(
        !joined.contains(TOKEN),
        "the audit payload leaked the token: {joined}"
    );
    let token_event = payloads
        .iter()
        .find(|payload| payload["key"] == "llm.primary.token")
        .expect("the token change is audited");
    assert_eq!(token_event["value"], "set");
    for (actor, household) in audit_actor(&db.pool, "admin_setting_changed").await {
        assert_eq!(actor, "system");
        assert_eq!(household, None);
    }
}

/// Чужой `Origin` на странице LLM — `403`, без сессии — `303` на вход.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_llm_page_checks_the_origin_and_the_session() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let (app, _) = console_with_llm(&db.pool, LlmConfig::empty(), SecretCipher::new(None, None));

    let (status, headers, _) = send(&app, Method::GET, "/admin/llm").await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "an anonymous GET goes to the login page"
    );
    assert_eq!(location(&headers), Some("/admin/login"));

    let (status, headers, _) = send_form(&app, "/admin/llm", &[("daily_quota", "300")]).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "an anonymous POST goes to the login page"
    );
    assert_eq!(location(&headers), Some("/admin/login"));
    assert_eq!(
        setting_value(&db.pool, "llm.daily_quota").await,
        None,
        "an anonymous POST writes nothing"
    );

    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;
    let (status, response_headers, _) = send_form_from(
        &app,
        "/admin/llm",
        test_peer(),
        headers_with(&[
            ("host", "nas:18081"),
            ("origin", "http://evil.example"),
            ("cookie", &cookie),
        ]),
        &[("daily_quota", "300")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_security_headers(&response_headers, "a foreign origin on the llm form");
    assert_eq!(setting_value(&db.pool, "llm.daily_quota").await, None);

    let (status, headers, body) = send_with_cookie(&app, Method::GET, "/admin/llm", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_security_headers(&headers, "GET /admin/llm");
    assert_no_external_urls(&body, "GET /admin/llm");
    assert_local_assets(&body, "GET /admin/llm");
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("<form method=\"post\" action=\"/admin/llm\">"),
        "the form is a plain POST to the console path: {text}"
    );
}

/// Проверка из консоли: кнопка, ход, отчёт, отпечаток CLI и отказы.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_check_from_the_console_shows_progress_and_saves_the_cli_fingerprint() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let server = MockServer::start().await;
    mount_smoke_answers(
        &server,
        Duration::from_millis(60),
        // 1150 мс при таймауте 1000 мс: задержка не мигает на границе
        // округления `{:.1}` под нагрузкой.
        Some(("simple-1", Duration::from_millis(1_150))),
    )
    .await;
    let (app, _) = console_with_server(&db.pool, &server, 1_000);
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    // Заполнить кеш статусов до прогона: после проверки главная должна
    // увидеть новую строку, не дожидаясь `CACHE_TTL`.
    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&body).contains("Основной слот: не проверен"),
        "the slot starts unchecked: {}",
        String::from_utf8_lossy(&body)
    );

    // Кнопка есть только у настроенного слота и показывает число фраз набора.
    let (status, headers, body) = send_with_cookie(&app, Method::GET, "/admin/llm", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_security_headers(&headers, "GET /admin/llm with a configured slot");
    assert_local_assets(&body, "GET /admin/llm with a configured slot");
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("action=\"/admin/llm/check/primary\""),
        "the configured slot has a check button: {text}"
    );
    assert!(
        text.contains("Проверить — 22 фразы"),
        "the button shows the built-in set size: {text}"
    );
    assert!(
        !text.contains("action=\"/admin/llm/check/fallback\""),
        "an unconfigured slot has no check button: {text}"
    );

    // Чужой Origin отвергается слоем роутера, анонимный POST ведёт на вход.
    let (status, _, _) = send_form_from(
        &app,
        "/admin/llm/check/primary",
        test_peer(),
        headers_with(&[
            ("host", "nas:18081"),
            ("origin", "http://evil.example"),
            ("cookie", &cookie),
        ]),
        &[("titles", "strict")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, headers, _) =
        send_form(&app, "/admin/llm/check/primary", &[("titles", "strict")]).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/login"));
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty(),
        "a refused check must not reach the LLM"
    );

    // POST запускает прогон и ведёт на страницу хода.
    let (status, headers, _) = start_check(&app, &cookie, "primary", "strict").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/llm/check"));

    // Страница хода обновляется опросом htmx; без JavaScript — <noscript>-
    // ссылкой «Обновить». Заголовка `Refresh` нет: иначе браузер с JavaScript
    // перезагружал бы страницу параллельно с опросом.
    let (status, headers, body) =
        send_with_cookie(&app, Method::GET, "/admin/llm/check", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_security_headers(&headers, "GET /admin/llm/check");
    assert!(
        headers.get(header::REFRESH).is_none(),
        "a running check must not also refresh the page via a response header"
    );
    assert_no_external_urls(&body, "GET /admin/llm/check");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("Проверка слота primary идёт"), "{text}");
    assert!(text.contains("из 22 фраз"), "{text}");
    assert!(
        text.contains("<noscript><p><a href=\"/admin/llm/check\">Обновить</a></p></noscript>"),
        "without JavaScript the page offers a manual refresh link: {text}"
    );
    assert!(
        !text.contains("http-equiv=\"refresh\""),
        "the body must not carry a meta refresh: {text}"
    );
    assert!(text.contains("hx-trigger=\"every 2s\""), "{text}");

    // Ход растёт: первая фраза медленная, следующие отвечают по очереди.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (_, _, body) = send_with_cookie(&app, Method::GET, "/admin/llm/check", &cookie).await;
        let text = String::from_utf8_lossy(&body).into_owned();
        if text.contains("идёт")
            && let Some(start) = text.find("пройдено ")
        {
            let done: usize = text[start + "пройдено ".len()..]
                .split(' ')
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            if done > 0 {
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the check page never showed live progress: {text}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let (status, headers, body) =
        send_with_cookie(&app, Method::GET, "/admin/llm/check-fragment", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_security_headers(&headers, "GET /admin/llm/check-fragment");
    assert_no_external_urls(&body, "GET /admin/llm/check-fragment");
    let fragment = String::from_utf8_lossy(&body);
    assert!(
        fragment.contains("hx-get=\"/admin/llm/check-fragment\""),
        "{fragment}"
    );

    // Отчёт: провал с фактическим временем и оценкой задержки; опрос прекращён.
    let text = wait_for_check_report(&app, &cookie).await;
    assert!(text.contains("пройдено 21 из 22"), "{text}");
    assert!(text.contains("simple-1"), "{text}");
    assert!(text.contains("timeout: ответ за 1."), "{text}");
    assert!(text.contains("Задержка p95:"), "{text}");
    assert!(
        !text.contains("hx-trigger=\"every 2s\""),
        "a finished check must stop polling: {text}"
    );

    // Главная использует общий кеш статусов: после прогона строка проверки
    // видна сразу, а не через минуту. Проверка здесь заведомо не прошла из-за
    // медленной фразы, поэтому статус — «не прошёл проверку», а не «не проверен».
    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let home = String::from_utf8_lossy(&body);
    assert!(
        home.contains("Основной слот: не прошёл проверку"),
        "the console check must invalidate the cached status: {home}"
    );
    assert!(
        !home.contains("Основной слот: не проверен"),
        "the previous status must not survive the check: {home}"
    );

    let console_fingerprint = saved_fingerprint(&db.pool, "primary").await;
    let console_checked_at = saved_checked_at(&db.pool, "primary").await;

    // Ненастроенный резервный слот: POST отклонён без обращения к LLM.
    let requests_before = server.received_requests().await.expect("requests").len();
    let (status, _, _) = start_check(&app, &cookie, "fallback", "strict").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        requests_before,
        "an unconfigured slot must not call the LLM"
    );

    // Тот же слот и стандарт в `capture-eval` дают тот же отпечаток.
    let database_url = command_database_url(&db.pool).await;
    let url = format!("{}/v1", server.uri());
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_tasklerq"))
        .env("DATABASE_URL", &database_url)
        .env("LLM_CAPTURE_ENABLED", "false")
        .env("LLM_PRIMARY_URL", &url)
        .env("LLM_PRIMARY_MODEL", "test/model")
        .env("LLM_PRIMARY_TIMEOUT_MS", "1000")
        .env("RUST_LOG", "error")
        .args(["capture-eval", "--slot", "primary"])
        .stdin(Stdio::null())
        .output()
        .await
        .expect("the CLI should run");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let cli_text = String::from_utf8_lossy(&output.stdout);
    assert!(cli_text.contains("timeout"), "{cli_text}");
    assert_eq!(
        saved_fingerprint(&db.pool, "primary").await,
        console_fingerprint,
        "the console and capture-eval save the same check fingerprint"
    );
    assert!(
        saved_checked_at(&db.pool, "primary").await > console_checked_at,
        "capture-eval must rewrite the row, not just leave an equal fingerprint"
    );
}

/// Повторное нажатие во время прогона не запускает второй.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_check_post_while_running_does_not_start_another_run() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let server = MockServer::start().await;
    mount_smoke_answers(&server, Duration::from_millis(150), None).await;

    // Оба слота настроены на один сервер: второй POST по другому слоту тоже
    // должен вести на ход первого прогона.
    let primary_url = format!("{}/v1", server.uri());
    let llm = llm_env(&[
        ("LLM_CAPTURE_ENABLED", "true"),
        ("LLM_PRIMARY_URL", &primary_url),
        ("LLM_PRIMARY_MODEL", "test/model"),
        ("LLM_PRIMARY_TIMEOUT_MS", "10000"),
        ("LLM_FALLBACK_URL", &primary_url),
        ("LLM_FALLBACK_MODEL", "test/model"),
        ("LLM_FALLBACK_TIMEOUT_MS", "10000"),
    ]);
    let (app, _) = console_with_llm(&db.pool, llm, SecretCipher::new(None, None));
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let (status, headers, _) = start_check(&app, &cookie, "primary", "strict").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/llm/check"));

    let (status, headers, _) = start_check(&app, &cookie, "fallback", "strict").await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "a second press does not start another run"
    );
    assert_eq!(location(&headers), Some("/admin/llm/check"));

    let text = wait_for_check_report(&app, &cookie).await;
    assert!(text.contains("Проверка слота primary завершена"), "{text}");
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(
        requests.len(),
        22,
        "only one run reached the LLM, not both slots"
    );
}

/// Блокировку держит другой процесс: страница говорит «проверка уже идёт».
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_check_held_by_another_process_reports_busy_without_calling_the_llm() {
    use sqlx::Connection;

    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let server = MockServer::start().await;
    mount_smoke_answers(&server, Duration::ZERO, None).await;
    let (app, _) = console_with_server(&db.pool, &server, 10_000);
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    // Отдельное соединение, как у `capture-eval` в другом процессе.
    let database_url = command_database_url(&db.pool).await;
    let mut lock = sqlx::PgConnection::connect(&database_url)
        .await
        .expect("an independent connection");
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(slot_check::CHECK_LOCK_KEY)
        .fetch_one(&mut lock)
        .await
        .expect("the test takes the check lock");
    assert!(acquired);

    let (status, headers, _) = start_check(&app, &cookie, "primary", "strict").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/llm/check"));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let text = loop {
        let (status, _, body) =
            send_with_cookie(&app, Method::GET, "/admin/llm/check", &cookie).await;
        assert_eq!(status, StatusCode::OK);
        let text = String::from_utf8_lossy(&body).into_owned();
        if text.contains("проверка уже идёт") {
            break text;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the busy check was never reported: {text}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(text.contains("Слот не проверен"), "{text}");
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty(),
        "a check locked by another process must not reach the LLM"
    );

    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(slot_check::CHECK_LOCK_KEY)
        .execute(&mut lock)
        .await
        .expect("unlock");
}

/// Токен слота сохраняется без пробелов по краям.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slot_token_is_saved_without_surrounding_spaces() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let (app, settings) = console_with_llm(&db.pool, LlmConfig::empty(), cipher_with_key(5));
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/llm",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("enabled_present", "1"),
            ("enabled", "1"),
            ("primary_url", "https://llm.example.test/v1"),
            ("primary_model", "vendor/model"),
            ("primary_token", "  tlq-trimmed-token \t"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/llm?saved=1"));
    assert_eq!(
        settings
            .handle()
            .load()
            .effective()
            .primary()
            .config()
            .and_then(|config| config.token.as_ref())
            .map(|token| token.expose().to_owned()),
        Some("tlq-trimmed-token".to_owned()),
        "a trailing space would break the Authorization header"
    );
}

/// POST без служебного поля флажка не выключает разбор.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_post_without_the_enabled_marker_keeps_capture_enabled() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let (app, settings) =
        console_with_llm(&db.pool, LlmConfig::empty(), SecretCipher::new(None, None));
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let (status, _, _) = send_form_from(
        &app,
        "/admin/llm",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("enabled_present", "1"),
            ("enabled", "1"),
            ("primary_url", "https://llm.example.test/v1"),
            ("primary_model", "vendor/model"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(
        settings.handle().load().effective().enabled(),
        "the first save enables capture"
    );

    // Неполный POST: только квота. Разбор обязан остаться включённым.
    let (status, _, _) = send_form_from(
        &app,
        "/admin/llm",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("daily_quota", "321")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(
        settings.handle().load().effective().enabled(),
        "a partial POST must not switch capture off"
    );
    assert_eq!(
        setting_value(&db.pool, "llm.enabled").await,
        Some(json!(true))
    );

    // Браузерная форма со снятым флажком присылает служебное поле: выключает.
    let (status, _, _) = send_form_from(
        &app,
        "/admin/llm",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("enabled_present", "1"), ("daily_quota", "321")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(
        !settings.handle().load().effective().enabled(),
        "an unchecked box in the browser form switches capture off"
    );
}

/// Отмена запроса сохранения не теряет событие аудита.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_llm_save_still_writes_the_audit_event() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let (app, _) = console_with_llm(&db.pool, LlmConfig::empty(), SecretCipher::new(None, None));
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    // Блокируем запись в settings: обработчик успевает дойти до применения, а
    // затем его ожидание отменяется. Отсоединённая задача должна дописать аудит.
    let mut blocker = db.pool.begin().await.expect("blocker transaction");
    sqlx::query("LOCK TABLE settings IN EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .expect("the settings table is locked for writing");

    let task_app = app.clone();
    let task_cookie = cookie.clone();
    let request = tokio::spawn(async move {
        send_form_from(
            &task_app,
            "/admin/llm",
            test_peer(),
            headers_with(&[("cookie", &task_cookie)]),
            &[
                ("enabled_present", "1"),
                ("enabled", "1"),
                ("primary_url", "https://llm.example.test/v1"),
                ("primary_model", "vendor/model"),
                ("daily_quota", "333"),
            ],
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(400)).await;
    request.abort();
    let cancelled = request.await;
    assert!(
        cancelled.is_err() || cancelled.expect("the request finished").0 == StatusCode::SEE_OTHER,
        "the test cancels the request while the save is blocked"
    );
    blocker.rollback().await.expect("release the settings lock");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let payloads = audit_payloads(&db.pool, "admin_setting_changed").await;
        if payloads
            .iter()
            .any(|payload| payload["key"] == "llm.daily_quota")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "a cancelled save lost its audit event: {payloads:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Секрет, который нельзя расшифровать, просит ввести токен заново.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_secret_that_cannot_be_decrypted_asks_for_a_new_token() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;

    let encrypter = cipher_with_key(3);
    let mut conn = db.pool.acquire().await.expect("connection");
    settings::write_value(
        &mut conn,
        "llm.primary.url",
        json!("https://llm.example.test/v1"),
    )
    .await
    .expect("the slot url is written");
    settings::write_value(&mut conn, "llm.primary.model", json!("vendor/model"))
        .await
        .expect("the slot model is written");
    settings::write_secret(
        &mut conn,
        "llm.primary.token",
        &tasklerq::config::SecretString::new("tlq-encrypted-token"),
        &encrypter,
    )
    .await
    .expect("the slot token is written");
    drop(conn);

    // Чужой ключ: шифротекст не читается.
    let (app, _) = console_with_llm(&db.pool, LlmConfig::empty(), cipher_with_key(4));
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;
    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/llm", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("не расшифровывается — введите токен заново"),
        "{text}"
    );
    assert!(!text.contains("tlq-encrypted-token"), "{text}");

    // Ключа нет вовсе.
    let (app, _) = console_with_llm(&db.pool, LlmConfig::empty(), SecretCipher::new(None, None));
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;
    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/llm", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("ключ шифрования не задан — введите токен заново"),
        "{text}"
    );
}

/// Отказ токена без ключа аудируется только несекретными полями.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_rejected_without_a_key_audits_only_plain_fields() {
    const TOKEN: &str = "tlq-rejected-secret-token";

    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let (app, _) = console_with_llm(&db.pool, LlmConfig::empty(), SecretCipher::new(None, None));
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/llm",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("enabled_present", "1"),
            ("enabled", "1"),
            ("primary_url", "https://llm.example.test/v1"),
            ("primary_model", "vendor/model"),
            ("primary_token", TOKEN),
            ("daily_quota", "322"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location(&headers),
        Some("/admin/llm?saved=1&secret=key_missing")
    );

    let payloads = audit_payloads(&db.pool, "admin_setting_changed").await;
    let keys: Vec<&str> = payloads
        .iter()
        .filter_map(|payload| payload["key"].as_str())
        .collect();
    for expected in [
        "llm.enabled",
        "llm.primary.url",
        "llm.primary.model",
        "llm.daily_quota",
    ] {
        assert!(
            keys.contains(&expected),
            "the saved plain field {expected} is audited: {payloads:?}"
        );
    }
    assert!(
        !keys.contains(&"llm.primary.token"),
        "a rejected token has no audit event: {payloads:?}"
    );
    let joined = payloads.iter().map(Value::to_string).collect::<String>();
    assert!(
        !joined.contains(TOKEN),
        "the audit leaked the token: {joined}"
    );
}

/// Шаг LLM мастера ведёт на страницу, а сохранённый слот завершает шаг.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_wizard_llm_step_leads_to_the_llm_page_and_completes_on_save() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    insert_household(&db.pool).await;
    let (app, settings) =
        console_with_llm(&db.pool, LlmConfig::empty(), SecretCipher::new(None, None));
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("href=\"/admin/llm\"") && text.contains("Настроить разбор"),
        "the llm step links to its page: {text}"
    );

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/llm",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("enabled_present", "1"),
            ("enabled", "1"),
            ("primary_url", "https://llm.example.test/v1"),
            ("primary_model", "vendor/model"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/llm?saved=1"));
    assert!(
        settings
            .handle()
            .load()
            .effective()
            .primary()
            .is_configured(),
        "the saved slot is effective"
    );

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("action=\"/admin/wizard/token\""),
        "the wizard moved past the llm step: {text}"
    );
    assert!(!text.contains("Настроить разбор"), "{text}");
}

/// Шаг токена: чужие и анонимные запросы, длина имени и отсутствие повторного
/// выпуска. Токен не получает cookie.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_token_step_refuses_repeats_and_foreign_requests() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    insert_household(&db.pool).await;
    let app = console_with(&db.pool, TenancyMode::Single, false, LlmConfig::empty());
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let (status, _, _) = send_form_from(
        &app,
        "/admin/wizard/skip",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("step", "llm")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (status, headers, _) = send_form(&app, "/admin/wizard/token", &[("name", "")]).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "an anonymous POST goes to the login page"
    );
    assert_eq!(location(&headers), Some("/admin/login"));
    assert_eq!(token_count(&db.pool).await, 0);

    let (status, response_headers, _) = send_form_from(
        &app,
        "/admin/wizard/token",
        test_peer(),
        headers_with(&[
            ("host", "nas:18081"),
            ("origin", "http://evil.example"),
            ("cookie", &cookie),
        ]),
        &[("name", "")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_security_headers(&response_headers, "a token POST with a foreign origin");
    assert_eq!(token_count(&db.pool).await, 0);

    let too_long = "x".repeat(65);
    let (status, headers, body) = send_form_from(
        &app,
        "/admin/wizard/token",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("name", &too_long)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a too long name is a form error");
    assert_security_headers(&headers, "the token form error");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("role=\"alert\""), "{text}");
    assert_eq!(token_count(&db.pool).await, 0);

    let (status, headers, body) = send_form_from(
        &app,
        "/admin/wizard/token",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("name", "Home Assistant")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        set_cookie(&headers).is_none(),
        "the token page must not receive a session cookie"
    );
    let token = issued_token(&body);
    assert!(token.starts_with("tlq_"), "{token}");
    assert_eq!(token_count(&db.pool).await, 1);

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/wizard/token",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("name", "Second")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin"));
    assert_eq!(
        token_count(&db.pool).await,
        1,
        "a repeated POST must not issue a second token"
    );
}

/// Ключ шифрования для настроек Telegram в тестах страницы.
const TELEGRAM_TEST_SECRET_KEY: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";

/// Конфигурация приложения с подменным Bot API и изолированной базой.
async fn telegram_config(
    pool: &PgPool,
    server: Option<&MockServer>,
    extra: &[(&str, &str)],
) -> Config {
    let database_url = command_database_url(pool).await;
    let mut vars: Vec<(String, String)> = vec![
        ("ENV".to_owned(), "development".to_owned()),
        ("DATABASE_URL".to_owned(), database_url),
        ("LLM_CAPTURE_ENABLED".to_owned(), "false".to_owned()),
        ("BIND_ADDR".to_owned(), "127.0.0.1:0".to_owned()),
        ("ADMIN_BIND_ADDR".to_owned(), "127.0.0.1:0".to_owned()),
        (
            "TASKLERQ_SECRET_KEY".to_owned(),
            TELEGRAM_TEST_SECRET_KEY.to_owned(),
        ),
    ];
    if let Some(server) = server {
        vars.push(("TELEGRAM_API_BASE_URL".to_owned(), server.uri()));
    }
    vars.extend(
        extra
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    );
    Config::from_map(vars).expect("the telegram test environment is valid")
}

fn bot_path(method_name: &str) -> String {
    format!(r"^/bot[^/]+/{method_name}$")
}

/// Ответы Bot API, не зависящие от теста; `getMe` и `getWebhookInfo` монтируются
/// отдельно, чтобы тест задавал их результат.
async fn mount_bot_base(server: &MockServer) {
    for method_name in [
        "setWebhook",
        "deleteWebhook",
        "sendMessage",
        "sendRichMessage",
        "editMessageText",
        "deleteMessage",
        "answerCallbackQuery",
    ] {
        Mock::given(method("POST"))
            .and(path_regex(bot_path(method_name)))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": true, "result": {"message_id": 1}})),
            )
            .mount(server)
            .await;
    }
}

async fn mount_get_me(server: &MockServer, id: i64, username: &str) {
    Mock::given(method("POST"))
        .and(path_regex(bot_path("getMe")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {"id": id, "is_bot": true, "first_name": "Tasklerq", "username": username}
        })))
        .mount(server)
        .await;
}

async fn mount_webhook_info(server: &MockServer, pending: u32, error: Option<(&str, i64)>) {
    let mut result = json!({
        "url": "https://tasklerq.example.com/webhooks/telegram",
        "pending_update_count": pending,
    });
    if let Some((message, date)) = error {
        result["last_error_message"] = json!(message);
        result["last_error_date"] = json!(date);
    }
    Mock::given(method("POST"))
        .and(path_regex(bot_path("getWebhookInfo")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": result})),
        )
        .mount(server)
        .await;
}

async fn bot_requests(server: &MockServer, method_name: &str) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .expect("requests")
        .into_iter()
        .filter(|request| request.url.path().ends_with(&format!("/{method_name}")))
        .collect()
}

/// Запустить приложение и войти в консоль.
async fn telegram_app(
    config: &Config,
    pool: &PgPool,
) -> (
    Application,
    Router,
    String,
    tokio::sync::watch::Sender<bool>,
) {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(config, pool.clone(), shutdown_rx)
        .await
        .expect("application with the admin console");
    insert_account(pool, "admin").await;
    let app = application
        .admin_router
        .clone()
        .expect("the admin console router");
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;
    (application, app, cookie, shutdown_tx)
}

async fn stop_application(application: Application, shutdown_tx: tokio::sync::watch::Sender<bool>) {
    let _ = shutdown_tx.send(true);
    for worker in application.workers {
        tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .expect("worker must stop on shutdown")
            .expect("worker task must join");
    }
}

fn webhook_router(application: &Application) -> Router {
    application
        .router
        .clone()
        .layer(MockConnectInfo(SocketAddr::from(([172, 18, 0, 1], 4040))))
}

fn telegram_update(update_id: i64, chat_id: i64, text: &str) -> Value {
    json!({
        "update_id": update_id,
        "message": {
            "message_id": update_id,
            "date": 1_700_000_000,
            "chat": {"id": chat_id, "type": "private", "first_name": "Alice"},
            "from": {"id": chat_id, "is_bot": false, "first_name": "Alice"},
            "text": text
        }
    })
}

async fn post_webhook(app: &Router, secret: &str, update: Value) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/telegram")
                .header("x-telegram-bot-api-secret-token", secret)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&update).expect("json")))
                .expect("request"),
        )
        .await
        .expect("response")
        .status()
}

fn submitted_secret(request: &wiremock::Request) -> String {
    let body: Value = serde_json::from_slice(&request.body).expect("setWebhook body");
    body["secret_token"]
        .as_str()
        .expect("setWebhook secret_token")
        .to_owned()
}

fn submitted_webhook_url(request: &wiremock::Request) -> String {
    let body: Value = serde_json::from_slice(&request.body).expect("setWebhook body");
    body["url"].as_str().expect("setWebhook url").to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_rejected_by_telegram_is_not_saved() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    Mock::given(method("POST"))
        .and(path_regex(bot_path("getMe")))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "ok": false,
            "error_code": 401,
            "description": "Unauthorized"
        })))
        .mount(&server)
        .await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;

    let (status, _, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", "123:definitely-not-accepted"),
            ("public_base_url", "https://tasklerq.example.com"),
        ],
    )
    .await;

    let text = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert!(text.contains("Токен не принят Telegram."), "{text}");
    assert_eq!(
        setting_rows(&db.pool, &["telegram.mode", "telegram.bot_token"]).await,
        0,
        "a rejected token must not write any telegram setting"
    );
    assert!(
        application.telegram_supervisor.ingress().load().is_none(),
        "a rejected token must not switch the running bot"
    );
    assert!(
        bot_requests(&server, "setWebhook").await.is_empty(),
        "a rejected token must not register a webhook"
    );

    stop_application(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connecting_a_bot_registers_the_webhook_and_shows_its_state() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    mount_get_me(&server, 555_001, "tasklerq_console_bot").await;
    mount_webhook_info(
        &server,
        3,
        Some(("Wrong response from the webhook: 500", 1_700_000_000)),
    )
    .await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;
    let token = "123:SECRET-console-bot-token";

    let (status, headers, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", token),
            ("public_base_url", "https://tasklerq.example.com"),
        ],
    )
    .await;
    let text = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::SEE_OTHER, "{text}");
    assert_eq!(
        location(&headers),
        Some("/admin/telegram?saved=1"),
        "a successful save returns to the page"
    );

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/telegram", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("tasklerq_console_bot"), "{text}");
    assert!(
        text.contains("https://tasklerq.example.com/webhooks/telegram"),
        "{text}"
    );
    assert!(text.contains("Ожидают обработки</dt><dd>3"), "{text}");
    assert!(
        text.contains("Wrong response from the webhook: 500"),
        "{text}"
    );
    assert!(
        !text.contains(token) && !text.contains("SECRET-console-bot-token"),
        "the token must not reach the page"
    );

    let requests = bot_requests(&server, "setWebhook").await;
    assert_eq!(requests.len(), 1, "the webhook must be registered once");
    assert_eq!(
        submitted_webhook_url(&requests[0]),
        "https://tasklerq.example.com/webhooks/telegram"
    );
    let secret = submitted_secret(&requests[0]);
    assert!(
        secret.len() >= 43
            && secret
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'),
        "the generated secret must be base64url of 32 bytes: {secret}"
    );

    let webhook_app = webhook_router(&application);
    assert_eq!(
        post_webhook(
            &webhook_app,
            &secret,
            telegram_update(90_001, 777_001, "/start"),
        )
        .await,
        StatusCode::OK,
        "the new secret must be accepted without a restart"
    );

    stop_application(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_http_public_address_is_rejected_in_production() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[("ENV", "production")]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;

    let (status, _, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", "123:any-token"),
            ("public_base_url", "http://tasklerq.example.com"),
        ],
    )
    .await;

    let text = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert!(
        text.contains("https://"),
        "the rejection must name the HTTPS requirement: {text}"
    );
    assert_eq!(
        setting_rows(&db.pool, &["telegram.mode", "telegram.bot_token"]).await,
        0,
        "a rejected address must not write settings"
    );

    stop_application(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_environment_token_makes_the_page_read_only() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    mount_webhook_info(&server, 0, None).await;
    let config = telegram_config(
        &db.pool,
        Some(&server),
        &[
            ("TELEGRAM_BOT_TOKEN", "123:SECRET-env-token"),
            ("TELEGRAM_MODE", "webhook"),
            ("TELEGRAM_WEBHOOK_SECRET", "env-webhook-secret"),
            (
                "TELEGRAM_WEBHOOK_URL",
                "https://tasklerq.example.com/webhooks/telegram",
            ),
            ("TELEGRAM_BOT_USERNAME", "env_console_bot"),
        ],
    )
    .await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/telegram", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("env_console_bot"), "{text}");
    assert!(text.contains("Ожидают обработки"), "{text}");
    assert!(
        !text.contains("name=\"token\""),
        "the environment token must not have an input field: {text}"
    );
    assert!(
        !text.contains("action=\"/admin/telegram\""),
        "the read-only page must have no save form: {text}"
    );

    let (status, _, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("mode", "disabled"), ("token", "123:other-token")],
    )
    .await;
    let text = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert!(text.contains("только для чтения"), "{text}");
    assert_eq!(
        setting_rows(&db.pool, &["telegram.mode", "telegram.bot_token"]).await,
        0,
        "a read-only POST must not write settings"
    );

    stop_application(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reissuing_the_secret_switches_the_accepted_webhook_secret() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    mount_get_me(&server, 555_002, "tasklerq_reissue_bot").await;
    mount_webhook_info(&server, 0, None).await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;

    let (status, _, _) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", "123:SECRET-reissue-token"),
            ("public_base_url", "https://tasklerq.example.com"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let first = submitted_secret(&bot_requests(&server, "setWebhook").await[0]);

    let (status, headers, body) = send_form_from(
        &app,
        "/admin/telegram/secret",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(location(&headers), Some("/admin/telegram?saved=1"));

    let requests = bot_requests(&server, "setWebhook").await;
    assert_eq!(
        requests.len(),
        2,
        "the reissue must register the webhook again"
    );
    let second = submitted_secret(&requests[1]);
    assert_ne!(first, second, "the new secret must differ from the old one");

    let webhook_app = webhook_router(&application);
    assert_eq!(
        post_webhook(
            &webhook_app,
            &first,
            telegram_update(90_010, 777_002, "/start"),
        )
        .await,
        StatusCode::UNAUTHORIZED,
        "the old secret must stop working"
    );
    assert_eq!(
        post_webhook(
            &webhook_app,
            &second,
            telegram_update(90_011, 777_002, "/start"),
        )
        .await,
        StatusCode::OK,
        "the new secret must be accepted"
    );

    stop_application(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabling_the_bot_returns_404_and_keeps_capture_working() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    mount_get_me(&server, 555_003, "tasklerq_disable_bot").await;
    mount_webhook_info(&server, 0, None).await;
    let completion = json!({
        "model": "test/model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": support::llm::v4::response(vec![
                    support::llm::v4::shopping("молоко")
                ]).to_string()
            },
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
    });
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion))
        .mount(&server)
        .await;

    let llm_url = format!("{}/v1", server.uri());
    let config = telegram_config(
        &db.pool,
        Some(&server),
        &[
            ("LLM_CAPTURE_ENABLED", "true"),
            ("LLM_PRIMARY_URL", &llm_url),
            ("LLM_PRIMARY_MODEL", "test/model"),
        ],
    )
    .await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;

    let (status, _, _) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", "123:SECRET-disable-token"),
            ("public_base_url", "https://tasklerq.example.com"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let secret = submitted_secret(&bot_requests(&server, "setWebhook").await[0]);

    let (status, _, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "disabled"),
            ("public_base_url", "https://tasklerq.example.com"),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "{}",
        String::from_utf8_lossy(&body)
    );

    let webhook_app = webhook_router(&application);
    assert_eq!(
        post_webhook(
            &webhook_app,
            &secret,
            telegram_update(90_020, 777_003, "/start"),
        )
        .await,
        StatusCode::NOT_FOUND,
        "a disabled bot must not accept webhook updates"
    );

    let capture = support::issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let (status, body) = support::request(
        &application.router,
        Method::POST,
        "/api/v1/capture",
        Some(&capture),
        Some(json!({"text": "молоко", "source": "api", "mode": "draft"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    stop_application(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_telegram_wizard_step_comes_after_setup_and_can_be_skipped() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("Telegram"), "{text}");
    assert!(text.contains("action=\"/admin/telegram\""), "{text}");
    assert!(
        !text.contains("action=\"/admin/wizard/household\""),
        "{text}"
    );

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/wizard/skip",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("step", "telegram")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/wizard"));
    assert_eq!(
        setting_value(&db.pool, "wizard.telegram_skipped").await,
        Some(Value::Bool(true))
    );

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("action=\"/admin/wizard/household\""),
        "{text}"
    );

    stop_application(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connecting_a_bot_from_the_wizard_makes_the_household_skippable() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    mount_get_me(&server, 555_004, "tasklerq_wizard_bot").await;
    mount_webhook_info(&server, 0, None).await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;

    let (status, headers, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", "123:SECRET-wizard-token"),
            ("public_base_url", "https://tasklerq.example.com"),
            ("next", "wizard"),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(location(&headers), Some("/admin/wizard"));

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin/wizard", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("action=\"/admin/wizard/household\""),
        "{text}"
    );
    assert!(
        text.contains("Пропустить"),
        "a connected and enabled bot must allow skipping the household: {text}"
    );

    let (status, headers, _) = send_form_from(
        &app,
        "/admin/wizard/skip",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("step", "household")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), Some("/admin/wizard"));
    assert_eq!(
        setting_value(&db.pool, "wizard.household_skipped").await,
        Some(Value::Bool(true))
    );

    stop_application(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_home_page_hides_the_queue_lag_after_the_bot_is_disabled() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    mount_get_me(&server, 555_005, "tasklerq_lag_bot").await;
    mount_webhook_info(&server, 0, None).await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;

    // Застрявшее обновление держим заблокированной строкой: `claim_next`
    // пропускает её через `SKIP LOCKED`, а `oldest_ready` продолжает видеть.
    let update_id = 90_100_i64;
    sqlx::query(
        "INSERT INTO telegram_updates (update_id, payload, available_at, received_at)
         VALUES ($1, '{}'::jsonb, now() - interval '10 minutes',
                 now() - interval '10 minutes')",
    )
    .bind(update_id)
    .execute(&db.pool)
    .await
    .expect("stuck update");
    let mut lock = db.pool.begin().await.expect("lock transaction");
    let locked: Option<i64> = sqlx::query_scalar(
        "SELECT update_id FROM telegram_updates WHERE update_id = $1 FOR UPDATE",
    )
    .bind(update_id)
    .fetch_optional(&mut *lock)
    .await
    .expect("lock stuck update");
    assert_eq!(locked, Some(update_id));

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !String::from_utf8_lossy(&body).contains("очередь обновлений Telegram отстаёт"),
        "a disabled bot must not report queue lag"
    );

    let cipher = SecretCipher::from_config(&config);
    let mut conn = db.pool.acquire().await.expect("connection");
    settings::write_value(&mut conn, "telegram.mode", json!("webhook"))
        .await
        .expect("mode");
    settings::write_value(
        &mut conn,
        "telegram.bot_username",
        json!("tasklerq_lag_bot"),
    )
    .await
    .expect("username");
    settings::write_value(
        &mut conn,
        "telegram.public_base_url",
        json!("https://tasklerq.example.com"),
    )
    .await
    .expect("public address");
    settings::write_secret(
        &mut conn,
        "telegram.bot_token",
        &SecretString::new("123:SECRET-lag-token"),
        &cipher,
    )
    .await
    .expect("token");
    settings::write_secret(
        &mut conn,
        "telegram.webhook_secret",
        &SecretString::new("lag-webhook-secret"),
        &cipher,
    )
    .await
    .expect("secret");
    drop(conn);
    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("webhook reload")
    );

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&body).contains("очередь обновлений Telegram отстаёт"),
        "an active generation with a stuck update must report the lag"
    );

    let mut conn = db.pool.acquire().await.expect("connection");
    settings::write_value(&mut conn, "telegram.mode", json!("disabled"))
        .await
        .expect("disable");
    drop(conn);
    assert!(
        !application
            .telegram_supervisor
            .reload()
            .await
            .expect("disable reload")
    );

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !String::from_utf8_lossy(&body).contains("очередь обновлений Telegram отстаёт"),
        "disabling the bot must hide the lag warning again"
    );

    lock.rollback().await.expect("release stuck update");
    stop_application(application, shutdown_tx).await;
}

/// Токен бота и секрет webhook не попадают ни в HTML, ни в журнал.
#[tokio::test]
async fn telegram_secrets_never_reach_the_page_or_the_log() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    mount_get_me(&server, 555_006, "tasklerq_secret_bot").await;
    mount_webhook_info(&server, 0, None).await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let token = "123456:SECRET-console-redaction";
    let logs = support::logs::LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;
    let (status, _, _body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", token),
            ("public_base_url", "https://tasklerq.example.com"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _, page) = send_with_cookie(&app, Method::GET, "/admin/telegram", &cookie).await;
    assert_eq!(status, StatusCode::OK);

    let secret = submitted_secret(&bot_requests(&server, "setWebhook").await[0]);
    let html = String::from_utf8_lossy(&page);
    let logged = logs.contents();
    for value in [token, secret.as_str()] {
        assert!(
            !html.contains(value),
            "the page leaked a telegram secret:\n{html}"
        );
        assert!(
            !logged.contains(value),
            "the log leaked a telegram secret:\n{logged}"
        );
    }

    stop_application(application, shutdown_tx).await;
}

/// Отказ `setWebhook` не скрывается за «Настройки сохранены».
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_setwebhook_is_shown_while_settings_stay_saved() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(bot_path("setWebhook")))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "ok": false,
            "error_code": 400,
            "description": "Bad Request: bad webhook: Failed to resolve host"
        })))
        .mount(&server)
        .await;
    mount_bot_base(&server).await;
    mount_get_me(&server, 555_007, "tasklerq_bad_webhook_bot").await;
    Mock::given(method("POST"))
        .and(path_regex(bot_path("getWebhookInfo")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {"url": "", "pending_update_count": 0}
        })))
        .mount(&server)
        .await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;
    let token = "123:SECRET-rejected-webhook";

    let (status, _, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", token),
            ("public_base_url", "https://tasklerq.example.com"),
        ],
    )
    .await;
    let text = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(
        text.contains("Telegram не подтвердил адрес webhook"),
        "the page must report the rejected registration: {text}"
    );
    assert!(!text.contains(token), "the token must not reach the page");
    assert_eq!(
        setting_rows(
            &db.pool,
            &[
                "telegram.mode",
                "telegram.bot_token",
                "telegram.webhook_secret",
                "telegram.public_base_url"
            ]
        )
        .await,
        4,
        "the settings stay saved when only setWebhook failed"
    );

    stop_application(application, shutdown_tx).await;
}

/// Выключение не требует действующего токена и `getMe`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_token_does_not_prevent_disabling_the_bot() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(bot_path("getMe")))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "ok": false,
            "error_code": 401,
            "description": "Unauthorized"
        })))
        .mount(&server)
        .await;
    mount_bot_base(&server).await;
    mount_webhook_info(&server, 0, None).await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;
    let cipher = SecretCipher::from_config(&config);
    let mut conn = db.pool.acquire().await.expect("connection");
    settings::write_value(&mut conn, "telegram.mode", json!("webhook"))
        .await
        .expect("mode");
    settings::write_value(&mut conn, "telegram.bot_id", json!(555_008))
        .await
        .expect("bot id");
    settings::write_value(
        &mut conn,
        "telegram.bot_username",
        json!("tasklerq_revoked_bot"),
    )
    .await
    .expect("username");
    settings::write_value(
        &mut conn,
        "telegram.public_base_url",
        json!("https://tasklerq.example.com"),
    )
    .await
    .expect("public address");
    settings::write_secret(
        &mut conn,
        "telegram.bot_token",
        &SecretString::new("123:SECRET-revoked-token"),
        &cipher,
    )
    .await
    .expect("token");
    settings::write_secret(
        &mut conn,
        "telegram.webhook_secret",
        &SecretString::new("revoked-webhook-secret"),
        &cipher,
    )
    .await
    .expect("secret");
    drop(conn);
    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("webhook reload")
    );

    let (status, headers, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "disabled"),
            ("public_base_url", "https://tasklerq.example.com"),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(location(&headers), Some("/admin/telegram?saved=1"));
    assert!(
        bot_requests(&server, "getMe").await.is_empty(),
        "disabling must not ask Telegram for the bot identity"
    );
    assert_eq!(
        setting_value(&db.pool, "telegram.mode").await,
        Some(json!("disabled"))
    );
    assert_eq!(
        setting_value(&db.pool, "telegram.bot_id").await,
        Some(json!(555_008)),
        "disabling keeps the stored bot identity"
    );
    assert_eq!(
        setting_value(&db.pool, "telegram.bot_username").await,
        Some(json!("tasklerq_revoked_bot"))
    );

    let webhook_app = webhook_router(&application);
    assert_eq!(
        post_webhook(
            &webhook_app,
            "revoked-webhook-secret",
            telegram_update(90_030, 777_004, "/start"),
        )
        .await,
        StatusCode::NOT_FOUND,
        "the disabled bot must stop accepting webhook updates"
    );

    stop_application(application, shutdown_tx).await;
}

const BOT_A_TOKEN: &str = "123:SECRET-bot-a-token";
const BOT_B_TOKEN: &str = "456:SECRET-bot-b-token";
const BOT_C_TOKEN: &str = "999:SECRET-bot-c-token";
const BOT_A_SECRET: &str = "secret-bot-a";

/// `getMe` только для заданного токена: тесты смены различают ботов по пути.
async fn mount_get_me_for(server: &MockServer, token: &str, id: i64, username: &str) {
    Mock::given(method("POST"))
        .and(path_regex(format!(r"^/bot{token}/getMe$")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {"id": id, "is_bot": true, "first_name": "Tasklerq", "username": username}
        })))
        .mount(server)
        .await;
}

/// Токен из пути запроса к подменному Bot API.
fn request_bot_token(request: &wiremock::Request) -> Option<String> {
    request
        .url
        .path()
        .strip_prefix("/bot")?
        .split('/')
        .next()
        .map(str::to_owned)
}

async fn seed_bot(
    pool: &PgPool,
    cipher: &SecretCipher,
    mode: &str,
    token: &str,
    secret: &str,
    id: i64,
    username: &str,
) {
    let mut conn = pool.acquire().await.expect("connection");
    settings::write_value(&mut conn, "telegram.mode", json!(mode))
        .await
        .expect("mode");
    settings::write_value(&mut conn, "telegram.bot_id", json!(id))
        .await
        .expect("bot id");
    settings::write_value(&mut conn, "telegram.bot_username", json!(username))
        .await
        .expect("username");
    settings::write_value(
        &mut conn,
        "telegram.public_base_url",
        json!("https://tasklerq.example.com"),
    )
    .await
    .expect("public address");
    settings::write_secret(
        &mut conn,
        "telegram.bot_token",
        &SecretString::new(token),
        cipher,
    )
    .await
    .expect("token");
    settings::write_secret(
        &mut conn,
        "telegram.webhook_secret",
        &SecretString::new(secret),
        cipher,
    )
    .await
    .expect("webhook secret");
}

/// Добавить третьего активного участника к household из двух человек.
async fn insert_third_member(pool: &PgPool, household_id: Uuid) {
    sqlx::query(
        "INSERT INTO users (id, household_id, telegram_user_id, telegram_chat_id, display_name, role)
         VALUES ($1, $2, $3, $3, 'Third participant', 'member')",
    )
    .bind(Uuid::now_v7())
    .bind(household_id)
    .bind(7_700_000_001_i64)
    .execute(pool)
    .await
    .expect("third participant");
}

async fn stored_bot_token(pool: &PgPool, config: &Config) -> String {
    let effective =
        settings::read_effective_telegram(pool, config, &SecretCipher::from_config(config))
            .await
            .expect("telegram settings");
    effective
        .bot_token()
        .expect("stored bot token")
        .expose()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changing_the_bot_with_participants_requires_confirmation() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    mount_get_me_for(&server, BOT_B_TOKEN, 555_101, "tasklerq_bot_b").await;
    mount_webhook_info(&server, 0, None).await;
    let household = support::create_household(&db.pool).await;
    insert_third_member(&db.pool, household.household_id).await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "webhook",
        BOT_A_TOKEN,
        BOT_A_SECRET,
        555_100,
        "tasklerq_bot_a",
    )
    .await;
    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("bot A reload")
    );
    let set_webhooks = bot_requests(&server, "setWebhook").await.len();
    let deleted_webhooks = bot_requests(&server, "deleteWebhook").await.len();

    let (status, _, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", BOT_B_TOKEN),
            ("public_base_url", "https://tasklerq.example.com"),
        ],
    )
    .await;

    let text = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(text.contains("Подтвердить смену бота"), "{text}");
    assert!(text.contains("tasklerq_bot_a"), "{text}");
    assert!(text.contains("tasklerq_bot_b"), "{text}");
    assert!(
        text.contains("Участников в инсталляции: 3"),
        "the confirmation must count active participants: {text}"
    );
    assert!(
        text.contains("не сможет писать участникам, пока они не отправят ему /start"),
        "{text}"
    );
    assert!(
        !text.contains(BOT_B_TOKEN),
        "the submitted token must not reach the page: {text}"
    );
    assert_eq!(
        setting_value(&db.pool, "telegram.bot_id").await,
        Some(json!(555_100)),
        "an unconfirmed change must keep the stored bot"
    );
    assert_eq!(
        setting_value(&db.pool, "telegram.bot_username").await,
        Some(json!("tasklerq_bot_a"))
    );
    assert_eq!(stored_bot_token(&db.pool, &config).await, BOT_A_TOKEN);
    assert_eq!(
        bot_requests(&server, "setWebhook").await.len(),
        set_webhooks,
        "an unconfirmed change must not register a webhook"
    );
    assert_eq!(
        bot_requests(&server, "deleteWebhook").await.len(),
        deleted_webhooks,
        "an unconfirmed change must not remove a webhook"
    );
    assert!(
        application.telegram_supervisor.ingress().load().is_some(),
        "an unconfirmed change must leave the running generation"
    );

    let (status, _, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("public_base_url", "https://tasklerq.example.com"),
            ("confirm_bot_change", "1"),
            ("confirm_bot_id", "555101"),
        ],
    )
    .await;
    let text = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert!(
        text.contains("Введите токен бота ещё раз"),
        "a confirmation without the token must be rejected: {text}"
    );
    assert_eq!(
        setting_value(&db.pool, "telegram.bot_id").await,
        Some(json!(555_100))
    );
    assert_eq!(
        bot_requests(&server, "setWebhook").await.len(),
        set_webhooks,
        "a confirmation without the token must not register a webhook"
    );

    stop_application(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirming_the_bot_change_switches_and_removes_the_old_webhook() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    mount_get_me_for(&server, BOT_B_TOKEN, 555_102, "tasklerq_bot_b").await;
    mount_webhook_info(&server, 0, None).await;
    let household = support::create_household(&db.pool).await;
    insert_third_member(&db.pool, household.household_id).await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "webhook",
        BOT_A_TOKEN,
        BOT_A_SECRET,
        555_103,
        "tasklerq_bot_a",
    )
    .await;
    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("bot A reload")
    );
    let old_secret = bot_requests(&server, "setWebhook")
        .await
        .into_iter()
        .find(|request| request_bot_token(request).as_deref() == Some(BOT_A_TOKEN))
        .map(|request| submitted_secret(&request))
        .expect("the old bot webhook");

    let (status, headers, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", BOT_B_TOKEN),
            ("public_base_url", "https://tasklerq.example.com"),
            ("confirm_bot_change", "1"),
            ("confirm_bot_id", "555102"),
        ],
    )
    .await;
    let text = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::SEE_OTHER, "{text}");
    assert_eq!(location(&headers), Some("/admin/telegram?saved=1"));

    assert_eq!(
        setting_value(&db.pool, "telegram.bot_id").await,
        Some(json!(555_102))
    );
    assert_eq!(
        setting_value(&db.pool, "telegram.bot_username").await,
        Some(json!("tasklerq_bot_b"))
    );
    assert_eq!(stored_bot_token(&db.pool, &config).await, BOT_B_TOKEN);

    let new_webhook = bot_requests(&server, "setWebhook")
        .await
        .into_iter()
        .find(|request| request_bot_token(request).as_deref() == Some(BOT_B_TOKEN))
        .expect("the new bot webhook must be registered");
    let new_secret = submitted_secret(&new_webhook);
    assert_eq!(
        submitted_webhook_url(&new_webhook),
        "https://tasklerq.example.com/webhooks/telegram"
    );

    let deleted = bot_requests(&server, "deleteWebhook").await;
    assert_eq!(deleted.len(), 1, "the old webhook must be removed once");
    assert_eq!(
        request_bot_token(&deleted[0]).as_deref(),
        Some(BOT_A_TOKEN),
        "deleteWebhook must use the previous token"
    );

    let webhook_app = webhook_router(&application);
    assert_eq!(
        post_webhook(
            &webhook_app,
            &new_secret,
            telegram_update(90_040, 777_005, "/start"),
        )
        .await,
        StatusCode::OK,
        "the new bot must accept updates"
    );
    assert_eq!(
        new_secret, old_secret,
        "the webhook secret is reused when only the bot changes"
    );

    stop_application(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_confirmation_is_bound_to_the_shown_bot() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    mount_get_me_for(&server, BOT_B_TOKEN, 555_111, "tasklerq_bot_b").await;
    mount_get_me_for(&server, BOT_C_TOKEN, 555_112, "tasklerq_bot_c").await;
    mount_webhook_info(&server, 0, None).await;
    let household = support::create_household(&db.pool).await;
    insert_third_member(&db.pool, household.household_id).await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "webhook",
        BOT_A_TOKEN,
        BOT_A_SECRET,
        555_110,
        "tasklerq_bot_a",
    )
    .await;
    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("bot A reload")
    );
    let set_webhooks = bot_requests(&server, "setWebhook").await.len();

    let (status, _, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", BOT_B_TOKEN),
            ("public_base_url", "https://tasklerq.example.com"),
        ],
    )
    .await;
    let text = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(text.contains("tasklerq_bot_b"), "{text}");

    // На втором вводе оказался другой бот: подтверждение относилось к B.
    let (status, _, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", BOT_C_TOKEN),
            ("public_base_url", "https://tasklerq.example.com"),
            ("confirm_bot_change", "1"),
            ("confirm_bot_id", "555111"),
        ],
    )
    .await;
    let text = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(
        text.contains("Подтвердить смену бота") && text.contains("tasklerq_bot_c"),
        "a different bot must be shown for confirmation again: {text}"
    );
    assert!(
        !text.contains(BOT_C_TOKEN),
        "the token must not reach the repeated confirmation: {text}"
    );
    assert_eq!(
        setting_value(&db.pool, "telegram.bot_id").await,
        Some(json!(555_110)),
        "a confirmation for another bot must not save anything"
    );
    assert_eq!(stored_bot_token(&db.pool, &config).await, BOT_A_TOKEN);
    assert_eq!(
        bot_requests(&server, "setWebhook").await.len(),
        set_webhooks,
        "a mismatched confirmation must not register a webhook"
    );

    stop_application(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_old_webhook_removal_does_not_cancel_the_switch() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    mount_get_me_for(&server, BOT_B_TOKEN, 555_104, "tasklerq_bot_b").await;
    mount_webhook_info(&server, 0, None).await;
    Mock::given(method("POST"))
        .and(path_regex(format!(r"^/bot{BOT_A_TOKEN}/deleteWebhook$")))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "ok": false,
            "error_code": 400,
            "description": "Bad Request: webhook is not set"
        })))
        .with_priority(1)
        .mount(&server)
        .await;
    let household = support::create_household(&db.pool).await;
    insert_third_member(&db.pool, household.household_id).await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "webhook",
        BOT_A_TOKEN,
        BOT_A_SECRET,
        555_105,
        "tasklerq_bot_a",
    )
    .await;
    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("bot A reload")
    );

    let (status, _, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", BOT_B_TOKEN),
            ("public_base_url", "https://tasklerq.example.com"),
            ("confirm_bot_change", "1"),
            ("confirm_bot_id", "555104"),
        ],
    )
    .await;
    let text = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(
        text.contains("Настройки Telegram сохранены"),
        "the page must report the saved settings: {text}"
    );
    assert!(
        text.contains("не удалось снять webhook прежнего бота"),
        "the page must mention the failed cleanup: {text}"
    );
    assert!(
        !text.contains(BOT_B_TOKEN),
        "the token must not reach the page after confirmation: {text}"
    );
    assert_eq!(
        setting_value(&db.pool, "telegram.bot_id").await,
        Some(json!(555_104)),
        "a cleanup failure must not cancel the bot change"
    );
    assert_eq!(stored_bot_token(&db.pool, &config).await, BOT_B_TOKEN);

    stop_application(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bot_change_without_participants_is_saved_immediately() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    mount_get_me_for(&server, BOT_B_TOKEN, 555_106, "tasklerq_bot_b").await;
    mount_webhook_info(&server, 0, None).await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "disabled",
        BOT_A_TOKEN,
        BOT_A_SECRET,
        555_107,
        "tasklerq_bot_a",
    )
    .await;

    let (status, headers, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", BOT_B_TOKEN),
            ("public_base_url", "https://tasklerq.example.com"),
        ],
    )
    .await;
    let text = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::SEE_OTHER, "{text}");
    assert_eq!(location(&headers), Some("/admin/telegram?saved=1"));
    assert_eq!(
        setting_value(&db.pool, "telegram.bot_id").await,
        Some(json!(555_106)),
        "without participants another bot is saved without confirmation"
    );
    assert_eq!(stored_bot_token(&db.pool, &config).await, BOT_B_TOKEN);

    let deleted = bot_requests(&server, "deleteWebhook").await;
    assert_eq!(
        deleted.len(),
        1,
        "without a previous generation the old token must still lose its webhook"
    );
    assert_eq!(
        request_bot_token(&deleted[0]).as_deref(),
        Some(BOT_A_TOKEN),
        "the fallback client must use the token stored in the database"
    );

    stop_application(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_bot_id_is_saved_without_confirmation() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    mount_get_me_for(
        &server,
        "789:SECRET-rotated-token",
        555_108,
        "tasklerq_bot_a",
    )
    .await;
    mount_webhook_info(&server, 0, None).await;
    let household = support::create_household(&db.pool).await;
    insert_third_member(&db.pool, household.household_id).await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "webhook",
        BOT_A_TOKEN,
        BOT_A_SECRET,
        555_108,
        "tasklerq_bot_a",
    )
    .await;
    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("webhook reload")
    );
    let deleted_webhooks = bot_requests(&server, "deleteWebhook").await.len();

    let (status, headers, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", "789:SECRET-rotated-token"),
            ("public_base_url", "https://tasklerq.example.com"),
        ],
    )
    .await;
    let text = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::SEE_OTHER, "{text}");
    assert_eq!(location(&headers), Some("/admin/telegram?saved=1"));
    assert_eq!(
        stored_bot_token(&db.pool, &config).await,
        "789:SECRET-rotated-token",
        "the same bot id must be saved without a confirmation form"
    );
    assert_eq!(
        bot_requests(&server, "deleteWebhook").await.len(),
        deleted_webhooks,
        "a token change for the same bot must not delete the fresh webhook"
    );

    stop_application(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabling_a_webhook_bot_removes_the_old_webhook_even_on_failure() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    mount_webhook_info(&server, 0, None).await;
    Mock::given(method("POST"))
        .and(path_regex(format!(r"^/bot{BOT_A_TOKEN}/deleteWebhook$")))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "ok": false,
            "error_code": 400,
            "description": "Bad Request: webhook is not set"
        })))
        .with_priority(1)
        .mount(&server)
        .await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "webhook",
        BOT_A_TOKEN,
        BOT_A_SECRET,
        555_109,
        "tasklerq_bot_a",
    )
    .await;
    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("webhook reload")
    );

    let (status, _, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "disabled"),
            ("public_base_url", "https://tasklerq.example.com"),
        ],
    )
    .await;
    let text = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        StatusCode::OK,
        "a failed cleanup is shown without failing the disable: {text}"
    );
    assert!(text.contains("Настройки Telegram сохранены"), "{text}");
    assert!(
        text.contains("не удалось снять webhook прежнего бота"),
        "{text}"
    );

    let deleted = bot_requests(&server, "deleteWebhook").await;
    assert_eq!(deleted.len(), 1, "disabling must remove the old webhook");
    assert_eq!(
        request_bot_token(&deleted[0]).as_deref(),
        Some(BOT_A_TOKEN),
        "deleteWebhook must use the previous token"
    );
    assert_eq!(
        setting_value(&db.pool, "telegram.mode").await,
        Some(json!("disabled"))
    );
    assert!(
        application.telegram_supervisor.ingress().load().is_none(),
        "the disabled bot must have no generation"
    );

    let webhook_app = webhook_router(&application);
    assert_eq!(
        post_webhook(
            &webhook_app,
            BOT_A_SECRET,
            telegram_update(90_050, 777_006, "/start"),
        )
        .await,
        StatusCode::NOT_FOUND,
        "the disabled bot must not accept webhook updates"
    );

    stop_application(application, shutdown_tx).await;
}

/// Дождаться заданного числа вызовов метода поддельного Bot API с токеном.
async fn wait_for_token_requests(
    server: &MockServer,
    method_name: &str,
    token: &str,
    count: usize,
) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let calls = bot_requests(server, method_name)
                .await
                .iter()
                .filter(|request| request_bot_token(request).as_deref() == Some(token))
                .count();
            if calls >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("{method_name} was not called {count} time(s) with the expected token")
    });
}

/// Дождаться статуса строки outbox: запись в базе отстаёт от запроса к Bot API.
async fn wait_for_outbox_status(pool: &PgPool, seq: i64, expected: &str) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let status: String =
                sqlx::query_scalar("SELECT status::text FROM outbox WHERE seq = $1")
                    .bind(seq)
                    .fetch_one(pool)
                    .await
                    .expect("outbox status");
            if status == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the outbox row did not reach `{expected}`"));
}

/// `getUpdates` заданного бота отвечает пусто, но достаточно медленно, чтобы
/// переключение поколения успело отменить запрос.
async fn mount_empty_updates_for(server: &MockServer, token: &str) {
    Mock::given(method("POST"))
        .and(path_regex(format!(r"^/bot{token}/getUpdates$")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(200))
                .set_body_json(json!({"ok": true, "result": []})),
        )
        .mount(server)
        .await;
}

/// Ротация токена того же бота: новое поколение, webhook и все вызовы Bot API
/// переходят на новый токен, а webhook прежнего поколения не снимается.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotating_the_token_of_the_same_bot_switches_every_bot_api_call() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    mount_get_me_for(&server, BOT_B_TOKEN, 42, "tasklerq_rotation_bot").await;
    mount_webhook_info(&server, 0, None).await;
    Mock::given(method("POST"))
        .and(path_regex(format!(r"^/bot{BOT_B_TOKEN}/getWebhookInfo$")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {
                "url": "https://tasklerq.example.com/webhooks/telegram",
                "pending_update_count": 13
            }
        })))
        .with_priority(1)
        .mount(&server)
        .await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "webhook",
        BOT_A_TOKEN,
        BOT_A_SECRET,
        42,
        "tasklerq_rotation_bot",
    )
    .await;
    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("bot A reload")
    );
    wait_for_token_requests(&server, "setWebhook", BOT_A_TOKEN, 1).await;
    let old_secret = bot_requests(&server, "setWebhook")
        .await
        .into_iter()
        .find(|request| request_bot_token(request).as_deref() == Some(BOT_A_TOKEN))
        .map(|request| submitted_secret(&request))
        .expect("the old webhook must be registered");

    let (status, headers, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", BOT_B_TOKEN),
            ("public_base_url", "https://tasklerq.example.com"),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "the same bot must be rotated without a confirmation form: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(location(&headers), Some("/admin/telegram?saved=1"));
    // Всё, что записано после ответа на форму, принадлежит новому поколению.
    let boundary = server.received_requests().await.expect("requests").len();

    let (value, secret): (Option<Value>, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT value, secret FROM settings
         WHERE scope = 'installation' AND household_id IS NULL AND key = 'telegram.bot_token'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("the token row");
    assert!(value.is_none(), "the token must be stored as a secret");
    let secret = secret.expect("the token ciphertext");
    assert!(
        !String::from_utf8_lossy(&secret).contains(BOT_B_TOKEN),
        "the stored token must be encrypted"
    );
    assert_eq!(stored_bot_token(&db.pool, &config).await, BOT_B_TOKEN);

    let webhooks = bot_requests(&server, "setWebhook").await;
    let new_webhook = webhooks
        .iter()
        .find(|request| request_bot_token(request).as_deref() == Some(BOT_B_TOKEN))
        .expect("the new token must register the webhook");
    assert_eq!(
        submitted_secret(new_webhook),
        old_secret,
        "a token rotation must reuse the webhook secret"
    );
    assert_eq!(
        submitted_webhook_url(new_webhook),
        "https://tasklerq.example.com/webhooks/telegram"
    );
    assert!(
        bot_requests(&server, "deleteWebhook").await.is_empty(),
        "a token rotation must not remove the webhook of the same bot"
    );

    let payloads = audit_payloads(&db.pool, "admin_setting_changed").await;
    let rotation = payloads
        .iter()
        .find(|payload| payload["key"] == json!("telegram.bot_token"))
        .expect("the rotation must be audited");
    assert_eq!(rotation["value"], json!("set"));
    let audit = serde_json::to_string(&payloads).expect("audit payloads");
    assert!(
        !audit.contains(BOT_A_TOKEN) && !audit.contains(BOT_B_TOKEN),
        "the audit must not store either token: {audit}"
    );

    let marker = "rotation-message-marker";
    let mut conn = db.pool.acquire().await.expect("connection");
    outbox::enqueue(
        &mut conn,
        777_010,
        &Outgoing::Message {
            html: marker.to_owned(),
            rich: false,
            delete_after: None,
            disable_preview: false,
        },
        None,
        Utc::now(),
    )
    .await
    .expect("outbox row");
    drop(conn);
    wait_for_token_requests(&server, "sendMessage", BOT_B_TOKEN, 1).await;
    let sends: Vec<_> = bot_requests(&server, "sendMessage")
        .await
        .into_iter()
        .filter(|request| String::from_utf8_lossy(&request.body).contains(marker))
        .collect();
    assert_eq!(sends.len(), 1, "the message must be sent exactly once");
    assert_eq!(
        request_bot_token(&sends[0]).as_deref(),
        Some(BOT_B_TOKEN),
        "the outbox must deliver with the new token"
    );

    let (status, _, page) = send_with_cookie(&app, Method::GET, "/admin/telegram", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8_lossy(&page);
    assert!(
        html.contains("Ожидают обработки</dt><dd>13"),
        "the page must show getWebhookInfo obtained with the new token: {html}"
    );
    assert!(
        !html.contains(BOT_A_TOKEN) && !html.contains(BOT_B_TOKEN),
        "the page must not render either token: {html}"
    );

    let requests = server.received_requests().await.expect("requests");
    let after_switch = &requests[boundary..];
    assert!(
        !after_switch.is_empty(),
        "the new generation must call the Bot API"
    );
    assert!(
        after_switch
            .iter()
            .all(|request| request_bot_token(request).as_deref() != Some(BOT_A_TOKEN)),
        "the old token must not be used after the switch"
    );
    assert!(
        after_switch
            .iter()
            .filter(|request| request.url.path().ends_with("/getWebhookInfo"))
            .all(|request| request_bot_token(request).as_deref() == Some(BOT_B_TOKEN)),
        "getWebhookInfo after the switch must use the new token"
    );

    stop_application(application, shutdown_tx).await;
}

/// Ротация токена в polling: обновления запрашиваются только новым токеном.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotating_the_token_in_polling_mode_polls_with_the_new_token() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    mount_get_me_for(&server, BOT_B_TOKEN, 42, "tasklerq_polling_rotation_bot").await;
    mount_empty_updates_for(&server, BOT_A_TOKEN).await;
    mount_empty_updates_for(&server, BOT_B_TOKEN).await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "polling",
        BOT_A_TOKEN,
        BOT_A_SECRET,
        42,
        "tasklerq_polling_rotation_bot",
    )
    .await;
    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("bot A reload")
    );
    wait_for_token_requests(&server, "getUpdates", BOT_A_TOKEN, 1).await;

    let (status, headers, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "polling"),
            ("token", BOT_B_TOKEN),
            ("public_base_url", "https://tasklerq.example.com"),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(location(&headers), Some("/admin/telegram?saved=1"));

    wait_for_token_requests(&server, "getUpdates", BOT_B_TOKEN, 1).await;
    // Ещё несколько циклов: если старый поллер возобновится, это станет
    // заметно до проверки.
    wait_for_token_requests(&server, "getUpdates", BOT_B_TOKEN, 3).await;
    let requests = server.received_requests().await.expect("requests");
    // Граница — первый опрос новым токеном. Всё, что пришло раньше, может
    // принадлежать старому поллеру, который ещё не успел отмениться.
    let first_new = requests
        .iter()
        .position(|request| {
            request.url.path().ends_with("/getUpdates")
                && request_bot_token(request).as_deref() == Some(BOT_B_TOKEN)
        })
        .expect("polling must continue with the new token");
    let after_switch: Vec<_> = requests[first_new..]
        .iter()
        .filter(|request| request.url.path().ends_with("/getUpdates"))
        .collect();
    assert!(
        !after_switch.is_empty(),
        "polling must continue with the new token"
    );
    assert!(
        after_switch
            .iter()
            .all(|request| request_bot_token(request).as_deref() == Some(BOT_B_TOKEN)),
        "no getUpdates call may use the old token after the switch"
    );

    stop_application(application, shutdown_tx).await;
}

/// Сообщение, поставленное в outbox до ротации, доставляется после неё один
/// раз и новым токеном.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_outbox_message_queued_before_rotation_is_delivered_by_the_new_token() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    mount_get_me_for(&server, BOT_B_TOKEN, 42, "tasklerq_outbox_rotation_bot").await;
    mount_webhook_info(&server, 0, None).await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "webhook",
        BOT_A_TOKEN,
        BOT_A_SECRET,
        42,
        "tasklerq_outbox_rotation_bot",
    )
    .await;
    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("bot A reload")
    );
    wait_for_token_requests(&server, "setWebhook", BOT_A_TOKEN, 1).await;

    // Строка поставлена до ротации, но не готова к доставке: иначе её могло бы
    // забрать прежнее поколение. После ротации тест делает её готовой.
    let marker = "pre-rotation-outbox-marker";
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");
    let seq = outbox::enqueue_at(
        &mut conn,
        777_011,
        &Outgoing::Message {
            html: marker.to_owned(),
            rich: false,
            delete_after: None,
            disable_preview: false,
        },
        None,
        now,
        now + ChronoDuration::hours(1),
    )
    .await
    .expect("outbox row")
    .expect("a new outbox row");
    drop(conn);

    let (status, headers, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", BOT_B_TOKEN),
            ("public_base_url", "https://tasklerq.example.com"),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(location(&headers), Some("/admin/telegram?saved=1"));

    sqlx::query("UPDATE outbox SET next_attempt_at = now() WHERE seq = $1")
        .bind(seq)
        .execute(&db.pool)
        .await
        .expect("make the queued message due");
    wait_for_token_requests(&server, "sendMessage", BOT_B_TOKEN, 1).await;
    wait_for_outbox_status(&db.pool, seq, "sent").await;
    let (status, message_id): (String, Option<i64>) =
        sqlx::query_as("SELECT status::text, message_id FROM outbox WHERE seq = $1")
            .bind(seq)
            .fetch_one(&db.pool)
            .await
            .expect("outbox row");
    assert_eq!(status, "sent");
    assert_eq!(message_id, Some(1));

    // Проверяем вызовы после того, как строка зафиксирована как `sent`:
    // повторная доставка прежним поколением проявилась бы уже после этого.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let sends: Vec<_> = bot_requests(&server, "sendMessage")
        .await
        .into_iter()
        .filter(|request| String::from_utf8_lossy(&request.body).contains(marker))
        .collect();
    assert_eq!(
        sends.len(),
        1,
        "the queued message must be sent exactly once"
    );
    assert!(
        sends
            .iter()
            .all(|request| request_bot_token(request).as_deref() == Some(BOT_B_TOKEN)),
        "the queued message must be delivered with the new token"
    );
    assert!(
        bot_requests(&server, "sendMessage")
            .await
            .iter()
            .all(|request| request_bot_token(request).as_deref() != Some(BOT_A_TOKEN)),
        "the old token must not deliver anything after the rotation"
    );

    stop_application(application, shutdown_tx).await;
}

/// Ротация не выводит ни прежний, ни новый токен в HTML и журнал.
#[tokio::test]
async fn token_rotation_keeps_both_tokens_out_of_the_page_and_the_log() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    mount_get_me_for(&server, BOT_B_TOKEN, 42, "tasklerq_secret_rotation_bot").await;
    mount_webhook_info(&server, 0, None).await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let logs = support::logs::LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "webhook",
        BOT_A_TOKEN,
        BOT_A_SECRET,
        42,
        "tasklerq_secret_rotation_bot",
    )
    .await;
    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("bot A reload")
    );
    wait_for_token_requests(&server, "setWebhook", BOT_A_TOKEN, 1).await;

    let (status, _, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", BOT_B_TOKEN),
            ("public_base_url", "https://tasklerq.example.com"),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let (status, _, page) = send_with_cookie(&app, Method::GET, "/admin/telegram", &cookie).await;
    assert_eq!(status, StatusCode::OK);

    let html = String::from_utf8_lossy(&page);
    let logged = logs.contents();
    assert!(
        !logged.trim().is_empty(),
        "the log buffer must not be empty, otherwise the redaction check proves nothing"
    );
    assert!(
        logged.contains("telegram webhook registered"),
        "the new webhook registration must reach the captured log:\n{logged}"
    );
    for token in [BOT_A_TOKEN, BOT_B_TOKEN] {
        assert!(
            !html.contains(token),
            "the page leaked a telegram token:\n{html}"
        );
        assert!(
            !logged.contains(token),
            "the log leaked a telegram token:\n{logged}"
        );
    }

    stop_application(application, shutdown_tx).await;
}

/// Консоль с настройками Telegram и без супервизора: страница и выпуск
/// ссылки-claim не трогают рантайм.
fn owner_console(pool: &PgPool, config: &Config, tenancy: TenancyMode) -> Router {
    let cipher = SecretCipher::from_config(config);
    let handle = LlmHandle::new(LlmSnapshot::from_effective(settings::effective_llm(
        &config.llm,
        &[],
        &cipher,
    )));
    let llm = LlmSettings::new(pool.clone(), config.llm.clone(), cipher, handle);
    build_admin_router(
        AdminState::new(pool.clone(), Vec::new(), Audit::new(pool.clone()), llm)
            .with_wizard(WizardSettings::new(tenancy, true))
            .with_telegram_config(config.clone()),
    )
}

/// Консоль и cookie администратора.
async fn owner_console_with_login(
    pool: &PgPool,
    config: &Config,
    tenancy: TenancyMode,
) -> (Router, String) {
    let app = owner_console(pool, config, tenancy);
    insert_account(pool, "admin").await;
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;
    (app, cookie)
}

/// Активный household без владельца.
async fn household_without_owner(pool: &PgPool, name: &str) -> Uuid {
    let mut tx = pool.begin().await.expect("household transaction");
    let household = households::create_empty(
        &mut tx,
        TenancyMode::Multi,
        name,
        "Europe/Moscow",
        &tasklerq::admin::admin_actor(),
        Utc::now(),
    )
    .await
    .expect("household is created");
    tx.commit().await.expect("commit the household");
    household.id
}

/// Назначить household владельца напрямую, как после claim.
async fn add_owner(pool: &PgPool, household_id: Uuid, telegram_id: i64) {
    sqlx::query(
        "INSERT INTO users (
             id, household_id, telegram_user_id, telegram_chat_id, display_name, role
         )
         VALUES ($1, $2, $3, $3, 'Owner', 'owner')",
    )
    .bind(Uuid::now_v7())
    .bind(household_id)
    .bind(telegram_id)
    .execute(pool)
    .await
    .expect("owner row");
}

/// Код из ссылки `https://t.me/<бот>?start=own_<код>`.
fn owner_code_from(html: &str) -> String {
    let marker = "?start=own_";
    let start = html
        .find(marker)
        .unwrap_or_else(|| panic!("no owner claim link in:\n{html}"))
        + marker.len();
    html[start..].chars().take(22).collect()
}

/// Внешние адреса из атрибутов `href` и `src`.
fn external_resource_urls(html: &str) -> Vec<&str> {
    let mut urls = Vec::new();
    for attribute in ["href=\"", "src=\""] {
        let mut rest = html;
        while let Some(position) = rest.find(attribute) {
            let start = position + attribute.len();
            let Some(end) = rest[start..].find('"') else {
                break;
            };
            let url = &rest[start..start + end];
            if url.starts_with("http") {
                urls.push(url);
            }
            rest = &rest[start + end..];
        }
    }
    urls
}

/// Бот, принимающий `/start own_…`, на той же базе.
fn owner_claim_harness(pool: &PgPool, telegram_id: i64, tenancy: TenancyMode) -> Harness {
    let bot = Bot::new("tasklerq_owner_bot")
        .with_tenancy_config(TenancyConfig {
            mode: tenancy,
            bootstrap_owner: None,
        })
        .with_audit(Audit::new(pool.clone()));
    Harness::with_handler(pool, telegram_id, Arc::new(bot), FakeTelegram::new())
}

/// Число действующих кодов `owner_claim` для цели.
async fn active_claim_count(pool: &PgPool, household_id: Option<Uuid>) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM admin_one_time_codes
         WHERE purpose = 'owner_claim' AND household_id IS NOT DISTINCT FROM $1
           AND used_at IS NULL",
    )
    .bind(household_id)
    .fetch_one(pool)
    .await
    .expect("active owner claim codes")
}

/// Действует ли открытое значение `owner_claim`.
async fn claim_is_active(pool: &PgPool, code: &str) -> bool {
    let mut conn = pool.acquire().await.expect("connection");
    codes::find_active(&mut conn, Purpose::OwnerClaim, code, Utc::now())
        .await
        .expect("find the owner claim")
        .is_some()
}

/// Код из ссылки `https://t.me/<бот>?start=adm_<код>`.
fn telegram_link_code_from(html: &str) -> String {
    let marker = "?start=adm_";
    let start = html
        .find(marker)
        .unwrap_or_else(|| panic!("no telegram link in:\n{html}"))
        + marker.len();
    html[start..].chars().take(22).collect()
}

/// Действует ли открытое значение `telegram_link`.
async fn telegram_link_is_active(pool: &PgPool, code: &str) -> bool {
    let mut conn = pool.acquire().await.expect("connection");
    codes::find_active(&mut conn, Purpose::TelegramLink, code, Utc::now())
        .await
        .expect("find the telegram link")
        .is_some()
}

/// Сообщения (`kind = 'message'`) чата в порядке постановки.
async fn outbox_messages(pool: &PgPool, chat: i64) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT payload->>'html' FROM outbox
         WHERE chat_id = $1 AND kind = 'message'
         ORDER BY seq",
    )
    .bind(chat)
    .fetch_all(pool)
    .await
    .expect("outbox messages")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_owner_claim_link_is_issued_with_an_inline_qr_and_works_in_telegram() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[]).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "polling",
        "123:SECRET-owner-claim",
        "owner-claim-secret",
        901_001,
        "tasklerq_owner_bot",
    )
    .await;
    let household = household_without_owner(&db.pool, "Дом").await;
    let (app, cookie) = owner_console_with_login(&db.pool, &config, TenancyMode::Multi).await;

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let home = String::from_utf8_lossy(&body);
    assert!(
        home.contains("Дом"),
        "the household target is listed:\n{home}"
    );
    assert!(
        home.contains("Выпустить ссылку"),
        "the household without an owner offers the link:\n{home}"
    );

    let target = household.to_string();
    let (status, _, body) = send_form_from(
        &app,
        "/admin/owner",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("target", target.as_str())],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let html = String::from_utf8_lossy(&body);
    assert!(
        html.contains("https://t.me/tasklerq_owner_bot?start=own_"),
        "the response shows the claim link:\n{html}"
    );
    assert!(html.contains("<svg"), "the QR is inline SVG:\n{html}");
    assert!(
        !html.contains("&lt;svg"),
        "askama must not escape the generated SVG:\n{html}"
    );
    assert!(
        !html.contains("src=\"http"),
        "no external image or script may be loaded:\n{html}"
    );
    for url in external_resource_urls(&html) {
        assert!(
            url.starts_with("https://t.me/"),
            "only the claim link may point outside: {url}\n{html}"
        );
    }
    assert!(
        audit_payloads(&db.pool, "admin_recovery_issued")
            .await
            .is_empty(),
        "issuing an owner claim link is not a recovery link"
    );

    let code = owner_code_from(&html);
    let mut bot = owner_claim_harness(&db.pool, support::next_telegram_id(), TenancyMode::Multi);
    bot.text(&format!("/start own_{code}")).await;

    let owner: Option<Uuid> = sqlx::query_scalar(
        "SELECT household_id FROM users
         WHERE telegram_user_id = $1 AND role = 'owner' AND deleted_at IS NULL",
    )
    .bind(bot.chat)
    .fetch_optional(&db.pool)
    .await
    .expect("claiming owner");
    assert_eq!(owner, Some(household), "the bot accepts the issued link");

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let home = String::from_utf8_lossy(&body);
    assert!(
        !home.contains("Выпустить ссылку"),
        "a household with an owner must not be offered:\n{home}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_single_installation_gets_a_claim_code_without_a_household() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[]).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "polling",
        "123:SECRET-empty-claim",
        "empty-claim-secret",
        901_002,
        "tasklerq_empty_bot",
    )
    .await;
    let (app, cookie) = owner_console_with_login(&db.pool, &config, TenancyMode::Single).await;

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let home = String::from_utf8_lossy(&body);
    assert!(
        home.contains("Пустая инсталляция"),
        "an empty single installation is a target:\n{home}"
    );
    assert!(home.contains("Выпустить ссылку"), "{home}");

    let (status, _, body) = send_form_from(
        &app,
        "/admin/owner",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("target", "empty")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("https://t.me/tasklerq_empty_bot?start=own_"));

    let household_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT household_id FROM admin_one_time_codes
         WHERE purpose = 'owner_claim' AND used_at IS NULL",
    )
    .fetch_one(&db.pool)
    .await
    .expect("the issued code");
    assert_eq!(
        household_id, None,
        "an empty installation code has no household"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_household_with_an_owner_gets_no_claim_link_and_rejects_the_post() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[]).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "polling",
        "123:SECRET-owned-claim",
        "owned-claim-secret",
        901_003,
        "tasklerq_owned_bot",
    )
    .await;
    let household = household_without_owner(&db.pool, "Дом с владельцем").await;
    add_owner(&db.pool, household, support::next_telegram_id()).await;
    let (app, cookie) = owner_console_with_login(&db.pool, &config, TenancyMode::Multi).await;

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let home = String::from_utf8_lossy(&body);
    assert!(
        !home.contains("Выпустить ссылку"),
        "a household with an owner must not be listed:\n{home}"
    );

    let target = household.to_string();
    let (status, _, body) = send_form_from(
        &app,
        "/admin/owner",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("target", target.as_str())],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let html = String::from_utf8_lossy(&body);
    assert!(
        html.contains("уже есть владелец"),
        "the rejected post explains why:\n{html}"
    );
    assert!(!html.contains("?start=own_"), "{html}");
    assert_eq!(active_claim_count(&db.pool, Some(household)).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reissuing_cancels_only_the_previous_link_of_the_same_household() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[]).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "polling",
        "123:SECRET-reissue-claim",
        "reissue-claim-secret",
        901_004,
        "tasklerq_reissue_bot",
    )
    .await;
    let dacha = household_without_owner(&db.pool, "Дача").await;
    let flat = household_without_owner(&db.pool, "Квартира").await;
    let (app, cookie) = owner_console_with_login(&db.pool, &config, TenancyMode::Multi).await;

    let issue = |target: Uuid| {
        let app = app.clone();
        let cookie = cookie.clone();
        async move {
            let target = target.to_string();
            let (status, _, body) = send_form_from(
                &app,
                "/admin/owner",
                test_peer(),
                headers_with(&[("cookie", &cookie)]),
                &[("target", target.as_str())],
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
            owner_code_from(&String::from_utf8_lossy(&body))
        }
    };

    let first_dacha = issue(dacha).await;
    let flat_code = issue(flat).await;
    assert!(
        claim_is_active(&db.pool, &first_dacha).await,
        "another household must not cancel the dacha link"
    );

    let second_dacha = issue(dacha).await;
    assert_ne!(first_dacha, second_dacha);
    assert!(
        !claim_is_active(&db.pool, &first_dacha).await,
        "reissuing for the same household cancels the previous code"
    );
    assert!(claim_is_active(&db.pool, &second_dacha).await);
    assert!(
        claim_is_active(&db.pool, &flat_code).await,
        "the apartment link keeps working"
    );
    assert_eq!(active_claim_count(&db.pool, Some(dacha)).await, 1);
    assert_eq!(active_claim_count(&db.pool, Some(flat)).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_telegram_shows_the_hint_and_issues_no_code() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[]).await;
    let household = household_without_owner(&db.pool, "Дом без бота").await;
    let (app, cookie) = owner_console_with_login(&db.pool, &config, TenancyMode::Multi).await;

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let home = String::from_utf8_lossy(&body);
    assert!(
        home.contains("Сначала подключите бота"),
        "the disabled bot is explained:\n{home}"
    );
    assert!(
        !home.contains("Выпустить ссылку"),
        "no button without a bot:\n{home}"
    );

    let target = household.to_string();
    let (status, _, body) = send_form_from(
        &app,
        "/admin/owner",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("target", target.as_str())],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("Сначала подключите бота"), "{html}");
    assert!(!html.contains("?start=own_"), "{html}");
    assert_eq!(active_claim_count(&db.pool, Some(household)).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_owner_claim_post_requires_the_same_origin() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[]).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "polling",
        "123:SECRET-origin-claim",
        "origin-claim-secret",
        901_005,
        "tasklerq_origin_bot",
    )
    .await;
    let household = household_without_owner(&db.pool, "Дом origin").await;
    let (app, cookie) = owner_console_with_login(&db.pool, &config, TenancyMode::Multi).await;

    let target = household.to_string();
    let attempts = [
        headers_with(&[("cookie", cookie.as_str())]),
        headers_with(&[
            ("cookie", cookie.as_str()),
            ("host", "nas:18081"),
            ("origin", "http://evil.example"),
        ]),
    ];
    for headers in attempts {
        let (status, _, _) = send_form_raw(
            &app,
            "/admin/owner",
            test_peer(),
            headers,
            &[("target", target.as_str())],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    assert_eq!(active_claim_count(&db.pool, Some(household)).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_empty_target_is_rejected_outside_single_mode() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[]).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "polling",
        "123:SECRET-multi-empty",
        "multi-empty-secret",
        901_006,
        "tasklerq_multi_empty_bot",
    )
    .await;
    let (app, cookie) = owner_console_with_login(&db.pool, &config, TenancyMode::Multi).await;

    let (status, _, body) = send_form_from(
        &app,
        "/admin/owner",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("target", "empty")],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("В режиме multi выберите household"), "{html}");
    assert!(!html.contains("?start=own_"), "{html}");
    assert_eq!(active_claim_count(&db.pool, None).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_empty_target_is_rejected_when_a_household_exists() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[]).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "polling",
        "123:SECRET-single-not-empty",
        "single-not-empty-secret",
        901_007,
        "tasklerq_single_not_empty_bot",
    )
    .await;
    household_without_owner(&db.pool, "Дом в single").await;
    let (app, cookie) = owner_console_with_login(&db.pool, &config, TenancyMode::Single).await;

    let (status, _, body) = send_form_from(
        &app,
        "/admin/owner",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("target", "empty")],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("Инсталляция уже не пуста"), "{html}");
    assert!(!html.contains("?start=own_"), "{html}");
    assert_eq!(active_claim_count(&db.pool, None).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_or_missing_household_target_is_rejected() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[]).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "polling",
        "123:SECRET-bad-target",
        "bad-target-secret",
        901_008,
        "tasklerq_bad_target_bot",
    )
    .await;
    let (app, cookie) = owner_console_with_login(&db.pool, &config, TenancyMode::Multi).await;

    let unknown = Uuid::now_v7().to_string();
    for target in ["not-a-uuid", unknown.as_str()] {
        let (status, _, body) = send_form_from(
            &app,
            "/admin/owner",
            test_peer(),
            headers_with(&[("cookie", &cookie)]),
            &[("target", target)],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let html = String::from_utf8_lossy(&body);
        assert!(
            html.contains("Неизвестная цель") || html.contains("Household не найден"),
            "{html}"
        );
        assert!(!html.contains("?start=own_"), "{html}");
    }
    assert_eq!(active_claim_count(&db.pool, None).await, 0);
}

/// Выпустить ссылку привязки Telegram и вернуть открытый код.
async fn issue_telegram_link(app: &Router, cookie: &str) -> String {
    let (status, _, body) = send_form_from(
        app,
        "/admin/telegram-link",
        test_peer(),
        headers_with(&[("cookie", cookie)]),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let html = String::from_utf8_lossy(&body);
    assert!(
        html.contains("?start=adm_"),
        "the response shows the link:\n{html}"
    );
    telegram_link_code_from(&html)
}

/// Подтвердить или отклонить кандидата в консоли.
async fn send_candidate(app: &Router, cookie: &str, action: &str, candidate: i64) {
    let path = format!("/admin/telegram-link/{action}");
    let candidate = candidate.to_string();
    let (status, _, body) = send_form_from(
        app,
        &path,
        test_peer(),
        headers_with(&[("cookie", cookie)]),
        &[("candidate", candidate.as_str())],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
}

/// Кандидат и подтверждённая привязка учётной записи.
async fn telegram_binding(pool: &PgPool) -> (Option<i64>, Option<String>, Option<i64>) {
    sqlx::query_as(
        "SELECT telegram_user_id, telegram_name, pending_telegram_user_id
         FROM admin_accounts",
    )
    .fetch_one(pool)
    .await
    .expect("telegram binding")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_telegram_link_is_issued_with_a_fifteen_minute_code_and_reissue_resets_the_candidate() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[]).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "polling",
        "123:SECRET-telegram-link",
        "telegram-link-secret",
        902_001,
        "tasklerq_link_bot",
    )
    .await;
    let (app, cookie) = owner_console_with_login(&db.pool, &config, TenancyMode::Single).await;

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let home = String::from_utf8_lossy(&body);
    assert!(home.contains("Telegram администратора"), "{home}");
    assert!(home.contains("Создать ссылку привязки"), "{home}");

    let first = issue_telegram_link(&app, &cookie).await;
    let (purpose, lifetime): (String, Option<f64>) = sqlx::query_as(
        "SELECT purpose, EXTRACT(EPOCH FROM (expires_at - created_at))::float8
         FROM admin_one_time_codes WHERE code_hash = $1",
    )
    .bind(hash_token(&first))
    .fetch_one(&db.pool)
    .await
    .expect("the issued telegram link");
    assert_eq!(purpose, "telegram_link");
    let lifetime = lifetime.expect("a telegram link always has an expiry");
    assert!(
        (lifetime - 900.0).abs() < 1.0,
        "the telegram link lives fifteen minutes: {lifetime}"
    );

    let second = issue_telegram_link(&app, &cookie).await;
    assert_ne!(first, second);
    assert!(
        !telegram_link_is_active(&db.pool, &first).await,
        "reissuing cancels the previous link"
    );
    assert!(telegram_link_is_active(&db.pool, &second).await);

    let mut bot = owner_claim_harness(&db.pool, support::next_telegram_id(), TenancyMode::Single);
    bot.text(&format!("/start adm_{second}")).await;
    let (_, _, pending) = telegram_binding(&db.pool).await;
    assert_eq!(pending, Some(bot.chat), "the bot records the candidate");

    let third = issue_telegram_link(&app, &cookie).await;
    assert!(telegram_link_is_active(&db.pool, &third).await);
    let (_, _, pending) = telegram_binding(&db.pool).await;
    assert_eq!(
        pending, None,
        "a new link starts a new attempt and drops the candidate"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_telegram_link_post_requires_the_same_origin() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[]).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "polling",
        "123:SECRET-link-origin",
        "link-origin-secret",
        902_002,
        "tasklerq_link_origin_bot",
    )
    .await;
    let (app, cookie) = owner_console_with_login(&db.pool, &config, TenancyMode::Single).await;

    let attempts = [
        headers_with(&[("cookie", cookie.as_str())]),
        headers_with(&[
            ("cookie", cookie.as_str()),
            ("host", "nas:18081"),
            ("origin", "http://evil.example"),
        ]),
    ];
    for headers in attempts {
        let (status, _, _) =
            send_form_raw(&app, "/admin/telegram-link", test_peer(), headers, &[]).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    let links: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM admin_one_time_codes WHERE purpose = 'telegram_link'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("telegram links");
    assert_eq!(links, 0, "a rejected POST issues no code");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_telegram_link_without_a_bot_shows_the_hint_and_issues_no_code() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[]).await;
    let (app, cookie) = owner_console_with_login(&db.pool, &config, TenancyMode::Single).await;

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let home = String::from_utf8_lossy(&body);
    assert!(home.contains("Сначала подключите бота"), "{home}");
    assert!(!home.contains("Создать ссылку привязки"), "{home}");

    let (status, _, body) = send_form_from(
        &app,
        "/admin/telegram-link",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("Сначала подключите бота"), "{html}");
    assert!(!html.contains("?start=adm_"), "{html}");
    let links: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM admin_one_time_codes WHERE purpose = 'telegram_link'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("telegram links");
    assert_eq!(links, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_telegram_link_binds_only_after_confirmation_and_notifies_the_account() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[]).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "polling",
        "123:SECRET-link-bind",
        "link-bind-secret",
        902_003,
        "tasklerq_bind_bot",
    )
    .await;
    let (app, cookie) = owner_console_with_login(&db.pool, &config, TenancyMode::Single).await;

    let code = issue_telegram_link(&app, &cookie).await;
    let mut bot = owner_claim_harness(&db.pool, support::next_telegram_id(), TenancyMode::Single);
    let bot_id = bot.chat;
    bot.text(&format!("/start adm_{code}")).await;
    bot.assert_screen_contains("Запрос на привязку отправлен");
    bot.assert_screen_contains("Подтвердите его в консоли администратора");

    let (bound, name, pending) = telegram_binding(&db.pool).await;
    assert_eq!(
        bound, None,
        "the candidate is not bound before confirmation"
    );
    assert_eq!(pending, Some(bot_id));
    assert_eq!(name, None);

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let home = String::from_utf8_lossy(&body);
    assert!(home.contains("Alice"), "the candidate is shown:\n{home}");
    assert!(home.contains("Подтвердить"), "{home}");

    send_candidate(&app, &cookie, "confirm", bot_id).await;

    let (bound, name, pending) = telegram_binding(&db.pool).await;
    assert_eq!(bound, Some(bot_id));
    assert_eq!(name.as_deref(), Some("Alice"));
    assert_eq!(pending, None);
    let payloads = audit_payloads(&db.pool, "admin_telegram_linked").await;
    assert_eq!(payloads.len(), 1);
    // Payload фиксирует только аккаунт: ни кода привязки, ни токенов.
    assert_eq!(
        payloads[0],
        json!({ "telegram_user_id": bot_id }),
        "the audit must carry no secrets"
    );
    assert!(!payloads[0].to_string().contains(&code));
    let messages = outbox_messages(&db.pool, bot_id).await;
    assert!(
        messages
            .iter()
            .any(|message| message.contains("консоли администратора")),
        "the account receives the confirmation message: {messages:?}"
    );

    // Привязанная учётная запись новую ссылку привязки не получает: скрытого
    // кандидата нельзя было бы подтвердить.
    let (status, _, body) = send_form_from(
        &app,
        "/admin/telegram-link",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let html = String::from_utf8_lossy(&body);
    assert!(
        html.contains("уже привязан"),
        "the console explains the linked account:\n{html}"
    );
    assert!(!html.contains("?start=adm_"), "{html}");
    let links: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM admin_one_time_codes
         WHERE purpose = 'telegram_link' AND used_at IS NULL",
    )
    .fetch_one(&db.pool)
    .await
    .expect("telegram links");
    assert_eq!(links, 0, "no new link is issued for a linked account");

    let (status, _, body) = send_with_cookie(&app, Method::GET, "/admin", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let home = String::from_utf8_lossy(&body);
    assert!(home.contains("Alice"), "{home}");
    assert!(home.contains("Отвязать"), "{home}");

    bot.text("/start").await;
    bot.assert_screen_contains("Обратитесь к администратору");
    assert!(
        !bot.screen().contains("Добавить"),
        "the binding must not grant the household menu:\n{}",
        bot.screen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stranger_who_opened_the_link_can_be_rejected_and_stays_unbound() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[]).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "polling",
        "123:SECRET-link-reject",
        "link-reject-secret",
        902_004,
        "tasklerq_reject_bot",
    )
    .await;
    let (app, cookie) = owner_console_with_login(&db.pool, &config, TenancyMode::Single).await;

    let code = issue_telegram_link(&app, &cookie).await;
    let mut bot = owner_claim_harness(&db.pool, support::next_telegram_id(), TenancyMode::Single);
    let stranger = bot.chat;
    bot.text(&format!("/start adm_{code}")).await;

    send_candidate(&app, &cookie, "reject", stranger).await;

    let (bound, name, pending) = telegram_binding(&db.pool).await;
    assert_eq!(bound, None, "the rejected account is not bound");
    assert_eq!(name, None);
    assert_eq!(pending, None);
    assert!(
        outbox_messages(&db.pool, stranger)
            .await
            .iter()
            .all(|message| !message.contains("консоли администратора")),
        "a rejected account gets no confirmation message"
    );

    bot.text("/nosuchcommand").await;
    let unknown = bot.latest().1;
    bot.text("/admin").await;
    let admin = bot.latest().1;
    assert_eq!(
        admin, unknown,
        "`/admin` from an unbound account must behave like an unknown command"
    );
    assert!(admin.contains("Не знаю такой команды"), "{admin}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unconfirmed_telegram_link_does_not_bind_and_admin_is_unknown() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[]).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "polling",
        "123:SECRET-link-unconfirmed",
        "link-unconfirmed-secret",
        902_005,
        "tasklerq_unconfirmed_bot",
    )
    .await;
    let (app, cookie) = owner_console_with_login(&db.pool, &config, TenancyMode::Single).await;

    let code = issue_telegram_link(&app, &cookie).await;
    let mut bot = owner_claim_harness(&db.pool, support::next_telegram_id(), TenancyMode::Single);
    bot.text(&format!("/start adm_{code}")).await;

    let (bound, name, _) = telegram_binding(&db.pool).await;
    assert_eq!(bound, None);
    assert_eq!(name, None);

    bot.text("/admin").await;
    assert!(
        bot.latest().1.contains("Не знаю такой команды"),
        "`/admin` stays unknown without confirmation:\n{}",
        bot.latest().1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopening_the_same_link_keeps_the_first_candidate() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[]).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "polling",
        "123:SECRET-link-reopen",
        "link-reopen-secret",
        902_006,
        "tasklerq_reopen_bot",
    )
    .await;
    let (app, cookie) = owner_console_with_login(&db.pool, &config, TenancyMode::Single).await;

    let code = issue_telegram_link(&app, &cookie).await;
    let mut first = owner_claim_harness(&db.pool, support::next_telegram_id(), TenancyMode::Single);
    let first_id = first.chat;
    first.text(&format!("/start adm_{code}")).await;

    let mut second =
        owner_claim_harness(&db.pool, support::next_telegram_id(), TenancyMode::Single);
    second.text(&format!("/start adm_{code}")).await;
    assert!(
        !second.screen().contains("Запрос на привязку отправлен"),
        "a spent link behaves like `/start` without an argument:\n{}",
        second.screen()
    );

    let (_, _, pending) = telegram_binding(&db.pool).await;
    assert_eq!(pending, Some(first_id), "the first candidate is kept");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_candidate_confirmation_applies_nothing() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[]).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "polling",
        "123:SECRET-link-stale",
        "link-stale-secret",
        902_007,
        "tasklerq_stale_bot",
    )
    .await;
    let (app, cookie) = owner_console_with_login(&db.pool, &config, TenancyMode::Single).await;

    let first_code = issue_telegram_link(&app, &cookie).await;
    let mut first = owner_claim_harness(&db.pool, support::next_telegram_id(), TenancyMode::Single);
    let first_id = first.chat;
    first.text(&format!("/start adm_{first_code}")).await;

    let second_code = issue_telegram_link(&app, &cookie).await;
    let mut second =
        owner_claim_harness(&db.pool, support::next_telegram_id(), TenancyMode::Single);
    let second_id = second.chat;
    second.text(&format!("/start adm_{second_code}")).await;

    let candidate = first_id.to_string();
    let (status, _, body) = send_form_from(
        &app,
        "/admin/telegram-link/confirm",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[("candidate", candidate.as_str())],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let html = String::from_utf8_lossy(&body);
    assert!(
        html.contains("Кандидат"),
        "the stale form is explained:\n{html}"
    );

    let (bound, name, pending) = telegram_binding(&db.pool).await;
    assert_eq!(bound, None, "a stale candidate is not applied");
    assert_eq!(name, None);
    assert_eq!(pending, Some(second_id), "the current candidate stays");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unlinking_the_telegram_account_clears_it_and_writes_audit() {
    let db = support::isolated_db().await;
    let config = telegram_config(&db.pool, None, &[]).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "polling",
        "123:SECRET-link-unlink",
        "link-unlink-secret",
        902_008,
        "tasklerq_unlink_bot",
    )
    .await;
    let (app, cookie) = owner_console_with_login(&db.pool, &config, TenancyMode::Single).await;

    let code = issue_telegram_link(&app, &cookie).await;
    let mut bot = owner_claim_harness(&db.pool, support::next_telegram_id(), TenancyMode::Single);
    let bot_id = bot.chat;
    bot.text(&format!("/start adm_{code}")).await;
    send_candidate(&app, &cookie, "confirm", bot_id).await;

    let (status, _, body) = send_form_from(
        &app,
        "/admin/telegram-link/unlink",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    let (bound, name, pending) = telegram_binding(&db.pool).await;
    assert_eq!(bound, None, "unlinking clears the telegram id");
    assert_eq!(name, None, "unlinking clears the stored name");
    assert_eq!(pending, None);
    assert_eq!(
        audit_payloads(&db.pool, "admin_telegram_unlinked")
            .await
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_login_records_the_request_origin_including_a_trusted_proxy() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let app = console(
        &db.pool,
        vec![IpNet::parse("127.0.0.1").expect("a loopback network")],
    );

    let fields = [("username", "admin"), ("password", TEST_PASSWORD)];
    let (status, _, _) = send_form(&app, "/admin/login", &fields).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let origin: Option<String> = sqlx::query_scalar("SELECT console_origin FROM admin_accounts")
        .fetch_one(&db.pool)
        .await
        .expect("console origin");
    assert_eq!(origin.as_deref(), Some("http://nas:18081"));

    let forwarded = headers_with(&[
        ("x-forwarded-host", "console.example"),
        ("x-forwarded-proto", "https"),
        ("origin", "https://console.example"),
    ]);
    let (status, _, _) =
        send_form_from(&app, "/admin/login", test_peer(), forwarded, &fields).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let origin: Option<String> = sqlx::query_scalar("SELECT console_origin FROM admin_accounts")
        .fetch_one(&db.pool)
        .await
        .expect("console origin after the proxy");
    assert_eq!(origin.as_deref(), Some("https://console.example"));
}

/// Первое сохранение группы webhook и его события `admin_setting_changed`.
async fn first_webhook_save_changes(pool: &PgPool) -> Vec<(String, String)> {
    let server = MockServer::start().await;
    mount_get_me(&server, 901_100, "tasklerq_first_save_bot").await;
    let config = telegram_config(pool, Some(&server), &[]).await;
    let app = owner_console(pool, &config, TenancyMode::Single);
    insert_account(pool, "admin").await;
    let cookie = login_cookie(&app, "admin", TEST_PASSWORD).await;

    let (status, _, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", "123:SECRET-first-save"),
            ("public_base_url", "https://tasklerq.example.com"),
            ("webhook_url_override", ""),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "{}",
        String::from_utf8_lossy(&body)
    );

    audit_payloads(pool, "admin_setting_changed")
        .await
        .into_iter()
        .map(|payload| {
            (
                payload["key"].as_str().unwrap_or_default().to_owned(),
                payload["value"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

/// Удаление отсутствующей строки `telegram.webhook_url_override` не пишет
/// `deleted`: сохранение с пустым адресом не меняет базу.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_absent_webhook_override_is_not_audited_as_deleted() {
    let db = support::isolated_db().await;
    let changes = first_webhook_save_changes(&db.pool).await;
    assert!(
        !changes
            .iter()
            .any(|(key, value)| key == "telegram.webhook_url_override" && value == "deleted"),
        "no row means no deletion: {changes:?}"
    );
}

/// Первое сохранение в режиме webhook аудирует `telegram.mode`: значение
/// совпадает со умолчанием, но строка появляется в базе.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_webhook_save_audits_the_mode() {
    let db = support::isolated_db().await;
    let changes = first_webhook_save_changes(&db.pool).await;
    assert!(
        changes
            .iter()
            .any(|(key, value)| key == "telegram.mode" && value == "changed"),
        "the first save writes the mode row and must audit it: {changes:?}"
    );
}

/// Отказ `deleteWebhook` прежнего бота не скрывает отказ `setWebhook` нового:
/// сообщение о новом webhook показывается первым.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_old_webhook_removal_does_not_hide_the_new_webhook_failure() {
    let db = support::isolated_db().await;
    let server = MockServer::start().await;
    mount_bot_base(&server).await;
    mount_get_me_for(&server, BOT_B_TOKEN, 43, "tasklerq_new_owner_bot").await;
    mount_webhook_info(&server, 0, None).await;
    Mock::given(method("POST"))
        .and(path_regex(format!(r"^/bot{BOT_A_TOKEN}/deleteWebhook$")))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "ok": false,
            "error_code": 400,
            "description": "Bad Request: old webhook removal failed"
        })))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(format!(r"^/bot{BOT_B_TOKEN}/setWebhook$")))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "ok": false,
            "error_code": 400,
            "description": "Bad Request: new webhook registration failed"
        })))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(format!(r"^/bot{BOT_B_TOKEN}/getWebhookInfo$")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {"url": "", "pending_update_count": 0}
        })))
        .with_priority(1)
        .mount(&server)
        .await;

    let config = telegram_config(&db.pool, Some(&server), &[]).await;
    let (application, app, cookie, shutdown_tx) = telegram_app(&config, &db.pool).await;
    let cipher = SecretCipher::from_config(&config);
    seed_bot(
        &db.pool,
        &cipher,
        "webhook",
        BOT_A_TOKEN,
        BOT_A_SECRET,
        42,
        "tasklerq_old_owner_bot",
    )
    .await;
    assert!(
        application
            .telegram_supervisor
            .reload()
            .await
            .expect("bot A reload")
    );
    wait_for_token_requests(&server, "setWebhook", BOT_A_TOKEN, 1).await;

    let (status, _, body) = send_form_from(
        &app,
        "/admin/telegram",
        test_peer(),
        headers_with(&[("cookie", &cookie)]),
        &[
            ("mode", "webhook"),
            ("token", BOT_B_TOKEN),
            ("public_base_url", "https://tasklerq.example.com"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let text = String::from_utf8_lossy(&body);
    let new_message = "Telegram не подтвердил адрес webhook";
    let old_message = "не удалось снять webhook прежнего бота";
    assert!(
        text.contains(new_message),
        "the new webhook failure must be reported:\n{text}"
    );
    assert!(
        text.contains(old_message),
        "the old webhook removal failure must still be reported:\n{text}"
    );
    let new_at = text.find(new_message).expect("new webhook message");
    let old_at = text.find(old_message).expect("old webhook message");
    assert!(
        new_at < old_at,
        "the new webhook failure must come first:\n{text}"
    );

    stop_application(application, shutdown_tx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_valid_telegram_link_does_not_make_a_candidate_for_a_linked_account() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let bound = support::next_telegram_id();
    sqlx::query("UPDATE admin_accounts SET telegram_user_id = $1, telegram_name = 'Alice'")
        .bind(bound)
        .execute(&db.pool)
        .await
        .expect("linked account");

    // Код выпущен в обход консоли: путь `/start adm_` не должен записывать
    // кандидата, которого нельзя подтвердить.
    let code = codes::issue(&db.pool, Purpose::TelegramLink, None, Utc::now())
        .await
        .expect("a telegram link code");

    let mut bot = owner_claim_harness(&db.pool, support::next_telegram_id(), TenancyMode::Single);
    bot.text(&format!("/start adm_{}", code.code)).await;

    let (bound_id, name, pending) = telegram_binding(&db.pool).await;
    assert_eq!(bound_id, Some(bound), "the binding is unchanged");
    assert_eq!(name.as_deref(), Some("Alice"));
    assert_eq!(pending, None, "a linked account gets no hidden candidate");
    assert!(
        telegram_link_is_active(&db.pool, &code.code).await,
        "the rejected attempt behaves like an invalid code and does not consume it"
    );
    assert!(
        !bot.screen().contains("Запрос на привязку отправлен"),
        "the screen is `/start` without a link:\n{}",
        bot.screen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_candidate_name_keeps_the_surname_and_the_username() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    let code = codes::issue(&db.pool, Purpose::TelegramLink, None, Utc::now())
        .await
        .expect("a telegram link code");

    let mut bot = owner_claim_harness(&db.pool, support::next_telegram_id(), TenancyMode::Single);
    let telegram_id = bot.chat;
    let update = json!({
        "update_id": 9_000_001,
        "message": {
            "message_id": 9_000_001,
            "date": 1_700_000_000,
            "chat": {"id": telegram_id, "type": "private", "first_name": "Иван"},
            "from": {
                "id": telegram_id,
                "is_bot": false,
                "first_name": "Иван",
                "last_name": "Петров",
                "username": "ivan"
            },
            "text": format!("/start adm_{}", code.code)
        }
    });
    bot.enqueue(update).await;
    bot.pump().await;

    let (_, _, pending) = telegram_binding(&db.pool).await;
    assert_eq!(pending, Some(telegram_id));
    let pending_name: Option<String> =
        sqlx::query_scalar("SELECT pending_telegram_name FROM admin_accounts")
            .fetch_one(&db.pool)
            .await
            .expect("the candidate name");
    assert_eq!(pending_name.as_deref(), Some("Иван Петров @ivan"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_request_origin_does_not_change_the_console_origin() {
    let db = support::isolated_db().await;
    insert_account(&db.pool, "admin").await;
    sqlx::query("UPDATE admin_accounts SET console_origin = 'http://nas:18081'")
        .execute(&db.pool)
        .await
        .expect("known origin");

    let state = console_state(&db.pool, Vec::new());
    // Без `Host` origin запроса неизвестен: вход не должен затирать адрес.
    let client = state.client(test_peer(), &HeaderMap::new());
    assert!(client.host.is_none());
    tasklerq::admin_console::session::start(&state, &client, Utc::now())
        .await
        .expect("a session without an origin");

    let origin: Option<String> = sqlx::query_scalar("SELECT console_origin FROM admin_accounts")
        .fetch_one(&db.pool)
        .await
        .expect("console origin");
    assert_eq!(origin.as_deref(), Some("http://nas:18081"));
}
