//! Профиль и настройки пользователя.

use chrono::NaiveTime;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::error::DomainError;
use super::recurrence::parse_timezone;
use super::validation::normalize_text;

/// Максимальная длина отображаемого имени.
pub const MAX_DISPLAY_NAME: usize = 64;
/// Минимальный lead time напоминания, минут.
pub const MIN_LEAD_MINUTES: u16 = 5;
/// Максимальный lead time напоминания, минут.
pub const MAX_LEAD_MINUTES: u16 = 1440;
/// Ключ настроек уведомлений в `users.settings`.
pub const NOTIFICATIONS_KEY: &str = "notifications";

/// Часовые пояса, которые бот предлагает при ошибке ввода.
pub const SUGGESTED_TIMEZONES: &[&str] = &[
    "Europe/Kaliningrad",
    "Europe/Moscow",
    "Europe/Samara",
    "Asia/Yekaterinburg",
    "Asia/Omsk",
    "Asia/Krasnoyarsk",
    "Asia/Irkutsk",
    "Asia/Yakutsk",
    "Asia/Vladivostok",
    "Asia/Magadan",
    "Asia/Kamchatka",
    "Europe/Kyiv",
    "Europe/Minsk",
    "Asia/Almaty",
    "Asia/Tbilisi",
    "Asia/Yerevan",
    "UTC",
];

/// Настройки уведомлений одного пользователя.
///
/// Отсутствующие в JSON поля принимают значения по умолчанию, поэтому
/// добавление нового поля не требует миграции данных.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationSettings {
    /// Напоминания о сроках.
    pub reminders_enabled: bool,
    /// Утренняя сводка.
    pub digest_enabled: bool,
    /// Уведомления о назначении.
    pub assignment_enabled: bool,
    /// За сколько минут до срока напоминать.
    pub lead_minutes: u16,
    /// Время утренней сводки, `HH:MM`.
    pub digest_time: String,
    /// Время напоминания о задачах на весь день, `HH:MM`.
    pub all_day_time: String,
    /// Тихие часы включены.
    pub quiet_hours_enabled: bool,
    /// Начало тихих часов, `HH:MM`.
    pub quiet_start: String,
    /// Конец тихих часов, `HH:MM`.
    pub quiet_end: String,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            reminders_enabled: true,
            digest_enabled: true,
            assignment_enabled: true,
            lead_minutes: 15,
            digest_time: "08:30".into(),
            all_day_time: "09:00".into(),
            quiet_hours_enabled: true,
            quiet_start: "22:00".into(),
            quiet_end: "08:00".into(),
        }
    }
}

impl NotificationSettings {
    /// Прочитать настройки из `users.settings`; повреждённые и не проходящие
    /// [`validate`](Self::validate) значения заменяются настройками по
    /// умолчанию целиком.
    pub fn from_settings(settings: &Value) -> Self {
        settings
            .get(NOTIFICATIONS_KEY)
            .and_then(|value| serde_json::from_value::<Self>(value.clone()).ok())
            .filter(|parsed| parsed.validate().is_ok())
            .unwrap_or_default()
    }

    /// Проверить значения.
    pub fn validate(&self) -> Result<(), DomainError> {
        if !(MIN_LEAD_MINUTES..=MAX_LEAD_MINUTES).contains(&self.lead_minutes) {
            return Err(DomainError::Validation(format!(
                "lead time must be between {MIN_LEAD_MINUTES} and {MAX_LEAD_MINUTES} minutes"
            )));
        }
        parse_hhmm(&self.digest_time, "digest time")?;
        parse_hhmm(&self.all_day_time, "all-day reminder time")?;
        let start = parse_hhmm(&self.quiet_start, "quiet hours start")?;
        let end = parse_hhmm(&self.quiet_end, "quiet hours end")?;
        if start == end {
            return Err(DomainError::Validation(
                "quiet hours start and end must differ".into(),
            ));
        }
        Ok(())
    }
}

/// Частичное обновление настроек уведомлений.
#[derive(Debug, Clone, Default)]
pub struct NotificationSettingsUpdate {
    /// Напоминания.
    pub reminders_enabled: Option<bool>,
    /// Утренняя сводка.
    pub digest_enabled: Option<bool>,
    /// Уведомления о назначении.
    pub assignment_enabled: Option<bool>,
    /// Lead time, минут.
    pub lead_minutes: Option<u16>,
    /// Время сводки.
    pub digest_time: Option<String>,
    /// Время напоминания о задачах на весь день.
    pub all_day_time: Option<String>,
    /// Тихие часы.
    pub quiet_hours_enabled: Option<bool>,
    /// Начало тихих часов.
    pub quiet_start: Option<String>,
    /// Конец тихих часов.
    pub quiet_end: Option<String>,
}

impl NotificationSettingsUpdate {
    /// Применить обновление к текущим настройкам и проверить результат.
    pub fn apply(
        self,
        mut current: NotificationSettings,
    ) -> Result<NotificationSettings, DomainError> {
        if let Some(value) = self.reminders_enabled {
            current.reminders_enabled = value;
        }
        if let Some(value) = self.digest_enabled {
            current.digest_enabled = value;
        }
        if let Some(value) = self.assignment_enabled {
            current.assignment_enabled = value;
        }
        if let Some(value) = self.lead_minutes {
            current.lead_minutes = value;
        }
        if let Some(value) = self.digest_time {
            current.digest_time = normalize_hhmm(&value, "digest time")?;
        }
        if let Some(value) = self.all_day_time {
            current.all_day_time = normalize_hhmm(&value, "all-day reminder time")?;
        }
        if let Some(value) = self.quiet_hours_enabled {
            current.quiet_hours_enabled = value;
        }
        if let Some(value) = self.quiet_start {
            current.quiet_start = normalize_hhmm(&value, "quiet hours start")?;
        }
        if let Some(value) = self.quiet_end {
            current.quiet_end = normalize_hhmm(&value, "quiet hours end")?;
        }
        current.validate()?;
        Ok(current)
    }
}

/// Изменение профиля пользователя.
#[derive(Debug, Clone, Default)]
pub struct ProfileUpdate {
    /// Новое отображаемое имя.
    pub display_name: Option<String>,
    /// Личный часовой пояс: `Some(None)` сбрасывает его на пояс household.
    pub timezone: Option<Option<String>>,
    /// Настройки уведомлений.
    pub notifications: Option<NotificationSettingsUpdate>,
}

/// Нормализовать и проверить отображаемое имя.
pub fn normalize_display_name(value: &str) -> Result<String, DomainError> {
    normalize_text(value, MAX_DISPLAY_NAME, "display name")
}

/// Проверить IANA-идентификатор часового пояса.
pub fn validate_timezone(value: &str) -> Result<String, DomainError> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 64 {
        return Err(DomainError::Validation(
            "timezone must be a valid IANA identifier of at most 64 characters".into(),
        ));
    }
    parse_timezone(value)?;
    Ok(value.to_owned())
}

/// Разобрать время `HH:MM`.
pub fn parse_hhmm(value: &str, field: &str) -> Result<NaiveTime, DomainError> {
    let valid_shape = value.len() == 5
        && value.as_bytes()[2] == b':'
        && value
            .bytes()
            .enumerate()
            .all(|(index, byte)| index == 2 || byte.is_ascii_digit());
    if !valid_shape {
        return Err(DomainError::Validation(format!(
            "{field} must be in HH:MM format"
        )));
    }
    NaiveTime::parse_from_str(value, "%H:%M")
        .map_err(|_| DomainError::Validation(format!("{field} must be in HH:MM format")))
}

fn normalize_hhmm(value: &str, field: &str) -> Result<String, DomainError> {
    let value = value.trim();
    parse_hhmm(value, field)?;
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn defaults_match_the_specification() {
        let settings = NotificationSettings::default();
        assert_eq!(settings.lead_minutes, 15);
        assert_eq!(settings.digest_time, "08:30");
        assert_eq!(settings.all_day_time, "09:00");
        assert_eq!(settings.quiet_start, "22:00");
        assert_eq!(settings.quiet_end, "08:00");
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn partial_json_falls_back_to_defaults() {
        let settings = NotificationSettings::from_settings(
            &json!({"notifications": {"lead_minutes": 60, "extra": true}}),
        );
        assert_eq!(settings.lead_minutes, 60);
        assert_eq!(settings.digest_time, "08:30");
        assert_eq!(
            NotificationSettings::from_settings(&json!({})),
            NotificationSettings::default()
        );
        assert_eq!(
            NotificationSettings::from_settings(&json!({"notifications": "broken"})),
            NotificationSettings::default()
        );
    }

    #[test]
    fn stored_values_failing_validation_fall_back_to_defaults() {
        assert_eq!(
            NotificationSettings::from_settings(&json!({"notifications": {"lead_minutes": 0}})),
            NotificationSettings::default()
        );
    }

    #[test]
    fn lead_time_is_bounded() {
        for lead in [0, 4, 1441] {
            let update = NotificationSettingsUpdate {
                lead_minutes: Some(lead),
                ..Default::default()
            };
            assert!(update.apply(NotificationSettings::default()).is_err());
        }
        for lead in [5, 60, 1440] {
            let update = NotificationSettingsUpdate {
                lead_minutes: Some(lead),
                ..Default::default()
            };
            assert_eq!(
                update
                    .apply(NotificationSettings::default())
                    .expect("valid lead")
                    .lead_minutes,
                lead
            );
        }
    }

    #[test]
    fn times_require_hh_mm() {
        for value in ["8:30", "24:00", "08:60", "0830", "08:3x", "", " 08:30"] {
            assert!(parse_hhmm(value, "time").is_err(), "{value:?}");
        }
        assert!(parse_hhmm("00:00", "time").is_ok());
        assert!(parse_hhmm("23:59", "time").is_ok());
    }

    #[test]
    fn quiet_hours_must_not_be_empty() {
        let update = NotificationSettingsUpdate {
            quiet_start: Some("22:00".into()),
            quiet_end: Some("22:00".into()),
            ..Default::default()
        };
        assert!(update.apply(NotificationSettings::default()).is_err());
    }

    #[test]
    fn timezone_must_be_iana() {
        assert_eq!(
            validate_timezone(" Europe/Moscow ").expect("valid"),
            "Europe/Moscow"
        );
        assert!(validate_timezone("Mars/Olympus").is_err());
        assert!(validate_timezone("").is_err());
        for suggestion in SUGGESTED_TIMEZONES {
            assert!(validate_timezone(suggestion).is_ok(), "{suggestion}");
        }
    }

    #[test]
    fn display_name_is_trimmed_and_bounded() {
        assert_eq!(normalize_display_name("  Мария ").expect("valid"), "Мария");
        assert!(normalize_display_name("   ").is_err());
        assert!(normalize_display_name(&"я".repeat(65)).is_err());
        assert!(normalize_display_name(&"я".repeat(64)).is_ok());
    }
}
