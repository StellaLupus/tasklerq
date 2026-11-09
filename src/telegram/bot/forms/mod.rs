//! Формы бота: единый редактор элемента, поиск и ввод значений настроек.
//!
//! Состояние формы лежит в `conversation_states` (одна форма на чат, 15
//! минут). Редактор копит поля в черновике и сохраняет их одной операцией:
//! для нового элемента создаёт его, для существующего применяет изменения, для
//! пункта черновика LLM переписывает сам пункт.

use uuid::Uuid;

use crate::domain::first_contact::classify;
use crate::domain::models::{List, ListKind};
use crate::domain::validation::{MAX_TITLE, sanitize};
use crate::storage::repositories::{households, items, lists};
use crate::telegram::views::{self, Lookup};

use super::model::{
    Editor, EditorTarget, FirstContactScreen, Form, ItemDraft, ItemSnapshot, Screen, Step,
    TimeField,
};
use super::{Ctx, Fail};

mod actions;
mod editor;
mod state;
mod view;

pub use actions::{on_action, on_text};
pub use editor::screen as editor_screen;
pub use view::view;

pub(super) use crate::domain::validation::MAX_INPUT_QUANTITY;
pub(super) const MAX_QUERY: usize = 100;

// ---------------------------------------------------------------------
// Хранение формы
// ---------------------------------------------------------------------

async fn put(ctx: &mut Ctx<'_>, form: &Form) -> Result<(), Fail> {
    let data = super::to_json(form)?;
    views::put_conversation(
        &mut *ctx.conn,
        ctx.chat_id,
        ctx.sender.id,
        form.kind_name(),
        &data,
        ctx.now,
    )
    .await?;
    Ok(())
}

async fn clear(ctx: &mut Ctx<'_>) -> Result<(), Fail> {
    views::clear_conversation(&mut *ctx.conn, ctx.chat_id).await?;
    Ok(())
}

/// Загрузить активную форму отправителя.
async fn load(ctx: &mut Ctx<'_>) -> Result<Option<Form>, Fail> {
    match views::conversation(&mut *ctx.conn, ctx.chat_id, ctx.now).await? {
        Lookup::Active(conversation) if conversation.telegram_user_id == ctx.sender.id => {
            Ok(parse(conversation.data))
        }
        _ => Ok(None),
    }
}

/// Разобрать состояние формы.
///
/// Сохранённый JSON прежних форм (`{"form":"create", …}`, `{"form":"edit", …}`)
/// больше не разбирается в `Editor`: он превращается в [`Form::LegacyEdit`], и
/// бот сообщает, что правка устарела.
pub(super) fn parse(data: serde_json::Value) -> Option<Form> {
    match serde_json::from_value::<Form>(data.clone()) {
        Ok(form) => Some(form),
        Err(_)
            if matches!(
                data.get("form").and_then(|form| form.as_str()),
                Some("create" | "edit")
            ) =>
        {
            Some(Form::LegacyEdit)
        }
        Err(_) => None,
    }
}

/// Показать, что активная форма истекла.
///
/// У редактора есть кнопка возврата на карточку; остальные формы, как и
/// раньше, ведут в меню.
pub(super) async fn expired(ctx: &mut Ctx<'_>) -> Result<(), Fail> {
    let screen = views::get(&mut *ctx.conn, ctx.chat_id)
        .await?
        .and_then(|view| serde_json::from_value::<Screen>(view.state).ok());
    clear(ctx).await?;
    if let Some(Screen::Editor { item_id, back }) = screen {
        ctx.banner("Ваша правка устарела. Начнём заново?");
        return ctx.present(Screen::Editor { item_id, back }).await;
    }
    ctx.banner("Форма устарела. Начнём заново?");
    if ctx.user.is_none() {
        start_onboarding(ctx).await
    } else {
        ctx.present(Screen::Menu).await
    }
}

/// Открыта ли у отправителя форма онбординга.
pub(super) async fn onboarding_is_open(ctx: &mut Ctx<'_>) -> Result<bool, Fail> {
    Ok(matches!(load(ctx).await?, Some(Form::Onboarding { .. })))
}

async fn open(ctx: &mut Ctx<'_>, form: Form) -> Result<(), Fail> {
    put(ctx, &form).await?;
    ctx.present(Screen::Form).await
}

// ---------------------------------------------------------------------
// Входы в формы
// ---------------------------------------------------------------------

/// Открыть единый редактор, уже собранный вызывающей стороной.
pub async fn open_editor(ctx: &mut Ctx<'_>, editor: Editor) -> Result<(), Fail> {
    editor::show(ctx, editor).await
}

/// Начать создание элемента через единый редактор.
///
/// Из списка тип и список уже известны; из меню берётся список по умолчанию
/// (или первый список этого типа). Поле «Название» открывается сразу.
pub async fn start_new_editor(
    ctx: &mut Ctx<'_>,
    list_id: Option<Uuid>,
    kind: ListKind,
    back: Option<Screen>,
) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let list = match list_id {
        Some(list_id) => lists::get_visible(&mut *ctx.conn, household_id, list_id).await?,
        None => {
            let candidates = lists_of_kind(ctx, kind).await?;
            candidates
                .iter()
                .find(|list| list.is_default)
                .or_else(|| candidates.first())
                .cloned()
                .ok_or_else(|| Fail::User("Нет списка для сохранения.".into()))?
        }
    };
    let editor = Editor {
        target: EditorTarget::New {
            kind: list.kind,
            back: back.unwrap_or(Screen::Menu),
        },
        draft: ItemDraft::default(),
        touched: std::collections::BTreeSet::new(),
        field: Some(Step::Title),
        field_back: None,
        field_backup: None,
        list_id: list.id,
        error: None,
        conflict: None,
        conflict_seen: None,
        duplicate: None,
    };
    editor::show(ctx, editor).await
}

/// Свободный текст без формы: редактор нового элемента с готовым названием.
pub async fn quick_add(ctx: &mut Ctx<'_>, text: &str) -> Result<(), Fail> {
    quick_add_in_list(ctx, text, None).await
}

/// Свободный текст без формы: редактор нового элемента в выбранном списке.
/// Исчезнувший список заменяется обычным списком задач по умолчанию.
pub async fn quick_add_in_list(
    ctx: &mut Ctx<'_>,
    text: &str,
    list_id: Option<Uuid>,
) -> Result<(), Fail> {
    let cleaned = sanitize(text);
    let text = cleaned.trim();
    let household_id = ctx.household()?.id;
    if text.chars().count() > MAX_TITLE {
        ctx.banner(format!(
            "Название слишком длинное: не больше {MAX_TITLE} символов."
        ));
        return ctx.present(Screen::Menu).await;
    }
    let list = match list_id {
        Some(list_id) => match lists::get_visible(&mut *ctx.conn, household_id, list_id).await {
            Ok(list) if list.kind == ListKind::Tasks => list,
            Ok(_) | Err(crate::domain::error::DomainError::NotFound) => {
                default_tasks_list(ctx).await?
            }
            Err(error) => return Err(error.into()),
        },
        None => default_tasks_list(ctx).await?,
    };

    let editor = Editor {
        target: EditorTarget::New {
            kind: list.kind,
            back: Screen::Menu,
        },
        draft: ItemDraft {
            title: text.to_owned(),
            ..ItemDraft::default()
        },
        touched: std::collections::BTreeSet::new(),
        field: None,
        field_back: None,
        field_backup: None,
        list_id: list.id,
        error: None,
        conflict: None,
        conflict_seen: None,
        duplicate: None,
    };
    editor::show(ctx, editor).await
}

async fn default_tasks_list(ctx: &mut Ctx<'_>) -> Result<List, Fail> {
    let household_id = ctx.household()?.id;
    let all = lists::list(&mut *ctx.conn, household_id).await?;
    all.iter()
        .filter(|list| list.kind == ListKind::Tasks && !list.is_archived)
        .find(|list| list.is_default)
        .or_else(|| {
            all.iter()
                .find(|list| list.kind == ListKind::Tasks && !list.is_archived)
        })
        .cloned()
        .ok_or_else(|| Fail::User("Нет списка задач для сохранения.".into()))
}

/// Показать результаты нового поиска. `on_error` — экран, на котором остаётся
/// пользователь, если запрос слишком длинный.
pub async fn search(ctx: &mut Ctx<'_>, text: &str, on_error: Screen) -> Result<(), Fail> {
    if text.chars().count() > MAX_QUERY {
        ctx.banner(format!(
            "Запрос слишком длинный: не больше {MAX_QUERY} символов."
        ));
        return ctx.present(on_error).await;
    }
    clear(ctx).await?;
    ctx.present(Screen::Results {
        query: text.to_owned(),
        status: super::model::StatusFilter::Open,
        page: 0,
    })
    .await
}

/// Открыть единый редактор существующего элемента.
pub async fn start_editor(ctx: &mut Ctx<'_>, item_id: Uuid, back: Screen) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let item = items::get(&mut *ctx.conn, household_id, item_id).await?;
    let draft = ItemDraft::from_item(&item, ctx.tz);
    let editor = Editor {
        target: EditorTarget::Item {
            item_id,
            base: Box::new(ItemSnapshot::from_item(&item)),
            back,
        },
        draft,
        touched: std::collections::BTreeSet::new(),
        field: None,
        field_back: None,
        field_backup: None,
        list_id: item.list_id,
        error: None,
        conflict: None,
        conflict_seen: None,
        duplicate: None,
    };
    editor::show(ctx, editor).await
}

/// Начать ввод даты и времени для быстрого срока.
pub async fn start_due_text(
    ctx: &mut Ctx<'_>,
    item_id: Uuid,
    version: i32,
    back: Screen,
) -> Result<(), Fail> {
    if !super::card::due_editable_item(ctx, item_id).await? {
        return ctx
            .present(Screen::Item {
                item_id,
                back: Box::new(back),
                completion_id: None,
            })
            .await;
    }
    let household_id = ctx.household()?.id;
    items::get(&mut *ctx.conn, household_id, item_id).await?;
    open(
        ctx,
        Form::DueText {
            item_id,
            version,
            back,
        },
    )
    .await
}

/// Начать ввод поискового запроса.
pub async fn start_search(ctx: &mut Ctx<'_>) -> Result<(), Fail> {
    ctx.user()?;
    open(ctx, Form::Search).await
}

/// Начать ввод нового имени.
pub async fn start_rename(ctx: &mut Ctx<'_>) -> Result<(), Fail> {
    ctx.user()?;
    open(ctx, Form::Rename).await
}

/// Начать подтверждение удаления household.
pub async fn start_delete_household(ctx: &mut Ctx<'_>) -> Result<(), Fail> {
    ctx.require_owner()?;
    open(ctx, Form::DeleteHousehold).await
}

/// Начать ввод своего времени напоминания.
pub async fn start_lead(ctx: &mut Ctx<'_>) -> Result<(), Fail> {
    ctx.user()?;
    open(ctx, Form::Lead).await
}

/// Начать ввод времени настройки уведомлений.
pub async fn start_time(ctx: &mut Ctx<'_>, field: TimeField) -> Result<(), Fail> {
    ctx.user()?;
    open(ctx, Form::Time { field }).await
}

/// Начать первый контакт: онбординг или информационный экран.
///
/// Классификатор решает по состоянию инсталляции, режиму размещения и
/// `bootstrap_owner`. В неонбординговых исходах незавершённая форма
/// онбординга закрывается, а форма не открывается.
///
/// Если отправитель уже стал участником, пока форма была открыта
/// (`admin user add` или `admin household create --owner-telegram-id`), форма
/// закрывается, а вместо неё показывается меню.
pub async fn start_onboarding(ctx: &mut Ctx<'_>) -> Result<(), Fail> {
    if ctx.user.is_some() {
        if matches!(load(ctx).await?, Some(Form::Onboarding { .. })) {
            clear(ctx).await?;
        }
        return ctx.present(Screen::Menu).await;
    }

    let installation = households::installation(&mut *ctx.conn).await?;
    let contact = classify(
        ctx.tenancy.mode,
        ctx.tenancy.bootstrap_owner,
        ctx.sender.id,
        installation,
    );
    let Some(screen) = FirstContactScreen::from_contact(contact) else {
        return open(ctx, Form::Onboarding { name: None }).await;
    };
    if matches!(load(ctx).await?, Some(Form::Onboarding { .. })) {
        clear(ctx).await?;
    }
    ctx.present(Screen::FirstContact(screen)).await
}

/// Сохранить состояние онбординга.
pub async fn put_onboarding(ctx: &mut Ctx<'_>, name: Option<String>) -> Result<(), Fail> {
    put(ctx, &Form::Onboarding { name }).await
}

/// Завершить форму.
pub async fn finish(ctx: &mut Ctx<'_>) -> Result<(), Fail> {
    clear(ctx).await
}

async fn lists_of_kind(ctx: &mut Ctx<'_>, kind: ListKind) -> Result<Vec<List>, Fail> {
    let household_id = ctx.household()?.id;
    let wanted_shopping = kind != ListKind::Tasks;
    Ok(lists::list(&mut *ctx.conn, household_id)
        .await?
        .into_iter()
        .filter(|list| !list.is_archived && (list.kind != ListKind::Tasks) == wanted_shopping)
        .collect())
}
