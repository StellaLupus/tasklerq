//! Интеграционные тесты черновиков capture: TTL, правка, подтверждение и
//! идемпотентность.

mod support;

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use tasklerq::capture::draft::{self, DraftError, ItemEdit, NewCapture};
use tasklerq::capture::schema::{ParsedResponse, RawItem};
use tasklerq::domain::error::DomainError;
use tasklerq::domain::models::{CaptureMode, CaptureSource, DraftStatus, Field, ListKind};
use tasklerq::storage::repositories::drafts;
use uuid::Uuid;

use support::TestHousehold;

fn raw(title: &str, kind: ListKind) -> RawItem {
    RawItem {
        target_kind: kind,
        target_list: None,
        title: title.to_owned(),
        notes: None,
        due_date: None,
        due_time: None,
        priority: None,
        assignee: None,
        assignee_id: None,
        tags: Vec::new(),
        quantity: None,
        unit: None,
        subtasks: Vec::new(),
        recurrence: None,
    }
}

fn capture(household: &TestHousehold, items: Vec<RawItem>) -> NewCapture {
    NewCapture {
        household_id: household.household_id,
        user_id: Some(household.owner_id),
        chat_id: Some(1),
        source: CaptureSource::Telegram,
        mode: CaptureMode::Draft,
        idempotency_key: None,
        request_hash: None,
        raw_text: "завтра позвонить маме и купить молоко".to_owned(),
        parsed: ParsedResponse {
            items,
            clarifications: Vec::new(),
        },
        fallback: false,
        prompt_version: Some("capture-v1".into()),
    }
}

async fn item_count(pool: &PgPool, household: &TestHousehold) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM items WHERE household_id = $1")
        .bind(household.household_id)
        .fetch_one(pool)
        .await
        .expect("count items")
}

fn date_in_days(now: DateTime<Utc>, days: i64) -> String {
    (now + Duration::days(days)).format("%Y-%m-%d").to_string()
}

async fn two_item_draft(pool: &PgPool, household: &TestHousehold, now: DateTime<Utc>) -> Uuid {
    let mut call = raw("Позвонить маме", ListKind::Tasks);
    call.due_date = Some(date_in_days(now, 1));
    call.due_time = Some("18:00".into());
    let mut milk = raw("Молоко", ListKind::Shopping);
    milk.quantity = Some(2.0);
    milk.unit = Some("л".into());

    let mut conn = pool.acquire().await.expect("connection");
    draft::create(&mut conn, capture(household, vec![call, milk]), now)
        .await
        .expect("draft")
        .id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn draft_lives_fifteen_minutes_and_hides_nothing_until_confirmed() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let id = two_item_draft(&db.pool, &household, now).await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let stored = drafts::get(&mut conn, household.household_id, id)
        .await
        .expect("draft");
    assert_eq!(stored.status, DraftStatus::Pending);
    assert_eq!(stored.expires_at - stored.created_at, Duration::minutes(15));
    assert_eq!(item_count(&db.pool, &household).await, 0);

    let view = draft::view(&mut conn, household.household_id, id, now)
        .await
        .expect("view");
    assert!(view.is_open(now));
    assert!(view.validated.is_valid());
    assert_eq!(view.validated.items.len(), 2);
    assert!(!view.is_open(now + Duration::minutes(15)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirm_creates_every_item_once_and_links_them_to_the_capture() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let id = two_item_draft(&db.pool, &household, now).await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let confirmed = draft::confirm(
        &mut conn,
        household.household_id,
        &household.owner(),
        id,
        now + Duration::minutes(1),
    )
    .await
    .expect("confirm");

    assert!(!confirmed.replayed);
    assert_eq!(confirmed.items.len(), 2);
    assert_eq!(confirmed.items[0].list_id, household.tasks_list_id);
    assert_eq!(confirmed.items[1].list_id, household.shopping_list_id);
    assert_eq!(confirmed.items[1].quantity, Some(2.0));
    assert!(
        confirmed
            .items
            .iter()
            .all(|item| item.capture_id == Some(id))
    );
    assert_eq!(item_count(&db.pool, &household).await, 2);

    let stored = drafts::get(&mut conn, household.household_id, id)
        .await
        .expect("draft");
    assert_eq!(stored.status, DraftStatus::Applied);
    assert!(stored.applied_at.is_some());

    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM item_events WHERE household_id = $1 AND event_type = 'created'",
    )
    .bind(household.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("events");
    assert_eq!(events, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_confirm_returns_the_first_result_without_duplicates() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let id = two_item_draft(&db.pool, &household, now).await;

    let mut conn = db.pool.acquire().await.expect("connection");
    let first = draft::confirm(
        &mut conn,
        household.household_id,
        &household.owner(),
        id,
        now,
    )
    .await
    .expect("first");
    // Повтор позже срока жизни тоже возвращает результат первого.
    let second = draft::confirm(
        &mut conn,
        household.household_id,
        &household.owner(),
        id,
        now + Duration::hours(1),
    )
    .await
    .expect("second");

    assert!(second.replayed);
    let ids = |items: &[tasklerq::domain::models::Item]| {
        items.iter().map(|item| item.id).collect::<Vec<_>>()
    };
    assert_eq!(ids(&first.items), ids(&second.items));
    assert_eq!(item_count(&db.pool, &household).await, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_confirms_create_items_only_once() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let id = two_item_draft(&db.pool, &household, now).await;

    let mut tasks = Vec::new();
    for _ in 0..4 {
        let pool = db.pool.clone();
        tasks.push(tokio::spawn(async move {
            let mut conn = pool.acquire().await.expect("connection");
            draft::confirm(
                &mut conn,
                household.household_id,
                &household.owner(),
                id,
                now,
            )
            .await
            .expect("confirm")
        }));
    }
    let mut replayed = 0;
    for task in tasks {
        let confirmed = task.await.expect("task");
        assert_eq!(confirmed.items.len(), 2);
        replayed += usize::from(confirmed.replayed);
    }

    assert_eq!(replayed, 3, "exactly one call creates the items");
    assert_eq!(item_count(&db.pool, &household).await, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_item_blocks_saving_until_it_is_fixed_or_removed() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();

    let mut past = raw("Оплатить счёт", ListKind::Tasks);
    past.due_date = Some(date_in_days(now, -3));
    let good = raw("Молоко", ListKind::Shopping);
    let mut conn = db.pool.acquire().await.expect("connection");
    let id = draft::create(&mut conn, capture(&household, vec![past, good]), now)
        .await
        .expect("draft")
        .id;

    let view = draft::view(&mut conn, household.household_id, id, now)
        .await
        .expect("view");
    assert_eq!(view.validated.items[0].errors[0].code, "date_past");
    assert!(view.validated.items[1].is_valid());

    let blocked = draft::confirm(
        &mut conn,
        household.household_id,
        &household.owner(),
        id,
        now,
    )
    .await
    .expect_err("invalid draft must not be saved");
    assert!(matches!(blocked, DraftError::Invalid(ref items) if items == &[0]));
    assert_eq!(item_count(&db.pool, &household).await, 0);

    let fixed = draft::edit_item(
        &mut conn,
        household.household_id,
        id,
        0,
        ItemEdit {
            due_date: Field::Value(date_in_days(now, 2)),
            ..ItemEdit::default()
        },
        now,
    )
    .await
    .expect("edit");
    assert!(fixed.validated.is_valid());

    let confirmed = draft::confirm(
        &mut conn,
        household.household_id,
        &household.owner(),
        id,
        now,
    )
    .await
    .expect("confirm");
    assert_eq!(confirmed.items.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_items_edits_the_draft_and_an_empty_draft_cannot_be_saved() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let id = two_item_draft(&db.pool, &household, now).await;
    let mut conn = db.pool.acquire().await.expect("connection");

    let view = draft::remove_item(&mut conn, household.household_id, id, 0, now)
        .await
        .expect("remove");
    assert_eq!(view.validated.items.len(), 1);
    assert_eq!(view.validated.items[0].title, "Молоко");

    let missing = draft::remove_item(&mut conn, household.household_id, id, 5, now)
        .await
        .expect_err("no such item");
    assert!(matches!(missing, DraftError::NoSuchItem(5)));

    draft::remove_item(&mut conn, household.household_id, id, 0, now)
        .await
        .expect("remove last");
    let empty = draft::confirm(
        &mut conn,
        household.household_id,
        &household.owner(),
        id,
        now,
    )
    .await
    .expect_err("empty draft");
    assert!(matches!(empty, DraftError::Invalid(ref items) if items.is_empty()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_replaces_fields_and_clears_time_with_the_date() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let id = two_item_draft(&db.pool, &household, now).await;
    let mut conn = db.pool.acquire().await.expect("connection");

    let view = draft::edit_item(
        &mut conn,
        household.household_id,
        id,
        0,
        ItemEdit {
            title: Some("  Позвонить бабушке\u{0} ".into()),
            due_date: Field::Null,
            ..ItemEdit::default()
        },
        now,
    )
    .await
    .expect("edit");

    let edited = &view.validated.items[0];
    assert!(edited.is_valid(), "{:?}", edited.errors);
    assert_eq!(edited.title, "Позвонить бабушке");
    let item = edited.item.as_ref().expect("item");
    assert!(item.due_at.is_none() && item.due_date.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_draft_is_rejected_and_creates_nothing() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let id = two_item_draft(&db.pool, &household, now).await;
    let mut conn = db.pool.acquire().await.expect("connection");

    let late = now + Duration::minutes(16);
    let confirm = draft::confirm(
        &mut conn,
        household.household_id,
        &household.owner(),
        id,
        late,
    )
    .await
    .expect_err("expired");
    assert!(matches!(confirm, DraftError::Expired));
    let edit = draft::remove_item(&mut conn, household.household_id, id, 0, late)
        .await
        .expect_err("expired");
    assert!(matches!(edit, DraftError::Expired));
    assert_eq!(item_count(&db.pool, &household).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_draft_creates_nothing_and_cancel_is_repeatable() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let id = two_item_draft(&db.pool, &household, now).await;
    let mut conn = db.pool.acquire().await.expect("connection");

    draft::cancel(&mut conn, household.household_id, id, now)
        .await
        .expect("cancel");
    draft::cancel(&mut conn, household.household_id, id, now)
        .await
        .expect("repeated cancel");
    let stored = drafts::get(&mut conn, household.household_id, id)
        .await
        .expect("draft");
    assert_eq!(stored.status, DraftStatus::Cancelled);

    let confirm = draft::confirm(
        &mut conn,
        household.household_id,
        &household.owner(),
        id,
        now,
    )
    .await
    .expect_err("cancelled");
    assert!(matches!(confirm, DraftError::Cancelled));
    assert_eq!(item_count(&db.pool, &household).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn applied_draft_cannot_be_cancelled_or_edited() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let id = two_item_draft(&db.pool, &household, now).await;
    let mut conn = db.pool.acquire().await.expect("connection");
    draft::confirm(
        &mut conn,
        household.household_id,
        &household.owner(),
        id,
        now,
    )
    .await
    .expect("confirm");

    let cancel = draft::cancel(&mut conn, household.household_id, id, now)
        .await
        .expect_err("applied");
    assert!(matches!(cancel, DraftError::Applied));
    let edit = draft::remove_item(&mut conn, household.household_id, id, 0, now)
        .await
        .expect_err("applied");
    assert!(matches!(edit, DraftError::Applied));
    assert_eq!(item_count(&db.pool, &household).await, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failure_in_the_middle_rolls_back_every_item() {
    let db = support::isolated_db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();

    // Триггер ломает вставку только для этого названия; база отдельная, потому
    // что DDL на `items` подвесил бы параллельные тесты общей базы.
    let marker = format!("boom-{}", Uuid::now_v7());
    let suffix = marker.replace('-', "_");
    sqlx::query(&format!(
        "CREATE FUNCTION fail_{suffix}() RETURNS trigger AS $$
         BEGIN
             IF NEW.title = '{marker}' THEN
                 RAISE EXCEPTION 'forced failure';
             END IF;
             RETURN NEW;
         END $$ LANGUAGE plpgsql"
    ))
    .execute(&db.pool)
    .await
    .expect("function");
    sqlx::query(&format!(
        "CREATE TRIGGER fail_{suffix} BEFORE INSERT ON items
         FOR EACH ROW EXECUTE FUNCTION fail_{suffix}()"
    ))
    .execute(&db.pool)
    .await
    .expect("trigger");

    let mut conn = db.pool.acquire().await.expect("connection");
    let id = draft::create(
        &mut conn,
        capture(
            &household,
            vec![
                raw("Первая", ListKind::Tasks),
                raw("Вторая", ListKind::Tasks),
                raw(&marker, ListKind::Tasks),
            ],
        ),
        now,
    )
    .await
    .expect("draft")
    .id;

    let result = draft::confirm(
        &mut conn,
        household.household_id,
        &household.owner(),
        id,
        now,
    )
    .await;
    sqlx::query(&format!("DROP TRIGGER fail_{suffix} ON items"))
        .execute(&db.pool)
        .await
        .expect("drop trigger");
    sqlx::query(&format!("DROP FUNCTION fail_{suffix}()"))
        .execute(&db.pool)
        .await
        .expect("drop function");

    assert!(result.is_err(), "the last insert must fail");
    assert_eq!(item_count(&db.pool, &household).await, 0);
    let stored = drafts::get(&mut conn, household.household_id, id)
        .await
        .expect("draft");
    assert_eq!(stored.status, DraftStatus::Pending);

    // После устранения причины тот же черновик подтверждается целиком.
    let retried = draft::confirm(
        &mut conn,
        household.household_id,
        &household.owner(),
        id,
        now,
    )
    .await
    .expect("retry");
    assert_eq!(retried.items.len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_household_sees_the_draft_as_missing() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let stranger = support::create_household(&db.pool).await;
    let now = Utc::now();
    let id = two_item_draft(&db.pool, &household, now).await;
    let mut conn = db.pool.acquire().await.expect("connection");

    let view = draft::view(&mut conn, stranger.household_id, id, now).await;
    assert!(matches!(view, Err(DomainError::NotFound)));
    let confirm =
        draft::confirm(&mut conn, stranger.household_id, &stranger.owner(), id, now).await;
    assert!(matches!(
        confirm,
        Err(DraftError::Domain(DomainError::NotFound))
    ));
    let cancel = draft::cancel(&mut conn, stranger.household_id, id, now).await;
    assert!(matches!(
        cancel,
        Err(DraftError::Domain(DomainError::NotFound))
    ));
    assert_eq!(item_count(&db.pool, &household).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_removes_every_item_of_the_capture_once() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let id = two_item_draft(&db.pool, &household, now).await;
    let mut conn = db.pool.acquire().await.expect("connection");
    draft::confirm(
        &mut conn,
        household.household_id,
        &household.owner(),
        id,
        now,
    )
    .await
    .expect("confirm");

    let removed = draft::undo(
        &mut conn,
        household.household_id,
        &household.owner(),
        id,
        now + Duration::minutes(5),
    )
    .await
    .expect("undo");
    assert_eq!(removed.len(), 2);
    assert_eq!(item_count_live(&db.pool, &household).await, 0);

    // Повторная отмена ничего не делает.
    let again = draft::undo(
        &mut conn,
        household.household_id,
        &household.owner(),
        id,
        now + Duration::minutes(6),
    )
    .await
    .expect("repeated undo");
    assert!(again.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_is_closed_after_ten_minutes_or_when_an_item_was_changed() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let late = two_item_draft(&db.pool, &household, now).await;
    let changed = two_item_draft(&db.pool, &household, now).await;
    let mut conn = db.pool.acquire().await.expect("connection");
    for id in [late, changed] {
        draft::confirm(
            &mut conn,
            household.household_id,
            &household.owner(),
            id,
            now,
        )
        .await
        .expect("confirm");
    }

    let expired = draft::undo(
        &mut conn,
        household.household_id,
        &household.owner(),
        late,
        now + Duration::minutes(11),
    )
    .await
    .expect_err("window is closed");
    assert!(matches!(expired, DraftError::UndoExpired));

    let first = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM items WHERE capture_id = $1 ORDER BY created_at, id LIMIT 1",
    )
    .bind(changed)
    .fetch_one(&db.pool)
    .await
    .expect("item");
    sqlx::query("UPDATE items SET version = version + 1 WHERE id = $1")
        .bind(first)
        .execute(&db.pool)
        .await
        .expect("touch");
    let blocked = draft::undo(
        &mut conn,
        household.household_id,
        &household.owner(),
        changed,
        now + Duration::minutes(1),
    )
    .await
    .expect_err("item was changed");
    assert!(matches!(blocked, DraftError::Changed));
    assert_eq!(item_count_live(&db.pool, &household).await, 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_draft_cannot_be_undone() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let id = two_item_draft(&db.pool, &household, now).await;
    let mut conn = db.pool.acquire().await.expect("connection");

    let result = draft::undo(
        &mut conn,
        household.household_id,
        &household.owner(),
        id,
        now,
    )
    .await;

    assert!(matches!(result, Err(DraftError::NotApplied)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replacing_an_item_revalidates_it() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let id = two_item_draft(&db.pool, &household, now).await;
    let mut conn = db.pool.acquire().await.expect("connection");

    let mut broken = raw("Позвонить маме", ListKind::Tasks);
    broken.due_date = Some(date_in_days(now, -4));
    let view = draft::replace_item(&mut conn, household.household_id, id, 0, broken, now)
        .await
        .expect("replace");
    assert_eq!(view.validated.items[0].errors[0].code, "date_past");
    assert_eq!(view.raw[0].title, "Позвонить маме");

    let mut chosen = raw("Позвонить маме", ListKind::Tasks);
    chosen.assignee_id = Some(household.member_id);
    let view = draft::replace_item(&mut conn, household.household_id, id, 0, chosen, now)
        .await
        .expect("replace");
    let item = view.validated.items[0].item.as_ref().expect("valid item");
    assert_eq!(item.assignee_user_id, Some(household.member_id));
}

async fn item_count_live(pool: &PgPool, household: &TestHousehold) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM items WHERE household_id = $1 AND deleted_at IS NULL")
        .bind(household.household_id)
        .fetch_one(pool)
        .await
        .expect("count live items")
}
