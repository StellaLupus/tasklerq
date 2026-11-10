//! Общие фикстуры для unit-тестов capture.

use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Europe::Moscow;
use uuid::Uuid;

use super::context::{CaptureContext, ListRef, MemberRef};
use crate::domain::models::{List, ListKind};

/// Пятница, 30 октября 2026, 14:05 по Москве.
pub fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 30, 11, 5, 0).unwrap()
}

/// Строка списка для [`CaptureContext::new`]: заполнены только читаемые поля.
pub fn model_list(name: &str, kind: ListKind, is_default: bool) -> List {
    let now = Utc::now();
    List {
        id: Uuid::now_v7(),
        household_id: Uuid::nil(),
        kind,
        name: name.to_owned(),
        is_default,
        is_archived: false,
        position: 0,
        created_at: now,
        updated_at: now,
        deleted_at: None,
    }
}

pub fn list(name: &str, kind: ListKind, is_default: bool) -> ListRef {
    ListRef {
        id: Uuid::now_v7(),
        name: name.to_owned(),
        kind,
        is_default,
    }
}

pub fn member(display_name: &str, username: Option<&str>) -> MemberRef {
    MemberRef {
        id: Uuid::now_v7(),
        display_name: display_name.to_owned(),
        username: username.map(ToOwned::to_owned),
    }
}

/// Household «Москва»: списки «Задачи», «Покупки», «Дача»; участники Анна и Пётр.
pub fn context() -> CaptureContext {
    let members = vec![member("Анна", Some("anna")), member("Пётр", None)];
    CaptureContext {
        now: now(),
        timezone: Moscow,
        lists: vec![
            list("Задачи", ListKind::Tasks, true),
            list("Покупки", ListKind::Shopping, true),
            list("Дача", ListKind::Tasks, false),
        ],
        author: members.first().cloned(),
        members,
    }
}
