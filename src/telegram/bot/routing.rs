//! Маршрутизация свободного текста по активному экрану.
//!
//! Одна таблица задаёт и путь текста, и подсказку ввода: подсказка строится из
//! [`TextRoute`], поэтому поведение и обещание экрана не расходятся.

use crate::domain::error::DomainError;
use crate::domain::models::{List, ListKind};
use crate::storage::repositories::lists;
use crate::telegram::ru;
use crate::telegram::views::{self, Lookup};

use super::model::{Form, Screen};
use super::{Ctx, Fail};

/// Куда уходит свободный текст на активном экране.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextRoute {
    /// Главное меню: общий разбор, подсказка меню.
    Menu,
    /// Открытое поле редактора, которое ждёт значение.
    EditorField,
    /// Редактор открыт, но текст сейчас не принимает.
    EditorBlocked,
    /// Поиск или результаты поиска: текст — новый запрос.
    Search,
    /// Список покупок или чек-лист: текст уходит в разбор с этим списком.
    ShoppingList {
        /// Список, в который будут добавлены позиции.
        list_id: uuid::Uuid,
        /// Название списка для подсказки.
        list_name: String,
        /// Прямое добавление без черновика (только для покупок).
        direct: bool,
    },
    /// Список задач: разбор с этим списком по умолчанию.
    TaskList {
        /// Список, выбранный по умолчанию.
        list_id: uuid::Uuid,
        /// Название списка для подсказки.
        list_name: String,
    },
    /// Остальные экраны: общий разбор.
    General,
}

impl TextRoute {
    /// Путь текста по экрану, активной форме и списку экрана.
    pub fn classify(screen: &Screen, form: Option<&Form>, list: Option<&List>) -> Self {
        let editor = match form {
            Some(Form::Editor(editor)) => Some(editor),
            _ => None,
        };
        match screen {
            Screen::Menu => Self::Menu,
            Screen::Editor { .. } => match editor.and_then(|editor| editor.field) {
                Some(step) if step.expects_text() => Self::EditorField,
                _ => Self::EditorBlocked,
            },
            Screen::Form => match form {
                Some(Form::Search) => Self::Search,
                Some(Form::Editor(editor)) => match editor.field {
                    Some(step) if step.expects_text() => Self::EditorField,
                    _ => Self::EditorBlocked,
                },
                _ => Self::General,
            },
            Screen::Results { .. } => Self::Search,
            Screen::List { .. } => match list {
                Some(list) if list.kind == ListKind::Tasks => Self::TaskList {
                    list_id: list.id,
                    list_name: list.name.clone(),
                },
                Some(list) => Self::ShoppingList {
                    list_id: list.id,
                    list_name: list.name.clone(),
                    direct: list.kind == ListKind::Shopping,
                },
                None => Self::General,
            },
            Screen::Welcome
            | Screen::FirstContact(_)
            | Screen::Lists
            | Screen::Item { .. }
            | Screen::Due { .. }
            | Screen::Deleted { .. }
            | Screen::ConfirmDelete { .. }
            | Screen::ConfirmClear { .. }
            | Screen::Cleared { .. }
            | Screen::ConfirmRemove { .. }
            | Screen::ConfirmTransfer { .. }
            | Screen::ConfirmRevokeToken { .. }
            | Screen::ConfirmLeave
            | Screen::InviteCreated { .. }
            | Screen::Mine { .. }
            | Screen::Settings
            | Screen::Profile
            | Screen::Danger
            | Screen::Notifications
            | Screen::LeadTime
            | Screen::Timezones
            | Screen::Family
            | Screen::Integrations { .. }
            | Screen::Member { .. }
            | Screen::Invites
            | Screen::Journal { .. }
            | Screen::Processing { .. }
            | Screen::Draft { .. } => Self::General,
        }
    }

    /// Подсказка ввода, которую показывает этот путь. `None` — экран уже
    /// объясняет ввод сам (поле редактора) или текст здесь ничего не делает.
    pub fn hint(&self, llm_capture: bool) -> Option<String> {
        match self {
            Self::Menu => Some(
                if llm_capture {
                    ru::MENU_HINT
                } else {
                    ru::MENU_HINT_NO_LLM
                }
                .to_owned(),
            ),
            Self::Search => Some(ru::SEARCH_HINT.to_owned()),
            Self::ShoppingList { list_name, .. } | Self::TaskList { list_name, .. } => {
                Some(ru::list_hint(list_name))
            }
            Self::EditorField | Self::EditorBlocked | Self::General => None,
        }
    }
}

/// Путь текста для активного экрана чата.
pub async fn route(ctx: &mut Ctx<'_>, screen: &Screen) -> Result<TextRoute, Fail> {
    let form = match screen {
        Screen::Editor { .. } | Screen::Form => active_form(ctx).await?,
        _ => None,
    };
    let list = match screen {
        Screen::List { list_id, .. } => {
            let household_id = ctx.household()?.id;
            match lists::get_visible(&mut *ctx.conn, household_id, *list_id).await {
                Ok(list) => Some(list),
                Err(DomainError::NotFound) => None,
                Err(error) => return Err(error.into()),
            }
        }
        _ => None,
    };
    Ok(TextRoute::classify(screen, form.as_ref(), list.as_ref()))
}

async fn active_form(ctx: &mut Ctx<'_>) -> Result<Option<Form>, Fail> {
    match views::conversation(&mut *ctx.conn, ctx.chat_id, ctx.now).await? {
        Lookup::Active(conversation) if conversation.telegram_user_id == ctx.sender.id => {
            Ok(super::forms::parse(conversation.data))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use chrono::Utc;
    use uuid::Uuid;

    use super::*;
    use crate::domain::models::{List, ListKind};
    use crate::telegram::bot::model::{
        Editor, EditorTarget, FirstContactScreen, ItemDraft, StatusFilter, Step,
    };

    fn editor(field: Option<Step>) -> Editor {
        Editor {
            target: EditorTarget::New {
                kind: ListKind::Tasks,
                back: Screen::Menu,
            },
            draft: ItemDraft::default(),
            touched: BTreeSet::new(),
            field,
            field_back: None,
            field_backup: None,
            list_id: Uuid::nil(),
            error: None,
            conflict: None,
            conflict_seen: None,
            duplicate: None,
        }
    }

    fn list(kind: ListKind, name: &str) -> List {
        List {
            id: Uuid::new_v4(),
            household_id: Uuid::nil(),
            kind,
            name: name.to_owned(),
            is_default: false,
            is_archived: false,
            position: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            deleted_at: None,
        }
    }

    fn screens() -> Vec<(Screen, Option<Form>, Option<List>)> {
        let tasks = list(ListKind::Tasks, "Дача");
        let shopping = list(ListKind::Shopping, "Покупки");
        let back = Box::new(Screen::Menu);
        vec![
            (Screen::Menu, None, None),
            (Screen::Welcome, None, None),
            (
                Screen::FirstContact(FirstContactScreen::AskInvite),
                None,
                None,
            ),
            (Screen::Lists, None, None),
            (
                Screen::List {
                    list_id: tasks.id,
                    page: 0,
                    edit: false,
                    from_lists: false,
                },
                None,
                Some(tasks),
            ),
            (
                Screen::List {
                    list_id: shopping.id,
                    page: 0,
                    edit: false,
                    from_lists: false,
                },
                None,
                Some(shopping),
            ),
            (
                Screen::List {
                    list_id: Uuid::nil(),
                    page: 0,
                    edit: false,
                    from_lists: false,
                },
                None,
                None,
            ),
            (
                Screen::Item {
                    item_id: Uuid::nil(),
                    back: back.clone(),
                },
                None,
                None,
            ),
            (
                Screen::Due {
                    item_id: Uuid::nil(),
                    back: back.clone(),
                },
                None,
                None,
            ),
            (
                Screen::Editor {
                    item_id: None,
                    back: back.clone(),
                },
                None,
                None,
            ),
            (
                Screen::Editor {
                    item_id: None,
                    back: back.clone(),
                },
                Some(Form::Editor(editor(Some(Step::Title)))),
                None,
            ),
            (
                Screen::Editor {
                    item_id: None,
                    back: back.clone(),
                },
                Some(Form::Editor(editor(Some(Step::Due)))),
                None,
            ),
            (
                Screen::Deleted {
                    item_id: Uuid::nil(),
                    title: String::new(),
                    back: back.clone(),
                },
                None,
                None,
            ),
            (
                Screen::ConfirmDelete {
                    item_id: Uuid::nil(),
                    back: back.clone(),
                },
                None,
                None,
            ),
            (
                Screen::ConfirmClear {
                    list_id: Uuid::nil(),
                    from_lists: false,
                },
                None,
                None,
            ),
            (
                Screen::Cleared {
                    list_id: Uuid::nil(),
                    item_ids: Vec::new(),
                    from_lists: false,
                },
                None,
                None,
            ),
            (
                Screen::ConfirmRemove {
                    user_id: Uuid::nil(),
                },
                None,
                None,
            ),
            (
                Screen::ConfirmTransfer {
                    user_id: Uuid::nil(),
                },
                None,
                None,
            ),
            (
                Screen::ConfirmRevokeToken {
                    token_id: Uuid::nil(),
                },
                None,
                None,
            ),
            (Screen::ConfirmLeave, None, None),
            (
                Screen::InviteCreated {
                    invite_id: Uuid::nil(),
                    code: String::new(),
                },
                None,
                None,
            ),
            (Screen::Mine { page: 0 }, None, None),
            (
                Screen::Results {
                    query: String::new(),
                    status: StatusFilter::Open,
                    page: 0,
                },
                None,
                None,
            ),
            (Screen::Settings, None, None),
            (Screen::Profile, None, None),
            (Screen::Danger, None, None),
            (Screen::Notifications, None, None),
            (Screen::LeadTime, None, None),
            (Screen::Timezones, None, None),
            (Screen::Family, None, None),
            (Screen::Integrations { page: 0 }, None, None),
            (
                Screen::Member {
                    user_id: Uuid::nil(),
                },
                None,
                None,
            ),
            (Screen::Invites, None, None),
            (Screen::Journal { page: 0 }, None, None),
            (Screen::Form, None, None),
            (Screen::Form, Some(Form::Search), None),
            (Screen::Form, Some(Form::Onboarding { name: None }), None),
            (Screen::Form, Some(Form::Editor(editor(None))), None),
            (Screen::Processing { job_id: None }, None, None),
            (
                Screen::Draft {
                    draft_id: Uuid::nil(),
                },
                None,
                None,
            ),
        ]
    }

    #[test]
    fn every_screen_hint_matches_the_text_route() {
        for (screen, form, list) in screens() {
            let route = TextRoute::classify(&screen, form.as_ref(), list.as_ref());
            let expected = |llm_capture: bool| match &screen {
                Screen::Menu => Some(
                    if llm_capture {
                        ru::MENU_HINT
                    } else {
                        ru::MENU_HINT_NO_LLM
                    }
                    .to_owned(),
                ),
                Screen::Results { .. } => Some(ru::SEARCH_HINT.to_owned()),
                Screen::List { .. } => list.as_ref().map(|list| ru::list_hint(&list.name)),
                Screen::Form => match form.as_ref() {
                    Some(Form::Search) => Some(ru::SEARCH_HINT.to_owned()),
                    _ => None,
                },
                Screen::Welcome
                | Screen::FirstContact(_)
                | Screen::Lists
                | Screen::Item { .. }
                | Screen::Due { .. }
                | Screen::Editor { .. }
                | Screen::Deleted { .. }
                | Screen::ConfirmDelete { .. }
                | Screen::ConfirmClear { .. }
                | Screen::Cleared { .. }
                | Screen::ConfirmRemove { .. }
                | Screen::ConfirmTransfer { .. }
                | Screen::ConfirmRevokeToken { .. }
                | Screen::ConfirmLeave
                | Screen::InviteCreated { .. }
                | Screen::Mine { .. }
                | Screen::Settings
                | Screen::Profile
                | Screen::Danger
                | Screen::Notifications
                | Screen::LeadTime
                | Screen::Timezones
                | Screen::Family
                | Screen::Integrations { .. }
                | Screen::Member { .. }
                | Screen::Invites
                | Screen::Journal { .. }
                | Screen::Processing { .. }
                | Screen::Draft { .. } => None,
            };
            for llm_capture in [true, false] {
                assert_eq!(
                    route.hint(llm_capture),
                    expected(llm_capture),
                    "hint and text path disagree for {screen:?}, llm_capture={llm_capture}"
                );
            }
        }
    }

    #[test]
    fn a_tasks_list_routes_capture_with_its_own_id() {
        let tasks = list(ListKind::Tasks, "Дача");
        let screen = Screen::List {
            list_id: tasks.id,
            page: 0,
            edit: false,
            from_lists: false,
        };
        assert_eq!(
            TextRoute::classify(&screen, None, Some(&tasks)),
            TextRoute::TaskList {
                list_id: tasks.id,
                list_name: "Дача".to_owned(),
            }
        );
    }

    #[test]
    fn an_editor_between_fields_blocks_text() {
        let screen = Screen::Editor {
            item_id: None,
            back: Box::new(Screen::Menu),
        };
        let form = Form::Editor(editor(None));
        assert_eq!(
            TextRoute::classify(&screen, Some(&form), None),
            TextRoute::EditorBlocked
        );
    }

    #[test]
    fn a_checklist_list_is_not_direct_but_keeps_its_hint() {
        let checklist = list(ListKind::Checklist, "Дача");
        let screen = Screen::List {
            list_id: checklist.id,
            page: 0,
            edit: false,
            from_lists: false,
        };
        assert_eq!(
            TextRoute::classify(&screen, None, Some(&checklist)),
            TextRoute::ShoppingList {
                list_id: checklist.id,
                list_name: "Дача".to_owned(),
                direct: false,
            }
        );
        assert_eq!(
            TextRoute::classify(&screen, None, Some(&checklist)).hint(true),
            Some(ru::list_hint("Дача"))
        );
    }
}
