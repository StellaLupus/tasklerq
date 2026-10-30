//! Общая инфраструктура интеграционных тестов.

#![allow(dead_code)]

pub mod telegram;

use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tasklerq::auth::tokens::hash_token;
use tasklerq::domain::identity::NewMember;
use tasklerq::domain::models::{
    Actor, ItemPriority, ItemSource, NewItem, RecurrenceInput, SubtaskInput, UserRole,
};
use tasklerq::domain::recurrence::parse_timezone;
use tasklerq::domain::validation::validate_new_item;
use tasklerq::http::router::{AppState, build_router};
use tasklerq::storage;
use tasklerq::storage::repositories::households::{self, NewOwner};
use tasklerq::storage::repositories::{items, lists, tokens};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tokio::sync::OnceCell;
use tower::ServiceExt;
use uuid::Uuid;

static DATABASE: OnceCell<SharedDatabase> = OnceCell::const_new();
static NEXT_HOUSEHOLD: AtomicI64 = AtomicI64::new(1);
static NEXT_TELEGRAM: AtomicI64 = AtomicI64::new(1);
static CONTAINER_ID: OnceLock<String> = OnceLock::new();

/// Удалить контейнер при выходе процесса. Статик не получает `Drop`, поэтому
/// testcontainers сам контейнер не остановит, а Ryuk в Rust-крейте нет:
/// без этого каждый тестовый бинарник оставляет контейнер postgres работать.
extern "C" fn remove_container() {
    if let Some(id) = CONTAINER_ID.get() {
        let _ = Command::new("docker").args(["rm", "-f", "-v", id]).output();
    }
}

/// Общий контейнер и URL базы. Контейнер живёт до конца процесса, а пул
/// создаётся отдельно для каждого теста: соединения sqlx привязаны к runtime,
/// в котором они открыты, а у каждого `#[tokio::test]` свой runtime.
struct SharedDatabase {
    url: String,
    _container: ContainerAsync<GenericImage>,
}

/// Тестовая база для одного теста.
pub struct TestDb {
    /// Пул, привязанный к runtime текущего теста.
    pub pool: PgPool,
}

/// Получить пул тестовой базы для текущего теста.
pub async fn db() -> TestDb {
    let shared = DATABASE.get_or_init(start_db).await;
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&shared.url)
        .await
        .expect("database should accept connections");
    TestDb { pool }
}

/// Отдельная база с применёнными миграциями на том же сервере.
///
/// Нужна тестам, которые меняют схему (триггеры, DDL): такие операции берут
/// блокировки на таблицы и подвешивают параллельные тесты общей базы.
pub async fn isolated_db() -> TestDb {
    let shared = DATABASE.get_or_init(start_db).await;
    let name = format!("isolated_{}", Uuid::now_v7().simple());
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&shared.url)
        .await
        .expect("database should accept connections");
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .expect("isolated database should be created");
    admin.close().await;

    let url = shared.url.replace("/tasklerq?", &format!("/{name}?"));
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("isolated database should accept connections");
    storage::migrations::run(&pool)
        .await
        .expect("migrations should apply");
    TestDb { pool }
}

/// URL общей тестовой базы (для запуска бинарника `tasklerq`).
pub async fn database_url() -> String {
    DATABASE.get_or_init(start_db).await.url.clone()
}

/// Уникальный Telegram ID, не пересекающийся с household из `create_household`.
pub fn next_telegram_id() -> i64 {
    7_000_000_000 + NEXT_TELEGRAM.fetch_add(1, Ordering::Relaxed)
}

/// Новый участник Telegram с уникальным ID.
pub fn new_member(name: &str) -> tasklerq::domain::identity::NewMember {
    let id = next_telegram_id();
    tasklerq::domain::identity::NewMember {
        telegram_user_id: id,
        telegram_chat_id: id,
        display_name: name.to_owned(),
        username: None,
    }
}

async fn start_db() -> SharedDatabase {
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

    if CONTAINER_ID.set(container.id().to_owned()).is_ok() {
        // SAFETY: обработчик не паникует и только запускает `docker rm`.
        unsafe { libc::atexit(remove_container) };
    }

    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("postgres port should be mapped");
    let url = format!("postgres://tasklerq:tasklerq@127.0.0.1:{port}/tasklerq?sslmode=disable");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("database should accept connections");
    storage::migrations::run(&pool)
        .await
        .expect("migrations should apply");
    pool.close().await;

    SharedDatabase {
        url,
        _container: container,
    }
}

/// Household с owner, member и двумя списками по умолчанию.
#[derive(Debug, Clone, Copy)]
pub struct TestHousehold {
    pub household_id: Uuid,
    pub owner_id: Uuid,
    pub member_id: Uuid,
    pub tasks_list_id: Uuid,
    pub shopping_list_id: Uuid,
}

impl TestHousehold {
    /// Актор-владелец.
    pub fn owner(&self) -> Actor {
        Actor::user(self.owner_id, UserRole::Owner)
    }

    /// Актор-участник.
    pub fn member(&self) -> Actor {
        Actor::user(self.member_id, UserRole::Member)
    }
}

/// Создать изолированный household для теста.
pub async fn create_household(pool: &PgPool) -> TestHousehold {
    let sequence = NEXT_HOUSEHOLD.fetch_add(1, Ordering::Relaxed);
    let now = Utc::now();
    let mut conn = pool.acquire().await.expect("connection");

    let onboarded = households::onboard(
        &mut conn,
        &format!("Test household {sequence}"),
        "Europe/Moscow",
        NewOwner {
            member: NewMember {
                telegram_user_id: 8_000_000_000 + sequence,
                telegram_chat_id: 8_000_000_000 + sequence,
                display_name: format!("Owner {sequence}"),
                username: None,
            },
            timezone: Some("Europe/Moscow".into()),
        },
        now,
    )
    .await
    .expect("household should be created");
    let (household, owner) = (onboarded.household, onboarded.user);

    let member_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (
             id, household_id, telegram_user_id, telegram_chat_id, display_name,
             role, settings, created_at, updated_at
         )
         VALUES ($1, $2, $3, $3, $4, 'member', '{}'::jsonb, $5, $5)",
    )
    .bind(member_id)
    .bind(household.id)
    .bind(9_000_000_000 + sequence)
    .bind(format!("Member {sequence}"))
    .bind(now)
    .execute(&mut *conn)
    .await
    .expect("member should be created");

    let all_lists = lists::list(&mut conn, household.id)
        .await
        .expect("default lists");
    let tasks_list_id = all_lists
        .iter()
        .find(|list| list.kind == tasklerq::domain::models::ListKind::Tasks)
        .expect("tasks list")
        .id;
    let shopping_list_id = all_lists
        .iter()
        .find(|list| list.kind == tasklerq::domain::models::ListKind::Shopping)
        .expect("shopping list")
        .id;

    TestHousehold {
        household_id: household.id,
        owner_id: owner.id,
        member_id,
        tasks_list_id,
        shopping_list_id,
    }
}

/// Новый элемент с минимальными полями.
pub fn new_item(list_id: Uuid, title: &str) -> NewItem {
    NewItem {
        list_id,
        title: title.to_owned(),
        notes: None,
        priority: ItemPriority::Normal,
        due_at: None,
        due_date: None,
        assignee_user_id: None,
        source: ItemSource::Api,
        quantity: None,
        unit: None,
        tags: Vec::new(),
        subtasks: Vec::new(),
        recurrence: None,
    }
}

/// Создать элемент через доменную валидацию и репозиторий.
pub async fn create_item(
    pool: &PgPool,
    household: &TestHousehold,
    list_id: Uuid,
    actor: &Actor,
    mut item: NewItem,
) -> tasklerq::domain::models::Item {
    let mut conn = pool.acquire().await.expect("connection");
    let list = lists::get(&mut conn, household.household_id, list_id)
        .await
        .expect("list");
    let timezone = parse_timezone("Europe/Moscow").expect("timezone");
    let today = Utc::now().with_timezone(&timezone).date_naive();
    let assignee = item.assignee_user_id;
    let validated = validate_new_item(list.kind, "Europe/Moscow", item.clone(), today)
        .expect("item should validate");
    item.list_id = list_id;
    items::create(
        &mut conn,
        household.household_id,
        actor,
        list_id,
        validated,
        assignee,
        Utc::now(),
    )
    .await
    .expect("item should be created")
}

/// Выпустить тестовый API-токен.
pub async fn issue_token(pool: &PgPool, household_id: Uuid, scopes: &[&str]) -> String {
    let token = format!("tlq_test_{}", Uuid::now_v7());
    let scopes: Vec<String> = scopes.iter().map(|scope| (*scope).to_owned()).collect();
    let prefix: String = token.chars().take(12).collect();
    let mut conn = pool.acquire().await.expect("connection");
    tokens::create(
        &mut conn,
        household_id,
        "test token",
        &hash_token(&token),
        &prefix,
        &scopes,
        None,
        None,
        Utc::now(),
    )
    .await
    .expect("token should be created");
    token
}

/// Роутер с тестовым пулом.
pub fn app(pool: &PgPool) -> Router {
    build_router(AppState::new(pool.clone()))
}

/// Выполнить HTTP-запрос и вернуть статус и JSON.
pub async fn request(
    app: &Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let body = match body {
        Some(body) => {
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&body).expect("json body"))
        }
        None => Body::empty(),
    };
    let request = builder.body(body).expect("request should build");
    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("router should respond");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body should be readable")
        .to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

/// Собрать recurrence-вход.
pub fn recurrence(
    freq: tasklerq::domain::models::RecurrenceFreq,
    byweekday: Vec<i16>,
) -> RecurrenceInput {
    RecurrenceInput {
        freq,
        interval: Some(1),
        byweekday,
        until: None,
        timezone: Some("Europe/Moscow".into()),
        anchor_date: None,
        month_rule: None,
    }
}

/// Подзадача.
pub fn subtask(title: &str) -> SubtaskInput {
    SubtaskInput {
        title: title.to_owned(),
        status: None,
    }
}
