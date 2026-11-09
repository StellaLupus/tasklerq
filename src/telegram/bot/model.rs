//! Модель бота: экраны, действия кнопок и черновик элемента.
//!
//! Экран и действие сериализуются в JSON. Экран хранится в `telegram_views`
//! (по нему перерисовывается устаревшее сообщение), действие — в
//! `callback_tokens` рядом с токеном кнопки.

use std::collections::BTreeSet;

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::capture::draft::DueChoice;
use crate::domain::error::DomainError;
use crate::domain::first_contact::FirstContact;
use crate::domain::models::{
    Field, Item, ItemPatch, ItemPriority, ItemStatus, ListKind, MonthRule, NewItem, Recurrence,
    RecurrenceFreq, RecurrenceInput, SubtaskInput,
};
use crate::domain::recurrence::resolve_local;
use crate::domain::validation::MAX_INPUT_QUANTITY;
use crate::telegram::rich::{Button, View, button};
use crate::telegram::ru;

/// Экран бота с кнопками-действиями.
pub type Ui = View<Action>;

/// Размер страницы списков и результатов.
pub const PAGE_SIZE: usize = 8;

/// Информационный экран первого контакта без формы онбординга.
///
/// `FirstContact::Onboard` сюда не входит: он открывает форму, а не экран.
/// Старое или битое состояние с `onboard` не десериализуется, и `refresh`
/// заменяет его меню.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum FirstContactScreen {
    /// Это частное пространство: попросить приглашение у владельца.
    AskInvite,
    /// Нужно назначить владельца командой CLI с этим Telegram ID.
    AdminCommand(i64),
    /// Инсталляция ещё не настроена, подробностей нет.
    NotConfigured,
    /// В `single` больше одного household: вмешательство администратора.
    NeedsAdmin,
}

impl FirstContactScreen {
    /// Экран для исхода классификатора; `None` — это онбординг.
    pub fn from_contact(contact: FirstContact) -> Option<Self> {
        match contact {
            FirstContact::Onboard => None,
            FirstContact::AskInvite => Some(Self::AskInvite),
            FirstContact::AdminCommand(telegram_id) => Some(Self::AdminCommand(telegram_id)),
            FirstContact::NotConfigured => Some(Self::NotConfigured),
            FirstContact::NeedsAdmin => Some(Self::NeedsAdmin),
        }
    }
}

/// Фильтр статуса в поиске.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusFilter {
    /// Только открытые.
    Open,
    /// Только выполненные.
    Done,
    /// Любые.
    All,
}

/// Экран навигации.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Screen {
    /// Главное меню.
    Menu,
    /// Приветствие после создания пространства.
    Welcome,
    /// Информационный экран первого контакта без формы онбординга.
    FirstContact(FirstContactScreen),
    /// Все списки.
    Lists,
    /// Список задач или покупок.
    List {
        /// Список.
        list_id: Uuid,
        /// Страница с нуля.
        page: u32,
        /// Режим правки покупок: тап по строке открывает карточку. Старое
        /// состояние без поля читается как обычный режим.
        #[serde(default)]
        edit: bool,
        /// Список открыт с экрана «Все списки»: «Назад» ведёт туда, а не в
        /// меню. Старое состояние без поля читается как список из меню.
        #[serde(default)]
        from_lists: bool,
    },
    /// Карточка элемента.
    Item {
        /// Элемент.
        item_id: Uuid,
        /// Куда ведёт «Назад».
        back: Box<Screen>,
        /// Выполнение, из записи журнала которого открыта карточка. Старое
        /// состояние без поля читается как карточка без контекста.
        #[serde(default)]
        completion_id: Option<Uuid>,
    },
    /// Быстрый выбор срока в карточке задачи.
    Due {
        /// Элемент.
        item_id: Uuid,
        /// Экран за карточкой: туда ведёт «Назад» и туда же возвращает
        /// сохранение срока.
        back: Box<Screen>,
    },
    /// Единый редактор элемента или открытый экран его поля.
    Editor {
        /// Элемент; у создания его ещё нет.
        item_id: Option<Uuid>,
        /// Куда ведёт «✖ Отмена» и куда возвращает сохранение.
        back: Box<Screen>,
    },
    /// Результат удаления с кнопкой отмены.
    Deleted {
        /// Элемент.
        item_id: Uuid,
        /// Название на момент удаления.
        title: String,
        /// Куда ведёт «Назад».
        back: Box<Screen>,
    },
    /// Подтверждение удаления.
    ConfirmDelete {
        /// Элемент.
        item_id: Uuid,
        /// Куда вернуться.
        back: Box<Screen>,
    },
    /// Подтверждение очистки выполненных покупок.
    ConfirmClear {
        /// Список.
        list_id: Uuid,
        /// Список открыт с экрана «Все списки». Старое состояние без поля
        /// читается как список из меню.
        #[serde(default)]
        from_lists: bool,
    },
    /// Результат очистки с кнопкой отмены.
    Cleared {
        /// Список.
        list_id: Uuid,
        /// Удалённые позиции.
        item_ids: Vec<Uuid>,
        /// Список открыт с экрана «Все списки». Старое состояние без поля
        /// читается как список из меню.
        #[serde(default)]
        from_lists: bool,
    },
    /// Подтверждение удаления участника.
    ConfirmRemove {
        /// Участник.
        user_id: Uuid,
    },
    /// Подтверждение передачи владения.
    ConfirmTransfer {
        /// Новый владелец.
        user_id: Uuid,
    },
    /// Подтверждение отзыва API-токена.
    ConfirmRevokeToken {
        /// Токен.
        token_id: Uuid,
    },
    /// Подтверждение выхода.
    ConfirmLeave,
    /// Ссылка на только что созданное приглашение.
    InviteCreated {
        /// Приглашение.
        invite_id: Uuid,
        /// Открытый код; в `telegram_views` не должен храниться дольше экрана.
        code: String,
    },
    /// Мои задачи.
    Mine {
        /// Страница.
        page: u32,
    },
    /// Журнал выполнений household, открытый из списка задач.
    History {
        /// Страница с нуля.
        page: u32,
        /// Список задач, из которого открыт журнал: туда ведёт «Назад».
        back: Box<Screen>,
    },
    /// Результаты поиска.
    Results {
        /// Запрос.
        query: String,
        /// Фильтр статуса.
        status: StatusFilter,
        /// Страница.
        page: u32,
    },
    /// Настройки.
    Settings,
    /// Профиль пользователя: имя и часовой пояс.
    Profile,
    /// Опасные действия: удаление пространства или выход.
    Danger,
    /// Настройки уведомлений.
    Notifications,
    /// Выбор за сколько напоминать о сроке.
    LeadTime,
    /// Выбор часового пояса.
    Timezones,
    /// Участники household.
    Family,
    /// Активные API-токены household.
    Integrations {
        /// Страница с нуля.
        page: u32,
    },
    /// Карточка участника.
    Member {
        /// Участник.
        user_id: Uuid,
    },
    /// Активные приглашения.
    Invites,
    /// Журнал членства.
    Journal {
        /// Страница.
        page: u32,
    },
    /// Текущий шаг формы из `conversation_states`.
    Form,
    /// Текст принят и ждёт разбора в фоне.
    Processing {
        /// Задание разбора; у старого состояния до выкатки его нет.
        #[serde(default)]
        job_id: Option<Uuid>,
    },
    /// Черновик capture: карточка, результат или причина, по которой он закрыт.
    Draft {
        /// Черновик.
        draft_id: Uuid,
    },
}

impl Screen {
    /// Экран, который запоминается для перерисовки. Одноразовые данные
    /// (например, код приглашения) в базе не сохраняются.
    pub fn persisted(&self) -> Self {
        match self {
            Self::InviteCreated { .. } => Self::Invites,
            other => other.clone(),
        }
    }

    /// Глубина экрана от главного меню: меню — 1, его прямые разделы — 2.
    pub fn depth(&self) -> usize {
        match self {
            Self::Menu | Self::Welcome | Self::FirstContact(_) => 1,
            Self::Lists
            | Self::Mine { .. }
            | Self::Results { .. }
            | Self::Settings
            | Self::Form
            | Self::Processing { .. }
            | Self::Draft { .. } => 2,
            Self::List {
                from_lists: true, ..
            } => 3,
            Self::List { .. } => 2,
            Self::Item { back, .. }
            | Self::History { back, .. }
            | Self::Deleted { back, .. }
            | Self::ConfirmDelete { back, .. } => back.depth() + 1,
            Self::Editor { back, .. } => back.depth() + 1,
            Self::Due { back, .. } => back.depth() + 2,
            Self::ConfirmClear {
                from_lists: true, ..
            }
            | Self::Cleared {
                from_lists: true, ..
            } => 4,
            Self::ConfirmClear { .. } | Self::Cleared { .. } => 3,
            Self::Profile
            | Self::Danger
            | Self::Notifications
            | Self::Family
            | Self::Integrations { .. }
            | Self::Journal { .. } => 3,
            Self::LeadTime | Self::Timezones | Self::ConfirmLeave => 4,
            Self::Member { .. } | Self::Invites | Self::ConfirmRevokeToken { .. } => 4,
            Self::ConfirmRemove { .. }
            | Self::ConfirmTransfer { .. }
            | Self::InviteCreated { .. } => 5,
        }
    }

    /// Короткое имя экрана для хлебных крошек.
    fn crumb(&self) -> &'static str {
        match self {
            Self::Menu => "Меню",
            Self::Welcome => "Пространство создано",
            Self::FirstContact(_) => "Первый контакт",
            Self::Lists => "Все списки",
            Self::List { .. } => "Список",
            Self::Item { .. } => "Карточка",
            Self::Due { .. } => "Срок",
            Self::Editor { .. } => "Редактор",
            Self::Deleted { .. } => "Удалено",
            Self::ConfirmDelete { .. } => "Удаление",
            Self::ConfirmClear { .. } => "Очистка",
            Self::Cleared { .. } => "Очищено",
            Self::ConfirmRemove { .. } => "Удаление участника",
            Self::ConfirmTransfer { .. } => "Передача владения",
            Self::ConfirmRevokeToken { .. } => "Отзыв токена",
            Self::ConfirmLeave => "Выход",
            Self::InviteCreated { .. } => "Приглашение создано",
            Self::Mine { .. } => "Мои задачи",
            Self::History { .. } => "История",
            Self::Results { .. } => "Поиск",
            Self::Settings => "Настройки",
            Self::Profile => "Профиль",
            Self::Danger => "Опасное",
            Self::Notifications => "Уведомления",
            Self::LeadTime => "Время напоминания",
            Self::Timezones => "Часовой пояс",
            Self::Family => "Семья",
            Self::Integrations { .. } => "Интеграции",
            Self::Member { .. } => "Участник",
            Self::Invites => "Приглашения",
            Self::Journal { .. } => "Журнал",
            Self::Form => "Форма",
            Self::Processing { .. } => "Разбираю сообщение…",
            Self::Draft { .. } => "Черновик",
        }
    }

    /// Короткие имена родителей от меню, без текущего экрана.
    fn parents(&self) -> Vec<&'static str> {
        match self {
            Self::Menu | Self::Welcome | Self::FirstContact(_) => Vec::new(),
            Self::Lists
            | Self::Mine { .. }
            | Self::Results { .. }
            | Self::Settings
            | Self::Form
            | Self::Processing { .. }
            | Self::Draft { .. } => Vec::new(),
            Self::List {
                from_lists: true, ..
            } => vec!["Все списки"],
            Self::List { .. } => Vec::new(),
            Self::Item { back, .. }
            | Self::History { back, .. }
            | Self::Deleted { back, .. }
            | Self::ConfirmDelete { back, .. } => {
                let mut parents = back.parents();
                parents.push(back.crumb());
                parents
            }
            Self::Editor { back, .. } => {
                let mut parents = back.parents();
                parents.push(back.crumb());
                parents
            }
            Self::Due { back, .. } => {
                let mut parents = back.parents();
                parents.push(back.crumb());
                parents.push("Карточка");
                parents
            }
            Self::ConfirmClear {
                from_lists: true, ..
            }
            | Self::Cleared {
                from_lists: true, ..
            } => vec!["Все списки", "Список"],
            Self::ConfirmClear { .. } | Self::Cleared { .. } => vec!["Список"],
            Self::Notifications
            | Self::Family
            | Self::Integrations { .. }
            | Self::Journal { .. }
            | Self::Profile
            | Self::Danger => vec!["Настройки"],
            Self::LeadTime => vec!["Настройки", "Уведомления"],
            Self::Timezones => vec!["Настройки", "Профиль"],
            Self::ConfirmLeave => vec!["Настройки", "Опасное"],
            Self::Member { .. } | Self::Invites => vec!["Настройки", "Семья"],
            Self::ConfirmRemove { .. } | Self::ConfirmTransfer { .. } => {
                vec!["Настройки", "Семья", "Участник"]
            }
            Self::ConfirmRevokeToken { .. } => vec!["Настройки", "Интеграции"],
            Self::InviteCreated { .. } => vec!["Настройки", "Семья", "Приглашения"],
        }
    }

    /// Хлебные крошки экрана: имена родителей и `title` через « › », не
    /// длиннее 60 символов. `parent` подменяет имя списка-родителя, если оно
    /// зависит от данных.
    pub fn trail(&self, title: &str, parent: Option<&str>) -> String {
        self.trail_with_names(title, parent, None, None)
    }

    /// Крошки по фактической цепочке `back` с настоящими именами: вместо
    /// служебного «Список» подставляется имя списка, вместо «Карточка» —
    /// название элемента, а `extra` добавляет служебный экран между последним
    /// родителем и заголовком (например, «Редактор»).
    pub fn trail_with_names(
        &self,
        title: &str,
        list_name: Option<&str>,
        card_title: Option<&str>,
        extra: Option<&str>,
    ) -> String {
        let deep = self.depth() > 2;
        let mut parts: Vec<String> = if deep {
            self.parents().into_iter().map(str::to_owned).collect()
        } else {
            Vec::new()
        };
        for part in &mut parts {
            let replacement = match part.as_str() {
                "Список" => list_name,
                "Карточка" => card_title,
                _ => None,
            };
            if let Some(replacement) = replacement {
                *part = replacement.to_owned();
            }
        }
        if deep && let Some(extra) = extra {
            parts.push(extra.to_owned());
        }
        parts.push(title.to_owned());
        clip_trail(parts.join(" › "))
    }
}

/// Заголовок экрана с хлебными крошками. Если крошки обрезали `title`, полный
/// текст показывается отдельным абзацем: получатель и действие должны
/// оставаться видимыми.
pub fn screen_heading(screen: &Screen, title: &str, parent: Option<&str>) -> Ui {
    screen_heading_names(screen, title, parent, None, None)
}

/// Заголовок экрана с крошками по цепочке `back` и настоящими именами
/// списка и карточки.
pub fn screen_heading_names(
    screen: &Screen,
    title: &str,
    list_name: Option<&str>,
    card_title: Option<&str>,
    extra: Option<&str>,
) -> Ui {
    let trail = screen.trail_with_names(title, list_name, card_title, extra);
    let mut ui = Ui::new().heading(trail.clone());
    if !trail.contains(title) {
        ui = ui.text(title.to_owned());
    }
    ui
}

/// Обрезать хлебные крошки до 60 символов.
fn clip_trail(text: String) -> String {
    const MAX: usize = 60;
    if text.chars().count() <= MAX {
        return text;
    }
    let mut clipped: String = text.chars().take(MAX.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

/// Шаг формы элемента.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Выбор списка.
    List,
    /// Название.
    Title,
    /// Срок.
    Due,
    /// Ввод даты текстом.
    DueText,
    /// Приоритет.
    Priority,
    /// Исполнитель.
    Assignee,
    /// Теги.
    Tags,
    /// Ввод нового тега.
    NewTag,
    /// Подзадачи.
    Subtasks,
    /// Повторение.
    Recurrence,
    /// Дни недели повторения.
    Weekdays,
    /// Правило месяца.
    MonthRule,
    /// Интервал повторения.
    Interval,
    /// Количество.
    Quantity,
    /// Единица измерения.
    Unit,
    /// Заметка.
    Notes,
}

impl Step {
    /// Шаг ждёт текст от пользователя.
    pub const fn expects_text(self) -> bool {
        matches!(
            self,
            Self::Title
                | Self::DueText
                | Self::NewTag
                | Self::Subtasks
                | Self::Interval
                | Self::Quantity
                | Self::Unit
                | Self::Notes
        )
    }
}

/// Поле редактора, которое пользователь уже менял.
///
/// Связанные экраны одного поля (`Due` и `DueText`, `Recurrence`, `Weekdays`,
/// `MonthRule` и `Interval`) дают одно и то же значение, поэтому в `touched`
/// попадает именно поле, а не открытый экран.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditorField {
    /// Название.
    Title,
    /// Срок.
    Due,
    /// Приоритет.
    Priority,
    /// Исполнитель.
    Assignee,
    /// Теги.
    Tags,
    /// Подзадачи.
    Subtasks,
    /// Повторение.
    Recurrence,
    /// Описание или заметка.
    Notes,
    /// Список.
    List,
    /// Количество.
    Quantity,
    /// Единица измерения.
    Unit,
}

impl EditorField {
    /// Поле, к которому относится открытый экран шага.
    pub const fn from_step(step: Step) -> Option<Self> {
        match step {
            Step::Title => Some(Self::Title),
            Step::Due | Step::DueText => Some(Self::Due),
            Step::Priority => Some(Self::Priority),
            Step::Assignee => Some(Self::Assignee),
            Step::Tags | Step::NewTag => Some(Self::Tags),
            Step::Subtasks => Some(Self::Subtasks),
            Step::Recurrence | Step::Weekdays | Step::MonthRule | Step::Interval => {
                Some(Self::Recurrence)
            }
            Step::Notes => Some(Self::Notes),
            Step::List => Some(Self::List),
            Step::Quantity => Some(Self::Quantity),
            Step::Unit => Some(Self::Unit),
        }
    }

    /// Шаг, который строит патч только для этого поля.
    pub const fn step(self) -> Step {
        match self {
            Self::Title => Step::Title,
            Self::Due => Step::Due,
            Self::Priority => Step::Priority,
            Self::Assignee => Step::Assignee,
            Self::Tags => Step::Tags,
            Self::Subtasks => Step::Subtasks,
            Self::Recurrence => Step::Recurrence,
            Self::Notes => Step::Notes,
            Self::List => Step::List,
            Self::Quantity => Step::Quantity,
            Self::Unit => Step::Unit,
        }
    }
}

/// Снимок значений элемента при открытии редактора.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemSnapshot {
    /// Список.
    pub list_id: Uuid,
    /// Название.
    pub title: String,
    /// Описание.
    pub notes: Option<String>,
    /// Срок со временем.
    pub due_at: Option<DateTime<Utc>>,
    /// Вседневный срок.
    pub due_date: Option<NaiveDate>,
    /// Приоритет.
    pub priority: ItemPriority,
    /// Исполнитель.
    pub assignee: Option<Uuid>,
    /// Теги.
    pub tags: Vec<String>,
    /// Подзадачи.
    pub subtasks: Vec<SubtaskDraft>,
    /// Правило повторения.
    pub recurrence: Option<Recurrence>,
    /// Количество.
    pub quantity: Option<f64>,
    /// Единица измерения.
    pub unit: Option<String>,
}

impl ItemSnapshot {
    /// Снять значения с сохранённого элемента.
    pub fn from_item(item: &Item) -> Self {
        Self {
            list_id: item.list_id,
            title: item.title.clone(),
            notes: item.notes.clone(),
            due_at: item.due_at,
            due_date: item.due_date,
            priority: item.priority,
            assignee: item.assignee_user_id,
            tags: item.tags.iter().map(|tag| tag.name.clone()).collect(),
            subtasks: item
                .subtasks
                .iter()
                .map(|subtask| SubtaskDraft {
                    title: subtask.title.clone(),
                    done: subtask.status == ItemStatus::Done,
                })
                .collect(),
            recurrence: item.recurrence.clone(),
            quantity: item.quantity,
            unit: item.unit.clone(),
        }
    }
}

/// Что правит единый редактор.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum EditorTarget {
    /// Новый элемент: поля ещё не записаны.
    New {
        /// Вид списка на момент открытия.
        kind: ListKind,
        /// Куда вернуться после сохранения или отмены.
        back: Screen,
    },
    /// Существующий элемент.
    Item {
        /// Элемент.
        item_id: Uuid,
        /// Значения полей при открытии редактора.
        base: Box<ItemSnapshot>,
        /// Куда вернуться после сохранения или отмены.
        back: Screen,
    },
    /// Пункт черновика LLM: правится в черновике, а не в элементе.
    DraftItem {
        /// Черновик.
        draft_id: Uuid,
        /// Номер элемента с нуля.
        index: usize,
        /// Вид списка пункта на момент открытия.
        kind: ListKind,
        /// У пункта не было выбранного списка: перед сохранением его нужно
        /// выбрать.
        #[serde(default)]
        list_missing: bool,
    },
}

/// Выбор на кнопке внутри формы.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "c", content = "v", rename_all = "snake_case")]
pub enum Choice {
    /// Выбрать список для нового элемента.
    List(Uuid),
    /// Срок «сегодня».
    DueToday,
    /// Срок «завтра».
    DueTomorrow,
    /// Без срока.
    DueNone,
    /// Приоритет.
    Priority(ItemPriority),
    /// Исполнитель; `None` снимает.
    Assignee(Option<Uuid>),
    /// Включить или выключить тег.
    TagToggle(String),
    /// Отметить подзадачу.
    SubtaskToggle(usize),
    /// Удалить подзадачу.
    SubtaskRemove(usize),
    /// Частота повторения; `None` отключает.
    Freq(Option<RecurrenceFreq>),
    /// День недели повторения.
    Weekday(i16),
    /// Правило месяца.
    MonthRule(MonthRule),
    /// Интервал.
    Interval(i16),
    /// Количество.
    Quantity(f64),
    /// Единица измерения.
    Unit(String),
}

/// Управление формой.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "f", content = "v", rename_all = "snake_case")]
pub enum FormAct {
    /// Выбор значения.
    Choose(Choice),
    /// Перейти к шагу.
    Go(Step),
    /// Закончить многошаговый выбор (теги, подзадачи, дни).
    Done,
    /// Вернуться в редактор с экрана поля без изменения поля.
    Back,
    /// Разрешить конфликт сохранения: «Оставить мой» или «Оставить текущий».
    Conflict {
        /// Оставить значения из редактора.
        mine: bool,
    },
    /// Сохранить.
    Save,
    /// Сохранить, даже если такая позиция уже есть.
    SaveAnyway,
    /// Увеличить количество существующей позиции.
    Increase {
        /// Существующая позиция.
        item_id: Uuid,
        /// Новое количество.
        quantity: f64,
        /// Версия существующей позиции.
        version: i32,
    },
    /// Отменить форму.
    Cancel,
}

/// Быстрый срок на экране карточки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuickDue {
    /// Сегодняшняя дата.
    Today,
    /// Завтрашняя дата.
    Tomorrow,
    /// Дата текущего срока не раньше сегодня плюс неделя.
    WeekLater,
    /// Снять срок.
    None,
}

/// Действие кнопки.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "a", rename_all = "snake_case")]
pub enum Action {
    /// Перейти на экран.
    Go {
        /// Экран.
        to: Screen,
    },
    /// Завершить элемент.
    Complete {
        /// Элемент.
        item_id: Uuid,
        /// Версия, которую видел пользователь.
        version: i32,
        /// Куда вернуться.
        back: Screen,
    },
    /// Вернуть элемент в работу.
    Reopen {
        /// Элемент.
        item_id: Uuid,
        /// Версия.
        version: i32,
        /// Куда вернуться.
        back: Screen,
        /// Выполнение, которое возвращается. Без значения возвращается
        /// последнее выполнение элемента, а если его нет — прежний `reopen`.
        #[serde(default)]
        completion_id: Option<Uuid>,
    },
    /// Отменить последнее действие над элементом.
    Undo {
        /// Элемент.
        item_id: Uuid,
        /// Куда вернуться.
        back: Screen,
    },
    /// Запросить подтверждение удаления.
    DeleteAsk {
        /// Элемент.
        item_id: Uuid,
        /// Куда вернуться.
        back: Screen,
    },
    /// Удалить элемент.
    DeleteDo {
        /// Элемент.
        item_id: Uuid,
        /// Куда вернуться.
        back: Screen,
    },
    /// Отменить удаление элемента.
    DeleteUndo {
        /// Элемент.
        item_id: Uuid,
        /// Куда вернуться.
        back: Screen,
    },
    /// Начать создание элемента через редактор.
    Add {
        /// Список; без значения бот возьмёт список по умолчанию.
        list_id: Option<Uuid>,
        /// Вид списка для выбора.
        kind: ListKind,
        /// Экран, откуда начали создание.
        #[serde(default)]
        back: Option<Screen>,
    },
    /// Открыть единый редактор существующего элемента.
    Edit {
        /// Элемент.
        item_id: Uuid,
        /// Куда вернуться после сохранения или отмены.
        back: Screen,
    },
    /// Выбрать быстрый срок в карточке.
    DueSet {
        /// Элемент.
        item_id: Uuid,
        /// Версия, которую видел пользователь.
        version: i32,
        /// Выбранный срок.
        due: QuickDue,
        /// Экран за карточкой.
        back: Screen,
    },
    /// Ввести дату и время текстом из быстрого срока.
    DueText {
        /// Элемент.
        item_id: Uuid,
        /// Версия, которую видел пользователь.
        version: i32,
        /// Экран за карточкой.
        back: Screen,
    },
    /// Действие внутри формы элемента.
    Form {
        /// Что сделать.
        act: FormAct,
    },
    /// Запросить очистку выполненных.
    ClearAsk {
        /// Список.
        list_id: Uuid,
        /// Список открыт с экрана «Все списки». Старый токен без поля
        /// читается как список из меню.
        #[serde(default)]
        from_lists: bool,
    },
    /// Очистить выполненные.
    ClearDo {
        /// Список.
        list_id: Uuid,
        /// Список открыт с экрана «Все списки». Старый токен без поля
        /// читается как список из меню.
        #[serde(default)]
        from_lists: bool,
    },
    /// Отменить очистку выполненных.
    ClearUndo {
        /// Список.
        list_id: Uuid,
        /// Удалённые позиции.
        item_ids: Vec<Uuid>,
        /// Список открыт с экрана «Все списки». Старый токен без поля
        /// читается как список из меню.
        #[serde(default)]
        from_lists: bool,
    },
    /// Увеличить найденный при прямом добавлении дубликат до суммы.
    DuplicateIncrease {
        /// Существующая позиция.
        item_id: Uuid,
        /// Версия, которую видел пользователь.
        version: i32,
        /// Новое количество.
        quantity: f64,
        /// Остальные дубликаты, которые ещё ждут выбора.
        #[serde(default)]
        rest: Vec<DuplicateChoice>,
        /// Ожидающий черновик этого же добавления: кнопка «Открыть» в
        /// следующем баннере.
        #[serde(default)]
        draft_id: Option<Uuid>,
        /// Позиции, созданные этим добавлением: «↩️ Отменить» в следующем
        /// баннере. Старый токен без поля читается пустым.
        #[serde(default)]
        created_ids: Vec<Uuid>,
    },
    /// Отменить прямое добавление покупок: мягко удалить созданные позиции.
    AddedUndo {
        /// Позиции, созданные этим добавлением.
        item_ids: Vec<Uuid>,
        /// Ожидающий черновик этого же добавления: кнопка «Открыть» в
        /// баннере. Старый токен без поля читается как `None`.
        #[serde(default)]
        draft_id: Option<Uuid>,
    },
    /// Открыть ручной редактор с исходным текстом из баннера ошибки разбора.
    ManualEntry {
        /// Исходный текст сообщения (не длиннее 2000 символов).
        text: String,
    },
    /// Добавить найденный дубликат отдельной позицией.
    DuplicateSeparate {
        /// Список.
        list_id: Uuid,
        /// Название.
        title: String,
        /// Количество.
        quantity: f64,
        /// Единица.
        unit: Option<String>,
        /// Остальные дубликаты, которые ещё ждут выбора.
        #[serde(default)]
        rest: Vec<DuplicateChoice>,
        /// Ожидающий черновик этого же добавления: кнопка «Открыть» в
        /// следующем баннере.
        #[serde(default)]
        draft_id: Option<Uuid>,
        /// Позиции, созданные этим добавлением: «↩️ Отменить» в следующем
        /// баннере. Старый токен без поля читается пустым.
        #[serde(default)]
        created_ids: Vec<Uuid>,
    },
    /// Начать ввод поискового запроса.
    SearchStart,
    /// Открыть форму переименования профиля.
    RenameStart,
    /// Открыть форму ввода времени напоминания.
    LeadStart,
    /// Открыть форму ввода времени настройки уведомлений.
    TimeStart {
        /// Какое время редактируется.
        field: TimeField,
    },
    /// Выбрать часовой пояс пользователя.
    SetTimezone {
        /// IANA-идентификатор.
        zone: String,
    },
    /// Переключить настройку уведомлений.
    Toggle {
        /// Настройка.
        setting: Setting,
    },
    /// Выбрать время напоминания.
    SetLead {
        /// Минут до срока.
        minutes: u16,
    },
    /// Создать приглашение.
    InviteCreate,
    /// Отозвать приглашение.
    InviteRevoke {
        /// Приглашение.
        invite_id: Uuid,
    },
    /// Отозвать API-токен.
    RevokeToken {
        /// Токен.
        token_id: Uuid,
    },
    /// Выпустить токен для Home Assistant.
    IssueHaToken,
    /// Удалить сообщение с одноразовым секретом.
    DismissSecret,
    /// Подтвердить удаление участника.
    MemberRemoveAsk {
        /// Участник.
        user_id: Uuid,
    },
    /// Удалить участника.
    MemberRemove {
        /// Участник.
        user_id: Uuid,
    },
    /// Подтвердить передачу владения.
    TransferAsk {
        /// Новый владелец.
        user_id: Uuid,
    },
    /// Передать владение.
    Transfer {
        /// Новый владелец.
        user_id: Uuid,
    },
    /// Подтвердить выход.
    LeaveAsk,
    /// Выйти из household.
    Leave,
    /// Начать удаление household.
    HouseholdDeleteStart,
    /// Выбрать часовой пояс при онбординге.
    OnboardZone {
        /// IANA-идентификатор.
        zone: String,
    },
    /// Отложить напоминание.
    Snooze {
        /// Элемент.
        item_id: Uuid,
        /// Вариант.
        until: SnoozeUntil,
        /// Версия на момент выпуска; старый токен без поля её не проверяет.
        #[serde(default)]
        version: Option<i32>,
    },
    /// Завершить задачу из уведомления.
    NoticeComplete {
        /// Элемент.
        item_id: Uuid,
        /// Версия.
        version: i32,
    },
    /// Открыть задачу из уведомления.
    NoticeOpen {
        /// Элемент.
        item_id: Uuid,
    },
    /// Отменить завершение из уведомления.
    NoticeUndo {
        /// Элемент.
        item_id: Uuid,
    },
    /// Перелистнуть сводное сообщение после тихих часов.
    BatchPage {
        /// Сводное сообщение.
        batch_id: Uuid,
        /// Страница с нуля.
        page: u32,
    },
    /// Завершить задачу из блока сводного сообщения.
    BatchComplete {
        /// Сводное сообщение.
        batch_id: Uuid,
        /// Элемент.
        item_id: Uuid,
        /// Версия на момент выпуска.
        version: i32,
    },
    /// Отложить задачу из блока сводного сообщения на час.
    BatchSnooze {
        /// Сводное сообщение.
        batch_id: Uuid,
        /// Элемент.
        item_id: Uuid,
        /// Версия на момент выпуска; старый токен без поля её не проверяет.
        #[serde(default)]
        version: Option<i32>,
    },
    /// Открыть задачу из блока сводного сообщения.
    BatchOpen {
        /// Сводное сообщение.
        batch_id: Uuid,
        /// Элемент.
        item_id: Uuid,
    },
    /// Завершить все ещё открытые задачи сводного сообщения.
    ///
    /// Входят и блоки, уже отложенные кнопкой «На час»: задача выполнена, а её
    /// snooze-доставка отменяется планировщиком как устаревшая. Блоки
    /// «Выполнено», «Задача удалена» и «Список в архиве» не трогаются.
    BatchCompleteAll {
        /// Сводное сообщение.
        batch_id: Uuid,
    },
    /// Отложить все ещё открытые задачи сводного сообщения на час.
    ///
    /// Блоки, у которых уже есть свой snooze, не откладываются повторно.
    BatchSnoozeAll {
        /// Сводное сообщение.
        batch_id: Uuid,
    },
    /// Обновить текущий экран.
    Refresh,
    /// Исправить элемент черновика в форме.
    DraftEdit {
        /// Черновик.
        draft_id: Uuid,
        /// Номер элемента с нуля.
        index: usize,
    },
    /// Убрать элемент из черновика.
    DraftRemove {
        /// Черновик.
        draft_id: Uuid,
        /// Номер элемента с нуля.
        index: usize,
    },
    /// Выбрать исполнителя в ответ на уточняющий вопрос.
    DraftAssign {
        /// Черновик.
        draft_id: Uuid,
        /// Номер элемента с нуля.
        index: usize,
        /// Выбранный участник.
        user_id: Uuid,
    },
    /// Выбрать «сегодня» или «завтра» в ответ на вопрос о дате.
    DraftDue {
        /// Черновик.
        draft_id: Uuid,
        /// Номер элемента с нуля.
        index: usize,
        /// Какой день выбрать.
        choice: DueChoice,
    },
    /// Открыть редактор правки сразу на поле срока.
    DraftEditDue {
        /// Черновик.
        draft_id: Uuid,
        /// Номер элемента с нуля.
        index: usize,
    },
    /// Сохранить все элементы черновика.
    DraftSave {
        /// Черновик.
        draft_id: Uuid,
    },
    /// Отказаться от черновика.
    DraftCancel {
        /// Черновик.
        draft_id: Uuid,
    },
    /// Отменить сохранённый capture целиком.
    DraftUndo {
        /// Черновик.
        draft_id: Uuid,
    },
}

/// Настройка, которую можно переключить.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Setting {
    /// Напоминания.
    Reminders,
    /// Утренняя сводка.
    Digest,
    /// Уведомления о назначении.
    Assignment,
    /// Тихие часы.
    QuietHours,
}

/// Время настройки уведомлений, которое вводится текстом.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeField {
    /// Время утренней сводки.
    Digest,
    /// Время вседневного напоминания.
    AllDay,
    /// Начало тихих часов.
    QuietStart,
    /// Конец тихих часов.
    QuietEnd,
}

/// На сколько отложить напоминание.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnoozeUntil {
    /// На час.
    OneHour,
    /// До завтра, 09:00.
    TomorrowMorning,
}

impl Action {
    /// Имя действия для диагностики в `callback_tokens.action`.
    pub fn name(&self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|value| {
                value
                    .get("a")
                    .and_then(|name| name.as_str().map(str::to_owned))
            })
            .unwrap_or_else(|| "unknown".to_owned())
    }

    /// Версия ресурса, на которую рассчитано действие.
    pub fn version(&self) -> Option<i32> {
        match self {
            Self::Complete { version, .. }
            | Self::Reopen { version, .. }
            | Self::DueSet { version, .. }
            | Self::DueText { version, .. }
            | Self::NoticeComplete { version, .. }
            | Self::BatchComplete { version, .. }
            | Self::DuplicateIncrease { version, .. } => Some(*version),
            Self::Snooze { version, .. } | Self::BatchSnooze { version, .. } => *version,
            Self::Form {
                act: FormAct::Increase { version, .. },
            } => Some(*version),
            _ => None,
        }
    }
}

/// Подзадача в черновике.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubtaskDraft {
    /// Название.
    pub title: String,
    /// Выполнена.
    pub done: bool,
}

/// Повторение в черновике.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecurrenceDraft {
    /// Частота.
    pub freq: RecurrenceFreq,
    /// Интервал 1..365.
    pub interval: i16,
    /// Дни недели ISO для weekly.
    pub weekdays: Vec<i16>,
    /// Правило месяца для monthly.
    pub month_rule: Option<MonthRule>,
}

/// Черновик элемента, который заполняет форма.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ItemDraft {
    /// Название.
    pub title: String,
    /// Заметка.
    pub notes: Option<String>,
    /// Дата срока.
    pub date: Option<NaiveDate>,
    /// Время срока; без даты не имеет смысла.
    pub time: Option<NaiveTime>,
    /// Приоритет.
    pub priority: ItemPriority,
    /// Исполнитель.
    pub assignee: Option<Uuid>,
    /// Теги.
    pub tags: Vec<String>,
    /// Подзадачи.
    pub subtasks: Vec<SubtaskDraft>,
    /// Повторение.
    pub recurrence: Option<RecurrenceDraft>,
    /// Количество.
    pub quantity: Option<f64>,
    /// Единица измерения.
    pub unit: Option<String>,
}

impl ItemDraft {
    /// Черновик по существующему элементу.
    pub fn from_item(item: &Item, timezone: Tz) -> Self {
        Self::from_snapshot(&ItemSnapshot::from_item(item), timezone)
    }

    /// Черновик по снимку значений элемента.
    pub fn from_snapshot(snapshot: &ItemSnapshot, timezone: Tz) -> Self {
        let (date, time) = match (snapshot.due_at, snapshot.due_date) {
            (Some(at), _) => {
                let local = at.with_timezone(&timezone);
                (Some(local.date_naive()), Some(local.time()))
            }
            (None, date) => (date, None),
        };
        Self {
            title: snapshot.title.clone(),
            notes: snapshot.notes.clone(),
            date,
            time,
            priority: snapshot.priority,
            assignee: snapshot.assignee,
            tags: snapshot.tags.clone(),
            subtasks: snapshot.subtasks.clone(),
            recurrence: snapshot.recurrence.as_ref().map(RecurrenceDraft::from_rule),
            quantity: snapshot.quantity,
            unit: snapshot.unit.clone(),
        }
    }

    /// Срок для домена: (со временем, на весь день).
    #[allow(clippy::type_complexity)]
    pub fn due(
        &self,
        timezone: Tz,
    ) -> Result<(Option<chrono::DateTime<chrono::Utc>>, Option<NaiveDate>), DomainError> {
        match (self.date, self.time) {
            (Some(date), Some(time)) => Ok((Some(resolve_local(timezone, date, time)?), None)),
            (Some(date), None) => Ok((None, Some(date))),
            (None, _) => Ok((None, None)),
        }
    }

    /// Вход домена для создания элемента.
    pub fn to_new_item(
        &self,
        list_id: Uuid,
        timezone: Tz,
        kind: ListKind,
    ) -> Result<NewItem, DomainError> {
        let (due_at, due_date) = self.due(timezone)?;
        let shopping = kind != ListKind::Tasks;
        Ok(NewItem {
            list_id,
            title: self.title.clone(),
            notes: self.notes.clone(),
            priority: self.priority,
            due_at,
            due_date,
            assignee_user_id: self.assignee,
            source: crate::domain::models::ItemSource::Telegram,
            quantity: shopping.then(|| self.quantity.unwrap_or(1.0)),
            unit: shopping.then(|| self.unit.clone()).flatten(),
            tags: self.tags.clone(),
            subtasks: self.subtask_inputs(),
            recurrence: self.recurrence_input(timezone),
        })
    }

    fn subtask_inputs(&self) -> Vec<SubtaskInput> {
        self.subtasks
            .iter()
            .map(|subtask| SubtaskInput {
                title: subtask.title.clone(),
                status: subtask.done.then_some(ItemStatus::Done),
            })
            .collect()
    }

    fn recurrence_input(&self, timezone: Tz) -> Option<RecurrenceInput> {
        self.recurrence.as_ref().map(|rule| RecurrenceInput {
            freq: rule.freq,
            interval: Some(rule.interval),
            byweekday: rule.weekdays.clone(),
            until: None,
            timezone: Some(timezone.name().to_owned()),
            anchor_date: None,
            month_rule: rule.month_rule,
        })
    }

    /// Патч, который меняет только поля шага `step`.
    pub fn patch_for(&self, step: Step, timezone: Tz) -> Result<ItemPatch, DomainError> {
        let mut patch = ItemPatch::default();
        match step {
            Step::Title => patch.title = Some(self.title.clone()),
            Step::Due | Step::DueText => {
                let (due_at, due_date) = self.due(timezone)?;
                patch.due_at = due_at.map_or(Field::Null, Field::Value);
                patch.due_date = due_date.map_or(Field::Null, Field::Value);
                // Оба поля сразу задавать нельзя: пустое остаётся нетронутым.
                if due_at.is_some() {
                    patch.due_date = Field::Missing;
                } else if due_date.is_some() {
                    patch.due_at = Field::Missing;
                }
            }
            Step::Priority => patch.priority = Some(self.priority),
            Step::Assignee => {
                patch.assignee_user_id = self.assignee.map_or(Field::Null, Field::Value);
            }
            Step::Tags | Step::NewTag => patch.tags = Some(self.tags.clone()),
            Step::Subtasks => patch.subtasks = Some(self.subtask_inputs()),
            Step::Recurrence | Step::Weekdays | Step::MonthRule | Step::Interval => {
                patch.recurrence = self
                    .recurrence_input(timezone)
                    .map_or(Field::Null, Field::Value);
            }
            Step::Quantity => patch.quantity = self.quantity.map_or(Field::Null, Field::Value),
            Step::Unit => patch.unit = self.unit.clone().map_or(Field::Null, Field::Value),
            Step::Notes => patch.notes = self.notes.clone().map_or(Field::Null, Field::Value),
            Step::List => {}
        }
        Ok(patch)
    }
}

impl RecurrenceDraft {
    /// Черновик по сохранённому правилу.
    pub fn from_rule(rule: &Recurrence) -> Self {
        Self {
            freq: rule.freq,
            interval: rule.interval,
            weekdays: rule.byweekday.clone(),
            month_rule: rule.month_rule,
        }
    }

    /// Правило с настройками по умолчанию для выбранной частоты.
    pub fn new(freq: RecurrenceFreq, today_weekday: i16) -> Self {
        Self {
            freq,
            interval: 1,
            weekdays: if freq == RecurrenceFreq::Weekly {
                vec![today_weekday]
            } else {
                Vec::new()
            },
            month_rule: (freq == RecurrenceFreq::Monthly).then_some(MonthRule::AnchorDay),
        }
    }
}

/// Форма, которую хранит `conversation_states`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "form", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // форма живёт до одного обновления
pub enum Form {
    /// Онбординг нового household.
    Onboarding {
        /// Название household после первого шага.
        name: Option<String>,
    },
    /// Единый редактор существующего элемента или пункта черновика.
    Editor(Editor),
    /// Прежняя пошаговая форма или правка поля. В базе не создаётся; так
    /// читается сохранённый JSON `{"form":"create", …}` или
    /// `{"form":"edit", …}` и трактуется как истёкшая правка.
    #[serde(rename = "legacy_edit")]
    LegacyEdit,
    /// Ввод даты и времени для быстрого срока в карточке.
    DueText {
        /// Элемент.
        item_id: Uuid,
        /// Версия, которую видел пользователь.
        version: i32,
        /// Экран за карточкой.
        back: Screen,
    },
    /// Ожидание поискового запроса.
    Search,
    /// Новое имя пользователя.
    Rename,
    /// Подтверждение удаления household названием.
    DeleteHousehold,
    /// Своё время напоминания в минутах.
    Lead,
    /// Время настройки уведомлений в формате `HH:MM`.
    Time {
        /// Какое время редактируется.
        field: TimeField,
    },
}

/// Позиция, которая уже есть в списке.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Duplicate {
    /// Существующая позиция.
    pub item_id: Uuid,
    /// Её название.
    pub title: String,
    /// Количество.
    pub quantity: Option<f64>,
    /// Единица.
    pub unit: Option<String>,
    /// Версия.
    pub version: i32,
}

impl Duplicate {
    /// Найти открытую позицию с тем же названием: правило редактора покупки.
    pub fn find<'a>(items: impl IntoIterator<Item = &'a Item>, title: &str) -> Option<Self> {
        let wanted = title.trim().to_lowercase();
        items
            .into_iter()
            .find(|item| {
                item.status == ItemStatus::Open && item.title.trim().to_lowercase() == wanted
            })
            .map(|item| Self {
                item_id: item.id,
                title: item.title.clone(),
                quantity: item.quantity,
                unit: item.unit.clone(),
                version: item.version,
            })
    }

    /// Количество после «Увеличить»: сумма при совпадающей единице и лимите.
    pub fn increase_total(&self, quantity: f64, unit: Option<&str>) -> Option<f64> {
        let existing = self.quantity?;
        let same_unit = unit.or(Some("шт")).map(str::to_lowercase)
            == self.unit.as_deref().map(str::to_lowercase);
        if !same_unit {
            return None;
        }
        let total = existing + quantity;
        (total <= MAX_INPUT_QUANTITY).then_some(total)
    }

    /// Подпись кнопки «Увеличить».
    pub fn increase_label(&self, total: f64) -> String {
        format!(
            "➕ Увеличить до {}",
            ru::quantity_label(total, self.unit.as_deref())
        )
    }

    /// Подпись кнопки «Добавить отдельно».
    pub fn separate_label(&self) -> &'static str {
        "📄 Добавить отдельно"
    }
}

/// Дубликат, который не сохраняется до выбора пользователя.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DuplicateChoice {
    /// Существующая открытая позиция.
    pub existing: Duplicate,
    /// Название из разбора.
    pub title: String,
    /// Количество из разбора.
    pub quantity: f64,
    /// Единица из разбора; `None` — при отдельном добавлении возьмётся «шт».
    pub unit: Option<String>,
}

impl DuplicateChoice {
    /// Количество после «Увеличить» для этого выбора.
    pub fn increase_total(&self) -> Option<f64> {
        let unit = self.unit.as_deref().or(self.existing.unit.as_deref());
        self.existing.increase_total(self.quantity, unit)
    }

    /// Кнопки выбора для одного дубликата; остальные переносятся в действии.
    pub fn buttons(
        &self,
        list_id: Uuid,
        rest: Vec<Self>,
        draft_id: Option<Uuid>,
        created_ids: &[Uuid],
    ) -> Vec<Button<Action>> {
        let mut buttons = Vec::new();
        if let Some(total) = self.increase_total() {
            buttons.push(button(
                self.existing.increase_label(total),
                Action::DuplicateIncrease {
                    item_id: self.existing.item_id,
                    version: self.existing.version,
                    quantity: total,
                    rest: rest.clone(),
                    draft_id,
                    created_ids: created_ids.to_vec(),
                },
            ));
        }
        buttons.push(button(
            self.existing.separate_label(),
            Action::DuplicateSeparate {
                list_id,
                title: self.title.clone(),
                quantity: self.quantity,
                unit: self.unit.clone(),
                rest,
                draft_id,
                created_ids: created_ids.to_vec(),
            },
        ));
        buttons
    }
}

/// Разделить новые позиции на создаваемые и дубликаты уже существующих.
///
/// Дубликатом считается открытая позиция с тем же названием: количество
/// суммируется, если совпадает единица. Позиции этого пакета между собой не
/// сравниваются: вызывающая сторона добавляет созданные позиции в `existing`
/// перед следующей проверкой, как и раньше.
pub fn plan_additions(
    existing: &[Item],
    new_items: Vec<NewItem>,
) -> (Vec<NewItem>, Vec<DuplicateChoice>) {
    let mut to_create: Vec<NewItem> = Vec::new();
    let mut duplicates: Vec<DuplicateChoice> = Vec::new();
    for new_item in new_items {
        if let Some(existing_item) = Duplicate::find(existing, &new_item.title) {
            add_duplicate(&mut duplicates, existing_item, &new_item);
        } else {
            to_create.push(new_item);
        }
    }
    (to_create, duplicates)
}

/// Слить выбор дубликата в накопленный список по тому же названию и единице.
pub fn merge_duplicate_choice(duplicates: &mut Vec<DuplicateChoice>, choice: DuplicateChoice) {
    let effective_unit = choice.unit.as_deref().or(choice.existing.unit.as_deref());
    if let Some(pending) = duplicates.iter_mut().find(|pending| {
        pending.existing.item_id == choice.existing.item_id
            && same_unit(
                pending.unit.as_deref().or(pending.existing.unit.as_deref()),
                effective_unit,
            )
    }) {
        pending.quantity += choice.quantity;
    } else {
        duplicates.push(choice);
    }
}

/// Слить ещё одну позицию в выбор дубликата с тем же названием и единицей.
fn add_duplicate(duplicates: &mut Vec<DuplicateChoice>, existing: Duplicate, new_item: &NewItem) {
    merge_duplicate_choice(
        duplicates,
        DuplicateChoice {
            existing,
            title: new_item.title.clone(),
            quantity: new_item.quantity.unwrap_or(1.0),
            unit: new_item.unit.clone(),
        },
    );
}

/// Кнопка «Отменить» созданные позиции, если они есть.
fn undo_button(created_ids: &[Uuid], draft_id: Option<Uuid>) -> Option<Button<Action>> {
    (!created_ids.is_empty()).then(|| {
        button(
            "↩️ Отменить",
            Action::AddedUndo {
                item_ids: created_ids.to_vec(),
                draft_id,
            },
        )
    })
}

/// Ряд «Открыть» ожидающего черновика.
fn draft_row(draft_id: Uuid) -> Vec<Button<Action>> {
    vec![button(
        "Открыть",
        Action::Go {
            to: Screen::Draft { draft_id },
        },
    )]
}

/// Ряды баннера прямого добавления без дубликатов: отмена созданных позиций и
/// «Открыть» ожидающего черновика. Списка здесь нет, поэтому фиктивный
/// `list_id` не нужен.
pub(super) fn status_rows(
    created_ids: &[Uuid],
    draft_id: Option<Uuid>,
) -> Vec<Vec<Button<Action>>> {
    let mut rows = Vec::new();
    if let Some(button) = undo_button(created_ids, draft_id) {
        rows.push(vec![button]);
    }
    if let Some(draft_id) = draft_id {
        rows.push(draft_row(draft_id));
    }
    rows
}

/// Ряды кнопок баннера прямого добавления: дубликаты и отмена созданных
/// позиций, а «Открыть» — отдельным последним рядом, чтобы кнопки не
/// обрезались по ширине.
pub fn addition_rows(
    duplicates: &[DuplicateChoice],
    list_id: Uuid,
    created_ids: &[Uuid],
    draft_id: Option<Uuid>,
) -> Vec<Vec<Button<Action>>> {
    let mut rows = Vec::new();
    let mut row = duplicates
        .split_first()
        .map(|(first, rest)| first.buttons(list_id, rest.to_vec(), draft_id, created_ids))
        .unwrap_or_default();
    if let Some(button) = undo_button(created_ids, draft_id) {
        row.push(button);
    }
    if !row.is_empty() {
        rows.push(row);
    }
    if let Some(draft_id) = draft_id {
        rows.push(draft_row(draft_id));
    }
    rows
}

/// Экран списка по текущему экрану: тот же список сохраняет страницу и режим,
/// иначе открывается первая страница обычного списка.
pub fn list_screen(current: Option<&Screen>, list_id: Uuid) -> Screen {
    if let Some(Screen::List {
        list_id: current_id,
        page,
        edit,
        from_lists,
    }) = current
        && *current_id == list_id
    {
        return Screen::List {
            list_id,
            page: *page,
            edit: *edit,
            from_lists: *from_lists,
        };
    }
    Screen::List {
        list_id,
        page: 0,
        edit: false,
        from_lists: false,
    }
}

/// Совпадают ли единицы измерения для слияния дубликатов.
pub fn same_unit(left: Option<&str>, right: Option<&str>) -> bool {
    let fold = |value: &str| value.trim().to_lowercase().replace('ё', "е");
    match (left, right) {
        (Some(left), Some(right)) => fold(left) == fold(right),
        (None, None) => true,
        _ => false,
    }
}

/// Сообщение о дубликатах, которые ещё ждут выбора.
pub fn duplicate_notice(duplicates: &[DuplicateChoice]) -> Option<String> {
    let next = duplicates.first()?;
    let more = duplicates.len().saturating_sub(1);
    if more == 0 {
        Some(format!("Уже в списке: «{}».", next.title))
    } else {
        Some(format!("Уже в списке: «{}» (и ещё {more}).", next.title))
    }
}

/// Единый редактор существующего элемента.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Editor {
    /// Что правим.
    pub target: EditorTarget,
    /// Текущие значения полей.
    pub draft: ItemDraft,
    /// Поля, которые пользователь менял.
    pub touched: BTreeSet<EditorField>,
    /// Открытый экран поля; `None` — сам редактор.
    pub field: Option<Step>,
    /// Родительский экран поля, если открыт его подэкран (например,
    /// «Другой интервал» из повторения).
    #[serde(default)]
    pub field_back: Option<Step>,
    /// Снимок черновика на момент открытия поля: «Назад» без изменения поля
    /// возвращает его.
    #[serde(default)]
    pub field_backup: Option<ItemDraft>,
    /// Список, выбранный в редакторе.
    pub list_id: Uuid,
    /// Последняя ошибка ввода.
    pub error: Option<String>,
    /// Поля, по которым при сохранении обнаружен конфликт.
    #[serde(default)]
    pub conflict: Option<BTreeSet<EditorField>>,
    /// Снимок текущего значения, показанный на экране конфликта: «Оставить
    /// мой» сравнивает поля с ним, а не с более поздней правкой другого
    /// участника.
    #[serde(default)]
    pub conflict_seen: Option<Box<ItemSnapshot>>,
    /// Такая же позиция, найденная при сохранении покупки.
    #[serde(default)]
    pub duplicate: Option<Duplicate>,
}

impl Form {
    /// Значение `conversation_states.kind`.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Onboarding { .. } => "onboarding",
            Self::Editor(_) => "editor",
            Self::LegacyEdit => "legacy_edit",
            Self::DueText { .. } => "due_text",
            Self::Search => "search",
            Self::Rename => "rename",
            Self::DeleteHousehold => "delete_household",
            Self::Lead => "lead",
            Self::Time { .. } => "time",
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn old_edit_callback_with_field_is_read() {
        let action: Action = serde_json::from_value(json!({
            "a": "edit",
            "item_id": Uuid::nil(),
            "field": "due",
            "back": {"t": "menu"},
        }))
        .expect("old edit token");
        assert_eq!(
            action,
            Action::Edit {
                item_id: Uuid::nil(),
                back: Screen::Menu,
            }
        );
    }

    #[test]
    fn snooze_actions_accept_old_tokens_without_a_version() {
        let snooze: Action = serde_json::from_value(json!({
            "a": "snooze",
            "item_id": Uuid::nil(),
            "until": "one_hour",
        }))
        .expect("old snooze token");
        assert_eq!(
            snooze,
            Action::Snooze {
                item_id: Uuid::nil(),
                until: SnoozeUntil::OneHour,
                version: None,
            }
        );
        assert_eq!(snooze.version(), None);

        let batch: Action = serde_json::from_value(json!({
            "a": "batch_snooze",
            "batch_id": Uuid::nil(),
            "item_id": Uuid::nil(),
        }))
        .expect("old batch snooze token");
        assert_eq!(
            batch,
            Action::BatchSnooze {
                batch_id: Uuid::nil(),
                item_id: Uuid::nil(),
                version: None,
            }
        );
        assert_eq!(batch.version(), None);
    }

    #[test]
    fn old_addition_undo_and_duplicate_tokens_are_read() {
        let undo: Action = serde_json::from_value(json!({
            "a": "added_undo",
            "item_ids": [Uuid::nil()],
        }))
        .expect("old added undo token");
        assert_eq!(
            undo,
            Action::AddedUndo {
                item_ids: vec![Uuid::nil()],
                draft_id: None,
            }
        );

        let increase: Action = serde_json::from_value(json!({
            "a": "duplicate_increase",
            "item_id": Uuid::nil(),
            "version": 3,
            "quantity": 2.0,
        }))
        .expect("old duplicate increase token");
        assert_eq!(
            increase,
            Action::DuplicateIncrease {
                item_id: Uuid::nil(),
                version: 3,
                quantity: 2.0,
                rest: Vec::new(),
                draft_id: None,
                created_ids: Vec::new(),
            }
        );

        let separate: Action = serde_json::from_value(json!({
            "a": "duplicate_separate",
            "list_id": Uuid::nil(),
            "title": "Хлеб",
            "quantity": 1.0,
        }))
        .expect("old duplicate separate token");
        assert_eq!(
            separate,
            Action::DuplicateSeparate {
                list_id: Uuid::nil(),
                title: "Хлеб".to_owned(),
                quantity: 1.0,
                unit: None,
                rest: Vec::new(),
                draft_id: None,
                created_ids: Vec::new(),
            }
        );
    }

    #[test]
    fn snooze_actions_carry_the_rendered_version() {
        let snooze = Action::Snooze {
            item_id: Uuid::nil(),
            until: SnoozeUntil::TomorrowMorning,
            version: Some(7),
        };
        let payload = serde_json::to_value(&snooze).expect("snooze payload");
        assert_eq!(payload["version"], json!(7));
        assert_eq!(snooze.version(), Some(7));

        let batch = Action::BatchSnooze {
            batch_id: Uuid::nil(),
            item_id: Uuid::nil(),
            version: Some(9),
        };
        let payload = serde_json::to_value(&batch).expect("batch snooze payload");
        assert_eq!(payload["version"], json!(9));
        assert_eq!(batch.version(), Some(9));
    }

    #[test]
    fn old_new_target_with_list_id_is_read() {
        let target: EditorTarget = serde_json::from_value(json!({
            "target": "new",
            "list_id": Uuid::nil(),
            "kind": "tasks",
            "back": {"t": "menu"},
        }))
        .expect("old new target");
        assert_eq!(
            target,
            EditorTarget::New {
                kind: ListKind::Tasks,
                back: Screen::Menu,
            }
        );
    }

    #[test]
    fn first_contact_screen_cannot_represent_onboarding() {
        assert!(
            serde_json::from_value::<Screen>(json!({"t": "first_contact", "kind": "onboard"}))
                .is_err(),
            "an old onboarding screen must fall back instead of being rendered"
        );
        assert_eq!(
            serde_json::from_value::<Screen>(json!({"t": "first_contact", "kind": "ask_invite"}))
                .expect("ask invite screen"),
            Screen::FirstContact(FirstContactScreen::AskInvite)
        );
    }

    #[test]
    fn trail_follows_depth_and_replaces_the_data_dependent_parent() {
        let list = Screen::List {
            list_id: Uuid::nil(),
            page: 0,
            edit: false,
            from_lists: false,
        };
        assert_eq!(Screen::Menu.depth(), 1);
        assert_eq!(list.depth(), 2);
        assert_eq!(list.trail("Задачи · 3", Some("Задачи")), "Задачи · 3");

        let card = Screen::Item {
            item_id: Uuid::nil(),
            back: Box::new(list.clone()),
            completion_id: None,
        };
        assert_eq!(card.depth(), 3);
        assert_eq!(
            card.trail("Оплатить интернет", Some("Задачи")),
            "Задачи › Оплатить интернет"
        );

        let confirm = Screen::ConfirmDelete {
            item_id: Uuid::nil(),
            back: Box::new(list),
        };
        assert_eq!(confirm.depth(), 3);
        assert_eq!(
            confirm.trail("Удалить «Оплатить интернет»?", Some("Задачи")),
            "Задачи › Удалить «Оплатить интернет»?"
        );

        let member = Screen::Member {
            user_id: Uuid::nil(),
        };
        assert_eq!(member.depth(), 4);
        assert_eq!(member.trail("Мария", None), "Настройки › Семья › Мария");
    }

    #[test]
    fn trail_is_clipped_to_sixty_characters() {
        let fits = "я".repeat(60);
        assert_eq!(Screen::Menu.trail(&fits, None), fits);

        let clipped = Screen::Menu.trail(&"я".repeat(61), None);
        assert_eq!(clipped.chars().count(), 60);
        assert_eq!(clipped, format!("{}…", "я".repeat(59)));
    }

    #[test]
    fn list_from_all_lists_is_deeper_and_keeps_the_lists_parent() {
        let list = Screen::List {
            list_id: Uuid::nil(),
            page: 0,
            edit: false,
            from_lists: true,
        };
        assert_eq!(list.depth(), 3);
        assert_eq!(list.trail("Задачи · 3", None), "Все списки › Задачи · 3");

        let card = Screen::Item {
            item_id: Uuid::nil(),
            back: Box::new(list),
            completion_id: None,
        };
        assert_eq!(card.depth(), 4);
        assert_eq!(
            card.trail("Оплатить интернет", Some("Задачи")),
            "Все списки › Задачи › Оплатить интернет"
        );
    }

    #[test]
    fn old_item_json_without_completion_is_read_as_a_plain_card() {
        let screen: Screen = serde_json::from_value(json!({
            "t": "item",
            "item_id": Uuid::nil(),
            "back": {"t": "menu"},
        }))
        .expect("legacy item screen");
        assert_eq!(
            screen,
            Screen::Item {
                item_id: Uuid::nil(),
                back: Box::new(Screen::Menu),
                completion_id: None,
            }
        );

        let action: Action = serde_json::from_value(json!({
            "a": "reopen",
            "item_id": Uuid::nil(),
            "version": 1,
            "back": {"t": "menu"},
        }))
        .expect("legacy reopen action");
        assert_eq!(
            action,
            Action::Reopen {
                item_id: Uuid::nil(),
                version: 1,
                back: Screen::Menu,
                completion_id: None,
            }
        );
    }

    #[test]
    fn old_list_json_without_origin_is_read_as_opened_from_menu() {
        let screen: Screen = serde_json::from_value(json!({
            "t": "list",
            "list_id": Uuid::nil(),
            "page": 0,
            "edit": false,
        }))
        .expect("legacy list screen");
        assert_eq!(
            screen,
            Screen::List {
                list_id: Uuid::nil(),
                page: 0,
                edit: false,
                from_lists: false,
            }
        );
    }

    #[test]
    fn old_clear_json_without_origin_is_read_as_opened_from_menu() {
        let confirm: Screen = serde_json::from_value(json!({
            "t": "confirm_clear",
            "list_id": Uuid::nil(),
        }))
        .expect("legacy confirm clear screen");
        assert_eq!(
            confirm,
            Screen::ConfirmClear {
                list_id: Uuid::nil(),
                from_lists: false,
            }
        );
        let cleared: Screen = serde_json::from_value(json!({
            "t": "cleared",
            "list_id": Uuid::nil(),
            "item_ids": [],
        }))
        .expect("legacy cleared screen");
        assert_eq!(
            cleared,
            Screen::Cleared {
                list_id: Uuid::nil(),
                item_ids: Vec::new(),
                from_lists: false,
            }
        );

        let action: Action = serde_json::from_value(json!({
            "a": "clear_do",
            "list_id": Uuid::nil(),
        }))
        .expect("legacy clear action");
        assert_eq!(
            action,
            Action::ClearDo {
                list_id: Uuid::nil(),
                from_lists: false,
            }
        );
    }

    #[test]
    fn deleted_screen_ignores_the_legacy_shopping_field() {
        let with_legacy: Screen = serde_json::from_value(json!({
            "t": "deleted",
            "item_id": Uuid::nil(),
            "title": "Молоко",
            "shopping": true,
            "back": {"t": "menu"},
        }))
        .expect("legacy deleted screen");
        let current: Screen = serde_json::from_value(json!({
            "t": "deleted",
            "item_id": Uuid::nil(),
            "title": "Молоко",
            "back": {"t": "menu"},
        }))
        .expect("deleted screen");
        assert_eq!(with_legacy, current);
    }

    #[test]
    fn every_deep_screen_has_a_parent_for_each_level() {
        let list = Screen::List {
            list_id: Uuid::nil(),
            page: 0,
            edit: false,
            from_lists: false,
        };
        let screens = vec![
            Screen::Item {
                item_id: Uuid::nil(),
                back: Box::new(list.clone()),
                completion_id: None,
            },
            Screen::Due {
                item_id: Uuid::nil(),
                back: Box::new(list.clone()),
            },
            Screen::Deleted {
                item_id: Uuid::nil(),
                title: "Задача".into(),
                back: Box::new(list.clone()),
            },
            Screen::ConfirmDelete {
                item_id: Uuid::nil(),
                back: Box::new(list),
            },
            Screen::ConfirmClear {
                list_id: Uuid::nil(),
                from_lists: false,
            },
            Screen::Cleared {
                list_id: Uuid::nil(),
                item_ids: Vec::new(),
                from_lists: false,
            },
            Screen::Notifications,
            Screen::LeadTime,
            Screen::Profile,
            Screen::Danger,
            Screen::Timezones,
            Screen::Family,
            Screen::Integrations { page: 0 },
            Screen::Invites,
            Screen::Journal { page: 0 },
            Screen::Member {
                user_id: Uuid::nil(),
            },
            Screen::ConfirmRemove {
                user_id: Uuid::nil(),
            },
            Screen::ConfirmTransfer {
                user_id: Uuid::nil(),
            },
            Screen::ConfirmRevokeToken {
                token_id: Uuid::nil(),
            },
            Screen::ConfirmLeave,
            Screen::InviteCreated {
                invite_id: Uuid::nil(),
                code: String::new(),
            },
        ];

        for screen in screens {
            assert!(screen.depth() > 2, "{screen:?}");
            assert_eq!(
                screen.depth(),
                screen.parents().len() + 2,
                "each deep level needs a parent: {screen:?}"
            );
        }
    }
}
