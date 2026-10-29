//! HTTP-сервер с graceful shutdown.
//!
//! Таймаут действует только на завершение уже принятых запросов после сигнала
//! остановки. До сигнала сервер работает без ограничения по времени.

use std::future::{Future, IntoFuture};
use std::io;
use std::time::Duration;

use axum::Router;
use tokio::net::TcpListener;

/// Максимальное время на завершение текущих запросов после сигнала остановки.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// Запустить сервер и ждать `shutdown`.
pub async fn serve<F>(listener: TcpListener, app: Router, shutdown: F) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    serve_with_grace(listener, app, shutdown, SHUTDOWN_GRACE).await
}

/// Запустить сервер с настраиваемым временем graceful shutdown.
///
/// Параметр `grace` вынесен отдельно, чтобы тест мог проверить, что сервер не
/// завершается по таймауту до получения сигнала.
pub async fn serve_with_grace<F>(
    listener: TcpListener,
    app: Router,
    shutdown: F,
    grace: Duration,
) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .into_future();
    tokio::pin!(server);

    tokio::select! {
        result = server.as_mut() => result,
        () = shutdown => {
            tracing::info!("shutdown signal received");
            let _ = shutdown_tx.send(());

            match tokio::time::timeout(grace, server.as_mut()).await {
                Ok(result) => result,
                Err(_) => {
                    tracing::warn!(
                        grace_seconds = grace.as_secs(),
                        "graceful shutdown exceeded"
                    );
                    Ok(())
                }
            }
        }
    }
}
