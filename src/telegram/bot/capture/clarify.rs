//! Уточняющие вопросы в карточке черновика.
//!
//! Один вопрос за раз: сначала неоднозначный исполнитель, затем
//! противоречивая дата. Ответ кнопкой правит черновик напрямую, без LLM.
//! Какие ошибки считаются вопросом, решает [`DraftItem`] — тот же код, что
//! проверяет ответ домена, поэтому карточка не может предложить кнопки для
//! вопроса, который домен уже не считает актуальным.

use crate::capture::context::MemberRef;
use crate::capture::draft::{DraftView, DueChoice};
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
        ui = ui.bold(format!("Какой исполнитель у «{}»?", item.title));
        let buttons = item
            .assignee_candidates
            .iter()
            .filter_map(|id| {
                view.context
                    .members
                    .iter()
                    .find(|member| member.id == *id)
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
            })
            .collect();
        return ui.row(buttons);
    }

    if let Some((index, item)) = first_date_question(view) {
        return ui
            .bold(format!("Уточните срок для «{}»:", item.title))
            .row(vec![
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
            ]);
    }

    // Вопросы модели без готовых вариантов: показать их можно, но ответ
    // придётся внести через форму. Новый разбор текста не запускаем.
    for question in view.validated.clarifications.iter().take(remaining) {
        ui = ui.text(format!("❓ {question}"));
    }
    if !view.validated.clarifications.is_empty() {
        ui = ui.text("Поправьте элемент через «Изменить».");
    }
    ui
}

fn open_question(view: &DraftView) -> bool {
    first_assignee_question(view).is_some() || first_date_question(view).is_some()
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

fn member_label(member: &MemberRef, members: &[MemberRef]) -> String {
    if let Some(username) = member.username.as_deref() {
        return format!("{} (@{username})", member.display_name);
    }
    let same_name = members
        .iter()
        .filter(|other| other.display_name == member.display_name)
        .count();
    if same_name <= 1 {
        return member.display_name.clone();
    }
    // Номер тёзки следует за порядком `users::list_active`. Карточка
    // перерисовывается после каждого ответа, поэтому подпись и токен кнопки
    // всегда берутся из одного снимка участников.
    let number = members
        .iter()
        .filter(|other| other.display_name == member.display_name)
        .position(|other| other.id == member.id)
        .map_or(1, |index| index + 1);
    format!("{} · {number}", member.display_name)
}
