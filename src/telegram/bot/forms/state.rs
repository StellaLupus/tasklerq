//! Применение значений к черновику: конечный автомат шагов формы.

use chrono::{Datelike, NaiveDate};

use crate::domain::models::{ListKind, RecurrenceFreq};
use crate::domain::validation::{MAX_NOTES, MAX_SUBTASKS, MAX_TAG, MAX_TAGS, MAX_TITLE, MAX_UNIT};
use crate::telegram::ru;

use super::MAX_QUANTITY;
use crate::telegram::bot::Ctx;
use crate::telegram::bot::model::{
    Choice, ItemDraft, ItemForm, RecurrenceDraft, Step, SubtaskDraft,
};

pub(super) fn today(ctx: &Ctx<'_>) -> NaiveDate {
    ctx.now.with_timezone(&ctx.tz).date_naive()
}

pub(super) fn apply_choice(
    draft: &mut ItemDraft,
    choice: Choice,
    today: NaiveDate,
) -> Result<(), String> {
    match choice {
        Choice::List(_) => {}
        Choice::DueToday => {
            draft.date = Some(today);
            draft.time = None;
        }
        Choice::DueTomorrow => {
            draft.date = Some(today + chrono::Duration::days(1));
            draft.time = None;
        }
        Choice::DueNone => {
            draft.date = None;
            draft.time = None;
        }
        Choice::Priority(priority) => draft.priority = priority,
        Choice::Assignee(assignee) => draft.assignee = assignee,
        Choice::TagToggle(name) => {
            if let Some(position) = draft.tags.iter().position(|tag| *tag == name) {
                draft.tags.remove(position);
            } else if draft.tags.len() >= MAX_TAGS {
                return Err(format!("Можно не больше {MAX_TAGS} тегов."));
            } else {
                draft.tags.push(name);
            }
        }
        Choice::SubtaskToggle(index) => {
            if let Some(subtask) = draft.subtasks.get_mut(index) {
                subtask.done = !subtask.done;
            }
        }
        Choice::SubtaskRemove(index) => {
            if index < draft.subtasks.len() {
                draft.subtasks.remove(index);
            }
        }
        Choice::Freq(None) => draft.recurrence = None,
        Choice::Freq(Some(freq)) => {
            let anchor = draft.date.unwrap_or(today);
            let weekday = i16::try_from(anchor.weekday().number_from_monday()).unwrap_or(1);
            draft.recurrence = Some(RecurrenceDraft::new(freq, weekday));
        }
        Choice::Weekday(day) => {
            let rule = draft
                .recurrence
                .as_mut()
                .ok_or_else(|| "Сначала выберите, как часто повторять.".to_owned())?;
            if let Some(position) = rule.weekdays.iter().position(|d| *d == day) {
                if rule.weekdays.len() == 1 {
                    return Err("Выберите хотя бы один день недели.".to_owned());
                }
                rule.weekdays.remove(position);
            } else {
                rule.weekdays.push(day);
                rule.weekdays.sort_unstable();
            }
        }
        Choice::MonthRule(month_rule) => {
            if let Some(rule) = draft.recurrence.as_mut() {
                rule.month_rule = Some(month_rule);
            }
        }
        Choice::Interval(interval) => {
            if !(1..=365).contains(&interval) {
                return Err("Интервал — от 1 до 365.".to_owned());
            }
            if let Some(rule) = draft.recurrence.as_mut() {
                rule.interval = interval;
            }
        }
        Choice::Quantity(quantity) => draft.quantity = Some(quantity),
        Choice::Unit(unit) => draft.unit = Some(unit),
    }
    Ok(())
}

pub(super) fn apply_text(
    step: Step,
    draft: &mut ItemDraft,
    text: &str,
    today: NaiveDate,
    now_time: chrono::NaiveTime,
) -> Result<(), String> {
    let text = text.trim();
    match step {
        Step::Title => {
            if text.is_empty() {
                return Err("Название не может быть пустым.".to_owned());
            }
            if text.chars().count() > MAX_TITLE {
                return Err(format!(
                    "Название слишком длинное: не больше {MAX_TITLE} символов."
                ));
            }
            draft.title = text.to_owned();
        }
        Step::DueText => {
            let parsed = ru::parse_due(text, today)
                .ok_or_else(|| format!("Не понял срок. {}", ru::DUE_HINT))?;
            let date = parsed.date.or(draft.date).unwrap_or(today);
            if date < today {
                return Err("Дата уже прошла.".to_owned());
            }
            if date == today && parsed.time.is_some_and(|time| time < now_time) {
                return Err("Это время сегодня уже прошло.".to_owned());
            }
            draft.date = Some(date);
            draft.time = parsed.time;
        }
        Step::NewTag => {
            let name = text.to_lowercase();
            if name.is_empty() || name.chars().count() > MAX_TAG {
                return Err(format!("Тег — от 1 до {MAX_TAG} символов."));
            }
            if !draft.tags.contains(&name) {
                if draft.tags.len() >= MAX_TAGS {
                    return Err(format!("Можно не больше {MAX_TAGS} тегов."));
                }
                draft.tags.push(name);
            }
        }
        Step::Subtasks => {
            if text.is_empty() || text.chars().count() > MAX_TITLE {
                return Err(format!("Подзадача — от 1 до {MAX_TITLE} символов."));
            }
            if draft.subtasks.len() >= MAX_SUBTASKS {
                return Err(format!("Можно не больше {MAX_SUBTASKS} подзадач."));
            }
            draft.subtasks.push(SubtaskDraft {
                title: text.to_owned(),
                done: false,
            });
        }
        Step::Interval => {
            let interval: i16 = text
                .parse()
                .ok()
                .filter(|value| (1..=365).contains(value))
                .ok_or_else(|| "Интервал — число от 1 до 365.".to_owned())?;
            if let Some(rule) = draft.recurrence.as_mut() {
                rule.interval = interval;
            }
        }
        Step::Quantity => {
            let quantity = ru::parse_number(text)
                .filter(|value| *value <= MAX_QUANTITY)
                .ok_or_else(|| {
                    format!("Количество — число больше 0 и не больше {MAX_QUANTITY}.")
                })?;
            draft.quantity = Some(quantity);
        }
        Step::Unit => {
            if text.chars().count() > MAX_UNIT {
                return Err(format!("Единица — не длиннее {MAX_UNIT} символов."));
            }
            draft.unit = (!text.is_empty()).then(|| text.to_owned());
        }
        Step::Notes => {
            if text == "-" {
                draft.notes = None;
            } else if text.chars().count() > MAX_NOTES {
                return Err(format!(
                    "Описание слишком длинное: не больше {MAX_NOTES} символов."
                ));
            } else {
                draft.notes = Some(text.to_owned());
            }
        }
        Step::List
        | Step::Due
        | Step::Priority
        | Step::Assignee
        | Step::Tags
        | Step::Recurrence
        | Step::Weekdays
        | Step::MonthRule
        | Step::Preview => {
            return Err("Здесь нужно нажать кнопку.".to_owned());
        }
    }
    Ok(())
}

/// Следующий шаг создания после завершённого.
pub(super) fn advance(form: &mut ItemForm, finished: Step) {
    let shopping = form.kind != ListKind::Tasks;
    let (next, leaf) = match finished {
        Step::List => (Step::Title, false),
        Step::Title => (if shopping { Step::Quantity } else { Step::Due }, true),
        Step::Due | Step::DueText => (Step::Priority, true),
        Step::Priority => (Step::Assignee, true),
        Step::Assignee => (if shopping { Step::Preview } else { Step::Tags }, true),
        Step::Tags | Step::NewTag => (Step::Subtasks, true),
        Step::Subtasks => (Step::Recurrence, true),
        Step::Recurrence => match form.draft.recurrence.as_ref().map(|rule| rule.freq) {
            None => (Step::Preview, true),
            Some(RecurrenceFreq::Weekly) => (Step::Weekdays, false),
            Some(RecurrenceFreq::Monthly) => (Step::MonthRule, false),
            Some(_) => (Step::Interval, false),
        },
        Step::Weekdays | Step::MonthRule => (Step::Interval, false),
        Step::Interval => (Step::Preview, false),
        Step::Quantity => (Step::Unit, true),
        Step::Unit | Step::Notes | Step::Preview => (Step::Preview, false),
    };
    form.step = if form.from_preview && leaf {
        Step::Preview
    } else {
        next
    };
    if form.step == Step::Preview {
        form.from_preview = false;
    }
    form.error = None;
}

#[cfg(test)]
mod tests {
    use chrono::NaiveTime;

    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("date")
    }

    fn noon() -> NaiveTime {
        NaiveTime::from_hms_opt(12, 0, 0).expect("time")
    }

    #[test]
    fn title_is_validated() {
        let mut draft = ItemDraft::default();
        assert!(apply_text(Step::Title, &mut draft, "   ", date(2026, 10, 30), noon()).is_err());
        assert!(
            apply_text(
                Step::Title,
                &mut draft,
                &"я".repeat(201),
                date(2026, 10, 30),
                noon()
            )
            .is_err()
        );
        apply_text(
            Step::Title,
            &mut draft,
            " Молоко ",
            date(2026, 10, 30),
            noon(),
        )
        .expect("ok");
        assert_eq!(draft.title, "Молоко");
    }

    #[test]
    fn past_dates_and_times_are_rejected_in_russian() {
        let today = date(2026, 10, 30);
        let mut draft = ItemDraft::default();

        let error = apply_text(Step::DueText, &mut draft, "29.10", today, noon()).unwrap_err();
        assert_eq!(error, "Дата уже прошла.");
        let error =
            apply_text(Step::DueText, &mut draft, "сегодня 09:00", today, noon()).unwrap_err();
        assert_eq!(error, "Это время сегодня уже прошло.");
        let error =
            apply_text(Step::DueText, &mut draft, "когда-нибудь", today, noon()).unwrap_err();
        assert!(error.starts_with("Не понял срок."), "{error}");

        apply_text(Step::DueText, &mut draft, "завтра 09:00", today, noon()).expect("ok");
        assert_eq!(draft.date, Some(date(2026, 10, 31)));
        assert!(draft.time.is_some());
    }

    #[test]
    fn tag_and_subtask_limits_have_clear_messages() {
        let today = date(2026, 10, 30);
        let mut draft = ItemDraft {
            tags: (0..MAX_TAGS).map(|n| format!("t{n}")).collect(),
            subtasks: (0..MAX_SUBTASKS)
                .map(|n| SubtaskDraft {
                    title: format!("s{n}"),
                    done: false,
                })
                .collect(),
            ..ItemDraft::default()
        };

        assert_eq!(
            apply_text(Step::NewTag, &mut draft, "лишний", today, noon()).unwrap_err(),
            "Можно не больше 10 тегов."
        );
        assert_eq!(
            apply_text(Step::Subtasks, &mut draft, "лишняя", today, noon()).unwrap_err(),
            "Можно не больше 50 подзадач."
        );
        assert_eq!(
            apply_choice(&mut draft, Choice::TagToggle("новый".into()), today).unwrap_err(),
            "Можно не больше 10 тегов."
        );
    }

    #[test]
    fn interval_must_be_between_one_and_365() {
        let today = date(2026, 10, 30);
        let mut draft = ItemDraft {
            recurrence: Some(RecurrenceDraft::new(RecurrenceFreq::Daily, 1)),
            ..ItemDraft::default()
        };

        for bad in ["0", "366", "abc", "-1"] {
            assert_eq!(
                apply_text(Step::Interval, &mut draft, bad, today, noon()).unwrap_err(),
                "Интервал — число от 1 до 365."
            );
        }
        apply_text(Step::Interval, &mut draft, "14", today, noon()).expect("ok");
        assert_eq!(
            draft.recurrence.as_ref().map(|rule| rule.interval),
            Some(14)
        );
    }

    #[test]
    fn last_weekday_cannot_be_removed() {
        let today = date(2026, 10, 30);
        let mut draft = ItemDraft {
            recurrence: Some(RecurrenceDraft::new(RecurrenceFreq::Weekly, 5)),
            ..ItemDraft::default()
        };

        assert!(apply_choice(&mut draft, Choice::Weekday(5), today).is_err());
        apply_choice(&mut draft, Choice::Weekday(1), today).expect("add");
        apply_choice(&mut draft, Choice::Weekday(5), today).expect("remove");
        assert_eq!(
            draft.recurrence.as_ref().map(|rule| rule.weekdays.clone()),
            Some(vec![1])
        );
    }

    #[test]
    fn creation_flow_skips_to_preview_when_editing_from_it() {
        let mut form = ItemForm {
            list_id: None,
            kind: ListKind::Tasks,
            step: Step::Due,
            draft: ItemDraft::default(),
            from_preview: true,
            error: None,
            duplicate: None,
            target: None,
        };

        advance(&mut form, Step::Due);
        assert_eq!(form.step, Step::Preview);
        assert!(!form.from_preview);

        form.step = Step::Title;
        advance(&mut form, Step::Title);
        assert_eq!(form.step, Step::Due);
    }

    #[test]
    fn recurrence_choice_chains_to_details_even_from_preview() {
        let mut form = ItemForm {
            list_id: None,
            kind: ListKind::Tasks,
            step: Step::Recurrence,
            draft: ItemDraft {
                recurrence: Some(RecurrenceDraft::new(RecurrenceFreq::Weekly, 1)),
                ..ItemDraft::default()
            },
            from_preview: true,
            error: None,
            duplicate: None,
            target: None,
        };

        advance(&mut form, Step::Recurrence);
        assert_eq!(form.step, Step::Weekdays);
        advance(&mut form, Step::Weekdays);
        assert_eq!(form.step, Step::Interval);
        advance(&mut form, Step::Interval);
        assert_eq!(form.step, Step::Preview);
    }
}
