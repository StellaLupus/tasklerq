# Proposal

## Why

В «Известных ограничениях» PR 2–20 записано несколько мест, где код
реально мешает. Есть копии, которые меняются вместе: около 55 строк
добавления частей совпадают почти дословно в двух местах, и любая правка
правил дубликатов или валидации нужна в обоих; хелпер захвата логов
скопирован в семь тестов. Есть константа предела количества, продублированная в двух слоях.
Есть устаревший комментарий, недостающие и плавающий тесты и нагрузочный
тест, который не перезапускали после 0003. Остальные пункты тех же PR при
проверке оказались эстетикой, совпадением значений или вредной правкой. Они
перечислены в design (Non-Goals) с причинами. Change идёт после 0007:
оба правят `capture_worker.rs`.

## What Changes

**Код**
- `finish_direct_split` (`capture_worker.rs`) и `intake_split`
  (`bot/capture/mod.rs`) используют одну функцию добавления частей. Около 55
  совпадающих строк остаются в одном месте.
- Один предел количества для ввода (`9999`) в `domain::validation` вместо
  копий в `bot/model.rs` и `capture/validate.rs`. Значения не меняются.
- `expect("batch candidates are never digests")` в `jobs/batch.rs` заменяется
  на `let … else` там, где вид уже проверяется.
- Комментарий поля `idempotency_key` в `CaptureRequest` исправлен: при
  расхождении заголовка и поля ответ `422`, а не «заголовок имеет
  приоритет».
- Удаляется `JournalPage::has_next`, который вызывают только тесты.

**Тесты**
- Хелпер захвата логов собирается в `tests/support` вместо семи копий.
- Точные копии `tick_and_deliver` и `button_of` в
  `tests/notification_actions.rs` заменяются версиями из `tests/support`.
- Тесты на отказ `undo_last` после возврата выполнения и на предел пяти строк
  в блоке «Выполнения».
- Плавающий тест
  `snooze_in_a_block_creates_one_delivery_and_an_ordinary_reminder_later`
  перестаёт зависеть от сдвига часов стенда между чтением времени и
  нажатием.
- Тест, закрепляющий существующее поведение: конец тихих часов на переходе на
  летнее время сдвигается на первую существующую минуту.
- `make load` после 0007, цифры §18 и пункт `failed=1` в §20
  `docs/deployment.md` обновлены.

## Capabilities

### New Capabilities

<!-- Нет. -->

### Modified Capabilities

<!-- Нет: поведение не меняется, `skip_specs: true`. -->

## Impact

- Код:
  - `src/telegram/capture_worker.rs`, `src/telegram/bot/capture/mod.rs`;
  - `src/domain/validation.rs`, `src/telegram/bot/model.rs`,
    `src/capture/validate.rs`;
  - `src/jobs/batch.rs`, `src/http/api/capture/mod.rs`;
  - `src/storage/repositories/items/completions.rs`.
- Тесты:
  - `tests/support`;
  - семь `*_logs.rs` и `tests/secret_redaction.rs`;
  - `tests/notification_actions.rs`, `tests/notification_batch_actions.rs`,
    `tests/item_lifecycle.rs`;
  - тесты карточки, `src/jobs/quiet.rs`.
- Документация: `docs/deployment.md` §18 и §20.
- Внешнее поведение, HTTP API и схема БД не меняются.
