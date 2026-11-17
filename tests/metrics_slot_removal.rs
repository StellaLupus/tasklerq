//! Метрика `llm_slot_verified` не должна переживать слот, убранный из снимка.
//!
//! Отдельный файл тестов: реестр метрик общий на процесс, а метки слотов
//! фиксированы (`primary`), поэтому проверка отсутствия серии не должна делить
//! процесс с тестами, которые ту же серию пишут.

mod support;

use std::time::Duration;

use chrono::Utc;
use serde_json::{Map, json};
use tasklerq::capture::eval::TitleMatch;
use tasklerq::capture::prompt;
use tasklerq::capture::slot_check;
use tasklerq::capture::slot_status::LlmSlotHealth;
use tasklerq::config::{LlmConfig, SecretString, SlotConfig};
use tasklerq::llm::{LlmChange, LlmHandle, LlmSettings, LlmSnapshot};
use tasklerq::settings;
use tasklerq::settings::crypto::SecretCipher;
use tasklerq::storage::repositories::llm_slot_checks::{self, NewCheck};

fn slot() -> SlotConfig {
    SlotConfig {
        url: "https://api.example.test/v1".parse().expect("url"),
        model: "vendor/primary".to_owned(),
        token: Some(SecretString::new("SECRET-slot-token")),
        timeout: Duration::from_secs(10),
        extra: Map::new(),
    }
}

fn env_with(primary: Option<SlotConfig>) -> LlmConfig {
    LlmConfig {
        enabled: primary.as_ref().map(|_| true),
        primary,
        fallback: None,
        connect_timeout: Duration::from_millis(1_000),
        connect_timeout_from_env: false,
        daily_quota: 200,
        daily_quota_from_env: false,
        ignored_env: Vec::new(),
    }
}

async fn store_passed_check(pool: &sqlx::PgPool, config: &SlotConfig) {
    let fingerprint = slot_check::fingerprint(
        config,
        &prompt::contract_hash(),
        &slot_check::set_hash(slot_check::BUILTIN_SET),
        TitleMatch::Strict,
    );
    let parts = fingerprint.parts.to_json();
    let report = json!({"total": 22, "passed": 22});
    let mut conn = pool.acquire().await.expect("connection");
    llm_slot_checks::upsert(
        &mut conn,
        NewCheck {
            slot: "primary",
            fingerprint: &fingerprint.value,
            fingerprint_parts: &parts,
            passed: 22,
            total: 22,
            p95_ms: 1_000,
            report: &report,
        },
        Utc::now(),
    )
    .await
    .expect("slot check should be stored");
}

fn metric_value(text: &str, series: &str) -> Option<f64> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(' ')?;
        (name == series).then(|| value.parse().ok()).flatten()
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_llm_drops_the_verified_series_of_its_slot() {
    let db = support::isolated_db().await;
    let primary = slot();
    let effective = settings::effective_llm(
        &env_with(Some(primary.clone())),
        &[],
        &SecretCipher::new(None, None),
    );
    let handle = LlmHandle::new(LlmSnapshot::from_effective(effective));
    let health = LlmSlotHealth::new(handle.clone());
    store_passed_check(&db.pool, &primary).await;

    let statuses = health.statuses(&db.pool, Utc::now()).await;
    assert_eq!(statuses.len(), 1);
    assert_eq!(
        metric_value(
            &tasklerq::metrics::render(),
            "llm_slot_verified{slot=\"primary\"}"
        ),
        Some(1.0),
        "a passed slot must set its series before the removal"
    );

    // Настройки без слотов в окружении: выключение разбора убирает цепочку.
    let settings = LlmSettings::new(
        db.pool.clone(),
        env_with(None),
        SecretCipher::new(None, None),
        handle,
    );
    settings
        .apply(&[LlmChange::set("llm.enabled", json!(false))])
        .await
        .expect("capture is disabled");

    let statuses = health.statuses(&db.pool, Utc::now()).await;
    assert!(statuses.is_empty(), "a disabled capture has no slots");
    assert_eq!(
        metric_value(
            &tasklerq::metrics::render(),
            "llm_slot_verified{slot=\"primary\"}"
        ),
        None,
        "the series of a slot absent from the snapshot must be removed"
    );
}
