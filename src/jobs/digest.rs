//! Утренняя сводка: планирование, содержимое и текст сообщения.

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use super::delivery::{self, DeliveryScope};
use super::keys::{digest_key_date, item_occurrence_key};
use super::notice::NoticeRow;
use super::quiet::{desired_schedule, resolved_timezone};
use crate::domain::events::effective_end;
use crate::domain::models::{ItemStatus, ListKind, UserRole};
use crate::domain::recurrence::resolve_local;
use crate::domain::settings::{DigestScope, NotificationSettings, parse_hhmm};
use crate::storage::repositories::items::completions::{self, Completion};
use crate::storage::repositories::notifications::{self, NewDelivery, NotificationKind};
use crate::storage::repositories::{items, users};
use crate::telegram::bot::notify;
use crate::telegram::ru;

pub use super::keys::digest_occurrence_key;

/// Время утренней сводки по умолчанию.
const DEFAULT_DIGEST_TIME: &str = "08:30";

fn default_digest_time() -> NaiveTime {
    parse_hhmm(DEFAULT_DIGEST_TIME, "digest time").expect("valid default time")
}

/// Отменить `pending deferred`-сводки, которые утром совпали бы с новой.
async fn cancel_deferred_on_same_day(
    conn: &mut PgConnection,
    user_id: Uuid,
    current_key: &str,
    timezone: Tz,
    delivery_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let rows: Vec<(Uuid, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, scheduled_at
         FROM notification_deliveries
         WHERE user_id = $1 AND kind = 'digest' AND status = 'pending'
           AND deferred AND occurrence_key <> $2",
    )
    .bind(user_id)
    .bind(current_key)
    .fetch_all(&mut *conn)
    .await?;

    let delivery_date = delivery_at.with_timezone(&timezone).date_naive();
    let mut cancelled = 0;
    for (id, scheduled_at) in rows {
        if scheduled_at.with_timezone(&timezone).date_naive() != delivery_date {
            continue;
        }
        cancelled += delivery::mark_cancelled(conn, id, now).await?;
    }
    Ok(cancelled)
}

#[derive(FromRow)]
struct DigestCandidate {
    user_id: Uuid,
    household_id: Uuid,
    settings: serde_json::Value,
    user_timezone: Option<String>,
    household_timezone: String,
}

/// Запланировать утренние сводки на текущий локальный день.
///
/// Сводка создаётся только после `digest_time` и живёт до конца локального дня:
/// если сервис был выключен в момент отправки, она уходит при первом тике в тот
/// же день. За прошлые дни запись не создаётся.
pub async fn plan(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let candidates: Vec<DigestCandidate> = sqlx::query_as(
        "SELECT u.id AS user_id, u.household_id, u.settings,
                u.timezone AS user_timezone, h.timezone AS household_timezone
         FROM users u
         JOIN households h ON h.id = u.household_id AND h.deleted_at IS NULL
         WHERE u.deleted_at IS NULL",
    )
    .fetch_all(pool)
    .await?;

    let mut conn = pool.acquire().await?;
    let mut planned = 0;
    let mut last_error = None;
    for candidate in candidates {
        let settings = NotificationSettings::from_settings(&candidate.settings);
        if !settings.digest_enabled {
            continue;
        }
        let timezone = resolved_timezone(
            candidate.user_timezone.as_deref(),
            Some(&candidate.household_timezone),
        );
        let local_date = now.with_timezone(&timezone).date_naive();
        let time = parse_hhmm(&settings.digest_time, "digest time")
            .unwrap_or_else(|_| default_digest_time());
        let scheduled_at = match resolve_local(timezone, local_date, time) {
            Ok(scheduled_at) => scheduled_at,
            Err(error) => {
                tracing::error!(
                    user_id = %candidate.user_id,
                    %error,
                    "failed to resolve digest time"
                );
                continue;
            }
        };
        let occurrence_key = digest_occurrence_key(local_date);

        // Отложенная через полночь сводка прошлого дня и сегодняшняя не должны
        // прийти одним утром. Если сегодняшняя (с учётом тихих часов) попадёт
        // на ту же локальную дату, прошлую отменяем — её заменит новая.
        // При выключенных тихих часах отложенную запись, наоборот, отпускаем
        // на ближайшем тике.
        if settings.quiet_hours_enabled {
            let delivery_at = desired_schedule(scheduled_at, now, timezone, &settings);
            cancel_deferred_on_same_day(
                &mut conn,
                candidate.user_id,
                &occurrence_key,
                timezone,
                delivery_at,
                now,
            )
            .await?;
        }

        if scheduled_at > now {
            continue;
        }

        let delivery = NewDelivery {
            household_id: candidate.household_id,
            user_id: candidate.user_id,
            item_id: None,
            kind: NotificationKind::Digest,
            occurrence_key: &occurrence_key,
            origin_key: None,
            replaces_message_id: None,
            scheduled_at,
        };
        match notifications::insert(&mut conn, &delivery, now).await {
            Ok(rows) => planned += rows,
            Err(error) => {
                tracing::error!(
                    user_id = %candidate.user_id,
                    %error,
                    "failed to plan digest delivery"
                );
                // Остальные получатели всё равно планируются: ошибка одной
                // записи не должна отменять шаг целиком, но шаг обязан
                // сообщить о сбое вызывающему.
                last_error = Some(error);
            }
        }
    }

    match last_error {
        Some(error) => Err(error),
        None => Ok(planned),
    }
}

/// Пересчитать время ещё не доставленных сводок по текущим настройкам.
///
/// Базой служит время сводки из `occurrence_key` и текущего `digest_time`, а не
/// `created_at`: поздний тик не должен навсегда сдвигать сводку.
pub async fn reschedule(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    delivery::reschedule_pending(pool, now, DeliveryScope::Digest, digest_intended).await
}

/// Момент сводки по ключу её вхождения и текущим настройкам получателя.
fn digest_intended(row: &NoticeRow) -> Option<DateTime<Utc>> {
    let settings = row.settings();
    let time =
        parse_hhmm(&settings.digest_time, "digest time").unwrap_or_else(|_| default_digest_time());
    Some(
        digest_key_date(&row.occurrence_key)
            .and_then(|date| resolve_local(row.timezone(), date, time).ok())
            .unwrap_or(row.created_at),
    )
}

#[derive(FromRow)]
struct DigestItem {
    id: Uuid,
    kind: ListKind,
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    end_at: Option<DateTime<Utc>>,
    end_date: Option<NaiveDate>,
}

/// Раздел строки снимка сводки на момент доставки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestSection {
    /// «Сегодня».
    Today,
    /// «Просрочено».
    Overdue,
    /// Событие.
    Event,
}

/// Строка снимка состава сводки (design, решение 1).
///
/// Снимок фиксируется при доставке в `notification_deliveries.digest_items`
/// и дальше не меняется: перерисовка читает состав отсюда, а состояние — из
/// `items` и событий выполнения. Первые 20 строк снимка — это 20 строк
/// текущей сводки в том же порядке.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestSnapshotItem {
    /// Элемент: задача или событие.
    pub item_id: Uuid,
    /// Раздел строки на момент доставки.
    pub section: DigestSection,
    /// Ключ вхождения срока (`keys::item_occurrence_key`), как у напоминаний;
    /// у события — ключ начала вхождения тем же способом.
    pub occurrence_key: String,
}

/// Ключ сортировки задач и событий сводки: timed по времени, затем
/// вседневные по дате.
fn digest_sort_key(item: &DigestItem) -> (u8, Option<DateTime<Utc>>, Option<NaiveDate>, Uuid) {
    (
        u8::from(item.due_at.is_none()),
        item.due_at,
        item.due_date,
        item.id,
    )
}

/// Событие на сегодня: начинается сегодня или началось раньше и ещё идёт.
fn event_is_today(
    item: &DigestItem,
    timezone: Tz,
    household_timezone: Tz,
    today: NaiveDate,
    now: DateTime<Utc>,
) -> bool {
    let (start_day, started) = match (item.due_at, item.due_date) {
        (Some(due_at), _) => (
            Some(due_at.with_timezone(&timezone).date_naive()),
            due_at <= now,
        ),
        (None, Some(due_date)) => (Some(due_date), due_date <= today),
        (None, None) => (None, false),
    };
    if start_day == Some(today) {
        return true;
    }
    started
        && effective_end(
            item.due_at,
            item.due_date,
            item.end_at,
            item.end_date,
            household_timezone,
        )
        .is_some_and(|end| end > now)
}

/// Собрать снимок состава сводки в момент доставки.
///
/// Разделы совпадают с экраном списка: timed-задача со сроком в прошлом и
/// вседневная задача с прошедшей датой — «Просрочено», остальные задачи с
/// локальной датой получателя — «Сегодня». В составе «только мои» задачи и
/// события фильтруются общим предикатом «мои» (у события исполнитель —
/// участник, чьё оно). Возвращается упорядоченный снимок всех строк в порядке
/// отрисовки; текущее состояние строк для отрисовки читает [`digest_view`].
async fn content(
    conn: &mut PgConnection,
    household_id: Uuid,
    user_id: Uuid,
    scope: DigestScope,
    timezone: Tz,
    household_timezone: Tz,
    now: DateTime<Utc>,
) -> Result<Vec<DigestSnapshotItem>, sqlx::Error> {
    let mine_for = match scope {
        DigestScope::Mine => Some(user_id),
        DigestScope::All => None,
    };
    let mut rows: Vec<DigestItem> = sqlx::query_as(&format!(
        "SELECT i.id, l.kind, i.due_at, i.due_date, i.end_at, i.end_date
         FROM items i
         JOIN lists l
           ON l.id = i.list_id AND l.deleted_at IS NULL
          AND NOT l.is_archived AND l.kind IN ('tasks', 'events')
         WHERE i.household_id = $1 AND i.status = 'open' AND i.deleted_at IS NULL
           AND (i.due_at IS NOT NULL OR i.due_date IS NOT NULL)
           AND ($2::uuid IS NULL OR {})",
        items::mine_predicate("$2"),
    ))
    .bind(household_id)
    .bind(mine_for)
    .fetch_all(&mut *conn)
    .await?;
    rows.sort_by_key(digest_sort_key);

    let today = now.with_timezone(&timezone).date_naive();
    let mut today_items = Vec::new();
    let mut overdue_items = Vec::new();
    for row in rows {
        // Ключ вхождения строки снимка — как у напоминаний; у события ключ
        // начала вхождения считается тем же способом.
        let occurrence_key = item_occurrence_key(row.due_at, row.due_date).unwrap_or_default();
        if row.kind == ListKind::Events {
            // Событие никогда не попадает в просроченные: прошедшее просто не
            // показывается, а начавшееся раньше и ещё идущее — в «Сегодня».
            if event_is_today(&row, timezone, household_timezone, today, now) {
                today_items.push(DigestSnapshotItem {
                    item_id: row.id,
                    section: DigestSection::Event,
                    occurrence_key,
                });
            }
            continue;
        }
        match (row.due_at, row.due_date) {
            (Some(due_at), None) if due_at < now => {
                overdue_items.push(DigestSnapshotItem {
                    item_id: row.id,
                    section: DigestSection::Overdue,
                    occurrence_key,
                });
            }
            (Some(due_at), None) if due_at.with_timezone(&timezone).date_naive() == today => {
                today_items.push(DigestSnapshotItem {
                    item_id: row.id,
                    section: DigestSection::Today,
                    occurrence_key,
                });
            }
            (None, Some(due_date)) if due_date < today => {
                overdue_items.push(DigestSnapshotItem {
                    item_id: row.id,
                    section: DigestSection::Overdue,
                    occurrence_key,
                });
            }
            (None, Some(due_date)) if due_date == today => {
                today_items.push(DigestSnapshotItem {
                    item_id: row.id,
                    section: DigestSection::Today,
                    occurrence_key,
                });
            }
            _ => {}
        }
    }

    // Порядок снимка — порядок отрисовки: «Сегодня», затем «Просрочено».
    Ok(today_items.into_iter().chain(overdue_items).collect())
}

/// Собрать сообщение сводки; содержимое собирается здесь, а не при
/// планировании.
///
/// Снимок состава и отпечаток отправленной отрисовки записываются в запись
/// доставки в той же транзакции, что ставит сообщение в outbox (design,
/// решения 1, 6). Отрисовка строится из только что записанного снимка и
/// текущего состояния через [`digest_view`], поэтому отправленный HTML и
/// `digest_hash` — один и тот же `Ui`.
pub(crate) async fn digest_message(
    conn: &mut PgConnection,
    row: &NoticeRow,
    telegram_user_id: i64,
    now: DateTime<Utc>,
) -> Result<Option<String>, sqlx::Error> {
    let snapshot = content(
        conn,
        row.household_id,
        row.user_id,
        row.settings().digest_scope,
        row.timezone(),
        resolved_timezone(None, row.household_timezone.as_deref()),
        now,
    )
    .await?;
    sqlx::query(
        "UPDATE notification_deliveries
         SET digest_items = $2, updated_at = $3
         WHERE id = $1",
    )
    .bind(row.id)
    .bind(serde_json::to_value(&snapshot).expect("digest snapshot serializes"))
    .bind(now)
    .execute(&mut *conn)
    .await?;

    let view = digest_view(conn, row.id).await?;
    let ui = notify::digest_ui(&view);
    let html = notify::render_notice(conn, telegram_user_id, row.household_id, &ui, now).await?;
    sqlx::query(
        "UPDATE notification_deliveries
         SET digest_hash = $2, updated_at = $3
         WHERE id = $1",
    )
    .bind(row.id)
    .bind(notify::ui_fingerprint(&ui))
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(Some(html))
}

/// Запись доставки сводки с данными получателя для [`digest_view`].
#[derive(FromRow)]
struct DigestDelivery {
    digest_items: Option<serde_json::Value>,
    occurrence_key: String,
    created_at: DateTime<Utc>,
    user_id: Uuid,
    household_id: Uuid,
    user_timezone: Option<String>,
    settings: serde_json::Value,
    role: UserRole,
    household_timezone: String,
}

/// Текущее состояние элемента снимка.
#[derive(FromRow)]
struct ViewItem {
    id: Uuid,
    title: String,
    status: ItemStatus,
    deleted_at: Option<DateTime<Utc>>,
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    end_at: Option<DateTime<Utc>>,
    end_date: Option<NaiveDate>,
    assignee_user_id: Option<Uuid>,
    version: i32,
    recurring: bool,
    list_kind: Option<String>,
    list_archived: Option<bool>,
    list_deleted_at: Option<DateTime<Utc>>,
}

/// Построить представление сводки из снимка доставки и текущего состояния
/// задач (0019, design, решения 1, 3, 4, 5).
///
/// Эту же функцию вызывают обработчик касания (пункт 4.1) и шаг
/// `sync_digests` (пункт 5.1). Состав строк берётся из снимка, состояние —
/// из `items` и событий выполнения; фильтр состава применяется по
/// **текущему** исполнителю задачи и участнику события тем же правилом
/// «мои», что [`content`] (в «только мои» ставшие чужими строки скрыты, при
/// снятии исполнителя — снова видны с «👥»). Запросы идут по всем строкам
/// снимка сразу (`= ANY`), не по одной на строку.
///
/// Функция не принимает «сейчас» и не зависит от текущего времени: день
/// сводки берётся из ключа доставки, все подписи относительны к нему, поэтому
/// отпечаток отрисовки не меняется с течением времени и полночь не вызывает
/// правок (design, решение 5).
pub async fn digest_view(
    conn: &mut PgConnection,
    delivery_id: Uuid,
) -> Result<notify::DigestView, sqlx::Error> {
    let delivery: DigestDelivery = sqlx::query_as(
        "SELECT d.digest_items, d.occurrence_key, d.created_at, d.user_id, d.household_id,
                u.timezone AS user_timezone, u.settings, u.role,
                h.timezone AS household_timezone
         FROM notification_deliveries d
         JOIN users u ON u.id = d.user_id
         JOIN households h ON h.id = d.household_id
         WHERE d.id = $1",
    )
    .bind(delivery_id)
    .fetch_one(&mut *conn)
    .await?;

    let timezone = resolved_timezone(
        delivery.user_timezone.as_deref(),
        Some(&delivery.household_timezone),
    );
    // Признак ручной сводки — суффикс `#manual` (design, решение 7); день —
    // дата ключа до суффикса. Пока `digest_key_date` не понимает суффикс
    // (пункт 6.1), отсекаем его здесь.
    let manual = delivery.occurrence_key.contains("#manual");
    let day = digest_key_date(
        delivery
            .occurrence_key
            .split('#')
            .next()
            .unwrap_or(&delivery.occurrence_key),
    )
    .unwrap_or_else(|| delivery.created_at.with_timezone(&timezone).date_naive());
    let scope = NotificationSettings::from_settings(&delivery.settings).digest_scope;
    let snapshot: Vec<DigestSnapshotItem> = delivery
        .digest_items
        .map(|value| serde_json::from_value(value).unwrap_or_default())
        .unwrap_or_default();

    let item_ids: Vec<Uuid> = snapshot.iter().map(|line| line.item_id).collect();
    let items: Vec<ViewItem> = sqlx::query_as(
        "SELECT i.id, i.title, i.status, i.deleted_at, i.due_at, i.due_date,
                i.end_at, i.end_date, i.assignee_user_id, i.version,
                (i.recurrence_freq IS NOT NULL) AS recurring,
                l.kind::text AS list_kind, l.is_archived AS list_archived,
                l.deleted_at AS list_deleted_at
         FROM items i
         LEFT JOIN lists l ON l.id = i.list_id
         WHERE i.household_id = $1 AND i.id = ANY($2)",
    )
    .bind(delivery.household_id)
    .bind(&item_ids)
    .fetch_all(&mut *conn)
    .await?;
    let items: std::collections::HashMap<Uuid, ViewItem> =
        items.into_iter().map(|item| (item.id, item)).collect();

    let completions = completions::active_for_items(conn, delivery.household_id, &item_ids)
        .await
        .map_err(domain_db)?;
    let mut completions_by_item: std::collections::HashMap<Uuid, Vec<Completion>> =
        std::collections::HashMap::new();
    for completion in completions {
        completions_by_item
            .entry(completion.item_id)
            .or_default()
            .push(completion);
    }

    let members = users::list_active(conn, delivery.household_id)
        .await
        .map_err(domain_db)?;
    let member = |user_id: Uuid| members.iter().find(|member| member.id == user_id);

    let mut lines = Vec::new();
    for line in &snapshot {
        let item = items.get(&line.item_id);
        // Фильтр состава по текущему исполнителю/участнику — то же правило,
        // что items::mine_predicate у content: назначена получателю или общая.
        if scope == DigestScope::Mine
            && !item.is_none_or(|item| {
                item.assignee_user_id
                    .is_none_or(|assignee| assignee == delivery.user_id)
            })
        {
            continue;
        }
        let title = item
            .map(|item| item.title.clone())
            .unwrap_or_else(|| completions::UNTITLED.to_owned());
        let is_event = line.section == DigestSection::Event;
        let gone = item.is_none_or(|item| {
            item.deleted_at.is_some() || item.list_deleted_at.is_some() || item.list_kind.is_none()
        });
        let state = if gone {
            if is_event {
                notify::DigestLineState::EventDeleted { title }
            } else {
                notify::DigestLineState::Deleted { title }
            }
        } else if item.is_some_and(|item| item.list_archived == Some(true)) {
            notify::DigestLineState::Archived { title }
        } else if is_event {
            let item = item.expect("item checked");
            notify::DigestLineState::Event {
                title,
                when: snapshot_event_when(&line.occurrence_key, item, timezone, day),
            }
        } else {
            let item = item.expect("item checked");
            // Выполненность — по вхождению из снимка: действующее выполнение,
            // чей плановый срок даёт ключ снимка; у неповторяющейся задачи —
            // также статус «выполнено» (design, решение 2).
            let completion = completions_by_item.get(&line.item_id).and_then(|list| {
                list.iter()
                    .find(|completion| {
                        occurrence_matches(
                            &line.occurrence_key,
                            completion.due_at,
                            completion.due_date,
                        )
                    })
                    .or_else(|| {
                        (!item.recurring && item.status == ItemStatus::Done)
                            .then(|| list.first())
                            .flatten()
                    })
            });
            // Выполненная неповторяющаяся задача без действующего события
            // выполнения (например, событие удалено хранением): выполненность
            // определяется статусом «выполнено» (design, решение 2). Имени и
            // кнопки нет — `can_undo` не на чем проверять.
            let done_without_event =
                !item.recurring && item.status == ItemStatus::Done && completion.is_none();
            match completion {
                Some(completion) => {
                    let actor_name = notify::member_name(
                        completion.actor_user_id,
                        delivery.user_id,
                        completion.actor_user_id.and_then(&member),
                    );
                    // «Следующее …» — текущий срок повторяющейся задачи.
                    let next = item.recurring && item.status == ItemStatus::Open;
                    notify::DigestLineState::Completed {
                        title,
                        actor_name,
                        next_due_at: next.then_some(item.due_at).flatten(),
                        next_due_date: next.then_some(item.due_date).flatten(),
                        version: item.version,
                        can_undo: items::can_undo(
                            completion.actor_user_id,
                            delivery.user_id,
                            delivery.role,
                        ),
                        completion_event: Some(completion.event_id),
                    }
                }
                None if done_without_event => notify::DigestLineState::Completed {
                    title,
                    actor_name: None,
                    next_due_at: None,
                    next_due_date: None,
                    version: item.version,
                    can_undo: false,
                    completion_event: None,
                },
                None => notify::DigestLineState::Open {
                    title,
                    due_at: item.due_at,
                    due_date: item.due_date,
                    version: item.version,
                    assignee: item.assignee_user_id,
                    assignee_name: notify::member_name(
                        item.assignee_user_id,
                        delivery.user_id,
                        item.assignee_user_id.and_then(&member),
                    ),
                },
            }
        };
        lines.push(notify::DigestViewLine {
            item_id: line.item_id,
            section: line.section,
            state,
        });
    }

    let shopping_open: i64 = sqlx::query_scalar(
        "SELECT count(*)
         FROM items i
         JOIN lists l
           ON l.id = i.list_id AND l.deleted_at IS NULL
          AND NOT l.is_archived AND l.kind = 'shopping'
         WHERE i.household_id = $1 AND i.status = 'open' AND i.deleted_at IS NULL",
    )
    .bind(delivery.household_id)
    .fetch_one(&mut *conn)
    .await?;

    let lists: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT kind::text, id
         FROM lists
         WHERE household_id = $1 AND deleted_at IS NULL AND NOT is_archived
           AND kind IN ('tasks', 'shopping')
         ORDER BY is_default DESC, position, created_at, id",
    )
    .bind(delivery.household_id)
    .fetch_all(&mut *conn)
    .await?;
    let find = |wanted: &str| {
        lists
            .iter()
            .find(|(kind, _)| kind == wanted)
            .map(|(_, id)| *id)
    };

    Ok(notify::DigestView {
        delivery_id,
        kind: if manual {
            notify::DigestKind::Manual
        } else {
            notify::DigestKind::Morning
        },
        day,
        timezone,
        scope,
        recipient_id: delivery.user_id,
        lines,
        shopping_open,
        tasks_list: find("tasks"),
        shopping_list: find("shopping"),
    })
}

/// Совпадение вхождения по разобранным значениям, а не строкам: срок с долями
/// секунды должен совпасть точно (как в синхронизации уведомлений 0018).
fn occurrence_matches(
    key: &str,
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
) -> bool {
    if let Some(date) = key.strip_prefix("date:") {
        return due_at.is_none() && NaiveDate::parse_from_str(date, "%Y-%m-%d").ok() == due_date;
    }
    let Ok(at) = DateTime::parse_from_rfc3339(key) else {
        return false;
    };
    due_date.is_none() && due_at == Some(at.with_timezone(&Utc))
}

/// Время события для строки сводки: начало — из ключа снимка, а не из
/// текущего срока (повторяющееся событие, сдвинутое на следующее вхождение,
/// остаётся со своим временем); длительность — из текущего состояния.
/// Подписи относительны к дню сводки.
fn snapshot_event_when(key: &str, item: &ViewItem, timezone: Tz, day: NaiveDate) -> String {
    if let Some(date) = key.strip_prefix("date:") {
        let Ok(start_date) = NaiveDate::parse_from_str(date, "%Y-%m-%d") else {
            return String::new();
        };
        let end_date = match (item.due_date, item.end_date) {
            (Some(due), Some(end)) if end > due => Some(start_date + (end - due)),
            _ => None,
        };
        return match end_date {
            Some(end) if end > start_date => format!(
                "{} — {}",
                ru::date_label(start_date, day),
                ru::date_label(end, day)
            ),
            _ if start_date == day => ru::ALL_DAY.to_owned(),
            _ => ru::date_label(start_date, day),
        };
    }
    let Ok(start) = DateTime::parse_from_rfc3339(key) else {
        return String::new();
    };
    let start = start.with_timezone(&Utc);
    let end = match (item.due_at, item.end_at) {
        (Some(due), Some(end)) if end > due => Some(start + (end - due)),
        _ => None,
    };
    let start_local = start.with_timezone(&timezone);
    let start_day = start_local.date_naive();
    let prefix = if start_day == day {
        String::new()
    } else {
        format!("{}, ", ru::date_label(start_day, day))
    };
    match end {
        Some(end) => {
            let end_local = end.with_timezone(&timezone);
            if end_local.date_naive() == start_day {
                format!(
                    "{prefix}{}–{}",
                    start_local.format("%H:%M"),
                    end_local.format("%H:%M")
                )
            } else {
                format!(
                    "{prefix}{} – {}, {}",
                    start_local.format("%H:%M"),
                    ru::date_label(end_local.date_naive(), day),
                    end_local.format("%H:%M")
                )
            }
        }
        None => format!("{prefix}{}", start_local.format("%H:%M")),
    }
}

/// Ошибка БД доменного слоя как ошибка sqlx; прочие варианты не возникают у
/// запросов выборки выполнений и участников.
fn domain_db(error: crate::domain::error::DomainError) -> sqlx::Error {
    match error {
        crate::domain::error::DomainError::Database(error) => error,
        other => sqlx::Error::Protocol(other.to_string()),
    }
}

/// Окно поиска прежней сводки для замены: 47 часов.
///
/// Telegram даёт удалять сообщения только в течение 48 часов; час запаса
/// учитывает очередь outbox и расхождение часов, чтобы удаление не уперлось
/// в лимит уже после доставки новой сводки (design 0017, решение 6). Строки
/// outbox при этом живут не меньше 48 часов (`jobs::retention`).
const REPLACE_WINDOW_HOURS: i64 = 47;

/// Отобрать прежние сводки получателя, которые уберёт новая.
///
/// Годятся только доставленные (`sent`) и ещё не убранные (`dismissed_at IS
/// NULL`) сводки этого получателя, чья строка outbox действительно доставила
/// сообщение в этом чате не раньше [`REPLACE_WINDOW_HOURS`]: более старые
/// Telegram удалить уже не даст. Каждое отобранное сообщение отмечается
/// `mark_dismissed`, его id возвращается для `replaces` новой сводки (0017,
/// решение 6). Функция общая: ежедневная доставка, выпуск отложенной тихими
/// часами по тику и ручной выпуск сходятся в `deliver_one_in_conn`, другой
/// путь доставки сводки (0019) сможет вызвать её сам.
pub(crate) async fn replace_previous(
    conn: &mut PgConnection,
    row: &NoticeRow,
    chat_id: i64,
    now: DateTime<Utc>,
) -> Result<Vec<i64>, sqlx::Error> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT o.message_id
         FROM notification_deliveries d
         JOIN outbox o ON o.seq = d.outbox_seq
         WHERE d.kind = 'digest' AND d.status = 'sent'
           AND d.user_id = $1 AND d.id <> $2 AND d.dismissed_at IS NULL
           AND o.chat_id = $3 AND o.message_id IS NOT NULL
           AND o.sent_at >= $4",
    )
    .bind(row.user_id)
    .bind(row.id)
    .bind(chat_id)
    .bind(now - Duration::hours(REPLACE_WINDOW_HOURS))
    .fetch_all(&mut *conn)
    .await?;
    for id in &ids {
        notifications::mark_dismissed(conn, chat_id, *id, now).await?;
    }
    Ok(ids)
}

/// Немедленно выпустить отложенные сводки получателя.
///
/// Вызывается из обработчика настроек в его транзакции после сводного
/// сообщения. Записи блокируются `FOR UPDATE SKIP LOCKED`, поэтому
/// планировщик не отправит ту же сводку второй раз.
pub(crate) async fn release_deferred(
    conn: &mut PgConnection,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT d.id FROM notification_deliveries d
         WHERE d.user_id = $1 AND d.status = 'pending' AND d.kind = 'digest'
           AND d.deferred
         ORDER BY d.scheduled_at, d.id
         FOR UPDATE OF d SKIP LOCKED",
    )
    .bind(user_id)
    .fetch_all(&mut *conn)
    .await?;

    let mut delivered = 0;
    for id in ids {
        delivered +=
            delivery::deliver_one_in_conn(conn, DeliveryScope::Digest, id, user_id, now, false)
                .await?;
    }
    Ok(delivered)
}
