//! Общие фикстуры для unit-тестов capture.

use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Europe::Moscow;
use uuid::Uuid;

use super::context::{CaptureContext, ListRef, MemberRef};
use crate::domain::models::ListKind;

/// Пятница, 30 октября 2026, 14:05 по Москве.
pub fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 30, 11, 5, 0).unwrap()
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
    CaptureContext {
        now: now(),
        timezone: Moscow,
        lists: vec![
            list("Задачи", ListKind::Tasks, true),
            list("Покупки", ListKind::Shopping, true),
            list("Дача", ListKind::Tasks, false),
        ],
        members: vec![member("Анна", Some("anna")), member("Пётр", None)],
    }
}
