//! Задачи 3.6–3.8: профиль и настройки, состав household, журнал.

mod support;

use axum::http::{Method, StatusCode};
use chrono::{Duration, Utc};
use serde_json::json;
use tasklerq::admin;
use tasklerq::auth::tokens::hash_token;
use tasklerq::domain::error::DomainError;
use tasklerq::domain::identity::{AuditEventType, NewMember, NoticeKind};
use tasklerq::domain::models::{Actor, ActorType, EventType, ItemPatch, UserRole};
use tasklerq::domain::settings::{NotificationSettings, NotificationSettingsUpdate, ProfileUpdate};
use tasklerq::storage::repositories::audit::{self, JournalCursor};
use tasklerq::storage::repositories::{households, invites, items, members, users};
use uuid::Uuid;

fn reason(error: &DomainError) -> Option<String> {
    match error {
        DomainError::Conflict { details, .. } => details
            .get("reason")
            .and_then(|value| value.as_str())
            .map(str::to_owned),
        _ => None,
    }
}

async fn update_profile(
    pool: &sqlx::PgPool,
    household: &support::TestHousehold,
    actor: &Actor,
    update: ProfileUpdate,
) -> Result<tasklerq::domain::models::User, DomainError> {
    let mut tx = pool.begin().await?;
    let user =
        users::update_profile(&mut tx, household.household_id, actor, update, Utc::now()).await?;
    tx.commit().await?;
    Ok(user)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_changes_name_timezone_and_validates_input() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let actor = household.member();

    let updated = update_profile(
        &db.pool,
        &household,
        &actor,
        ProfileUpdate {
            display_name: Some("  Мария Петровна  ".into()),
            timezone: Some(Some("Asia/Omsk".into())),
            ..Default::default()
        },
    )
    .await
    .expect("profile");
    assert_eq!(updated.display_name, "Мария Петровна");
    assert_eq!(updated.timezone.as_deref(), Some("Asia/Omsk"));

    // Некорректный пояс отклоняется целиком: имя тоже не меняется.
    let error = update_profile(
        &db.pool,
        &household,
        &actor,
        ProfileUpdate {
            display_name: Some("Другое имя".into()),
            timezone: Some(Some("Mars/Olympus".into())),
            ..Default::default()
        },
    )
    .await
    .expect_err("invalid timezone");
    assert!(matches!(error, DomainError::Validation(_)));
    let mut conn = db.pool.acquire().await.expect("conn");
    let current = users::get(&mut conn, household.household_id, household.member_id)
        .await
        .expect("user");
    assert_eq!(current.display_name, "Мария Петровна");
    assert_eq!(current.timezone.as_deref(), Some("Asia/Omsk"));

    for name in ["", "   ", &"я".repeat(65)] {
        let result = update_profile(
            &db.pool,
            &household,
            &actor,
            ProfileUpdate {
                display_name: Some(name.to_owned()),
                ..Default::default()
            },
        )
        .await;
        assert!(
            matches!(result, Err(DomainError::Validation(_))),
            "{name:?}"
        );
    }

    // Сброс личного пояса возвращает пояс household.
    let cleared = update_profile(
        &db.pool,
        &household,
        &actor,
        ProfileUpdate {
            timezone: Some(None),
            ..Default::default()
        },
    )
    .await
    .expect("clear");
    assert_eq!(cleared.timezone, None);
    assert_eq!(cleared.display_name, "Мария Петровна");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_settings_are_validated_and_independent() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;

    let owner_update = NotificationSettingsUpdate {
        lead_minutes: Some(60),
        digest_time: Some("07:45".into()),
        quiet_hours_enabled: Some(false),
        ..Default::default()
    };
    let owner = update_profile(
        &db.pool,
        &household,
        &household.owner(),
        ProfileUpdate {
            notifications: Some(owner_update),
            ..Default::default()
        },
    )
    .await
    .expect("owner settings");
    let owner_settings = NotificationSettings::from_settings(&owner.settings);
    assert_eq!(owner_settings.lead_minutes, 60);
    assert_eq!(owner_settings.digest_time, "07:45");
    assert!(!owner_settings.quiet_hours_enabled);
    assert!(
        owner_settings.reminders_enabled,
        "untouched fields keep defaults"
    );

    // Настройки второго участника остаются по умолчанию.
    let mut conn = db.pool.acquire().await.expect("conn");
    let member = users::get(&mut conn, household.household_id, household.member_id)
        .await
        .expect("member");
    assert_eq!(
        NotificationSettings::from_settings(&member.settings),
        NotificationSettings::default()
    );
    drop(conn);

    // Изменение второго не затрагивает первого.
    update_profile(
        &db.pool,
        &household,
        &household.member(),
        ProfileUpdate {
            notifications: Some(NotificationSettingsUpdate {
                reminders_enabled: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .expect("member settings");
    let mut conn = db.pool.acquire().await.expect("conn");
    let owner_again = users::get(&mut conn, household.household_id, household.owner_id)
        .await
        .expect("owner");
    assert_eq!(
        NotificationSettings::from_settings(&owner_again.settings),
        owner_settings
    );
    drop(conn);

    for update in [
        NotificationSettingsUpdate {
            lead_minutes: Some(4),
            ..Default::default()
        },
        NotificationSettingsUpdate {
            lead_minutes: Some(1441),
            ..Default::default()
        },
        NotificationSettingsUpdate {
            digest_time: Some("25:00".into()),
            ..Default::default()
        },
    ] {
        let result = update_profile(
            &db.pool,
            &household,
            &household.owner(),
            ProfileUpdate {
                notifications: Some(update),
                ..Default::default()
            },
        )
        .await;
        assert!(matches!(result, Err(DomainError::Validation(_))));
    }

    // Системный актор профиль не меняет.
    let system = update_profile(
        &db.pool,
        &household,
        &Actor::system(ActorType::HomeAssistant),
        ProfileUpdate::default(),
    )
    .await;
    assert!(matches!(system, Err(DomainError::Forbidden(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renaming_does_not_rewrite_history() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();

    let mut tx = db.pool.begin().await.expect("tx");
    let created = invites::create(&mut tx, household.household_id, &household.owner(), now)
        .await
        .expect("invite");
    let joined = invites::accept(
        &mut tx,
        &created.code,
        support::new_member("Старое имя"),
        now,
    )
    .await
    .expect("join");
    tx.commit().await.expect("commit");

    update_profile(
        &db.pool,
        &household,
        &Actor::user(joined.user.id, UserRole::Member),
        ProfileUpdate {
            display_name: Some("Новое имя".into()),
            ..Default::default()
        },
    )
    .await
    .expect("rename");

    let mut conn = db.pool.acquire().await.expect("conn");
    let journal = audit::membership_journal(
        &mut conn,
        household.household_id,
        &household.owner(),
        now + Duration::minutes(1),
        None,
        50,
    )
    .await
    .expect("journal");
    let joined_event = journal
        .iter()
        .find(|event| event.event_type == AuditEventType::MemberJoined)
        .expect("member_joined");
    assert_eq!(joined_event.payload["display_name"], "Старое имя");

    let current = users::get(&mut conn, household.household_id, joined.user.id)
        .await
        .expect("user");
    assert_eq!(current.display_name, "Новое имя");
    assert_eq!(
        current.telegram_user_id, joined.user.telegram_user_id,
        "telegram id is immutable"
    );
}

async fn run<F, T>(pool: &sqlx::PgPool, operation: F) -> Result<T, DomainError>
where
    F: for<'c> FnOnce(
        &'c mut sqlx::PgConnection,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, DomainError>> + Send + 'c>,
    >,
{
    let mut tx = pool.begin().await?;
    let result = operation(&mut tx).await?;
    tx.commit().await?;
    Ok(result)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_a_member_revokes_access_and_unassigns_tasks() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();

    // Задача владельца, назначенная на участника; задача участника остаётся.
    let mut assigned = support::new_item(household.tasks_list_id, "Назначенная");
    assigned.assignee_user_id = Some(household.member_id);
    let assigned = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        assigned,
    )
    .await;
    let authored = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.member(),
        support::new_item(household.tasks_list_id, "Моя задача"),
    )
    .await;

    // Токен участника и его приглашение.
    let member_token = {
        let mut tx = db.pool.begin().await.expect("tx");
        let issued = tasklerq::auth::tokens::issue(
            &mut tx,
            household.household_id,
            "Токен участника",
            &["ha:read".to_owned()],
            None,
            &household.member(),
            now,
        )
        .await
        .expect("token");
        tx.commit().await.expect("commit");
        issued.token
    };
    let app = support::app(&db.pool);
    let (status, _) = support::request(
        &app,
        Method::GET,
        "/api/v1/household",
        Some(&member_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Член не может удалять других.
    let denied = run(&db.pool, |conn| {
        let (h, actor, target) = (
            household.household_id,
            household.member(),
            household.owner_id,
        );
        Box::pin(async move { members::remove(conn, h, &actor, target, now).await })
    })
    .await;
    assert!(matches!(denied, Err(DomainError::Forbidden(_))));

    let change = run(&db.pool, |conn| {
        let (h, actor, target) = (
            household.household_id,
            household.owner(),
            household.member_id,
        );
        Box::pin(async move { members::remove(conn, h, &actor, target, now).await })
    })
    .await
    .expect("remove");

    // Назначение снято, версия выросла, событие записано.
    assert_eq!(change.unassigned_items, 1);
    let mut conn = db.pool.acquire().await.expect("conn");
    let reloaded = items::get(&mut conn, household.household_id, assigned.id)
        .await
        .expect("item");
    assert_eq!(reloaded.assignee_user_id, None);
    assert_eq!(reloaded.version, assigned.version + 1);
    let events: Vec<(EventType, serde_json::Value)> = sqlx::query_as(
        "SELECT event_type, payload FROM item_events WHERE item_id = $1 ORDER BY created_at, id",
    )
    .bind(assigned.id)
    .fetch_all(&mut *conn)
    .await
    .expect("events");
    assert!(events.iter().any(|event| event.0 == EventType::Unassigned));

    // Созданная участником задача осталась с авторством.
    let kept = items::get(&mut conn, household.household_id, authored.id)
        .await
        .expect("authored");
    assert_eq!(kept.created_by_user_id, Some(household.member_id));
    assert!(kept.deleted_at.is_none());

    // Доступ закрыт: пользователь не найден, его токен отозван.
    assert!(matches!(
        users::get(&mut conn, household.household_id, household.member_id).await,
        Err(DomainError::NotFound)
    ));
    drop(conn);
    let (status, _) = support::request(
        &app,
        Method::GET,
        "/api/v1/household",
        Some(&member_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Удалённому участнику отправляется уведомление без данных household.
    assert_eq!(change.notices.len(), 1);
    assert_eq!(change.notices[0].recipient_user_id, household.member_id);
    assert_eq!(change.notices[0].kind, NoticeKind::AccessRevoked);

    // Повторное удаление: участника уже нет, владелец не удаляет сам себя.
    let again = run(&db.pool, |conn| {
        let (h, actor, target) = (
            household.household_id,
            household.owner(),
            household.member_id,
        );
        Box::pin(async move { members::remove(conn, h, &actor, target, now).await })
    })
    .await;
    assert!(matches!(again, Err(DomainError::NotFound)));
    let myself = run(&db.pool, |conn| {
        let (h, actor, target) = (
            household.household_id,
            household.owner(),
            household.owner_id,
        );
        Box::pin(async move { members::remove(conn, h, &actor, target, now).await })
    })
    .await
    .expect_err("owner cannot remove self");
    assert_eq!(reason(&myself).as_deref(), Some("owner_cannot_be_removed"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removed_member_can_return_with_a_new_invite() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();

    let mut conn = db.pool.acquire().await.expect("conn");
    let removed = users::get(&mut conn, household.household_id, household.member_id)
        .await
        .expect("member");
    drop(conn);

    run(&db.pool, |conn| {
        let (h, actor, target) = (
            household.household_id,
            household.owner(),
            household.member_id,
        );
        Box::pin(async move { members::remove(conn, h, &actor, target, now).await })
    })
    .await
    .expect("remove");

    let mut tx = db.pool.begin().await.expect("tx");
    let invite = invites::create(&mut tx, household.household_id, &household.owner(), now)
        .await
        .expect("invite");
    let joined = invites::accept(
        &mut tx,
        &invite.code,
        tasklerq::domain::identity::NewMember {
            telegram_user_id: removed.telegram_user_id,
            telegram_chat_id: removed.telegram_chat_id,
            display_name: "Вернулась".into(),
            username: None,
        },
        now,
    )
    .await
    .expect("rejoin");
    tx.commit().await.expect("commit");
    assert_ne!(
        joined.user.id, removed.id,
        "a returning member is a new user"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn member_can_leave_and_owner_is_notified() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();

    let mut assigned = support::new_item(household.tasks_list_id, "На выходящего");
    assigned.assignee_user_id = Some(household.member_id);
    support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        assigned,
    )
    .await;

    let change = run(&db.pool, |conn| {
        let (h, actor) = (household.household_id, household.member());
        Box::pin(async move { members::leave(conn, h, &actor, now).await })
    })
    .await
    .expect("leave");
    assert_eq!(change.unassigned_items, 1);
    assert_eq!(change.notices.len(), 1);
    assert_eq!(change.notices[0].recipient_user_id, household.owner_id);
    assert!(matches!(
        change.notices[0].kind,
        NoticeKind::MemberLeft { .. }
    ));

    let mut conn = db.pool.acquire().await.expect("conn");
    assert!(matches!(
        users::get(&mut conn, household.household_id, household.member_id).await,
        Err(DomainError::NotFound)
    ));
    let remaining = users::list_active(&mut conn, household.household_id)
        .await
        .expect("members");
    assert_eq!(remaining.len(), 1, "household data stays with the owner");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_must_transfer_ownership_before_leaving() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();

    let leave = |actor: Actor| {
        let pool = db.pool.clone();
        let h = household.household_id;
        async move {
            run(&pool, move |conn| {
                Box::pin(async move { members::leave(conn, h, &actor, now).await })
            })
            .await
        }
    };

    let blocked = leave(household.owner()).await.expect_err("transfer first");
    assert_eq!(reason(&blocked).as_deref(), Some("transfer_required"));

    let change = run(&db.pool, |conn| {
        let (h, actor, target) = (
            household.household_id,
            household.owner(),
            household.member_id,
        );
        Box::pin(async move { members::transfer_ownership(conn, h, &actor, target, now).await })
    })
    .await
    .expect("transfer");
    assert_eq!(change.notices.len(), 1);
    assert_eq!(change.notices[0].recipient_user_id, household.member_id);
    assert_eq!(change.notices[0].kind, NoticeKind::OwnershipReceived);

    let mut conn = db.pool.acquire().await.expect("conn");
    let new_owner = users::get(&mut conn, household.household_id, household.member_id)
        .await
        .expect("new owner");
    let old_owner = users::get(&mut conn, household.household_id, household.owner_id)
        .await
        .expect("old owner");
    assert_eq!(new_owner.role, UserRole::Owner);
    assert_eq!(old_owner.role, UserRole::Member);
    drop(conn);

    // Новый владелец получил права, прежний их потерял.
    let denied = run(&db.pool, |conn| {
        let (h, actor) = (household.household_id, household.owner());
        Box::pin(async move { invites::create(conn, h, &actor, now).await.map(|_| ()) })
    })
    .await;
    assert!(
        matches!(denied, Err(DomainError::Forbidden(_))),
        "stale actor data must not grant ownership: the role is re-read from the database"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sole_owner_cannot_leave_but_can_delete_the_household() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();

    run(&db.pool, |conn| {
        let (h, actor, target) = (
            household.household_id,
            household.owner(),
            household.member_id,
        );
        Box::pin(async move { members::remove(conn, h, &actor, target, now).await })
    })
    .await
    .expect("remove member");

    let blocked = run(&db.pool, |conn| {
        let (h, actor) = (household.household_id, household.owner());
        Box::pin(async move { members::leave(conn, h, &actor, now).await })
    })
    .await
    .expect_err("sole owner");
    assert_eq!(reason(&blocked).as_deref(), Some("sole_owner"));

    // Передача владения самому себе и чужому пользователю отклоняется.
    for target in [
        household.owner_id,
        Uuid::now_v7(),
        support::create_household(&db.pool).await.owner_id,
    ] {
        let result = run(&db.pool, |conn| {
            let (h, actor) = (household.household_id, household.owner());
            Box::pin(async move { members::transfer_ownership(conn, h, &actor, target, now).await })
        })
        .await;
        assert!(result.is_err(), "{target}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn household_deletion_requires_confirmation_and_revokes_access() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();
    let app = support::app(&db.pool);

    let token = support::issue_token(&db.pool, household.household_id, &["ha:read"]).await;
    let mut tx = db.pool.begin().await.expect("tx");
    let invite = invites::create(&mut tx, household.household_id, &household.owner(), now)
        .await
        .expect("invite");
    tx.commit().await.expect("commit");

    let mut conn = db.pool.acquire().await.expect("conn");
    let name = households::get(&mut conn, household.household_id)
        .await
        .expect("household")
        .name;
    drop(conn);

    // Подтверждение должно совпадать с названием; участник удалять не может.
    for confirmation in ["", "не то", &name.to_lowercase()] {
        let result = run(&db.pool, |conn| {
            let (h, actor, text) = (
                household.household_id,
                household.owner(),
                confirmation.to_owned(),
            );
            Box::pin(async move { members::delete_household(conn, h, &actor, &text, now).await })
        })
        .await;
        assert!(
            matches!(result, Err(DomainError::Validation(_))),
            "{confirmation:?}"
        );
    }
    let forbidden = run(&db.pool, |conn| {
        let (h, actor, text) = (household.household_id, household.member(), name.clone());
        Box::pin(async move { members::delete_household(conn, h, &actor, &text, now).await })
    })
    .await;
    assert!(matches!(forbidden, Err(DomainError::Forbidden(_))));

    let change = run(&db.pool, |conn| {
        let (h, actor, text) = (household.household_id, household.owner(), name.clone());
        Box::pin(async move { members::delete_household(conn, h, &actor, &text, now).await })
    })
    .await
    .expect("delete");

    // Уведомление получает участник, но не владелец, который удалял.
    assert_eq!(change.notices.len(), 1);
    assert_eq!(change.notices[0].recipient_user_id, household.member_id);
    assert_eq!(
        change.notices[0].kind,
        NoticeKind::HouseholdDeleted {
            household_name: name.clone()
        }
    );

    // Мягкое удаление: строки остаются до retention.
    let (deleted_at,): (Option<chrono::DateTime<Utc>>,) =
        sqlx::query_as("SELECT deleted_at FROM households WHERE id = $1")
            .bind(household.household_id)
            .fetch_one(&db.pool)
            .await
            .expect("row");
    assert!(deleted_at.is_some());

    // Токены отозваны, приглашения не работают, участники потеряли доступ.
    let (status, _) =
        support::request(&app, Method::GET, "/api/v1/household", Some(&token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let mut tx = db.pool.begin().await.expect("tx");
    let join = invites::accept(&mut tx, &invite.code, support::new_member("Поздно"), now).await;
    assert!(join.is_err(), "invites of a deleted household are unusable");
    drop(tx);
    let mut conn = db.pool.acquire().await.expect("conn");
    for user_id in [household.owner_id, household.member_id] {
        assert!(matches!(
            users::get(&mut conn, household.household_id, user_id).await,
            Err(DomainError::NotFound)
        ));
    }

    // Бывший участник может создать новый household.
    let (telegram_id,): (i64,) = sqlx::query_as("SELECT telegram_user_id FROM users WHERE id = $1")
        .bind(household.owner_id)
        .fetch_one(&mut *conn)
        .await
        .expect("telegram id");
    let onboarded = households::onboard(
        &mut conn,
        "Новая семья",
        "Europe/Moscow",
        households::NewOwner {
            member: NewMember {
                telegram_user_id: telegram_id,
                telegram_chat_id: telegram_id,
                display_name: "Владелец".into(),
                username: None,
            },
            timezone: None,
        },
        now,
    )
    .await
    .expect("new household");
    assert!(onboarded.created);
    assert_ne!(onboarded.household.id, household.household_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn members_edit_shared_tasks_and_the_actor_is_logged() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;

    let item = support::create_item(
        &db.pool,
        &household,
        household.tasks_list_id,
        &household.owner(),
        support::new_item(household.tasks_list_id, "Общая задача"),
    )
    .await;

    let mut conn = db.pool.acquire().await.expect("conn");
    let updated = items::update(
        &mut conn,
        household.household_id,
        &household.member(),
        item.id,
        item.version,
        serde_json::from_value::<ItemPatch>(json!({"title": "Изменена участником"}))
            .expect("patch"),
        "Europe/Moscow",
        Utc::now(),
    )
    .await
    .expect("member edits a task of another member");
    assert_eq!(updated.title, "Изменена участником");

    let (actor_user_id,): (Option<Uuid>,) = sqlx::query_as(
        "SELECT actor_user_id FROM item_events WHERE item_id = $1 ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(item.id)
    .fetch_one(&mut *conn)
    .await
    .expect("event");
    assert_eq!(actor_user_id, Some(household.member_id));

    // Участник не может удалить чужую задачу, владелец может.
    let denied = items::soft_delete(
        &mut conn,
        household.household_id,
        &household.member(),
        item.id,
        Utc::now(),
    )
    .await;
    assert!(matches!(denied, Err(DomainError::Forbidden(_))));
    items::soft_delete(
        &mut conn,
        household.household_id,
        &household.owner(),
        item.id,
        Utc::now(),
    )
    .await
    .expect("owner deletes any task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journal_is_owner_only_paged_and_free_of_secrets() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();

    // Приглашение, вход и выпуск токена из CLI.
    let mut tx = db.pool.begin().await.expect("tx");
    let invite = invites::create(&mut tx, household.household_id, &household.owner(), now)
        .await
        .expect("invite");
    invites::accept(&mut tx, &invite.code, support::new_member("Гость"), now)
        .await
        .expect("join");
    tx.commit().await.expect("commit");
    let issued = admin::create_token(
        &db.pool,
        Some(household.household_id),
        "Журнал",
        &["ha:read".to_owned()],
        None,
        now,
    )
    .await
    .expect("token");

    // Событие старше 90 дней в журнал не попадает.
    let mut conn = db.pool.acquire().await.expect("conn");
    audit::record(
        &mut conn,
        Some(household.household_id),
        &Actor::system(ActorType::System),
        AuditEventType::MemberLeft,
        json!({"display_name": "Давно"}),
        now - Duration::days(91),
    )
    .await
    .expect("old event");

    let journal = audit::membership_journal(
        &mut conn,
        household.household_id,
        &household.owner(),
        now + Duration::seconds(1),
        None,
        50,
    )
    .await
    .expect("journal");
    let types: Vec<_> = journal.iter().map(|event| event.event_type).collect();
    assert!(types.contains(&AuditEventType::HouseholdCreated));
    assert!(types.contains(&AuditEventType::InviteCreated));
    assert!(types.contains(&AuditEventType::InviteUsed));
    assert!(types.contains(&AuditEventType::MemberJoined));
    assert!(
        !types.contains(&AuditEventType::TokenCreated),
        "token events belong to the security log, not membership"
    );
    assert!(
        journal
            .iter()
            .all(|event| event.payload["display_name"] != "Давно"),
        "events older than 90 days are hidden"
    );
    // Новые события идут первыми.
    assert!(
        journal
            .windows(2)
            .all(|pair| (pair[0].created_at, pair[0].id) >= (pair[1].created_at, pair[1].id))
    );

    // Ни кода приглашения, ни токена, ни хешей.
    let rendered = serde_json::to_string(&journal).expect("json");
    for secret in [
        invite.code.as_str(),
        &hash_token(&invite.code),
        issued.token.as_str(),
        &hash_token(&issued.token),
    ] {
        assert!(!rendered.contains(secret));
    }

    // Постраничное чтение без пропусков и повторов.
    let mut seen = Vec::new();
    let mut cursor: Option<JournalCursor> = None;
    loop {
        let page = audit::membership_journal(
            &mut conn,
            household.household_id,
            &household.owner(),
            now + Duration::seconds(1),
            cursor,
            2,
        )
        .await
        .expect("page");
        if page.is_empty() {
            break;
        }
        cursor = page.last().map(JournalCursor::from);
        seen.extend(page.into_iter().map(|event| event.id));
    }
    let expected: Vec<_> = journal.iter().map(|event| event.id).collect();
    assert_eq!(seen, expected);

    // Участник и системные акторы журнал не читают; чужой владелец не находит.
    for actor in [household.member(), Actor::system(ActorType::HomeAssistant)] {
        let result =
            audit::membership_journal(&mut conn, household.household_id, &actor, now, None, 10)
                .await;
        assert!(matches!(result, Err(DomainError::Forbidden(_))));
    }
    let foreign = support::create_household(&db.pool).await;
    let result = audit::membership_journal(
        &mut conn,
        household.household_id,
        &foreign.owner(),
        now,
        None,
        10,
    )
    .await;
    assert!(matches!(result, Err(DomainError::NotFound)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ownership_transfer_and_removals_are_journaled_with_source() {
    let db = support::db().await;
    let household = support::create_household(&db.pool).await;
    let now = Utc::now();

    run(&db.pool, |conn| {
        let (h, actor, target) = (
            household.household_id,
            household.owner(),
            household.member_id,
        );
        Box::pin(async move { members::transfer_ownership(conn, h, &actor, target, now).await })
    })
    .await
    .expect("transfer");

    let mut conn = db.pool.acquire().await.expect("conn");
    let new_owner = Actor::user(household.member_id, UserRole::Owner);
    let journal = audit::membership_journal(
        &mut conn,
        household.household_id,
        &new_owner,
        now + Duration::seconds(1),
        None,
        50,
    )
    .await
    .expect("journal");
    let role_changes: Vec<_> = journal
        .iter()
        .filter(|event| event.event_type == AuditEventType::RoleChanged)
        .collect();
    assert_eq!(role_changes.len(), 2);
    assert!(
        role_changes
            .iter()
            .all(|event| event.actor_type == ActorType::User)
    );
    let pairs: Vec<_> = role_changes
        .iter()
        .map(|event| (event.payload["from"].as_str(), event.payload["to"].as_str()))
        .collect();
    assert!(pairs.contains(&(Some("owner"), Some("member"))));
    assert!(pairs.contains(&(Some("member"), Some("owner"))));

    // Системное действие отображается с источником и не маскируется под участника.
    audit::record(
        &mut conn,
        Some(household.household_id),
        &Actor::system(ActorType::System),
        AuditEventType::HouseholdCreated,
        json!({"household_name": "x"}),
        now + Duration::milliseconds(500),
    )
    .await
    .expect("system event");
    let journal = audit::membership_journal(
        &mut conn,
        household.household_id,
        &new_owner,
        now + Duration::seconds(1),
        None,
        50,
    )
    .await
    .expect("journal");
    let system = journal
        .iter()
        .find(|event| event.actor_type == ActorType::System)
        .expect("system event");
    assert_eq!(system.actor_user_id, None);
}
