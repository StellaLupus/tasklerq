//! Задачи 3.2–3.4: онбординг, приглашения, роли.

mod support;

use chrono::{Duration, Utc};
use serde_json::json;
use tasklerq::admin;
use tasklerq::auth::tokens::hash_token;
use tasklerq::domain::error::DomainError;
use tasklerq::domain::identity::{AuditEventType, NewMember, NoticeKind};
use tasklerq::domain::models::{Actor, ActorType, ListKind, UserRole};
use tasklerq::storage::repositories::households::{self, NewOwner};
use tasklerq::storage::repositories::{invites, lists, members};
use uuid::Uuid;

fn owner(name: &str) -> NewOwner {
    let id = support::next_telegram_id();
    NewOwner {
        member: NewMember {
            telegram_user_id: id,
            telegram_chat_id: id,
            display_name: name.to_owned(),
            username: Some("owner".into()),
        },
        timezone: None,
    }
}

fn reason(error: &DomainError) -> Option<String> {
    match error {
        DomainError::Conflict { details, .. } => details
            .get("reason")
            .and_then(|value| value.as_str())
            .map(str::to_owned),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn onboarding_creates_household_owner_and_default_lists() {
    let db = support::db().await;
    let mut tx = db.pool.begin().await.expect("tx");
    let new_owner = owner("Алексей");
    let telegram_id = new_owner.member.telegram_user_id;

    let onboarded = households::onboard(&mut tx, "Семья", "Europe/Moscow", new_owner, Utc::now())
        .await
        .expect("onboarding");
    assert!(onboarded.created);
    assert_eq!(onboarded.user.role, UserRole::Owner);
    assert_eq!(onboarded.user.telegram_user_id, telegram_id);
    assert_eq!(onboarded.household.timezone, "Europe/Moscow");

    let all = lists::list(&mut tx, onboarded.household.id)
        .await
        .expect("lists");
    let names: Vec<_> = all
        .iter()
        .map(|list| (list.kind, list.name.as_str()))
        .collect();
    assert_eq!(
        names,
        vec![(ListKind::Tasks, "Задачи"), (ListKind::Shopping, "Покупки")]
    );
    tx.commit().await.expect("commit");

    let created: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE household_id = $1 AND event_type = 'household_created'",
    )
    .bind(onboarded.household.id)
    .fetch_one(&db.pool)
    .await
    .expect("count");
    assert_eq!(created, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_onboarding_step_returns_the_same_household() {
    let db = support::db().await;
    let new_owner = owner("Повтор");
    let telegram_id = new_owner.member.telegram_user_id;

    let mut tx = db.pool.begin().await.expect("tx");
    let first = households::onboard(
        &mut tx,
        "Первая",
        "Europe/Moscow",
        new_owner.clone(),
        Utc::now(),
    )
    .await
    .expect("first");
    tx.commit().await.expect("commit");

    let mut tx = db.pool.begin().await.expect("tx");
    let second = households::onboard(&mut tx, "Вторая", "Asia/Omsk", new_owner, Utc::now())
        .await
        .expect("second");
    tx.commit().await.expect("commit");

    assert!(first.created);
    assert!(!second.created);
    assert_eq!(first.household.id, second.household.id);
    assert_eq!(second.household.name, "Первая");

    let households_count: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT household_id) FROM users WHERE telegram_user_id = $1",
    )
    .bind(telegram_id)
    .fetch_one(&db.pool)
    .await
    .expect("count");
    assert_eq!(households_count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_onboarding_creates_one_household() {
    let db = support::db().await;
    let new_owner = owner("Гонка");
    let telegram_id = new_owner.member.telegram_user_id;

    let mut handles = Vec::new();
    for _ in 0..4 {
        let pool = db.pool.clone();
        let new_owner = new_owner.clone();
        handles.push(tokio::spawn(async move {
            let mut tx = pool.begin().await.expect("tx");
            let result =
                households::onboard(&mut tx, "Гонка", "Europe/Moscow", new_owner, Utc::now())
                    .await
                    .expect("onboarding");
            tx.commit().await.expect("commit");
            result
        }));
    }
    let mut ids = Vec::new();
    let mut created = 0;
    for handle in handles {
        let result = handle.await.expect("task");
        ids.push(result.household.id);
        created += usize::from(result.created);
    }
    ids.dedup();
    assert_eq!(created, 1, "exactly one request creates the household");
    assert_eq!(ids.len(), 1);

    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM users WHERE telegram_user_id = $1 AND deleted_at IS NULL",
    )
    .bind(telegram_id)
    .fetch_one(&db.pool)
    .await
    .expect("count");
    assert_eq!(rows, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_timezone_does_not_create_household() {
    let db = support::db().await;
    let new_owner = owner("Часы");
    let telegram_id = new_owner.member.telegram_user_id;
    let name = format!("Без пояса {}", Uuid::now_v7());

    let mut tx = db.pool.begin().await.expect("tx");
    let result = households::onboard(&mut tx, &name, "Mars/Olympus", new_owner, Utc::now()).await;
    assert!(
        matches!(result, Err(DomainError::Validation(_))),
        "{result:?}"
    );
    drop(tx);

    let households_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM households WHERE name = $1")
            .bind(&name)
            .fetch_one(&db.pool)
            .await
            .expect("count");
    let users_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE telegram_user_id = $1")
            .bind(telegram_id)
            .fetch_one(&db.pool)
            .await
            .expect("count");
    assert_eq!((households_count, users_count), (0, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_added_owner_skips_onboarding_and_second_owner_is_rejected() {
    let db = support::db().await;
    let household = admin::create_household(&db.pool, "Админская", "Europe/Moscow", Utc::now())
        .await
        .expect("household");
    let telegram_id = support::next_telegram_id();

    let (_, user) = admin::add_user(
        &db.pool,
        Some(household.id),
        UserRole::Owner,
        telegram_id,
        Some("Хозяин"),
        Utc::now(),
    )
    .await
    .expect("owner");
    assert_eq!(user.role, UserRole::Owner);
    assert_eq!(
        user.telegram_chat_id, telegram_id,
        "private chat id equals user id"
    );

    // Первое обращение к боту: онбординг возвращает существующий household.
    let mut tx = db.pool.begin().await.expect("tx");
    let onboarded = households::onboard(
        &mut tx,
        "Новая",
        "Europe/Moscow",
        NewOwner {
            member: NewMember {
                telegram_user_id: telegram_id,
                telegram_chat_id: telegram_id,
                display_name: "Хозяин".into(),
                username: None,
            },
            timezone: None,
        },
        Utc::now(),
    )
    .await
    .expect("onboarding");
    tx.commit().await.expect("commit");
    assert!(!onboarded.created);
    assert_eq!(onboarded.household.id, household.id);

    let second = admin::add_user(
        &db.pool,
        Some(household.id),
        UserRole::Owner,
        support::next_telegram_id(),
        None,
        Utc::now(),
    )
    .await;
    let error = second.expect_err("second owner");
    assert!(matches!(error, DomainError::Conflict { .. }));
    assert_eq!(reason(&error).as_deref(), Some("owner_exists"));
    assert!(admin::describe(&error).contains("409"));

    // Участник добавляется без ограничений владельца.
    let (_, member) = admin::add_user(
        &db.pool,
        Some(household.id),
        UserRole::Member,
        support::next_telegram_id(),
        None,
        Utc::now(),
    )
    .await
    .expect("member");
    assert_eq!(member.display_name, admin::DEFAULT_MEMBER_NAME);

    // Пользователь не может состоять в двух household.
    let duplicate = admin::add_user(
        &db.pool,
        Some(household.id),
        UserRole::Member,
        telegram_id,
        None,
        Utc::now(),
    )
    .await
    .expect_err("already a member");
    assert_eq!(reason(&duplicate).as_deref(), Some("already_member"));

    // Создание household и добавление участников администратором попадает в
    // журнал от имени системного актора; отказ ничего не записывает.
    let events: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT event_type::text, actor_type::text, payload->>'role'
         FROM audit_events
         WHERE household_id = $1
         ORDER BY created_at, id",
    )
    .bind(household.id)
    .fetch_all(&db.pool)
    .await
    .expect("audit");
    let system = "system".to_owned();
    assert_eq!(
        events,
        vec![
            ("household_created".to_owned(), system.clone(), None),
            (
                "member_joined".to_owned(),
                system.clone(),
                Some("owner".into())
            ),
            ("member_joined".to_owned(), system, Some("member".into())),
        ]
    );
}

async fn create_invite(
    pool: &sqlx::PgPool,
    household: &support::TestHousehold,
    now: chrono::DateTime<Utc>,
) -> invites::CreatedInvite {
    let mut tx = pool.begin().await.expect("tx");
    let created = invites::create(&mut tx, household.household_id, &household.owner(), now)
        .await
        .expect("invite");
    tx.commit().await.expect("commit");
    created
}

async fn accept(
    pool: &sqlx::PgPool,
    code: &str,
    member: NewMember,
    now: chrono::DateTime<Utc>,
) -> Result<tasklerq::domain::identity::Joined, DomainError> {
    let mut tx = pool.begin().await?;
    let joined = invites::accept(&mut tx, code, member, now).await?;
    tx.commit().await?;
    Ok(joined)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invite_lets_a_new_member_join() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let created = create_invite(&db.pool, &household, now).await;

    assert_eq!(created.code.len(), 16);
    // PostgreSQL хранит микросекунды, `now` — наносекунды.
    let ttl = created.invite.expires_at - now;
    assert!(
        (ttl - Duration::hours(72)).abs() < Duration::microseconds(1),
        "ttl is {ttl}"
    );
    assert_eq!(
        invites::deep_link("@tasklerqbot", &created.code),
        format!("https://t.me/tasklerqbot?start={}", created.code)
    );

    let stored: String = sqlx::query_scalar("SELECT code_hash FROM invites WHERE id = $1")
        .bind(created.invite.id)
        .fetch_one(&db.pool)
        .await
        .expect("hash");
    assert_eq!(stored, hash_token(&created.code));
    assert_ne!(stored, created.code);

    let joiner = support::new_member("Мария");
    let joined = accept(
        &db.pool,
        &created.code,
        joiner.clone(),
        now + Duration::hours(1),
    )
    .await
    .expect("join");
    assert_eq!(joined.user.role, UserRole::Member);
    assert_eq!(joined.user.household_id, household.household_id);
    assert_eq!(joined.user.telegram_user_id, joiner.telegram_user_id);

    assert_eq!(joined.notices.len(), 1);
    assert_eq!(joined.notices[0].recipient_user_id, household.owner_id);
    assert_eq!(
        joined.notices[0].kind,
        NoticeKind::MemberJoined {
            display_name: "Мария".into()
        }
    );

    let (used_by, used_at): (Option<Uuid>, Option<chrono::DateTime<Utc>>) =
        sqlx::query_as("SELECT used_by_user_id, used_at FROM invites WHERE id = $1")
            .bind(created.invite.id)
            .fetch_one(&db.pool)
            .await
            .expect("invite");
    assert_eq!(used_by, Some(joined.user.id));
    assert!(used_at.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_invites_are_rejected_without_creating_users() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();

    // Истёкшее.
    let expired = create_invite(&db.pool, &household, now).await;
    let late = support::new_member("Поздно");
    let error = accept(
        &db.pool,
        &expired.code,
        late.clone(),
        now + Duration::hours(73),
    )
    .await
    .expect_err("expired");
    assert_eq!(reason(&error).as_deref(), Some("expired"));

    // В момент `expires_at` приглашение уже недействительно. Момент берётся из
    // записи: Postgres хранит микросекунды, и `now + 72h` с наносекундами
    // оказался бы строго позже сохранённого значения, не проверяя границу.
    let boundary = accept(
        &db.pool,
        &expired.code,
        late.clone(),
        expired.invite.expires_at,
    )
    .await
    .expect_err("boundary");
    assert_eq!(reason(&boundary).as_deref(), Some("expired"));

    // За микросекунду до границы приглашение ещё действует.
    let last_moment = create_invite(&db.pool, &household, now).await;
    accept(
        &db.pool,
        &last_moment.code,
        support::new_member("Успел"),
        last_moment.invite.expires_at - Duration::microseconds(1),
    )
    .await
    .expect("one microsecond before expiry");

    // Повторное использование.
    let used = create_invite(&db.pool, &household, now).await;
    accept(&db.pool, &used.code, support::new_member("Первый"), now)
        .await
        .expect("first use");
    let second = support::new_member("Второй");
    let error = accept(&db.pool, &used.code, second.clone(), now)
        .await
        .expect_err("reuse");
    assert_eq!(reason(&error).as_deref(), Some("used"));

    // Отзыв.
    let revoked = create_invite(&db.pool, &household, now).await;
    let mut tx = db.pool.begin().await.expect("tx");
    invites::revoke(
        &mut tx,
        household.household_id,
        &household.owner(),
        revoked.invite.id,
        now,
    )
    .await
    .expect("revoke");
    tx.commit().await.expect("commit");
    let error = accept(&db.pool, &revoked.code, second.clone(), now)
        .await
        .expect_err("revoked");
    assert_eq!(reason(&error).as_deref(), Some("revoked"));

    // Неизвестные и некорректные коды.
    for code in ["", "short", "this-code-is-way-too-long", "AAAAAAAAAAAAAAAA"] {
        let error = accept(&db.pool, code, second.clone(), now)
            .await
            .expect_err("unknown");
        assert!(matches!(error, DomainError::NotFound), "{code}");
    }

    for rejected in [&late, &second] {
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM users WHERE telegram_user_id = $1")
                .bind(rejected.telegram_user_id)
                .fetch_one(&db.pool)
                .await
                .expect("count");
        assert_eq!(count, 0, "rejected invite must not create a user");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_use_of_one_code_admits_one_user() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let created = create_invite(&db.pool, &household, now).await;

    let mut handles = Vec::new();
    for index in 0..4 {
        let pool = db.pool.clone();
        let code = created.code.clone();
        let member = support::new_member(&format!("Гость {index}"));
        handles.push(tokio::spawn(async move {
            accept(&pool, &code, member, now).await
        }));
    }
    let mut joined = 0;
    for handle in handles {
        if handle.await.expect("task").is_ok() {
            joined += 1;
        }
    }
    assert_eq!(joined, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_most_five_active_invites() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();

    let mut created = Vec::new();
    for _ in 0..5 {
        created.push(create_invite(&db.pool, &household, now).await);
    }
    let mut tx = db.pool.begin().await.expect("tx");
    let error = invites::create(&mut tx, household.household_id, &household.owner(), now)
        .await
        .expect_err("limit");
    assert_eq!(reason(&error).as_deref(), Some("invite_limit"));
    drop(tx);

    // Отзыв освобождает слот.
    let mut tx = db.pool.begin().await.expect("tx");
    invites::revoke(
        &mut tx,
        household.household_id,
        &household.owner(),
        created[0].invite.id,
        now,
    )
    .await
    .expect("revoke");
    // Повторный отзыв идемпотентен.
    invites::revoke(
        &mut tx,
        household.household_id,
        &household.owner(),
        created[0].invite.id,
        now,
    )
    .await
    .expect("revoke again");
    invites::create(&mut tx, household.household_id, &household.owner(), now)
        .await
        .expect("slot is free");
    tx.commit().await.expect("commit");

    // Истёкшие приглашения слоты не занимают.
    let later = now + Duration::hours(73);
    let active = invites::list_active(
        &mut db.pool.acquire().await.expect("conn"),
        household.household_id,
        &household.owner(),
        later,
    )
    .await
    .expect("active");
    assert!(active.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn members_and_system_actors_cannot_manage_invites_or_members() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let other_member = support::create_household(&db.pool).await.member_id;
    let now = Utc::now();
    let created = create_invite(&db.pool, &household, now).await;

    let outsiders = [
        household.member(),
        Actor::system(ActorType::HomeAssistant),
        Actor::system(ActorType::Voice),
        // Токен интеграции несёт роль owner, но не является пользователем.
        Actor {
            user_id: None,
            role: Some(UserRole::Owner),
            actor_type: ActorType::HomeAssistant,
        },
    ];

    for actor in &outsiders {
        let mut conn = db.pool.acquire().await.expect("conn");
        assert!(matches!(
            invites::create(&mut conn, household.household_id, actor, now).await,
            Err(DomainError::Forbidden(_))
        ));
        assert!(matches!(
            invites::revoke(
                &mut conn,
                household.household_id,
                actor,
                created.invite.id,
                now
            )
            .await,
            Err(DomainError::Forbidden(_))
        ));
        assert!(matches!(
            invites::list_active(&mut conn, household.household_id, actor, now).await,
            Err(DomainError::Forbidden(_))
        ));
        assert!(matches!(
            members::remove(
                &mut conn,
                household.household_id,
                actor,
                household.owner_id,
                now
            )
            .await,
            Err(DomainError::Forbidden(_))
        ));
        assert!(matches!(
            members::transfer_ownership(
                &mut conn,
                household.household_id,
                actor,
                household.member_id,
                now
            )
            .await,
            Err(DomainError::Forbidden(_))
        ));
        assert!(matches!(
            members::delete_household(&mut conn, household.household_id, actor, "x", now).await,
            Err(DomainError::Forbidden(_))
        ));
    }

    // Ничего не изменилось.
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM invites WHERE household_id = $1 AND revoked_at IS NULL",
    )
    .bind(household.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("count");
    assert_eq!(active, 1);
    let members_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM users WHERE household_id = $1 AND deleted_at IS NULL",
    )
    .bind(household.household_id)
    .fetch_one(&db.pool)
    .await
    .expect("count");
    assert_eq!(members_count, 2);

    // Владелец чужого household не может управлять этим.
    let mut conn = db.pool.acquire().await.expect("conn");
    let foreign = support::create_household(&db.pool).await;
    assert!(matches!(
        invites::revoke(
            &mut conn,
            foreign.household_id,
            &foreign.owner(),
            created.invite.id,
            now
        )
        .await,
        Err(DomainError::NotFound)
    ));
    assert!(matches!(
        members::remove(
            &mut conn,
            foreign.household_id,
            &foreign.owner(),
            other_member,
            now
        )
        .await,
        Err(DomainError::NotFound)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invitation_events_are_audited_without_codes() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let created = create_invite(&db.pool, &household, now).await;
    accept(&db.pool, &created.code, support::new_member("Аудит"), now)
        .await
        .expect("join");

    let rows: Vec<(AuditEventType, serde_json::Value)> = sqlx::query_as(
        "SELECT event_type, payload FROM audit_events
         WHERE household_id = $1 ORDER BY created_at, id",
    )
    .bind(household.household_id)
    .fetch_all(&db.pool)
    .await
    .expect("events");
    let types: Vec<_> = rows.iter().map(|(kind, _)| *kind).collect();
    assert!(types.contains(&AuditEventType::InviteCreated));
    assert!(types.contains(&AuditEventType::InviteUsed));
    assert!(types.contains(&AuditEventType::MemberJoined));

    let rendered = json!(rows.iter().map(|(_, payload)| payload).collect::<Vec<_>>()).to_string();
    assert!(!rendered.contains(&created.code));
    assert!(!rendered.contains(&hash_token(&created.code)));
}
