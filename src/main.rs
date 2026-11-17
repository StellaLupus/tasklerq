//! Точка входа Tasklerq.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use tasklerq::admin;
use tasklerq::admin_console;
use tasklerq::app::build_application;
use tasklerq::capture::eval::TitleMatch;
use tasklerq::capture::slot_check;
use tasklerq::config::{
    self, Config, DatabaseConfig, LlmConfig, SecretKeyConfig, SecretString, TenancyConfig,
};
use tasklerq::domain::error::DomainError;
use tasklerq::domain::models::UserRole;
use tasklerq::domain::settings::DEFAULT_TIMEZONE;
use tasklerq::http::server;
use tasklerq::llm::OpenAiBackend;
use tasklerq::redaction;
use tasklerq::settings::{self, SettingSource, crypto::SecretCipher};
use tasklerq::storage;
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
    /// Прогнать smoke-набор `capture-v4` через один слот.
    ///
    /// Конфигурация слотов берётся из `settings` (консоль администратора),
    /// окружения `LLM_*` и умолчаний; база — из `DATABASE_URL`. Набор по
    /// умолчанию (`src/capture/prompts/smoke-v4.json`) встроен в бинарник: в
    /// образе Docker нет исходников `tests/`, а проверка идёт в контейнере.
    /// `--set PATH` — другой набор для разработки; набор с версией контракта,
    /// отличной от действующей, — ошибка до вызовов LLM.
    ///
    /// Каждая фраза уходит в бэкенд выбранного слота напрямую: без цепочки,
    /// перехода на резервный слот и предохранителя. Ожидание — до 20 секунд,
    /// судейство — по таймауту слота, поэтому верный, но медленный ответ
    /// попадает в отчёт с фактическим временем. Фразы идут по очереди.
    ///
    /// Проверка доступна и при `LLM_CAPTURE_ENABLED=false`, если запрошенный
    /// слот задан: так слот проверяют до включения разбора. Одновременно
    /// выполняется не более одной проверки на инсталляцию: второй запуск
    /// завершается ошибкой «проверка уже идёт» без обращения к LLM.
    ///
    /// `--titles strict|normalized` задаёт и сохраняет стандарт слота: строгий
    /// (форма — часть контракта) или с видом глагола и синонимами. Без флага
    /// действует сохранённый стандарт. Второй режим печатается справочно по
    /// тому же прогону; вердикт, код возврата и запись — по стандарту.
    ///
    /// Коды возврата: 0 — все фразы пройдены; 1 — слот не прошёл проверку;
    /// 2 — ошибка конфигурации, слота или набора (версия контракта проверяется
    /// до вызовов LLM), недоступная база или идущая проверка; результат в
    /// `llm_slot_checks` не записан.
    CaptureEval(CaptureEvalArgs),
    /// Отрисовать запросы разбора `capture-v4` из JSONL в JSONL.
    ///
    /// Вход (stdin), одна запись на строку:
    /// {"id": "строка или число", "context": {...}, "text": "фраза"}.
    ///
    /// context: now — момент RFC 3339 с поясом; timezone — пояс IANA
    /// household; lists — массив {"name", "kind":
    /// "tasks|shopping|checklist|events", "default": bool}; members — массив
    /// имён; author — имя или null. Идентификаторы не нужны: в запрос попадают
    /// только имена. lists и members можно не задавать — это пустые перечни;
    /// author обязан быть среди members.
    ///
    /// Неизвестные поля и опечатки в именах полей — ошибка строки, а не другой
    /// запрос. Пустые строки (из пробелов) пропускаются, нумерация строк при
    /// этом сохраняется.
    ///
    /// Выход (stdout), одна запись на строку: {"id", "messages", "schema",
    /// "prompt_version", "contract_hash"} — сообщения и схема в том же виде, в
    /// каком они уходят в тело chat/completions. База, сеть и LLM не
    /// используются.
    ///
    /// При первой ошибочной строке команда останавливается: сообщение с
    /// номером строки и id пишется в stderr, код возврата ненулевой, уже
    /// выведенные строки остаются в stdout.
    CaptureRender,
}

#[derive(Debug, Args)]
struct CaptureEvalArgs {
    /// Слот для проверки.
    #[arg(long, value_enum)]
    slot: SlotArg,
    /// Файл smoke-набора; без значения — встроенный набор `capture-v4`.
    #[arg(long, value_name = "PATH")]
    set: Option<PathBuf>,
    /// Сравнение названий: `strict` — только основы слов, `normalized` —
    /// ещё вид глагола и синонимы («Вынести мусор» ≈ «Выносить мусор»).
    ///
    /// Флаг задаёт и сохраняет стандарт слота: режим входит в отпечаток
    /// проверки, а смена стандарта делает прежнюю проверку устаревшей. Без
    /// флага действует сохранённый стандарт (`strict`, если его ещё не
    /// задавали). Второй режим печатается справочно по тому же прогону, без
    /// повторных вызовов слота.
    #[arg(long, value_enum)]
    titles: Option<TitleArg>,
}

/// Режим сравнения названий для `capture-eval`.
#[derive(Debug, Clone, Copy, Default, ValueEnum)]
enum TitleArg {
    /// Только основы слов.
    #[default]
    Strict,
    /// Основы, вид глагола и синонимы.
    Normalized,
}

impl TitleArg {
    fn mode(self) -> TitleMatch {
        match self {
            Self::Strict => TitleMatch::Strict,
            Self::Normalized => TitleMatch::Normalized,
        }
    }
}

/// Слот, который проверяет `capture-eval`.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum SlotArg {
    /// Основной слот.
    Primary,
    /// Резервный слот.
    Fallback,
}

impl SlotArg {
    /// Имя слота для отчёта и `llm_slot_checks`.
    fn name(self) -> &'static str {
        match self {
            Self::Primary => tasklerq::llm::PRIMARY_SLOT,
            Self::Fallback => tasklerq::llm::FALLBACK_SLOT,
        }
    }
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
    /// Ключ шифрования секретных настроек.
    SecretKey {
        #[command(subcommand)]
        command: SecretKeyCommand,
    },
    /// Выпустить setup-код первого входа в веб-консоль.
    SetupCode,
    /// Выпустить ссылку восстановления пароля администратора.
    ResetPassword,
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
    SecretKey(SecretKeyCommand),
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
    /// Household; в `single` можно опустить, если household единственный;
    /// в `multi` обязателен.
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
        /// Household; в `single` можно опустить, если household единственный;
        /// в `multi` обязателен.
        #[arg(long)]
        household: Option<Uuid>,
    },
}

#[derive(Debug, Args)]
struct TokenCreate {
    /// Household; в `single` можно опустить, если household единственный;
    /// в `multi` обязателен.
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
    /// Создать household и списки «Задачи», «Покупки» и «Календарь».
    ///
    /// В режиме `single` второй активный household отклоняется с `409`. С
    /// `--owner-telegram-id` владелец создаётся в той же транзакции; без него
    /// печатается подсказка, как добавить владельца через `admin user add`.
    Create {
        /// Название household.
        #[arg(long)]
        name: String,
        /// Часовой пояс IANA.
        #[arg(long, default_value = DEFAULT_TIMEZONE)]
        timezone: String,
        /// Telegram ID владельца (положительное целое).
        #[arg(long, value_name = "ID", value_parser = clap::value_parser!(i64).range(1..))]
        owner_telegram_id: Option<i64>,
        /// Отображаемое имя владельца; требует --owner-telegram-id.
        #[arg(long, requires = "owner_telegram_id")]
        owner_name: Option<String>,
    },
    /// Показать активные household без секретов.
    List,
    /// Мягко удалить household и отозвать его токены и приглашения.
    ///
    /// Household и все активные участники помечаются удалёнными; токены и
    /// приглашения отзываются сразу. Данные физически удалит retention.
    /// Без `--yes` команда просит ввести название household; в
    /// неинтерактивном режиме без `--yes` завершается ошибкой и ничего не
    /// меняет. Уведомления участникам не отправляются.
    Delete {
        /// Идентификатор household.
        #[arg(long)]
        household: Uuid,
        /// Не спрашивать подтверждение названием.
        #[arg(long)]
        yes: bool,
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
        /// Household; в `single` можно опустить, если household единственный;
        /// в `multi` обязателен.
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

#[derive(Debug, Subcommand)]
enum SecretKeyCommand {
    /// Перешифровать секреты прежним ключом на текущий в одной транзакции.
    ///
    /// Текущий ключ — `TASKLERQ_SECRET_KEY`, прежний — `TASKLERQ_SECRET_KEY_OLD`.
    /// Строки, уже зашифрованные текущим ключом, не меняются. Если хоть одна
    /// строка не расшифровывается ни одним ключом, ничего не меняется.
    Rotate,
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
        Ok(code) => code,
        Err(error) => {
            let message = redaction::sanitize(&format!("{error:#}"));
            eprintln!("tasklerq: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<ExitCode> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            serve().await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Migrate => {
            migrate().await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Seed => {
            seed().await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Admin { command } => {
            run_admin(command).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Healthcheck { url } => {
            healthcheck(&url).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::CaptureEval(args) => capture_eval(args).await,
        Command::CaptureRender => {
            capture_render()?;
            Ok(ExitCode::SUCCESS)
        }
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

    let listeners = server::bind_listeners(&config)
        .await
        .context("failed to bind http listeners")?;
    let address = listeners
        .public
        .local_addr()
        .context("failed to read the bound address")?;
    let admin_address = match listeners.admin.as_ref() {
        Some(listener) => Some(
            listener
                .local_addr()
                .context("failed to read the bound admin console address")?,
        ),
        None => None,
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let setup_pool = pool.clone();
    let application = build_application(&config, pool, shutdown_rx)
        .await
        .context("telegram startup failed")?;
    let telegram_mode = application.telegram_mode;
    let telegram_source = application.telegram_source;
    let workers = application.workers;
    let app = application.router;
    let admin = listeners.admin.zip(application.admin_router);
    tracing::info!(
        mode = telegram_mode.as_str(),
        source = telegram_source.map_or("none", SettingSource::as_str),
        "telegram configured"
    );

    tracing::info!(%address, "http server listening");
    match admin_address {
        Some(address) => {
            tracing::info!(%address, "admin console listening");
            admin_console::codes::announce_setup_code(
                &setup_pool,
                &config,
                Some(address.port()),
                Utc::now(),
            )
            .await;
        }
        None => tracing::info!("admin console is disabled"),
    }

    server::serve_listeners(listeners.public, app, admin, async move {
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

async fn setup_code() -> Result<()> {
    init_cli_tracing();

    let address = admin_console_address();
    let database = DatabaseConfig::from_env().context("invalid database configuration")?;
    let pool = storage::db::connect(&database)
        .await
        .context("database connection failed")?;

    match admin_console::codes::issue_setup_code(&pool, Utc::now())
        .await
        .map_err(explain)?
    {
        Some(issued) => {
            println!("{}", issued.code);
            match address {
                ConsoleAddress::Port(port) => eprintln!(
                    "Откройте http://<адрес хоста>:{port}/admin/setup и введите код. \
                     Повторно код не показывается."
                ),
                ConsoleAddress::Disabled => eprintln!(
                    "Консоль выключена (ADMIN_BIND_ADDR=disabled): включите её и выпустите \
                     код заново. Повторно код не показывается."
                ),
                ConsoleAddress::Invalid => eprintln!(
                    "ADMIN_BIND_ADDR не разобран: откройте /admin/setup на хосте, где слушает \
                     консоль. Повторно код не показывается."
                ),
            }
            Ok(())
        }
        None => bail!(
            "учётная запись администратора уже существует; \
             сменить пароль: tasklerq admin reset-password"
        ),
    }
}

/// Выпустить ссылку восстановления и напечатать её путь.
///
/// В базе остаётся только хеш кода; открытое значение печатается один раз.
/// Полный адрес сервер не знает, поэтому путь выводится отдельно, а порт
/// консоли подсказывается из `ADMIN_BIND_ADDR`.
async fn reset_password() -> Result<()> {
    init_cli_tracing();

    let address = admin_console_address();
    let database = DatabaseConfig::from_env().context("invalid database configuration")?;
    let pool = storage::db::connect(&database)
        .await
        .context("database connection failed")?;

    let now = Utc::now();
    let Some(issued) = admin_console::codes::issue_recovery_code(&pool, now)
        .await
        .map_err(explain)?
    else {
        bail!(
            "учётная запись администратора не создана; \
             выпустите setup-код: tasklerq admin setup-code"
        );
    };

    // Код в журнал и аудит не попадает: событие фиксирует только выпуск.
    tasklerq::security_audit::Audit::new(pool.clone())
        .admin_recovery_issued(now)
        .await;

    println!("/admin/recover/{}", issued.code);
    println!(
        "Срок действия: {} минут.",
        admin_console::codes::RECOVERY_TTL_MINUTES
    );
    match address {
        ConsoleAddress::Port(port) => eprintln!(
            "Откройте http://<адрес хоста>:{port}/admin/recover/{} в течение {} минут. \
             Повторно ссылка не показывается.",
            issued.code,
            admin_console::codes::RECOVERY_TTL_MINUTES
        ),
        ConsoleAddress::Disabled => eprintln!(
            "Консоль выключена (ADMIN_BIND_ADDR=disabled): ссылка не откроется, пока \
             консоль не включена. Повторно ссылка не показывается."
        ),
        ConsoleAddress::Invalid => eprintln!(
            "ADMIN_BIND_ADDR не разобран: откройте /admin/recover/{} на хосте, где слушает \
             консоль, в течение {} минут. Повторно ссылка не показывается.",
            issued.code,
            admin_console::codes::RECOVERY_TTL_MINUTES
        ),
    }

    Ok(())
}

/// Куда CLI подсказывает открыть страницу консоли.
enum ConsoleAddress {
    /// Консоль слушает этот порт.
    Port(u16),
    /// `ADMIN_BIND_ADDR=disabled`.
    Disabled,
    /// Значение не разобралось: `serve` с ним не запустится, но команда
    /// выпуска кода работает.
    Invalid,
}

/// Адрес консоли из `ADMIN_BIND_ADDR` для подсказки в CLI.
///
/// Разбор общий с [`Config`]: невалидное значение не роняет команду, потому
/// что код или ссылка нужны и без работающего listener'а.
fn admin_console_address() -> ConsoleAddress {
    let raw = std::env::var("ADMIN_BIND_ADDR").ok();
    match config::parse_admin_bind_addr(raw.as_deref()) {
        Ok(Some(address)) => ConsoleAddress::Port(address.port()),
        Ok(None) => ConsoleAddress::Disabled,
        // В журнал не пишем: JSON-подписчик выводит в stdout, а команда печатает
        // код или ссылку первой строкой. Подсказка ниже и так сообщает о
        // невалидном значении.
        Err(_) => ConsoleAddress::Invalid,
    }
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
        // Webhook использует ту же действующую конфигурацию Telegram, что и
        // сервис: при токене в окружении база не нужна, иначе команда читает
        // настройки консоли тем же ключом шифрования.
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
        AdminCommand::SecretKey { command } => {
            run_admin_database(DatabaseCommand::SecretKey(command)).await
        }
        AdminCommand::SetupCode => setup_code().await,
        AdminCommand::ResetPassword => reset_password().await,
        AdminCommand::Export(args) => run_admin_database(DatabaseCommand::Export(args)).await,
    }
}

async fn run_admin_database(command: DatabaseCommand) -> Result<()> {
    let database = DatabaseConfig::from_env().context("invalid database configuration")?;
    let pool = storage::db::connect(&database)
        .await
        .context("database connection failed")?;
    let now = Utc::now();
    let tenancy = TenancyConfig::from_env().context("invalid tenancy configuration")?;

    match command {
        DatabaseCommand::Token(command) => match command {
            TokenCommand::Create(args) => {
                let issued = admin::create_token(
                    &pool,
                    tenancy.mode,
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
                for token in admin::list_tokens(&pool, tenancy.mode, household)
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
            HouseholdCommand::Create {
                name,
                timezone,
                owner_telegram_id,
                owner_name,
            } => match owner_telegram_id {
                Some(telegram_user_id) => {
                    let (household, owner) = admin::create_household_with_owner(
                        &pool,
                        tenancy.mode,
                        &name,
                        &timezone,
                        telegram_user_id,
                        owner_name.as_deref(),
                        now,
                    )
                    .await
                    .map_err(explain)?;
                    println!("id: {}", household.id);
                    println!("name: {}", household.name);
                    println!("timezone: {}", household.timezone);
                    println!("owner: {}", owner.telegram_user_id);
                }
                None => {
                    let household =
                        admin::create_household(&pool, tenancy.mode, &name, &timezone, now)
                            .await
                            .map_err(explain)?;
                    println!("id: {}", household.id);
                    println!("name: {}", household.name);
                    println!("timezone: {}", household.timezone);
                    println!(
                        "tasklerq admin user add --telegram-id <id> --role owner --household {}",
                        household.id
                    );
                }
            },
            HouseholdCommand::List => {
                for household in admin::list_households(&pool, now).await.map_err(explain)? {
                    let owner = household
                        .owner_telegram_id
                        .map_or_else(|| "-".to_owned(), |id| id.to_string());
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                        household.id,
                        household.name,
                        household.timezone,
                        owner,
                        household.members,
                        household.open_tasks,
                        household.active_tokens,
                        household.created_at.to_rfc3339()
                    );
                }
            }
            HouseholdCommand::Delete { household, yes } => {
                if !yes {
                    let name = admin::household_name(&pool, household)
                        .await
                        .map_err(explain)?;
                    require_name_confirmation(&name)?;
                }
                let deleted = admin::delete_household(&pool, household, now)
                    .await
                    .map_err(explain)?;
                println!("id: {}", household);
                println!("members: {}", deleted.members.len());
                println!("revoked_tokens: {}", deleted.revoked_tokens);
                println!("revoked_invites: {}", deleted.revoked_invites);
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
                    tenancy.mode,
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
        DatabaseCommand::SecretKey(command) => match command {
            SecretKeyCommand::Rotate => {
                let keys =
                    SecretKeyConfig::from_env().context("invalid secret key configuration")?;
                let cipher = SecretCipher::new(keys.current, keys.old);
                let report = settings::rotate_secret_key(&pool, &cipher).await?;
                println!(
                    "{} перешифровано, {} уже под текущим ключом",
                    report.rotated(),
                    report.already_current()
                );
            }
        },
        DatabaseCommand::Export(args) => {
            let export = admin::export(&pool, tenancy.mode, args.household, now)
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

/// Спросить подтверждение названием household. Без TTY опасная команда
/// выполняется только с `--yes`.
fn require_name_confirmation(expected: &str) -> Result<()> {
    use std::io::{IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        bail!("нужно подтверждение (--yes); ничего не изменено");
    }
    eprint!("Введите название household для подтверждения: ");
    std::io::stderr()
        .flush()
        .context("failed to write the confirmation prompt")?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("failed to read the confirmation")?;
    if tasklerq::domain::identity::confirmation_matches(expected, &answer) {
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

async fn telegram_bot_token(config: &Config) -> Result<SecretString> {
    // Окружение описывает группу целиком при токене: база не нужна.
    if let Some(group) = &config.telegram {
        return group
            .bot_token
            .clone()
            .context("TELEGRAM_BOT_TOKEN is not configured");
    }

    let keys = SecretKeyConfig::from_env().context("invalid secret key configuration")?;
    let cipher = SecretCipher::new(keys.current, keys.old);
    let database = DatabaseConfig::from_env().context("invalid database configuration")?;
    let pool = storage::db::connect(&database)
        .await
        .context("database connection failed")?;
    let effective = settings::read_effective_telegram(&pool, config, &cipher)
        .await
        .context("failed to read telegram settings from the database")?;
    let reason = effective
        .problem()
        .map_or("not_set", |problem| problem.code());

    effective
        .config()
        .and_then(|group| group.bot_token.clone())
        .with_context(|| {
            format!(
                "telegram bot is not configured ({reason}): set TELEGRAM_BOT_TOKEN or \
                 configure the bot in the admin console"
            )
        })
}

async fn webhook_info() -> Result<()> {
    let config = Config::from_env().context("invalid configuration")?;
    let token = telegram_bot_token(&config).await?;
    let base_url = config
        .telegram_api_base_url
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

/// Код возврата `capture-eval`, когда проверка не выполнена: конфигурация,
/// слот, набор или запись результата.
const CAPTURE_EVAL_ERROR_EXIT: u8 = 2;

/// Прогнать smoke-набор через слот.
///
/// Коды возврата: 0 — пройдены все фразы, 1 — слот не прошёл проверку,
/// 2 — проверка не выполнена (конфигурация, слот, набор, база) или уже идёт.
/// Ошибка конфигурации печатается здесь же, чтобы отличить её от провала слота.
async fn capture_eval(args: CaptureEvalArgs) -> Result<ExitCode> {
    init_cli_tracing();

    match run_capture_eval(args).await {
        Ok(report) => Ok(if report.passed() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }),
        Err(error) => {
            let message = redaction::sanitize(&format!("{error:#}"));
            eprintln!("tasklerq: {message}");
            Ok(ExitCode::from(CAPTURE_EVAL_ERROR_EXIT))
        }
    }
}

/// Подготовить слот, прогнать набор, напечатать отчёт и вернуть результат.
///
/// Отчёт печатается до проверки записи: если база откажет, результат проверки
/// всё равно виден, а код возврата останется «проверка не выполнена».
async fn run_capture_eval(args: CaptureEvalArgs) -> Result<tasklerq::capture::eval::Report> {
    let (pool, prepared) = prepare_check(&args).await?;
    let result = slot_check::run_check(&pool, prepared, args.titles.map(TitleArg::mode)).await?;
    print_check(&result);
    result.saved?;
    Ok(result.report)
}

/// Напечатать отчёт проверки до записи результата.
fn print_check(check: &slot_check::CheckResult) {
    println!("Слот: {} ({})", check.slot_name, check.model);
    println!("Отпечаток: {}", check.fingerprint.value);
    match &check.other {
        Some(other) => println!(
            "Названия: {} — {} из {} (стандарт), {} — {} из {} (справочно)",
            check.report.titles.as_str(),
            check.report.passed_count(),
            check.report.total(),
            check.other_titles.as_str(),
            other.passed_count(),
            other.total(),
        ),
        None => println!(
            "Названия: {} — {} из {} (стандарт)",
            check.report.titles.as_str(),
            check.report.passed_count(),
            check.report.total(),
        ),
    }
    print!("{}", check.report.to_text());
}

/// Проверить конфигурацию и прочитать набор до любых обращений к LLM.
///
/// Слот собирается через [`settings::read_effective_llm`] (база, окружение и
/// умолчания): иначе слот, заданный только в консоли, был бы для команды «не
/// настроен». Если настройки или база недоступны, действует слот из окружения и
/// умолчаний: проверка слота из `.env` работает и без базы, а настроенный слот
/// упрётся в недоступную базу до вызовов LLM.
async fn prepare_check(
    args: &CaptureEvalArgs,
) -> Result<(sqlx::PgPool, slot_check::PreparedCheck)> {
    let llm = LlmConfig::from_env().context("invalid llm configuration")?;
    let keys = SecretKeyConfig::from_env().context("invalid secret key configuration")?;
    let cipher = SecretCipher::new(keys.current, keys.old);
    let slot_name = args.slot.name();

    let database = DatabaseConfig::from_env().context("invalid database configuration")?;
    let (pool, connection_error) = match storage::db::connect(&database).await {
        Ok(pool) => (Some(pool), None),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "database connection failed; checking the slot from the environment"
            );
            (None, Some(error))
        }
    };
    let effective = match &pool {
        Some(pool) => match settings::read_effective_llm(pool, &llm, &cipher).await {
            Ok(effective) => effective,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "could not read llm settings; checking the slot from the environment"
                );
                settings::effective_llm(&llm, &[], &cipher)
            }
        },
        None => settings::effective_llm(&llm, &[], &cipher),
    };

    let effective_slot = match args.slot {
        SlotArg::Primary => effective.primary(),
        SlotArg::Fallback => effective.fallback(),
    };
    let slot = effective_slot.config().cloned().ok_or_else(|| {
        let prefix = slot_name.to_ascii_uppercase();
        let reason = effective_slot
            .problem()
            .map_or("not_set", |problem| problem.code());
        anyhow!(
            "слот {slot_name} не настроен ({reason}): задайте слот в консоли \
             администратора или LLM_{prefix}_URL и LLM_{prefix}_MODEL"
        )
    })?;

    let set_text = match &args.set {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("failed to read the smoke set {}", path.display()))?,
        None => slot_check::BUILTIN_SET.to_owned(),
    };
    let set = slot_check::load_set(&set_text)
        .map_err(anyhow::Error::msg)
        .context("invalid smoke set")?;

    let backend = OpenAiBackend::from_slot(&slot, effective.connect_timeout())
        .map_err(|error| anyhow!("invalid slot configuration: {error}"))?;

    // Слот, набор и бэкенд проверены; только теперь недоступная база делает
    // команду невозможной — до первого вызова LLM.
    let Some(pool) = pool else {
        return Err(anyhow::Error::new(
            connection_error.expect("a missing pool carries its connection error"),
        )
        .context("database connection failed"));
    };

    Ok((
        pool,
        slot_check::PreparedCheck {
            slot_name,
            slot,
            backend: Box::new(backend),
            set,
            set_text,
        },
    ))
}

/// Отрисовать запросы `capture-v4` для проекта дообучения.
///
/// Читает JSONL из stdin, пишет JSONL в stdout. База, сеть и LLM не нужны.
/// При первой ошибочной строке останавливается: уже выведенные строки
/// остаются, код возврата ненулевой.
fn capture_render() -> Result<()> {
    use std::io::{BufRead, BufWriter, Write};

    let stdin = std::io::stdin();
    let mut stdout = BufWriter::new(std::io::stdout().lock());

    for (index, line) in stdin.lock().lines().enumerate() {
        let number = index + 1;
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                stdout.flush().ok();
                return Err(error).context("failed to read stdin");
            }
        };
        // Пустая строка не запись: датасет может быть с пустыми строками между
        // блоками, а номер следующей ошибки должен остаться прежним.
        if line.trim().is_empty() {
            continue;
        }
        match tasklerq::capture::render::render_line(&line) {
            Ok(rendered) => {
                writeln!(stdout, "{rendered}").context("failed to write stdout")?;
            }
            Err(error) => {
                stdout.flush().ok();
                let id = error
                    .id
                    .map(|id| format!(" (id: {id})"))
                    .unwrap_or_default();
                bail!("строка {number}{id}: {}", error.message);
            }
        }
    }

    stdout.flush().context("failed to write stdout")?;
    Ok(())
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
