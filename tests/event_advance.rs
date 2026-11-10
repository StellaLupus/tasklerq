//! Автопереход повторяющегося события на следующее вхождение в тике
//! планировщика.

mod support;

use std::sync::Arc;

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use sqlx::PgPool;
use support::{TestDb, TestHousehold};
use tasklerq::domain::models::{Item, ListKind, RecurrenceFreq, RecurrenceInput};
use tasklerq::domain::recurrence::Due;
use tasklerq::jobs::scheduler::Scheduler;
use tasklerq::storage::repositories::{items, lists};
use tokio::sync::Notify;
use uuid::Uuid;

fn scheduler(pool: &PgPool) -> Scheduler {
    Scheduler::new(pool.clone(), Arc::new(Notify::new()))
}

fn utc(year: i32, month: u32, day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, 0, 0)
        .single()
        .expect("fixed moment")
}

async fn create_event(
    db: &TestDb,
    household: &TestHousehold,
    title: &str,
    due_at: DateTime<Utc>,
    end_at: DateTime<Utc>,
    recurrence: RecurrenceInput,
) -> Item {
    let mut item = support::new_item(household.events_list_id, title);
    item.due_at = Some(due_at);
    item.end_at = Some(end_at);
    item.recurrence = Some(recurrence);
    support::create_item(
        &db.pool,
        household,
        household.events_list_id,
        &household.owner(),
        item,
    )
    .await
}

async fn item_events_for(pool: &PgPool, item_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM item_events WHERE item_id = $1")
        .bind(item_id)
        .fetch_one(pool)
        .await
        .expect("item events")
}

async fn deliveries_for(pool: &PgPool, item_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM notification_deliveries WHERE item_id = $1")
        .bind(item_id)
        .fetch_one(pool)
        .await
        .expect("deliveries")
}

async fn reload(pool: &PgPool, household: &TestHousehold, item_id: Uuid) -> Item {
    let mut conn = pool.acquire().await.expect("connection");
    tasklerq::storage::repositories::items::get(&mut conn, household.household_id, item_id)
        .await
        .expect("item")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_weekly_event_moves_to_the_next_week_without_journal_records() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    // Вторник 10 ноября 2026, 18:00–19:00 по Москве.
    let start = utc(2026, 11, 10, 15);
    let end = utc(2026, 11, 10, 16);
    let event = create_event(
        &db,
        &household,
        "Тренировка",
        start,
        end,
        support::recurrence(RecurrenceFreq::Weekly, vec![2]),
    )
    .await;
    let events_before = item_events_for(&db.pool, event.id).await;

    scheduler(&db.pool)
        .tick(end)
        .await
        .expect("tick advances the event");

    let current = reload(&db.pool, &household, event.id).await;
    assert_eq!(current.due_at, Some(start + Duration::days(7)));
    assert_eq!(current.end_at, Some(end + Duration::days(7)));
    assert_eq!(
        current.version,
        event.version + 1,
        "one transition moves the version by exactly one"
    );
    assert_eq!(
        item_events_for(&db.pool, event.id).await,
        events_before,
        "the transition must not write the item journal"
    );
    assert_eq!(
        deliveries_for(&db.pool, event.id).await,
        0,
        "the transition must not create deliveries"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_weeks_of_downtime_cause_a_single_transition() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let start = utc(2026, 11, 3, 15);
    let end = utc(2026, 11, 3, 16);
    let event = create_event(
        &db,
        &household,
        "Тренировка",
        start,
        end,
        support::recurrence(RecurrenceFreq::Weekly, vec![2]),
    )
    .await;

    // Вторник 24 ноября, 20:00 по Москве: три недели вхождений уже прошли.
    let now = utc(2026, 11, 24, 17);
    scheduler(&db.pool)
        .tick(now)
        .await
        .expect("tick skips the missed occurrences");

    let current = reload(&db.pool, &household, event.id).await;
    assert_eq!(
        current.due_at,
        Some(utc(2026, 12, 1, 15)),
        "the event stands on the first occurrence after now"
    );
    assert_eq!(current.end_at, Some(utc(2026, 12, 1, 16)));
    assert_eq!(
        current.version,
        event.version + 1,
        "missed occurrences are skipped at once, not one by one"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_series_with_until_stays_on_its_last_occurrence() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let start = utc(2026, 11, 3, 15);
    let end = utc(2026, 11, 3, 16);
    let mut rule = support::recurrence(RecurrenceFreq::Weekly, vec![2]);
    rule.until = Some(NaiveDate::from_ymd_opt(2026, 11, 17).expect("date"));
    let event = create_event(&db, &household, "Тренировка", start, end, rule).await;

    // Вторник 17 ноября, 20:00 по Москве: последнее вхождение закончилось.
    let now = utc(2026, 11, 17, 17);
    scheduler(&db.pool)
        .tick(now)
        .await
        .expect("tick moves the series to its last occurrence");

    let last_start = utc(2026, 11, 17, 15);
    let last_end = utc(2026, 11, 17, 16);
    let current = reload(&db.pool, &household, event.id).await;
    assert_eq!(
        current.due_at,
        Some(last_start),
        "after a downtime the series lands on its last occurrence"
    );
    assert_eq!(current.end_at, Some(last_end), "the end is shifted too");
    assert_eq!(current.version, event.version + 1);

    scheduler(&db.pool)
        .tick(now)
        .await
        .expect("a second tick leaves the finished series alone");
    let current = reload(&db.pool, &household, event.id).await;
    assert_eq!(current.due_at, Some(last_start));
    assert_eq!(
        current.version,
        event.version + 1,
        "the second tick is a no-op"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_series_already_on_its_last_occurrence_does_not_change() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let start = utc(2026, 11, 17, 15);
    let end = utc(2026, 11, 17, 16);
    let mut rule = support::recurrence(RecurrenceFreq::Weekly, vec![2]);
    rule.until = Some(NaiveDate::from_ymd_opt(2026, 11, 17).expect("date"));
    let event = create_event(&db, &household, "Тренировка", start, end, rule).await;

    // Среда 18 ноября: последнее вхождение уже прошло.
    scheduler(&db.pool)
        .tick(utc(2026, 11, 18, 12))
        .await
        .expect("tick leaves the finished series on its last occurrence");

    let current = reload(&db.pool, &household, event.id).await;
    assert_eq!(current.due_at, Some(start));
    assert_eq!(current.end_at, Some(end));
    assert_eq!(
        current.version, event.version,
        "a finished series must not change the row"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_moved_occurrence_returns_to_the_rule_and_clears_the_slot() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    // Вторник 10 ноября 2026, 18:00–19:00 по Москве.
    let start = utc(2026, 11, 10, 15);
    let end = utc(2026, 11, 10, 16);
    let event = create_event(
        &db,
        &household,
        "Тренировка",
        start,
        end,
        support::recurrence(RecurrenceFreq::Weekly, vec![2]),
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        event.id,
        event.version,
        // Среда 11 ноября, 19:00 по Москве.
        Due::Timed(utc(2026, 11, 11, 16)),
        "Europe/Moscow",
        utc(2026, 11, 10, 12),
    )
    .await
    .expect("reschedule the nearest occurrence");
    drop(conn);

    let rule = moved.recurrence.as_ref().expect("rule");
    assert_eq!(
        rule.occurrence_date,
        Some(NaiveDate::from_ymd_opt(2026, 11, 10).expect("date"))
    );
    assert_eq!(
        rule.occurrence_time,
        Some(NaiveTime::from_hms_opt(18, 0, 0).expect("time"))
    );

    // Среда 11 ноября, 21:00 по Москве: перенесённое вхождение закончилось.
    scheduler(&db.pool)
        .tick(utc(2026, 11, 11, 18))
        .await
        .expect("tick returns the event to the rule");

    let current = reload(&db.pool, &household, event.id).await;
    assert_eq!(
        current.due_at,
        Some(utc(2026, 11, 17, 15)),
        "the next occurrence is the next Tuesday at 18:00"
    );
    assert_eq!(current.end_at, Some(utc(2026, 11, 17, 16)));
    let rule = current.recurrence.as_ref().expect("rule");
    assert_eq!(
        rule.occurrence_date, None,
        "the moved occurrence slot must be cleared"
    );
    assert_eq!(rule.occurrence_time, None);
    assert_eq!(current.version, moved.version + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ongoing_event_is_not_advanced() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let start = utc(2026, 11, 10, 15);
    let end = utc(2026, 11, 10, 16);
    let event = create_event(
        &db,
        &household,
        "Тренировка",
        start,
        end,
        support::recurrence(RecurrenceFreq::Weekly, vec![2]),
    )
    .await;

    scheduler(&db.pool)
        .tick(start)
        .await
        .expect("tick during the occurrence");

    let current = reload(&db.pool, &household, event.id).await;
    assert_eq!(current.due_at, Some(start));
    assert_eq!(current.version, event.version);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archived_events_list_is_not_advanced() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let start = utc(2026, 11, 10, 15);
    let end = utc(2026, 11, 10, 16);
    let mut conn = db.pool.acquire().await.expect("connection");
    let list = lists::create(
        &mut conn,
        household.household_id,
        ListKind::Events,
        "Другой календарь",
        Utc::now(),
    )
    .await
    .expect("second events list");
    drop(conn);
    let mut item = support::new_item(list.id, "Тренировка");
    item.due_at = Some(start);
    item.end_at = Some(end);
    item.recurrence = Some(support::recurrence(RecurrenceFreq::Weekly, vec![2]));
    let event = support::create_item(&db.pool, &household, list.id, &household.owner(), item).await;
    sqlx::query("UPDATE lists SET is_archived = true WHERE id = $1")
        .bind(list.id)
        .execute(&db.pool)
        .await
        .expect("archive");

    scheduler(&db.pool)
        .tick(end)
        .await
        .expect("tick skips archived lists");

    let current = reload(&db.pool, &household, event.id).await;
    assert_eq!(current.due_at, Some(start));
    assert_eq!(current.version, event.version);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_all_day_event_advance_keeps_the_inclusive_multi_day_length() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let start = NaiveDate::from_ymd_opt(2027, 3, 1).expect("date");
    let end = NaiveDate::from_ymd_opt(2027, 3, 3).expect("date");
    let mut item = support::new_item(household.events_list_id, "Отчётный период");
    item.due_date = Some(start);
    item.end_date = Some(end);
    item.recurrence = Some(support::recurrence(RecurrenceFreq::Monthly, Vec::new()));
    let event = support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &household.owner(),
        item,
    )
    .await;

    // 4 марта 03:00 по Москве: вседневное вхождение 1–3 марта закончилось.
    scheduler(&db.pool)
        .tick(utc(2027, 3, 4, 0))
        .await
        .expect("tick advances the all-day event");

    let current = reload(&db.pool, &household, event.id).await;
    assert_eq!(
        current.due_date,
        Some(NaiveDate::from_ymd_opt(2027, 4, 1).expect("date"))
    );
    assert_eq!(
        current.end_date,
        Some(NaiveDate::from_ymd_opt(2027, 4, 3).expect("date")),
        "the next occurrence also occupies three days"
    );
    assert_eq!(current.version, event.version + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daily_all_day_event_moves_to_the_occurrence_that_just_started() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let start = NaiveDate::from_ymd_opt(2026, 3, 1).expect("date");
    let mut item = support::new_item(household.events_list_id, "Зарядка");
    item.due_date = Some(start);
    item.recurrence = Some(support::recurrence(RecurrenceFreq::Daily, Vec::new()));
    let event = support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &household.owner(),
        item,
    )
    .await;

    // 2 марта 00:00:30 по Москве: вхождение 1 марта закончилось, а вхождение
    // 2 марта уже началось.
    scheduler(&db.pool)
        .tick(utc(2026, 3, 1, 21) + Duration::seconds(30))
        .await
        .expect("tick advances to the ongoing occurrence");

    let current = reload(&db.pool, &household, event.id).await;
    assert_eq!(
        current.due_date,
        Some(NaiveDate::from_ymd_opt(2026, 3, 2).expect("date")),
        "the occurrence that has already started today is not skipped"
    );
    assert_eq!(current.end_date, None);
    assert_eq!(current.version, event.version + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_weekly_event_after_downtime_lands_on_the_occurrence_in_progress() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let start = utc(2026, 11, 3, 15);
    let end = utc(2026, 11, 3, 16);
    let event = create_event(
        &db,
        &household,
        "Тренировка",
        start,
        end,
        support::recurrence(RecurrenceFreq::Weekly, vec![2]),
    )
    .await;

    // Вторник 10 ноября, 18:30 по Москве: недельное вхождение идёт с 18:00.
    scheduler(&db.pool)
        .tick(utc(2026, 11, 10, 15) + Duration::minutes(30))
        .await
        .expect("tick lands on the ongoing occurrence");

    let current = reload(&db.pool, &household, event.id).await;
    assert_eq!(current.due_at, Some(utc(2026, 11, 10, 15)));
    assert_eq!(current.end_at, Some(utc(2026, 11, 10, 16)));
    assert_eq!(current.version, event.version + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recurring_event_without_an_end_keeps_the_implicit_duration() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let start = utc(2026, 11, 10, 15);
    let mut item = support::new_item(household.events_list_id, "Тренировка");
    item.due_at = Some(start);
    item.recurrence = Some(support::recurrence(RecurrenceFreq::Daily, Vec::new()));
    let event = support::create_item(
        &db.pool,
        &household,
        household.events_list_id,
        &household.owner(),
        item,
    )
    .await;

    // Без явного конца вхождение длится час: тик ровно в конце.
    scheduler(&db.pool)
        .tick(start + Duration::hours(1))
        .await
        .expect("tick advances the event without an end");

    let current = reload(&db.pool, &household, event.id).await;
    assert_eq!(current.due_at, Some(start + Duration::days(1)));
    assert_eq!(current.end_at, None);
    assert_eq!(current.version, event.version + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_weekly_event_advances_with_local_time_across_the_dst_change() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    sqlx::query("UPDATE households SET timezone = 'America/New_York' WHERE id = $1")
        .bind(household.household_id)
        .execute(&db.pool)
        .await
        .expect("household timezone");
    let new_york: chrono_tz::Tz = "America/New_York".parse().expect("timezone");
    // Вторник 27 октября 2026, 18:00–19:00 по Нью-Йорку, ещё летнее время.
    let start = utc(2026, 10, 27, 22);
    let end = utc(2026, 10, 27, 23);
    let mut rule = support::recurrence(RecurrenceFreq::Weekly, vec![2]);
    rule.timezone = Some("America/New_York".into());
    let event = create_event(&db, &household, "Тренировка", start, end, rule).await;

    // Среда 4 ноября 00:00 UTC: вхождение вторника 3 ноября уже закончилось,
    // после перехода на зимнее время оно шло с 18:00 до 19:00 EST.
    scheduler(&db.pool)
        .tick(utc(2026, 11, 4, 0))
        .await
        .expect("tick advances across the offset change");

    let current = reload(&db.pool, &household, event.id).await;
    let next_start = current.due_at.expect("next start");
    let next_end = current.end_at.expect("next end");
    assert_eq!(
        next_start.with_timezone(&new_york).time(),
        chrono::NaiveTime::from_hms_opt(18, 0, 0).unwrap()
    );
    assert_eq!(
        next_end.with_timezone(&new_york).time(),
        chrono::NaiveTime::from_hms_opt(19, 0, 0).unwrap()
    );
    assert_eq!(current.version, event.version + 1);
}
