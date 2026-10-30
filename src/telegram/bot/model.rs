//! Модель бота: экраны, действия кнопок и черновик элемента.
//!
//! Экран и действие сериализуются в JSON. Экран хранится в `telegram_views`
//! (по нему перерисовывается устаревшее сообщение), действие — в
//! `callback_tokens` рядом с токеном кнопки.

use chrono::{NaiveDate, NaiveTime};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::error::DomainError;
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
    },
    /// Карточка элемента.
    Item {
        /// Элемент.
        item_id: Uuid,
        /// Куда ведёт «Назад».
        back: Box<Screen>,
    },
    /// Результат завершения задачи с кнопкой отмены.
    Completed {
        /// Элемент.
        item_id: Uuid,
        /// Куда ведёт «К списку».
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
    Processing,
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
            | Self::NoticeComplete { version, .. } => Some(*version),
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
        }
    }
}
