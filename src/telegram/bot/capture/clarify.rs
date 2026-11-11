//! Уточняющие вопросы в карточке черновика.
//!
//! Один вопрос за раз: сначала неоднозначный исполнитель и противоречивая дата
//! из ошибок валидатора, затем открытые уточнения модели в порядке ответа.
//! Ответ кнопкой правит черновик напрямую, без LLM: тема уточнения выбирает
//! готовый экран — участники, «Сегодня/Завтра/Выбрать дату», списки нужного
//! вида, «Весь день» и ввод времени текстом. Какие ошибки считаются вопросом,
//! решает [`DraftItem`] — тот же код, что проверяет ответ домена, поэтому
//! карточка не может предложить кнопки для вопроса, который домен уже не
//! считает актуальным.

use crate::capture::context::MemberRef;
use crate::capture::draft::{DraftView, DueChoice};
use crate::capture::schema::{Clarification, ClarificationTopic};
use crate::capture::validate::DraftItem;
use crate::telegram::rich::button;

use super::super::model::{Action, Ui};

pub(super) fn clarification(mut ui: Ui, view: &DraftView) -> Ui {
    let draft_id = view.draft.id;
    let remaining = view.clarifications_left();

    if remaining == 0 {
        if open_question(view) {
            ui = ui
                .bold("Больше не буду уточнять автоматически.")
                .text("Откройте «Изменить», чтобы поправить элемент вручную.");
        }
        return ui;
    }

    if let Some((index, item)) = first_assignee_question(view) {
        return with_rows(
            ui.bold(format!("Какой исполнитель у «{}»?", item.title)),
            member_buttons(view, draft_id, index, Some(&item.assignee_candidates)),
        );
    }

    if let Some((index, item)) = first_date_question(view) {
        return ui
            .bold(format!("Уточните срок для «{}»:", item.title))
            .row(date_buttons(draft_id, index));
    }

    for clarification in &view.clarifications {
        if !model_question_open(view, clarification) {
            continue;
        }
        let index = clarification.item;
        let Some(item) = view.validated.items.get(index) else {
            continue;
        };
        match clarification.topic {
            ClarificationTopic::Assignee => {
                return with_rows(
                    ui.bold(format!("Какой исполнитель у «{}»?", item.title)),
                    member_buttons(view, draft_id, index, None),
                );
            }
            ClarificationTopic::Date => {
                return ui
                    .bold(format!("Уточните срок для «{}»:", item.title))
                    .row(date_buttons(draft_id, index));
            }
            ClarificationTopic::Time => {
                return ui.bold(format!("Во сколько «{}»?", item.title)).row(vec![
                    button("Весь день", Action::DraftAllDay { draft_id, index }),
                    button("Ввести время", Action::DraftTime { draft_id, index }),
                ]);
            }
            ClarificationTopic::List => {
                let lists: Vec<_> = view
                    .context
                    .lists
                    .iter()
                    .filter(|list| list.kind == item.kind)
                    .collect();
                let buttons: Vec<_> = lists
                    .iter()
                    .map(|list| {
                        button(
                            list.name.clone(),
                            Action::DraftList {
                                draft_id,
                                index,
                                list_id: list.id,
                            },
                        )
                    })
                    .collect();
                return with_rows(
                    ui.bold(format!("В какой список «{}»?", item.title)),
                    buttons.chunks(2).map(<[_]>::to_vec).collect(),
                );
            }
        }
    }
    ui
}

fn open_question(view: &DraftView) -> bool {
    first_assignee_question(view).is_some()
        || first_date_question(view).is_some()
        || view
            .clarifications
            .iter()
            .any(|clarification| model_question_open(view, clarification))
}

/// Уточнение модели можно показать: элемент существует, поле не заполнено, а
/// у вопроса есть хотя бы один осмысленный ответ.
fn model_question_open(view: &DraftView, clarification: &Clarification) -> bool {
    let Some(item) = view.validated.items.get(clarification.item) else {
        return false;
    };
    let Some(raw) = view.raw.get(clarification.item) else {
        return false;
    };
    if clarification.topic.is_filled(raw, Some(&view.context)) {
        return false;
    }
    match clarification.topic {
        // Время без даты валидатор отвергает: сначала должен появиться срок.
        ClarificationTopic::Time => raw.due_date.is_some(),
        ClarificationTopic::List => view.context.lists.iter().any(|list| list.kind == item.kind),
        ClarificationTopic::Date | ClarificationTopic::Assignee => true,
    }
}

fn first_assignee_question(view: &DraftView) -> Option<(usize, &DraftItem)> {
    view.validated
        .items
        .iter()
        .enumerate()
        .find(|(_, item)| item.has_assignee_question())
}

fn first_date_question(view: &DraftView) -> Option<(usize, &DraftItem)> {
    view.validated
        .items
        .iter()
        .enumerate()
        .find(|(_, item)| item.has_date_question())
}

fn with_rows(mut ui: Ui, rows: Vec<Vec<crate::telegram::rich::Button<Action>>>) -> Ui {
    for row in rows {
        ui = ui.row(row);
    }
    ui
}

fn date_buttons(draft_id: uuid::Uuid, index: usize) -> Vec<crate::telegram::rich::Button<Action>> {
    vec![
        button(
            "Сегодня",
            Action::DraftDue {
                draft_id,
                index,
                choice: DueChoice::Today,
            },
        ),
        button(
            "Завтра",
            Action::DraftDue {
                draft_id,
                index,
                choice: DueChoice::Tomorrow,
            },
        ),
        button("Выбрать дату", Action::DraftEditDue { draft_id, index }),
    ]
}

/// Кнопки участников: тёзки различимы по username или номеру. `allowed` —
/// кандидаты неоднозначного имени; у вопроса модели ограничения нет.
fn member_buttons(
    view: &DraftView,
    draft_id: uuid::Uuid,
    index: usize,
    allowed: Option<&[uuid::Uuid]>,
) -> Vec<Vec<crate::telegram::rich::Button<Action>>> {
    let buttons: Vec<_> = view
        .context
        .members
        .iter()
        .filter(|member| allowed.is_none_or(|ids| ids.contains(&member.id)))
        .map(|member| {
            button(
                member_label(member, &view.context.members),
                Action::DraftAssign {
                    draft_id,
                    index,
                    user_id: member.id,
                },
            )
        })
        .collect();
    buttons.chunks(2).map(<[_]>::to_vec).collect()
}

fn member_label(member: &MemberRef, members: &[MemberRef]) -> String {
    if let Some(username) = member.username.as_deref() {
        return format!("{} (@{username})", member.display_name);
    }
    let namesakes: Vec<&MemberRef> = members
        .iter()
        .filter(|other| other.display_name == member.display_name)
        .collect();
    if namesakes.len() <= 1 {
        return member.display_name.clone();
    }
    // Номер тёзки следует за порядком `users::list_active`. Карточка
    // перерисовывается после каждого ответа, поэтому подпись и токен кнопки
    // всегда берутся из одного снимка участников.
    let number = namesakes
        .iter()
        .position(|other| other.id == member.id)
        .map_or(1, |index| index + 1);
    format!("{} · {number}", member.display_name)
}
