//! Модель бота: экраны, действия кнопок и черновик элемента.
//!
//! Экран и действие сериализуются в JSON. Экран хранится в `telegram_views`
//! (по нему перерисовывается устаревшее сообщение), действие — в
//! `callback_tokens` рядом с токеном кнопки.

use chrono::{NaiveDate, NaiveTime};
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
use crate::telegram::rich::View;

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
    /// Выбор: добавить задачу или покупку.
    Add,
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
    },
    /// Карточка элемента.
    Item {
        /// Элемент.
        item_id: Uuid,
        /// Куда ведёт «Назад».
        back: Box<Screen>,
    },
    /// Результат удаления с кнопкой отмены.
    Deleted {
        /// Элемент.
        item_id: Uuid,
        /// Название на момент удаления.
        title: String,
        /// Удалён из списка покупок: поле сохранено ради формата JSON и на
        /// подписи кнопок больше не влияет.
        shopping: bool,
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
    },
    /// Результат очистки с кнопкой отмены.
    Cleared {
        /// Список.
        list_id: Uuid,
        /// Удалённые позиции.
        item_ids: Vec<Uuid>,
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
    /// Настройки уведомлений.
    Notifications,
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
            Self::Add
            | Self::Lists
            | Self::List { .. }
            | Self::Mine { .. }
            | Self::Results { .. }
            | Self::Settings
            | Self::Form
            | Self::Processing { .. }
            | Self::Draft { .. } => 2,
            Self::Item { back, .. }
            | Self::Deleted { back, .. }
            | Self::ConfirmDelete { back, .. } => back.depth() + 1,
            Self::ConfirmClear { .. } | Self::Cleared { .. } => 3,
            Self::Notifications
            | Self::Timezones
            | Self::Family
            | Self::Integrations { .. }
            | Self::Journal { .. }
            | Self::ConfirmLeave => 3,
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
            Self::Add => "Что добавить?",
            Self::Lists => "Все списки",
            Self::List { .. } => "Список",
            Self::Item { .. } => "Карточка",
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
            Self::Results { .. } => "Поиск",
            Self::Settings => "Настройки",
            Self::Notifications => "Уведомления",
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
            Self::Add
            | Self::Lists
            | Self::List { .. }
            | Self::Mine { .. }
            | Self::Results { .. }
            | Self::Settings
            | Self::Form
            | Self::Processing { .. }
            | Self::Draft { .. } => Vec::new(),
            Self::Item { back, .. }
            | Self::Deleted { back, .. }
            | Self::ConfirmDelete { back, .. } => {
                let mut parents = back.parents();
                parents.push(back.crumb());
                parents
            }
            Self::ConfirmClear { .. } | Self::Cleared { .. } => vec!["Список"],
            Self::Notifications
            | Self::Timezones
            | Self::Family
            | Self::Integrations { .. }
            | Self::Journal { .. } => vec!["Настройки"],
            Self::ConfirmLeave => vec!["Настройки"],
            Self::Member { .. } | Self::Invites => vec!["Настройки", "Семья"],
            Self::ConfirmRemove { .. } | Self::ConfirmTransfer { .. } => {
                vec!["Настройки", "Семья", "Участник"]
            }
            Self::ConfirmRevokeToken { .. } => vec!["Настройки", "Интеграции"],
            Self::InviteCreated { .. } => vec!["Настройки", "Семья", "Приглашения"],
        }
    }

    /// Хлебные крошки экрана: имена родителей и `title` через « › », не
    /// длиннее 60 символов. `parent` подменяет имя непосредственного родителя,
    /// если оно зависит от данных (например, имя списка).
    pub fn trail(&self, title: &str, parent: Option<&str>) -> String {
        let deep = self.depth() > 2;
        let mut parts: Vec<String> = if deep {
            self.parents().into_iter().map(str::to_owned).collect()
        } else {
            Vec::new()
        };
        if deep && let Some(parent) = parent {
            parts.pop();
            parts.push(parent.to_owned());
        }
        parts.push(title.to_owned());
        clip_trail(parts.join(" › "))
    }
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
    /// Предпросмотр.
    Preview,
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
    /// Пропустить шаг.
    Skip,
    /// Закончить многошаговый выбор (теги, подзадачи, дни).
    Done,
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
    /// Начать создание элемента.
    Add {
        /// Список; без значения бот предложит выбрать.
        list_id: Option<Uuid>,
        /// Вид списка для выбора.
        kind: ListKind,
    },
    /// Открыть шаг редактирования существующего элемента.
    Edit {
        /// Элемент.
        item_id: Uuid,
        /// Шаг.
        step: Step,
        /// Куда вернуться.
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
    },
    /// Очистить выполненные.
    ClearDo {
        /// Список.
        list_id: Uuid,
    },
    /// Отменить очистку выполненных.
    ClearUndo {
        /// Список.
        list_id: Uuid,
        /// Удалённые позиции.
        item_ids: Vec<Uuid>,
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
    /// Открыть форму правки сразу на шаге срока.
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
            | Self::NoticeComplete { version, .. }
            | Self::BatchComplete { version, .. } => Some(*version),
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
        let (date, time) = match (item.due_at, item.due_date) {
            (Some(at), _) => {
                let local = at.with_timezone(&timezone);
                (Some(local.date_naive()), Some(local.time()))
            }
            (None, date) => (date, None),
        };
        Self {
            title: item.title.clone(),
            notes: item.notes.clone(),
            date,
            time,
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
            recurrence: item.recurrence.as_ref().map(RecurrenceDraft::from_rule),
            quantity: item.quantity,
            unit: item.unit.clone(),
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
            Step::List | Step::Preview => {}
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
    /// Создание элемента.
    Create(ItemForm),
    /// Редактирование поля существующего элемента.
    Edit(EditForm),
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

/// Создание элемента.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemForm {
    /// Список, если уже выбран.
    pub list_id: Option<Uuid>,
    /// Вид списка.
    pub kind: ListKind,
    /// Текущий шаг.
    pub step: Step,
    /// Заполненные поля.
    pub draft: ItemDraft,
    /// После шага вернуться к предпросмотру.
    pub from_preview: bool,
    /// Последняя ошибка ввода.
    pub error: Option<String>,
    /// Найденная такая же позиция списка покупок.
    #[serde(default)]
    pub duplicate: Option<Duplicate>,
    /// Форма правит элемент черновика capture, а не создаёт новый.
    #[serde(default)]
    pub target: Option<DraftTarget>,
    /// Быстрая форма: список и его тип можно сменить прямо из предпросмотра.
    #[serde(default)]
    pub quick: bool,
}

/// Элемент черновика capture, который правит форма.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftTarget {
    /// Черновик.
    pub draft_id: Uuid,
    /// Номер элемента с нуля.
    pub index: usize,
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

/// Редактирование поля существующего элемента.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditForm {
    /// Элемент.
    pub item_id: Uuid,
    /// Шаг.
    pub step: Step,
    /// Куда вернуться по завершении.
    pub back: Screen,
    /// Последняя ошибка ввода.
    pub error: Option<String>,
}

impl Form {
    /// Значение `conversation_states.kind`.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Onboarding { .. } => "onboarding",
            Self::Create(_) => "create",
            Self::Edit(_) => "edit",
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
        };
        assert_eq!(Screen::Menu.depth(), 1);
        assert_eq!(list.depth(), 2);
        assert_eq!(list.trail("Задачи · 3", Some("Задачи")), "Задачи · 3");

        let card = Screen::Item {
            item_id: Uuid::nil(),
            back: Box::new(list.clone()),
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
    fn every_deep_screen_has_a_parent_for_each_level() {
        let list = Screen::List {
            list_id: Uuid::nil(),
            page: 0,
            edit: false,
        };
        let screens = vec![
            Screen::Item {
                item_id: Uuid::nil(),
                back: Box::new(list.clone()),
            },
            Screen::Deleted {
                item_id: Uuid::nil(),
                title: "Задача".into(),
                shopping: false,
                back: Box::new(list.clone()),
            },
            Screen::ConfirmDelete {
                item_id: Uuid::nil(),
                back: Box::new(list),
            },
            Screen::ConfirmClear {
                list_id: Uuid::nil(),
            },
            Screen::Cleared {
                list_id: Uuid::nil(),
                item_ids: Vec::new(),
            },
            Screen::Notifications,
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
