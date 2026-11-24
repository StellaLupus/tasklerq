//! Синхронизация отправленных сводок (0019, решение 6).
//!
//! Шаг тика `sync_digests` обновляет последнюю сводку получателя без его
//! действия, когда меняется состояние задач из её снимка: выполнение любым
//! участником и путём (бот, HTTP API, Home Assistant), отмена, переименование,
//! перенос, назначение, удаление и архив. Сравнение отрисовки, а не событий,
//! одним механизмом ловит все эти изменения; повторный тик и сводка, уже
//! перерисованная обработчиком касания, ничего не дублируют.
//!
//! Кандидат — для каждого получателя последняя по созданию запись доставки
//! сводки (новее записей этого получателя нет — ни `pending`, ни `sent`), если
//! она `sent`, со снимком (`digest_items IS NOT NULL`), не убрана
//! (`dismissed_at IS NULL`), строка outbox доставила сообщение
//! (`message_id IS NOT NULL`), `sent_at` не старше 47 часов, а получатель жив,
//! в том же household и с чатом, совпадающим с чатом доставки. Если новая
//! сводка уже поставлена, но ещё не доставлена, прежняя кандидатом не считается
//! — её удалит замена из 0017.
//!
//! Каждая запись обрабатывается в своей транзакции под `FOR UPDATE SKIP LOCKED`
//! с повторной проверкой условий после блокировки. Отрисовка строится той же
//! [`digest_view`], что у касания, и сравнивается по отпечатку без токенов с
//! `digest_hash`; при расхождении ставится `Outgoing::Edit` с
//! `resend_if_gone = false` (удалённое пользователем сообщение не возвращается)
//! и записывается новый `digest_hash`. Ошибка одной записи не прерывает
//! остальные, но возвращается наверх, чтобы тик сообщил её с названием шага.

use chrono::{DateTime, Duration, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use super::digest::digest_view;
use crate::telegram::bot::notify;
use crate::telegram::outbox::{self, Outgoing};

/// Сообщение должно быть отправлено не старше этого срока: сводка заменяется
/// ежедневно, а более старые перерисовывать незачем (design, решение 6).
const SENT_WINDOW_HOURS: i64 = 47;

/// Условия кандидата, общие для выборки и повторной проверки под блокировкой.
///
/// `NOT EXISTS` отсекает прежние сводки, когда у получателя уже есть более
/// новая запись (`pending` или `sent`): поставленная, но не доставленная новая
/// сводка сама кандидатом не станет (`message_id IS NULL`), а прежнюю удалит
/// замена из 0017. SQL-условия живого получателя (`u.…`, `h.…`) проверяются и
/// при выборке, и под блокировкой.
///
/// Предфильтр `d.updated_at >= $1 - 1 день` обслуживается частичным индексом
/// `notification_deliveries_sent_idx` (0026): без него выборка каждые 30 секунд
/// читала бы все `sent`-доставки за всё время хранения. Это надмножество
/// кандидатов: запись получает `status = 'sent'` и `updated_at` в момент
/// постановки в outbox, `o.sent_at` не раньше этого момента, а `updated_at`
/// только растёт. Сутки запаса покрывают задержку outbox между постановкой и
/// отправкой, поэтому любая доставка с `o.sent_at >= $1` проходит предфильтр.
///
/// `o.chat_id = u.telegram_chat_id` отсекает сводки, доставленные в прежний
/// чат получателя: их `message_id` принадлежит старому чату, править его в
/// новом нельзя (как `replace_previous` в `digest.rs`).
const CANDIDATE_WHERE: &str = "d.kind = 'digest'
     AND d.status = 'sent'
     AND d.updated_at >= $1 - INTERVAL '1 day'
     AND d.digest_items IS NOT NULL
     AND d.dismissed_at IS NULL
     AND o.message_id IS NOT NULL
     AND o.sent_at >= $1
     AND o.chat_id = u.telegram_chat_id
     AND u.id IS NOT NULL AND u.deleted_at IS NULL
     AND u.household_id = d.household_id
     AND h.id IS NOT NULL AND h.deleted_at IS NULL
     AND u.telegram_user_id IS NOT NULL
     AND u.telegram_chat_id IS NOT NULL
     AND NOT EXISTS (
         SELECT 1 FROM notification_deliveries newer
         WHERE newer.user_id = d.user_id AND newer.kind = 'digest'
           AND newer.status IN ('pending', 'sent')
           AND (newer.created_at, newer.id) > (d.created_at, d.id)
     )";

/// Источники выборки: доставка, её строка outbox, получатель и household.
const FROM: &str = "FROM notification_deliveries d
     JOIN outbox o ON o.seq = d.outbox_seq
     JOIN users u ON u.id = d.user_id
     JOIN households h ON h.id = d.household_id";

/// Поля кандидата под блокировкой: получатель и сообщение проверены условиями.
#[derive(FromRow)]
struct SyncRow {
    id: Uuid,
    household_id: Uuid,
    digest_hash: Option<String>,
    telegram_user_id: i64,
    telegram_chat_id: i64,
    message_id: i64,
}

/// Синхронизировать отправленные сводки; возвращает число правок. Ошибка одной
/// записи логируется и не прерывает остальные, но возвращается наверх, чтобы
/// тик сообщил её с названием шага.
pub(crate) async fn sync(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let sent_since = now - Duration::hours(SENT_WINDOW_HOURS);
    let sql = format!("SELECT d.id {FROM} WHERE {CANDIDATE_WHERE} ORDER BY d.created_at, d.id");
    let ids: Vec<Uuid> = sqlx::query_scalar(&sql)
        .bind(sent_since)
        .fetch_all(pool)
        .await?;

    let mut changed = 0;
    let mut last_error = None;
    for id in ids {
        match sync_row(pool, id, now, sent_since).await {
            Ok(rows) => changed += rows,
            Err(error) => {
                tracing::error!(
                    delivery_id = %id,
                    %error,
                    "failed to sync digest delivery"
                );
                last_error = Some(error);
            }
        }
    }
    match last_error {
        Some(error) => Err(error),
        None => Ok(changed),
    }
}

/// Перечитать запись под блокировкой и обработать её по свежему состоянию.
///
/// `0` — строку держит другая транзакция, она уже не подходит под условия или
/// отрисовка не изменилась.
async fn sync_row(
    pool: &PgPool,
    id: Uuid,
    now: DateTime<Utc>,
    sent_since: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let sql = format!(
        "SELECT d.id, d.household_id, d.digest_hash,
                u.telegram_user_id, u.telegram_chat_id, o.message_id
         {FROM}
         WHERE d.id = $2 AND {CANDIDATE_WHERE}
         FOR UPDATE OF d SKIP LOCKED"
    );
    let row: Option<SyncRow> = sqlx::query_as(&sql)
        .bind(sent_since)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let changed = match row {
        Some(row) => process(&mut tx, &row, now).await?,
        None => 0,
    };
    tx.commit().await?;
    Ok(changed)
}

/// Перерисовать сводку, если отпечаток текущей отрисовки разошёлся с
/// `digest_hash`, и записать новый отпечаток в той же транзакции.
async fn process(
    conn: &mut PgConnection,
    row: &SyncRow,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let view = digest_view(conn, row.id).await?;
    let ui = notify::digest_ui(&view);
    let fingerprint = notify::ui_fingerprint(&ui);
    if row.digest_hash.as_deref() == Some(fingerprint.as_str()) {
        return Ok(0);
    }
    let html =
        notify::render_notice(conn, row.telegram_user_id, row.household_id, &ui, now).await?;
    outbox::enqueue(
        conn,
        row.telegram_chat_id,
        &Outgoing::Edit {
            message_id: row.message_id,
            html,
            resend_if_gone: false,
        },
        None,
        now,
    )
    .await?;
    sqlx::query(
        "UPDATE notification_deliveries
         SET digest_hash = $2, updated_at = $3
         WHERE id = $1",
    )
    .bind(row.id)
    .bind(&fingerprint)
    .bind(now)
    .execute(conn)
    .await?;
    Ok(1)
}
