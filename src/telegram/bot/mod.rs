//! Логика бота: разбор обновлений, навигация и действия.
//!
//! Обработка одного обновления выполняется в транзакции воркера и не вызывает
//! Telegram API: экраны и уведомления попадают в `outbox` вместе с доменными
//! эффектами, поэтому действие и его отображение фиксируются атомарно. Только
//! быстрый ответ на callback (`answerCallbackQuery`) возвращается наружу и
//! отправляется после коммита.

mod capture;
mod card;
mod forms;
pub mod model;
pub mod notify;
mod onboarding;
mod screens;
mod settings;
mod shopping;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use futures_util::future::BoxFuture;
use sqlx::PgConnection;

use crate::domain::error::DomainError;
use crate::domain::identity::{MembershipChange, MembershipNotice, NoticeKind};
use crate::domain::models::{Actor, Household, User};
use crate::domain::recurrence::parse_timezone;
use crate::storage::repositories::{households, users};

use super::callbacks::{self, Consumed, NewToken, Scope};
use super::outbox::{self, Outgoing};
use super::rich::{self, Block, Span};
use super::update::{Incoming, Sender};
use super::views::{self, Lookup};
use model::{Action, Form, Screen, Ui};

/// Часовой пояс, если ни у пользователя, ни у household его нет.
const FALLBACK_TIMEZONE: &str = "Europe/Moscow";

/// Ответ на нажатие кнопки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// Идентификатор callback-запроса.
    pub query_id: String,
    /// Текст всплывающего уведомления.
    pub text: Option<String>,
    /// Показать окно вместо короткой подсказки.
    pub alert: bool,
}

/// Результат обработки обновления.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Ответ на callback-запрос, который нужно отправить после коммита.
    pub answer: Option<Answer>,
}

/// Адресат экрана, показываемого вне обработки его обновления.
#[derive(Debug, Clone, Copy)]
pub struct Recipient<'a> {
    /// Участник.
    pub user: &'a User,
    /// Его household.
    pub household: &'a Household,
    /// Чат, куда выводится экран.
    pub chat_id: i64,
}

/// Бот: настройки, не зависящие от обновления.
#[derive(Debug, Clone)]
pub struct Bot {
    /// Имя бота для ссылок-приглашений.
    pub username: String,
    /// Свободный текст уходит на разбор в LLM, а не в быстрое добавление.
    pub llm_capture: bool,
}

/// Сбой обработки: инфраструктурный (повторить) или объяснимый пользователю.
#[derive(Debug)]
pub enum Fail {
    /// Ошибка базы данных: транзакция откатывается, обновление повторяется.
    Infra(sqlx::Error),
    /// Ожидаемая ситуация: показывается пользователю. Эффекты шага,
    /// завершившегося так, откатываются (см. [`Ctx::in_step`]).
    User(String),
}

impl From<sqlx::Error> for Fail {
    fn from(error: sqlx::Error) -> Self {
        Self::Infra(error)
    }
}

impl From<DomainError> for Fail {
    fn from(error: DomainError) -> Self {
        match error {
            DomainError::Database(error) => Self::Infra(error),
            DomainError::Validation(_) => {
                Self::User("Не получилось сохранить: проверьте введённые данные.".into())
            }
            DomainError::NotFound => Self::User("Не нашёл это — возможно, оно уже удалено.".into()),
            DomainError::Forbidden(_) => Self::User("Недостаточно прав для этого действия.".into()),
            DomainError::Conflict { .. } => {
                Self::User("Данные уже изменились. Показываю актуальное состояние.".into())
            }
        }
    }
}

/// Состояние контекста на начало шага: нужно, чтобы вернуть его при откате.
struct StepGuard {
    user: Option<User>,
    household: Option<Household>,
    tz: Tz,
}

/// Контекст обработки одного обновления.
pub struct Ctx<'a> {
    /// Соединение внутри транзакции воркера.
    pub conn: &'a mut PgConnection,
    /// Момент обработки.
    pub now: DateTime<Utc>,
    /// Имя бота.
    pub bot_username: &'a str,
    /// Чат.
    pub chat_id: i64,
    /// Отправитель.
    pub sender: Sender,
    /// Участник household, если он есть.
    pub user: Option<User>,
    /// Household участника.
    pub household: Option<Household>,
    /// Часовой пояс отображения.
    pub tz: Tz,
    /// Подсказка для ответа на callback.
    pub toast: Option<(String, bool)>,
    /// Сообщение над следующим экраном.
    pub banner: Option<String>,
    /// Свободный текст уходит на разбор в LLM.
    pub llm_capture: bool,
}

impl Ctx<'_> {
    /// Участник; для экранов, недоступных без household.
    pub fn user(&self) -> Result<&User, Fail> {
        self.user
            .as_ref()
            .ok_or_else(|| Fail::User("Сначала создайте пространство: отправьте /start.".into()))
    }

    /// Household участника.
    pub fn household(&self) -> Result<&Household, Fail> {
        self.household
            .as_ref()
            .ok_or_else(|| Fail::User("Сначала создайте пространство: отправьте /start.".into()))
    }

    /// Проверить, что отправитель — владелец household.
    pub fn require_owner(&self) -> Result<(), Fail> {
        if self.actor()?.is_owner() {
            Ok(())
        } else {
            Err(Fail::User("Это доступно только владельцу.".into()))
        }
    }

    /// Актор доменных операций.
    pub fn actor(&self) -> Result<Actor, Fail> {
        let user = self.user()?;
        Ok(Actor::user(user.id, user.role))
    }

    /// Показать короткое уведомление в ответ на нажатие.
    pub fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), false));
    }

    /// Показать уведомление окном.
    pub fn alert(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), true));
    }

    /// Строка над экраном: сообщение об ошибке или результате.
    pub fn banner(&mut self, text: impl Into<String>) {
        self.banner = Some(text.into());
    }

    /// Показать экран: собрать, выпустить токены и поставить в outbox.
    pub async fn present(&mut self, screen: Screen) -> Result<(), Fail> {
        let ui = screens::build(self, &screen).await?;
        self.show(&screen, ui).await
    }

    /// Показать готовый экран. `screen` запоминается для перерисовки.
    pub async fn show(&mut self, screen: &Screen, mut ui: Ui) -> Result<(), Fail> {
        if let Some(banner) = self.banner.take() {
            ui.blocks
                .insert(0, Block::Paragraph(vec![Span::bold(banner)]));
        }
        let html = self.render(&ui, Scope::Screen, self.now).await?;
        let state = to_json(&screen.persisted())?;
        let key = state
            .get("t")
            .and_then(|key| key.as_str())
            .unwrap_or("screen")
            .to_owned();
        views::set(&mut *self.conn, self.chat_id, &key, &state, self.now).await?;
        outbox::enqueue(
            &mut *self.conn,
            self.chat_id,
            &Outgoing::Screen { html },
            None,
            self.now,
        )
        .await?;
        Ok(())
    }

    /// Выпустить токены и собрать HTML.
    async fn render(
        &mut self,
        ui: &Ui,
        scope: Scope,
        issued_at: DateTime<Utc>,
    ) -> Result<String, Fail> {
        let household_id = self.household.as_ref().map(|household| household.id);
        Ok(issue_and_render(
            &mut *self.conn,
            self.sender.id,
            household_id,
            ui,
            scope,
            issued_at,
        )
        .await?)
    }

    /// Поставить обычное сообщение чата (ответ без кнопок).
    pub async fn say(&mut self, text: &str) -> Result<(), Fail> {
        outbox::enqueue(
            &mut *self.conn,
            self.chat_id,
            &Outgoing::Message {
                html: rich::escape(text),
                rich: false,
            },
            None,
            self.now,
        )
        .await?;
        Ok(())
    }

    /// Перерисовать текущий экран чата (или меню).
    pub async fn refresh(&mut self) -> Result<(), Fail> {
        let screen = match views::get(&mut *self.conn, self.chat_id).await? {
            Some(view) => serde_json::from_value::<Screen>(view.state).unwrap_or(Screen::Menu),
            None => Screen::Menu,
        };
        if self.user.is_none() {
            return forms::start_onboarding(self).await;
        }
        self.present(screen).await
    }

    /// Оповестить участников об изменении состава household.
    pub async fn deliver_notices(&mut self, change: &MembershipChange) -> Result<(), Fail> {
        self.deliver_notice_list(&change.notices).await
    }

    /// Оповестить получателей из списка.
    pub async fn deliver_notice_list(&mut self, notices: &[MembershipNotice]) -> Result<(), Fail> {
        for notice in notices {
            let text = match &notice.kind {
                NoticeKind::MemberJoined { display_name } => {
                    format!("К вам присоединился(-ась) {display_name}.")
                }
                NoticeKind::MemberLeft { display_name } => {
                    format!("{display_name} вышел(-ла) из пространства.")
                }
                NoticeKind::AccessRevoked => {
                    "Ваш доступ к семейному пространству прекращён.".to_owned()
                }
                NoticeKind::OwnershipReceived => {
                    "Вам передано владение пространством: теперь вы управляете участниками."
                        .to_owned()
                }
                NoticeKind::HouseholdDeleted { household_name } => {
                    format!("Пространство «{household_name}» удалено владельцем.")
                }
            };
            outbox::enqueue(
                &mut *self.conn,
                notice.chat_id,
                &Outgoing::Message {
                    html: rich::escape(&text),
                    rich: false,
                },
                None,
                self.now,
            )
            .await?;
        }
        Ok(())
    }
}

/// Выпустить токены для кнопок экрана и собрать HTML.
///
/// Токены создаются «задним числом» от `issued_at`, поэтому уведомление,
/// запланированное на завтра, не получит просроченные кнопки.
pub async fn issue_and_render(
    conn: &mut PgConnection,
    telegram_user_id: i64,
    household_id: Option<uuid::Uuid>,
    ui: &Ui,
    scope: Scope,
    issued_at: DateTime<Utc>,
) -> Result<String, sqlx::Error> {
    let specs: Vec<NewToken> = ui
        .actions()
        .into_iter()
        .map(|action| {
            Ok(NewToken {
                telegram_user_id,
                household_id,
                scope,
                action: action.name(),
                payload: to_json(action)?,
                version: action.version(),
            })
        })
        .collect::<Result<_, sqlx::Error>>()?;
    let tokens = callbacks::issue_many(conn, &specs, issued_at).await?;
    Ok(rich::render(ui, &tokens).html)
}

impl Bot {
    /// Создать бота.
    pub fn new(username: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            llm_capture: false,
        }
    }

    /// Показать экран пользователю вне обработки его обновления: так воркер
    /// разбора заменяет «Разбираю…» карточкой черновика. Экран попадает в
    /// `outbox` в транзакции вызывающего; ожидаемые отказы (например, чужой
    /// экран) не считаются сбоем и только пишутся в журнал.
    pub async fn present_to(
        &self,
        conn: &mut PgConnection,
        now: DateTime<Utc>,
        to: Recipient<'_>,
        screen: Screen,
        banner: Option<String>,
    ) -> Result<(), sqlx::Error> {
        let mut ctx = self.context(conn, now, to, banner);
        match ctx.present(screen).await {
            Ok(()) => Ok(()),
            Err(Fail::Infra(error)) => Err(error),
            Err(Fail::User(_)) => {
                tracing::debug!("screen could not be presented to the user");
                Ok(())
            }
        }
    }

    /// Открыть ручную форму с исходным текстом: так заканчивается разбор,
    /// который не удался или недоступен. Форма ставится в `outbox` в
    /// транзакции вызывающего.
    pub async fn present_quick_add(
        &self,
        conn: &mut PgConnection,
        now: DateTime<Utc>,
        to: Recipient<'_>,
        text: &str,
        banner: Option<String>,
    ) -> Result<(), sqlx::Error> {
        let mut ctx = self.context(conn, now, to, banner);
        match forms::quick_add(&mut ctx, text).await {
            Ok(()) => Ok(()),
            Err(Fail::Infra(error)) => Err(error),
            Err(Fail::User(_)) => {
                tracing::debug!("quick add form could not be presented to the user");
                Ok(())
            }
        }
    }

    fn context<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        now: DateTime<Utc>,
        to: Recipient<'_>,
        banner: Option<String>,
    ) -> Ctx<'a> {
        let Recipient {
            user,
            household,
            chat_id,
        } = to;
        Ctx {
            conn,
            now,
            bot_username: &self.username,
            chat_id,
            sender: Sender {
                id: user.telegram_user_id,
                display_name: user.display_name.clone(),
                username: user.username.clone(),
            },
            user: Some(user.clone()),
            household: Some(household.clone()),
            tz: display_timezone(Some(user), Some(household)),
            toast: None,
            banner,
            llm_capture: self.llm_capture,
        }
    }

    /// Включить или выключить разбор свободного текста через LLM.
    #[must_use]
    pub fn with_llm_capture(mut self, enabled: bool) -> Self {
        self.llm_capture = enabled;
        self
    }

    /// Обработать обновление. Вызывать в транзакции; при `Err` её нужно
    /// откатить и повторить обновление позже.
    pub async fn handle(
        &self,
        conn: &mut PgConnection,
        incoming: Incoming,
        now: DateTime<Utc>,
    ) -> Result<Outcome, sqlx::Error> {
        let (chat_id, sender) = match &incoming {
            Incoming::Text {
                chat_id, sender, ..
            }
            | Incoming::Callback {
                chat_id, sender, ..
            } => (*chat_id, sender.clone()),
        };

        let user = users::find_active_by_telegram(&mut *conn, sender.id)
            .await
            .map_err(into_sqlx)?;
        let household = match &user {
            Some(user) => Some(
                households::get(&mut *conn, user.household_id)
                    .await
                    .map_err(into_sqlx)?,
            ),
            None => None,
        };
        let tz = display_timezone(user.as_ref(), household.as_ref());

        let mut ctx = Ctx {
            conn,
            now,
            bot_username: &self.username,
            chat_id,
            sender,
            user,
            household,
            tz,
            toast: None,
            banner: None,
            llm_capture: self.llm_capture,
        };

        let query_id = match &incoming {
            Incoming::Callback { query_id, .. } => Some(query_id.clone()),
            Incoming::Text { .. } => None,
        };
        let result = match incoming {
            Incoming::Text { text, .. } => {
                ctx.in_step(move |ctx| Box::pin(async move { on_text(ctx, &text).await }))
                    .await
            }
            Incoming::Callback {
                message_id, data, ..
            } => on_callback(&mut ctx, message_id, &data).await,
        };

        match result {
            Ok(()) => {}
            Err(Fail::Infra(error)) => return Err(error),
            Err(Fail::User(message)) => recover(&mut ctx, message, query_id.is_some()).await?,
        }

        let answer = query_id.map(|query_id| {
            let (text, alert) = match ctx.toast.take() {
                Some((text, alert)) => (Some(text), alert),
                None => (None, false),
            };
            Answer {
                query_id,
                text,
                alert,
            }
        });
        Ok(Outcome { answer })
    }
}

/// Сериализовать значение для хранения. Ошибка не прячется в `Null`: иначе
/// кнопка или форма молча «пропадали» бы, а сбой не попадал в журнал.
pub(super) fn to_json<T: serde::Serialize>(value: &T) -> Result<serde_json::Value, sqlx::Error> {
    serde_json::to_value(value).map_err(|error| sqlx::Error::Encode(Box::new(error)))
}

pub(super) fn into_sqlx(error: DomainError) -> sqlx::Error {
    match error {
        DomainError::Database(error) => error,
        other => sqlx::Error::Protocol(format!("unexpected domain error: {other}")),
    }
}

/// Часовой пояс отображения: личный, затем пояс household.
pub fn display_timezone(user: Option<&User>, household: Option<&Household>) -> Tz {
    user.and_then(|user| user.timezone.as_deref())
        .or_else(|| household.map(|household| household.timezone.as_str()))
        .and_then(|name| parse_timezone(name).ok())
        .unwrap_or_else(|| parse_timezone(FALLBACK_TIMEZONE).unwrap_or(chrono_tz::UTC))
}

/// Объяснить пользователю ошибку и показать актуальный экран.
async fn recover(ctx: &mut Ctx<'_>, message: String, callback: bool) -> Result<(), sqlx::Error> {
    // Эффекты неудавшегося шага уже откатаны savepoint-ом, транзакция жива.
    if callback {
        ctx.toast = Some((message.clone(), false));
    } else {
        ctx.banner = Some(message);
    }
    match ctx.refresh().await {
        Ok(()) => Ok(()),
        Err(Fail::Infra(error)) => Err(error),
        Err(Fail::User(_)) => Ok(()),
    }
}

async fn on_text(ctx: &mut Ctx<'_>, text: &str) -> Result<(), Fail> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(());
    }

    if let Some((command, argument)) = parse_command(text) {
        return on_command(ctx, &command, argument).await;
    }

    match views::conversation(&mut *ctx.conn, ctx.chat_id, ctx.now).await? {
        Lookup::Active(conversation) if conversation.telegram_user_id == ctx.sender.id => {
            match serde_json::from_value::<Form>(conversation.data) {
                Ok(form) => forms::on_text(ctx, form, text).await,
                Err(_) => {
                    views::clear_conversation(&mut *ctx.conn, ctx.chat_id).await?;
                    ctx.present(Screen::Menu).await
                }
            }
        }
        Lookup::Expired(_) => {
            views::clear_conversation(&mut *ctx.conn, ctx.chat_id).await?;
            ctx.banner("Форма устарела. Начнём заново?");
            ctx.menu_or_onboarding().await
        }
        _ => {
            if ctx.user.is_none() {
                return forms::start_onboarding(ctx).await;
            }
            if ctx.llm_capture {
                capture::intake(ctx, text).await
            } else {
                forms::quick_add(ctx, text).await
            }
        }
    }
}

impl<'a> Ctx<'a> {
    /// Выполнить шаг обработки в savepoint.
    ///
    /// При `Fail::User` записи шага (включая очередь `outbox`) откатываются, а
    /// сведения об участнике в контексте возвращаются к прежним: пользователь
    /// видит объяснение, а не половину действия. Транзакция остаётся рабочей —
    /// в том числе после ошибки на стороне PostgreSQL. При `Fail::Infra`
    /// откатывается вся транзакция воркера, поэтому savepoint не трогается.
    ///
    /// Единственная точка входа: шаг нельзя начать и забыть завершить.
    ///
    /// TODO(тест): сегодня ни одно действие не пишет до `Fail::User`, поэтому
    /// вызовы `in_step` в `handle` и `on_callback` сквозным тестом не покрыты.
    /// Как только появится первое такое действие, добавьте тест через
    /// `Bot::handle`: записи должны откатиться.
    pub async fn in_step<F>(&mut self, run: F) -> Result<(), Fail>
    where
        F: for<'c> FnOnce(&'c mut Ctx<'a>) -> BoxFuture<'c, Result<(), Fail>>,
    {
        sqlx::query("SAVEPOINT bot_step")
            .execute(&mut *self.conn)
            .await?;
        let before = StepGuard {
            user: self.user.clone(),
            household: self.household.clone(),
            tz: self.tz,
        };
        let result = run(self).await;
        match &result {
            Err(Fail::Infra(_)) => {}
            Err(Fail::User(_)) => {
                sqlx::query("ROLLBACK TO SAVEPOINT bot_step")
                    .execute(&mut *self.conn)
                    .await?;
                sqlx::query("RELEASE SAVEPOINT bot_step")
                    .execute(&mut *self.conn)
                    .await?;
                self.user = before.user;
                self.household = before.household;
                self.tz = before.tz;
            }
            Ok(()) => {
                sqlx::query("RELEASE SAVEPOINT bot_step")
                    .execute(&mut *self.conn)
                    .await?;
            }
        }
        result
    }

    /// Открыть меню или предложить онбординг (в отличие от [`Ctx::refresh`],
    /// не возвращается на сохранённый экран).
    async fn menu_or_onboarding(&mut self) -> Result<(), Fail> {
        if self.user.is_none() {
            forms::start_onboarding(self).await
        } else {
            self.present(Screen::Menu).await
        }
    }
}

fn parse_command(text: &str) -> Option<(String, &str)> {
    let rest = text.strip_prefix('/')?;
    let (command, argument) = match rest.split_once(char::is_whitespace) {
        Some((command, argument)) => (command, argument.trim()),
        None => (rest, ""),
    };
    let command = command.split('@').next().unwrap_or(command).to_lowercase();
    Some((command, argument))
}

async fn on_command(ctx: &mut Ctx<'_>, command: &str, argument: &str) -> Result<(), Fail> {
    match command {
        "start" => onboarding::on_start(ctx, argument).await,
        "menu" => {
            views::clear_conversation(&mut *ctx.conn, ctx.chat_id).await?;
            ctx.menu_or_onboarding().await
        }
        "cancel" => {
            views::clear_conversation(&mut *ctx.conn, ctx.chat_id).await?;
            ctx.banner("Действие отменено.");
            ctx.menu_or_onboarding().await
        }
        "settings" => {
            ctx.user()?;
            views::clear_conversation(&mut *ctx.conn, ctx.chat_id).await?;
            ctx.present(Screen::Settings).await
        }
        "help" => {
            ctx.say(HELP).await?;
            Ok(())
        }
        _ => {
            ctx.say("Не знаю такой команды. Отправьте /help, чтобы увидеть список.")
                .await?;
            Ok(())
        }
    }
}

const HELP: &str = "Я помогаю вести задачи и покупки всей семьёй.\n\n\
Просто напишите, что нужно сделать, — я предложу сохранить это как задачу.\n\n\
/menu — главное меню\n\
/settings — настройки\n\
/cancel — отменить текущее действие\n\
/help — эта справка";

async fn on_callback(ctx: &mut Ctx<'_>, message_id: i64, data: &str) -> Result<(), Fail> {
    let consumed = callbacks::consume(&mut *ctx.conn, data, ctx.sender.id, ctx.now).await?;
    let token = match consumed {
        Consumed::Valid(token) => token,
        Consumed::Replayed(scope) => {
            ctx.toast("Это действие уже выполнено.");
            return refresh_unless_notice(ctx, scope).await;
        }
        Consumed::Expired(scope) => {
            ctx.toast("Это действие устарело. Открываю актуальный экран.");
            return refresh_unless_notice(ctx, scope).await;
        }
        Consumed::Foreign => {
            ctx.alert("Эта кнопка предназначена другому пользователю.");
            return Ok(());
        }
        Consumed::Unknown => {
            ctx.toast("Это действие уже неактуально.");
            return ctx.refresh().await;
        }
    };

    // Токен действует только пока пользователь состоит в том же household.
    let current_household = ctx.household.as_ref().map(|household| household.id);
    if token.household_id != current_household {
        ctx.toast("Это действие уже неактуально.");
        return ctx.refresh().await;
    }

    // Кнопка заменённого сообщения экрана не выполняется: пользователь видит
    // актуальный экран в активном сообщении.
    if token.scope == Scope::Screen {
        let active = views::get(&mut *ctx.conn, ctx.chat_id).await?;
        if let Some(active) = active
            && active
                .message_id
                .is_some_and(|active_id| active_id != message_id)
        {
            ctx.toast("Это сообщение устарело. Показываю актуальный экран.");
            return ctx.refresh().await;
        }
    }

    let Ok(action) = serde_json::from_value::<Action>(token.payload) else {
        ctx.toast("Это действие уже неактуально.");
        return ctx.refresh().await;
    };
    // Токен уже потреблён: откат действия его не возвращает.
    ctx.in_step(|ctx| Box::pin(run_action(ctx, message_id, action)))
        .await
}

/// Кнопка уведомления не должна перерисовывать экран навигации.
async fn refresh_unless_notice(ctx: &mut Ctx<'_>, scope: Scope) -> Result<(), Fail> {
    if scope == Scope::Notice {
        Ok(())
    } else {
        ctx.refresh().await
    }
}

async fn run_action(ctx: &mut Ctx<'_>, message_id: i64, action: Action) -> Result<(), Fail> {
    match action {
        Action::Go { to } => {
            if !matches!(to, Screen::Form) {
                views::clear_conversation(&mut *ctx.conn, ctx.chat_id).await?;
            }
            ctx.present(to).await
        }
        Action::Refresh => ctx.refresh().await,

        Action::OnboardZone { zone } => onboarding::choose_zone(ctx, &zone).await,

        Action::Complete {
            item_id,
            version,
            back,
        } => card::complete(ctx, item_id, version, back).await,
        Action::Reopen {
            item_id,
            version,
            back,
        } => card::reopen(ctx, item_id, version, back).await,
        Action::Undo { item_id, back } => card::undo(ctx, item_id, back).await,
        Action::DeleteAsk { item_id, back } => card::delete_ask(ctx, item_id, back).await,
        Action::DeleteDo { item_id, back } => card::delete_do(ctx, item_id, back).await,

        Action::Add { list_id, kind } => forms::start_create(ctx, list_id, kind).await,
        Action::Edit {
            item_id,
            step,
            back,
        } => forms::start_edit(ctx, item_id, step, back).await,
        Action::Form { act } => forms::on_action(ctx, act).await,

        Action::ClearAsk { list_id } => shopping::clear_ask(ctx, list_id).await,
        Action::ClearDo { list_id } => shopping::clear_do(ctx, list_id).await,
        Action::ClearUndo { list_id, item_ids } => {
            shopping::clear_undo(ctx, list_id, &item_ids).await
        }

        Action::SearchStart => forms::start_search(ctx).await,
        Action::RenameStart => forms::start_rename(ctx).await,
        Action::LeadStart => forms::start_lead(ctx).await,
        Action::HouseholdDeleteStart => forms::start_delete_household(ctx).await,

        Action::SetTimezone { zone } => settings::set_timezone(ctx, &zone).await,
        Action::Toggle { setting } => settings::toggle(ctx, setting).await,
        Action::SetLead { minutes } => settings::set_lead(ctx, minutes).await,
        Action::InviteCreate => settings::invite_create(ctx).await,
        Action::InviteRevoke { invite_id } => settings::invite_revoke(ctx, invite_id).await,
        Action::MemberRemoveAsk { user_id } => settings::member_remove_ask(ctx, user_id).await,
        Action::MemberRemove { user_id } => settings::member_remove(ctx, user_id).await,
        Action::TransferAsk { user_id } => settings::transfer_ask(ctx, user_id).await,
        Action::Transfer { user_id } => settings::transfer(ctx, user_id).await,
        Action::LeaveAsk => settings::leave_ask(ctx).await,
        Action::Leave => settings::leave(ctx).await,

        Action::Snooze { item_id, until } => notify::snooze(ctx, message_id, item_id, until).await,
        Action::NoticeComplete { item_id, version } => {
            notify::complete(ctx, message_id, item_id, version).await
        }
        Action::NoticeOpen { item_id } => notify::open(ctx, item_id).await,
        Action::NoticeUndo { item_id } => notify::undo(ctx, message_id, item_id).await,

        Action::DraftEdit { draft_id, index } => capture::edit(ctx, draft_id, index).await,
        Action::DraftAssign {
            draft_id,
            index,
            user_id,
        } => capture::assign(ctx, draft_id, index, user_id).await,
        Action::DraftDue {
            draft_id,
            index,
            choice,
        } => capture::due(ctx, draft_id, index, choice).await,
        Action::DraftEditDue { draft_id, index } => capture::edit_due(ctx, draft_id, index).await,
        Action::DraftRemove { draft_id, index } => capture::remove(ctx, draft_id, index).await,
        Action::DraftSave { draft_id } => capture::save(ctx, draft_id).await,
        Action::DraftCancel { draft_id } => capture::cancel(ctx, draft_id).await,
        Action::DraftUndo { draft_id } => capture::undo(ctx, draft_id).await,
    }
}
