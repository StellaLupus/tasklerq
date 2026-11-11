//! Проверка, что журнал LLM-разбора не содержит исходный текст.
//!
//! Тест живёт в отдельном файле: `tracing` кеширует интерес к callsite, и в
//! общем тестовом бинарнике параллельные тесты успевают отключить его до того,
//! как здесь появится подписчик. Один тест на процесс делает перехват
//! детерминированным.

mod support;

use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::Utc;
use sqlx::Row;
use tasklerq::capture::prompt::PROMPT_VERSION;
use tasklerq::domain::models::{CaptureMode, CaptureSource};
use tasklerq::llm::{Attempt, Chain, LlmError};
use tasklerq::security_audit::Audit;
use tasklerq::storage::repositories::llm_runs::{self, FAILURE, NewRun, RunFields, SUCCESS};
use tasklerq::telegram::bot::Bot;
use tasklerq::telegram::capture_worker::{CaptureWorker, Pass};
use tokio::sync::Notify;

use support::logs::LogBuffer;
use support::telegram::{FakeTelegram, Harness};
use support::{db, next_telegram_id};

/// Цепочка, которая всегда отвечает пустым ответом.
fn failing_chain() -> Arc<Chain> {
    Arc::new(support::llm::chain(
        support::llm::BackendDouble::new(vec![Err(LlmError::EmptyResponse)]),
        None,
    ))
}

#[test]
fn llm_logs_do_not_contain_the_source_text() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let db = db().await;
            let id = next_telegram_id();
            let handler = Bot::new("tasklerqbot")
                .with_llm_capture(true)
                .with_tenancy(tasklerq::config::TenancyMode::Multi);
            let ai = failing_chain();
            let mut bot =
                Harness::with_handler(&db.pool, id, Arc::new(handler.clone()), FakeTelegram::new());
            bot.text("/start").await;
            bot.text("Семья").await;
            bot.press("Москва").await;
            bot.press("меню").await;

            let worker = CaptureWorker::new(
                db.pool.clone(),
                ai.clone(),
                handler,
                Arc::new(Notify::new()),
                Audit::new(db.pool.clone()),
            )
            .only_chat(id);
            bot.text("секретная заметка про подарок").await;
            assert_eq!(worker.run_once(bot.now).await.expect("pass"), Pass::Dead);
        });
    });

    let captured = logs.contents();
    assert!(
        captured.contains("llm capture request failed"),
        "the failure must be logged:\n{captured}"
    );
    assert!(
        !captured.contains("секретная заметка") && !captured.contains("подарок"),
        "the source text must not reach the logs:\n{captured}"
    );
}

fn attempt(
    slot: &'static str,
    number: u16,
    error_code: Option<&'static str>,
    duration: StdDuration,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
) -> Attempt {
    Attempt {
        slot,
        number,
        error_code,
        rejected_by_parse: false,
        duration,
        input_tokens,
        output_tokens,
        model: None,
    }
}

fn run_fields(household: &support::TestHousehold, source: CaptureSource) -> RunFields {
    RunFields {
        household_id: household.household_id,
        user_id: Some(household.owner_id),
        source,
        mode: CaptureMode::Draft,
        prompt_version: Some(PROMPT_VERSION),
    }
}

async fn recorded_runs(
    pool: &sqlx::PgPool,
    household_id: uuid::Uuid,
) -> Vec<sqlx::postgres::PgRow> {
    sqlx::query(
        "SELECT slot, attempt, status, error_code, latency_ms, input_tokens, output_tokens
         FROM llm_runs
         WHERE household_id = $1
         ORDER BY attempt NULLS FIRST, created_at",
    )
    .bind(household_id)
    .fetch_all(pool)
    .await
    .expect("runs should be readable")
}

#[tokio::test]
async fn fallback_attempts_are_recorded_and_counted_as_one_run() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let attempts = vec![
        attempt(
            "primary",
            1,
            Some("timeout"),
            StdDuration::from_millis(4_000),
            None,
            None,
        ),
        attempt(
            "fallback",
            2,
            None,
            StdDuration::from_millis(1_000),
            Some(100),
            Some(20),
        ),
    ];

    let mut conn = db.pool.acquire().await.expect("connection");
    for run in
        llm_runs::runs_from_attempts(run_fields(&household, CaptureSource::Telegram), &attempts)
    {
        llm_runs::record(&mut conn, run, now)
            .await
            .expect("run should be recorded");
    }

    let rows = recorded_runs(&db.pool, household.household_id).await;
    assert_eq!(rows.len(), 2);

    let primary = &rows[0];
    assert_eq!(
        primary.get::<Option<String>, _>("slot").as_deref(),
        Some("primary")
    );
    assert_eq!(primary.get::<Option<i16>, _>("attempt"), Some(1));
    assert_eq!(primary.get::<String, _>("status"), FAILURE);
    assert_eq!(
        primary.get::<Option<String>, _>("error_code").as_deref(),
        Some("timeout")
    );
    assert_eq!(primary.get::<i32, _>("latency_ms"), 4_000);
    assert_eq!(primary.get::<Option<i32>, _>("input_tokens"), None);
    assert_eq!(primary.get::<Option<i32>, _>("output_tokens"), None);

    let fallback = &rows[1];
    assert_eq!(
        fallback.get::<Option<String>, _>("slot").as_deref(),
        Some("fallback")
    );
    assert_eq!(fallback.get::<Option<i16>, _>("attempt"), Some(2));
    assert_eq!(fallback.get::<String, _>("status"), SUCCESS);
    assert_eq!(fallback.get::<Option<String>, _>("error_code"), None);
    assert_eq!(fallback.get::<i32, _>("latency_ms"), 1_000);
    assert_eq!(fallback.get::<Option<i32>, _>("input_tokens"), Some(100));
    assert_eq!(fallback.get::<Option<i32>, _>("output_tokens"), Some(20));

    let used = llm_runs::count_since(
        &mut conn,
        household.household_id,
        now - chrono::Duration::hours(24),
    )
    .await
    .expect("quota should be counted");
    assert_eq!(used, 1, "a run through two slots spends one quota unit");
}

#[tokio::test]
async fn successful_attempt_records_slot_number_latency_and_tokens() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let attempts = vec![attempt(
        "primary",
        1,
        None,
        StdDuration::from_millis(1_234),
        Some(1_500),
        Some(300),
    )];

    let mut conn = db.pool.acquire().await.expect("connection");
    for run in llm_runs::runs_from_attempts(run_fields(&household, CaptureSource::Api), &attempts) {
        llm_runs::record(&mut conn, run, now)
            .await
            .expect("run should be recorded");
    }

    let rows = recorded_runs(&db.pool, household.household_id).await;
    assert_eq!(rows.len(), 1);
    let run = &rows[0];
    assert_eq!(
        run.get::<Option<String>, _>("slot").as_deref(),
        Some("primary")
    );
    assert_eq!(run.get::<Option<i16>, _>("attempt"), Some(1));
    assert_eq!(run.get::<String, _>("status"), SUCCESS);
    assert_eq!(run.get::<Option<String>, _>("error_code"), None);
    assert_eq!(run.get::<i32, _>("latency_ms"), 1_234);
    assert_eq!(run.get::<Option<i32>, _>("input_tokens"), Some(1_500));
    assert_eq!(run.get::<Option<i32>, _>("output_tokens"), Some(300));

    let used = llm_runs::count_since(
        &mut conn,
        household.household_id,
        now - chrono::Duration::hours(24),
    )
    .await
    .expect("quota should be counted");
    assert_eq!(used, 1);
}

#[tokio::test]
async fn rows_without_attempt_count_toward_the_quota_but_extra_attempts_do_not() {
    let db = db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");
    let fields = run_fields(&household, CaptureSource::Telegram);

    let legacy = NewRun {
        household_id: fields.household_id,
        user_id: fields.user_id,
        source: fields.source,
        mode: fields.mode,
        prompt_version: fields.prompt_version,
        slot: None,
        attempt: None,
        status: SUCCESS,
        error_code: None,
        latency_ms: 500,
        input_tokens: None,
        output_tokens: None,
    };
    llm_runs::record(&mut conn, legacy, now)
        .await
        .expect("legacy run should be recorded");

    let since = now - chrono::Duration::hours(24);
    assert_eq!(
        llm_runs::count_since(&mut conn, household.household_id, since)
            .await
            .expect("quota"),
        1,
        "a row from before 0018 still counts as a run"
    );

    let second_attempt = NewRun {
        household_id: fields.household_id,
        user_id: fields.user_id,
        source: fields.source,
        mode: fields.mode,
        prompt_version: fields.prompt_version,
        slot: Some("fallback"),
        attempt: Some(2),
        status: SUCCESS,
        error_code: None,
        latency_ms: 700,
        input_tokens: None,
        output_tokens: None,
    };
    llm_runs::record(&mut conn, second_attempt, now)
        .await
        .expect("second attempt should be recorded");
    assert_eq!(
        llm_runs::count_since(&mut conn, household.household_id, since)
            .await
            .expect("quota"),
        1,
        "an extra slot attempt is not a separate run"
    );

    let first_attempt = NewRun {
        household_id: fields.household_id,
        user_id: fields.user_id,
        source: fields.source,
        mode: fields.mode,
        prompt_version: fields.prompt_version,
        slot: Some("primary"),
        attempt: Some(1),
        status: SUCCESS,
        error_code: None,
        latency_ms: 600,
        input_tokens: None,
        output_tokens: None,
    };
    llm_runs::record(&mut conn, first_attempt, now)
        .await
        .expect("first attempt should be recorded");
    assert_eq!(
        llm_runs::count_since(&mut conn, household.household_id, since)
            .await
            .expect("quota"),
        2
    );
}

#[tokio::test]
async fn run_records_have_no_columns_for_the_source_text_or_the_model_answer() {
    let db = db().await;
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns WHERE table_name = 'llm_runs'",
    )
    .fetch_all(&db.pool)
    .await
    .expect("columns should be readable");

    for forbidden in ["raw_text", "text", "content", "response", "answer"] {
        assert!(
            !columns.iter().any(|column| column == forbidden),
            "llm_runs must not store user text or the model answer, found {forbidden}"
        );
    }
}
