//! HTTP-сервер с graceful shutdown.
//!
//! Таймаут действует только на завершение уже принятых запросов после сигнала
//! остановки. До сигнала сервер работает без ограничения по времени.

use std::future::{Future, IntoFuture};
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use tokio::net::TcpListener;

use crate::config::Config;

/// Максимальное время на завершение текущих запросов после сигнала остановки.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// Связанные listener'ы сервиса.
pub struct Listeners {
    /// Публичный HTTP-listener.
    pub public: TcpListener,
    /// Listener консоли; `None`, если консоль выключена.
    pub admin: Option<TcpListener>,
}

/// Связать публичный listener и, если консоль включена, listener консоли.
///
/// Ошибка связывания называет адрес: при занятом порте консоли оператор должен
/// видеть, какой именно listener не поднялся.
pub async fn bind_listeners(config: &Config) -> io::Result<Listeners> {
    let public = bind(config.bind_addr, "http server").await?;
    let admin = match config.admin_bind_addr {
        Some(address) => Some(bind(address, "admin console").await?),
        None => None,
    };

    Ok(Listeners { public, admin })
}

async fn bind(address: SocketAddr, name: &str) -> io::Result<TcpListener> {
    TcpListener::bind(address).await.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to bind {name} to {address}: {error}"),
        )
    })
}

/// Запустить публичный listener и, если передан, listener консоли.
///
/// У listener'ов общий сигнал остановки: `shutdown` завершается один раз, оба
/// сервера получают его и дорабатывают текущие запросы.
pub async fn serve_listeners<F>(
    public: TcpListener,
    public_router: Router,
    admin: Option<(TcpListener, Router)>,
    shutdown: F,
) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let signal = tokio::spawn(async move {
        shutdown.await;
        let _ = stop_tx.send(true);
    });

    let public = serve(public, public_router, wait_for_stop(stop_rx.clone()));
    let admin = async move {
        match admin {
            Some((listener, router)) => serve(listener, router, wait_for_stop(stop_rx)).await,
            None => Ok(()),
        }
    };

    let result = tokio::try_join!(public, admin).map(|_| ());
    signal.abort();
    result
}

async fn wait_for_stop(mut stop: tokio::sync::watch::Receiver<bool>) {
    loop {
        if *stop.borrow_and_update() {
            return;
        }
        if stop.changed().await.is_err() {
            return;
        }
    }
}

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
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
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
