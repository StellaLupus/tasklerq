//! Нагрузочный тест webhook: 10 пользователей, 10 000 задач, 50 запросов/с.
//!
//! Тест помечен `#[ignore]`: обычный `make test` его не запускает. Запуск —
//! `make load` (release-сборка). Сервер, воркеры и outbox настоящие; Telegram
//! Bot API подменён wiremock, PostgreSQL — testcontainers.

mod support;

use std::collections::HashMap;
use std::time::{Duration as StdDuration, Instant};

use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tasklerq::app::build_application_with_update_limit;
use tasklerq::config::Config;
use tasklerq::http::server;
use tasklerq::metrics;
use tasklerq::telegram::bot::model::{Action, Screen};
use tasklerq::telegram::callbacks::{self, NewToken, Scope};
use tasklerq::telegram::queue;
use tasklerq::telegram::webhook::SECRET_HEADER;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::{MissedTickBehavior, sleep};
use uuid::Uuid;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use support::TestHousehold;

const SECRET: &str = "load-test-webhook-secret";
const USERS: usize = 10;
const ITEMS: i64 = 10_000;
const REQUESTS_PER_SECOND: u64 = 50;
const LOAD_SECONDS: u64 = 60;
/// Допуск роста очереди между первой и последней третью.
const QUEUE_GROWTH_TOLERANCE: i64 = 10;
/// Сколько ждём опустошения очереди после последнего запроса.
const DRAIN_TIMEOUT: StdDuration = StdDuration::from_secs(10);
/// Сколько ждём, пока метрики догонят уже обработанные обновления: счётчик
/// увеличивается после коммита и ответа, то есть может отставать от пустой
/// очереди.
const METRIC_SETTLE_TIMEOUT: StdDuration = StdDuration::from_secs(5);
/// p99 webhook-ответа без LLM.
const P99_LIMIT: StdDuration = StdDuration::from_millis(100);

#[derive(Debug, Clone, Copy)]
struct LoadUser {
    id: Uuid,
    telegram_user_id: i64,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    at: StdDuration,
    pending: i64,
    oldest_age_ms: Option<i64>,
}

/// Источник обновлений: чередует текст и callback-нажатия между пользователями.
struct Updates {
    users: Vec<LoadUser>,
    callbacks: HashMap<i64, Vec<String>>,
    next_callback: HashMap<i64, usize>,
    sequence: u64,
    update_id: i64,
}

impl Updates {
    fn new(users: Vec<LoadUser>, callbacks: HashMap<i64, Vec<String>>, base: i64) -> Self {
        let next_callback = callbacks.keys().map(|chat| (*chat, 0)).collect();
        Self {
            users,
            callbacks,
            next_callback,
            sequence: 0,
            update_id: base,
        }
    }

    fn next(&mut self) -> Value {
        let index = usize::try_from(self.sequence).unwrap_or(0) % self.users.len();
        let user = self.users[index];
        let step = self.sequence % 8;
        self.sequence += 1;
        self.update_id += 1;
        let id = self.update_id;

        match step {
            0 | 6 => text_update(id, user.telegram_user_id, "/menu"),
            1 => text_update(id, user.telegram_user_id, "Купить молоко"),
            4 => text_update(id, user.telegram_user_id, "Позвонить маме"),
            2 | 3 | 5 | 7 => {
                let data = self.next_callback(user.telegram_user_id);
                callback_update(id, user.telegram_user_id, &data, 1)
            }
            _ => unreachable!("step is modulo 8"),
        }
    }

    fn next_callback(&mut self, chat_id: i64) -> String {
        let list = self.callbacks.get(&chat_id).expect("callback tokens");
        let index = self.next_callback.entry(chat_id).or_default();
        let data = list[*index % list.len()].clone();
        *index += 1;
        data
    }
}

fn text_update(id: i64, chat_id: i64, text: &str) -> Value {
    json!({
        "update_id": id,
        "message": {
            "message_id": id,
            "date": 1_700_000_000,
            "chat": {"id": chat_id, "type": "private", "first_name": "Нагрузка"},
            "from": {"id": chat_id, "is_bot": false, "first_name": "Нагрузка"},
            "text": text
        }
    })
}

fn callback_update(id: i64, chat_id: i64, data: &str, message_id: i64) -> Value {
    json!({
        "update_id": id,
        "callback_query": {
            "id": format!("cb{id}"),
            "chat_instance": "load",
            "from": {"id": chat_id, "is_bot": false, "first_name": "Нагрузка"},
            "data": data,
            "message": {
                "message_id": message_id,
                "date": 1_700_000_000,
                "chat": {"id": chat_id, "type": "private", "first_name": "Нагрузка"},
                "text": "экран"
            }
        }
    })
}

async fn seed_users(pool: &PgPool, household: &TestHousehold) -> Vec<LoadUser> {
    let now = Utc::now();
    // owner и member уже созданы стендом; добавляем остальных участников.
    for index in 0..(USERS - 2) {
        let telegram_user_id = 10_000_000_000 + i64::try_from(index).expect("index");
        sqlx::query(
            "INSERT INTO users (
                 id, household_id, telegram_user_id, telegram_chat_id, display_name,
                 role, settings, created_at, updated_at
             )
             VALUES ($1, $2, $3, $3, $4, 'member', '{}'::jsonb, $5, $5)",
        )
        .bind(Uuid::now_v7())
        .bind(household.household_id)
        .bind(telegram_user_id)
        .bind(format!("Участник {}", index + 3))
        .bind(now)
        .execute(pool)
        .await
        .expect("load user");
    }

    let rows: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT id, telegram_user_id FROM users
         WHERE household_id = $1 AND deleted_at IS NULL
         ORDER BY telegram_user_id",
    )
    .bind(household.household_id)
    .fetch_all(pool)
    .await
    .expect("load users");

    assert_eq!(rows.len(), USERS);
    rows.into_iter()
        .map(|(id, telegram_user_id)| LoadUser {
            id,
            telegram_user_id,
        })
        .collect()
}

async fn seed_items(pool: &PgPool, household: &TestHousehold, users: &[LoadUser]) {
    let user_ids: Vec<Uuid> = users.iter().map(|user| user.id).collect();
    let batch = 1_000_i64;
    for start in (0..ITEMS).step_by(usize::try_from(batch).expect("batch")) {
        let end = (start + batch - 1).min(ITEMS - 1);
        sqlx::query(
            "INSERT INTO items (
                 id, household_id, list_id, title, status, priority, due_at, due_date,
                 assignee_user_id, created_by_user_id, source, completed_at,
                 completed_by_user_id, quantity, unit, position, created_at, updated_at
             )
             SELECT gen_random_uuid(), $1,
                    CASE WHEN g % 10 < 7 THEN $2::uuid ELSE $3::uuid END,
                    'Задача ' || g,
                    CASE WHEN g % 3 = 0 THEN 'done'::item_status ELSE 'open'::item_status END,
                    (ARRAY['low', 'normal', 'high', 'urgent']::item_priority[])[(g % 4) + 1],
                    CASE WHEN g % 4 <> 0 AND g % 7 = 0
                         THEN now() + make_interval(hours => g % 24) ELSE NULL END,
                    CASE WHEN g % 4 = 0 THEN current_date + (g % 30) ELSE NULL END,
                    ($4::uuid[])[(g % 10) + 1],
                    $5,
                    'api',
                    CASE WHEN g % 3 = 0 THEN now() - interval '1 day' ELSE NULL END,
                    CASE WHEN g % 3 = 0 THEN ($4::uuid[])[(g % 10) + 1] ELSE NULL END,
                    CASE WHEN g % 10 >= 7 AND g % 2 = 0
                         THEN (1 + (g % 5))::numeric(7, 3) ELSE NULL END,
                    CASE WHEN g % 10 >= 7 AND g % 2 = 0 THEN 'шт' ELSE NULL END,
                    g,
                    now() - make_interval(days => g % 365),
                    now() - make_interval(days => g % 365)
             FROM generate_series($6::int, $7::int) AS g",
        )
        .bind(household.household_id)
        .bind(household.tasks_list_id)
        .bind(household.shopping_list_id)
        .bind(&user_ids)
        .bind(household.owner_id)
        .bind(i32::try_from(start).expect("start"))
        .bind(i32::try_from(end).expect("end"))
        .execute(pool)
        .await
        .expect("load items");
    }

    let now = Utc::now();
    for index in 0..5 {
        sqlx::query(
            "INSERT INTO tags (id, household_id, name, created_at)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(Uuid::now_v7())
        .bind(household.household_id)
        .bind(format!("tag-{index}"))
        .bind(now)
        .execute(pool)
        .await
        .expect("load tag");
    }
    sqlx::query(
        "INSERT INTO item_tags (item_id, tag_id)
         SELECT i.id, t.id
         FROM items i
         JOIN tags t
           ON t.household_id = i.household_id
          AND t.name = 'tag-' || (i.position % 5)
         WHERE i.household_id = $1 AND i.position % 3 = 0",
    )
    .bind(household.household_id)
    .execute(pool)
    .await
    .expect("load item tags");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM items WHERE household_id = $1")
        .bind(household.household_id)
        .fetch_one(pool)
        .await
        .expect("item count");
    assert_eq!(count, ITEMS, "load test needs exactly {ITEMS} items");
}

async fn issue_callbacks(
    pool: &PgPool,
    household: &TestHousehold,
    users: &[LoadUser],
) -> HashMap<i64, Vec<String>> {
    let item_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM items
         WHERE household_id = $1 AND list_id = $2 AND status = 'open'
         ORDER BY position
         LIMIT 20",
    )
    .bind(household.household_id)
    .bind(household.tasks_list_id)
    .fetch_all(pool)
    .await
    .expect("load item ids");
    assert!(!item_ids.is_empty(), "callbacks need items");

    let mut payloads: Vec<(&'static str, Value)> = vec![
        (
            "go",
            serde_json::to_value(Action::Go {
                to: Screen::List {
                    list_id: household.tasks_list_id,
                    page: 0,
                    edit: false,
                    from_lists: false,
                },
            })
            .expect("action"),
        ),
        (
            "go",
            serde_json::to_value(Action::Go {
                to: Screen::List {
                    list_id: household.shopping_list_id,
                    page: 0,
                    edit: false,
                    from_lists: false,
                },
            })
            .expect("action"),
        ),
        (
            "go",
            serde_json::to_value(Action::Go {
                to: Screen::Mine { page: 0 },
            })
            .expect("action"),
        ),
        (
            "search_start",
            serde_json::to_value(Action::SearchStart).expect("action"),
        ),
    ];
    for item_id in item_ids {
        payloads.push((
            "go",
            serde_json::to_value(Action::Go {
                to: Screen::Item {
                    item_id,
                    back: Box::new(Screen::List {
                        list_id: household.tasks_list_id,
                        page: 0,
                        edit: false,
                        from_lists: false,
                    }),
                },
            })
            .expect("action"),
        ));
    }

    let now = Utc::now();
    let mut conn = pool.acquire().await.expect("connection");
    let mut issued = HashMap::new();
    for user in users {
        let specs: Vec<NewToken> = (0..100)
            .map(|index| {
                let (action, payload) = &payloads[index % payloads.len()];
                NewToken {
                    telegram_user_id: user.telegram_user_id,
                    household_id: Some(household.household_id),
                    scope: Scope::Screen,
                    action: (*action).to_owned(),
                    payload: payload.clone(),
                    version: None,
                }
            })
            .collect();
        let tokens = callbacks::issue_many(&mut conn, &specs, now)
            .await
            .expect("callback tokens");
        issued.insert(user.telegram_user_id, tokens);
    }
    issued
}

async fn sample_queue(pool: &PgPool, started: Instant) -> Sample {
    let mut conn = pool.acquire().await.expect("connection");
    let now = Utc::now();
    let counts = queue::counts(&mut conn).await.expect("queue counts");
    let oldest = queue::oldest_ready(&mut conn, now)
        .await
        .expect("oldest ready");
    Sample {
        at: started.elapsed(),
        pending: counts.pending,
        oldest_age_ms: oldest.map(|at| (now - at).num_milliseconds()),
    }
}

fn metric_counter(text: &str, status: &str) -> u64 {
    let prefix = format!("telegram_updates_total{{status=\"{status}\"}} ");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

/// Сколько обновлений учтено метриками после `before` по терминальным исходам.
///
/// `deferred` — не терминальный исход: отложенное обновление позже станет
/// `processed` или `dropped`, поэтому ожидание дренажа его не считает.
fn handled_since(before: &str, after: &str) -> u64 {
    ["processed", "dropped", "failed", "dead"]
        .iter()
        .map(|status| metric_counter(after, status).saturating_sub(metric_counter(before, status)))
        .sum()
}

/// Дождаться, пока метрики насчитают все отправленные обновления.
///
/// Счётчик `telegram_updates_total` увеличивается после коммита и ответа,
/// поэтому пустая очередь ещё не означает, что метрики готовы.
async fn wait_for_metrics(before: &str, sent: u64) -> String {
    let started = Instant::now();
    loop {
        let after = metrics::render();
        if handled_since(before, &after) >= sent || started.elapsed() >= METRIC_SETTLE_TIMEOUT {
            return after;
        }
        sleep(StdDuration::from_millis(50)).await;
    }
}

fn percentile(sorted: &[StdDuration], fraction: f64) -> StdDuration {
    if sorted.is_empty() {
        return StdDuration::ZERO;
    }
    let rank = (sorted.len() as f64 * fraction).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn millis(value: StdDuration) -> f64 {
    value.as_secs_f64() * 1_000.0
}

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    update_base: i64,
    update_limit: Option<u32>,
    all_processed: bool,
}

struct ScenarioResult {
    name: &'static str,
    processed: u64,
    deferred: u64,
    dropped: u64,
    p50: StdDuration,
    p95: StdDuration,
    p99: StdDuration,
    max: StdDuration,
    drain: StdDuration,
    /// Скорость обработки воркерами. Прогон открытый: поток 50 запросов/с,
    /// поэтому величина упирается в него и не показывает запас воркера.
    processed_rate: f64,
}

/// Пропускает в stderr только строки о неудачных попытках воркера: остальной
/// журнал под нагрузкой (тысячи предупреждений о лимите) только мешает.
struct FailureLogs;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FailureLogs {
    type Writer = FailureLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        FailureLogWriter
    }
}

struct FailureLogWriter;

impl std::io::Write for FailureLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        const MARKERS: [&str; 4] = [
            "telegram update failed",
            "telegram update handler panicked",
            "telegram update pass failed",
            "rollback failed",
        ];
        let text = String::from_utf8_lossy(buf);
        if MARKERS.iter().any(|marker| text.contains(marker)) {
            eprint!("{text}");
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn load_config(database_url: &str, telegram_url: &str, public_url: &str) -> Config {
    Config::from_map(vec![
        ("ENV".to_owned(), "development".to_owned()),
        ("PUBLIC_BASE_URL".to_owned(), public_url.to_owned()),
        ("DATABASE_URL".to_owned(), database_url.to_owned()),
        ("DATABASE_MAX_CONNECTIONS".to_owned(), "10".to_owned()),
        ("TELEGRAM_MODE".to_owned(), "webhook".to_owned()),
        ("TELEGRAM_BOT_TOKEN".to_owned(), "123:load-test".to_owned()),
        ("TELEGRAM_WEBHOOK_SECRET".to_owned(), SECRET.to_owned()),
        (
            "TELEGRAM_WEBHOOK_URL".to_owned(),
            format!("{public_url}/webhooks/telegram"),
        ),
        ("TELEGRAM_API_BASE_URL".to_owned(), telegram_url.to_owned()),
        ("LLM_CAPTURE_ENABLED".to_owned(), "false".to_owned()),
        ("CAPTURE_API_ENABLED".to_owned(), "false".to_owned()),
    ])
    .expect("load config")
}

async fn run_scenario(
    scenario: Scenario,
    pool: &PgPool,
    database_url: &str,
    telegram_url: &str,
    household: &TestHousehold,
    users: &[LoadUser],
) -> ScenarioResult {
    let callbacks = issue_callbacks(pool, household, users).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let public_url = format!("http://{address}");
    let config = load_config(database_url, telegram_url, &public_url);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application_with_update_limit(
        &config,
        pool.clone(),
        shutdown_rx.clone(),
        scenario.update_limit,
    )
    .await
    .expect("application");
    let router = application.router;
    let workers = application.workers;
    let mut server_shutdown = shutdown_rx;
    let server = tokio::spawn(server::serve(listener, router, async move {
        let _ = server_shutdown.changed().await;
    }));

    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(64)
        .build()
        .expect("http client");
    let url = format!("http://{address}/webhooks/telegram");
    let mut updates = Updates::new(users.to_vec(), callbacks, scenario.update_base);
    let before = metrics::render();
    let queue_before = {
        let mut conn = pool.acquire().await.expect("connection");
        queue::counts(&mut conn).await.expect("queue counts")
    };

    let started = Instant::now();
    let interval = StdDuration::from_micros(1_000_000 / REQUESTS_PER_SECOND);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Burst);
    let mut tasks = JoinSet::new();
    let mut latencies = Vec::new();
    let mut sent = 0_u64;

    // Очередь сэмплируется отдельной задачей: запрос к базе не должен
    // задерживать тикер, который отправляет webhook-запросы.
    let sample_pool = pool.clone();
    let (sample_stop_tx, mut sample_stop_rx) = watch::channel(false);
    let sampling = tokio::spawn(async move {
        let mut samples = Vec::new();
        let mut interval = tokio::time::interval(StdDuration::from_secs(5));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            tokio::select! {
                _ = interval.tick() => samples.push(sample_queue(&sample_pool, started).await),
                _ = sample_stop_rx.changed() => break,
            }
        }
        samples
    });

    while started.elapsed() < StdDuration::from_secs(LOAD_SECONDS) {
        ticker.tick().await;
        if started.elapsed() >= StdDuration::from_secs(LOAD_SECONDS) {
            break;
        }
        let body = serde_json::to_vec(&updates.next()).expect("update json");
        let client = client.clone();
        let url = url.clone();
        tasks.spawn(async move {
            let began = Instant::now();
            let response = client
                .post(&url)
                .header(SECRET_HEADER, SECRET)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .expect("webhook response");
            let status = response.status();
            let _ = response.bytes().await.expect("webhook body");
            (status, began.elapsed())
        });
        sent += 1;
    }

    while let Some(result) = tasks.join_next().await {
        let (status, latency) = result.expect("request task");
        assert_eq!(status.as_u16(), 200, "every webhook must return 200");
        latencies.push(latency);
    }

    let drain_started = Instant::now();
    let counts = loop {
        let counts = {
            let mut conn = pool.acquire().await.expect("connection");
            queue::counts(&mut conn).await.expect("queue counts")
        };
        if counts.pending == 0 || drain_started.elapsed() >= DRAIN_TIMEOUT {
            break counts;
        }
        sleep(StdDuration::from_millis(100)).await;
    };
    let drain = drain_started.elapsed();
    // Воркеры успевают разгребать очередь: скорость — обработанные записи за
    // всё время прогона, включая drain.
    let worker_seconds = started.elapsed().as_secs_f64();
    let processed_rows = counts.processed - queue_before.processed;
    let processed_rate = processed_rows as f64 / worker_seconds;

    let _ = sample_stop_tx.send(true);
    let mut samples = tokio::time::timeout(StdDuration::from_secs(5), sampling)
        .await
        .expect("sampling task stops")
        .expect("sampling task");
    samples.push(sample_queue(pool, started).await);

    let after = wait_for_metrics(&before, sent).await;
    let processed = metric_counter(&after, "processed") - metric_counter(&before, "processed");
    let deferred = metric_counter(&after, "deferred") - metric_counter(&before, "deferred");
    let dropped = metric_counter(&after, "dropped") - metric_counter(&before, "dropped");
    let failed = metric_counter(&after, "failed") - metric_counter(&before, "failed");
    let dead = metric_counter(&after, "dead") - metric_counter(&before, "dead");

    // `last_error` стирается при успешном повторе, поэтому сохраняем и сам
    // факт попытки: по `attempts` видно, какое обновление упало, а текст
    // ошибки приходит из журнала `FailureLogs`.
    if failed > 0 {
        let retried: Vec<(i64, String, i32, Option<String>)> = sqlx::query_as(
            "SELECT update_id, status::text, attempts, last_error
             FROM telegram_updates
             WHERE attempts > 0
             ORDER BY update_id",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        println!("updates with failed attempts: {retried:?}");
    }

    latencies.sort();
    let p50 = percentile(&latencies, 0.50);
    let p95 = percentile(&latencies, 0.95);
    let p99 = percentile(&latencies, 0.99);
    let max = latencies.last().copied().unwrap_or_default();

    println!(
        "load scenario '{}': users={USERS} items={ITEMS} rate={REQUESTS_PER_SECOND}/s duration={LOAD_SECONDS}s update_limit={:?}",
        scenario.name, scenario.update_limit
    );
    println!(
        "webhook latency ms: p50={:.3} p95={:.3} p99={:.3} max={:.3}",
        millis(p50),
        millis(p95),
        millis(p99),
        millis(max)
    );
    println!(
        "requests={sent} processed={processed} deferred={deferred} dropped={dropped} \
         failed={failed} dead={dead}"
    );
    println!(
        "worker: rows={processed_rows} elapsed={:.3}s processed_rate={:.1}/s drain={:.3}s",
        worker_seconds,
        processed_rate,
        drain.as_secs_f64()
    );
    println!(
        "queue samples (at, pending, oldest_age_ms): {:?}",
        samples
            .iter()
            .map(|sample| (
                sample.at.as_secs_f64(),
                sample.pending,
                sample.oldest_age_ms
            ))
            .collect::<Vec<_>>()
    );
    println!(
        "final queue: pending={} processed={} dead={}",
        counts.pending, counts.processed, counts.dead
    );

    assert!(
        sent >= REQUESTS_PER_SECOND * LOAD_SECONDS - 100,
        "the load loop must send the planned number of requests: {sent}"
    );
    if scenario.all_processed {
        assert_eq!(
            processed, sent,
            "every update must go through the handler when the user limit is raised"
        );
        assert_eq!(
            deferred, 0,
            "no update may be deferred when the user limit is raised"
        );
        assert_eq!(
            dropped, 0,
            "no update may be dropped when the user limit is raised"
        );
    } else {
        assert_eq!(
            processed + dropped,
            sent,
            "every update ends as processed or dropped"
        );
        assert!(
            dropped > 0,
            "the deferred queue limit must be exercised under load"
        );
    }
    assert_eq!(failed, 0, "no update may fail under load");
    assert_eq!(dead, 0, "no update may reach the dead-letter queue");
    assert!(
        p99 < P99_LIMIT,
        "p99 webhook latency is {:.3} ms, expected < {} ms",
        millis(p99),
        millis(P99_LIMIT)
    );

    let third = samples.len() / 3;
    assert!(third > 0, "need at least three queue samples");
    let first_max = samples[..third]
        .iter()
        .map(|sample| sample.pending)
        .max()
        .unwrap_or(0);
    let last_max = samples[samples.len() - third..]
        .iter()
        .map(|sample| sample.pending)
        .max()
        .unwrap_or(0);
    assert!(
        last_max <= first_max + QUEUE_GROWTH_TOLERANCE,
        "telegram queue grows: first third max {first_max}, last third max {last_max}, tolerance {QUEUE_GROWTH_TOLERANCE}; samples {samples:?}"
    );
    assert_eq!(
        counts.pending, 0,
        "queue must drain within {DRAIN_TIMEOUT:?}"
    );
    assert_eq!(counts.dead, 0, "dead-letter queue must stay empty");

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(StdDuration::from_secs(5), server).await;
    for worker in workers {
        worker.abort();
    }

    ScenarioResult {
        name: scenario.name,
        processed,
        deferred,
        dropped,
        p50,
        p95,
        p99,
        max,
        drain,
        processed_rate,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "load test: run with `make load`"]
async fn webhook_keeps_latency_and_queue_under_load() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("tasklerq=warn")),
        )
        .with_writer(FailureLogs)
        .with_ansi(false)
        .try_init();
    let db = support::isolated_db().await;
    let database_name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&db.pool)
        .await
        .expect("database name");
    let database_url = support::database_url_for(&database_name).await;
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await
        .expect("load pool");

    let household = support::create_household(&pool).await;
    let users = seed_users(&pool, &household).await;
    seed_items(&pool, &household, &users).await;

    let telegram = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {"message_id": 1}
        })))
        .mount(&telegram)
        .await;

    // Первый прогон — боевой лимит 20 обновлений/мин на пользователя: часть
    // обновлений откладывается, а сверх очереди отложенных отбрасывается.
    let limited = run_scenario(
        Scenario {
            name: "production limit",
            update_base: 1_000_000_000,
            update_limit: None,
            all_processed: false,
        },
        &pool,
        &database_url,
        &telegram.uri(),
        &household,
        &users,
    )
    .await;

    // Второй прогон — тот же поток и данные, но лимит пользователя поднят
    // только в тесте: все обновления должны пройти через обработчик.
    let processed = run_scenario(
        Scenario {
            name: "raised update limit",
            update_base: 2_000_000_000,
            update_limit: Some(100_000),
            all_processed: true,
        },
        &pool,
        &database_url,
        &telegram.uri(),
        &household,
        &users,
    )
    .await;

    println!(
        "summary: '{}' p50={:.3} p95={:.3} p99={:.3} max={:.3} processed={} deferred={} dropped={} \
         drain={:.3}s processed_rate={:.1}/s | \
         '{}' p50={:.3} p95={:.3} p99={:.3} max={:.3} processed={} deferred={} dropped={} \
         drain={:.3}s processed_rate={:.1}/s",
        limited.name,
        millis(limited.p50),
        millis(limited.p95),
        millis(limited.p99),
        millis(limited.max),
        limited.processed,
        limited.deferred,
        limited.dropped,
        limited.drain.as_secs_f64(),
        limited.processed_rate,
        processed.name,
        millis(processed.p50),
        millis(processed.p95),
        millis(processed.p99),
        millis(processed.max),
        processed.processed,
        processed.deferred,
        processed.dropped,
        processed.drain.as_secs_f64(),
        processed.processed_rate
    );
}
