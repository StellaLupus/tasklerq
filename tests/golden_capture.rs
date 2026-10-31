//! Golden set LLM-capture: структура набора, метрика элементов и guard версии
//! промпта. Сети и базы здесь нет: живой замер делает `tasklerq capture-eval`.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tasklerq::capture::eval::{
    Baseline, BaselineEntry, Category, GoldenSet, GuardOutcome, MIN_ACCURACY, PhraseResult,
    check_baseline, judge, prompt_hash,
};
use tasklerq::capture::prompt::PROMPT_VERSION;

const GOLDEN: &str = include_str!("golden/capture.json");
const BASELINE: &str = include_str!("golden/baseline.json");

const CONTEXT: &str = r#"{
    "now": "2026-10-30T11:05:00Z",
    "timezone": "Europe/Moscow",
    "author": "Анна",
    "lists": [
        {"name": "Задачи", "kind": "tasks", "default": true},
        {"name": "Покупки", "kind": "shopping", "default": true}
    ],
    "members": [{"name": "Анна"}, {"name": "Пётр"}]
}"#;

fn golden() -> GoldenSet {
    GoldenSet::parse(GOLDEN).expect("golden set")
}

/// Набор из одной фразы: так юнит-тесты метрики не зависят от данных набора.
fn judge_one(expected: &Value, clarifications: &Value, response: Value) -> PhraseResult {
    let text = json!({
        "version": 1,
        "context": serde_json::from_str::<Value>(CONTEXT).expect("context"),
        "phrases": [{
            "id": "one",
            "category": "simple",
            "text": "позвонить маме",
            "expect": {"items": expected, "clarifications": clarifications}
        }]
    });
    let set = GoldenSet::parse(&text.to_string()).expect("set");
    judge(&set.context, &set.phrases[0], &response)
}

fn response(title: &str) -> Value {
    json!({
        "items": [{"target_kind": "tasks", "title": title, "target_list": "Задачи"}],
        "clarifications": []
    })
}

#[test]
fn golden_set_covers_every_category_with_a_fixed_context() {
    let set = golden();
    assert!(
        set.phrases.len() >= 30,
        "в golden set {} фраз, нужно не меньше 30",
        set.phrases.len()
    );

    let mut ids = HashSet::new();
    for phrase in &set.phrases {
        assert!(ids.insert(phrase.id.clone()), "повтор id {}", phrase.id);

        let context = phrase
            .effective_context(&set.context)
            .to_context()
            .unwrap_or_else(|error| panic!("{}: {error}", phrase.id));
        assert!(!context.lists.is_empty(), "{}: нет списков", phrase.id);
        assert!(!context.members.is_empty(), "{}: нет участников", phrase.id);
        assert!(context.author.is_some(), "{}: нет автора", phrase.id);
        assert_eq!(
            context.now,
            "2026-10-30T11:05:00Z"
                .parse::<DateTime<Utc>>()
                .expect("fixed now"),
            "{}: контекст зависит от текущего времени",
            phrase.id
        );
        assert_eq!(context.timezone.name(), "Europe/Moscow");
    }

    for category in Category::ALL {
        assert!(
            set.phrases.iter().any(|phrase| phrase.category == category),
            "нет фраз категории «{}»",
            category.label()
        );
    }

    for phrase in &set.phrases {
        match phrase.category {
            Category::Garbage => assert!(
                phrase.expect.items.is_empty(),
                "{}: мусор не должен давать элементы",
                phrase.id
            ),
            Category::Ambiguity => assert!(
                !phrase.expect.clarifications.is_empty(),
                "{}: неоднозначность должна ждать уточнения",
                phrase.id
            ),
            _ => assert!(
                !phrase.expect.items.is_empty(),
                "{}: нет ожидаемых элементов",
                phrase.id
            ),
        }
    }
}

#[test]
fn perfect_answer_scores_one() {
    let result = judge_one(
        &json!([{"title": "Позвонить маме", "kind": "tasks", "list": "Задачи"}]),
        &json!([]),
        response("позвонить   маме"),
    );

    assert!(result.passed(), "{:?}", result.problems);
    assert_eq!(result.totals.expected, 1);
    assert_eq!(result.totals.matched, 1);
    assert_eq!(result.totals.extra, 0);
    assert!((result.accuracy() - 1.0).abs() < f64::EPSILON);
}

#[test]
fn missing_item_lowers_accuracy() {
    let result = judge_one(
        &json!([{"title": "Позвонить маме", "kind": "tasks", "list": "Задачи"}]),
        &json!([]),
        json!({"items": [], "clarifications": []}),
    );

    assert!(!result.passed());
    assert_eq!(result.totals.matched, 0);
    assert_eq!(result.totals.extra, 0);
    assert_eq!(result.accuracy(), 0.0);
}

#[test]
fn extra_item_is_counted_in_the_denominator() {
    let result = judge_one(
        &json!([{"title": "Позвонить маме", "kind": "tasks", "list": "Задачи"}]),
        &json!([]),
        json!({
            "items": [
                {"target_kind": "tasks", "title": "Позвонить маме", "target_list": "Задачи"},
                {"target_kind": "tasks", "title": "Лишняя задача", "target_list": "Задачи"}
            ],
            "clarifications": []
        }),
    );

    assert!(!result.passed());
    assert_eq!(result.totals.matched, 1);
    assert_eq!(result.totals.expected, 1);
    assert_eq!(result.totals.extra, 1);
    assert!((result.accuracy() - 0.5).abs() < f64::EPSILON);
}

#[test]
fn wrong_assignee_does_not_match() {
    let result = judge_one(
        &json!([{
            "title": "Позвонить маме",
            "kind": "tasks",
            "list": "Задачи",
            "assignee": "Анна"
        }]),
        &json!([]),
        json!({
            "items": [{
                "target_kind": "tasks",
                "title": "Позвонить маме",
                "target_list": "Задачи",
                "assignee": "Пётр"
            }],
            "clarifications": []
        }),
    );

    assert!(!result.passed());
    assert_eq!(result.totals.matched, 0);
    assert_eq!(result.accuracy(), 0.0);
}

#[test]
fn title_comparison_ignores_case_spaces_and_yo() {
    let result = judge_one(
        &json!([{"title": "Съесть ещё", "kind": "tasks", "list": "Задачи"}]),
        &json!([]),
        response("  съесть   еще  "),
    );

    assert!(result.passed(), "{:?}", result.problems);
}

#[test]
fn guesses_without_a_clarification_do_not_pass() {
    let expected = json!([{"title": "Позвонить маме", "kind": "tasks", "list": "Задачи"}]);

    let guessed = judge_one(&expected, &json!(["due_date"]), response("Позвонить маме"));
    assert!(
        !guessed.passed(),
        "угадывание без вопроса не должно проходить"
    );
    assert!(!guessed.clarifications_ok);
    assert!(
        guessed.accuracy() < 1.0,
        "пропущенное уточнение должно снижать точность"
    );
    assert_eq!(guessed.totals.expected, 2, "элемент и уточнение");
    assert_eq!(guessed.totals.matched, 1);

    let asked = judge_one(
        &expected,
        &json!(["due_date"]),
        json!({
            "items": [{"target_kind": "tasks", "title": "Позвонить маме", "target_list": "Задачи"}],
            "clarifications": ["Во сколько позвонить?"]
        }),
    );
    assert!(asked.passed(), "{:?}", asked.problems);
    assert!(asked.clarifications_ok);
    assert!((asked.accuracy() - 1.0).abs() < f64::EPSILON);
}

#[test]
fn alternative_titles_match_but_other_titles_do_not() {
    let expected = json!([{
        "title": "Оплачивать квартиру",
        "alt_titles": ["Оплатить квартиру"],
        "kind": "tasks",
        "list": "Задачи"
    }]);

    let alt = judge_one(&expected, &json!([]), response("Оплатить квартиру"));
    assert!(alt.passed(), "{:?}", alt.problems);

    let other = judge_one(&expected, &json!([]), response("Квартира"));
    assert!(!other.passed());
}

/// Фраза с одним ожидаемым уточнением и заданным вопросом модели.
fn judge_question(topic: &str, question: &str) -> PhraseResult {
    judge_one(
        &json!([{"title": "Позвонить маме", "kind": "tasks", "list": "Задачи"}]),
        &json!([topic]),
        json!({
            "items": [{"target_kind": "tasks", "title": "Позвонить маме", "target_list": "Задачи"}],
            "clarifications": [question]
        }),
    )
}

#[test]
fn due_question_is_recognized_in_any_word_form() {
    for question in [
        "На какую дату и время назначена встреча с врачом?",
        "Уточните время встречи",
        "Какое число подойдёт?",
        "До какого срока?",
    ] {
        let result = judge_question("due_date", question);
        assert!(
            result.clarifications_ok,
            "{question}: {:?}",
            result.problems
        );
    }
}

#[test]
fn words_that_only_contain_a_due_stem_are_not_due_questions() {
    for question in [
        "Кому передать документы?",
        "Что нужно сдать?",
        "Сделать это вовремя?",
    ] {
        let result = judge_question("due_date", question);
        assert!(!result.clarifications_ok, "{question}");
    }
}

#[test]
fn assignee_question_is_recognized_in_any_word_form() {
    for question in [
        "Кому поручить?",
        "Какому из них поручить?",
        "Кем это будет сделано?",
        "Кто-то из вас?",
    ] {
        let result = judge_question("assignee", question);
        assert!(
            result.clarifications_ok,
            "{question}: {:?}",
            result.problems
        );
    }
}

#[test]
fn member_name_counts_as_assignee_question_next_to_a_verb_like_peredat() {
    let result = judge_question("assignee", "Пётр или Анна должен передать документы?");

    assert!(result.clarifications_ok, "{:?}", result.problems);
}

#[test]
fn compound_question_covers_both_topics() {
    let result = judge_one(
        &json!([{"title": "Позвонить маме", "kind": "tasks", "list": "Задачи"}]),
        &json!(["assignee", "due_date"]),
        json!({
            "items": [{"target_kind": "tasks", "title": "Позвонить маме", "target_list": "Задачи"}],
            "clarifications": ["Кому поручить и когда?"]
        }),
    );

    assert!(result.passed(), "{:?}", result.problems);
    assert!(result.clarifications_ok);
}

#[test]
fn monthly_anchor_day_matches_missing_month_rule() {
    let result = judge_one(
        &json!([{
            "title": "Оплачивать квартиру",
            "kind": "tasks",
            "list": "Задачи",
            "due_date": "2026-10-30",
            "recurrence": {"freq": "monthly"}
        }]),
        &json!([]),
        json!({
            "items": [{
                "target_kind": "tasks",
                "title": "Оплачивать квартиру",
                "target_list": "Задачи",
                "recurrence": {"freq": "monthly", "month_rule": "anchor_day"}
            }],
            "clarifications": []
        }),
    );

    assert!(result.passed(), "{:?}", result.problems);
}

#[test]
fn guard_blocks_failing_and_stale_baselines() {
    let hash = prompt_hash();
    let entry = |accuracy: f64, hash: &str| BaselineEntry {
        accuracy,
        prompt_hash: hash.to_owned(),
        date: "2026-10-30T12:00:00Z".to_owned(),
        phrases: 36,
        passed: 36,
    };

    assert_eq!(
        check_baseline(&Baseline::default(), PROMPT_VERSION, &hash),
        Ok(GuardOutcome::Skipped)
    );

    let mut other = Baseline::default();
    other.record("capture-v1", entry(0.95, &hash));
    let error = check_baseline(&other, PROMPT_VERSION, &hash).expect_err("версии нет");
    assert!(
        error.contains("прогоните `tasklerq capture-eval`"),
        "{error}"
    );

    let mut low = Baseline::default();
    low.record(PROMPT_VERSION, entry(0.89, &hash));
    let error = check_baseline(&low, PROMPT_VERSION, &hash).expect_err("точность ниже порога");
    assert!(error.contains("0.890"), "{error}");

    let mut stale = Baseline::default();
    stale.record(PROMPT_VERSION, entry(0.95, "deadbeef"));
    let error = check_baseline(&stale, PROMPT_VERSION, &hash).expect_err("чужой хеш");
    assert!(error.contains("исходники промпта"), "{error}");

    let mut passing = Baseline::default();
    passing.record(PROMPT_VERSION, entry(MIN_ACCURACY, &hash));
    assert_eq!(
        check_baseline(&passing, PROMPT_VERSION, &hash),
        Ok(GuardOutcome::Passed)
    );
}

#[test]
fn prompt_version_has_a_passing_measured_baseline() {
    let baseline = Baseline::parse(BASELINE).expect("baseline json");

    match check_baseline(&baseline, PROMPT_VERSION, &prompt_hash()) {
        Ok(GuardOutcome::Skipped) => eprintln!(
            "golden baseline пуст: живой замер ещё не выполнялся, guard пропущен; \
             прогоните `tasklerq capture-eval`."
        ),
        Ok(GuardOutcome::Passed) => {}
        Err(error) => panic!("{error}"),
    }
}
