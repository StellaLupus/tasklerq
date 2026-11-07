//! Интеграционные тесты жизненного цикла элементов.

mod support;

use chrono::{Datelike, Duration, NaiveDate, SubsecRound, TimeZone, Utc};
use tasklerq::domain::error::DomainError;
use tasklerq::domain::models::{
    EventType, Field, ItemPatch, ItemStatus, ListKind, MonthRule, RecurrenceFreq,
};
use tasklerq::domain::recurrence::Due;
use tasklerq::storage::repositories::{items, lists};

fn date(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).expect("valid date")
}

fn utc(year: i32, month: u32, day: u32, hour: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, 0, 0)
        .single()
        .expect("fixed moment")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn version_conflict_returns_current_version() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Версия"),
    )
    .await;
    assert_eq!(created.version, 1);

    let mut conn = db.pool.acquire().await.expect("connection");
    let patch = ItemPatch {
        title: Some("Новое имя".into()),
        ..ItemPatch::default()
    };
    let updated = items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        patch.clone(),
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("first update should win");
    assert_eq!(updated.version, 2);
    assert_eq!(updated.title, "Новое имя");

    let stale = items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        patch,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect_err("stale version must fail");
    match stale {
        DomainError::Conflict { details, .. } => {
            assert_eq!(details["current_version"], 2);
            assert_eq!(details["item"]["title"], "Новое имя");
        }
        other => panic!("expected conflict, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_and_reopen_are_idempotent_and_restore_due() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let today = Utc::now().date_naive();
    let mut input = support::new_item(household.tasks_list_id, "Купить хлеб");
    input.due_date = Some(today);
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("complete");
    assert_eq!(completed.status, ItemStatus::Done);
    assert!(completed.completed_at.is_some());
    assert_eq!(completed.version, 2);

    let repeated = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        None,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("repeated complete is idempotent");
    assert_eq!(repeated.version, 2);
    assert_eq!(repeated.completed_at, completed.completed_at);

    let reopened = items::reopen(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(repeated.version),
        Utc::now(),
    )
    .await
    .expect("reopen");
    assert_eq!(reopened.status, ItemStatus::Open);
    assert_eq!(reopened.due_date, Some(today));
    assert!(reopened.completed_at.is_none());
    assert_eq!(reopened.version, 3);

    let repeated_reopen = items::reopen(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        None,
        Utc::now(),
    )
    .await
    .expect("repeated reopen is idempotent");
    assert_eq!(repeated_reopen.version, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recurring_completion_moves_due_date_forward_once() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let today = Utc::now().date_naive();
    let weekday = today.weekday().number_from_monday() as i16;
    let mut input = support::new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(today);
    input.recurrence = Some(support::recurrence(
        tasklerq::domain::models::RecurrenceFreq::Weekly,
        vec![weekday],
    ));
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        None,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("complete recurring");
    assert_eq!(completed.status, ItemStatus::Open);
    assert!(completed.completed_at.is_some());
    assert_eq!(completed.due_date, Some(today + Duration::days(7)));
    assert_eq!(completed.version, 2);

    let repeated = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        None,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("repeated complete");
    assert_eq!(repeated.version, 2);
    assert_eq!(repeated.due_date, Some(today + Duration::days(7)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_after_reschedule_records_a_new_occurrence() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let today = date(2027, 3, 15);
    let mut input = support::new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(today);
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Daily, Vec::new()));
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let first = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        utc(2027, 3, 15, 9),
    )
    .await
    .expect("first completion");
    assert_eq!(first.due_date, Some(today + Duration::days(1)));
    assert!(first.completed_at.is_some());

    // «Завтра» после выполнения уже стоит на завтрашней дате: выбор ничего не
    // меняет и версию не двигает. Это не повторное нажатие «Выполнено», а
    // отдельное действие, после которого выполнение должно записаться.
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        first.version,
        Due::AllDay(today + Duration::days(1)),
        "Europe/Moscow",
        utc(2027, 3, 15, 10),
    )
    .await
    .expect("reschedule to tomorrow");
    assert_eq!(moved.version, first.version);

    let second = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(moved.version),
        "Europe/Moscow",
        utc(2027, 3, 15, 11),
    )
    .await
    .expect("second completion");
    assert_eq!(second.due_date, Some(today + Duration::days(2)));
    assert_eq!(second.version, moved.version + 1);

    let events = tasklerq::storage::repositories::events::for_item(
        &mut conn,
        household.household_id,
        created.id,
    )
    .await
    .expect("events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == EventType::Completed)
            .count(),
        2,
        "both completions are recorded"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reschedule_occurrence_keeps_the_series_anchor() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let anchor = date(2025, 11, 5);
    let mut input = support::new_item(household.tasks_list_id, "Оплатить счета");
    input.due_date = Some(anchor);
    let mut rule = support::recurrence(RecurrenceFreq::Monthly, Vec::new());
    rule.month_rule = Some(MonthRule::AnchorDay);
    input.recurrence = Some(rule);
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let now = utc(2025, 11, 7, 12);
    let mut conn = db.pool.acquire().await.expect("connection");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        Due::AllDay(date(2025, 11, 7)),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("reschedule occurrence");

    assert_eq!(moved.due_date, Some(date(2025, 11, 7)));
    assert_eq!(moved.due_at, None);
    assert_eq!(moved.version, created.version + 1);
    let moved_rule = moved.recurrence.as_ref().expect("rule is kept");
    assert_eq!(moved_rule.freq, RecurrenceFreq::Monthly);
    assert_eq!(moved_rule.month_rule, Some(MonthRule::AnchorDay));
    assert_eq!(moved_rule.anchor_date, anchor, "the series anchor is kept");

    let history = tasklerq::storage::repositories::events::for_item(
        &mut conn,
        household.household_id,
        created.id,
    )
    .await
    .expect("events");
    let updated: Vec<_> = history
        .iter()
        .filter(|event| event.event_type == EventType::Updated)
        .collect();
    assert_eq!(updated.len(), 1);
    assert_eq!(
        updated[0].payload["before"]["due_date"],
        serde_json::json!("2025-11-05")
    );
    assert_eq!(
        updated[0].payload["after"]["due_date"],
        serde_json::json!("2025-11-07")
    );

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(moved.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete rescheduled occurrence");
    assert_eq!(completed.due_date, Some(date(2025, 12, 5)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reschedule_occurrence_past_the_next_occurrence_stays_on_the_rule() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut input = support::new_item(household.tasks_list_id, "Вынести мусор");
    input.due_date = Some(date(2025, 11, 4));
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Weekly, vec![2]));
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let now = utc(2025, 11, 12, 12);
    let mut conn = db.pool.acquire().await.expect("connection");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        Due::AllDay(date(2025, 11, 12)),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("move past the next occurrence");
    assert_eq!(moved.due_date, Some(date(2025, 11, 12)));
    assert_eq!(
        moved.recurrence.as_ref().expect("rule").anchor_date,
        date(2025, 11, 4)
    );

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(moved.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete on the new date");
    assert_eq!(
        completed.due_date,
        Some(date(2025, 11, 18)),
        "the next occurrence is the Tuesday after the moved date"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reschedule_occurrence_matches_update_without_recurrence() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let old_due = date(2025, 11, 5);
    let new_due = date(2025, 11, 7);
    let mut left = support::new_item(household.tasks_list_id, "Одинаковая задача");
    left.due_date = Some(old_due);
    let left = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        left,
    )
    .await;
    let mut right = support::new_item(household.tasks_list_id, "Одинаковая задача");
    right.due_date = Some(old_due);
    let right = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        right,
    )
    .await;

    let now = utc(2025, 11, 6, 12);
    let mut conn = db.pool.acquire().await.expect("connection");
    // Позиция в списке зависит от порядка создания, а не от переноса срока.
    sqlx::query("UPDATE items SET position = 0 WHERE id = ANY($1)")
        .bind(vec![left.id, right.id])
        .execute(&mut *conn)
        .await
        .expect("align positions");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        left.id,
        left.version,
        Due::AllDay(new_due),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("reschedule");
    let updated = items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        right.id,
        right.version,
        ItemPatch {
            due_date: Field::Value(new_due),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        now,
    )
    .await
    .expect("update");

    assert_eq!(moved.due_at, updated.due_at);
    assert_eq!(moved.due_date, updated.due_date);
    assert_eq!(moved.version, updated.version);
    assert_eq!(moved.status, updated.status);
    assert_eq!(moved.recurrence, updated.recurrence);

    let moved_event = tasklerq::storage::repositories::events::for_item(
        &mut conn,
        household.household_id,
        left.id,
    )
    .await
    .expect("moved events")
    .into_iter()
    .find(|event| event.event_type == EventType::Updated)
    .expect("moved event");
    let updated_event = tasklerq::storage::repositories::events::for_item(
        &mut conn,
        household.household_id,
        right.id,
    )
    .await
    .expect("updated events")
    .into_iter()
    .find(|event| event.event_type == EventType::Updated)
    .expect("updated event");

    let normalize = |mut payload: serde_json::Value| {
        for key in ["before", "after"] {
            let item = payload[key].as_object_mut().expect("item snapshot");
            item.insert("id".into(), serde_json::Value::Null);
            item.insert("created_at".into(), serde_json::Value::Null);
            item.insert("updated_at".into(), serde_json::Value::Null);
        }
        payload
    };
    assert_eq!(
        normalize(moved_event.payload),
        normalize(updated_event.payload),
        "rescheduling a task without recurrence writes the same event as update"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reschedule_occurrence_rejects_a_stale_version() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut input = support::new_item(household.tasks_list_id, "Версия переноса");
    input.due_date = Some(date(2025, 11, 5));
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Monthly, Vec::new()));
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let error = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version + 1,
        Due::AllDay(date(2025, 11, 7)),
        "Europe/Moscow",
        utc(2025, 11, 6, 12),
    )
    .await
    .expect_err("stale version must fail");
    match error {
        DomainError::Conflict { details, .. } => {
            assert_eq!(details["current_version"], created.version);
            assert_eq!(details["item"]["title"], "Версия переноса");
        }
        other => panic!("expected conflict, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reschedule_occurrence_does_not_normalize_last_day() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut input = support::new_item(household.tasks_list_id, "Отчёт за месяц");
    input.due_date = Some(date(2025, 10, 31));
    let mut rule = support::recurrence(RecurrenceFreq::Monthly, Vec::new());
    rule.month_rule = Some(MonthRule::LastDay);
    input.recurrence = Some(rule);
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let now = utc(2025, 11, 7, 12);
    let mut conn = db.pool.acquire().await.expect("connection");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        Due::AllDay(date(2025, 11, 7)),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("reschedule last-day occurrence");
    assert_eq!(
        moved.due_date,
        Some(date(2025, 11, 7)),
        "the moved due date is not normalized to the month end"
    );
    assert_eq!(
        moved.recurrence.as_ref().expect("rule").month_rule,
        Some(MonthRule::LastDay)
    );

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(moved.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    assert_eq!(completed.due_date, Some(date(2025, 11, 30)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reschedule_occurrence_earlier_than_its_slot_keeps_the_slot() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let anchor = date(2026, 11, 10);
    let mut input = support::new_item(household.tasks_list_id, "Вынести мусор");
    input.due_date = Some(anchor);
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Weekly, vec![2]));
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let now = utc(2026, 11, 6, 12);
    let mut conn = db.pool.acquire().await.expect("connection");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        Due::AllDay(date(2026, 11, 6)),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("reschedule earlier than the slot");
    assert_eq!(moved.due_date, Some(date(2026, 11, 6)));
    let moved_rule = moved.recurrence.as_ref().expect("rule");
    assert_eq!(moved_rule.occurrence_date, Some(anchor));
    assert_eq!(moved_rule.anchor_date, anchor);

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(moved.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete before the slot");
    assert_eq!(completed.due_date, Some(date(2026, 11, 17)));
    assert_eq!(
        completed.recurrence.as_ref().expect("rule").occurrence_date,
        None,
        "completion clears the slot"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reschedule_occurrence_of_an_overdue_slot_moves_to_today() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let anchor = date(2026, 11, 3);
    let mut input = support::new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(anchor);
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Weekly, vec![2]));
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let now = utc(2026, 11, 6, 12);
    let mut conn = db.pool.acquire().await.expect("connection");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        Due::AllDay(date(2026, 11, 6)),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("reschedule overdue occurrence");
    assert_eq!(
        moved.recurrence.as_ref().expect("rule").occurrence_date,
        Some(anchor)
    );

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(moved.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete overdue occurrence");
    assert_eq!(completed.due_date, Some(date(2026, 11, 10)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_reschedule_keeps_the_first_slot() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let anchor = date(2026, 11, 10);
    let mut input = support::new_item(household.tasks_list_id, "Проверить почту");
    input.due_date = Some(anchor);
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Weekly, vec![2]));
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let first = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        Due::AllDay(date(2026, 11, 6)),
        "Europe/Moscow",
        utc(2026, 11, 6, 12),
    )
    .await
    .expect("first reschedule");
    assert_eq!(
        first.recurrence.as_ref().expect("rule").occurrence_date,
        Some(anchor)
    );

    let second = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        first.version,
        Due::AllDay(date(2026, 11, 3)),
        "Europe/Moscow",
        utc(2026, 11, 3, 12),
    )
    .await
    .expect("second reschedule");
    assert_eq!(second.due_date, Some(date(2026, 11, 3)));
    assert_eq!(
        second.recurrence.as_ref().expect("rule").occurrence_date,
        Some(anchor)
    );

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(second.version),
        "Europe/Moscow",
        utc(2026, 11, 3, 12),
    )
    .await
    .expect("complete after repeated reschedule");
    assert_eq!(completed.due_date, Some(date(2026, 11, 17)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reschedule_back_to_the_slot_clears_it() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let anchor = date(2026, 11, 10);
    let mut input = support::new_item(household.tasks_list_id, "Оплатить счета");
    input.due_date = Some(anchor);
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Weekly, vec![2]));
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let now = utc(2026, 11, 6, 12);
    let mut conn = db.pool.acquire().await.expect("connection");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        Due::AllDay(date(2026, 11, 6)),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("move away from the slot");
    assert_eq!(
        moved.recurrence.as_ref().expect("rule").occurrence_date,
        Some(anchor)
    );

    let returned = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        moved.version,
        Due::AllDay(anchor),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("move back to the slot");
    assert_eq!(returned.due_date, Some(anchor));
    assert_eq!(
        returned.recurrence.as_ref().expect("rule").occurrence_date,
        None,
        "returning to the slot clears it"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_due_clears_the_slot() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let anchor = date(2026, 11, 10);
    let mut input = support::new_item(household.tasks_list_id, "Сдать отчёт");
    input.due_date = Some(anchor);
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Weekly, vec![2]));
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let now = utc(2026, 11, 6, 12);
    let mut conn = db.pool.acquire().await.expect("connection");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        Due::AllDay(date(2026, 11, 6)),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("reschedule occurrence");
    assert_eq!(
        moved.recurrence.as_ref().expect("rule").occurrence_date,
        Some(anchor)
    );

    let updated = items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        moved.version,
        ItemPatch {
            due_date: Field::Value(date(2026, 11, 7)),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        now,
    )
    .await
    .expect("update due");
    assert_eq!(
        updated.recurrence.as_ref().expect("rule").occurrence_date,
        None
    );
    assert_eq!(
        updated.recurrence.as_ref().expect("rule").anchor_date,
        date(2026, 11, 7),
        "a real due change reanchors the series"
    );
    assert!(updated.version > moved.version);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_recurrence_without_a_new_due_clears_the_slot() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let anchor = date(2026, 11, 10);
    let mut input = support::new_item(household.tasks_list_id, "Проверить почту");
    input.due_date = Some(anchor);
    let mut rule = support::recurrence(RecurrenceFreq::Weekly, vec![2]);
    rule.anchor_date = Some(anchor);
    input.recurrence = Some(rule);
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let now = utc(2026, 11, 6, 12);
    let mut conn = db.pool.acquire().await.expect("connection");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        Due::AllDay(date(2026, 11, 6)),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("reschedule occurrence");
    assert_eq!(
        moved.recurrence.as_ref().expect("rule").occurrence_date,
        Some(anchor)
    );

    let mut changed = support::recurrence(RecurrenceFreq::Weekly, vec![3]);
    changed.anchor_date = Some(anchor);
    let updated = items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        moved.version,
        ItemPatch {
            recurrence: Field::Value(changed),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        now,
    )
    .await
    .expect("patch the rule without changing the due date");
    assert_eq!(updated.due_date, Some(date(2026, 11, 6)));
    assert_eq!(
        updated.recurrence.as_ref().expect("rule").occurrence_date,
        None,
        "a patched rule with other weekdays clears the slot"
    );
    assert!(updated.version > moved.version);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_repeating_the_current_due_keeps_the_slot() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let anchor = date(2026, 11, 3);
    let slot = date(2026, 11, 10);
    let moved_to = date(2026, 11, 6);
    let mut input = support::new_item(household.tasks_list_id, "Проверить почту");
    input.due_date = Some(slot);
    let mut rule = support::recurrence(RecurrenceFreq::Weekly, vec![2]);
    rule.anchor_date = Some(anchor);
    input.recurrence = Some(rule);
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let now = utc(2026, 11, 6, 12);
    let mut conn = db.pool.acquire().await.expect("connection");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        Due::AllDay(moved_to),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("reschedule occurrence");
    assert_eq!(
        moved.recurrence.as_ref().expect("rule").occurrence_date,
        Some(slot)
    );
    assert_eq!(moved.recurrence.as_ref().expect("rule").anchor_date, anchor);

    let updated = items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        moved.version,
        ItemPatch {
            due_date: Field::Value(moved_to),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        now,
    )
    .await
    .expect("repeat the current due");
    assert_eq!(updated.version, moved.version);
    assert_eq!(updated.due_date, Some(moved_to));
    let rule = updated.recurrence.as_ref().expect("rule");
    assert_eq!(rule.occurrence_date, Some(slot));
    assert_eq!(rule.anchor_date, anchor);

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        updated.id,
        Some(updated.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete after a repeated due");
    assert_eq!(completed.due_date, Some(date(2026, 11, 17)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_repeating_the_current_rule_keeps_the_slot() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let anchor = date(2026, 11, 3);
    let slot = date(2026, 11, 10);
    let moved_to = date(2026, 11, 6);
    let mut input = support::new_item(household.tasks_list_id, "Проверить почту");
    input.due_date = Some(slot);
    let mut rule = support::recurrence(RecurrenceFreq::Weekly, vec![2]);
    rule.anchor_date = Some(anchor);
    input.recurrence = Some(rule.clone());
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let now = utc(2026, 11, 6, 12);
    let mut conn = db.pool.acquire().await.expect("connection");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        Due::AllDay(moved_to),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("reschedule occurrence");
    assert_eq!(
        moved.recurrence.as_ref().expect("rule").occurrence_date,
        Some(slot)
    );

    let updated = items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        moved.version,
        ItemPatch {
            recurrence: Field::Value(rule),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        now,
    )
    .await
    .expect("repeat the current rule");
    assert_eq!(updated.version, moved.version);
    assert_eq!(updated.due_date, Some(moved_to));
    let rule = updated.recurrence.as_ref().expect("rule");
    assert_eq!(rule.occurrence_date, Some(slot));
    assert_eq!(rule.anchor_date, anchor);

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        updated.id,
        Some(updated.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete after a repeated rule");
    assert_eq!(completed.due_date, Some(date(2026, 11, 17)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_after_reschedule_restores_the_previous_due_and_slot() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let anchor = date(2026, 11, 10);
    let mut input = support::new_item(household.tasks_list_id, "Позвонить врачу");
    input.due_date = Some(anchor);
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Weekly, vec![2]));
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let first = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        Due::AllDay(date(2026, 11, 6)),
        "Europe/Moscow",
        utc(2027, 1, 1, 12),
    )
    .await
    .expect("first reschedule");

    let now = utc(2027, 1, 1, 13);
    let second = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        first.version,
        Due::AllDay(date(2026, 11, 3)),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("second reschedule");
    assert_eq!(
        second.recurrence.as_ref().expect("rule").occurrence_date,
        Some(anchor)
    );

    let restored = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        now,
    )
    .await
    .expect("undo reschedule");
    assert_eq!(restored.due_date, Some(date(2026, 11, 6)));
    assert_eq!(
        restored.recurrence.as_ref().expect("rule").occurrence_date,
        Some(anchor),
        "the slot from payload.before is restored"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_snapshot_without_occurrence_date_parses() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut input = support::new_item(household.tasks_list_id, "Старый снимок");
    input.due_date = Some(date(2026, 11, 10));
    input.recurrence = Some(support::recurrence(RecurrenceFreq::Weekly, vec![2]));
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let event = tasklerq::storage::repositories::events::for_item(
        &mut conn,
        household.household_id,
        created.id,
    )
    .await
    .expect("events")
    .into_iter()
    .find(|event| event.event_type == EventType::Created)
    .expect("created event");
    let mut snapshot = event.payload["after"].clone();
    snapshot["recurrence"]
        .as_object_mut()
        .expect("recurrence object")
        .remove("occurrence_date");

    let parsed: tasklerq::domain::models::Item =
        serde_json::from_value(snapshot).expect("old event snapshot parses");
    assert_eq!(
        parsed.recurrence.expect("rule").occurrence_date,
        None,
        "a snapshot without the field gets no slot"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_restores_the_occurrence_slot_from_the_completion_snapshot() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let slot = date(2026, 11, 10);
    let mut input = support::new_item(household.tasks_list_id, "Вынести мусор");
    input.due_date = Some(slot);
    let mut rule = support::recurrence(RecurrenceFreq::Weekly, vec![2]);
    rule.until = Some(slot);
    input.recurrence = Some(rule);
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let now = utc(2026, 11, 6, 12);
    let mut conn = db.pool.acquire().await.expect("connection");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        Due::AllDay(date(2026, 11, 6)),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("move the last occurrence");
    assert_eq!(
        moved.recurrence.as_ref().expect("rule").occurrence_date,
        Some(slot)
    );

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(moved.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete the last occurrence");
    assert_eq!(completed.status, ItemStatus::Done);
    assert_eq!(completed.due_date, Some(date(2026, 11, 6)));

    let reopened = items::reopen(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(completed.version),
        utc(2026, 11, 6, 13),
    )
    .await
    .expect("reopen");
    assert_eq!(reopened.status, ItemStatus::Open);
    assert_eq!(reopened.due_date, Some(date(2026, 11, 6)));
    assert_eq!(
        reopened.recurrence.as_ref().expect("rule").occurrence_date,
        Some(slot),
        "the slot from payload.before is restored"
    );

    let repeated = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(reopened.version),
        "Europe/Moscow",
        utc(2026, 11, 6, 14),
    )
    .await
    .expect("complete again");
    assert_eq!(
        repeated.due_date,
        Some(date(2026, 11, 6)),
        "the restored slot prevents the next due date from landing on 10 November"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_does_not_restore_the_slot_after_the_rule_was_removed() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let slot = date(2026, 11, 10);
    let mut input = support::new_item(household.tasks_list_id, "Вынести мусор");
    input.due_date = Some(slot);
    let mut rule = support::recurrence(RecurrenceFreq::Weekly, vec![2]);
    rule.until = Some(slot);
    input.recurrence = Some(rule);
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let now = utc(2026, 11, 6, 12);
    let mut conn = db.pool.acquire().await.expect("connection");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        Due::AllDay(date(2026, 11, 6)),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("move the last occurrence");
    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(moved.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete the last occurrence");
    assert_eq!(completed.status, ItemStatus::Done);

    let without_rule = items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        completed.version,
        ItemPatch {
            recurrence: Field::Null,
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        utc(2026, 11, 6, 13),
    )
    .await
    .expect("remove the rule from a completed task");
    assert!(without_rule.recurrence.is_none());

    let reopened = items::reopen(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(without_rule.version),
        utc(2026, 11, 6, 14),
    )
    .await
    .expect("reopen must not write a slot without a rule");
    assert_eq!(reopened.status, ItemStatus::Open);
    assert_eq!(reopened.due_date, Some(date(2026, 11, 6)));
    assert!(reopened.recurrence.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_does_not_restore_a_slot_of_a_changed_rule() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let slot = date(2026, 11, 10);
    let mut input = support::new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(slot);
    let mut rule = support::recurrence(RecurrenceFreq::Weekly, vec![2]);
    rule.until = Some(slot);
    input.recurrence = Some(rule);
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let now = utc(2026, 11, 6, 12);
    let mut conn = db.pool.acquire().await.expect("connection");
    let moved = items::reschedule_occurrence(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        Due::AllDay(date(2026, 11, 6)),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("move the last occurrence");
    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(moved.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete the last occurrence");

    let changed = items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        completed.version,
        ItemPatch {
            recurrence: Field::Value(support::recurrence(RecurrenceFreq::Weekly, vec![3])),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        utc(2026, 11, 6, 13),
    )
    .await
    .expect("change the rule after completion");
    assert_eq!(
        changed.recurrence.as_ref().expect("rule").occurrence_date,
        None
    );

    let reopened = items::reopen(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(changed.version),
        utc(2026, 11, 6, 14),
    )
    .await
    .expect("reopen");
    assert_eq!(reopened.status, ItemStatus::Open);
    assert_eq!(reopened.due_date, Some(date(2026, 11, 6)));
    assert_eq!(
        reopened.recurrence.as_ref().expect("rule").occurrence_date,
        None,
        "the old slot does not belong to the new rule"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn soft_delete_is_reverted_by_undo() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Удалить меня"),
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let deleted = items::soft_delete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now(),
    )
    .await
    .expect("soft delete");
    assert!(deleted.deleted_at.is_some());
    assert_eq!(deleted.version, 2);
    assert!(matches!(
        items::get(&mut conn, household.household_id, created.id).await,
        Err(DomainError::NotFound)
    ));

    let undone = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now(),
    )
    .await
    .expect("undo delete");
    assert!(undone.deleted_at.is_none());
    assert_eq!(undone.version, deleted.version + 1);
    assert_eq!(undone.title, "Удалить меня");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_creation_soft_deletes_item() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Случайная запись"),
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let undone = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now(),
    )
    .await
    .expect("undo create");
    assert!(undone.deleted_at.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_complete_and_edit_restore_previous_state() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let today = Utc::now().date_naive();
    let mut input = support::new_item(household.tasks_list_id, "Исходное название");
    input.due_date = Some(today);
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let edited = items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        ItemPatch {
            title: Some("Изменённое название".into()),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("edit");
    assert_eq!(edited.title, "Изменённое название");

    let undone_edit = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now(),
    )
    .await
    .expect("undo edit");
    assert_eq!(undone_edit.title, "Исходное название");
    assert_eq!(undone_edit.version, edited.version + 1);

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        None,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("complete");
    assert_eq!(completed.status, ItemStatus::Done);

    let undone_complete = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now(),
    )
    .await
    .expect("undo complete");
    assert_eq!(undone_complete.status, ItemStatus::Open);
    assert!(undone_complete.completed_at.is_none());
    assert_eq!(undone_complete.due_date, Some(today));
    assert_eq!(undone_complete.version, completed.version + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_undo_does_not_roll_back_an_earlier_action() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Одна отмена"),
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        None,
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("complete");
    let undone = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now(),
    )
    .await
    .expect("first undo");
    assert_eq!(undone.status, ItemStatus::Open);

    let second = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now(),
    )
    .await
    .expect_err("second undo must not revert the create");
    assert!(matches!(second, DomainError::Conflict { .. }));
    let current = items::get(&mut conn, household.household_id, created.id)
        .await
        .expect("item stays");
    assert_eq!(current.status, ItemStatus::Open);
    assert!(current.deleted_at.is_none());
    assert_eq!(current.version, completed.version + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_undo_window_is_rejected() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Поздно отменять"),
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let error = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now() + Duration::minutes(items::UNDO_WINDOW_MINUTES + 1),
    )
    .await
    .expect_err("undo window must expire");
    match error {
        DomainError::Conflict { message, details } => {
            assert!(message.contains("undo window"));
            assert!(details["event_id"].is_string());
        }
        other => panic!("expected conflict, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn member_cannot_delete_owner_item_and_foreign_household_is_hidden() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let foreign = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Чужая задача"),
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    assert!(matches!(
        items::soft_delete(
            &mut conn,
            household.household_id,
            &household.member(),
            created.id,
            Utc::now(),
        )
        .await,
        Err(DomainError::Forbidden(_))
    ));
    assert!(matches!(
        items::get(&mut conn, foreign.household_id, created.id).await,
        Err(DomainError::NotFound)
    ));
    assert!(matches!(
        items::soft_delete(
            &mut conn,
            foreign.household_id,
            &foreign.owner(),
            created.id,
            Utc::now(),
        )
        .await,
        Err(DomainError::NotFound)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_rejects_stale_event_after_list_deletion() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut conn = db.pool.acquire().await.expect("connection");

    // Дополнительный список, который можно удалить целиком.
    let list = lists::create(
        &mut conn,
        household.household_id,
        ListKind::Checklist,
        "Временный",
        Utc::now(),
    )
    .await
    .expect("list");

    let created = support::create_item(
        &db.pool,
        &household,
        list.id,
        &household.owner(),
        support::new_item(list.id, "Пропадёт со списком"),
    )
    .await;
    let edited = items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        created.version,
        ItemPatch {
            title: Some("Изменённая".into()),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("edit");
    assert_eq!(edited.version, 2);

    // Удаление списка мягко удаляет элементы и поднимает version, не записывая
    // событий: последнее событие элемента устаревает.
    lists::delete(&mut conn, household.household_id, list.id, true, Utc::now())
        .await
        .expect("delete list");

    let error = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Utc::now(),
    )
    .await
    .expect_err("undo must not resurrect an item in a deleted list");
    assert!(matches!(error, DomainError::Conflict { .. }));

    // Элемент остаётся удалённым вместе со списком.
    assert!(matches!(
        items::get(&mut conn, household.household_id, created.id).await,
        Err(DomainError::NotFound)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_is_limited_to_the_actor_or_the_owner() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut conn = db.pool.acquire().await.expect("connection");

    let by_owner = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Создал владелец"),
    )
    .await;
    let by_member = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.member(),
        support::new_item(household.tasks_list_id, "Создал участник"),
    )
    .await;

    // Участник не может отменить чужое действие, и элемент остаётся на месте.
    assert!(matches!(
        items::undo_last(
            &mut conn,
            household.household_id,
            &household.member(),
            by_owner.id,
            Utc::now(),
        )
        .await,
        Err(DomainError::Forbidden(_))
    ));
    assert!(
        items::get(&mut conn, household.household_id, by_owner.id)
            .await
            .is_ok()
    );

    // Автор отменяет собственное действие.
    let undone = items::undo_last(
        &mut conn,
        household.household_id,
        &household.member(),
        by_member.id,
        Utc::now(),
    )
    .await
    .expect("actor can undo own action");
    assert!(undone.deleted_at.is_some());

    // Владелец может отменить действие участника.
    let by_member_again = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.member(),
        support::new_item(household.tasks_list_id, "Ещё от участника"),
    )
    .await;
    let undone = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        by_member_again.id,
        Utc::now(),
    )
    .await
    .expect("owner can undo any action");
    assert!(undone.deleted_at.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assignee_must_belong_to_the_household() {
    use axum::http::{Method, StatusCode};
    use serde_json::json;

    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let foreign = support::create_household(&db.pool).await;
    let item = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Назначение"),
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("connection");
    for stranger in [foreign.owner_id, foreign.member_id, uuid::Uuid::now_v7()] {
        let result = items::update(
            &mut conn,
            household.household_id,
            &household.owner(),
            item.id,
            item.version,
            ItemPatch {
                assignee_user_id: tasklerq::domain::models::Field::Value(stranger),
                ..ItemPatch::default()
            },
            "Europe/Moscow",
            Utc::now(),
        )
        .await;
        assert!(
            matches!(result, Err(DomainError::Validation(_))),
            "update must reject assignee {stranger}, got {result:?}"
        );
    }

    let token = support::issue_token(&db.pool, household.household_id, &["ha:write"]).await;
    let app = support::app(&db.pool);
    let (status, _) = support::request(
        &app,
        Method::POST,
        &format!("/api/v1/lists/{}/items", household.tasks_list_id),
        Some(&token),
        Some(json!({"title": "Чужому", "assignee_user_id": foreign.owner_id})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Свой участник по-прежнему назначается.
    let assigned = items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        item.id,
        item.version,
        ItemPatch {
            assignee_user_id: tasklerq::domain::models::Field::Value(household.member_id),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("own member can be assigned");
    assert_eq!(assigned.assignee_user_id, Some(household.member_id));
}

/// Записи выполнений списка с начала суток до `now`.
async fn completions(
    conn: &mut sqlx::PgConnection,
    household: &support::TestHousehold,
    since: chrono::DateTime<Utc>,
) -> Vec<items::completions::Completion> {
    items::completions::for_list_since(conn, household.household_id, household.tasks_list_id, since)
        .await
        .expect("completions")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undone_completion_is_not_listed() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;
    let now = Utc::now().trunc_subsecs(6);
    let mut conn = db.pool.acquire().await.expect("connection");

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    assert_eq!(
        completions(&mut conn, &household, now - Duration::days(1))
            .await
            .len(),
        1
    );

    items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        now + Duration::seconds(1),
    )
    .await
    .expect("undo");

    assert!(
        completions(&mut conn, &household, now - Duration::days(1))
            .await
            .is_empty(),
        "an undone completion must disappear"
    );
    assert_eq!(
        completed.completed_at,
        Some(now),
        "completion is recorded at the actual moment"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undoing_a_rename_does_not_add_a_completion() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;
    let now = Utc::now().trunc_subsecs(6);
    let mut conn = db.pool.acquire().await.expect("connection");

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        completed.version,
        ItemPatch {
            title: Some("Вынести мусор и картон".into()),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        now + Duration::seconds(1),
    )
    .await
    .expect("rename");
    items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        now + Duration::seconds(2),
    )
    .await
    .expect("undo rename");

    let listed = completions(&mut conn, &household, now - Duration::days(1)).await;
    assert_eq!(
        listed.len(),
        1,
        "the compensating event is not a completion"
    );
    assert_eq!(listed[0].completed_at, now);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undoing_a_delete_does_not_add_a_completion() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;
    let now = Utc::now().trunc_subsecs(6);
    let mut conn = db.pool.acquire().await.expect("connection");

    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    items::soft_delete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        now + Duration::seconds(1),
    )
    .await
    .expect("delete");
    items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        now + Duration::seconds(2),
    )
    .await
    .expect("undo delete");

    let listed = completions(&mut conn, &household, now - Duration::days(1)).await;
    assert_eq!(
        listed.len(),
        1,
        "the compensating event is not a completion"
    );
    assert_eq!(listed[0].completed_at, now);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undoing_a_reopen_restores_the_original_completion() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;
    let now = Utc::now().trunc_subsecs(6);
    let mut conn = db.pool.acquire().await.expect("connection");

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    items::reopen(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(completed.version),
        now + Duration::seconds(1),
    )
    .await
    .expect("reopen");
    items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        now + Duration::seconds(2),
    )
    .await
    .expect("undo reopen");

    let listed = completions(&mut conn, &household, now - Duration::days(1)).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].completed_at, now,
        "the original completion keeps its time"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_after_undoing_a_rename_hides_the_old_completion() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;
    let now = Utc::now().trunc_subsecs(6);
    let mut conn = db.pool.acquire().await.expect("connection");

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        completed.version,
        ItemPatch {
            title: Some("Вынести мусор и картон".into()),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        now + Duration::seconds(1),
    )
    .await
    .expect("rename");
    let restored = items::undo_last(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        now + Duration::seconds(2),
    )
    .await
    .expect("undo rename");
    items::reopen(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(restored.version),
        now + Duration::seconds(3),
    )
    .await
    .expect("reopen");

    assert!(
        completions(&mut conn, &household, now - Duration::days(1))
            .await
            .is_empty(),
        "a compensating completed event must not mask the reopen"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_completion_after_reopen_is_listed() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;
    let now = Utc::now().trunc_subsecs(6);
    let mut conn = db.pool.acquire().await.expect("connection");

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("first completion");
    let reopened = items::reopen(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(completed.version),
        now + Duration::seconds(1),
    )
    .await
    .expect("reopen");
    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(reopened.version),
        "Europe/Moscow",
        now + Duration::seconds(2),
    )
    .await
    .expect("second completion");

    let listed = completions(&mut conn, &household, now - Duration::days(1)).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].completed_at, now + Duration::seconds(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_list_id_prefers_the_snapshot_and_falls_back_to_the_item() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;
    let now = Utc::now().trunc_subsecs(6);
    let mut conn = db.pool.acquire().await.expect("connection");

    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");

    sqlx::query("UPDATE items SET list_id = $2 WHERE id = $1")
        .bind(created.id)
        .bind(household.shopping_list_id)
        .execute(&mut *conn)
        .await
        .expect("move the item");

    let tasks = items::completions::for_list_since(
        &mut conn,
        household.household_id,
        household.tasks_list_id,
        now - Duration::days(1),
    )
    .await
    .expect("tasks completions");
    assert_eq!(
        tasks.len(),
        1,
        "the snapshot list wins over the current item list"
    );
    let shopping = items::completions::for_list_since(
        &mut conn,
        household.household_id,
        household.shopping_list_id,
        now - Duration::days(1),
    )
    .await
    .expect("shopping completions");
    assert!(shopping.is_empty());

    sqlx::query(
        "UPDATE item_events
         SET payload = jsonb_set(payload, '{before,list_id}', '\"not-a-uuid\"'::jsonb)
         WHERE id = $1",
    )
    .bind(tasks[0].event_id)
    .execute(&mut *conn)
    .await
    .expect("break the snapshot list");

    let shopping = items::completions::for_list_since(
        &mut conn,
        household.household_id,
        household.shopping_list_id,
        now - Duration::days(1),
    )
    .await
    .expect("fallback completions");
    assert_eq!(shopping.len(), 1, "a broken snapshot list falls back");

    sqlx::query(
        "UPDATE item_events
         SET payload = jsonb_set(payload, '{before}', (payload->'before') - 'list_id')
         WHERE id = $1",
    )
    .bind(tasks[0].event_id)
    .execute(&mut *conn)
    .await
    .expect("drop the snapshot list");

    let shopping = items::completions::for_list_since(
        &mut conn,
        household.household_id,
        household.shopping_list_id,
        now - Duration::days(1),
    )
    .await
    .expect("fallback completions");
    assert_eq!(shopping.len(), 1, "a missing snapshot list falls back");
    let tasks = items::completions::for_list_since(
        &mut conn,
        household.household_id,
        household.tasks_list_id,
        now - Duration::days(1),
    )
    .await
    .expect("tasks without snapshot list");
    assert!(tasks.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopened_completion_without_undo_link_is_not_listed() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    items::reopen(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(completed.version),
        now + Duration::seconds(1),
    )
    .await
    .expect("reopen");

    let events = tasklerq::storage::repositories::events::for_item(
        &mut conn,
        household.household_id,
        created.id,
    )
    .await
    .expect("events");
    let reopened = events
        .iter()
        .find(|event| event.event_type == EventType::Reopened)
        .expect("reopened event");
    assert!(
        reopened.undo_of.is_none(),
        "reopen still writes no undo link"
    );

    assert!(
        completions(&mut conn, &household, now - Duration::days(1))
            .await
            .is_empty(),
        "a completion returned to work must disappear"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_keeps_the_title_from_the_moment_of_completion() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Полить цветы"),
    )
    .await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    items::update(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        completed.version,
        ItemPatch {
            title: Some("Полить цветы на балконе".into()),
            ..ItemPatch::default()
        },
        "Europe/Moscow",
        now + Duration::seconds(1),
    )
    .await
    .expect("rename");

    let listed = completions(&mut conn, &household, now - Duration::days(1)).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, "Полить цветы");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_snapshot_without_title_is_tolerated() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let mut input = support::new_item(household.tasks_list_id, "Вынести мусор");
    input.due_at = Some(Utc::now() + Duration::hours(2));
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");

    let completed = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");

    sqlx::query(
        "UPDATE item_events
         SET payload = jsonb_set(
             payload,
             '{before}',
             ((payload->'before') - 'title') || '{\"due_at\": \"not-a-date\"}'::jsonb
         )
         WHERE item_id = $1 AND event_type = 'completed'",
    )
    .bind(created.id)
    .execute(&mut *conn)
    .await
    .expect("break the snapshot");

    let listed = completions(&mut conn, &household, now - Duration::days(1)).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, items::completions::UNTITLED);
    assert_eq!(listed[0].due_at, None, "a broken due date is tolerated");
    assert_eq!(listed[0].item_id, completed.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleted_item_completions_are_not_listed() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Вынести мусор"),
    )
    .await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");

    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete");
    items::soft_delete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        now + Duration::seconds(1),
    )
    .await
    .expect("delete");

    assert!(
        completions(&mut conn, &household, now - Duration::days(1))
            .await
            .is_empty(),
        "deleted items must not contribute completions"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completions_are_filtered_by_list() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");

    let task = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Задача"),
    )
    .await;
    let purchase = support::create_item(
        &db.pool,
        &household,
        household.shopping_list_id,
        &household.owner(),
        support::new_item(household.shopping_list_id, "Молоко"),
    )
    .await;
    let task_id = task.id;
    for item in [task, purchase] {
        items::complete(
            &mut conn,
            household.household_id,
            &household.owner(),
            item.id,
            Some(item.version),
            "Europe/Moscow",
            now,
        )
        .await
        .expect("complete");
    }

    let listed = completions(&mut conn, &household, now - Duration::days(1)).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].item_id, task_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recurring_completions_keep_their_planned_due_dates() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let today = Utc::now().date_naive();
    let mut input = support::new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(today);
    input.recurrence = Some(support::recurrence(
        tasklerq::domain::models::RecurrenceFreq::Daily,
        Vec::new(),
    ));
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("connection");

    let first = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("first completion");
    assert_eq!(first.status, ItemStatus::Open);

    // Второе вхождение: следующее вхождение уже наступило. Возвращаем срок и
    // снимаем отметку, чтобы `complete` записал ещё одно выполнение.
    let second_due = today + Duration::days(1);
    sqlx::query("UPDATE items SET completed_at = NULL, due_date = $2 WHERE id = $1")
        .bind(created.id)
        .bind(second_due)
        .execute(&mut *conn)
        .await
        .expect("prepare second occurrence");
    items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        None,
        "Europe/Moscow",
        now,
    )
    .await
    .expect("second completion");

    let listed = completions(&mut conn, &household, now - Duration::days(1)).await;
    assert_eq!(listed.len(), 2);
    let mut due_dates: Vec<_> = listed.iter().map(|entry| entry.due_date).collect();
    due_dates.sort();
    assert_eq!(due_dates, vec![Some(today), Some(second_due)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_after_the_last_occurrence_keeps_the_earlier_completion() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now().trunc_subsecs(6);
    // `complete` считает «сегодня» в поясе household: при UTC-дате тест
    // ломается после 21:00 UTC, когда в Москве уже следующие сутки.
    let today = now.with_timezone(&chrono_tz::Europe::Moscow).date_naive();
    let tomorrow = today + Duration::days(1);
    let mut input = support::new_item(household.tasks_list_id, "Полить цветы");
    input.due_date = Some(today);
    let mut rule = support::recurrence(tasklerq::domain::models::RecurrenceFreq::Daily, Vec::new());
    rule.until = Some(tomorrow);
    input.recurrence = Some(rule);
    let created = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        input,
    )
    .await;
    let mut conn = db.pool.acquire().await.expect("connection");

    // Первое вхождение: серия продолжается, задача остаётся открытой.
    let first = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(created.version),
        "Europe/Moscow",
        now,
    )
    .await
    .expect("complete the first occurrence");
    assert_eq!(first.status, ItemStatus::Open);

    // Последнее вхождение: следующего нет, задача становится выполненной.
    let second_at = now + Duration::days(1);
    let second = items::complete(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(first.version),
        "Europe/Moscow",
        second_at,
    )
    .await
    .expect("complete the last occurrence");
    assert_eq!(second.status, ItemStatus::Done);

    items::reopen(
        &mut conn,
        household.household_id,
        &household.owner(),
        created.id,
        Some(second.version),
        second_at + Duration::seconds(1),
    )
    .await
    .expect("reopen the last occurrence");

    let listed = completions(&mut conn, &household, now - Duration::days(1)).await;
    assert_eq!(
        listed.len(),
        1,
        "reopen hides only the last completion, not the earlier one"
    );
    assert_eq!(listed[0].completed_at, now);
}
