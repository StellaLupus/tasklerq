//! Readiness: база, применённые миграции и задержка очереди.

mod support;

use std::sync::Arc;
use std::time::Duration as StdDuration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use http_body_util::BodyExt;
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use tasklerq::app::build_application;
use tasklerq::capture::eval::TitleMatch;
use tasklerq::capture::prompt;
use tasklerq::capture::slot_check;
use tasklerq::capture::slot_status::{CACHE_TTL, LlmSlotHealth, SlotStatus};
use tasklerq::config::{
    CaptureConfig, Config, LlmConfig, SecretKey, SecretString, SlotConfig, TenancyMode,
};
use tasklerq::http::api::capture::CaptureState;
use tasklerq::http::router::{AppState, build_router};
use tasklerq::llm::{Chain, LlmChange, LlmHandle, LlmSettings, LlmSnapshot};
use tasklerq::security_audit::Audit;
use tasklerq::settings;
use tasklerq::settings::crypto::SecretCipher;
use tasklerq::storage::repositories::llm_slot_checks::{self, NewCheck};
use tasklerq::storage::repositories::llm_slot_settings;
use tokio::sync::watch;
use tower::ServiceExt;

async fn ready(pool: &PgPool) -> (StatusCode, Value) {
    ready_with_mode(pool, TenancyMode::Single).await
}

async fn ready_with_mode(pool: &PgPool, mode: TenancyMode) -> (StatusCode, Value) {
    let state = AppState::new(pool.clone(), Audit::new(pool.clone())).with_tenancy_mode(mode);
    get(&build_router(state), "/health/ready").await
}

async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = serde_json::from_slice(&bytes).expect("JSON");
    (status, body)
}

fn slot_config(model: &str) -> SlotConfig {
    SlotConfig {
        url: "https://api.example.test/v1".parse().expect("url"),
        model: model.to_owned(),
        token: Some(SecretString::new("SECRET-slot-token")),
        timeout: StdDuration::from_secs(10),
        extra: Map::new(),
    }
}

/// Цепочка из двойников и состояние проверки слотов для readiness.
fn llm_health(
    primary: &SlotConfig,
    fallback: Option<&SlotConfig>,
) -> (Arc<Chain>, Arc<LlmSlotHealth>) {
    let (chain, _handle, health) = llm_health_with_handle(primary, fallback);
    (chain, health)
}

/// То же, но с доступом к снимку для применения настроек в тесте.
fn llm_health_with_handle(
    primary: &SlotConfig,
    fallback: Option<&SlotConfig>,
) -> (Arc<Chain>, LlmHandle, Arc<LlmSlotHealth>) {
    let answer = || {
        support::llm::BackendDouble::always(Ok(support::llm::v4::response(vec![
            support::llm::v4::shopping("Молоко"),
        ])))
    };
    let chain = Arc::new(support::llm::chain(answer(), fallback.map(|_| answer())));
    let llm = LlmConfig {
        enabled: Some(true),
        primary: Some(primary.clone()),
        fallback: fallback.cloned(),
        connect_timeout: StdDuration::from_millis(1_000),
        connect_timeout_from_env: false,
        daily_quota: 200,
        daily_quota_from_env: false,
        ignored_env: Vec::new(),
    };
    let effective = settings::effective_llm(&llm, &[], &SecretCipher::new(None, None));
    let handle = LlmHandle::new(LlmSnapshot::from_effective_and_chain(
        effective,
        Some(chain.clone()),
    ));
    let health = Arc::new(LlmSlotHealth::new(handle.clone()));
    (chain, handle, health)
}

/// Применение настроек без слотов в окружении: слот целиком приходит из базы.
fn llm_settings(pool: &PgPool, handle: LlmHandle) -> LlmSettings {
    let env = LlmConfig {
        enabled: None,
        primary: None,
        fallback: None,
        connect_timeout: StdDuration::from_millis(1_000),
        connect_timeout_from_env: false,
        daily_quota: 200,
        daily_quota_from_env: false,
        ignored_env: Vec::new(),
    };
    LlmSettings::new(pool.clone(), env, SecretCipher::new(None, None), handle)
}

async fn store_check(
    pool: &PgPool,
    slot: &str,
    config: &SlotConfig,
    passed: usize,
    total: usize,
    titles: TitleMatch,
) {
    let fingerprint = slot_check::fingerprint(
        config,
        &prompt::contract_hash(),
        &slot_check::set_hash(slot_check::BUILTIN_SET),
        titles,
    );
    let parts = fingerprint.parts.to_json();
    let report = json!({"total": total, "passed": passed});
    let mut conn = pool.acquire().await.expect("connection");
    llm_slot_checks::upsert(
        &mut conn,
        NewCheck {
            slot,
            fingerprint: &fingerprint.value,
            fingerprint_parts: &parts,
            passed,
            total,
            p95_ms: 1_000,
            report: &report,
        },
        Utc::now(),
    )
    .await
    .expect("slot check should be stored");
}

async fn ready_with_llm(pool: &PgPool, health: Arc<LlmSlotHealth>) -> (StatusCode, Value) {
    let state = AppState::new(pool.clone(), Audit::new(pool.clone()))
        .with_tenancy_mode(TenancyMode::Single)
        .with_llm_slots(health);
    get(&build_router(state), "/health/ready").await
}

fn secret_key(byte: u8) -> SecretKey {
    SecretKey::parse(&BASE64.encode([byte; 32])).expect("32 bytes are a key")
}

fn secret_cipher(current: Option<u8>, old: Option<u8>) -> SecretCipher {
    SecretCipher::new(current.map(secret_key), old.map(secret_key))
}

async fn ready_with_secret_cipher(pool: &PgPool, cipher: SecretCipher) -> (StatusCode, Value) {
    let state = AppState::new(pool.clone(), Audit::new(pool.clone()))
        .with_tenancy_mode(TenancyMode::Single)
        .with_secret_cipher(cipher);
    get(&build_router(state), "/health/ready").await
}

async fn store_secret(pool: &PgPool, cipher: &SecretCipher) {
    let mut conn = pool.acquire().await.expect("connection");
    settings::write_secret(
        &mut conn,
        "llm.primary.token",
        &SecretString::new("SECRET-stored-token"),
        cipher,
    )
    .await
    .expect("secret setting is written");
}

fn capture_config() -> CaptureConfig {
    CaptureConfig {
        enabled: true,
        voice_timeout_ms: 8_000,
        voice_fallback_raw: false,
        rate_limit_per_min: 10,
        max_text_chars: 2_000,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_requires_every_embedded_migration_to_be_successful() {
    let db = support::isolated_db().await;

    // Полностью мигрированная база готова и отдаёт версию и время.
    let (status, body) = ready(&db.pool).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    let time = body["time"].as_str().expect("time");
    DateTime::parse_from_rfc3339(time).expect("RFC 3339");

    // Неуспешная запись в журнале миграций — не готов.
    sqlx::query(
        "UPDATE _sqlx_migrations SET success = false
         WHERE version = (SELECT max(version) FROM _sqlx_migrations)",
    )
    .execute(&db.pool)
    .await
    .expect("mark failed");
    let (status, body) = ready(&db.pool).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "service_unavailable");

    // Возврат успешного флага снова делает базу готовой.
    sqlx::query(
        "UPDATE _sqlx_migrations SET success = true
         WHERE version = (SELECT max(version) FROM _sqlx_migrations)",
    )
    .execute(&db.pool)
    .await
    .expect("mark successful");
    assert_eq!(ready(&db.pool).await.0, StatusCode::OK);

    // Отсутствующая миграция (журнал не содержит последнюю версию) — не готов.
    sqlx::query(
        "DELETE FROM _sqlx_migrations
         WHERE version = (SELECT max(version) FROM _sqlx_migrations)",
    )
    .execute(&db.pool)
    .await
    .expect("drop migration record");
    let (status, body) = ready(&db.pool).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "service_unavailable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_mode_with_one_household_has_no_warnings() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;

    let (status, body) = ready_with_mode(&db.pool, TenancyMode::Single).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "ok");
    assert!(
        body.get("warnings").is_none(),
        "one household must not warn:\n{body}"
    );
    assert!(
        !body
            .to_string()
            .contains(&household.household_id.to_string())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_mode_with_two_households_warns_without_names_or_ids() {
    let db = support::isolated_db().await;
    let first = support::create_household(&db.pool).await;
    let second = support::create_household(&db.pool).await;

    let (status, body) = ready_with_mode(&db.pool, TenancyMode::Single).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["warnings"], json!(["multiple_households"]));
    let response = body.to_string();
    for household in [first, second] {
        assert!(!response.contains(&household.household_id.to_string()));
    }
    assert!(!response.contains("Test household"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_mode_with_two_households_has_no_warnings() {
    let db = support::isolated_db().await;
    support::create_household(&db.pool).await;
    support::create_household(&db.pool).await;

    let (status, body) = ready_with_mode(&db.pool, TenancyMode::Multi).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.get("warnings").is_none(),
        "multi mode must not warn:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warning_disappears_after_a_soft_delete_without_rebuilding_the_app() {
    let db = support::isolated_db().await;
    let first = support::create_household(&db.pool).await;
    let second = support::create_household(&db.pool).await;
    let state = AppState::new(db.pool.clone(), Audit::new(db.pool.clone()))
        .with_tenancy_mode(TenancyMode::Single);
    let app = build_router(state);

    let (status, body) = get(&app, "/health/ready").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["warnings"], json!(["multiple_households"]));

    tasklerq::admin::delete_household(&db.pool, second.household_id, Utc::now())
        .await
        .expect("second household should be deleted");

    let (status, body) = get(&app, "/health/ready").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.get("warnings").is_none(),
        "the warning must disappear without a restart:\n{body}"
    );
    assert!(!body.to_string().contains(&first.household_id.to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn liveness_never_reports_warnings() {
    let db = support::isolated_db().await;
    support::create_household(&db.pool).await;
    support::create_household(&db.pool).await;
    let state = AppState::new(db.pool.clone(), Audit::new(db.pool.clone()))
        .with_tenancy_mode(TenancyMode::Single);
    let app = build_router(state);

    let (status, body) = get(&app, "/health/live").await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "ok");
    assert!(
        body.get("warnings").is_none(),
        "liveness must never compute degradation:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_build_wires_the_tenancy_mode_into_readiness() {
    let db = support::isolated_db().await;
    support::create_household(&db.pool).await;
    support::create_household(&db.pool).await;
    let database_url = support::database_url().await;

    // Проверяем именно боевую сборку: дефолт `AppState::new` — `Single`, поэтому
    // забытое `with_tenancy_mode` проявится в `multi` ложным предупреждением.
    for (mode, expected_warning) in [("single", true), ("multi", false)] {
        let config = Config::from_map([
            ("ENV", "development"),
            ("PUBLIC_BASE_URL", "http://127.0.0.1:18080"),
            ("DATABASE_URL", database_url.as_str()),
            ("TELEGRAM_MODE", "disabled"),
            ("TENANCY_MODE", mode),
            ("LLM_CAPTURE_ENABLED", "false"),
        ])
        .expect("test config should be valid");
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let application = build_application(&config, db.pool.clone(), shutdown_rx)
            .await
            .expect("application should build");

        let (status, body) = get(&application.router, "/health/ready").await;
        assert_eq!(status, StatusCode::OK, "{mode}: {body}");
        if expected_warning {
            assert_eq!(
                body["warnings"],
                json!(["multiple_households"]),
                "{mode}: {body}"
            );
        } else {
            assert!(
                body.get("warnings").is_none(),
                "{mode}: the configured mode must reach readiness:\n{body}"
            );
        }

        shutdown_tx.send(true).expect("shutdown signal");
        for worker in application.workers {
            tokio::time::timeout(StdDuration::from_secs(5), worker)
                .await
                .expect("worker must stop on shutdown")
                .expect("worker task must join");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_llm_slots_unavailable_is_a_warning_not_a_failure() {
    let db = support::isolated_db().await;
    let primary = slot_config("vendor/primary");
    let fallback = slot_config("vendor/fallback");
    let (chain, health) = llm_health(&primary, Some(&fallback));
    store_check(&db.pool, "primary", &primary, 22, 22, TitleMatch::Strict).await;
    store_check(&db.pool, "fallback", &fallback, 22, 22, TitleMatch::Strict).await;
    let now = Utc::now();
    support::llm::open_primary(&chain, now);
    support::llm::open_fallback(&chain, now);

    let (status, body) = ready_with_llm(&db.pool, health).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["warnings"], json!(["llm_unavailable"]), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_primary_breaker_alone_does_not_warn_about_llm() {
    let db = support::isolated_db().await;
    let primary = slot_config("vendor/primary");
    let fallback = slot_config("vendor/fallback");
    let (chain, health) = llm_health(&primary, Some(&fallback));
    store_check(&db.pool, "primary", &primary, 22, 22, TitleMatch::Strict).await;
    store_check(&db.pool, "fallback", &fallback, 22, 22, TitleMatch::Strict).await;
    support::llm::open_primary(&chain, Utc::now());

    let (status, body) = ready_with_llm(&db.pool, health).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.get("warnings").is_none(),
        "the fallback slot still serves capture:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unchecked_primary_slot_is_reported_in_readiness() {
    let db = support::isolated_db().await;
    let primary = slot_config("vendor/primary");
    let (_chain, health) = llm_health(&primary, None);

    let (status, body) = ready_with_llm(&db.pool, health).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["warnings"],
        json!(["llm_slot_unverified:primary"]),
        "{body}"
    );
    let response = body.to_string();
    assert!(!response.contains("vendor/primary"), "{response}");
    assert!(!response.contains("api.example.test"), "{response}");
    assert!(!response.contains("SECRET-slot-token"), "{response}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_normalized_check_names_its_standard_in_readiness() {
    let db = support::isolated_db().await;
    let primary = slot_config("vendor/primary");
    let (_chain, health) = llm_health(&primary, None);
    let mut conn = db.pool.acquire().await.expect("connection");
    llm_slot_settings::set_titles(&mut conn, "primary", "normalized")
        .await
        .expect("standard");
    drop(conn);
    store_check(
        &db.pool,
        "primary",
        &primary,
        17,
        22,
        TitleMatch::Normalized,
    )
    .await;

    let (status, body) = ready_with_llm(&db.pool, health).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["warnings"],
        json!(["llm_slot_unverified:primary:normalized"]),
        "the warning must name the saved standard:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_standard_makes_the_saved_check_stale() {
    let db = support::isolated_db().await;
    let primary = slot_config("vendor/primary");
    let (_chain, health) = llm_health(&primary, None);
    store_check(&db.pool, "primary", &primary, 22, 22, TitleMatch::Strict).await;
    let mut conn = db.pool.acquire().await.expect("connection");
    llm_slot_settings::set_titles(&mut conn, "primary", "normalized")
        .await
        .expect("standard");
    drop(conn);

    let (status, body) = ready_with_llm(&db.pool, health).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["warnings"],
        json!(["llm_slot_unverified:primary:strict"]),
        "a strict check must not pass as normalized:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_change_makes_the_slot_stale_without_waiting_for_the_cache() {
    let db = support::isolated_db().await;
    let primary = slot_config("vendor/primary");
    let (_chain, handle, health) = llm_health_with_handle(&primary, None);
    store_check(&db.pool, "primary", &primary, 22, 22, TitleMatch::Strict).await;

    let (status, body) = ready_with_llm(&db.pool, health.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.get("warnings").is_none(),
        "a passed slot must not warn before the change:\n{body}"
    );

    // Меняем только модель: адрес и включение разбора пишем в той же группе,
    // потому что у настроек в этом тесте нет слотов из окружения.
    llm_settings(&db.pool, handle)
        .apply(&[
            LlmChange::set("llm.primary.url", json!(primary.url.as_str())),
            LlmChange::set("llm.primary.model", json!("vendor/other")),
            LlmChange::set("llm.enabled", json!(true)),
        ])
        .await
        .expect("a model change is applied");

    // Ни перезапуска, ни `invalidate`, ни ожидания `CACHE_TTL`: кеш прежней
    // версии снимка недействителен сразу.
    let (status, body) = ready_with_llm(&db.pool, health).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["warnings"],
        json!(["llm_slot_unverified:primary:strict"]),
        "the changed model must be stale immediately:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slot_added_after_start_is_seen_by_readiness_without_a_restart() {
    let db = support::isolated_db().await;
    let handle = LlmHandle::empty();
    let health = Arc::new(LlmSlotHealth::new(handle.clone()));

    let (status, body) = ready_with_llm(&db.pool, health.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.get("warnings").is_none(),
        "a service without slots has nothing to warn about:\n{body}"
    );

    llm_settings(&db.pool, handle)
        .apply(&[
            LlmChange::set("llm.primary.url", json!("https://api.example.test/v1")),
            LlmChange::set("llm.primary.model", json!("vendor/primary")),
            LlmChange::set("llm.enabled", json!(true)),
        ])
        .await
        .expect("a slot is applied without a check");

    let (status, body) = ready_with_llm(&db.pool, health).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["warnings"],
        json!(["llm_slot_unverified:primary"]),
        "a slot added in the console must appear in readiness at once:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unchecked_slot_does_not_block_capture() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let token = support::issue_token(&db.pool, household.household_id, &["capture:write"]).await;
    let primary = slot_config("vendor/primary");
    let (chain, health) = llm_health(&primary, None);
    let state = AppState::new(db.pool.clone(), Audit::new(db.pool.clone()))
        .with_tenancy_mode(TenancyMode::Single)
        .with_llm_slots(health)
        .with_capture(CaptureState::new(
            capture_config(),
            LlmHandle::new(LlmSnapshot::for_tests_without_settings(Some(chain), 200)),
        ));
    let app = build_router(state);

    let (status, body) = support::request(
        &app,
        Method::POST,
        "/api/v1/capture",
        Some(&token),
        Some(json!({"text": "Молоко", "source": "api", "mode": "auto"})),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "applied", "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_passed_slot_has_no_readiness_warning() {
    let db = support::isolated_db().await;
    let primary = slot_config("vendor/primary");
    let (_chain, health) = llm_health(&primary, None);
    store_check(&db.pool, "primary", &primary, 22, 22, TitleMatch::Strict).await;

    let (status, body) = ready_with_llm(&db.pool, health).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.get("warnings").is_none(),
        "a passed slot must not warn:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_readiness_status_is_cached_for_a_minute() {
    let db = support::isolated_db().await;
    let primary = slot_config("vendor/primary");
    let (_chain, health) = llm_health(&primary, None);
    store_check(&db.pool, "primary", &primary, 22, 22, TitleMatch::Strict).await;

    let (status, body) = ready_with_llm(&db.pool, health.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("warnings").is_none(), "{body}");

    // Проверка изменилась, но readiness ещё смотрит в кеш.
    store_check(&db.pool, "primary", &primary, 17, 22, TitleMatch::Strict).await;
    let (status, body) = ready_with_llm(&db.pool, health.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.get("warnings").is_none(),
        "the cached status must hide the new row:\n{body}"
    );

    // Явный сброс кеша заменяет ожидание минуты.
    health.invalidate();
    let (status, body) = ready_with_llm(&db.pool, health).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["warnings"],
        json!(["llm_slot_unverified:primary:strict"]),
        "a saved check names its standard:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_readiness_cache_expires_after_its_ttl() {
    let db = support::isolated_db().await;
    let primary = slot_config("vendor/primary");
    let (_chain, health) = llm_health(&primary, None);
    store_check(&db.pool, "primary", &primary, 22, 22, TitleMatch::Strict).await;

    let now = Utc::now();
    let statuses = health.statuses(&db.pool, now).await;
    assert_eq!(statuses[0].status, SlotStatus::Passed);

    // Новая проверка в базе не видна, пока не истёк срок кеша.
    store_check(&db.pool, "primary", &primary, 17, 22, TitleMatch::Strict).await;
    let cached = health
        .statuses(&db.pool, now + CACHE_TTL - ChronoDuration::seconds(1))
        .await;
    assert_eq!(cached[0].status, SlotStatus::Passed);

    // Ровно через срок статус перечитывается из базы.
    let fresh = health.statuses(&db.pool, now + CACHE_TTL).await;
    assert_eq!(
        fresh[0].status,
        SlotStatus::Failed {
            passed: 17,
            total: 22
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_readiness_refresh_keeps_the_previous_status_and_extends_the_cache() {
    let db = support::isolated_db().await;
    let primary = slot_config("vendor/primary");
    let (_chain, health) = llm_health(&primary, None);
    store_check(&db.pool, "primary", &primary, 22, 22, TitleMatch::Strict).await;
    let now = Utc::now();
    assert_eq!(
        health.statuses(&db.pool, now).await[0].status,
        SlotStatus::Passed
    );

    // Проверка в базе изменилась, но прочитать её нечем.
    store_check(&db.pool, "primary", &primary, 17, 22, TitleMatch::Strict).await;
    let broken = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(StdDuration::from_millis(500))
        .connect_lazy("postgres://tasklerq:secret@127.0.0.1:1/tasklerq")
        .expect("lazy pool");
    let kept = health.statuses(&broken, now + CACHE_TTL).await;
    assert_eq!(
        kept[0].status,
        SlotStatus::Passed,
        "a failed read must keep the previous status"
    );

    // Отметка кеша сдвинута: сразу после ошибки база снова не опрашивается.
    let still_cached = health
        .statuses(&db.pool, now + CACHE_TTL + ChronoDuration::seconds(1))
        .await;
    assert_eq!(still_cached[0].status, SlotStatus::Passed);

    // После нового срока статус перечитывается и видит новую проверку.
    let fresh = health.statuses(&db.pool, now + CACHE_TTL + CACHE_TTL).await;
    assert_eq!(
        fresh[0].status,
        SlotStatus::Failed {
            passed: 17,
            total: 22
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_refresh_after_a_snapshot_change_does_not_return_the_old_cache() {
    let db = support::isolated_db().await;
    let primary = slot_config("vendor/primary");
    let (_chain, handle, health) = llm_health_with_handle(&primary, None);
    store_check(&db.pool, "primary", &primary, 22, 22, TitleMatch::Strict).await;
    let now = Utc::now();
    assert_eq!(
        health.statuses(&db.pool, now).await[0].status,
        SlotStatus::Passed
    );

    llm_settings(&db.pool, handle)
        .apply(&[
            LlmChange::set("llm.primary.url", json!(primary.url.as_str())),
            LlmChange::set("llm.primary.model", json!("vendor/other")),
            LlmChange::set("llm.enabled", json!(true)),
        ])
        .await
        .expect("a model change is applied");

    let broken = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(StdDuration::from_millis(500))
        .connect_lazy("postgres://tasklerq:secret@127.0.0.1:1/tasklerq")
        .expect("lazy pool");
    let statuses = health.statuses(&broken, now + CACHE_TTL).await;

    assert_eq!(statuses.len(), 1);
    assert_eq!(
        statuses[0].status,
        SlotStatus::Unverified,
        "a cached status of another snapshot version must not be kept"
    );
}

#[tokio::test]
async fn startup_warns_about_an_unchecked_primary_slot_without_secrets() {
    let db = support::isolated_db().await;
    let database_url = support::database_url().await;
    let config = Config::from_map([
        ("ENV", "development"),
        ("PUBLIC_BASE_URL", "http://127.0.0.1:18080"),
        ("DATABASE_URL", database_url.as_str()),
        ("TELEGRAM_MODE", "disabled"),
        ("TENANCY_MODE", "single"),
        ("LLM_CAPTURE_ENABLED", "true"),
        ("LLM_PRIMARY_URL", "https://api.example.test/v1"),
        ("LLM_PRIMARY_MODEL", "vendor/primary"),
        ("LLM_PRIMARY_TOKEN", "SECRET-slot-token"),
    ])
    .expect("test config should be valid");

    // current_thread: подписчик `set_default` живёт на потоке теста, и все
    // записи старта попадают в буфер.
    let logs = support::logs::LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(&config, db.pool.clone(), shutdown_rx)
        .await
        .expect("an unchecked slot must not stop startup");

    let (status, body) = get(&application.router, "/health/ready").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["warnings"],
        json!(["llm_slot_unverified:primary"]),
        "{body}"
    );

    let text = logs.contents();
    let warns_about_primary = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .any(|record| {
            record["level"] == "WARN"
                && record["fields"]["slot"] == "primary"
                && record["fields"]["status"] == "unverified"
        });
    assert!(
        warns_about_primary,
        "the startup log must warn with slot = primary:\n{text}"
    );
    assert!(!text.contains("SECRET-slot-token"), "token leaked:\n{text}");
    assert!(
        !text.contains("api.example.test"),
        "slot address leaked:\n{text}"
    );

    shutdown_tx.send(true).expect("shutdown signal");
    for worker in application.workers {
        tokio::time::timeout(StdDuration::from_secs(5), worker)
            .await
            .expect("worker must stop on shutdown")
            .expect("worker task must join");
    }
}

#[tokio::test]
async fn startup_names_the_saved_standard_of_a_passed_slot() {
    let db = support::isolated_db().await;
    let database_url = support::database_url().await;
    let config = Config::from_map([
        ("ENV", "development"),
        ("PUBLIC_BASE_URL", "http://127.0.0.1:18080"),
        ("DATABASE_URL", database_url.as_str()),
        ("TELEGRAM_MODE", "disabled"),
        ("TENANCY_MODE", "single"),
        ("LLM_CAPTURE_ENABLED", "true"),
        ("LLM_PRIMARY_URL", "https://api.example.test/v1"),
        ("LLM_PRIMARY_MODEL", "vendor/primary"),
        ("LLM_PRIMARY_TOKEN", "SECRET-slot-token"),
    ])
    .expect("test config should be valid");

    let primary = slot_config("vendor/primary");
    let mut conn = db.pool.acquire().await.expect("connection");
    llm_slot_settings::set_titles(&mut conn, "primary", "normalized")
        .await
        .expect("standard");
    drop(conn);
    store_check(
        &db.pool,
        "primary",
        &primary,
        22,
        22,
        TitleMatch::Normalized,
    )
    .await;

    // current_thread: подписчик `set_default` живёт на потоке теста, и все
    // записи старта попадают в буфер.
    let logs = support::logs::LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(&config, db.pool.clone(), shutdown_rx)
        .await
        .expect("a passed slot must not stop startup");

    let (status, body) = get(&application.router, "/health/ready").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.get("warnings").is_none(),
        "a passed slot must not warn:\n{body}"
    );

    let text = logs.contents();
    let names_the_standard = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .any(|record| {
            record["level"] == "INFO"
                && record["fields"]["slot"] == "primary"
                && record["fields"]["status"] == "passed"
                && record["fields"]["titles"] == "normalized"
        });
    assert!(
        names_the_standard,
        "the startup log must name the saved standard:\n{text}"
    );
    assert!(!text.contains("SECRET-slot-token"), "token leaked:\n{text}");
    assert!(
        !text.contains("api.example.test"),
        "slot address leaked:\n{text}"
    );

    shutdown_tx.send(true).expect("shutdown signal");
    for worker in application.workers {
        tokio::time::timeout(StdDuration::from_secs(5), worker)
            .await
            .expect("worker must stop on shutdown")
            .expect("worker task must join");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_reports_unreadable_stored_secrets_without_names() {
    let db = support::isolated_db().await;
    store_secret(&db.pool, &secret_cipher(Some(1), None)).await;

    for (cipher, expected) in [
        (secret_cipher(None, None), "secret_key_missing"),
        (secret_cipher(Some(2), None), "secret_undecryptable"),
    ] {
        let (status, body) = ready_with_secret_cipher(&db.pool, cipher).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["warnings"], json!([expected]), "{body}");
        let response = body.to_string();
        assert!(!response.contains("llm.primary.token"), "{response}");
        assert!(!response.contains("SECRET-stored-token"), "{response}");
    }

    for cipher in [
        secret_cipher(Some(1), None),
        secret_cipher(Some(2), Some(1)),
    ] {
        let (status, body) = ready_with_secret_cipher(&db.pool, cipher).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.get("warnings").is_none(),
            "a readable secret must not warn:\n{body}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tampered_secret_is_reported_as_undecryptable() {
    let db = support::isolated_db().await;
    store_secret(&db.pool, &secret_cipher(Some(1), None)).await;

    let mut blob: Vec<u8> = sqlx::query_scalar("SELECT secret FROM settings WHERE key = $1")
        .bind("llm.primary.token")
        .fetch_one(&db.pool)
        .await
        .expect("one secret row");
    let last = blob.len() - 1;
    blob[last] ^= 0x01;
    sqlx::query("UPDATE settings SET secret = $1 WHERE key = $2")
        .bind(&blob)
        .bind("llm.primary.token")
        .execute(&db.pool)
        .await
        .expect("tampered secret is stored");

    let (status, body) = ready_with_secret_cipher(&db.pool, secret_cipher(Some(1), None)).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["warnings"], json!(["secret_undecryptable"]), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_without_stored_secrets_does_not_warn_about_a_missing_key() {
    let db = support::isolated_db().await;

    let (status, body) = ready_with_secret_cipher(&db.pool, secret_cipher(None, None)).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.get("warnings").is_none(),
        "no stored secrets means no key warning:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_secret_saved_after_start_is_seen_without_a_restart() {
    let db = support::isolated_db().await;
    let state = AppState::new(db.pool.clone(), Audit::new(db.pool.clone()))
        .with_tenancy_mode(TenancyMode::Single)
        .with_secret_cipher(secret_cipher(None, None));
    let app = build_router(state);

    let (status, body) = get(&app, "/health/ready").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("warnings").is_none(), "{body}");

    store_secret(&db.pool, &secret_cipher(Some(1), None)).await;

    let (status, body) = get(&app, "/health/ready").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["warnings"], json!(["secret_key_missing"]), "{body}");
}

#[tokio::test]
async fn startup_warns_about_stored_secrets_without_a_key() {
    let db = support::isolated_db().await;
    let database_url = support::database_url().await;
    let config = Config::from_map([
        ("ENV", "development"),
        ("PUBLIC_BASE_URL", "http://127.0.0.1:18080"),
        ("DATABASE_URL", database_url.as_str()),
        ("TELEGRAM_MODE", "disabled"),
        ("TENANCY_MODE", "single"),
        ("LLM_CAPTURE_ENABLED", "false"),
    ])
    .expect("test config should be valid");

    // current_thread: подписчик `set_default` живёт на потоке теста, и все
    // записи старта попадают в буфер.
    let logs = support::logs::LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Секретов нет — строки о ключе тоже нет.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(&config, db.pool.clone(), shutdown_rx)
        .await
        .expect("an empty settings table must not stop startup");
    shutdown_tx.send(true).expect("shutdown signal");
    for worker in application.workers {
        tokio::time::timeout(StdDuration::from_secs(5), worker)
            .await
            .expect("worker must stop on shutdown")
            .expect("worker task must join");
    }
    let before = logs.contents();
    assert!(
        !before.contains("secret_key_missing"),
        "no stored secrets means no startup warning:\n{before}"
    );

    // Секрет записан ключом A, сервис запускается без ключа.
    store_secret(&db.pool, &secret_cipher(Some(1), None)).await;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let application = build_application(&config, db.pool.clone(), shutdown_rx)
        .await
        .expect("an unreadable secret must not stop startup");

    let text = logs.contents();
    let warned = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .any(|record| {
            record["level"] == "WARN"
                && record["fields"]["code"] == "secret_key_missing"
                && record["fields"]["secrets"] == 1
        });
    assert!(
        warned,
        "the startup log must warn with the code and row count:\n{text}"
    );
    assert!(
        !text.contains("llm.primary.token"),
        "the startup log must not name settings:\n{text}"
    );
    assert!(
        !text.contains("SECRET-stored-token"),
        "the startup log must not contain secret values:\n{text}"
    );

    shutdown_tx.send(true).expect("shutdown signal");
    for worker in application.workers {
        tokio::time::timeout(StdDuration::from_secs(5), worker)
            .await
            .expect("worker must stop on shutdown")
            .expect("worker task must join");
    }
}
