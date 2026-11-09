# Tasks

## 1. Telegram

- [x] 1.1 В `present_to()` (`src/telegram/bot/mod.rs`) при `Fail::User` повторять показ с `Screen::Menu` и тем же баннером (design, решение 1). Проверить тестом сценария «Список удалён, пока шёл разбор»: в outbox меню с баннером результата и его кнопками.
- [x] 1.2 Добавить `#[serde(default)] draft_id` в `Action::AddedUndo` и передавать его при сборке баннера; после отмены показывать «Открыть». `present_after_undo()` вызывает `present_after_duplicate()`. В действия выбора дубликата добавить позиции исходного добавления (`#[serde(default)]`), чтобы следующий баннер сохранял «↩️ Отменить» (design, решение 2). Проверить тестами сценариев «Отмена над карточкой задачи» и «Отмена после выбора по дубликату» и тестом разбора JSON старых `AddedUndo` и действия выбора без новых полей.
- [x] 1.3 В `set_due` (`src/telegram/bot/card.rs`) у задачи без срока на «Без срока» показывать карточку без баннера и без `items::update` (design, решение 3). Проверить тестом сценария «Срока не было»: в outbox карточка без «Срок снят», `version` не изменилась.

## 2. Capture

- [ ] 2.1 В обработчике `src/http/api/capture/mod.rs` проверять длину по `sanitize(text).trim()` до квоты и LLM (design, решение 6). Проверить тестом в `tests/capture_api.rs`: `text` из `\u0007\u0008` → `422`, фейковый LLM не вызван, позиций нет, `llm_runs` пуст.

## 3. Уведомления

- [ ] 3.1 В `Scheduler::tick()` (`src/jobs/scheduler.rs`) выполнять шаги по отдельности, ошибку каждого писать с `step`, возвращать `Err` последней ошибки, если упал хотя бы один шаг (design, решение 5). Проверить тестом в `tests/notifications.rs`: триггер `BEFORE INSERT ON notification_deliveries WHEN (NEW.kind = 'digest')` с `RAISE EXCEPTION`, тик возвращает `Err`, но напоминание, срок которого наступил, поставлено в outbox. Удалить триггер в конце теста.
- [ ] 3.2 В `reminder_stale()` (`src/jobs/notice.rs`) считать snooze без `origin_key` устаревшим (design, решение 4). Убрать упоминания переходного правила в комментариях и в `docs/domain-model.md`. Заменить тесты 0005 «NULL до миграции не устаревает» тестом: pending snooze без `origin_key` после тика `cancelled` и не доставлен.

## 4. Итоговая проверка

- [ ] 4.1 Прогнать `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` и `cargo test`, все должны пройти. Затем `openspec validate 0007-correctness-debt --strict` должен пройти без ошибок.

## Workflow follow-up

- Открыть PR → `main`, после мержа задеплоить на NAS.
