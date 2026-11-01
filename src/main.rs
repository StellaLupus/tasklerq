//! Точка входа Tasklerq.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use tasklerq::admin;
use tasklerq::app::build_application;
use tasklerq::capture::{eval, prompt};
use tasklerq::config::{self, Config, DatabaseConfig, LlmConfig};
use tasklerq::domain::error::DomainError;
use tasklerq::domain::models::UserRole;
use tasklerq::home_assistant::{AiTask, GenerateData, HttpAiTask, MAX_GENERATE_TIMEOUT};
use tasklerq::http::server;
use tasklerq::redaction;
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
    /// Прогнать golden set LLM-capture и записать baseline.
    CaptureEval(CaptureEvalArgs),
}

#[derive(Debug, Args)]
struct CaptureEvalArgs {
    /// Файл golden set.
    #[arg(long, default_value = "tests/golden/capture.json", value_name = "PATH")]
    golden: PathBuf,
    /// Baseline по версиям промпта.
    #[arg(
        long,
        default_value = "tests/golden/baseline.json",
        value_name = "PATH"
    )]
    baseline: PathBuf,
}

#[derive(Debug, Subcommand)]
enum AdminCommand {
    /// API-токены.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Очереди заданий.
    Queue {
        #[command(subcommand)]
        command: QueueCommand,
    },
    /// Состояние webhook Telegram.
    Webhook {
        #[command(subcommand)]
        command: WebhookCommand,
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
    /// Выгрузить данные household в JSON.
    Export(ExportArgs),
}

/// Команды, для которых нужна база данных.
///
/// Отдельный тип не даёт передать в [`run_admin_database`] команду webhook,
/// которая работает без базы.
#[derive(Debug)]
enum DatabaseCommand {
    Token(TokenCommand),
    Queue(QueueCommand),
    Household(HouseholdCommand),
    User(UserCommand),
    Export(ExportArgs),
}

#[derive(Debug, Subcommand)]
enum QueueCommand {
    /// Количество заданий по статусам и возраст самой старой готовой записи.
    Stats,
}

#[derive(Debug, Subcommand)]
enum WebhookCommand {
    /// Показать состояние webhook без токена бота.
    Info,
}

#[derive(Debug, Args)]
struct ExportArgs {
    /// Household; без значения используется единственный существующий.
    #[arg(long)]
    household: Option<Uuid>,
    /// Файл выгрузки; без значения JSON печатается в stdout.
    #[arg(long, value_name = "PATH")]
    output: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum TokenCommand {
    /// Выпустить токен. Значение показывается один раз.
    Create(TokenCreate),
    /// Отозвать токен по идентификатору.
    Revoke {
        /// Идентификатор токена.
        id: Uuid,
        /// Не спрашивать подтверждение.
        #[arg(long)]
        yes: bool,
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
    /// Scopes через запятую.
    #[arg(
        long,
        value_delimiter = ',',
        required = true,
        value_name = "SCOPE",
        help = scopes_help()
    )]
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
    /// Показать household без секретов.
    List,
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

/// Справка `--scopes`, собранная из [`tasklerq::auth::tokens::SCOPES`], чтобы
/// список не расходился с валидацией.
fn scopes_help() -> String {
    format!(
        "Scopes через запятую: {}.",
        tasklerq::auth::tokens::SCOPES.join(", ")
    )
}

#[tokio::main]
async fn main() -> ExitCode {
    // .env удобен локально и в docker compose; в production переменные приходят
    // из env-file или Docker secrets.
    dotenvy::dotenv().ok();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let message = redaction::sanitize(&format!("{error:#}"));
            eprintln!("tasklerq: {message}");
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
        Command::CaptureEval(args) => capture_eval(args).await,
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

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let application = build_application(&config, pool, shutdown_rx)
        .await
        .context("telegram startup failed")?;
    let workers = application.workers;
    let app = application.router;
    tracing::info!(
        mode = config.telegram.mode.as_str(),
        workers = workers.len(),
        "telegram configured"
    );

    tracing::info!(%address, "http server listening");

    server::serve(listener, app, async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    })
    .await
    .context("http server failed")?;

    for worker in workers {
        if tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .is_err()
        {
            tracing::warn!("a background worker did not stop in time");
        }
    }

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

    match command {
        // Webhook не требует базы: команда работает даже при недоступном
        // PostgreSQL и использует ту же конфигурацию Telegram, что сервис.
        AdminCommand::Webhook { .. } => webhook_info().await,
        AdminCommand::Token { command } => {
            run_admin_database(DatabaseCommand::Token(command)).await
        }
        AdminCommand::Queue { command } => {
            run_admin_database(DatabaseCommand::Queue(command)).await
        }
        AdminCommand::Household { command } => {
            run_admin_database(DatabaseCommand::Household(command)).await
        }
        AdminCommand::User { command } => run_admin_database(DatabaseCommand::User(command)).await,
        AdminCommand::Export(args) => run_admin_database(DatabaseCommand::Export(args)).await,
    }
}

async fn run_admin_database(command: DatabaseCommand) -> Result<()> {
    let database = DatabaseConfig::from_env().context("invalid database configuration")?;
    let pool = storage::db::connect(&database)
        .await
        .context("database connection failed")?;
    let now = Utc::now();

    match command {
        DatabaseCommand::Token(command) => match command {
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
            TokenCommand::Revoke { id, yes } => {
                if !yes {
                    require_confirmation(&format!("Отозвать токен {id}? [y/N] "))?;
                }
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
        DatabaseCommand::Queue(command) => match command {
            QueueCommand::Stats => {
                let stats = admin::queue_stats(&pool, now).await.map_err(explain)?;
                print_status_counts(
                    "telegram_updates",
                    &stats.telegram_updates,
                    &["pending", "processed", "dead"],
                );
                print_status_counts(
                    "outbox",
                    &stats.outbox,
                    &["pending", "sent", "dead", "skipped"],
                );
                print_status_counts(
                    "capture_jobs",
                    &stats.capture_jobs,
                    &["pending", "running", "done", "dead"],
                );
                print_status_counts(
                    "notification_deliveries",
                    &stats.notification_deliveries,
                    &["pending", "sent", "cancelled"],
                );
                match stats.oldest_ready {
                    Some(oldest) => println!(
                        "oldest_ready_age_seconds: {}",
                        (stats.now - oldest).num_seconds().max(0)
                    ),
                    None => println!("oldest_ready_age_seconds: none"),
                }
            }
        },
        DatabaseCommand::Household(command) => match command {
            HouseholdCommand::Create { name, timezone } => {
                let household = admin::create_household(&pool, &name, &timezone, now)
                    .await
                    .map_err(explain)?;
                println!("id: {}", household.id);
                println!("name: {}", household.name);
                println!("timezone: {}", household.timezone);
            }
            HouseholdCommand::List => {
                for household in admin::list_households(&pool).await.map_err(explain)? {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        household.id,
                        household.name,
                        household.timezone,
                        household.members,
                        household.created_at.to_rfc3339()
                    );
                }
            }
        },
        DatabaseCommand::User(command) => match command {
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
        DatabaseCommand::Export(args) => {
            let export = admin::export(&pool, args.household, now)
                .await
                .map_err(explain)?;
            let mut json =
                serde_json::to_string_pretty(&export).context("failed to serialize the export")?;
            json.push('\n');
            match args.output {
                Some(path) => write_export_file(&path, &json)?,
                None => print!("{json}"),
            }
        }
    }

    Ok(())
}

/// Спросить подтверждение в интерактивном терминале. Без TTY опасная команда
/// выполняется только с `--yes`.
fn require_confirmation(prompt: &str) -> Result<()> {
    use std::io::{IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        bail!("нужно подтверждение (--yes); ничего не изменено");
    }
    eprint!("{prompt}");
    std::io::stderr()
        .flush()
        .context("failed to write the confirmation prompt")?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("failed to read the confirmation")?;
    if matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "д" | "да"
    ) {
        Ok(())
    } else {
        bail!("отмена; ничего не изменено");
    }
}

fn print_status_counts(name: &str, counts: &[(String, i64)], known: &[&str]) {
    println!("{name}:");
    for status in known {
        let count = counts
            .iter()
            .find(|(value, _)| value == status)
            .map_or(0, |(_, count)| *count);
        println!("  {status}: {count}");
    }
    for (status, count) in counts {
        if !known.contains(&status.as_str()) {
            println!("  {status}: {count}");
        }
    }
}

fn webhook_display_url(raw: &str) -> String {
    let clean = |value: &str| {
        let end = value.find(['?', '#']).unwrap_or(value.len());
        redaction::sanitize(&redaction::redact_url(&value[..end]))
    };

    let Ok(url) = url::Url::parse(raw) else {
        return clean(raw);
    };

    let mut shown = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
    if let Some(port) = url.port() {
        shown.push(':');
        shown.push_str(&port.to_string());
    }
    shown.push_str(url.path());
    clean(&shown)
}

async fn webhook_info() -> Result<()> {
    let config = Config::from_env().context("invalid configuration")?;
    let token = config
        .telegram
        .bot_token
        .clone()
        .context("TELEGRAM_BOT_TOKEN is not configured")?;
    let base_url = config
        .telegram
        .api_base_url
        .as_ref()
        .map_or(tasklerq::telegram::api::DEFAULT_BASE_URL, |url| {
            url.as_str()
        });
    let api = tasklerq::telegram::api::HttpTelegramApi::new(token, base_url)
        .context("failed to build the Telegram client")?;
    let info = tasklerq::telegram::api::TelegramApi::get_webhook_info(&api)
        .await
        .context("getWebhookInfo failed")?;

    println!("url: {}", webhook_display_url(&info.url));
    println!("pending_update_count: {}", info.pending_update_count);
    match info.last_error_date {
        Some(timestamp) => println!(
            "last_error_date: {}",
            chrono::DateTime::from_timestamp(timestamp, 0)
                .map(|value| value.to_rfc3339())
                .unwrap_or_else(|| timestamp.to_string())
        ),
        None => println!("last_error_date: none"),
    }
    println!(
        "last_error_message: {}",
        info.last_error_message
            .as_deref()
            .map_or_else(|| "none".to_owned(), redaction::sanitize)
    );
    match info.max_connections {
        Some(max_connections) => println!("max_connections: {max_connections}"),
        None => println!("max_connections: none"),
    }
    println!(
        "ip_address: {}",
        info.ip_address
            .as_deref()
            .map_or_else(|| "none".to_owned(), redaction::sanitize)
    );

    Ok(())
}

fn write_export_file(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;

    // Режим при создании не меняет уже существующий файл: выставляем права на
    // открытом дескрипторе до записи, чтобы данные не оказались в файле с
    // чужими правами даже на мгновение.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }

    file.write_all(contents.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    drop(file);

    Ok(())
}

fn explain(error: DomainError) -> anyhow::Error {
    anyhow::anyhow!(admin::describe(&error))
}

/// Прогнать golden set через настоящий путь разбора: тот же промпт, парсер,
/// валидация и клиент Home Assistant, что и в проде.
async fn capture_eval(args: CaptureEvalArgs) -> Result<()> {
    let text = std::fs::read_to_string(&args.golden)
        .with_context(|| format!("не удалось прочитать {}", args.golden.display()))?;
    let set = eval::GoldenSet::parse(&text).context("некорректный golden set")?;

    let llm = LlmConfig::from_env().context("некорректная конфигурация LLM")?;
    if !llm.enabled {
        bail!("LLM_CAPTURE_ENABLED=false: для capture-eval нужен включённый разбор");
    }
    let (Some(url), Some(token), Some(entity)) =
        (&llm.base_url, &llm.token, &llm.ai_task_entity_id)
    else {
        bail!("задайте HA_BASE_URL, HA_TOKEN и HA_AI_TASK_ENTITY_ID");
    };
    let ai = HttpAiTask::new(url.as_str(), token.clone(), entity)
        .context("некорректный клиент Home Assistant")?;

    let structure = prompt::response_structure();
    let mut responses = BTreeMap::new();
    for phrase in &set.phrases {
        let context = phrase
            .effective_context(&set.context)
            .to_context()
            .map_err(|error| anyhow::anyhow!("фраза {}: {error}", phrase.id))?;
        let instructions = prompt::build_instructions(&context, &phrase.text);
        let request = GenerateData {
            task_name: prompt::TASK_NAME,
            instructions: &instructions,
            structure: &structure,
        };
        let mut retried = false;
        let data = loop {
            match ai.generate_data(&request, MAX_GENERATE_TIMEOUT).await {
                Ok(data) => break data,
                // Один повтор для тех же ошибок, что очередь повторяет в проде:
                // иначе один 503 из 36 вызовов выбрасывает весь прогон.
                Err(error) if error.is_retryable() && !retried => {
                    retried = true;
                    tracing::warn!(
                        error = error.code(),
                        phrase = %phrase.id,
                        "retrying capture eval request"
                    );
                }
                Err(error) => {
                    return Err(anyhow::Error::new(error)).with_context(|| {
                        format!("Home Assistant не разобрал фразу {}", phrase.id)
                    });
                }
            }
        };
        responses.insert(phrase.id.clone(), data);
    }

    let report = eval::evaluate(&set, &responses);
    println!(
        "Golden set: {} фраз, промпт {}",
        set.phrases.len(),
        prompt::PROMPT_VERSION
    );
    print_report(&report);

    let accuracy = report.overall().accuracy();
    let mut baseline = eval::Baseline::load(&args.baseline)
        .with_context(|| format!("не удалось прочитать {}", args.baseline.display()))?;
    baseline.record(
        prompt::PROMPT_VERSION,
        eval::BaselineEntry {
            accuracy,
            prompt_hash: eval::prompt_hash(),
            date: Utc::now().to_rfc3339(),
            phrases: set.phrases.len(),
            passed: report.passed(),
        },
    );
    baseline
        .save(&args.baseline)
        .with_context(|| format!("не удалось записать {}", args.baseline.display()))?;

    if accuracy < eval::MIN_ACCURACY {
        bail!(
            "точность {accuracy:.3} ниже минимальной {}: версия {} не годится",
            eval::MIN_ACCURACY,
            prompt::PROMPT_VERSION
        );
    }
    Ok(())
}

fn print_report(report: &eval::Report) {
    let overall = report.overall();
    println!(
        "Точность: {:.1}% (совпало {} из {}; ожидалось {}, лишних {})",
        overall.accuracy() * 100.0,
        overall.matched,
        overall.expected + overall.extra,
        overall.expected,
        overall.extra
    );
    println!(
        "Фраз пройдено: {} из {}",
        report.passed(),
        report.results.len()
    );
    println!("По категориям:");
    for (category, totals) in report.by_category() {
        println!(
            "  {:<22} {:>6.1}% ({} из {})",
            category.label(),
            totals.accuracy() * 100.0,
            totals.matched,
            totals.expected + totals.extra
        );
    }

    let failed: Vec<_> = report.failed().collect();
    if failed.is_empty() {
        println!("Все фразы прошли.");
        return;
    }
    println!("Провалились:");
    for result in failed {
        println!(
            "  {} ({}): точность {:.0}%",
            result.id,
            result.category.label(),
            result.accuracy() * 100.0
        );
        if let Some(error) = &result.error {
            println!("    ошибка: {error}");
        }
        for problem in &result.problems {
            println!("    - {problem}");
        }
    }
}

async fn healthcheck(url: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("failed to build HTTP client")?;

    let response = client.get(url).send().await.with_context(|| {
        format!(
            "healthcheck request to {} failed",
            redaction::redact_url(url)
        )
    })?;

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
