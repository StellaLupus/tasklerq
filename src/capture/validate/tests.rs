use chrono::TimeZone;

use super::*;
use crate::capture::test_support::{context, list, member};

fn raw(kind: ListKind, title: &str) -> RawItem {
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

fn task() -> RawItem {
    raw(ListKind::Tasks, "Позвонить маме")
}

fn check(raw: RawItem) -> DraftItem {
    validate_item(&context(), ItemSource::HaVoice, raw)
}

fn codes(item: &DraftItem) -> Vec<&'static str> {
    item.errors.iter().map(|error| error.code).collect()
}

fn weekly(days: &[i64]) -> RawRecurrence {
    RawRecurrence {
        freq: Some("weekly".to_owned()),
        weekdays: days.to_vec(),
        ..RawRecurrence::default()
    }
}

#[test]
fn valid_task_becomes_new_item_with_source() {
    let mut raw = task();
    raw.notes = Some("  про рецепт ".to_owned());
    raw.priority = Some("High".to_owned());
    raw.tags = vec!["#Семья".to_owned(), "семья".to_owned()];
    raw.subtasks = vec!["Набрать".to_owned()];

    let draft = check(raw);

    assert!(draft.is_valid(), "{:?}", draft.errors);
    let item = draft.item.unwrap();
    assert_eq!(item.title, "Позвонить маме");
    assert_eq!(item.notes.as_deref(), Some("про рецепт"));
    assert_eq!(item.priority, ItemPriority::High);
    assert_eq!(item.tags, ["семья"]);
    assert_eq!(item.subtasks.len(), 1);
    assert_eq!(item.source, ItemSource::HaVoice);
    assert_eq!(Some(item.list_id), draft.list_id);
}

#[test]
fn title_length_is_one_to_two_hundred() {
    assert_eq!(codes(&check(raw(ListKind::Tasks, "   "))), ["title_length"]);
    assert_eq!(
        codes(&check(raw(ListKind::Tasks, &"я".repeat(201)))),
        ["title_length"]
    );
    assert!(check(raw(ListKind::Tasks, &"я".repeat(200))).is_valid());
}

#[test]
fn control_characters_are_removed_from_title() {
    let draft = check(raw(ListKind::Tasks, "куп\u{0}ить\u{7} хлеб"));

    assert_eq!(draft.title, "купить хлеб");
}

#[test]
fn html_in_title_is_kept_as_data() {
    let draft = check(raw(ListKind::Shopping, "<b>купить</b> хлеб"));

    assert!(draft.is_valid());
    assert_eq!(draft.item.unwrap().title, "<b>купить</b> хлеб");
}

#[test]
fn notes_longer_than_two_thousand_are_rejected() {
    let mut raw = task();
    raw.notes = Some("я".repeat(2_001));
    assert_eq!(codes(&check(raw)), ["notes_length"]);

    let mut raw = task();
    raw.notes = Some("я".repeat(2_000));
    assert!(check(raw).is_valid());
}

#[test]
fn past_date_is_an_item_error_and_today_is_allowed() {
    let mut yesterday = task();
    yesterday.due_date = Some("2026-10-29".to_owned());
    let draft = check(yesterday);
    assert_eq!(codes(&draft), ["date_past"]);
    assert_eq!(draft.errors[0].message, "Дата уже прошла");
    assert!(draft.item.is_none());

    let mut today = task();
    today.due_date = Some("2026-10-30".to_owned());
    assert!(check(today).is_valid());
}

#[test]
fn time_earlier_today_is_an_item_error() {
    // Сейчас 14:05 по Москве: прошедшее время недопустимо, текущая минута и
    // позже — допустимы, как и в ручной форме. Секунды не важны: модель видит
    // время с точностью до минуты, поэтому «в 14:05» в 14:05:59 ещё не прошло.
    for seconds in [0, 1, 30, 59] {
        let mut context = context();
        context.now += chrono::Duration::seconds(seconds);
        for (time, valid) in [
            ("00:00", false),
            ("14:04", false),
            ("14:05", true),
            ("14:06", true),
            ("23:59", true),
        ] {
            let mut raw = task();
            raw.due_date = Some("2026-10-30".to_owned());
            raw.due_time = Some(time.to_owned());
            let draft = validate_item(&context, ItemSource::Api, raw);
            assert_eq!(draft.is_valid(), valid, "{time} at +{seconds}s");
            if !valid {
                assert_eq!(codes(&draft), ["time_past"], "{time} at +{seconds}s");
                assert_eq!(draft.errors[0].message, "Это время уже прошло");
            }
        }
    }

    // Завтрашнее раннее время — в будущем.
    let mut tomorrow = task();
    tomorrow.due_date = Some("2026-10-31".to_owned());
    tomorrow.due_time = Some("00:00".to_owned());
    assert!(check(tomorrow).is_valid());

    // Прошедшая дата уже названа ошибкой, время не дублирует её.
    let mut yesterday = task();
    yesterday.due_date = Some("2026-10-29".to_owned());
    yesterday.due_time = Some("09:00".to_owned());
    assert_eq!(codes(&check(yesterday)), ["date_past"]);
}

#[test]
fn date_horizon_is_five_years() {
    let mut edge = task();
    edge.due_date = Some("2031-10-30".to_owned());
    assert!(check(edge).is_valid());

    let mut far = task();
    far.due_date = Some("2031-10-31".to_owned());
    let draft = check(far);
    assert_eq!(codes(&draft), ["date_far"]);
    assert_eq!(draft.errors[0].message, "Слишком далёкая дата");
}

#[test]
fn malformed_date_and_time_are_rejected() {
    for value in ["завтра", "2026-13-01", "30.10.2026", "2026-02-30"] {
        let mut raw = task();
        raw.due_date = Some(value.to_owned());
        assert_eq!(codes(&check(raw)), ["date_invalid"], "{value}");
    }
    for value in ["25:00", "18:60", "вечером", "6pm"] {
        let mut raw = task();
        raw.due_date = Some("2026-10-31".to_owned());
        raw.due_time = Some(value.to_owned());
        assert_eq!(codes(&check(raw)), ["time_invalid"], "{value}");
    }
}

#[test]
fn time_without_date_is_rejected_instead_of_guessed() {
    let mut raw = task();
    raw.due_time = Some("18:00".to_owned());

    let draft = check(raw);

    assert_eq!(codes(&draft), ["time_without_date"]);
}

#[test]
fn date_and_time_become_utc_moment_in_household_timezone() {
    let mut raw = task();
    raw.due_date = Some("2026-10-30".to_owned());
    raw.due_time = Some("18:00".to_owned());

    let item = check(raw).item.unwrap();

    assert_eq!(item.due_date, None);
    assert_eq!(
        item.due_at,
        Some(Utc.with_ymd_and_hms(2026, 10, 30, 15, 0, 0).unwrap())
    );
}

#[test]
fn date_without_time_is_an_all_day_due() {
    let mut raw = task();
    raw.due_date = Some("2026-10-31".to_owned());

    let item = check(raw).item.unwrap();

    assert_eq!(item.due_at, None);
    assert_eq!(item.due_date, NaiveDate::from_ymd_opt(2026, 10, 31));
}

#[test]
fn every_priority_maps_to_its_domain_value() {
    for (value, expected) in [
        ("low", ItemPriority::Low),
        ("normal", ItemPriority::Normal),
        ("high", ItemPriority::High),
        ("urgent", ItemPriority::Urgent),
        (" URGENT ", ItemPriority::Urgent),
    ] {
        let mut raw = task();
        raw.priority = Some(value.to_owned());
        let item = check(raw).item.expect("valid item");
        assert_eq!(item.priority, expected, "{value}");
    }
    assert_eq!(check(task()).item.unwrap().priority, ItemPriority::Normal);
}

#[test]
fn invalid_date_and_invalid_time_are_both_reported() {
    let mut raw = task();
    raw.due_date = Some("вчера".to_owned());
    raw.due_time = Some("25:00".to_owned());

    assert_eq!(codes(&check(raw)), ["date_invalid", "time_invalid"]);

    // Верное время при неверной дате второй ошибки не даёт.
    let mut raw = task();
    raw.due_date = Some("вчера".to_owned());
    raw.due_time = Some("18:00".to_owned());
    assert_eq!(codes(&check(raw)), ["date_invalid"]);
}

#[test]
fn priority_must_be_known() {
    let mut raw = task();
    raw.priority = Some("критично".to_owned());

    let draft = check(raw);

    assert_eq!(codes(&draft), ["priority_unknown"]);
    assert_eq!(draft.errors[0].message, "Неизвестный приоритет");
}

#[test]
fn assignee_must_be_a_household_member() {
    let context = context();
    let mut raw = task();
    raw.assignee = Some("Мария".to_owned());
    let draft = validate_item(&context, ItemSource::Api, raw);
    assert_eq!(codes(&draft), ["assignee_unknown"]);
    assert_eq!(draft.errors[0].message, "Такого участника нет");
    assert!(draft.item.is_none());

    for name in ["анна", " Анна ", "@anna", "ANNA"] {
        let mut raw = task();
        raw.assignee = Some(name.to_owned());
        let item = validate_item(&context, ItemSource::Api, raw).item.unwrap();
        assert_eq!(item.assignee_user_id, Some(context.members[0].id), "{name}");
    }
}

#[test]
fn yo_and_ye_are_the_same_letter_for_names() {
    let context = context();
    let mut raw = task();
    raw.assignee = Some("Петр".to_owned());

    let item = validate_item(&context, ItemSource::Api, raw).item.unwrap();

    assert_eq!(item.assignee_user_id, Some(context.members[1].id));
}

#[test]
fn ambiguous_assignee_is_not_guessed() {
    let mut context = context();
    context.members = vec![
        member("Алексей", Some("alex_a")),
        member("Алексей", Some("alex_b")),
    ];
    let expected: Vec<_> = context.members.iter().map(|member| member.id).collect();
    let mut raw = task();
    raw.assignee = Some("Алексей".to_owned());

    let draft = validate_item(&context, ItemSource::Api, raw);

    assert_eq!(codes(&draft), [ASSIGNEE_AMBIGUOUS]);
    assert_eq!(draft.assignee_candidates, expected);
    assert!(draft.item.is_none());
}

#[test]
fn auto_mode_drops_an_unknown_or_ambiguous_assignee_instead_of_failing() {
    let mut context = context();
    context.members = vec![
        member("Алексей", Some("alex_a")),
        member("Алексей", Some("alex_b")),
    ];
    for name in ["Мария", "Алексей"] {
        let mut raw = task();
        raw.assignee = Some(name.to_owned());

        let draft = validate_item_in(&context, ItemSource::HaVoice, raw, ValidationMode::Auto);

        assert_eq!(codes(&draft), Vec::<&str>::new(), "{name}");
        assert!(draft.assignee_candidates.is_empty(), "{name}");
        assert!(
            draft.item.expect("item").assignee_user_id.is_none(),
            "{name}: the item must be created without an assignee"
        );
    }
}

#[test]
fn self_reference_is_resolved_to_the_author() {
    let context = context();
    let author = context.author.clone().expect("author");
    let mut raw = task();
    raw.assignee = Some("мне".to_owned());

    let draft = validate_item(&context, ItemSource::Telegram, raw);

    assert!(draft.is_valid(), "{:?}", draft.errors);
    assert_eq!(draft.item.expect("item").assignee_user_id, Some(author.id));
}

#[test]
fn chosen_member_resolves_an_ambiguous_name() {
    let mut context = context();
    context.members = vec![
        member("Алексей", Some("alex_a")),
        member("Алексей", Some("alex_b")),
    ];
    let chosen = context.members[1].id;
    let mut raw = task();
    raw.assignee = Some("Алексей".to_owned());
    raw.assignee_id = Some(chosen);

    let draft = validate_item(&context, ItemSource::Api, raw);

    assert!(draft.is_valid(), "{:?}", draft.errors);
    assert_eq!(draft.item.expect("item").assignee_user_id, Some(chosen));
}

#[test]
fn chosen_member_who_left_the_household_is_rejected() {
    let mut raw = task();
    raw.assignee_id = Some(Uuid::now_v7());

    let draft = check(raw);

    assert_eq!(codes(&draft), ["assignee_unknown"]);
}

#[test]
fn username_and_display_name_of_one_member_are_not_ambiguous() {
    let mut context = context();
    context.members = vec![member("Anna", Some("anna")), member("Борис", Some("boris"))];
    let mut raw = task();
    raw.assignee = Some("anna".to_owned());

    assert!(validate_item(&context, ItemSource::Api, raw).is_valid());
}

#[test]
fn tags_are_limited_in_count_and_length() {
    let mut raw = task();
    raw.tags = (0..11).map(|n| format!("тег{n}")).collect();
    assert_eq!(codes(&check(raw)), ["tags_count"]);

    let mut raw = task();
    raw.tags = (0..10).map(|n| format!("тег{n}")).collect();
    assert!(check(raw).is_valid());

    let mut raw = task();
    raw.tags = vec!["я".repeat(33)];
    assert_eq!(codes(&check(raw)), ["tag_length"]);

    let mut raw = task();
    raw.tags = vec!["я".repeat(32)];
    assert!(check(raw).is_valid());
}

#[test]
fn quantity_must_be_positive_and_at_most_9999() {
    for quantity in [0.0, -1.0, 10_000.0, 9_999.5] {
        let mut raw = raw(ListKind::Shopping, "Молоко");
        raw.quantity = Some(quantity);
        let draft = check(raw);
        assert_eq!(codes(&draft), ["quantity_range"], "{quantity}");
        assert_eq!(draft.errors[0].message, "Количество вне диапазона");
    }
    // В диапазоне, но с лишними знаками: причина называется честно.
    for quantity in [0.0001, 1.2345] {
        let mut raw = raw(ListKind::Shopping, "Молоко");
        raw.quantity = Some(quantity);
        assert_eq!(codes(&check(raw)), ["quantity_precision"], "{quantity}");
    }
    for quantity in [0.001, 2.0, 2.5, 9_999.0] {
        let mut raw = raw(ListKind::Shopping, "Молоко");
        raw.quantity = Some(quantity);
        assert!(check(raw).is_valid(), "{quantity}");
    }
}

#[test]
fn shopping_item_keeps_quantity_and_unit() {
    let mut raw = raw(ListKind::Shopping, "Молоко");
    raw.quantity = Some(2.0);
    raw.unit = Some(" л ".to_owned());

    let item = check(raw).item.unwrap();

    assert_eq!(item.quantity, Some(2.0));
    assert_eq!(item.unit.as_deref(), Some("л"));
}

#[test]
fn subtasks_are_limited() {
    let mut raw = task();
    raw.subtasks = (0..51).map(|n| format!("шаг {n}")).collect();
    assert_eq!(codes(&check(raw)), ["subtasks_count"]);

    let mut raw = task();
    raw.subtasks = (0..50).map(|n| format!("шаг {n}")).collect();
    assert!(check(raw).is_valid());

    let mut raw = task();
    raw.subtasks = vec!["я".repeat(201)];
    assert_eq!(codes(&check(raw)), ["subtask_length"]);
}

#[test]
fn recurrence_interval_zero_is_rejected() {
    let mut raw = task();
    raw.recurrence = Some(RawRecurrence {
        freq: Some("daily".to_owned()),
        interval: Some(0),
        ..RawRecurrence::default()
    });

    let draft = check(raw);

    assert_eq!(codes(&draft), ["recurrence_interval"]);
    assert_eq!(draft.errors[0].message, "Интервал — от 1 до 365");
    assert!(draft.item.is_none());
}

#[test]
fn recurrence_interval_bounds() {
    for interval in [1, 365] {
        let mut raw = task();
        raw.recurrence = Some(RawRecurrence {
            freq: Some("daily".to_owned()),
            interval: Some(interval),
            ..RawRecurrence::default()
        });
        assert!(check(raw).is_valid(), "{interval}");
    }
    // Код ошибки обязателен: без него отказ страховки `domain_rejected`
    // неотличим от настоящей проверки.
    for interval in [-1, 0, 366] {
        let mut raw = task();
        raw.recurrence = Some(RawRecurrence {
            freq: Some("daily".to_owned()),
            interval: Some(interval),
            ..RawRecurrence::default()
        });
        assert_eq!(
            codes(&check(raw)),
            ["recurrence_interval"],
            "interval {interval}"
        );
    }
}

#[test]
fn weekly_recurrence_needs_valid_weekdays() {
    let mut raw = task();
    raw.recurrence = Some(weekly(&[]));
    let draft = check(raw);
    assert_eq!(codes(&draft), ["recurrence_weekdays"]);
    assert_eq!(draft.errors[0].message, "Укажите дни недели");

    for days in [&[2, 8][..], &[0][..], &[-1, 3][..]] {
        let mut raw = task();
        raw.recurrence = Some(weekly(days));
        assert_eq!(codes(&check(raw)), ["recurrence_weekdays"], "{days:?}");
    }

    let mut raw = task();
    raw.due_date = Some("2026-11-03".to_owned());
    raw.recurrence = Some(weekly(&[2]));
    let item = check(raw).item.unwrap();
    let recurrence = item.recurrence.unwrap();
    assert_eq!(recurrence.freq, RecurrenceFreq::Weekly);
    assert_eq!(recurrence.byweekday, [2]);
    assert_eq!(recurrence.interval, Some(1));
}

#[test]
fn missing_or_unknown_frequency_is_rejected() {
    let mut raw = task();
    raw.recurrence = Some(RawRecurrence {
        interval: Some(2),
        ..RawRecurrence::default()
    });
    assert_eq!(codes(&check(raw)), ["recurrence_freq"]);

    let mut raw = task();
    raw.recurrence = Some(RawRecurrence {
        freq: Some("hourly".to_owned()),
        ..RawRecurrence::default()
    });
    assert_eq!(codes(&check(raw)), ["recurrence_freq"]);
}

#[test]
fn irrelevant_weekdays_and_month_rule_are_dropped() {
    let mut raw = task();
    raw.recurrence = Some(RawRecurrence {
        freq: Some("daily".to_owned()),
        weekdays: vec![1, 2],
        month_rule: Some("last_day".to_owned()),
        ..RawRecurrence::default()
    });

    let recurrence = check(raw).item.unwrap().recurrence.unwrap();

    assert!(recurrence.byweekday.is_empty());
    assert_eq!(recurrence.month_rule, None);
}

#[test]
fn monthly_month_rule_is_validated() {
    let mut raw = task();
    raw.recurrence = Some(RawRecurrence {
        freq: Some("monthly".to_owned()),
        month_rule: Some("last_day".to_owned()),
        ..RawRecurrence::default()
    });
    let recurrence = check(raw).item.unwrap().recurrence.unwrap();
    assert_eq!(recurrence.month_rule, Some(MonthRule::LastDay));

    let mut raw = task();
    raw.recurrence = Some(RawRecurrence {
        freq: Some("monthly".to_owned()),
        month_rule: Some("sometimes".to_owned()),
        ..RawRecurrence::default()
    });
    assert_eq!(codes(&check(raw)), ["recurrence_month_rule"]);
}

#[test]
fn recurrence_until_is_within_start_and_five_years() {
    let cases = [
        ("2026-11-03", true),
        ("2026-11-02", false),
        ("2031-11-03", true),
        ("2031-11-04", false),
        ("когда-нибудь", false),
    ];
    for (until, valid) in cases {
        let mut raw = task();
        raw.due_date = Some("2026-11-03".to_owned());
        raw.recurrence = Some(RawRecurrence {
            until: Some(until.to_owned()),
            ..weekly(&[2])
        });
        let draft = check(raw);
        assert_eq!(draft.is_valid(), valid, "{until}: {:?}", draft.errors);
        if !valid {
            assert_eq!(draft.errors[0].message, "Неверная дата окончания");
        }
    }
}

#[test]
fn recurrence_is_only_for_task_lists() {
    let mut raw = raw(ListKind::Shopping, "Молоко");
    raw.recurrence = Some(RawRecurrence {
        freq: Some("daily".to_owned()),
        ..RawRecurrence::default()
    });

    assert_eq!(codes(&check(raw)), ["recurrence_kind"]);
}

#[test]
fn named_list_of_matching_kind_is_used() {
    let context = context();
    let mut raw = task();
    raw.target_list = Some(" дача ".to_owned());

    let draft = validate_item(&context, ItemSource::Api, raw);

    assert_eq!(draft.list_id, Some(context.lists[2].id));
    assert!(!draft.list_auto_selected);
}

#[test]
fn unknown_list_falls_back_to_default_and_is_flagged() {
    let context = context();
    let mut raw = task();
    raw.target_list = Some("Гараж".to_owned());

    let draft = validate_item(&context, ItemSource::Api, raw);

    assert!(draft.is_valid());
    assert_eq!(draft.list_id, Some(context.lists[0].id));
    assert!(draft.list_auto_selected);
}

#[test]
fn list_of_another_kind_is_not_used_by_name() {
    let context = context();
    let mut raw = raw(ListKind::Shopping, "Молоко");
    raw.target_list = Some("Задачи".to_owned());

    let draft = validate_item(&context, ItemSource::Api, raw);

    assert_eq!(draft.list_id, Some(context.lists[1].id));
    assert!(draft.list_auto_selected);
}

#[test]
fn unnamed_list_uses_default_without_flag() {
    let context = context();

    let draft = validate_item(&context, ItemSource::Api, raw(ListKind::Shopping, "Хлеб"));

    assert_eq!(draft.list_id, Some(context.lists[1].id));
    assert!(!draft.list_auto_selected);
}

#[test]
fn household_without_list_of_kind_rejects_item() {
    let mut context = context();
    context
        .lists
        .retain(|list| list.kind != ListKind::Checklist);

    let draft = validate_item(
        &context,
        ItemSource::Api,
        raw(ListKind::Checklist, "Собрать сумку"),
    );

    assert_eq!(codes(&draft), ["list_missing"]);
    assert!(draft.item.is_none());
}

#[test]
fn default_list_prefers_flagged_list() {
    let mut context = context();
    context.lists = vec![
        list("Старый", ListKind::Tasks, false),
        list("Основной", ListKind::Tasks, true),
    ];

    assert_eq!(
        context
            .default_list(ListKind::Tasks)
            .map(|l| l.name.as_str()),
        Some("Основной")
    );

    context.lists[1].is_default = false;
    assert_eq!(
        context
            .default_list(ListKind::Tasks)
            .map(|l| l.name.as_str()),
        Some("Старый")
    );
}

#[test]
fn all_errors_of_an_item_are_collected() {
    let mut raw = task();
    raw.due_date = Some("2026-10-01".to_owned());
    raw.assignee = Some("Мария".to_owned());
    raw.priority = Some("???".to_owned());

    let draft = check(raw);

    assert_eq!(
        codes(&draft),
        ["priority_unknown", "assignee_unknown", "date_past"]
    );
    assert!(draft.item.is_none());
}

#[test]
fn one_invalid_item_blocks_the_whole_draft_but_keeps_the_rest_visible() {
    let mut bad = task();
    bad.due_date = Some("2026-10-29".to_owned());
    let parsed = ParsedResponse {
        items: vec![raw(ListKind::Shopping, "Молоко"), bad],
        clarifications: vec!["Когда?".to_owned()],
    };

    let draft = validate_items(&context(), ItemSource::HaVoice, parsed);

    assert!(!draft.is_valid());
    assert!(draft.items[0].is_valid());
    assert!(!draft.items[1].is_valid());
    assert_eq!(draft.clarifications, ["Когда?"]);
}

#[test]
fn empty_draft_is_not_valid() {
    let draft = validate_items(&context(), ItemSource::Api, ParsedResponse::default());

    assert!(!draft.is_valid());
}

#[test]
fn mixed_valid_draft_passes() {
    let mut call = task();
    call.due_date = Some("2026-10-30".to_owned());
    call.due_time = Some("18:00".to_owned());
    let mut milk = raw(ListKind::Shopping, "Молоко");
    milk.quantity = Some(2.0);
    milk.unit = Some("л".to_owned());
    let parsed = ParsedResponse {
        items: vec![milk, raw(ListKind::Shopping, "Хлеб"), call],
        clarifications: Vec::new(),
    };

    let draft = validate_items(&context(), ItemSource::Telegram, parsed);

    assert!(draft.is_valid());
    assert!(draft.items.iter().all(|item| item.item.is_some()));
}

#[test]
fn long_unit_is_truncated_like_the_domain_does() {
    let mut raw = raw(ListKind::Shopping, "Молоко");
    raw.quantity = Some(1.0);
    raw.unit = Some("я".repeat(40));

    let draft = check(raw);

    assert!(draft.is_valid(), "{:?}", draft.errors);
    assert_eq!(draft.item.unwrap().unit, Some("я".repeat(MAX_UNIT)));
}

#[test]
fn leading_at_sign_is_not_stripped_from_list_names() {
    let mut context = context();
    context.lists.push(list("@дом", ListKind::Tasks, false));
    let at_home = context.lists.last().unwrap().id;

    let mut exact = task();
    exact.target_list = Some("@дом".to_owned());
    let draft = validate_item(&context, ItemSource::Api, exact);
    assert_eq!(draft.list_id, Some(at_home));
    assert!(!draft.list_auto_selected);

    let mut bare = task();
    bare.target_list = Some("дом".to_owned());
    let draft = validate_item(&context, ItemSource::Api, bare);
    assert_ne!(draft.list_id, Some(at_home));
    assert!(draft.list_auto_selected);
}

#[test]
fn domain_disagreement_becomes_an_item_error() {
    let context = context();
    let mut item = check(task()).item.unwrap();
    item.title = String::new();
    let mut errors = Vec::new();

    let confirmed = confirm_domain(
        &context,
        ListKind::Tasks,
        item,
        context.today(),
        &mut errors,
    );

    assert!(confirmed.is_none());
    assert_eq!(
        errors,
        [ItemError {
            field: "item",
            code: "domain_rejected",
            message: "Элемент не прошёл проверку",
        }]
    );
}

/// Перечень граничных случаев, а не property-тест: каждый из них слой capture
/// принимает, и домен обязан согласиться, иначе сработает страховка
/// `domain_rejected`. Это не доказывает, что страховка недостижима для всех
/// входов; новый найденный случай добавляется сюда.
#[test]
fn domain_accepts_every_enumerated_valid_item() {
    let long = |n: usize| "я".repeat(n);
    let mut cases: Vec<RawItem> = vec![
        task(),
        raw(ListKind::Tasks, &long(MAX_TITLE)),
        raw(ListKind::Checklist, " Собрать сумку "),
        raw(ListKind::Shopping, "<b>хлеб</b>"),
        raw(ListKind::Tasks, "куп\u{0}ить\u{7}"),
    ];

    let mut notes = task();
    notes.notes = Some(long(MAX_NOTES));
    cases.push(notes);

    for priority in ["low", "Normal", " HIGH ", "urgent"] {
        let mut raw = task();
        raw.priority = Some(priority.to_owned());
        cases.push(raw);
    }

    for (date, time) in [
        ("2026-10-30", None),
        ("2026-10-30", Some("14:05")),
        ("2026-10-30", Some("23:59:59")),
        ("2026-11-01", Some("18:00")),
        ("2031-10-30", None),
        ("2031-10-30", Some("12:00")),
    ] {
        let mut raw = task();
        raw.due_date = Some(date.to_owned());
        raw.due_time = time.map(str::to_owned);
        cases.push(raw);
    }

    let mut tags = task();
    tags.tags = (0..MAX_TAGS).map(|n| format!("#Тег{n}")).collect();
    cases.push(tags);
    let mut tag = task();
    tag.tags = vec![long(MAX_TAG), "Дубль".to_owned(), "дубль".to_owned()];
    cases.push(tag);

    let mut subtasks = task();
    subtasks.subtasks = (0..MAX_SUBTASKS).map(|n| format!("шаг {n}")).collect();
    cases.push(subtasks);
    let mut subtask = task();
    subtask.subtasks = vec![long(MAX_TITLE)];
    cases.push(subtask);

    for quantity in [0.001, 1.0, 2.5, 9_999.0] {
        for unit in [
            None,
            Some("л"),
            Some("  кг "),
            Some("очень-длинная-единица-измерения"),
        ] {
            let mut raw = raw(ListKind::Shopping, "Молоко");
            raw.quantity = Some(quantity);
            raw.unit = unit.map(str::to_owned);
            cases.push(raw);
        }
    }

    for (freq, interval) in [
        ("daily", None),
        ("daily", Some(365)),
        ("monthly", Some(1)),
        ("yearly", Some(2)),
    ] {
        for month_rule in [None, Some("anchor_day"), Some("last_day")] {
            let mut raw = task();
            raw.due_date = Some("2026-11-03".to_owned());
            raw.recurrence = Some(RawRecurrence {
                freq: Some(freq.to_owned()),
                interval,
                month_rule: month_rule.map(str::to_owned),
                // У «чужих» частот модель заполняет их по привычке.
                weekdays: vec![1, 2],
                until: Some("2031-11-03".to_owned()),
            });
            cases.push(raw);
        }
    }
    for days in [&[1][..], &[1, 3, 5], &[1, 2, 3, 4, 5, 6, 7]] {
        let mut raw = task();
        raw.due_date = Some("2026-11-03".to_owned());
        raw.recurrence = Some(RawRecurrence {
            until: Some("2027-01-01".to_owned()),
            interval: Some(2),
            ..weekly(days)
        });
        cases.push(raw);
    }

    // Серии без срока: домен сам подставляет первое вхождение.
    for (freq, month_rule) in [
        ("daily", None),
        ("monthly", None),
        ("monthly", Some("last_day")),
        ("yearly", None),
    ] {
        for until in [None, Some("2026-12-31"), Some("2031-10-30")] {
            let mut raw = task();
            raw.recurrence = Some(RawRecurrence {
                freq: Some(freq.to_owned()),
                month_rule: month_rule.map(str::to_owned),
                until: until.map(str::to_owned),
                ..RawRecurrence::default()
            });
            cases.push(raw);
        }
    }
    // Ежедневная серия, которая заканчивается в день создания: одно вхождение.
    let mut one_day = task();
    one_day.recurrence = Some(RawRecurrence {
        freq: Some("daily".to_owned()),
        until: Some("2026-10-30".to_owned()),
        ..RawRecurrence::default()
    });
    cases.push(one_day);
    // Повтор дня недели в списке, в том числе без срока.
    for due in [None, Some("2026-11-03")] {
        let mut raw = task();
        raw.due_date = due.map(str::to_owned);
        raw.recurrence = Some(weekly(&[2, 2, 5]));
        cases.push(raw);
    }
    // Срок не на день недели серии и `until` ровно на нём.
    let mut off_weekday = task();
    off_weekday.due_date = Some("2026-10-30".to_owned());
    off_weekday.recurrence = Some(RawRecurrence {
        until: Some("2026-10-30".to_owned()),
        ..weekly(&[2])
    });
    cases.push(off_weekday);

    let mut named = task();
    named.target_list = Some("Дача".to_owned());
    named.assignee = Some("@Anna".to_owned());
    cases.push(named);

    let mut context = context();
    context
        .lists
        .push(list("Чек-лист", ListKind::Checklist, true));
    for (index, raw) in cases.into_iter().enumerate() {
        let draft = validate_item(&context, ItemSource::HaVoice, raw.clone());
        assert!(
            draft.is_valid(),
            "случай {index} должен быть валиден: {:?}\n{raw:?}",
            draft.errors
        );
        assert!(draft.item.is_some(), "случай {index}");
    }
}

#[test]
fn dst_gap_and_overlap_resolve_in_household_timezone() {
    let mut context = context();
    context.timezone = chrono_tz::Europe::Berlin;
    context.now = Utc.with_ymd_and_hms(2026, 10, 20, 10, 0, 0).unwrap();
    // Неоднозначное время (25.10.2026 часы идут 03:00 → 02:00) — первое
    // вхождение, летнее; несуществующее (28.03.2027 02:00 → 03:00) сдвигается
    // вперёд до первого существующего момента.
    let cases = [
        (
            "2026-10-25",
            "02:30",
            Utc.with_ymd_and_hms(2026, 10, 25, 0, 30, 0),
        ),
        (
            "2027-03-28",
            "02:30",
            Utc.with_ymd_and_hms(2027, 3, 28, 1, 0, 0),
        ),
    ];

    for (date, time, expected) in cases {
        let mut raw = task();
        raw.due_date = Some(date.to_owned());
        raw.due_time = Some(time.to_owned());

        let draft = validate_item(&context, ItemSource::Api, raw);

        assert!(draft.is_valid(), "{date} {time}: {:?}", draft.errors);
        assert_eq!(
            draft.item.unwrap().due_at,
            expected.single(),
            "{date} {time}"
        );
    }
}

fn monthly_last_day() -> RawRecurrence {
    RawRecurrence {
        freq: Some("monthly".to_owned()),
        month_rule: Some("last_day".to_owned()),
        ..RawRecurrence::default()
    }
}

#[test]
fn effective_due_shows_the_first_occurrence_of_a_series_without_a_date() {
    let mut raw = task();
    raw.recurrence = Some(RawRecurrence {
        freq: Some("daily".to_owned()),
        ..RawRecurrence::default()
    });

    let draft = check(raw);

    assert!(draft.is_valid(), "{:?}", draft.errors);
    // Домен подставит первое вхождение при сохранении, карточка показывает его.
    assert_eq!(
        draft.effective_due_date,
        Some(chrono::NaiveDate::from_ymd_opt(2026, 10, 30).unwrap())
    );
    // Сам `NewItem` остаётся исходным: домен нормализует его заново.
    assert_eq!(draft.item.expect("item").due_date, None);
}

#[test]
fn effective_due_follows_the_last_day_of_the_month_rule() {
    let mut raw = task();
    raw.due_date = Some("2026-11-03".to_owned());
    raw.recurrence = Some(monthly_last_day());

    let draft = check(raw);

    assert!(draft.is_valid(), "{:?}", draft.errors);
    assert_eq!(
        draft.effective_due_date,
        Some(chrono::NaiveDate::from_ymd_opt(2026, 11, 30).unwrap())
    );
    assert_eq!(
        draft.item.expect("item").due_date,
        Some(chrono::NaiveDate::from_ymd_opt(2026, 11, 3).unwrap())
    );
}

#[test]
fn effective_due_of_a_plain_item_is_its_own() {
    let mut raw = task();
    raw.due_date = Some("2026-11-03".to_owned());

    let draft = check(raw);

    assert_eq!(
        draft.effective_due_date,
        Some(chrono::NaiveDate::from_ymd_opt(2026, 11, 3).unwrap())
    );
    assert_eq!(draft.effective_due_at, None);
}

#[test]
fn series_that_ends_before_its_first_occurrence_has_its_own_error() {
    // 2026-10-30 — пятница, вторник наступит 3 ноября, после `until`.
    let mut raw = task();
    raw.recurrence = Some(RawRecurrence {
        until: Some("2026-10-31".to_owned()),
        ..weekly(&[2])
    });

    let draft = check(raw);

    assert_eq!(codes(&draft), ["recurrence_range"]);
    assert_eq!(
        draft.errors[0].message,
        "Серия закончится раньше первого повторения"
    );
}

#[test]
fn series_with_a_due_date_is_valid_even_if_the_next_weekday_is_after_until() {
    // Сам срок (пятница) остаётся единственным вхождением.
    let mut raw = task();
    raw.due_date = Some("2026-10-30".to_owned());
    raw.recurrence = Some(RawRecurrence {
        until: Some("2026-10-31".to_owned()),
        ..weekly(&[2])
    });

    assert!(check(raw).is_valid());
}

#[test]
fn dates_must_be_exactly_yyyy_mm_dd() {
    for date in [
        "+2026-11-03",
        "2026-1-3",
        "2026-11-3",
        "03.11.2026",
        "2026-11-03T10:00",
    ] {
        let mut raw = task();
        raw.due_date = Some(date.to_owned());
        assert_eq!(codes(&check(raw)), ["date_invalid"], "{date}");
    }
}

#[test]
fn times_are_hh_mm_with_optional_seconds() {
    for time in ["9:05", "18", "18:0", "18:00:0", "1800"] {
        let mut raw = task();
        raw.due_date = Some("2026-11-03".to_owned());
        raw.due_time = Some(time.to_owned());
        assert_eq!(codes(&check(raw)), ["time_invalid"], "{time}");
    }
    for time in ["09:05", "18:00:30"] {
        let mut raw = task();
        raw.due_date = Some("2026-11-03".to_owned());
        raw.due_time = Some(time.to_owned());
        assert!(check(raw).is_valid(), "{time}");
    }
}

#[test]
fn quantity_precision_has_its_own_message() {
    let mut raw = raw(ListKind::Shopping, "Молоко");
    raw.quantity = Some(1.2345);
    let draft = check(raw);
    assert_eq!(codes(&draft), ["quantity_precision"]);
    assert_eq!(
        draft.errors[0].message,
        "Не больше трёх знаков после запятой"
    );

    for quantity in [0.0, -1.0, 10_000.0, f64::NAN] {
        let mut raw = raw_shopping();
        raw.quantity = Some(quantity);
        assert_eq!(codes(&check(raw)), ["quantity_range"], "{quantity}");
    }

    let mut raw = raw_shopping();
    raw.quantity = Some(1.234);
    assert!(check(raw).is_valid());
}

fn raw_shopping() -> RawItem {
    raw(ListKind::Shopping, "Молоко")
}
