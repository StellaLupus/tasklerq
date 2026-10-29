## Context

Tasklerq создаётся с нуля: в репозитории пока нет прикладного кода, только
OpenSpec-артефакты и проектные документы. Спецификации поведения находятся в
`openspec/changes/0001-foundation/specs/`; мотивация — в `proposal.md`.

Ключевые ограничения, формирующие решение:

- **Rust** как язык сервиса; **PostgreSQL** уже есть на сервере пользователя.
- **Telegram webhook** через публичный HTTPS-домен; long polling допустим
  только локально.
- **RichMessage** — молодой API Telegram Bot API. `teloxide` 0.17 не содержит
  типов RichMessage, поэтому транспорт нужно изолировать.
- **Home Assistant Container** (Docker): нет Supervisor, add-on и ingress.
  MQTT discovery не умеет `todo`/`calendar`, поэтому нужен custom component.
- **LLM — Google Gemini через Home Assistant AI Task** (`ai_task.generate_data`).
  Прямой вызов Gemini из Rust не планируется.
- **Capture без Telegram обязателен**: задачи добавляются голосом через HA
  Assist, автоматизацией и HTTP API; бот не должен быть единственной точкой
  входа.
- **Семейный масштаб**: один экземпляр, до ~10 пользователей, до ~10 000
  активных задач. Микросервисы и брокер сообщений не оправданы.
- **Русский UI**, английские идентификаторы, схема БД и API.

## Goals / Non-Goals

**Goals:**

- Определить границы модулей и потоков данных так, чтобы Telegram, LLM, Home
  Assistant и PostgreSQL развивались независимо.
- Обеспечить надёжную обработку webhook и исходящих сообщений без потери и
  дублей.
- Сделать LLM безопасным помощником: только разбор, только черновик, только
  после подтверждения.
- Дать Home Assistant нативные `todo`/`calendar` без MQTT-костылей.
- Дать создание задач без Telegram: голосом через HA Assist, автоматизацией и
  HTTP API, с идемпотентностью и понятным голосовым ответом.
- Уложиться в инфраструктуру пользователя: существующий PostgreSQL, reverse
  proxy, Docker, HA Container.
- Оставить путь развития без переписывания: веб-UI, CalDAV, несколько
  household, более сложные повторения.

**Non-Goals:**

- Реализация кода в рамках этого изменения.
- Веб-интерфейс, мобильное приложение, OAuth, публичный API.
- CalDAV/ICS и нативная синхронизация с телефонным календарём.
- Голосовые сообщения Telegram и собственное распознавание речи (голосовой
  capture через HA Assist входит в MVP), групповые чаты, несколько household на
  пользователя.
- Сложные правила повторения с исключениями и редактированием отдельного
  вхождения.
- HA add-on/ingress, Kubernetes, горизонтальное масштабирование.

## Decisions

### D1. Модульный монолит на Rust

Один бинарник `tasklerq` на axum + tokio + sqlx. Внутри — модули с явными
границами, но без сетевого разделения. Фоновые воркеры запускаются в том же
процессе как tokio-задачи и используют PostgreSQL как очередь.

```text
Telegram ──HTTPS──▶ reverse proxy ──▶ axum /webhooks/telegram
                                           │
                                           ▼
                                  telegram_updates (PostgreSQL)
                                           │  FOR UPDATE SKIP LOCKED
                                           ▼
                    ┌──────────────┬───────────────┬──────────────┐
                    │ Telegram     │ Domain         │ Scheduler    │
                    │ worker       │ services       │ / outbox     │
                    └──────┬───────┴───────┬────────┴──────┬───────┘
                           │               │               │
                           ▼               ▼               ▼
                    Telegram API      PostgreSQL      Home Assistant
                                           ▲
                                           │ REST API + Bearer
                                    HA custom component
```

| Модуль | Ответственность |
| --- | --- |
| `config` | Чтение и валидация окружения, секреты, версия. |
| `http` | Роутер, middleware, request ID, ошибки, health, API v1. |
| `auth` | API-токены, scopes, проверка household, роли. |
| `telegram` | Webhook, очередь, транспорт, RichMessage, callback-токены, экраны. |
| `domain` | Household, списки, задачи, покупки, повторения, события, поиск. |
| `capture` | Оркестрация LLM-разбора, черновики, валидация, подтверждение. |
| `home_assistant` | Клиент HA REST, вызов `ai_task.generate_data`, проверка сущности. |
| `storage` | Репозитории sqlx, транзакции, миграции, запросы очередей. |
| `jobs` | Воркеры обновлений, outbox, напоминания, retention, purge. |
| `admin` | CLI для токенов, очередей, экспорта и диагностики. |

**Почему не микросервисы.** Семейный масштаб не требует независимого
масштабирования; сетевые границы добавили бы аутентификацию, трассировку,
деплой и отладку без пользы.

**Почему не Python/Node.** Rust выбран пользователем; он даёт предсказуемый
бинарник, строгую типизацию домена и хорошую производительность при обработке
очередей.

**Альтернативы.** Отдельный worker-сервис (лишняя инфраструктура); serverless
(плохо для постоянных воркеров и webhook); встроенная БД (исключена
требованием PostgreSQL).

### D2. Модель данных: единые `items` и `lists`

Домен разделён на household, списки и элементы. Задачи и покупки используют
одну таблицу `items`, потому что у них общие операции: создание, редактирование,
завершение, мягкое удаление, поиск, события, назначение и синхронизация с HA.
Различия вынесены в `lists.kind` и nullable-поля.

```text
households 1─* users
households 1─* lists 1─* items 1─* subtasks
items *─* tags (item_tags)
items 1─* item_events
households 1─* invites
households 1─* api_tokens
households 1─* capture_drafts 1─* llm_runs
```

| Таблица | Назначение |
| --- | --- |
| `households` | Название, часовой пояс, soft delete. |
| `users` | Telegram ID, чат, имя, роль, настройки, timezone. |
| `lists` | `kind` (`tasks`/`shopping`/`checklist`), имя, default, archive, position. |
| `items` | Задача/позиция: title, notes, status, due, priority, assignee, quantity, recurrence, version, search, `source`, `capture_id`. |
| `subtasks` | Подзадачи задачи, статус, позиция. |
| `tags`, `item_tags` | Теги household и связь с элементами. |
| `item_events` | Аудит и отмена: actor, type, payload, undo_of. |
| `invites` | Хеш кода, TTL, роль, использование, отзыв. |
| `api_tokens` | Хеш токена, prefix, scopes, expiry, last_used, revoke. |
| `telegram_updates` | Очередь входящих обновлений, `update_id` PK, attempts, available_at. |
| `telegram_views` | Активное сообщение экрана, view_key, состояние. |
| `conversation_states` | Пошаговая форма пользователя, TTL. |
| `callback_tokens` | Хеш одноразового callback-токена, действие, payload, version, TTL. |
| `capture_drafts` | Исходный текст, структура LLM, статус, TTL, `source`, `mode`, `idempotency_key`, `capture_id`; хранит и черновики, и применённые capture. |
| `llm_runs` | Метаданные разбора без исходного текста. |
| `outbox` | Исходящие сообщения, retry, message_id. |
| `notification_deliveries` | Дедупликация напоминаний и сводок. |
| `audit_events` | Членство и безопасность: `household_id` (NULL для системных), актор, тип, payload без секретов. |

**Идентификаторы.** UUIDv7: сортируемость по времени и безопасность в URL.
Telegram `update_id` — bigint.

**Ключевые ограничения:**

- `items.due_at` и `items.due_date` взаимоисключающие; оба nullable.
- `items.recurrence_freq` задан только для `lists.kind = 'tasks'`.
- `quantity`/`unit` используются для `shopping` и `checklist`; для `tasks`
  игнорируются.
- `items.version` увеличивается при каждом изменении; API требует версию.
- `items.source` — `telegram`/`ha_todo`/`ha_voice`/`ha_automation`/`api`/
  `import`; `items.capture_id` ссылается на `capture_drafts` для внешнего
  capture и очищается при retention, не удаляя элемент.
- `items.created_by_user_id` nullable: у голосового/автоматического capture нет
  Telegram-пользователя; авторство выражается `source` и `item_events.actor_type`
  (`voice`/`home_assistant`/`system`).
- `users.telegram_user_id` уникален среди неудалённых (`deleted_at IS NULL`);
  один пользователь — один household. Удалённый участник может вернуться по
  новому приглашению.
- `lists` уникальны по `(household_id, lower(name))` среди неудалённых.
- `tags` уникальны по `(household_id, lower(name))`.

**Повторения.** Правило хранится на элементе:

```text
recurrence_freq        daily | weekly | monthly | yearly
recurrence_interval    1..365
recurrence_byweekday   smallint[] (ISO 1..7), только для weekly
recurrence_until       date, nullable
recurrence_timezone    IANA
recurrence_anchor_date date — исходная дата серии
recurrence_month_rule  anchor_day | last_day — только для monthly
```

`due_at`/`due_date` — текущее (ближайшее) вхождение. При завершении следующее
вхождение вычисляется от якоря, а не от текущей даты, чтобы не накапливать
дрейф; следующим становится первое вхождение, строго позже и текущего срока, и
момента завершения (пропущенные вхождения просроченной серии не копятся). Для
`monthly` в режиме `anchor_day` (по умолчанию) день берётся из якоря, и если в
месяце такого дня нет, вхождение пропускается; в режиме `last_day` вхождением
всегда служит последний день месяца (28/29/30/31), а срок серии — последний день
месяца указанной даты. Для `yearly` аналогично пропускается 29 февраля в
невисокосный год. Завершённое вхождение сохраняется в `item_events` вместе с
прежним сроком; повторное открытие восстанавливает срок. Развёртка календаря
ограничена 366 днями и 1000 вхождениями на запрос.

**Часовые пояса.** `due_at` — `timestamptz` (UTC). `due_date` — `date`.
Повторения считаются в `recurrence_timezone` по локальному настенному времени;
при переходе DST час сохраняется, несуществующее время сдвигается вперёд.

**Поиск.** Генерируемая колонка `search_vector` (`simple` + `pg_trgm`) по
title/notes; теги и подзадачи добавляются join-запросом. `pg_trgm` даёт
подстрочный поиск для русских словоформ; при недоступности расширения
используется `ILIKE`. Поиск всегда ограничен household и исключает
`deleted_at`, архивные списки и (по умолчанию) выполненные элементы.

**Мягкое удаление.** `items.deleted_at`, `lists.deleted_at`,
`households.deleted_at`. Все запросы фильтруют `deleted_at IS NULL`; отдельный
retention-job удаляет данные через 30 дней (для household — после
подтверждения и 30 дней).

**Транзакции.** Одна транзакция на: применение черновика (все элементы),
прямое применение внешнего capture (все элементы + capture + события),
завершение задачи + событие + следующее вхождение, очистку выполненных,
удаление участника (снятие назначений + отзыв приглашений), подтверждение
приглашения.

### D3. Telegram: транспорт, webhook, RichMessage и состояние

**Транспорт.** `TelegramApi` — trait с методами `send_rich_message`,
`edit_rich_message`, `send_message`, `edit_message_text`,
`answer_callback_query`, `delete_message`, `set_webhook`, `get_webhook_info`.
Реализация `HttpTelegramApi` использует `reqwest`; типы `Update`, `Message`,
`User`, `CallbackQuery` берутся из `teloxide-core`, а RichMessage-структуры
описываются локально. Это позволяет обновлять RichMessage независимо от
`teloxide`.

**RichMessage.** Рендерер собирает HTML с кнопками `<tg-button>` и
`<tg-button-row>`; каждое действие получает callback-токен. Запасного режима с
обычным текстом и inline-клавиатурой нет: RichMessage проверен владельцем на
другом продукте. Точная JSON-схема RichMessage инкапсулирована в
`HttpTelegramApi`; домен и экраны работают с `RenderedView`.

**Webhook.** `POST /webhooks/telegram`:

1. Проверить метод, размер тела (≤1 МБ) и секрет за постоянное время.
2. Разобрать `Update`; при ошибке — `400` без сохранения.
3. Вставить `telegram_updates` с `ON CONFLICT (update_id) DO NOTHING`.
4. Вернуть `200` не позднее 5 секунд. Если запись не удалась — `500`, чтобы
   Telegram повторил доставку.

Воркер забирает задания `FOR UPDATE SKIP LOCKED`, помечает `processing`,
выполняет обработчик, фиксирует `processed`/`failed`. Временные ошибки —
экспоненциальная задержка (1, 2, 4, … до 5 минут), максимум 10 попыток; после
этого — `dead` и метрика. Эффекты идемпотентны на уровне домена.

**Callback-токены.** `callback_data` ограничен 64 байтами. Токен: 12 случайных
байт → base64url (16 символов), префикс `t:`. В БД — SHA-256 хеш, household,
user, action, payload, version, expires_at (≤24 ч), consumed_at. Нажатие
разрешается в транзакции: проверка пользователя → атомарное потребление →
выполнение → ответ `answerCallbackQuery`. Просроченный/повторный токен даёт
toast и обновление экрана.

**Состояние экранов.** `telegram_views` хранит `(chat_id, message_id)`,
`view_key` и JSON состояния; при переходе сообщение редактируется. Если
редактирование невозможно (сообщение удалено/старше лимита), адаптер удаляет
старое и отправляет новое ровно один раз. `conversation_states` хранит одну
активную форму пользователя с TTL 15 минут.

**Исходящие.** Всё, что отправляется вне ответа на callback, идёт через
`outbox`: payload, chat_id, kind, attempts, next_attempt_at, message_id.
Воркер соблюдает 1 сообщение/сек на чат и 30/сек глобально, обрабатывает `429`
по `retry_after`, сохраняет `message_id` для последующего редактирования.

**Почему очередь, а не inline-обработка.** LLM может отвечать секунды; Telegram
повторяет доставку при таймауте; очередь даёт дедупликацию, retry и
освобождает webhook.

**Альтернативы.** Long polling (только dev); синхронная обработка (риск
таймаутов и дублей); `teloxide` dispatcher (не поддерживает RichMessage).

### D4. LLM-capture: HA AI Task как единственный LLM-шлюз

**Поток:**

```text
Telegram text
  → capture service
  → POST {HA_BASE_URL}/api/services/ai_task/generate_data?return_response
       Authorization: Bearer {HA_TOKEN}
       { entity_id, task_name, instructions, structure }
  → HA ai_task (Gemini)
  → { data: { items: [...], clarifications: [...] } }
  → JSON schema + domain validation
  → capture_drafts (15 min TTL)
  → Telegram draft card
  → user confirms
  → transaction: create items + event + outbox result
```

**Запрос.** `task_name` — «Разбор заметки Tasklerq». `instructions` содержит
правила, текущие дату/время/часовой пояс, списки, участников и
пользовательский текст в разделителях. `structure` — вложенный
`selector.object`:

```json
{
  "items": {
    "selector": {
      "object": {
        "multiple": true,
        "fields": {
          "target_kind": {"selector": {"select": {"options": ["tasks", "shopping", "checklist"]}}},
          "target_list": {"selector": {"text": null}},
          "title": {"selector": {"text": null}},
          "notes": {"selector": {"text": {"multiline": true}}},
          "due_date": {"selector": {"text": null}},
          "due_time": {"selector": {"text": null}},
          "all_day": {"selector": {"boolean": null}},
          "priority": {"selector": {"select": {"options": ["low", "normal", "high", "urgent"]}}},
          "assignee": {"selector": {"text": null}},
          "tags": {"selector": {"object": {"multiple": true, "fields": {"value": {"selector": {"text": null}}}}}},
          "quantity": {"selector": {"number": {"min": 0, "max": 9999, "mode": "box"}}},
          "unit": {"selector": {"text": null}},
          "subtasks": {"selector": {"object": {"multiple": true, "fields": {"title": {"selector": {"text": null}}}}}},
          "recurrence": {"selector": {"object": {"fields": {
            "freq": {"selector": {"select": {"options": ["daily", "weekly", "monthly", "yearly"]}}},
            "interval": {"selector": {"number": {"min": 1, "max": 365, "mode": "box"}}},
            "month_rule": {"selector": {"select": {"options": ["anchor_day", "last_day"]}}},
            "weekdays": {"selector": {"object": {"multiple": true, "fields": {"day": {"selector": {"number": {"min": 1, "max": 7, "mode": "box"}}}}}}},
            "until": {"selector": {"text": null}}
          }}}}
        }
      }
    }
  },
  "clarifications": {
    "selector": {"object": {"multiple": true, "fields": {"question": {"selector": {"text": null}}}}}
  }
}
```

Схема упрощена для читаемости. Структура проверена вручную на HA 2026.10.4
(`ai_task.google_ai_task_2`, 2026-10-29): ответ приходит как
`service_response.data.items`, вложенные `object`-селекторы принимаются, а поля
без значения и пустой `clarifications` не возвращаются вовсе, поэтому парсер
обязан считать их необязательными. Автоматически форма проверяется в
интеграционном тесте с HA. Gemini может игнорировать `min`/`max`, поэтому
диапазоны повторно проверяются в Rust.

**Валидация.** После ответа: JSON-схема → проверка типов и лимитов → доменная
валидация (даты, списки, участники, теги, повторения) → нормализация
(trim, lower-case тегов, IANA timezone) → черновик. Ошибки привязываются к
элементу и блокируют сохранение. Идентификаторы из LLM игнорируются.

**Черновик.** `capture_drafts.parsed` хранит только валидированную структуру;
`raw_text` — для повторного показа и аудита, TTL 30 дней. Подтверждение
атомарно, идемпотентно по `draft_id`; после успеха — событие и кнопка отмены.

**Устойчивость.** Таймаут 20 с. Circuit breaker: 5 ошибок за 5 минут →
размыкание на 2 минуты. Повтор только для `5xx`/сетевых ошибок, не для
таймаута (чтобы не платить дважды). После двух некорректных ответов —
ручная форма с исходным текстом.

**Приватность.** В HA/Gemini уходят только текст, дата/время, timezone, списки
и имена участников. Логи — без текста; `llm_runs` — версия промпта, статус,
latency, токены, код ошибки. Отключение: `LLM_CAPTURE_ENABLED=false`.

**Альтернативы.** Прямой Gemini API (противоречит требованию HA); LLM с
инструментами записи (опасно); только regex-парсер (не покрывает свободный
текст, остаётся fallback).

### D4a. Capture без Telegram: голос, автоматизации, API

**Зачем.** Бот — основной UI, но не единственная точка входа. Задачу нужно
добавить голосом через HA Assist, автоматизацией или прямым HTTP-запросом, даже
если Telegram недоступен.

**Источники.** `capture_drafts.source` — `telegram`, `ha_voice`,
`ha_automation`, `api`, `shortcut`. `items.source` — `telegram`, `ha_todo`,
`ha_voice`, `ha_automation`, `api`, `import`. Источник неизменяем и доступен в
API/UI; по нему можно фильтровать. `ha_voice` и `ha_automation` переносятся в
элемент как есть; `shortcut` и `api` дают `items.source=api`; создание через
`todo.add_item` даёт `ha_todo`.

**API.**

```http
POST /api/v1/capture
Authorization: Bearer tlq_...
Idempotency-Key: <uuid>
Content-Type: application/json
```

```json
{
  "text": "завтра купить молоко и хлеб",
  "source": "ha_voice",
  "mode": "auto",
  "hint": "shopping",
  "list_id": null,
  "idempotency_key": "019..."
}
```

Ответ:

```json
{
  "capture_id": "019...",
  "status": "applied",
  "fallback": false,
  "speech": "Добавил 2 покупки: молоко, хлеб.",
  "items": [
    {"id": "019...", "title": "молоко", "list_id": "019...", "kind": "shopping",
     "due_date": null, "due_at": null}
  ],
  "draft_id": null,
  "expires_at": null,
  "clarifications": []
}
```

`speech` — обычный текст без Markdown, не длиннее 200 символов; его озвучивает
HA Assist. `status` — `applied`, `draft` или `rejected`. Для `draft`
возвращается `draft_id` и `expires_at`; `draft_id` совпадает с `capture_id`.
Подтверждение — `POST /api/v1/captures/{id}/confirm`, отмена —
`POST /api/v1/captures/{id}/cancel`, undo —
`POST /api/v1/captures/{id}/undo`.

**Режимы.** `auto` — валидированный разбор применяется сразу одной транзакцией
(`items` + `capture_drafts` + `item_events`), без черновика и без сообщения в
Telegram. `draft` — сохраняется черновик с TTL 15 минут; вызывающая сторона
подтверждает его сама. Telegram всегда использует `draft` + подтверждение.

**Идемпотентность.** `Idempotency-Key` (заголовок или поле) уникален в рамках
`(household_id, source)`. Повтор возвращает результат первого применения и не
создаёт дубликаты. HA-компонент генерирует UUID на каждый вызов сервиса и
переиспользует его при сетевом retry. Ключ резервируется до вызова LLM: повтор,
пока первый запрос ещё обрабатывается, получает `409` с `Retry-After` и не
запускает второй разбор; тот же ключ с другим телом запроса — `422`.

**Валидация.** Та же, что в D4, с послаблениями для `auto`: неизвестный список
заменяется списком по умолчанию, неизвестный исполнитель снимается (элемент
остаётся без исполнителя), пустые необязательные поля отбрасываются. Жёсткие
ошибки (пустое/длинное название, дата в прошлом, неверное повторение) отклоняют
весь capture; частичного создания нет.

**Fallback.** Если LLM недоступна, вернула ошибку, исчерпана квота или дважды
не прошла валидация, `auto` создаёт одну позицию из исходного текста в списке по
умолчанию (`hint` или `tasks`), без срока, с `fallback=true`. Для
`source=ha_voice` таймаут LLM — 8 секунд, чтобы голосовой pipeline успел
ответить; остальные источники — 20 секунд. Если fallback отключён, возвращается
`503`, и HA озвучивает понятную ошибку; исходный текст не теряется в пределах
retention.

**Голосовой ответ.** Короткая сводка: «Добавил 2 покупки: молоко, хлеб»,
«Записал как есть: купить молоко», «Не смог добавить: дата уже прошла». При
большом количестве — «Добавил 5 элементов». Ошибки не содержат внутренних кодов
и текста LLM.

**Без бота.** Внешний capture не создаёт запись в `outbox` и не требует
работающего Telegram. `TELEGRAM_MODE=disabled` отключает webhook, воркер и
исходящие сообщения; `TELEGRAM_BOT_TOKEN` в этом режиме не обязателен. Если
элемент назначен участнику, уведомление о назначении MAY отправляться по
настройке; подтверждение создания — нет.

**Отмена.** `items.capture_id` связывает созданные элементы с capture. Undo в
течение 10 минут мягко удаляет все элементы capture, если они не изменены
после создания; иначе — `409`. HA-сервис `tasklerq.undo_last_capture` отменяет
последний голосовой capture household за 10 минут.

**Путь Home Assistant.** Компонент регистрирует сервис `tasklerq.capture`
(`text`, `source` (`ha_voice`/`ha_automation`, по умолчанию `ha_automation`),
`mode`, `hint`, `list_id`, `idempotency_key`) и возвращает ответ через
`SupportsResponse.OPTIONAL` (`ONLY` отклонил бы вызов из автоматизации без
`response_variable`). Для HA Assist поставляется пример
`conversation.intents` + `intent_script`, который вызывает сервис и озвучивает
`response.speech`. Простой путь `todo.add_item` остаётся для команд без разбора
дат/тегов; он создаёт элемент с `source=ha_todo`.

**Почему не LLM-инструменты в HA.** AI Task не имеет Assist-инструментов, а
разбор должен быть единым для Telegram и голоса, с общей валидацией, квотой,
идемпотентностью и аудитом. Поэтому HA передаёт текст в Tasklerq, а не вызывает
`todo` из LLM.

**Альтернативы.** Отдельный голосовой сервис (лишний компонент); парсинг в HA
через conversation agent (другая модель, нет общей валидации); запись напрямую
в БД из HA (нарушает границу API).

### D5. Home Assistant: тонкий компонент и polling

**Почему custom component.** MQTT discovery не поддерживает `todo` и
`calendar`; шаблонные сенсоры не дают нативных сущностей; CalDAV даёт
календарь, но не задачи с приоритетами/тегами и требует отдельного сервера.
Компонент — единственный способ получить нативные `todo`/`calendar` в HA
Container.

**Структура компонента:**

```text
custom_components/tasklerq/
  __init__.py          # setup, coordinator, service registration
  manifest.json        # domain, version, requirements, iot_class
  config_flow.py       # URL + token, reauth, options
  api.py               # aiohttp client, Bearer, errors
  coordinator.py       # DataUpdateCoordinator, snapshot since
  services.py          # capture / confirm / cancel / undo_last_capture
  services.yaml        # описания полей сервисов для UI
  todo.py              # TodoListEntity per list
  calendar.py          # CalendarEntity aggregate
  diagnostics.py       # redacted diagnostics
  const.py
  strings.json
  translations/ru.json
```

**Сервисы capture.** `tasklerq.capture` вызывает `POST /api/v1/capture` с
токеном интеграции и возвращает `status`, `speech`, `items`, `draft_id`,
`capture_id`. `tasklerq.confirm_capture` и `tasklerq.cancel_capture` работают с
`draft`, `tasklerq.undo_last_capture` отменяет последний голосовой capture.
Сервисы объявляются с `SupportsResponse.OPTIONAL`; ошибки — `HomeAssistantError` с
русским текстом. Для HA Assist в документации и примере пакета поставляются
`conversation.intents` и `intent_script`, которые вызывают сервис и озвучивают
`response.speech`; Python-код intent не нужен.

**Snapshot API.** `GET /api/v1/snapshot?since=<cursor>&calendar_end=<date>`
возвращает `lists`, `items`, `deleted_item_ids`, `calendar_events`,
`server_time`, `next_since`.
Компонент хранит курсор; при первом запуске/смене токена/раз в сутки —
полная синхронизация. Это дешевле, чем отдельный запрос на каждый список.
Курсор должен переживать конкурентные коммиты: `next_since` — это `server_time`
минус 5 секунд перекрытия, а компонент применяет изменения идемпотентно по `id`
и `version`.

**Todo mapping:**

| HA | Tasklerq |
| --- | --- |
| `summary` | `title` |
| `description` | `notes` |
| `due` (date/datetime) | `due_date`/`due_at` |
| `status` | `open`/`done` |
| create | POST item |
| update | PATCH item + version |
| delete | DELETE (soft) |
| complete | POST complete |

Одна сущность на неархивированный список; `unique_id` =
`tasklerq:<household_uuid>:<list_uuid>`. Дополнительные поля — в атрибуте
`tasklerq_items` (priority, tags, assignee, recurrence), потому что нативная
модель `todo` их не содержит.

**Calendar mapping.** Одна read-only сущность `calendar.tasklerq`; события —
задачи со сроком. Вседневные — all-day, со временем — timed. Повторяющиеся
задачи разворачиваются сервером на горизонте (по умолчанию 30 дней, максимум
90). Создание/редактирование событий не поддерживается.

**Запись и конфликты.** Компонент передаёт `version` из последнего снапшота.
`409` → `UpdateFailed`/`HomeAssistantError`, немедленный refresh, пользователь
видит конфликт. Молчаливое перезаписывание запрещено.

**Polling.** По умолчанию 60 с (30–300). При ошибке — backoff 30 с…5 мин,
сущности `unavailable`. При `401` — reauth. Запись инициирует refresh, чтобы
UI обновился быстрее.

**Установка.** HACS custom repository (HACS уже установлен) или копирование в
`/volume2/docker/homeassistant/custom_components/tasklerq` (внутри HA —
`/config/custom_components/tasklerq`); перезапуск HA; добавить интеграцию.
В текущей среде HA работает в `network_mode: host`, поэтому URL Tasklerq —
`http://127.0.0.1:18080`; `8080` на NAS занят Zigbee2MQTT, внутренний порт
контейнера остаётся `8080`.

**Альтернативы.** MQTT (нет `todo`/`calendar`); AppDaemon (нет нативных
сущностей); CalDAV (только календарь, polling 15 минут); прямые REST-сенсоры
(нет `todo`-сервисов).

### D6. Identity: один household, приглашения, роли

- Первый пользователь создаёт household и становится `owner`; создаются
  «Задачи» и «Покупки». Альтернатива без онбординга в боте: администратор
  создаёт household и добавляет владельца командой
  `tasklerq admin user add --telegram-id <id> --role owner`. В приватном чате
  `chat_id` равен Telegram ID, поэтому CLI достаточно ID; когда этот человек
  откроет бота, он сразу увидит главное меню. Второй `owner` не создаётся
  (`409`): владение передаётся штатно. Бот не может написать первым тому, кто ещё
  не нажимал `/start`, поэтому уведомления начнутся после первого обращения.
- Приглашение: 12 случайных байт → base64url 16 символов; в БД — хеш, TTL 72 ч,
  роль `member`, до 5 активных. Deep link
  `https://t.me/tasklerqbot?start=<code>`.
- Пользователь не может состоять в двух household. Это упрощает авторизацию:
  `user.household_id` — единственный tenant-ключ.
- Роли: owner управляет household/участниками/списками; member работает с
  контентом. Проверки — в сервисном слое, не в UI.
- Системные действия (Home Assistant, голос, автоматизация) выполняются от
  `actor_type=voice`/`home_assistant`/`system`, а не от участника. Они не
  получают роль и не могут управлять household, но в пределах scope токена
  (`ha:write`, `capture:write`) создают, редактируют, завершают и удаляют
  элементы; `created_by_user_id` у элемента может быть NULL.
- Удаление участника: снять назначения, отозвать приглашения, сохранить
  авторство задач, уведомить.
- Выход owner: transfer ownership или удаление household.

**Почему не мультитенантность.** Семейный сценарий не требует переключения
между household; будущее расширение — добавить `memberships` и заменить
`users.household_id` на связь, не меняя доменные правила. Вход не только через
Telegram потребует вынести `telegram_user_id` в таблицу идентичностей; обе
миграции аддитивны.

**Задел: связь HA/Telegram и алиасы.** Исполнитель и автор всегда ссылаются на
`users.id`, поэтому человек только из HA добавляется обычной строкой `users` (со
снятым NOT NULL у Telegram-полей) и записью в `user_identities`. Алиасы —
отдельная таблица `user_aliases`, попадающая в промпт новой версией; «говорящий»
в голосовом capture — необязательное поле API, а `actor_type` не заменяется.
Таблицы не создаются до появления второго потребителя; план — в
`docs/roadmap.md`.

### D7. Уведомления и планировщик

Планировщик — tokio-задача с тиком 30 секунд. Он выбирает:

- задачи, у которых `due_at - lead_time` попадает в окно;
- вседневные задачи, у которых локальное время напоминания наступило;
- утренние сводки по локальному времени пользователя;
- записи `notification_deliveries` со статусом `pending` и `scheduled_at <= now()`.

Ключ дедупликации: `(user_id, item_id, occurrence_key, kind)`, где
`occurrence_key` — дата/время вхождения в UTC или `digest:<date>`. Уникальный
индекс не даёт создать дубль после перезапуска. Получатель — исполнитель; если
его нет, то автор-пользователь; если нет и автора (элемент создан голосом, из HA
или через API), то каждый участник с включёнными напоминаниями. Доставка — через
outbox;
`message_id` сохраняется для редактирования.

Тихие часы: если `scheduled_at` попадает в интервал, доставка переносится на
его конец. Действия пользователя в боте не блокируются. Snooze создаёт
отдельную доставку с ключом, включающим время snooze.

Внешний capture не создаёт подтверждающее сообщение в Telegram. Уведомление о
назначении MAY отправляться, если элемент назначен участнику и настройки это
разрешают; утренняя сводка включает такие элементы на общих основаниях.

**Почему не cron на задачу.** Задачи меняются, повторяются и живут в
timezone; один тик + SQL-выборка проще и устойчивее.

### D8. Развёртывание

```text
                    Internet
                       │ 443/TLS
              ┌────────▼─────────┐
              │ Synology DSM     │
              │ Reverse Proxy    │
              │ tasklerq.        │
              │ stellalupus.ru   │
              └────────┬─────────┘
                       │ 127.0.0.1:18080
              ┌────────▼───┐        ┌──────────────────┐
              │ tasklerq   │───────▶│ PostgreSQL 18    │
              │ container  │ shared │ postgres:5432    │
              │ (bridge)   │  -db   │ (external)       │
              └──┬──────┬──┘        └──────────────────┘
                 │      │
 host.docker     │      │ HTTPS
 .internal       │      ▼
                 ▼  ┌──────────────┐
          ┌────────┐│ Telegram API │
          │ HA host│└──────────────┘
          │ :8123  │
          └───┬────┘
              │ ai_task.generate_data
              ▼
         Google Gemini
```

- Один контейнер Tasklerq, внешний PostgreSQL, Synology Reverse Proxy на хосте.
- Host-порт `18080` публикуется только на loopback; TLS завершается прокси.
  `8080` занят Zigbee2MQTT, поэтому используется `18080`; внутри контейнера —
  `8080`.
- HA работает в `network_mode: host`. HA → Tasklerq:
  `http://127.0.0.1:18080`; Tasklerq → HA: `http://host.docker.internal:8123`
  (через `extra_hosts`) или `http://192.168.10.110:8123`.
- PostgreSQL — контейнер `postgres:18-alpine` в сети `shared-db`; Tasklerq
  подключается как `postgres:5432`.
- Конфигурация — env; секреты — env-file 0600 или Docker secrets.
- Миграции — при старте под advisory lock или `tasklerq migrate` перед
  запуском. Expand/contract для отката.
- Health: `/health/live`, `/health/ready`; readiness проверяет БД, миграции и
  задержку очередей.
- Backup: ежедневный `pg_dump`, 14 дней локально, затем Hyper Backup на внешний
  диск; restore drill.
- Retention: updates 7 д, drafts 30 д (включая `raw_text`), llm_runs 90 д,
  events 365 д, deleted 30 д, sent outbox 30 д.
- Часовой пояс household по умолчанию — `Europe/Moscow`.

**Почему не HA add-on.** HA Container не поддерживает add-on/ingress; add-on
потребовал бы Supervisor. **Почему не Kubernetes.** Один сервис и семейный
масштаб; Kubernetes добавил бы сложность без пользы.

### D9. Безопасность

| Угроза | Контроль |
| --- | --- |
| Поддельный webhook | Секрет в заголовке, constant-time сравнение, 401, лимит тела. |
| Поддельный callback | Одноразовый токен, привязка к user/household, TTL, атомарное потребление. |
| Поддельный внешний capture | Bearer-токен со scope `capture:write`, лимит тела и текста, rate limit, `Idempotency-Key`. |
| IDOR | Tenant-ключ household во всех запросах, 404 для чужих ресурсов. |
| XSS в RichMessage | Экранирование HTML, хранение как данных. |
| SQL-инъекция | Параметризованные запросы sqlx, без конкатенации. |
| LLM-инъекция | Данные в делимитерах, schema-only, без инструментов, серверная валидация, подтверждение. |
| Утечка секретов | env/secret files, redaction в логах/diagnostics, ротация. |
| Спам и флуд | Лимиты на пользователя/IP, LLM-квота, 429. |
| Replay | `update_id` PK, одноразовые callback-токены, идемпотентные эффекты. |
| Утечка PII | Минимизация, отсутствие текста в логах, retention, выгрузка/удаление. |

API-токены: 32 байта случайности, префикс `tlq_`, SHA-256 хеш, scopes
`ha:read`/`ha:write`/`capture:write`, expiry, revoke, `last_used_at`. Паролей
нет. `capture:write` даёт только создание/подтверждение/отмену capture и не
даёт чтения списков; HA-компоненту выдаются `ha:read` + `ha:write` +
`capture:write`.

### D10. Тестирование и качество

- **Unit:** валидация, повторения (включая 31-е, последний день месяца и 29 февраля), timezone/DST,
  рендеринг RichMessage, callback-токены, парсинг LLM.
- **Integration:** testcontainers PostgreSQL; репозитории, транзакции, очередь
  `SKIP LOCKED`, миграции.
- **Contract:** mock Telegram (`wiremock`) и mock HA; проверка схемы
  `ai_task.generate_data`; формат запросов RichMessage; capture API
  (auto/draft/idempotency/undo) и HA-сервисы.
- **Python:** `pytest-homeassistant-custom-component`; config flow, todo,
  calendar, diagnostics, конфликты.
- **E2E:** docker compose + fake Telegram + fake HA; сценарии «создать задачу»,
  «подтвердить черновик», «голосовой capture без Telegram», «завершить из HA»,
  «напоминание».
- **LLM golden set:** ≥30 русских фраз, проверка извлечения элементов ≥90%;
  версия промпта обязательна.
- **CI:** `cargo fmt --check`, `clippy -D warnings`, `cargo test`,
  `sqlx prepare --check`, `cargo audit`; Python `ruff`, `mypy`, `pytest`;
  `openspec validate --strict`.
- **Нагрузка:** 10 пользователей, 10 000 задач, 50 webhook/с; проверить
  latency webhook <100 мс (без LLM), очередь не растёт бесконечно.
- **Ручной чек-лист:** Telegram iOS/Android/Desktop, HA Companion, русские
  даты, длинные списки, конфликт версий, отключение LLM.

### D11. Наблюдаемость

- JSON-логи: `timestamp`, `level`, `request_id`, `update_id`, `route`,
  `status`, `duration_ms`, `error_code`, `version`. Без текста сообщений,
  токенов и имён.
- Метрики: `telegram_updates_total{status}`, `telegram_queue_lag_seconds`,
  `outbox_pending`, `capture_total{source,status}`,
  `capture_fallback_total{source}`, `llm_requests_total{status}`,
  `llm_latency_seconds`, `llm_validation_failures_total`,
  `http_requests_total{route,status}`, `db_pool_in_use`.
- Health: live — процесс; ready — БД, миграции, задержка очередей ниже порога.
- Алерты: readiness >2 мин, queue lag >5 мин, LLM error rate >20% за 15 мин,
  webhook auth failures >50/5 мин, backup failed.

## Risks / Trade-offs

| Риск | Митигация |
| --- | --- |
| RichMessage API меняется | Адаптер, contract-тесты, ручной чек-лист клиентов. |
| LLM отвечает медленно/дорого/неверно | Таймаут 20 с, для голоса 8 с, circuit breaker, лимиты, кэш промпта, ручная форма/fallback, golden set. |
| Голосовой pipeline не успевает за LLM | Короткий таймаут 8 с, fallback-создание одной позиции, короткий `speech`, асинхронный LLM не блокирует ответ. |
| Голосовой capture создаёт дубль | `Idempotency-Key` от HA-компонента, уникальность `(household, source, key)`, повтор возвращает первый результат. |
| HA polling даёт задержку до 60 с | Настраиваемый интервал, refresh после записи, явное указание задержки в UI. |
| Повторения и DST сложны | Простые правила, якорь серии, unit-тесты на границы, отсутствие исключений в MVP. |
| Один household ограничивает развитие | Модель `memberships` в будущем; текущие правила не меняются. |
| Публичный webhook — поверхность атаки | TLS, секрет, лимиты, 401, мониторинг отказов, reverse proxy. |
| PostgreSQL — единая точка отказа | Backup, readiness, pool, retry, документация restore. |
| Telegram 429/лимиты | Outbox, 1 msg/s на чат, retry_after, дедупликация. |
| LLM видит пользовательский текст | Минимизация контекста, раскрытие в документации, отключаемый capture, retention. |
| Сложность синхронизации HA | Snapshot + version, конфликт 409, read-only календарь, contract-тесты. |

## Migration Plan

Изменение создаёт только документацию и спецификации; миграции данных нет.

**Этапы реализации (см. `tasks.md`):**

1. **Foundation:** Cargo-проект, конфигурация, Docker, миграции, health.
2. **Domain:** household/users/lists/items/tags/subtasks/events, повторения,
   поиск, API, токены.
3. **Telegram:** транспорт, webhook-очередь, RichMessage, экраны, формы,
   callback-токены, shopping UI.
4. **LLM и capture:** HA AI Task client, схема, валидация, черновик, fallback,
   внешний capture API (auto/draft/idempotency/undo), голосовой сценарий,
   golden set.
5. **HA:** custom component, todo/calendar, сервисы capture, Assist intent,
   snapshot, конфликты, diagnostics.
6. **Notifications:** scheduler, reminders, digest, assignment, quiet hours,
   outbox, retention.
7. **Hardening:** security, load, backup/restore, runbooks, релиз.

**Откат:** предыдущий Docker-образ + совместимая схема (expand/contract). Перед
обновлением — backup. Разрушительные миграции — только в следующем релизе.

**Данные:** импорт из старых систем не предусмотрен; при необходимости —
CLI export/import JSON после MVP.

## Open Questions

Среда развёртывания определена: Synology NAS `192.168.10.110`, Synology
Reverse Proxy, домен `tasklerq.stellalupus.ru`, PostgreSQL 18 в сети
`shared-db`, HA Container `2026.10.4` в `network_mode: host`, HACS, AI Task
`ai_task.google_ai_task_2`, бот `@tasklerqbot`, часовой пояс `Europe/Moscow`,
`raw_text` — 30 дней, backup — Hyper Backup на внешний диск.

Не меняют архитектуру и спецификации, но нужны для финальной документации:

- Бюджет Gemini при квоте 200 разборов в сутки; модель выбирается в HA.

Роль и БД `tasklerq`, домен, reverse proxy и сертификат созданы 2026-10-29.