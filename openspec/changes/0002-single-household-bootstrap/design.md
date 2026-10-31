# Design

## Context

Мотивация описана в proposal.md (раздел «Why»), требования — в дельтах спек.
Ниже только то, что в текущем коде влияет на подход.

- Household создаётся в двух местах:
  - `households::create_empty`: CLI, без владельца;
  - `households::onboard` → `create_with_owner`: бот.

  `onboard` блокирует Telegram ID (`users::lock_telegram_user`) и идемпотентен
  по нему. На состояние инсталляции он не смотрит.
- Бот запускает онбординг из нескольких мест (`onboarding::on_start`,
  `bot/mod.rs`, `forms/actions.rs`). Все они идут через
  `forms::start_onboarding`. Онбординг состоит из нескольких шагов (название →
  пояс) и заканчивается вызовом `onboarding::create`.
- В БД есть `users_telegram_active_idx` и `users_household_owner_idx`.
  Ограничения на число household нет. `admin::select_household` уже отказывает
  при 2+ household без `--household`.
- `members::delete_household` уже делает мягкое удаление с отзывом токенов и
  приглашений. Он требует владельца-актора и подтверждения названием.
- Outbox затирает `payload` в состояниях `sent`, `skipped` и `dead`
  (`outbox.rs`). Доставка at-least-once.
- Экраны (`Screen`) хранятся в `telegram_views`, действия кнопок — в
  `callback_tokens` со scope `screen` или `notice`.

## Goals / Non-Goals

**Goals:**

- Одна функция создания household для бота и CLI. Режим и единственность
  проверяются в ней, а не в вызывающем коде.
- Ветвление первого контакта в боте — чистая функция от состояния инсталляции.
  Её можно покрыть юнит-тестами без Telegram.
- Ни одного нового места, где открытое значение API-токена попадает в базу.

**Non-Goals:**

- Миграции схемы и хранение режима в БД.
- Индекс единственности на уровне БД.
- Любая логика `multi` сверх «как сейчас» и обязательного `--household`.

## Decisions

### D1. Единственность — advisory lock в сервисе, а не индекс

Единая точка `households::create(conn, policy, NewHousehold { owner:
Option<NewOwner>, .. })` внутри транзакции делает следующее:

1. Если владелец указан, берёт блокировку его Telegram ID (как сейчас).
2. Берёт `pg_advisory_xact_lock(<константа "tasklerq:installation">)`.
3. В режиме `single` считает активные household (`deleted_at IS NULL`) и при
   `count >= 1` возвращает `DomainError::Conflict` с `reason =
   "household_exists"`.
4. Вставляет household, списки и (если указан) владельца, пишет аудит.

`create_empty` и `create_with_owner` становятся приватными вариантами или
сливаются. `onboard` сохраняет идемпотентность по Telegram ID и вызывает
`create`.

Порядок блокировок везде один: сначала Telegram ID, потом installation. Так нет
взаимоблокировки с `onboard` и `user add`.

*Альтернативы:*

- Частичный уникальный индекс `ON households ((true)) WHERE deleted_at IS NULL`
  ломает `multi`. Переключать его миграцией по режиму хрупко.
- Таблица-синглтон `installation` с режимом. Она защищает от переключения
  `single → multi → single`, но требует миграции, а польза мала: деградация
  (D4) и так покрывает это состояние.

### D2. Политика размещения передаётся явно

В `Config` добавляется `tenancy: TenancyConfig { mode: TenancyMode,
bootstrap_owner: Option<i64> }`. Значение пробрасывается:

- в бот — как новое поле `Ctx`;
- в admin-функции — параметром.

Репозиторий получает `TenancyMode` аргументом и не читает окружение.
`TenancyMode::parse` устроен по образцу `TelegramMode::parse`.

Предупреждения при старте (`BOOTSTRAP_OWNER_TELEGRAM_ID` не задан при
включённом Telegram, деградация из D4) пишутся из `app.rs` после подключения к
БД.

### D3. Первый контакт — классификатор состояния

Чистая функция:

```
classify(mode, bootstrap_owner, sender_id, installation) -> FirstContact
  installation: Empty | Single { has_owner } | Multiple
  FirstContact: Onboard | AskInvite | AdminCommand(sender_id)
              | NotConfigured | NeedsAdmin
```

Таблица для `single`:

| installation | bootstrap_owner | Результат |
|---|---|---|
| Empty | не задан | Onboard |
| Empty | = sender | Onboard |
| Empty | ≠ sender | NotConfigured |
| Single, owner есть | любой | AskInvite |
| Single, owner нет | не задан или = sender | AdminCommand(sender) |
| Single, owner нет | ≠ sender | NotConfigured |
| Multiple | любой | NeedsAdmin |

В `multi` результат всегда `Onboard`.

`installation` читается одним запросом: число активных household и наличие
владельца, если household один. `forms::start_onboarding` вызывает классификатор
и либо открывает форму, либо показывает информационный экран без формы. Так
все точки входа получают ветвление автоматически.

На последнем шаге (`onboarding::create`) `Conflict(household_exists)` из D1
ловится через savepoint, как в `accept_invite`. После этого состояние
перечитывается, бот показывает экран по классификатору, а форма закрывается.

Ветка `AdminCommand` при `bootstrap_owner ≠ sender` превращается в
`NotConfigured`. Посторонний не должен получать даже безобидную подсказку, если
администратор явно указал ожидаемого владельца.

### D4. Деградация — вычисляемое состояние, а не флаг

Флаг «degraded» нигде не хранится:

- классификатор возвращает `NeedsAdmin`, когда household больше одного;
- `create` из D1 отказывает при `count >= 1`;
- `/health/ready` в `single` делает `SELECT count(*) FROM households WHERE
  deleted_at IS NULL` и при значении больше 1 добавляет
  `warnings: ["multiple_households"]`, оставляя статус 200;
- `app.rs` один раз при старте пишет error-лог с числом household.

Состояние исчезает само, как только администратор удаляет лишнее, без
перезапуска.

*Альтернатива — fail-fast при старте.* Отклонена: после обновления упали бы HA
и API на рабочей инсталляции (см. proposal).

### D5. CLI

- `household create`:
  - флаги `--owner-telegram-id` и `--owner-name`;
  - вызывает `households::create` из D1;
  - без владельца печатает подсказку
    `tasklerq admin user add --telegram-id <id> --role owner --household <uuid>`.
- `household list` выводит одну строку на household: id, название, пояс,
  Telegram ID владельца или `-`, число участников, число открытых задач, число
  активных токенов. Агрегаты считаются одним запросом с подзапросами.
- `household delete --household <id> [--yes]`:
  - без `--yes` спрашивает подтверждение на stdin (ввести название);
  - в неинтерактивном режиме без `--yes` отказывает.

  Логика мягкого удаления выносится из `members::delete_household` в общую
  функцию `delete_household_unchecked(conn, household_id, actor, now)`.
  Владельческая версия делает проверки и вызывает её. Admin-версия вызывает её
  с `admin_actor()`. Уведомления участникам отправляет только владельческий путь
  через бот: у CLI нет outbox-контекста, и при починке инсталляции уведомлять
  некого.
- `resolve_household` получает `TenancyMode`. В `multi` без `--household`
  возвращается `Validation("pass --household")` даже при одном household.

### D6. Экран «Интеграции» и одноразовый показ токена

- Новые `Screen::Integrations` и `Screen::ConfirmRevokeToken { token_id }`.
  Кнопка «🔌 Интеграции» появляется в настройках только у владельца.
- Список берётся из `tokens::list` с фильтром «не отозван и не истёк».
- Отзыв:
  - проверяет `users::require_current_owner`;
  - сначала проверяет `household_id` токена, а потом отзывает. Чужой токен даёт
    `NotFound` (защита от IDOR);
  - пишет аудит с актором-владельцем через тот же путь, что
    `auth::tokens::revoke`.
- Выпуск: `auth::tokens::issue(.., name = "Home Assistant", scopes =
  [ha:read, ha:write, capture:write], expires_at = None, actor = owner)`.
- Открытое значение уходит **отдельным** `Outgoing::Message { rich: true }`.
  Текст — `<tg-spoiler>tlq_…</tg-spoiler>`, ниже кнопка «Я сохранил».
  - Кнопка — callback со scope `notice` и действием `Action::DismissSecret`
    без данных. Её обработчик удаляет сообщение, из которого пришёл callback.
  - Если `deleteMessage` вернул ошибку (сообщение старше 48 часов), сообщение
    редактируется на «Токен скрыт».
  - Затем экран «Интеграции» перерисовывается.
- Значение токена не попадает в `Screen`, в `callback_tokens`, в
  `conversation_states` и в логи. В outbox оно лежит только до отправки, затем
  `payload` затирается существующим механизмом.

*Альтернатива — показать токен на экране навигации и стереть редактированием.*
Отклонена: экран сохраняется в `telegram_views`, а значит, открытое значение
попало бы в базу.

### D7. Документация

`docs/deployment.md`, раздел «Первый запуск»:

- основной путь — через бот;
- `BOOTSTRAP_OWNER_TELEGRAM_ID` рекомендован;
- CLI-путь — `household create --owner-telegram-id` для headless-режима и
  bootstrap;
- отдельный runbook «два household после обновления».

`docs/home-assistant.md`: выпуск и перевыпуск токена через «Настройки →
Интеграции», CLI как альтернатива. В таблицу переменных добавляются
`TENANCY_MODE` и `BOOTSTRAP_OWNER_TELEGRAM_ID`.

## Risks / Trade-offs

- [Окно гонки на пустой инсталляции без `BOOTSTRAP_OWNER_TELEGRAM_ID`] →
  предупреждение в логе при старте, рекомендация в документации и bootstrap
  через CLI одной командой.
- [At-least-once доставка может отправить сообщение с токеном дважды] → каждая
  копия несёт свою кнопку «Я сохранил». Риск такой же, как у остальных
  сообщений. Владелец видит дубль и удаляет его той же кнопкой.
- [Токен выпущен, но сообщение умерло в outbox] → токен остаётся активным и
  виден в «Интеграциях» по префиксу, владелец отзывает его вручную.
  Автоматический отзыв не делаем: это связало бы outbox с доменом токенов.
- [Переключение `single → multi → single` оставляет 2+ household] → D4 переводит
  инсталляцию в деградацию, а `household list/delete` позволяет починить. Это
  осознанный компромисс вместо хранения режима в БД.
- [Новая ветка «пространство уже есть» меняет поведение для существующих
  инсталляций, где кто-то рассчитывал на самостоятельный онбординг] → такой
  сценарий остаётся в `TENANCY_MODE=multi`. Изменение помечено **BREAKING** в
  proposal.
- [`count(*)` по households в `/health/ready` на каждый запрос] → таблица
  крошечная. При необходимости проверку можно кешировать.

## Migration Plan

1. Развернуть новую версию без изменения окружения. Режим по умолчанию
   `single`, миграций нет.
2. На инсталляции с двумя household сервис стартует в деградированном режиме:
   error в логе, `warnings` в `/health/ready`.
3. Починить:
   1. `tasklerq admin household list` — найти household с владельцем и задачами;
   2. выпустить для него токен через «Настройки → Интеграции» или
      `admin token create --household <id>`;
   3. переподключить HA (reauth с новым токеном);
   4. `tasklerq admin household delete --household <лишний> --yes`.
4. Задать `BOOTSTRAP_OWNER_TELEGRAM_ID` в `.env` для будущих переустановок
   (необязательно).

Откат: предыдущий образ работает с той же схемой. Мягко удалённый household
восстанавливается вручную (`UPDATE households SET deleted_at = NULL`, то же для
участников) до срока retention. Отозванные токены не восстанавливаются,
выпускаются новые.

Архивировать изменение после `0001-foundation`: дельты содержат `MODIFIED`
требований, которые появятся в `openspec/specs/` только после архивации 0001.
