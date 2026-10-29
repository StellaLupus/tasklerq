//! Пошаговые формы: создание и редактирование элементов, поиск и ввод
//! значений настроек.
//!
//! Состояние формы лежит в `conversation_states` (одна форма на чат, 15
//! минут). Создание копит поля в черновике и сохраняет их одной операцией;
//! редактирование применяет каждое изменение к элементу сразу, поэтому
//! черновик там — это текущее состояние элемента.

use uuid::Uuid;

use crate::domain::models::{List, ListKind};
use crate::domain::validation::MAX_TITLE;
use crate::storage::repositories::{items, lists};
use crate::telegram::views::{self, Lookup};

use super::model::{EditForm, Form, ItemDraft, ItemForm, Screen, Step};
use super::{Ctx, Fail};

mod actions;
mod state;
mod view;

pub use actions::{on_action, on_text};
pub use view::view;

pub(super) const MAX_QUANTITY: f64 = 9_999.0;
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
            Ok(serde_json::from_value(conversation.data).ok())
        }
        _ => Ok(None),
    }
}

async fn open(ctx: &mut Ctx<'_>, form: Form) -> Result<(), Fail> {
    put(ctx, &form).await?;
    ctx.present(Screen::Form).await
}

// ---------------------------------------------------------------------
// Входы в формы
// ---------------------------------------------------------------------

/// Начать создание элемента.
pub async fn start_create(
    ctx: &mut Ctx<'_>,
    list_id: Option<Uuid>,
    kind: ListKind,
) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    let (list_id, kind) = match list_id {
        Some(list_id) => {
            let list = lists::get_visible(&mut *ctx.conn, household_id, list_id).await?;
            (Some(list.id), list.kind)
        }
        None => {
            let candidates = lists_of_kind(ctx, kind).await?;
            match candidates.as_slice() {
                [only] => (Some(only.id), kind),
                _ => (None, kind),
            }
        }
    };
    let step = if list_id.is_some() {
        Step::Title
    } else {
        Step::List
    };
    open(
        ctx,
        Form::Create(ItemForm {
            list_id,
            kind,
            step,
            draft: ItemDraft::default(),
            from_preview: false,
            error: None,
            duplicate: None,
        }),
    )
    .await
}

/// Свободный текст без формы: черновик с готовым названием.
pub async fn quick_add(ctx: &mut Ctx<'_>, text: &str) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    if text.chars().count() > MAX_TITLE {
        ctx.banner(format!(
            "Название слишком длинное: не больше {MAX_TITLE} символов."
        ));
        return ctx.present(Screen::Menu).await;
    }
    let all = lists::list(&mut *ctx.conn, household_id).await?;
    let list = all
        .iter()
        .filter(|list| list.kind == ListKind::Tasks && !list.is_archived)
        .find(|list| list.is_default)
        .or_else(|| {
            all.iter()
                .find(|list| list.kind == ListKind::Tasks && !list.is_archived)
        })
        .ok_or_else(|| Fail::User("Нет списка задач для сохранения.".into()))?;

    open(
        ctx,
        Form::Create(ItemForm {
            list_id: Some(list.id),
            kind: ListKind::Tasks,
            step: Step::Preview,
            draft: ItemDraft {
                title: text.to_owned(),
                ..ItemDraft::default()
            },
            from_preview: false,
            error: None,
            duplicate: None,
        }),
    )
    .await
}

/// Открыть шаг редактирования существующего элемента.
pub async fn start_edit(
    ctx: &mut Ctx<'_>,
    item_id: Uuid,
    step: Step,
    back: Screen,
) -> Result<(), Fail> {
    let household_id = ctx.household()?.id;
    // Проверяем, что элемент существует и принадлежит household.
    items::get(&mut *ctx.conn, household_id, item_id).await?;
    open(
        ctx,
        Form::Edit(EditForm {
            item_id,
            step,
            back,
            error: None,
        }),
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

/// Начать онбординг: шаг с названием.
pub async fn start_onboarding(ctx: &mut Ctx<'_>) -> Result<(), Fail> {
    open(ctx, Form::Onboarding { name: None }).await
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
