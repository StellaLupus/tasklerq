//! Smoke-тест инфраструктуры: реальный PostgreSQL, миграции, readiness и
//! graceful shutdown.

use std::time::Duration;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use tokio::net::TcpListener;

use tasklerq::config::{DatabaseConfig, SecretString};
use tasklerq::http::router::{AppState, build_router};
use tasklerq::http::server::serve_with_grace;
use tasklerq::storage;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serves_readiness_and_shuts_down_only_after_signal() {
    let container = GenericImage::new("postgres", "18-alpine")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_USER", "tasklerq")
        .with_env_var("POSTGRES_PASSWORD", "tasklerq")
        .with_env_var("POSTGRES_DB", "tasklerq")
        .start()
        .await
        .expect("postgres container should start");

    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("postgres port should be mapped");
    let database = DatabaseConfig {
        url: SecretString::new(format!(
            "postgres://tasklerq:tasklerq@127.0.0.1:{port}/tasklerq?sslmode=disable"
        )),
        max_connections: 5,
    };

    let pool = storage::db::connect(&database)
        .await
        .expect("database should accept connections");
    storage::migrations::run(&pool)
        .await
        .expect("migrations should apply to an empty database");
    storage::migrations::run(&pool)
        .await
        .expect("repeated migrations should be idempotent");

    let statement_timeout: String = sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(&pool)
        .await
        .expect("statement_timeout should be readable");
    assert_eq!(statement_timeout, "30s");

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener.local_addr().expect("bound address");
    let app = build_router(AppState::new(pool));

    let grace = Duration::from_millis(200);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve_with_grace(
        listener,
        app,
        async move {
            let _ = shutdown_rx.await;
        },
        grace,
    ));

    // Старая реализация оборачивала весь сервер в 30-секундный таймаут и
    // завершалась даже без сигнала. Короткий grace делает ту же регрессию
    // видимой без 30-секундного теста.
    tokio::time::sleep(grace * 3).await;
    assert!(
        !server.is_finished(),
        "server must outlive the shutdown grace period"
    );

    let response = reqwest::get(format!("http://{address}/health/ready"))
        .await
        .expect("readiness request should complete");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("readiness should be JSON");
    assert_eq!(body["status"], "ok");

    shutdown_tx.send(()).expect("server should await shutdown");
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server should stop promptly after the signal")
        .expect("server task should not panic")
        .expect("server should exit cleanly");
}
