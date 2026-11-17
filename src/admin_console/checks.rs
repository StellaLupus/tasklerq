//! Ход проверки слота из консоли.
//!
//! Проверка идёт в фоне: прогон до 22 фраз по 20 секунд не укладывается в
//! HTTP-таймаут прокси, поэтому POST только запускает задачу и отвечает `303` на
//! страницу хода. Ход живёт в памяти процесса: одна структура в
//! [`crate::admin_console::router::AdminState`] на инсталляцию — одновременно
//! выполняется не более одной проверки. Межпроцессно его защищает
//! advisory-блокировка прогона ([`crate::capture::slot_check::run_check`]).
//!
//! Повторное нажатие во время прогона не запускает второй: слот в памяти уже
//! занят, и POST ведёт на ход текущего прогона, даже если нажата кнопка другого
//! слота.

use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::task::JoinHandle;

use crate::capture::slot_check::CheckResult;
use crate::llm::{FALLBACK_SLOT, PRIMARY_SLOT};

/// Сообщение, когда фоновая задача проверки завершилась аварийно.
pub const CHECK_INTERRUPTED: &str = "Проверка прервана: фоновая задача завершилась с ошибкой.";

/// Слот, который проверяет кнопка консоли.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckSlot {
    /// Основной слот.
    Primary,
    /// Резервный слот.
    Fallback,
}

impl CheckSlot {
    /// Слоты, у которых бывает кнопка проверки.
    pub const ALL: [Self; 2] = [Self::Primary, Self::Fallback];

    /// Разобрать слот из адреса формы; неизвестный — `None`.
    pub fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "primary" => Some(Self::Primary),
            "fallback" => Some(Self::Fallback),
            _ => None,
        }
    }

    /// Слот в адресе формы.
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Fallback => "fallback",
        }
    }

    /// Имя слота для отчёта и `llm_slot_checks`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Primary => PRIMARY_SLOT,
            Self::Fallback => FALLBACK_SLOT,
        }
    }

    /// Действующий слот конфигурации.
    pub fn config(
        self,
        effective: &crate::settings::EffectiveLlm,
    ) -> Option<&crate::config::SlotConfig> {
        match self {
            Self::Primary => effective.primary().config(),
            Self::Fallback => effective.fallback().config(),
        }
    }
}

/// Итог завершённой проверки.
#[derive(Debug, Clone)]
pub enum CheckOutcome {
    /// Отчёт готов; `saved` — удалось ли записать результат.
    Report {
        /// Читаемый отчёт с причинами провалов и оценкой задержки.
        report: String,
        /// Сколько фраз пройдено.
        passed: usize,
        /// Сколько фраз в наборе.
        total: usize,
        /// Результат проверки записан.
        saved: bool,
    },
    /// Проверка не выполнена: сообщение без секретов и адресов.
    Failed {
        /// Причина для страницы.
        message: String,
    },
}

/// Снимок хода проверки для страницы.
#[derive(Debug, Clone)]
pub struct CheckSnapshot {
    /// Имя слота.
    pub slot: &'static str,
    /// Сколько фраз уже пройдено.
    pub done: usize,
    /// Сколько фраз в наборе.
    pub total: usize,
    /// Итог; `None` — проверка идёт.
    pub outcome: Option<CheckOutcome>,
}

impl CheckSnapshot {
    /// Проверка ещё идёт.
    pub fn running(&self) -> bool {
        self.outcome.is_none()
    }
}

/// Одна проверка слота в памяти процесса.
struct CheckRun {
    slot: &'static str,
    done: usize,
    total: usize,
    outcome: Option<CheckOutcome>,
}

/// Состояние проверки слота: одна на процесс.
#[derive(Clone, Default)]
pub struct CheckState {
    inner: Arc<Mutex<Option<CheckRun>>>,
}

impl CheckState {
    /// Состояние без запущенных проверок.
    pub fn new() -> Self {
        Self::default()
    }

    /// Занять проверку под слот.
    ///
    /// `false` — проверка уже идёт: второй прогон не начинается. Завершённая
    /// проверка место освобождает, и следующая кнопка запускает новый прогон.
    pub fn try_start(&self, slot: &'static str, total: usize) -> bool {
        let mut run = self.inner.lock().expect("check state lock");
        if run
            .as_ref()
            .is_some_and(|current| current.outcome.is_none())
        {
            return false;
        }
        *run = Some(CheckRun {
            slot,
            done: 0,
            total,
            outcome: None,
        });
        true
    }

    /// Отметить пройденные фразы; после завершения не меняет итог.
    pub fn advance(&self, done: usize, total: usize) {
        let mut run = self.inner.lock().expect("check state lock");
        if let Some(current) = run.as_mut()
            && current.outcome.is_none()
        {
            current.done = done.min(total);
            current.total = total;
        }
    }

    /// Записать готовый отчёт.
    pub fn finish(&self, report: String, passed: usize, total: usize, saved: bool) {
        let mut run = self.inner.lock().expect("check state lock");
        if let Some(current) = run.as_mut() {
            current.done = total;
            current.total = total;
            current.outcome = Some(CheckOutcome::Report {
                report,
                passed,
                total,
                saved,
            });
        }
    }

    /// Записать ошибку прогона.
    pub fn fail(&self, message: impl Into<String>) {
        let mut run = self.inner.lock().expect("check state lock");
        if let Some(current) = run.as_mut() {
            current.outcome = Some(CheckOutcome::Failed {
                message: message.into(),
            });
        }
    }

    /// Дождаться фоновой задачи и завершить ход, если она упала.
    ///
    /// Паника или отмена задачи иначе оставили бы `outcome: None` навсегда:
    /// страница опрашивала бы «идёт» до перезапуска процесса. Завершённый
    /// прогон сам записывает отчёт, поэтому здесь ничего не меняется.
    pub async fn watch(&self, task: JoinHandle<()>) {
        if task.await.is_err() {
            let mut run = self.inner.lock().expect("check state lock");
            if let Some(current) = run.as_mut()
                && current.outcome.is_none()
            {
                current.outcome = Some(CheckOutcome::Failed {
                    message: CHECK_INTERRUPTED.to_owned(),
                });
            }
        }
    }

    /// Запустить прогон отдельной задачей и следить за её завершением.
    ///
    /// Запуск и наблюдатель живут вместе: обработчик не может забыть повесить
    /// наблюдателя, и паника фоновой задачи не оставляет ход «идущим».
    /// Возвращаемый handle — задача наблюдателя; боевой путь его не ждёт.
    pub fn spawn<F>(&self, run: F) -> JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let watcher = self.clone();
        let task = tokio::spawn(run);
        tokio::spawn(async move { watcher.watch(task).await })
    }

    /// Текущий ход проверки; `None` — проверок ещё не было.
    pub fn current(&self) -> Option<CheckSnapshot> {
        self.inner
            .lock()
            .expect("check state lock")
            .as_ref()
            .map(|run| CheckSnapshot {
                slot: run.slot,
                done: run.done,
                total: run.total,
                outcome: run.outcome.clone(),
            })
    }

    /// Записать результат прогона.
    pub fn finish_result(&self, result: CheckResult) {
        self.finish(
            result.report.to_text(),
            result.report.passed_count(),
            result.report.total(),
            result.saved.is_ok(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_start_is_refused_until_the_first_finishes() {
        let state = CheckState::new();

        assert!(state.try_start(PRIMARY_SLOT, 22));
        assert!(
            !state.try_start(FALLBACK_SLOT, 22),
            "one check per process, even for another slot"
        );
        assert!(state.current().expect("the running check").running());

        state.finish("отчёт".to_owned(), 22, 22, true);
        assert!(
            state.try_start(FALLBACK_SLOT, 22),
            "the finished one is replaced"
        );
    }

    #[test]
    fn progress_stops_at_the_end_and_after_finish() {
        let state = CheckState::new();
        state.try_start(PRIMARY_SLOT, 22);
        state.advance(2, 22);

        let snapshot = state.current().expect("the running check");
        assert_eq!(snapshot.done, 2);
        assert_eq!(snapshot.total, 22);

        state.finish("отчёт".to_owned(), 22, 22, false);
        state.advance(0, 22);

        let snapshot = state.current().expect("the finished check");
        assert!(!snapshot.running());
        assert_eq!(snapshot.done, 22);
        let Some(CheckOutcome::Report { saved, .. }) = snapshot.outcome else {
            panic!("a finished check carries the report");
        };
        assert!(!saved);
    }

    #[test]
    fn a_failed_check_keeps_its_message() {
        let state = CheckState::new();
        state.try_start(PRIMARY_SLOT, 22);
        state.fail("проверка уже идёт");

        let snapshot = state.current().expect("the failed check");
        assert!(!snapshot.running());
        let Some(CheckOutcome::Failed { message }) = snapshot.outcome else {
            panic!("a failed check carries the message");
        };
        assert_eq!(message, "проверка уже идёт");
    }

    #[test]
    fn slots_round_trip_through_their_slugs() {
        for slot in CheckSlot::ALL {
            assert_eq!(CheckSlot::from_slug(slot.slug()), Some(slot));
        }
        assert_eq!(CheckSlot::from_slug("other"), None);
    }

    #[tokio::test]
    async fn a_panic_in_the_background_task_interrupts_the_run() {
        let state = CheckState::new();
        assert!(state.try_start(PRIMARY_SLOT, 22));
        state.advance(3, 22);

        let watcher = state.spawn(async {
            panic!("the substitute backend failed");
        });
        watcher.await.expect("the watcher task joins");

        let snapshot = state.current().expect("the run is kept");
        assert!(
            !snapshot.running(),
            "a crashed task must not leave the run running forever"
        );
        let Some(CheckOutcome::Failed { message }) = snapshot.outcome else {
            panic!("an interrupted run carries a failure");
        };
        assert!(message.contains("прервана"), "{message}");
    }
}
