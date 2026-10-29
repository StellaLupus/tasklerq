//! Callback-токены: одноразовость, привязка к пользователю и срок жизни.

mod support;

use chrono::{Duration, Utc};
use serde_json::json;
use support::{create_household, db};
use tasklerq::auth::tokens::hash_token;
use tasklerq::telegram::callbacks::{self, Consumed, MAX_CALLBACK_BYTES, NewToken, Scope};

fn spec(user: i64, household: Option<uuid::Uuid>) -> NewToken {
    NewToken {
        telegram_user_id: user,
        household_id: household,
        scope: Scope::Screen,
        action: "go".into(),
        payload: json!({"a": "go"}),
        version: Some(3),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_token_is_consumed_once() {
    let db = db().await;
    let household = create_household(&db.pool).await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("conn");

    let tokens = callbacks::issue_many(&mut conn, &[spec(1, Some(household.household_id))], now)
        .await
        .expect("issue");
    let token = &tokens[0];
    assert!(token.len() <= MAX_CALLBACK_BYTES);
    assert!(token.starts_with("t:"));

    let first = callbacks::consume(&mut conn, token, 1, now)
        .await
        .expect("consume");
    let Consumed::Valid(valid) = first else {
        panic!("valid expected, got {first:?}")
    };
    assert_eq!(valid.household_id, Some(household.household_id));
    assert_eq!(valid.version, Some(3));
    assert_eq!(valid.payload, json!({"a": "go"}));

    let second = callbacks::consume(&mut conn, token, 1, now)
        .await
        .expect("consume");
    assert!(
        matches!(second, Consumed::Replayed(Scope::Screen)),
        "{second:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_is_stored_only_as_hash() {
    let db = db().await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("conn");
    let tokens = callbacks::issue_many(&mut conn, &[spec(2, None)], now)
        .await
        .expect("issue");

    let raw: i64 = sqlx::query_scalar("SELECT count(*) FROM callback_tokens WHERE token_hash = $1")
        .bind(&tokens[0])
        .fetch_one(&db.pool)
        .await
        .expect("count");
    let hashed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM callback_tokens WHERE token_hash = $1")
            .bind(hash_token(&tokens[0]))
            .fetch_one(&db.pool)
            .await
            .expect("count");

    assert_eq!((raw, hashed), (0, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_user_cannot_use_or_burn_the_token() {
    let db = db().await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("conn");
    let tokens = callbacks::issue_many(&mut conn, &[spec(10, None)], now)
        .await
        .expect("issue");

    let stranger = callbacks::consume(&mut conn, &tokens[0], 11, now)
        .await
        .expect("consume");
    assert!(matches!(stranger, Consumed::Foreign), "{stranger:?}");

    // Чужая попытка не тратит токен владельца.
    let owner = callbacks::consume(&mut conn, &tokens[0], 10, now)
        .await
        .expect("consume");
    assert!(matches!(owner, Consumed::Valid(_)), "{owner:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_and_unknown_tokens_are_refused() {
    let db = db().await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("conn");
    let tokens = callbacks::issue_many(&mut conn, &[spec(20, None)], now)
        .await
        .expect("issue");

    let late = callbacks::consume(
        &mut conn,
        &tokens[0],
        20,
        now + Duration::hours(24) + Duration::seconds(1),
    )
    .await
    .expect("consume");
    assert!(matches!(late, Consumed::Expired(Scope::Screen)), "{late:?}");

    let unknown = callbacks::consume(&mut conn, "t:AAAAAAAAAAAAAAAA", 20, now)
        .await
        .expect("consume");
    assert!(matches!(unknown, Consumed::Unknown));
    let garbage = callbacks::consume(&mut conn, "что-то-ещё", 20, now)
        .await
        .expect("consume");
    assert!(matches!(garbage, Consumed::Unknown));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifetime_cannot_exceed_24_hours() {
    let db = db().await;
    let mut conn = db.pool.acquire().await.expect("conn");
    callbacks::issue_many(&mut conn, &[spec(30, None)], Utc::now())
        .await
        .expect("issue");

    let result = sqlx::query(
        "INSERT INTO callback_tokens (token_hash, telegram_user_id, action, expires_at, created_at)
         VALUES ('x', 1, 'go', now() + INTERVAL '25 hours', now())",
    )
    .execute(&db.pool)
    .await;

    assert!(
        result.is_err(),
        "database must reject a lifetime above 24 hours"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_consumption_executes_exactly_once() {
    let db = db().await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("conn");
    let tokens = callbacks::issue_many(&mut conn, &[spec(40, None)], now)
        .await
        .expect("issue");

    let mut handles = Vec::new();
    for _ in 0..8 {
        let pool = db.pool.clone();
        let token = tokens[0].clone();
        handles.push(tokio::spawn(async move {
            let mut conn = pool.acquire().await.expect("conn");
            callbacks::consume(&mut conn, &token, 40, now)
                .await
                .expect("consume")
        }));
    }
    let mut valid = 0;
    for handle in handles {
        if matches!(handle.await.expect("join"), Consumed::Valid(_)) {
            valid += 1;
        }
    }

    assert_eq!(valid, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_tokens_are_purged() {
    let db = db().await;
    let now = Utc::now();
    let mut conn = db.pool.acquire().await.expect("conn");
    let tokens = callbacks::issue_many(&mut conn, &[spec(50, None)], now - Duration::days(3))
        .await
        .expect("issue");

    let purged = callbacks::purge_expired(&mut conn, now)
        .await
        .expect("purge");

    assert!(purged >= 1);
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM callback_tokens WHERE token_hash = $1")
            .bind(hash_token(&tokens[0]))
            .fetch_one(&db.pool)
            .await
            .expect("count");
    assert_eq!(left, 0);
}
