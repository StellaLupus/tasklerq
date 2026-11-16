//! Консоль администратора: отдельный listener, роутер и определение клиента.

mod support;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, header};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::PgPool;
use tasklerq::admin_console::codes::{self, Purpose};
use tasklerq::admin_console::router::{AdminState, build_router as build_admin_router};
use tasklerq::admin_console::setup::{self, SetupError};
use tasklerq::app::build_application;
use tasklerq::auth::tokens::hash_token;
use tasklerq::config::{Config, IpNet};
use tasklerq::domain::error::DomainError;
use tasklerq::http::router::{AppState, build_router};
use tasklerq::http::server::{bind_listeners, serve_listeners};
use tasklerq::security_audit::Audit;
use tower::ServiceExt;
use url::form_urlencoded;
use uuid::Uuid;

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

fn console(pool: &PgPool, trusted_proxies: Vec<IpNet>) -> Router {
    build_admin_router(AdminState::new(
        pool.clone(),
        trusted_proxies,
        Audit::new(pool.clone()),
    ))
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
    let app = build_admin_router(AdminState::new(
        db.pool.clone(),
        Vec::new(),
        Audit::new(db.pool.clone()),
    ));

    let (status, headers, _) = send(&app, Method::GET, "/admin").await;

    assert_eq!(status, StatusCode::SEE_OTHER);
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
    let console = build_admin_router(AdminState::new(
        db.pool.clone(),
        Vec::new(),
        Audit::new(db.pool.clone()),
    ));
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
    assert_eq!(location(&headers), Some("/admin"));
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
    assert_eq!(location(&headers), Some("/admin"));
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

    let state = AdminState::new(db.pool.clone(), Vec::new(), Audit::new(db.pool.clone()));
    let result = setup::create_account(
        &state,
        &issued.code,
        "second",
        "unused-password-hash",
        Utc::now(),
    )
    .await;
    assert!(
        matches!(result, Err(SetupError::AlreadyConfigured)),
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
    let db = support::db().await;
    let state = AdminState::new(db.pool.clone(), Vec::new(), Audit::new(db.pool.clone()));
    let ip: std::net::IpAddr = "203.0.113.5".parse().expect("an IP address");
    let now = Utc::now();

    for _ in 0..5 {
        state.setup_limiter().record(&ip, now);
    }
    assert!(!state.setup_limiter().peek(&ip, now).allowed);
    assert!(state.login_limiter().peek(&ip, now).allowed);
    assert!(state.current_password_limiter().peek(&ip, now).allowed);
    assert!(state.recovery_limiter().peek(&ip, now).allowed);

    for _ in 0..5 {
        state.login_limiter().record(&ip, now);
    }
    assert!(!state.login_limiter().peek(&ip, now).allowed);
    assert!(
        state.recovery_limiter().peek(&ip, now).allowed,
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
    assert_eq!(location(&headers), Some("/admin"));
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
        Some("/admin"),
        "an active session opens the main page instead of the login form"
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
    assert_eq!(location(&response_headers), Some("/admin"));
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
