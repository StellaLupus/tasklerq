# Tasks

## 1. Тестовые хелперы

- [x] 1.1 Собрать `LogBuffer` с `MakeWriter` в `tests/support` и перевести на него `tests/capture_api_logs.rs`, `tests/degradation_logs.rs`, `tests/llm_logs.rs`, `tests/observability_logs.rs`, `tests/outbox_dead_logs.rs`, `tests/webhook_rate_limit_logs.rs`, `tests/secret_redaction.rs`. `FailureLogs` в `tests/load_webhook.rs` и копии в `src/app.rs`, `src/config/tests.rs` не трогать. Проверить: `cargo test` проходит, `grep -ln "struct LogBuffer" tests/*.rs` пуст.
- [x] 1.2 Заменить в `tests/notification_actions.rs` копии `tick_and_deliver` и `button_of` версиями из `tests/support/notifications.rs`. Проверить: `cargo test --test notification_actions` проходит.
- [x] 1.3 Собрать в хелперы копии в тестах: подготовку «карточка задачи поверх прямого добавления с черновиком» (три теста отмены в `tests/telegram_capture.rs`) и триггер, ломающий вставку сводки (`tests/notifications.rs`, `tests/scheduler_step_logs.rs`). Проверить: `cargo test --test telegram_capture --test notifications --test scheduler_step_logs` проходит.

## 2. Код

- [x] 2.1 Вынести общую функцию добавления частей из `finish_direct_split` (`src/telegram/capture_worker.rs`) и `intake_split` (`src/telegram/bot/capture/mod.rs`) (design, решение 1). Проверить: `tests/telegram_capture.rs`, `tests/capture_fallback.rs` и тесты экрана покупок проходят без изменений.
- [x] 2.2 Ввести `MAX_INPUT_QUANTITY` в `src/domain/validation.rs` и использовать её вместо `MAX_QUANTITY` в `src/telegram/bot/model.rs` и `src/capture/validate.rs` (design, решение 2). Проверить: `cargo test` проходит, `grep -rn "9_999.0" src` находит одно определение.
- [x] 2.3 Заменить `expect` в `src/jobs/batch.rs` на `let … else` при проверке вида и сохранить `scope` в `BatchCandidate` (design, решение 3). Проверить: `cargo test --test notification_batches` проходит, `grep -n "expect(" src/jobs/batch.rs` пуст.
- [x] 2.4 Исправить комментарий поля `idempotency_key` в `CaptureRequest` (`src/http/api/capture/mod.rs`): при расхождении заголовка и поля ответ `422`, как в `idempotency.rs`. Миграцию `0007` не трогать. Удалить `JournalPage::has_next` (`src/storage/repositories/items/completions.rs`) и его вызовы в `tests/item_lifecycle.rs`. Проверить: `cargo test --test item_lifecycle` проходит.
  - Доделано по ревью: doc-комментарий `resolve_idempotency_key` тоже без «приоритета».
- [x] 2.5 Убрать ветки `if rows.is_empty()` при сборке баннера прямого добавления (`Banner::with_button_rows` уже отбрасывает пустые ряды) в `shopping.rs`, `capture_worker.rs`, `bot/capture/mod.rs` и строить ряд «Открыть» черновика без `addition_rows(&[], Uuid::nil(), …)`. Проверить: тесты прямого добавления и отмены проходят без изменений, `grep -n "Uuid::nil()" src/telegram/bot/shopping.rs` пуст.
- [x] 2.6 Показ карточки `Screen::Item { item_id, back, completion_id: None }` в `set_due`, `apply_due` и `finish_due` (`src/telegram/bot/card.rs`) — одной функцией вместо четырёх копий. Проверить: тесты карточки и сроков проходят без изменений.

## 3. Недостающие тесты

- [x] 3.1 Добавить тесты: `undo_last` после возврата выполнения из журнала отказывает (закрепляет текущее поведение: событие `reopened` с `undo_of` повторно не отменяется); блок «Выполнения» в карточке показывает не больше пяти строк (`telegram-rich-ui`, «Блок выполнений в карточке»). Проверить: тесты проходят.
- [x] 3.2 Брать ожидаемое время в `notification_batch_actions::snooze_in_a_block_creates_one_delivery_and_an_ordinary_reminder_later` от времени записи в базе или от интервала часов стенда до и после нажатия (design, решение 4). Проверить: `cargo test --test notification_batch_actions -- --exact snooze_in_a_block_creates_one_delivery_and_an_ordinary_reminder_later` выводит `1 passed`; рассуждение в комментарии теста объясняет, почему сдвиг часов стенда больше не влияет.
- [x] 3.3 Добавить юнит-тест в `src/jobs/quiet.rs`: тихие часы в `Europe/Berlin` с концом в 02:30 на 2026-03-29 дают конец интервала в 03:00 по местному времени. Проверить: тест проходит.

## 4. Нагрузочный тест

- [ ] 4.1 После мержа 0007 прогнать `make load` на релизной сборке, обновить таблицы `docs/deployment.md` §18 (дата, p99, queue pending, drain) и пункт про `failed=1` в §20. Если тест красный, сначала починить его под текущие `deferred`/`dropped`. Проверить: в §18 нет пометки «исторический прогон до изменений 3.3–3.4».

## 5. Итоговая проверка

- [x] 5.1 Прогнать `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` и `cargo test`, все должны пройти. Затем `openspec validate 0008-code-debt --strict` должен пройти без ошибок.

## Workflow follow-up

- Открыть PR → `main` после мержа 0007, задеплоить на NAS.
