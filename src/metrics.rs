//! Метрики Prometheus: небольшой реестр без внешних зависимостей.
//!
//! Метки задаются только вызывающим кодом из фиксированных множеств: никаких
//! идентификаторов, текста сообщений и токенов. HTTP-маршрут передаётся
//! шаблоном (`MatchedPath`), а не сырым путём, чтобы кардинальность не росла.
//! Реестр живёт в процессе; гистограмма хранит счётчики по корзинам, сумму и
//! число наблюдений.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::{Mutex, OnceLock};

use chrono::Utc;
use sqlx::PgPool;

use crate::telegram::queue;

/// Корзины гистограммы латентности LLM в секундах.
const LATENCY_BUCKETS: [(&str, f64); 9] = [
    ("0.1", 0.1),
    ("0.25", 0.25),
    ("0.5", 0.5),
    ("1", 1.0),
    ("2", 2.0),
    ("5", 5.0),
    ("8", 8.0),
    ("13", 13.0),
    ("21", 21.0),
];

type Labels = Vec<(&'static str, String)>;
type Key = (&'static str, Labels);

#[derive(Debug, Default)]
struct Histogram {
    /// Число наблюдений в каждой корзине без накопления.
    buckets: [u64; LATENCY_BUCKETS.len()],
    sum: f64,
    count: u64,
}

#[derive(Debug, Default)]
struct Registry {
    counters: BTreeMap<Key, u64>,
    gauges: BTreeMap<Key, f64>,
    histograms: BTreeMap<Key, Histogram>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

fn lock() -> std::sync::MutexGuard<'static, Registry> {
    registry().lock().unwrap_or_else(|error| error.into_inner())
}

fn key(name: &'static str, labels: &[(&'static str, &str)]) -> Key {
    let mut labels: Labels = labels
        .iter()
        .map(|(label, value)| (*label, (*value).to_owned()))
        .collect();
    labels.sort_by(|left, right| left.0.cmp(right.0));
    (name, labels)
}

fn add_counter(name: &'static str, labels: &[(&'static str, &str)]) {
    *lock().counters.entry(key(name, labels)).or_default() += 1;
}

fn observe(name: &'static str, labels: &[(&'static str, &str)], value: f64) {
    let mut registry = lock();
    let histogram = registry.histograms.entry(key(name, labels)).or_default();
    histogram.count += 1;
    histogram.sum += value;
    if let Some(index) = LATENCY_BUCKETS
        .iter()
        .position(|(_, upper)| value <= *upper)
    {
        histogram.buckets[index] += 1;
    }
}

/// Исход обработки обновления Telegram.
pub fn telegram_update(status: &'static str) {
    add_counter("telegram_updates_total", &[("status", status)]);
}

/// Сообщение outbox переведено в dead-letter.
pub fn outbox_dead() {
    add_counter("outbox_dead_total", &[]);
}

/// Итог внешнего или Telegram-capture.
pub fn capture(source: &str, status: &str) {
    add_counter("capture_total", &[("source", source), ("status", status)]);
}

/// Capture создал позицию из исходного текста без LLM.
pub fn capture_fallback(source: &str) {
    add_counter("capture_fallback_total", &[("source", source)]);
}

/// Итог вызова слота LLM: `success` или `failure`.
pub fn llm_request(slot: &'static str, status: &'static str) {
    add_counter("llm_requests_total", &[("slot", slot), ("status", status)]);
}

/// Латентность вызова слота LLM, секунды.
pub fn llm_latency(slot: &'static str, seconds: f64) {
    observe("llm_latency_seconds", &[("slot", slot)], seconds.max(0.0));
}

/// Слот прошёл проверку smoke-набором: 1 — статус «прошёл», 0 — любой другой.
pub fn llm_slot_verified(slot: &'static str, verified: bool) {
    let value = if verified { 1.0 } else { 0.0 };
    lock()
        .gauges
        .insert(key("llm_slot_verified", &[("slot", slot)]), value);
}

/// Ответ LLM не прошёл разбор схемы.
pub fn llm_validation_failure() {
    add_counter("llm_validation_failures_total", &[]);
}

/// HTTP-запрос завершён; `route` — шаблон маршрута.
pub fn http_request(route: &str, status: u16) {
    let status = status.to_string();
    add_counter(
        "http_requests_total",
        &[("route", route), ("status", &status)],
    );
}

/// Webhook отклонён из-за неверного секрета.
pub fn webhook_auth_failure() {
    add_counter("webhook_auth_failures_total", &[]);
}

/// Webhook отклонён лимитом частоты; считается каждый отказ, в том числе
/// подавленный в аудите.
pub fn webhook_rate_limited() {
    add_counter("webhook_rate_limited_total", &[]);
}

/// Отказов аутентификации webhook в окне больше порога.
pub fn webhook_auth_failure_threshold_exceeded() {
    add_counter("webhook_auth_failure_threshold_exceeded_total", &[]);
}

/// Значения gauges, которые считаются запросом к базе при скрейпе.
struct DatabaseGauges {
    queue_lag: f64,
    outbox_pending: i64,
    pool_in_use: u32,
}

async fn database_gauges(pool: &PgPool) -> Result<DatabaseGauges, sqlx::Error> {
    let now = Utc::now();
    let mut conn = pool.acquire().await?;
    let oldest = queue::oldest_ready(&mut conn, now).await?;
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox WHERE status = 'pending'")
        .fetch_one(&mut *conn)
        .await?;
    drop(conn);

    let queue_lag = oldest.map_or(0.0, |oldest| {
        (now - oldest).num_milliseconds().max(0) as f64 / 1_000.0
    });
    let idle = u32::try_from(pool.num_idle()).unwrap_or(u32::MAX);
    Ok(DatabaseGauges {
        queue_lag,
        outbox_pending: pending,
        pool_in_use: pool.size().saturating_sub(idle),
    })
}

fn apply_database_gauges(registry: &mut Registry, gauges: DatabaseGauges) {
    registry
        .gauges
        .insert(key("telegram_queue_lag_seconds", &[]), gauges.queue_lag);
    registry.gauges.insert(
        key("outbox_pending", &[]),
        gauges.outbox_pending.max(0) as f64,
    );
    registry
        .gauges
        .insert(key("db_pool_in_use", &[]), f64::from(gauges.pool_in_use));
}

/// Текст в формате Prometheus exposition 0.0.4.
pub fn render() -> String {
    render_locked(&lock())
}

/// Обновить gauges из базы и сразу отрендерить всё под одним локом.
///
/// Два параллельных скрейпа не могут перезаписать gauges между подсчётом и
/// рендером: соседний скрейп ждёт блокировку до конца формирования ответа.
pub async fn render_with_database_gauges(pool: &PgPool) -> Result<String, sqlx::Error> {
    let gauges = database_gauges(pool).await?;
    let mut registry = lock();
    apply_database_gauges(&mut registry, gauges);
    Ok(render_locked(&registry))
}

fn render_locked(registry: &Registry) -> String {
    let mut output = String::new();

    render_simple(&mut output, "counter", &registry.counters);
    render_simple(&mut output, "gauge", &registry.gauges);
    render_histograms(&mut output, &registry.histograms);
    output
}

fn render_simple<T: std::fmt::Display>(output: &mut String, kind: &str, values: &BTreeMap<Key, T>) {
    let mut current = "";
    for ((name, labels), value) in values {
        if *name != current {
            write_family(output, name, kind);
            current = name;
        }
        let _ = writeln!(output, "{} {}", series(name, labels), value);
    }
}

fn render_histograms(output: &mut String, values: &BTreeMap<Key, Histogram>) {
    let mut current = "";
    for ((name, labels), histogram) in values {
        if *name != current {
            write_family(output, name, "histogram");
            current = name;
        }

        let mut cumulative = 0_u64;
        for (index, (le, _)) in LATENCY_BUCKETS.iter().enumerate() {
            cumulative += histogram.buckets[index];
            let _ = writeln!(
                output,
                "{}_bucket{} {}",
                name,
                labels_with(labels, "le", le),
                cumulative
            );
        }
        let _ = writeln!(
            output,
            "{}_bucket{} {}",
            name,
            labels_with(labels, "le", "+Inf"),
            histogram.count
        );
        let _ = writeln!(
            output,
            "{}_sum{} {}",
            name,
            label_block(labels),
            format_value(histogram.sum)
        );
        let _ = writeln!(
            output,
            "{}_count{} {}",
            name,
            label_block(labels),
            histogram.count
        );
    }
}

fn write_family(output: &mut String, name: &str, kind: &str) {
    let _ = writeln!(output, "# HELP {name} {}", help(name));
    let _ = writeln!(output, "# TYPE {name} {kind}");
}

fn help(name: &str) -> &'static str {
    match name {
        "telegram_updates_total" => "Telegram updates handled by the worker.",
        "telegram_queue_lag_seconds" => "Age of the oldest ready pending Telegram update.",
        "outbox_pending" => "Outbox messages waiting for delivery.",
        "outbox_dead_total" => "Outbox messages moved to the dead-letter queue.",
        "capture_total" => "Capture outcomes by source and status.",
        "capture_fallback_total" => "Captures created from raw text without the LLM.",
        "llm_requests_total" => "LLM slot capture requests by slot and outcome.",
        "llm_latency_seconds" => "LLM slot capture request latency by slot.",
        "llm_slot_verified" => "LLM slot passed the latest smoke-set check (1) or not (0).",
        "llm_validation_failures_total" => "LLM responses rejected by schema validation.",
        "http_requests_total" => "HTTP requests by matched route template and status.",
        "db_pool_in_use" => "Database pool connections currently checked out.",
        "webhook_auth_failures_total" => "Telegram webhook requests rejected by the secret.",
        "webhook_rate_limited_total" => {
            "Telegram webhook requests rejected by the per-client rate limit."
        }
        "webhook_auth_failure_threshold_exceeded_total" => {
            "Webhook auth failures per 5-minute window above the alerting threshold."
        }
        _ => "Tasklerq metric.",
    }
}

fn series(name: &str, labels: &[(&'static str, String)]) -> String {
    format!("{name}{}", label_block(labels))
}

fn label_block(labels: &[(&'static str, String)]) -> String {
    if labels.is_empty() {
        return String::new();
    }
    let pairs: Vec<String> = labels
        .iter()
        .map(|(label, value)| format!("{label}=\"{}\"", escape(value)))
        .collect();
    format!("{{{}}}", pairs.join(","))
}

fn labels_with(labels: &[(&'static str, String)], extra: &'static str, value: &str) -> String {
    let mut pairs: Vec<String> = labels
        .iter()
        .map(|(label, value)| format!("{label}=\"{}\"", escape(value)))
        .collect();
    pairs.push(format!("{extra}=\"{}\"", escape(value)));
    format!("{{{}}}", pairs.join(","))
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn format_value(value: f64) -> String {
    if !value.is_finite() {
        return "0".to_owned();
    }
    let rounded = (value * 1_000.0).round() / 1_000.0;
    if rounded.fract() == 0.0 {
        format!("{}", rounded as i64)
    } else {
        format!("{rounded}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_labels_render_in_prometheus_format() {
        // Уникальные метки: глобальный реестр делят все unit-тесты процесса.
        capture("unit-counter", "applied");
        let rendered = render();
        assert!(
            rendered.contains("# TYPE capture_total counter"),
            "{rendered}"
        );
        assert!(
            rendered.contains("capture_total{source=\"unit-counter\",status=\"applied\"} 1"),
            "{rendered}"
        );
    }

    #[test]
    fn histograms_render_cumulative_buckets_sum_and_count() {
        let labels = [("test", "unit-histogram")];
        observe("llm_latency_seconds", &labels, 0.3);
        observe("llm_latency_seconds", &labels, 9.0);
        let rendered = render();

        assert!(
            rendered.contains("llm_latency_seconds_bucket{test=\"unit-histogram\",le=\"0.5\"} 1"),
            "{rendered}"
        );
        assert!(
            rendered.contains("llm_latency_seconds_bucket{test=\"unit-histogram\",le=\"13\"} 2"),
            "{rendered}"
        );
        assert!(
            rendered.contains("llm_latency_seconds_bucket{test=\"unit-histogram\",le=\"+Inf\"} 2"),
            "{rendered}"
        );
        assert!(
            rendered.contains("llm_latency_seconds_count{test=\"unit-histogram\"} 2"),
            "{rendered}"
        );
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }
}
