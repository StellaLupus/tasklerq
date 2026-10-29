//! Точка входа Tasklerq.

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tasklerq::config::{self, Config, DatabaseConfig};
use tasklerq::http::router::{AppState, build_router};
use tasklerq::http::server;
use tasklerq::storage;
use tokio::net::TcpListener;
use tokio::signal;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "tasklerq",
    version,
    about = "Семейные задачи и покупки: Telegram, Home Assistant и календарь"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Запустить HTTP-сервер и фоновые воркеры.
    Serve,
    /// Применить миграции базы данных.
    Migrate,
    /// Заполнить базу демонстрационными данными.
    Seed,
    /// Проверить readiness-эндпоинт (используется Docker healthcheck).
    Healthcheck {
        /// URL проверки.
        #[arg(
            long,
            default_value = "http://127.0.0.1:8080/health/ready",
            value_name = "URL"
        )]
        url: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    // .env удобен локально и в docker compose; в production переменные приходят
    // из env-file или Docker secrets.
    dotenvy::dotenv().ok();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tasklerq: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve().await,
        Command::Migrate => migrate().await,
        Command::Seed => seed().await,
        Command::Healthcheck { url } => healthcheck(&url).await,
    }
}

async fn serve() -> Result<()> {
    let config = Config::from_env().context("invalid configuration")?;
    init_tracing(&config.log_filter);

    tracing::info!(
        version = tasklerq::VERSION,
        env = config.env.as_str(),
        "starting tasklerq"
    );
    config.log_startup();

    let pool = storage::db::connect(&config.database)
        .await
        .context("database connection failed")?;
    storage::migrations::run(&pool)
        .await
        .context("database migration failed")?;

    let listener = TcpListener::bind(config.bind_addr)
        .await
        .with_context(|| format!("failed to bind {}", config.bind_addr))?;
    let address = listener
        .local_addr()
        .context("failed to read the bound address")?;

    let app = build_router(AppState::new(pool));
    tracing::info!(%address, "http server listening");

    server::serve(listener, app, shutdown_signal())
        .await
        .context("http server failed")?;

    tracing::info!("stopped");
    Ok(())
}

async fn migrate() -> Result<()> {
    init_cli_tracing();

    let database = DatabaseConfig::from_env().context("invalid database configuration")?;
    let pool = storage::db::connect(&database)
        .await
        .context("database connection failed")?;
    storage::migrations::run(&pool)
        .await
        .context("database migration failed")?;

    tracing::info!("migrations applied");
    Ok(())
}

async fn seed() -> Result<()> {
    init_cli_tracing();

    let database = DatabaseConfig::from_env().context("invalid database configuration")?;
    let pool = storage::db::connect(&database)
        .await
        .context("database connection failed")?;
    storage::seed::run(&pool).await.context("seed failed")?;

    tracing::info!("seed complete");
    Ok(())
}

async fn healthcheck(url: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("failed to build HTTP client")?;

    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("healthcheck request to {url} failed"))?;

    if !response.status().is_success() {
        bail!("healthcheck failed with status {}", response.status());
    }

    Ok(())
}

fn init_cli_tracing() {
    init_tracing(&config::log_filter_from_env());
}

fn init_tracing(filter: &str) {
    let filter =
        EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new(config::DEFAULT_LOG_FILTER));

    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_current_span(true)
        .with_span_list(false)
        .finish();

    if let Err(error) = tracing::subscriber::set_global_default(subscriber) {
        eprintln!("tasklerq: failed to install logging subscriber: {error}");
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = signal::ctrl_c().await {
            tracing::error!(%error, "failed to listen for Ctrl+C");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "failed to listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
