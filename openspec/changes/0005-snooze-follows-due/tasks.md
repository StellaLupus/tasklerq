# Tasks

## 1. Подготовка ветки

- [ ] 1.1 Создать ветку `fix/0005-snooze-follows-due` от `main`. На `main` с 2026-11-06 падают два теста в `src/domain/recurrence.rs`, зависящие от текущей даты: их исправил коммит `bcc32e1` на `feat/0004-phase-a`. Перенести из него только правку этих тестов. Проверить: `cargo test --lib domain::recurrence` проходит.

## 2. Ключ исходного вхождения в записи доставки

- [ ] 2.1 Добавить `migrations/0012_snooze_origin.sql` с `ALTER TABLE notification_deliveries ADD COLUMN origin_key text;` и с комментарием о том, что `NULL` у snooze означает запись до миграции (design, решения 1 и 5). Проверить: `tests/migrations.rs` проходит на чистой базе, и новый тест применяет 0012 поверх существующей pending-записи snooze, после чего у неё `origin_key IS NULL`.
- [ ] 2.2 Добавить в `src/jobs/keys.rs` функцию `item_occurrence_key(due_at, due_date) -> Option<String>` и перевести на неё `NoticeRow::current_key()` (design, решение 3). Проверить юнит-тестами в `keys.rs`: timed-срок даёт тот же ключ, что `occurrence_key()`, вседневный — `date:YYYY-MM-DD`, ни одного или оба срока — `None`.
- [ ] 2.3 Добавить `origin_key: Option<&str>` в `NewDelivery` и записывать его в `insert_with_flags()` (`src/storage/repositories/notifications.rs`). Все существующие вызовы передают `None`. Обновить описание `notification_deliveries` в `docs/domain-model.md`: колонка `origin_key` и новое правило отмены snooze. Проверить: `cargo build` и существующие тесты `tests/notifications.rs` проходят.

## 3. Snooze следует за сроком

- [ ] 3.1 Передавать в `insert_snooze()` загруженную задачу и записывать `origin_key = item_occurrence_key(item.due_at, item.due_date)`. Обновить вызовы в `snooze()` и `snooze_batch_item()`. Проверить тестом в `tests/notification_actions.rs`: после «Отложить на час» у timed- и у вседневной задачи `origin_key` записи совпадает с ключом напоминания по этому сроку.
- [ ] 3.2 Добавить `d.origin_key` в `NOTICE_ROW_COLUMNS` и `NoticeRow`. В `reminder_stale()` заменить `return false` для snooze на правило из design (решение 2): `NULL` не устаревает, `current_key() != origin_key` устаревает. Переписать doc-комментарий `is_stale()` в `src/jobs/notice.rs`. Проверить тестами в `tests/notification_actions.rs` сценарии «Срок изменён после snooze» (запись `cancelled`, напоминание о новом сроке приходит в срок минус lead time), «Срок убран после snooze» и «Повторяющаяся задача выполнена после snooze» из дельты `notifications-reminders`.
- [ ] 3.3 Проверить, что snooze при прежнем сроке не ломается: тесты сценариев «Snooze до утра при прежнем сроке» (с подменой `now` через полночь) и «Snooze, перенесённый тихими часами». Добавить тест на запись snooze с `origin_key = NULL`: она доставляется, хотя срок задачи изменился. Существующие `snooze_*` в `tests/notification_actions.rs`, `tests/notification_batch_actions.rs` и `tests/notification_manual_release.rs` должны проходить без изменения ожиданий.

## 4. Версия в кнопках snooze

- [ ] 4.1 Добавить `#[serde(default)] version: Option<i32>` в `Action::Snooze` и `Action::BatchSnooze`, отдавать её из `Action::version()`. Записывать `Some(item.version)` в `reminder_ui()` и `Some(entry.version)` в кнопку «😴 На час» в `notify/batch.rs`. Передать версию через диспетчер `src/telegram/bot/mod.rs`. Проверить юнит-тестом: JSON `Snooze` и `BatchSnooze` без `version` разбирается в `None`, а новый payload кнопок содержит версию.
- [ ] 4.2 В `snooze()` после проверок «недоступна» и «уже выполнена» при `Some(v) != item.version` показывать тост «Задача уже изменилась», не вставлять snooze и перерисовывать напоминание через `show_current()`. Проверить тестами в `tests/notification_actions.rs` сценарии «Отложить в устаревшем напоминании» (доставок snooze нет, сообщение перерисовано) и «Отложить по новой версии».
- [ ] 4.3 В `batch_snooze()` при устаревшей версии показывать тост «Задача уже изменилась», не вставлять snooze, не менять `batch_state` и перерисовывать блок через `render_batch()`. «Отложить все на час» вызывает `snooze_batch_item()` без версии. Проверить тестами в `tests/notification_batch_actions.rs` сценарий «На час в устаревшем блоке сводного сообщения» и то, что `snooze_all_*` проходят как раньше.

## 5. Итоговая проверка

- [ ] 5.1 Прогнать `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` и `cargo test`, все должны пройти. Затем `openspec validate 0005-snooze-follows-due --strict` должен пройти без ошибок.

## Workflow follow-up

- Открыть PR `fix/0005-snooze-follows-due` → `main`, после мержа задеплоить на NAS.
- После мержа сдвинуть номер миграции этапа C в `0004-telegram-ux-rework` (`tasks.md` 9.1, `design.md` Migration Plan) с `0012` на `0013`.
- Архивировать изменение после ревью и деплоя.
