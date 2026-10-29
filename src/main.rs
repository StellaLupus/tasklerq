//! Точка входа Tasklerq.

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use tasklerq::admin;
use tasklerq::config::{self, Config, DatabaseConfig};
use tasklerq::domain::error::DomainError;
use tasklerq::domain::models::UserRole;
use tasklerq::http::router::{AppState, build_router};
use tasklerq::http::server;
use tasklerq::storage;
use tokio::net::TcpListener;
use tokio::signal;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

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
    /// Административные команды: токены, household и пользователи.
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
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

#[derive(Debug, Subcommand)]
enum AdminCommand {
    /// API-токены.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Household.
    Household {
        #[command(subcommand)]
        command: HouseholdCommand,
    },
    /// Пользователи.
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
}

#[derive(Debug, Subcommand)]
enum TokenCommand {
    /// Выпустить токен. Значение показывается один раз.
    Create(TokenCreate),
    /// Отозвать токен по идентификатору.
    Revoke {
        /// Идентификатор токена.
        id: Uuid,
    },
    /// Показать токены household без секретов.
    List {
        /// Household; без значения используется единственный существующий.
        #[arg(long)]
        household: Option<Uuid>,
    },
}

#[derive(Debug, Args)]
struct TokenCreate {
    /// Household; без значения используется единственный существующий.
    #[arg(long)]
    household: Option<Uuid>,
    /// Название токена.
    #[arg(long)]
    name: String,
    /// Scopes через запятую: ha:read, ha:write, capture:write.
    #[arg(long, value_delimiter = ',', required = true)]
    scopes: Vec<String>,
    /// Срок действия в днях; без значения токен бессрочный.
    #[arg(long)]
    expires_in_days: Option<i64>,
}

#[derive(Debug, Subcommand)]
enum HouseholdCommand {
    /// Создать household без владельца.
    Create {
        /// Название household.
        #[arg(long)]
        name: String,
        /// Часовой пояс IANA.
        #[arg(long, default_value = "Europe/Moscow")]
        timezone: String,
    },
}

#[derive(Debug, Subcommand)]
enum UserCommand {
    /// Добавить участника по Telegram ID.
    Add {
        /// Telegram user id (в приватном чате равен chat id).
        #[arg(long)]
        telegram_id: i64,
        /// Роль. Второй владелец в household не создаётся.
        #[arg(long, value_enum)]
        role: RoleArg,
        /// Household; без значения используется единственный существующий.
        #[arg(long)]
        household: Option<Uuid>,
        /// Отображаемое имя.
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RoleArg {
    Owner,
    Member,
}

impl From<RoleArg> for UserRole {
    fn from(role: RoleArg) -> Self {
        match role {
            RoleArg::Owner => Self::Owner,
            RoleArg::Member => Self::Member,
        }
    }
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
        Command::Admin { command } => run_admin(command).await,
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
    storage::migrations::run(&pool)
        .await
        .context("database migration failed")?;
    storage::seed::run(&pool).await.context("seed failed")?;

    tracing::info!("seed complete");
    Ok(())
}

async fn run_admin(command: AdminCommand) -> Result<()> {
    init_cli_tracing();

    let database = DatabaseConfig::from_env().context("invalid database configuration")?;
    let pool = storage::db::connect(&database)
        .await
        .context("database connection failed")?;
    let now = Utc::now();

    match command {
        AdminCommand::Token { command } => match command {
            TokenCommand::Create(args) => {
                let issued = admin::create_token(
                    &pool,
                    args.household,
                    &args.name,
                    &args.scopes,
                    args.expires_in_days,
                    now,
                )
                .await
                .map_err(explain)?;
                println!("id: {}", issued.record.id);
                println!("name: {}", issued.record.name);
                println!("scopes: {}", issued.record.scopes.join(","));
                match issued.record.expires_at {
                    Some(expires_at) => println!("expires_at: {}", expires_at.to_rfc3339()),
                    None => println!("expires_at: never"),
                }
                println!("token: {}", issued.token);
                eprintln!("Сохраните токен сейчас: повторно его показать нельзя.");
            }
            TokenCommand::Revoke { id } => {
                let (token, revoked_now) =
                    admin::revoke_token(&pool, id, now).await.map_err(explain)?;
                if revoked_now {
                    println!("revoked: {} ({})", token.id, token.prefix);
                } else {
                    println!("already revoked: {} ({})", token.id, token.prefix);
                }
            }
            TokenCommand::List { household } => {
                for token in admin::list_tokens(&pool, household)
                    .await
                    .map_err(explain)?
                {
                    let state = if token.revoked_at.is_some() {
                        "revoked"
                    } else {
                        "active"
                    };
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        token.id,
                        token.prefix,
                        state,
                        token.scopes.join(","),
                        token.name
                    );
                }
            }
        },
        AdminCommand::Household { command } => match command {
            HouseholdCommand::Create { name, timezone } => {
                let household = admin::create_household(&pool, &name, &timezone, now)
                    .await
                    .map_err(explain)?;
                println!("id: {}", household.id);
                println!("name: {}", household.name);
                println!("timezone: {}", household.timezone);
            }
        },
        AdminCommand::User { command } => match command {
            UserCommand::Add {
                telegram_id,
                role,
                household,
                name,
            } => {
                let (household, user) = admin::add_user(
                    &pool,
                    household,
                    role.into(),
                    telegram_id,
                    name.as_deref(),
                    now,
                )
                .await
                .map_err(explain)?;
                println!("household: {}", household.id);
                println!("user: {}", user.id);
                println!("telegram_id: {}", user.telegram_user_id);
            }
        },
    }

    Ok(())
}

fn explain(error: DomainError) -> anyhow::Error {
    anyhow::anyhow!(admin::describe(&error))
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
