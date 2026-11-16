//! Консоль администратора: отдельный listener, роутер и определение клиента.

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use chrono::{Duration as ChronoDuration, Utc};
use http_body_util::BodyExt;
use tasklerq::admin_console::codes::{self, Purpose};
use tasklerq::admin_console::router::{AdminState, build_router as build_admin_router};
use tasklerq::app::build_application;
use tasklerq::auth::tokens::hash_token;
use tasklerq::config::Config;
use tasklerq::domain::error::DomainError;
use tasklerq::http::router::{AppState, build_router};
use tasklerq::http::server::{bind_listeners, serve_listeners};
use tasklerq::security_audit::Audit;
use tower::ServiceExt;
use uuid::Uuid;

const REQUEST_ID: &str = "test-request-id";

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

async fn send(app: &Router, method: Method, path: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("x-request-id", REQUEST_ID)
                .body(Body::empty())
                .expect("request should build"),
        )
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
    let app = build_admin_router(AdminState::new(db.pool.clone(), Vec::new()));

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
    let console = build_admin_router(AdminState::new(db.pool.clone(), Vec::new()));
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
