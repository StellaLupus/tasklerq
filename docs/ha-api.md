# REST API Tasklerq для Home Assistant

Точный контракт REST API, по которому пишется клиент Home Assistant
(`tasklerq-ha`). Документ описывает только то, что реализовано в коде сервера.

- **Версия API:** `v1` (префикс пути `/api/v1`).
- **Базовый путь:** `/api/v1`; health-эндпоинты (`/health/live`,
  `/health/ready`) находятся вне него и клиенту не нужны.
- Сверено с кодом на коммите bd2504e.
- **Источник правды:** `src/http/router.rs`, `src/http/api/*.rs`,
  `src/http/error.rs`, `src/http/middleware.rs`, `src/auth/*.rs` и тесты
  `tests/api.rs`, `tests/http_contract.rs`, `tests/capture_api.rs`,
  `tests/calendar.rs`.
- **Язык:** сообщения об ошибках API — английские (для разработчиков);
  `speech` в capture-ответах — русский, для озвучивания.

Карта эндпоинтов, которые нужны компоненту:

| Метод | Путь | Scope | Назначение |
| --- | --- | --- | --- |
| `GET` | `/api/v1/household` | `ha:read` | Household, участники, timezone. |
| `GET` | `/api/v1/lists` | `ha:read` | Все неудалённые списки. |
| `POST` | `/api/v1/lists` | `ha:write` | Создать список. |
| `PATCH` | `/api/v1/lists/{id}` | `ha:write` | Переименовать/архивировать. |
| `DELETE` | `/api/v1/lists/{id}` | `ha:write` | Мягко удалить список и его позиции. |
| `GET` | `/api/v1/lists/{id}/items` | `ha:read` | Позиции списка. |
| `POST` | `/api/v1/lists/{id}/items` | `ha:write` | Создать позицию. |
| `GET` | `/api/v1/items/{id}` | `ha:read` | Карточка позиции. |
| `PATCH` | `/api/v1/items/{id}` | `ha:write` | Изменить позицию (нужна `version`). |
| `DELETE` | `/api/v1/items/{id}` | `ha:write` | Мягко удалить позицию. |
| `POST` | `/api/v1/items/{id}/complete` | `ha:write` | Завершить. |
| `POST` | `/api/v1/items/{id}/reopen` | `ha:write` | Открыть завершённую. |
| `POST` | `/api/v1/items/{id}/undo` | `ha:write` | Отменить последнее действие над позицией. |
| `GET` | `/api/v1/snapshot` | `ha:read` | Инкрементальный снапшот для координатора. |
| `GET` | `/api/v1/calendar` | `ha:read` | Вхождения за интервал. |
| `POST` | `/api/v1/capture` | `capture:write` | Внешний capture текста. |
| `POST` | `/api/v1/captures/{id}/confirm` | `capture:write` | Подтвердить черновик. |
| `POST` | `/api/v1/captures/{id}/cancel` | `capture:write` | Отменить черновик. |
| `POST` | `/api/v1/captures/{id}/undo` | `capture:write` | Отменить применённый capture. |
| `POST` | `/api/v1/captures/undo-last` | `capture:write` | Отменить последний голосовой capture. |

`GET /api/v1/search` в коде есть, но компоненту не нужен и здесь не описан.

## 1. Общие соглашения

### 1.1 Аутентификация и scopes

Все эндпоинты требуют заголовок:

```http
Authorization: Bearer tlq_...
```

- Схема проверяется буквально: префикс `Bearer ` (с пробелом), затем токен
  без пробелов по краям. Токен обязан начинаться с `tlq_`.
- Токен в query-параметре не принимается.
- Неизвестный, отозванный или истёкший токен → `401 unauthorized`
  (`Missing bearer token` или `Invalid or expired token`).
- Токен без нужного scope → `403 forbidden`
  (`Token is missing scope <scope>`).

Scopes не иерархичны: `ha:write` не даёт права на чтение, `ha:read` — на
запись. Допустимые значения: `ha:read`, `ha:write`, `capture:write`.

| Scope | Что разрешает |
| --- | --- |
| `ha:read` | `GET /household`, `GET /lists`, `GET /lists/{id}/items`, `GET /items/{id}`, `GET /snapshot`, `GET /calendar`. |
| `ha:write` | `POST/PATCH/DELETE` списков и позиций, `complete`, `reopen`, `undo` позиции. |
| `capture:write` | Все `/capture` и `/captures/*`. Не даёт доступа к спискам и позициям. |

Компоненту нужен токен со всеми тремя scope, если он использует capture;
для read-only дашборда достаточно `ha:read`.

### 1.2 Заголовки запроса

| Заголовок | Где обязателен | Правила |
| --- | --- | --- |
| `Authorization` | Всегда | `Bearer <token>`. |
| `Content-Type` | Тела с JSON (`POST`, `PATCH`, `POST /capture`) | Должен быть `application/json` (параметры charset допускаются axum). |
| `Idempotency-Key` | Нет; только `POST /capture` | 1..128 видимых ASCII-символов (`0x21..0x7E`, без пробелов). Пустой = без ключа. Несколько заголовков → `422`. |
| `X-Request-Id` | Нет | 1..128 символов `[A-Za-z0-9._:-]`. Валидное значение возвращается в ответе; невалидное заменяется сгенерированным UUIDv7. |

`If-Match`, `ETag`, `If-None-Match` **не реализованы**. Оптимистичная
блокировка передаётся полем `version` в теле (см. §1.5).

### 1.3 Заголовки ответа

| Заголовок | Когда | Значение |
| --- | --- | --- |
| `X-Request-Id` | Всегда | Идентификатор запроса. Он же `error.request_id` в теле ошибки. |
| `Retry-After` | `429`; `409` при незавершённом `Idempotency-Key` | Целое число секунд. |
| `Content-Type` | Ответы приложения с телом | `application/json`; у `204` тела нет. |

`request_id` также входит в каждое тело ошибки приложения. Клиенту
рекомендуется писать его в лог — по нему ищут запрос на сервере.

### 1.4 Формат ошибок

Ошибки приложения всегда имеют конверт:

```json
{
  "error": {
    "code": "conflict",
    "message": "Task was modified",
    "details": {
      "current_version": 4,
      "item": { "...": "полная карточка позиции" }
    },
    "request_id": "019a1b2c-3d4e-7f60-8a9b-0c1d2e3f4a5b"
  }
}
```

| Поле | Тип | Nullable | Описание |
| --- | --- | --- | --- |
| `error.code` | string | нет | Машиночитаемый код из таблицы ниже. |
| `error.message` | string | нет | Английское сообщение для разработчика. |
| `error.details` | object | нет | Обычно `{}`; у части `409` содержит данные для восстановления. |
| `error.request_id` | string | да | Тот же id, что в `X-Request-Id`. |

Коды и HTTP-статусы, которые возвращает код:

| `code` | HTTP | Когда |
| --- | --- | --- |
| `validation_error` | `422` | Некорректные поля, даты, перечисления, UUID в теле capture, `version`; невалидные элементы черновика; конфликт `Idempotency-Key`. |
| `unauthorized` | `401` | Нет токена, токен неизвестен/отозван/истёк. |
| `forbidden` | `403` | У токена нет требуемого scope. |
| `not_found` | `404` | Нет ресурса, чужой household, архивный список там, где он скрыт, выключенный capture, неизвестный маршрут. |
| `conflict` | `409` | Несовпадение `version`, состояние черновика/capture, лимит списков, дубль имени, удаление/архивирование списка по умолчанию, занятый `Idempotency-Key`. |
| `gone` | `410` | Подтверждение черновика старше TTL (15 минут). |
| `rate_limited` | `429` | Лимит частоты capture или исчерпанная суточная квота LLM в режиме `draft`. |
| `internal_error` | `500` | Ошибка БД/внутренняя ошибка. |
| `service_unavailable` | `503` | LLM не настроена/недоступна, предохранитель открыт, квота исчерпана там, где fallback запрещён. |
| `payload_too_large` | `413` | Тело `POST /capture` больше 16 КиБ. |
| `unsupported_media_type` | `415` | Нет `Content-Type: application/json` или он неверен (на `POST /capture`). |
| `method_not_allowed` | `405` | Метод не поддерживается маршрутом. |

Детали `409`, значимые для клиента:

| Ситуация | `details` |
| --- | --- |
| `PATCH /items/{id}` с устаревшей `version` | `{"current_version": <int>, "item": <полная карточка ItemResponse>}`. |
| `POST /items/{id}/complete` или `/reopen` с неверной `version` | То же: `{"current_version": <int>, "item": <полная карточка ItemResponse>}`. |
| `POST /items/{id}/undo`: окно истекло | `{"event_id": "<uuid>", "created_at": "<RFC 3339>"}`. |
| Остальные `409` | `{}`. |

**Ошибки уровня фреймворка.** Не все отказы проходят через конверт:

- невалидный UUID в пути (`/items/not-a-uuid`) → `400` с текстовым телом
  (axum `Path` rejection), без `error.code`;
- битый JSON или неверные типы в теле `POST/PATCH` списков и позиций →
  `400`/`415`/`422` с текстовым телом axum (`Json` rejection), без конверта;
- на `POST /capture` те же отказы перехвачены и всегда отдают конверт:
  битый JSON и неизвестные поля → `422 validation_error`, нет
  `Content-Type` → `415 unsupported_media_type`, большое тело →
  `413 payload_too_large`.

Клиенту стоит обрабатывать оба варианта: наличие `error.code` и «сырой»
текст с 4xx.

### 1.5 Версии и оптимистичная блокировка

- `version` есть только у позиции (`items`), не у списка.
- Новая позиция создаётся с `version = 1`.
- Любая мутация позиции увеличивает `version` на 1 и обновляет `updated_at`:
  `PATCH`, `complete`, `reopen`, `DELETE`, `undo`, а также каскадное удаление
  позиций при `DELETE /lists/{id}`.
- `PATCH /items/{id}` требует `version` в теле. Несовпадение → `409 conflict`
  с `details.current_version` и полной карточкой в `details.item`.
- `complete` и `reopen` принимают `version` необязательно: если передана и не
  совпала — `409`; если не передана — применяется к текущему состоянию.
- Списки версии не имеют: `PATCH /lists/{id}` — last write wins.
- Повторная операция, которая уже применена, идемпотентна и возвращает `200`
  с текущей позицией (`complete` завершённой, `reopen` открытой,
  `DELETE` уже удалённой — `204`).

### 1.6 Время, даты и timezone

| Понятие | Формат |
| --- | --- |
| Момент времени (`due_at`, `completed_at`, `created_at`, `updated_at`, `server_time`, `next_since`, `expires_at`) | RFC 3339 в UTC: `2026-11-03T12:00:00Z`. На входе принимается и смещение (`2026-11-03T15:00:00+03:00`), сервер приводит к UTC. |
| Дата без времени (`due_date`, `until`, `anchor_date`, `start`/`end` all-day события) | `YYYY-MM-DD`. |
| Параметры `start`/`end`/`calendar_end`/`since` | `start`/`end`/`calendar_end` — `YYYY-MM-DD`; `since` — RFC 3339. |

- Все моменты времени в ответах — UTC с суффиксом `Z`.
- Доли секунды могут присутствовать: `Utc::now()` даёт до 9 знаков, значения
  из PostgreSQL — до 6. Клиент MUST принимать любой RFC 3339 (включая
  `2026-11-03T12:00:00.123456Z`) и передавать `next_since` обратно как есть,
  не переформатируя его.
- Границы календарных суток, «сегодня» и вседневные события считаются в
  timezone household (`households.timezone`, IANA). Timezone household
  приходит в `GET /household`.
- Повторения считаются по локальному настенному времени в
  `recurrence.timezone` (по умолчанию — timezone household). Переход DST не
  сдвигает локальное время серии.
- `due_at` и `due_date` взаимоисключающие: одновременно задать оба нельзя.

### 1.7 Идентификаторы

- Все id — UUID в строковом виде (в коде генерируются UUIDv7). Регистр в
  ответах — нижний.
- Невалидный UUID в пути даёт `400` (текстовое тело axum), а не `404`.
- Невалидный UUID в теле `POST /capture` (`list_id`) даёт
  `422 validation_error`; в телах списков и позиций (`assignee_user_id`,
  `list_id`) — `422` от axum без конверта (см. §1.4).

### 1.8 Перечисления и общие ограничения

| Перечисление | Значения |
| --- | --- |
| `ListKind` | `tasks`, `shopping`, `checklist` |
| `ItemStatus` | `open`, `done` |
| `ItemPriority` | `low`, `normal`, `high`, `urgent` |
| `ItemSource` | `telegram`, `ha_todo`, `ha_voice`, `ha_automation`, `api`, `import` |
| `RecurrenceFreq` | `daily`, `weekly`, `monthly`, `yearly` |
| `MonthRule` | `anchor_day`, `last_day` |

Ограничения текстовых полей (для `title`, `notes`, тегов, подзадач и названия
списка считаются символы Unicode после `trim` и удаления управляющих символов,
кроме `\n` и `\t`; `unit` только обрезается по краям):

| Поле | Ограничение |
| --- | --- |
| `title` позиции | 1..200, обязательное, не пустое после trim. |
| `notes` | 0..2000; пустая строка допустима. |
| `unit` | Обрезается до 16 символов; пустое значение заменяется на `шт`. |
| Тег | 1..32; приводится к нижнему регистру; дубликаты удаляются. |
| Тегов на позицию | Не более 10. |
| Подзадач | Не более 50; заголовок 1..200. |
| Название списка | 1..64. |
| Списков в household | Не более 20 (включая списки по умолчанию и архивные). |
| `text` capture | 1..`CAPTURE_MAX_TEXT_CHARS` (по умолчанию 2000; конфиг допускает 100..2000). |

Неизвестные поля в теле `POST/PATCH` списков и позиций **игнорируются**
(структуры без `deny_unknown_fields`). В теле `POST /capture` неизвестное поле
даёт `422`.

## 2. Household

### `GET /api/v1/household`

Scope: `ha:read`. Параметров нет.

Ответ `200`:

| Поле | Тип | Nullable | Описание |
| --- | --- | --- | --- |
| `household.id` | uuid | нет | Идентификатор household. |
| `household.name` | string | нет | Название. |
| `household.timezone` | string | нет | IANA timezone, например `Europe/Moscow`. |
| `household.created_at` | string | нет | RFC 3339 UTC. |
| `household.updated_at` | string | нет | RFC 3339 UTC. |
| `household.deleted_at` | string | да | В ответе всегда `null` (удалённый household даёт `404`). |
| `members` | array | нет | Активные участники (`deleted_at IS NULL`). |
| `members[].id` | uuid | нет | Идентификатор участника. |
| `members[].household_id` | uuid | нет | Household. |
| `members[].telegram_user_id` | integer | нет | Telegram user id. |
| `members[].telegram_chat_id` | integer | нет | Telegram chat id. |
| `members[].display_name` | string | нет | Отображаемое имя. |
| `members[].username` | string | да | Telegram username. |
| `members[].role` | string | нет | `owner` или `member`. |
| `members[].timezone` | string | да | Личный IANA timezone. |
| `members[].settings` | object | нет | Произвольный JSON настроек уведомлений (по умолчанию `{}`). |
| `members[].created_at` | string | нет | RFC 3339 UTC. |
| `members[].updated_at` | string | нет | RFC 3339 UTC. |
| `members[].deleted_at` | string | да | Всегда `null` в этом ответе. |

Порядок участников: `role`, `display_name`, `id`.

Пример:

```json
{
  "household": {
    "id": "019a1b2c-3d4e-7f60-8a9b-0c1d2e3f4a5b",
    "name": "Семья",
    "timezone": "Europe/Moscow",
    "created_at": "2026-10-01T08:00:00Z",
    "updated_at": "2026-10-01T08:00:00Z",
    "deleted_at": null
  },
  "members": [
    {
      "id": "019a1b2c-4a5b-7c6d-8e9f-0a1b2c3d4e5f",
      "household_id": "019a1b2c-3d4e-7f60-8a9b-0c1d2e3f4a5b",
      "telegram_user_id": 100000001,
      "telegram_chat_id": 100000001,
      "display_name": "Алексей",
      "username": "alexey",
      "role": "owner",
      "timezone": "Europe/Moscow",
      "settings": {},
      "created_at": "2026-10-01T08:00:00Z",
      "updated_at": "2026-10-01T08:00:00Z",
      "deleted_at": null
    }
  ]
}
```

Коды: `200`; `401`; `403`; `404` (household удалён); `500`.

## 3. Списки

### Объект `List`

| Поле | Тип | Nullable | Описание |
| --- | --- | --- | --- |
| `id` | uuid | нет | Идентификатор. |
| `household_id` | uuid | нет | Household. |
| `kind` | string | нет | `tasks`, `shopping`, `checklist`. |
| `name` | string | нет | Название. |
| `is_default` | boolean | нет | Список по умолчанию своего `kind`. |
| `is_archived` | boolean | нет | Архивирован. |
| `position` | integer | нет | Порядок в меню. |
| `created_at` | string | нет | RFC 3339 UTC. |
| `updated_at` | string | нет | RFC 3339 UTC. |
| `deleted_at` | string | да | В ответах API всегда `null`: удалённые списки не возвращаются. |

`unique_id` для HA: `tasklerq:<household_uuid>:<list_uuid>`.

### `GET /api/v1/lists`

Scope: `ha:read`. Параметров нет.

Ответ `200` — JSON-массив объектов `List`, все неудалённые списки household,
включая архивные (`is_archived: true`). Порядок: `position`, `created_at`,
`id`.

```json
[
  {
    "id": "019a1b2c-5b6c-7d8e-9f00-1a2b3c4d5e6f",
    "household_id": "019a1b2c-3d4e-7f60-8a9b-0c1d2e3f4a5b",
    "kind": "tasks",
    "name": "Задачи",
    "is_default": true,
    "is_archived": false,
    "position": 0,
    "created_at": "2026-10-01T08:00:00Z",
    "updated_at": "2026-10-01T08:00:00Z",
    "deleted_at": null
  },
  {
    "id": "019a1b2c-6c7d-7e8f-9a0b-2b3c4d5e6f70",
    "household_id": "019a1b2c-3d4e-7f60-8a9b-0c1d2e3f4a5b",
    "kind": "shopping",
    "name": "Покупки",
    "is_default": true,
    "is_archived": false,
    "position": 1,
    "created_at": "2026-10-01T08:00:00Z",
    "updated_at": "2026-10-01T08:00:00Z",
    "deleted_at": null
  }
]
```

Коды: `200`; `401`; `403`; `500`.

### `POST /api/v1/lists`

Scope: `ha:write`. `Content-Type: application/json`.

Тело:

| Поле | Тип | Обязательно | Ограничения |
| --- | --- | --- | --- |
| `kind` | string | да | `shopping` или `checklist`. `tasks` отклоняется (`422`): список задач в household один и создаётся при онбординге. |
| `name` | string | да | 1..64 символа; уникален в household без учёта регистра среди неудалённых списков. |

```json
{"kind": "checklist", "name": "Дача"}
```

Ответ `201` — объект `List` (см. выше), например:

```json
{
  "id": "019a1b2c-7d8e-7f90-8a1b-3c4d5e6f7081",
  "household_id": "019a1b2c-3d4e-7f60-8a9b-0c1d2e3f4a5b",
  "kind": "checklist",
  "name": "Дача",
  "is_default": false,
  "is_archived": false,
  "position": 2,
  "created_at": "2026-10-31T10:00:00Z",
  "updated_at": "2026-10-31T10:00:00Z",
  "deleted_at": null
}
```

Коды:

| Код | Когда |
| --- | --- |
| `201` | Список создан. |
| `401` / `403` | Нет токена / нет `ha:write`. |
| `409` | Уже 20 списков (`household cannot have more than 20 lists`) или имя занято (`list with this name already exists`). |
| `422` | `kind=tasks`, пустое/длинное имя, битое тело. |
| `500` | Ошибка БД. |

### `PATCH /api/v1/lists/{id}`

Scope: `ha:write`. `Content-Type: application/json`.

Тело (все поля необязательны; `{}` допустим и обновляет `updated_at`):

| Поле | Тип | Nullable | Ограничения |
| --- | --- | --- | --- |
| `name` | string | да | 1..64; уникален без учёта регистра. `null` = не менять. |
| `is_archived` | boolean | да | `true` архивирует, `false` возвращает из архива. `null` = не менять. Список по умолчанию нельзя архивировать. |

```json
{"name": "Дача 2"}
```

Ответ `200` — обновлённый `List`.

Коды: `200`; `400` (невалидный UUID в пути); `401`; `403`; `404`
(нет/чужой/уже удалён); `409` (имя занято, `default list cannot be archived`);
`422` (пустое/длинное имя, битое тело); `500`.

После архивирования:
- список остаётся в `GET /lists` и в `snapshot.lists` с `is_archived: true`;
- `GET /lists/{id}/items` и `POST /lists/{id}/items` для него дают `404`;
- существующие позиции по-прежнему видны в `GET /items/{id}` и в снапшоте,
  но `PATCH /items/{id}` для них даёт `404` (см. §4.4).

### `DELETE /api/v1/lists/{id}`

Scope: `ha:write`. Тела нет.

Мягко удаляет список и все его неудалённые позиции: у позиций выставляются
`deleted_at = updated_at = now`, `version = version + 1`. Поэтому в следующем
инкрементальном снапшоте они придут в `deleted_item_ids`.

Ответ `204` без тела.

Коды: `204`; `400` (невалидный UUID в пути); `401`; `403`; `404`
(нет/чужой/уже удалён); `409` (`default list cannot be deleted`); `500`.

Повторный `DELETE` уже удалённого списка даёт `404`, а не `204`.

## 4. Позиции

### Объект `ItemResponse`

Все ответы с позицией — это `Item` с добавленным `subtask_progress`.

| Поле | Тип | Nullable | Описание |
| --- | --- | --- | --- |
| `id` | uuid | нет | Идентификатор. |
| `household_id` | uuid | нет | Household. |
| `list_id` | uuid | нет | Список. |
| `title` | string | нет | Название. |
| `notes` | string | да | Описание. |
| `status` | string | нет | `open` или `done`. |
| `priority` | string | нет | `low`, `normal`, `high`, `urgent`. |
| `due_at` | string | да | RFC 3339 UTC; взаимоисключающе с `due_date`. |
| `due_date` | string | да | `YYYY-MM-DD`; взаимоисключающе с `due_at`. |
| `assignee_user_id` | uuid | да | Активный участник household. |
| `created_by_user_id` | uuid | да | Автор; у внешнего API/HA — `null`. |
| `source` | string | нет | `telegram`, `ha_todo`, `ha_voice`, `ha_automation`, `api`, `import`. |
| `capture_id` | uuid | да | Capture, создавший позицию. |
| `completed_at` | string | да | RFC 3339 UTC. |
| `completed_by_user_id` | uuid | да | Кто завершил. |
| `quantity` | number | да | Количество; только для `shopping`/`checklist`, иначе `null`. |
| `unit` | string | да | Единица; только для `shopping`/`checklist`, иначе `null`. |
| `position` | integer | нет | Порядок в списке. |
| `version` | integer | нет | Версия для оптимистичной блокировки. |
| `recurrence` | object | да | Правило повторения (см. ниже) или `null`. |
| `created_at` | string | нет | RFC 3339 UTC. |
| `updated_at` | string | нет | RFC 3339 UTC. |
| `deleted_at` | string | да | В обычных ответах `null`; не `null` только в ответе `POST /items/{id}/undo`, если отменено создание. |
| `tags` | array | нет | Массив тегов. |
| `tags[].id` | uuid | нет | Идентификатор тега. |
| `tags[].household_id` | uuid | нет | Household. |
| `tags[].name` | string | нет | Имя в нижнем регистре. |
| `tags[].created_at` | string | нет | RFC 3339 UTC. |
| `subtasks` | array | нет | Массив подзадач. |
| `subtasks[].id` | uuid | нет | Идентификатор. |
| `subtasks[].item_id` | uuid | нет | Родительская позиция. |
| `subtasks[].title` | string | нет | Название. |
| `subtasks[].status` | string | нет | `open` или `done`. |
| `subtasks[].position` | integer | нет | Порядок. |
| `subtasks[].created_at` | string | нет | RFC 3339 UTC. |
| `subtasks[].completed_at` | string | да | RFC 3339 UTC. |
| `subtask_progress.done` | integer | нет | Выполнено подзадач. |
| `subtask_progress.total` | integer | нет | Всего подзадач. |

Объект `recurrence`:

| Поле | Тип | Nullable | Описание |
| --- | --- | --- | --- |
| `freq` | string | нет | `daily`, `weekly`, `monthly`, `yearly`. |
| `interval` | integer | нет | 1..365. |
| `byweekday` | array[integer] | нет | Дни недели ISO 1..7 (пн=1); только для `weekly`, иначе `[]`. |
| `until` | string | да | `YYYY-MM-DD`, конец серии включительно. |
| `timezone` | string | нет | IANA timezone правила. |
| `anchor_date` | string | нет | `YYYY-MM-DD`, якорь серии. |
| `month_rule` | string | да | `anchor_day`/`last_day`; только для `monthly`, иначе `null`. |

Пример `ItemResponse`:

```json
{
  "id": "019a1b2c-8e9f-7a0b-9c1d-4d5e6f708192",
  "household_id": "019a1b2c-3d4e-7f60-8a9b-0c1d2e3f4a5b",
  "list_id": "019a1b2c-5b6c-7d8e-9f00-1a2b3c4d5e6f",
  "title": "Оплатить интернет",
  "notes": "До 18:00",
  "status": "open",
  "priority": "high",
  "due_at": "2026-11-05T15:00:00Z",
  "due_date": null,
  "assignee_user_id": "019a1b2c-4a5b-7c6d-8e9f-0a1b2c3d4e5f",
  "created_by_user_id": null,
  "source": "ha_todo",
  "capture_id": null,
  "completed_at": null,
  "completed_by_user_id": null,
  "quantity": null,
  "unit": null,
  "position": 3,
  "version": 2,
  "recurrence": {
    "freq": "monthly",
    "interval": 1,
    "byweekday": [],
    "until": null,
    "timezone": "Europe/Moscow",
    "anchor_date": "2026-11-05",
    "month_rule": "anchor_day"
  },
  "created_at": "2026-10-20T09:00:00Z",
  "updated_at": "2026-10-31T09:30:00Z",
  "deleted_at": null,
  "tags": [
    {
      "id": "019a1b2c-9f0a-7b1c-8d2e-5e6f708192a3",
      "household_id": "019a1b2c-3d4e-7f60-8a9b-0c1d2e3f4a5b",
      "name": "финансы",
      "created_at": "2026-10-20T09:00:00Z"
    }
  ],
  "subtasks": [
    {
      "id": "019a1b2c-a0b1-7c2d-9e3f-6f708192a3b4",
      "item_id": "019a1b2c-8e9f-7a0b-9c1d-4d5e6f708192",
      "title": "Проверить счёт",
      "status": "open",
      "position": 0,
      "created_at": "2026-10-20T09:00:00Z",
      "completed_at": null
    }
  ],
  "subtask_progress": {"done": 0, "total": 1}
}
```

### 4.1 `GET /api/v1/lists/{id}/items`

Scope: `ha:read`. Параметров нет.

Ответ `200`:

```json
{"items": []}
```

Элементы массива — объекты `ItemResponse` (пример выше).

Порядок: открытые (`open`) раньше завершённых (`done`), затем `position`,
`created_at`, `id`.

Коды: `200`; `400` (невалидный UUID в пути); `401`; `403`; `404` (список не
существует, чужой, удалён или архивирован); `500`.

### 4.2 `POST /api/v1/lists/{id}/items`

Scope: `ha:write`. `Content-Type: application/json`.

Тело:

| Поле | Тип | Обязательно | Ограничения |
| --- | --- | --- | --- |
| `title` | string | да | 1..200 символов. |
| `notes` | string | нет | 0..2000; `null` = без описания. |
| `priority` | string | нет | `low`/`normal`/`high`/`urgent`; по умолчанию `normal`. |
| `due_at` | string | нет | RFC 3339; нельзя вместе с `due_date`. |
| `due_date` | string | нет | `YYYY-MM-DD`; нельзя вместе с `due_at`. |
| `assignee_user_id` | uuid | нет | Должен быть активным участником household, иначе `422`. |
| `quantity` | number | нет | Только для `shopping`/`checklist`: `> 0`, `<= 9999.999`, не более 3 знаков после запятой; по умолчанию `1`. Для `tasks` игнорируется. |
| `unit` | string | нет | Только для `shopping`/`checklist`: до 16 символов; по умолчанию `шт`. Для `tasks` игнорируется. |
| `tags` | array[string] | нет | До 10 тегов, каждый 1..32; приводится к нижнему регистру, дубликаты удаляются. По умолчанию `[]`. |
| `subtasks` | array[object] | нет | До 50; элемент `{"title": string 1..200, "status"?: "open"\|"done"}`; `status` по умолчанию `open`. |
| `recurrence` | object | нет | Только для списков `tasks`; поля — как у `recurrence` в ответе, но `interval`, `byweekday`, `until`, `timezone`, `anchor_date`, `month_rule` необязательны. |
| `source` | string | нет | `ItemSource`; по умолчанию `api`. Компонент для `todo`-сущности передаёт `ha_todo`. |

Правила `recurrence` на входе:

| Поле | Обязательно | Ограничения |
| --- | --- | --- |
| `freq` | да | `daily`, `weekly`, `monthly`, `yearly`. |
| `interval` | нет | 1..365; по умолчанию 1. |
| `byweekday` | нет | Для `weekly` — минимум один день 1..7; для остальных должен быть пустым. |
| `until` | нет | `YYYY-MM-DD`; не раньше `anchor_date` и не позже чем через 60 месяцев от неё. |
| `timezone` | нет | IANA; по умолчанию timezone household. |
| `anchor_date` | нет | `YYYY-MM-DD`; по умолчанию `due_date`/дата `due_at` или сегодня в timezone household. |
| `month_rule` | нет | `anchor_day`/`last_day`; только для `monthly`; по умолчанию `anchor_day`. |

Если у повторяющейся позиции не задан срок, сервер вычисляет первое вхождение
(вседневную дату) не раньше сегодня. `last_day` приводит `due_date` к
последнему дню месяца.

Ответ `201` — созданный `ItemResponse`.

Коды: `201`; `400` (невалидный UUID в пути); `401`; `403`; `404` (список не
существует, чужой, удалён или архивирован); `422` (валидация полей, чужой
исполнитель, повторение в не-`tasks` списке, `due_at`+`due_date`); `500`.

### 4.3 `GET /api/v1/items/{id}`

Scope: `ha:read`. Параметров нет. Ответ `200` — `ItemResponse`.

Коды: `200`; `400` (невалидный UUID в пути); `401`; `403`; `404` (нет, чужой
или мягко удалён); `500`.

Позиция архивного списка по этому маршруту **доступна** (`200`): проверка
архива есть только у операций со списком и у `PATCH`.

### 4.4 `PATCH /api/v1/items/{id}`

Scope: `ha:write`. `Content-Type: application/json`.

Тело:

| Поле | Тип | Nullable | Семантика |
| --- | --- | --- | --- |
| `version` | integer | нет | Обязательна. Ожидаемая текущая версия. |
| `title` | string | да | 1..200. `null` игнорируется (очистить название нельзя). |
| `notes` | string | да | 0..2000. `null` очищает. |
| `priority` | string | да | `low`/`normal`/`high`/`urgent`. `null` игнорируется. |
| `due_at` | string | да | RFC 3339. `null` очищает `due_at`. |
| `due_date` | string | да | `YYYY-MM-DD`. `null` очищает `due_date`. |
| `assignee_user_id` | uuid | да | `null` снимает исполнителя; значение проверяется на активного участника. |
| `quantity` | number | да | `null` → `1`; только для `shopping`/`checklist`, для `tasks` игнорируется. |
| `unit` | string | да | `null` → `шт`; только для `shopping`/`checklist`, для `tasks` игнорируется. |
| `tags` | array[string] | да | Заменяет весь набор тегов. `null` игнорируется. |
| `subtasks` | array[object] | да | Заменяет весь набор подзадач. `null` игнорируется. |
| `recurrence` | object | да | `null` удаляет повторение; объект заменяет правило. Только для `tasks`. |
| `list_id` | uuid | да | Перенос в другой список того же `kind`; целевой список должен быть неудалённым и неархивным. `null` игнорируется. |

Важные правила:

- Нельзя передавать `due_at` и `due_date` одновременно, **даже как `null`**:
  если оба поля присутствуют в JSON, ответ `422`. Чтобы очистить срок,
  передайте `null` только для того поля, которое установлено.
- При установке одного из сроков второе поле очищается автоматически.
- Если позиция лежит в архивном списке, `PATCH` даёт `404`.
- `complete`, `reopen`, `DELETE` и `undo` архивность списка не проверяют:
  для позиции архивного списка они работают.
- Перенос в список другого `kind` → `422`.

Пример:

```json
{"version": 2, "title": "Оплатить интернет", "due_date": "2026-11-06"}
```

Ответ `200` — обновлённый `ItemResponse` с `version = 3`.

Коды: `200`; `400` (невалидный UUID); `401`; `403`; `404` (нет, чужой,
удалён или список архивный); `409` (несовпадение `version`, см. §1.4);
`422` (валидация, оба срока, чужой исполнитель, смена `kind`); `500`.

### 4.5 `DELETE /api/v1/items/{id}`

Scope: `ha:write`. Тела нет.

Мягко удаляет позицию: `deleted_at = updated_at = now`, `version += 1`.
Ответ `204` без тела. Повторный вызов для уже удалённой позиции тоже `204`
(идемпотентно).

Коды: `204`; `400` (невалидный UUID); `401`; `403`; `404` (нет/чужой);
`500`.

После удаления `GET /items/{id}`, `PATCH`, `complete`, `reopen` дают `404`;
`undo` ещё может восстановить позицию.

### 4.6 `POST /api/v1/items/{id}/complete`

Scope: `ha:write`. Тело необязательно:

```json
{"version": 2}
```

Либо тело отсутствует (лучше без `Content-Type`), либо это валидный
JSON-объект, например `{}`. Если передан `Content-Type: application/json` с
пустым телом, axum отвечает `400` текстом без конверта (см. §1.4).

| Поле | Тип | Обязательно | Описание |
| --- | --- | --- | --- |
| `version` | integer | нет | Если передана и не совпала — `409`. |

Поведение:

- Обычная позиция: `status = done`, `completed_at = now`,
  `completed_by_user_id = null` (актор API без пользователя), `version += 1`.
- Уже завершённая: `200` с текущей позицией, `version` не меняется
  (идемпотентно).
- Повторяющаяся: завершение не ставит `done`, а переносит срок на следующее
  вхождение; `status` остаётся `open`, `completed_at` заполняется,
  `version += 1`. Если серия закончилась (`until`), позиция становится `done`.

Ответ `200` — `ItemResponse`.

Коды: `200`; `400`; `401`; `403`; `404` (нет/чужой/удалён); `409`
(несовпадение `version`; `details` — как в §1.4); `500`.

### 4.7 `POST /api/v1/items/{id}/reopen`

Scope: `ha:write`. Тело необязательно, то же `{"version"?: int}`. Либо тело
отсутствует (лучше без `Content-Type`), либо это валидный JSON-объект, например
`{}`; с `Content-Type: application/json` и пустым телом axum отвечает `400`
текстом без конверта.

Открывает завершённую позицию: `status = open`, восстанавливает `due_at` /
`due_date` из снимка последнего завершения (если он есть), очищает
`completed_at`/`completed_by_user_id`, `version += 1`. Уже открытая позиция
возвращается без изменений (`200`, версия та же).

Ответ `200` — `ItemResponse`.

Коды: `200`; `400`; `401`; `403`; `404`; `409` (несовпадение `version`;
`details` — как в §1.4); `500`.

### 4.8 `POST /api/v1/items/{id}/undo`

Scope: `ha:write`. Тела нет.

Отменяет последнее действие над позицией в течение 10 минут:

- отмена создания → мягкое удаление (`deleted_at` не `null` в ответе);
- отмена завершения/изменения → восстанавливается снимок `before`,
  `version` увеличивается.

Повторная отмена после уже применённой компенсации → `409`. Окно истекло →
`409` с `details.event_id` и `details.created_at`. Позиция менялась после
действия → `409`.

Ответ `200` — `ItemResponse` (может иметь `deleted_at != null`).

Коды: `200`; `400`; `401`; `403`; `404`; `409`; `500`.

## 5. Snapshot

### `GET /api/v1/snapshot`

Scope: `ha:read`.

Query-параметры:

| Параметр | Тип | Обязателен | Описание |
| --- | --- | --- | --- |
| `since` | string | нет | RFC 3339. Курсор из предыдущего ответа (`next_since`). **Отсутствие параметра** означает полную синхронизацию. Пустая строка (`?since=`) — ошибка `422`: параметр нужно именно не передавать. |
| `calendar_end` | string | нет | `YYYY-MM-DD`, исключающий конец календарного горизонта. Без него `calendar_events` пуст. |

Ответ `200`:

| Поле | Тип | Nullable | Описание |
| --- | --- | --- | --- |
| `server_time` | string | нет | Момент формирования ответа, RFC 3339 UTC. |
| `next_since` | string | нет | `server_time - 5 секунд`. Курсор для следующего запроса. |
| `lists` | array | нет | **Всегда полный** список неудалённых списков (включая архивные), объекты `List`. От `since` не зависит. |
| `items` | array | нет | Живые (`deleted_at IS NULL`) позиции, изменённые после `since`; при полной синхронизации — все живые позиции. Объекты `ItemResponse`. |
| `deleted_item_ids` | array[uuid] | нет | Id мягко удалённых позиций, изменённых после `since`. При полной синхронизации — всегда `[]`. |
| `calendar_events` | array | нет | Вхождения календаря, только если передан `calendar_end`; иначе `[]`. Объекты `CalendarEvent` (см. §6). |
| `calendar_truncated` | boolean | нет | `true`, если `calendar_events` усечён до 1000 вхождений. |

Пример полной синхронизации:

```json
{
  "server_time": "2026-11-03T12:00:00Z",
  "next_since": "2026-11-03T11:59:55Z",
  "lists": [
    {
      "id": "019a1b2c-5b6c-7d8e-9f00-1a2b3c4d5e6f",
      "household_id": "019a1b2c-3d4e-7f60-8a9b-0c1d2e3f4a5b",
      "kind": "tasks",
      "name": "Задачи",
      "is_default": true,
      "is_archived": false,
      "position": 0,
      "created_at": "2026-10-01T08:00:00Z",
      "updated_at": "2026-10-01T08:00:00Z",
      "deleted_at": null
    }
  ],
  "items": [
    {
      "id": "019a1b2c-8e9f-7a0b-9c1d-4d5e6f708192",
      "household_id": "019a1b2c-3d4e-7f60-8a9b-0c1d2e3f4a5b",
      "list_id": "019a1b2c-5b6c-7d8e-9f00-1a2b3c4d5e6f",
      "title": "Оплатить интернет",
      "notes": null,
      "status": "open",
      "priority": "normal",
      "due_at": null,
      "due_date": "2026-11-05",
      "assignee_user_id": null,
      "created_by_user_id": null,
      "source": "ha_todo",
      "capture_id": null,
      "completed_at": null,
      "completed_by_user_id": null,
      "quantity": null,
      "unit": null,
      "position": 0,
      "version": 1,
      "recurrence": null,
      "created_at": "2026-10-20T09:00:00Z",
      "updated_at": "2026-10-20T09:00:00Z",
      "deleted_at": null,
      "tags": [],
      "subtasks": [],
      "subtask_progress": {"done": 0, "total": 0}
    }
  ],
  "deleted_item_ids": [],
  "calendar_events": [],
  "calendar_truncated": false
}
```

Пример инкрементального ответа после удаления позиции:

```json
{
  "server_time": "2026-11-03T12:01:00Z",
  "next_since": "2026-11-03T12:00:55Z",
  "lists": [],
  "items": [],
  "deleted_item_ids": ["019a1b2c-8e9f-7a0b-9c1d-4d5e6f708192"],
  "calendar_events": [],
  "calendar_truncated": false
}
```

`lists` и `items` сокращены до `[]` для краткости; в реальном ответе они
содержат полный набор списков и изменённые живые позиции.

### 5.1 Полная синхронизация

Полная синхронизация — это запрос **без** параметра `since`:

- `items` — все живые позиции household (включая позиции архивных списков);
- `deleted_item_ids` — пуст;
- `lists` — все неудалённые списки;
- календарь — только если передан `calendar_end`.

Клиент должен заменить локальное состояние: позиции, которых нет в `items`,
удалить; списки, которых нет в `lists`, удалить. Так обрабатываются удаления,
совершённые до первого запроса или вне окна курсора.

### 5.2 Инкрементальная синхронизация

Если `since` передан:

- `items` — живые позиции с `updated_at > since`;
- `deleted_item_ids` — id позиций с `deleted_at IS NOT NULL` и
  `updated_at > since`;
- `lists` — по-прежнему **полный** список, а не изменения;
- `calendar_events` — по-прежнему полный горизонт от сегодня, а не изменения.

Применение:

1. Обновить/добавить позиции из `items` по `id` (не по `version`: версия
   нужна только для записи).
2. Удалить локальные позиции из `deleted_item_ids`.
3. Синхронизировать набор списков с `lists` (см. §5.3).
4. Заменить календарь, если он запрашивался.

`next_since = server_time - 5 s` создаёт перекрытие: позиции, изменённые в
последние 5 секунд, придут повторно. Это нормально: клиент применяет
изменения идемпотентно по `id`, а `deleted_item_ids` дедуплицирует.

### 5.3 Списки и их удаление

В ответе **нет** `deleted_list_ids` и версии списка. Удаление списка
определяется по отсутствию его `id` в полном массиве `lists`. Поэтому:

- сравнивайте `lists` с локальным набором на каждом ответе, даже
  инкрементальном;
- архивный список остаётся в `lists` с `is_archived: true`; сущность `todo`
  для него нужно убрать или пометить недоступной;
- удалённый список исчезает из `lists`; его позиции приходят в
  `deleted_item_ids` (при удалении списка сервер мягко удаляет их и поднимает
  `version`);
- позиции архивных списков продолжают приходить в `items`. Клиент должен
  игнорировать позиции, чей `list_id` отсутствует в `lists` или помечен
  `is_archived: true`.

### 5.4 Календарь в снапшоте

`calendar_end` задаёт исключающий конец горизонта; начало — сегодня в timezone
household. Если `calendar_end <= сегодня`, ответ успешен, но
`calendar_events = []` (это не ошибка, в отличие от `GET /calendar`).
Если `calendar_end` дальше чем на 366 дней от сегодня — `422`.

Если вхождений больше 1000, сервер сортирует их и оставляет первые 1000,
выставляя `calendar_truncated: true`; ошибки нет, чтобы не ломать
`todo`-синхронизацию. Полный горизонт в этом случае нужно запрашивать через
`GET /calendar` (но и там лимит 1000 вхождений — см. §6).

### 5.5 Когда обязательна полная синхронизация

Сервер различает только «`since` передан» и «`since` не передан». Полная
синхронизация нужна клиенту:

- при первом запуске и после потери сохранённого курсора;
- при смене токена/интеграции;
- если сервер ответил `422` на `since` (курсор повреждён) — повторить без
  `since`;
- не реже раза в сутки (политика компонента из `design.md`): мягко удалённые
  позиции хранятся 30 дней (`RETENTION_DELETED_DAYS`, по умолчанию 30), после
  чего физически удаляются и перестают попадать в `deleted_item_ids`. Курсор
  старше retention без полной синхронизации может пропустить удаления.

Курсор (`next_since`) следует сохранять между перезапусками HA; при сомнении
передавать его не нужно.

Коды: `200`; `401`; `403`; `422` (`since` не RFC 3339, `calendar_end` не
`YYYY-MM-DD`, горизонт больше 366 дней); `500`.

## 6. Calendar

### `GET /api/v1/calendar`

Scope: `ha:read`.

Query-параметры (оба обязательны):

| Параметр | Тип | Описание |
| --- | --- | --- |
| `start` | string | `YYYY-MM-DD`, начало интервала включительно. |
| `end` | string | `YYYY-MM-DD`, конец интервала **исключающий**. Должен быть строго больше `start`; интервал не больше 366 дней. |

Ответ `200`:

```json
{"events": []}
```

Элементы массива — объекты `CalendarEvent` (см. таблицу ниже).

`CalendarEvent`:

| Поле | Тип | Nullable | Описание |
| --- | --- | --- | --- |
| `item_id` | uuid | нет | Позиция. |
| `list_id` | uuid | нет | Список. |
| `occurrence_key` | string | нет | Детерминированный ключ вхождения. Для all-day: `<item_id>:<YYYY-MM-DD>`; для timed: `<item_id>:<локальное время>` или `<item_id>:<UTC>` у одиночного события. |
| `start` | string | нет | Для all-day — `YYYY-MM-DD`; для timed — RFC 3339 UTC. |
| `end` | string | нет | Для all-day — следующий день `YYYY-MM-DD`; для timed — `start + 1 час`, RFC 3339 UTC. |
| `all_day` | boolean | нет | `true` для `due_date`, `false` для `due_at`. |
| `title` | string | нет | Название позиции. |
| `status` | string | нет | `open` или `done`. |

Пример:

```json
{
  "events": [
    {
      "item_id": "019a1b2c-8e9f-7a0b-9c1d-4d5e6f708192",
      "list_id": "019a1b2c-5b6c-7d8e-9f00-1a2b3c4d5e6f",
      "occurrence_key": "019a1b2c-8e9f-7a0b-9c1d-4d5e6f708192:2026-11-10",
      "start": "2026-11-10",
      "end": "2026-11-11",
      "all_day": true,
      "title": "Вседневная",
      "status": "open"
    },
    {
      "item_id": "019a1b2c-a1b2-7c3d-8e4f-5e6f708192a3",
      "list_id": "019a1b2c-5b6c-7d8e-9f00-1a2b3c4d5e6f",
      "occurrence_key": "019a1b2c-a1b2-7c3d-8e4f-5e6f708192a3:2026-11-10T09:00:00Z",
      "start": "2026-11-10T09:00:00Z",
      "end": "2026-11-10T10:00:00Z",
      "all_day": false,
      "title": "Встреча",
      "status": "open"
    }
  ]
}
```

Правила:

- В выдачу попадают только позиции списков `kind=tasks`, неудалённых и
  неархивных, у которых есть `due_at`, `due_date` или повторение.
- Завершённые позиции тоже попадают (`status: "done"`); фильтрация — на
  стороне клиента/карточки.
- Повторяющаяся позиция разворачивается в отдельные события на интервале
  `[start, end)`; `until` учитывается. При завершении серии следующее
  вхождение — первое строго позже текущего срока и момента завершения;
  пропущенные вхождения не накапливаются.
- Одиночное timed-событие относится к локальной дате household timezone;
  повторяющееся — к локальной дате `recurrence.timezone`.
- Порядок: локальная дата начала, затем all-day раньше timed, затем момент
  времени, затем `occurrence_key`.
- Больше 1000 вхождений на запрос → `422 validation_error` (в отличие от
  снапшота, где календарь усекается).

Коды: `200`; `401`; `403`; `422` (нет `start`/`end`, неверный формат,
`end <= start`, интервал > 366 дней, > 1000 вхождений); `500`.

## 7. Capture

Все маршруты `/capture` и `/captures/*` требуют scope `capture:write`.
Если capture выключен (`CAPTURE_API_ENABLED=false`), маршруты не
регистрируются и дают `404 not_found` (включая `undo-last`), без вызова LLM и
разбора тела.

### 7.1 `POST /api/v1/capture`

Scope: `capture:write`. `Content-Type: application/json`. Лимит тела —
16 КиБ (`413 payload_too_large`).

Заголовки:

| Заголовок | Обязателен | Описание |
| --- | --- | --- |
| `Authorization` | да | Bearer-токен. |
| `Content-Type` | да | `application/json`. |
| `Idempotency-Key` | нет | Ключ повтора. Если заданы и заголовок, и поле `idempotency_key`, значения должны совпасть, иначе `422`. |
| `X-Request-Id` | нет | Пробрасывается в ответ. |

Тело (`deny_unknown_fields` — неизвестное поле даёт `422`):

| Поле | Тип | Обязательно | Ограничения |
| --- | --- | --- | --- |
| `text` | string | да | После trim 1..`CAPTURE_MAX_TEXT_CHARS` (по умолчанию 2000). |
| `source` | string | да | `ha_voice`, `ha_automation`, `api`, `shortcut`. `telegram` запрещён. |
| `mode` | string | да | `auto` или `draft`. |
| `hint` | string | нет | `tasks`, `shopping`, `checklist` — тип списка по умолчанию для всех элементов. |
| `list_id` | uuid | нет | Конкретный список household; приоритетнее `hint`. Неизвестный, чужой или архивный список → `422` (`Unknown list`). |
| `idempotency_key` | string | нет | 1..128 видимых ASCII (`0x21..0x7E`, без пробелов). Пустая строка = без ключа. |

```json
{
  "text": "завтра купить молоко и хлеб",
  "source": "ha_voice",
  "mode": "auto",
  "hint": "shopping",
  "list_id": null,
  "idempotency_key": "019a1b2c-3d4e-7f60-8a9b-0c1d2e3f4a5b"
}
```

Ответ `200` (не `201`):

| Поле | Тип | Nullable | Описание |
| --- | --- | --- | --- |
| `capture_id` | uuid | нет | Идентификатор capture; у черновика совпадает с `draft_id`. У `rejected` — сгенерированный id, ресурса capture с ним нет. |
| `status` | string | нет | `applied`, `draft`, `rejected`. |
| `fallback` | boolean | нет | `true`, если создана одна позиция из исходного текста без LLM. |
| `speech` | string | нет | Русская фраза для озвучивания, до 200 символов. |
| `items` | array | нет | Созданные элементы (`applied`) или элементы черновика (`draft`); у `rejected` — `[]`. |
| `items[].id` | uuid | да | Id созданной позиции; у черновика `null`. |
| `items[].title` | string | нет | Название. |
| `items[].list_id` | uuid | да | Список; у черновика может быть `null`, если список ещё не выбран. |
| `items[].kind` | string | нет | `tasks`, `shopping`, `checklist`. |
| `items[].due_date` | string | да | `YYYY-MM-DD`. |
| `items[].due_at` | string | да | RFC 3339 UTC. |
| `draft_id` | uuid | да | Id черновика при `status=draft`; иначе `null`. |
| `expires_at` | string | да | TTL черновика (15 минут) при `status=draft`; иначе `null`. |
| `clarifications` | array[string] | нет | Вопросы модели, до 3; может быть непустым и у `applied`. |

Пример `applied`:

```json
{
  "capture_id": "019a1b2c-b2c3-7d4e-9f5a-6f708192a3b4",
  "status": "applied",
  "fallback": false,
  "speech": "Добавил 2 покупки: молоко, хлеб.",
  "items": [
    {
      "id": "019a1b2c-c3d4-7e5f-8a6b-708192a3b4c5",
      "title": "молоко",
      "list_id": "019a1b2c-6c7d-7e8f-9a0b-2b3c4d5e6f70",
      "kind": "shopping",
      "due_date": null,
      "due_at": null
    },
    {
      "id": "019a1b2c-d4e5-7f6a-9b7c-8192a3b4c5d6",
      "title": "хлеб",
      "list_id": "019a1b2c-6c7d-7e8f-9a0b-2b3c4d5e6f70",
      "kind": "shopping",
      "due_date": null,
      "due_at": null
    }
  ],
  "draft_id": null,
  "expires_at": null,
  "clarifications": []
}
```

Пример `draft`:

```json
{
  "capture_id": "019a1b2c-e5f6-7a7b-8c8d-92a3b4c5d6e7",
  "status": "draft",
  "fallback": false,
  "speech": "Сохранил черновик: 2 покупки.",
  "items": [
    {
      "id": null,
      "title": "молоко",
      "list_id": "019a1b2c-6c7d-7e8f-9a0b-2b3c4d5e6f70",
      "kind": "shopping",
      "due_date": null,
      "due_at": null
    }
  ],
  "draft_id": "019a1b2c-e5f6-7a7b-8c8d-92a3b4c5d6e7",
  "expires_at": "2026-10-31T10:15:00Z",
  "clarifications": []
}
```

Пример `rejected` (ничего не создано; `200`, не ошибка):

```json
{
  "capture_id": "019a1b2c-f6a7-7b8c-9d9e-a3b4c5d6e7f8",
  "status": "rejected",
  "fallback": false,
  "speech": "Не смог добавить: дата уже прошла.",
  "items": [],
  "draft_id": null,
  "expires_at": null,
  "clarifications": []
}
```

Режимы:

- `auto` — валидный разбор применяется сразу одной транзакцией. Любая
  жёсткая ошибка отклоняет весь capture (`status=rejected`). Неизвестный
  список заменяется списком по умолчанию, неизвестный/неоднозначный
  исполнитель снимается, пустые необязательные поля отбрасываются.
- `draft` — сохраняется черновик с TTL 15 минут; элементы не создаются.
  Черновик подтверждается/отменяется через `/captures/{id}/*`.

Созданные позиции получают `source`: `ha_voice` → `ha_voice`,
`ha_automation` → `ha_automation`, `api` и `shortcut` → `api`.

Разбор ограничен 20 элементами. Если модель вернула больше, ответ —
`status=rejected` (`200`), ничего не создаётся.

Fallback (`fallback=true`, `status=applied`): одна позиция из исходного
текста, без срока/тегов/исполнителя. Список выбирается по `list_id`, затем
`hint` (список по умолчанию типа), затем список задач по умолчанию. Работает
в `auto`, если включён `CAPTURE_VOICE_FALLBACK_RAW` (по умолчанию `true`);
имя настройки не ограничивает её источником `ha_voice`.

Коды:

| Код | Когда |
| --- | --- |
| `200` | `applied`, `draft` или `rejected`. |
| `401` | Нет/невалидный токен; проверяется до тела. |
| `403` | Нет `capture:write`; проверяется до тела. |
| `404` | Capture API выключен. |
| `409` | Тот же `Idempotency-Key` ещё обрабатывается (`Retry-After: 2`) или резерв перехвачен другим запросом (`Retry-After: 2`). |
| `413` | Тело > 16 КиБ. |
| `415` | Нет/неверный `Content-Type`. |
| `422` | Битый JSON, неизвестное поле, пустой/длинный `text`, неверные `source`/`mode`/`hint`, неизвестный/чужой/архивный `list_id`, неверный `Idempotency-Key`, разные значения `Idempotency-Key` в заголовке и поле, тот же ключ с другим телом. |
| `429` | Больше `CAPTURE_RATE_LIMIT_PER_MIN` (по умолчанию 10) запросов в минуту на токен, либо квота LLM исчерпана в `draft`. Всегда с `Retry-After`. |
| `500` | Внутренняя ошибка/БД. |
| `503` | LLM не настроена, сбой/таймаут LLM, открыт предохранитель или исчерпана квота, когда fallback запрещён. |

Таймаут LLM: 8 секунд для `source=ha_voice` (`CAPTURE_VOICE_TIMEOUT_MS`),
20 секунд для остальных. HTTP-таймаут клиента должен быть больше.

#### Идемпотентность

- Ключ уникален в рамках `(household, source)`.
- Ключ можно передать заголовком или полем; если заданы оба и различаются —
  `422`; если совпадают — используется это значение.
- Повтор с тем же ключом и тем же `text`/`source`/`mode`/`hint`/`list_id`
  возвращает сохранённый первый ответ целиком и не вызывает LLM.
- Тот же ключ с другим телом → `422`.
- Пока первый запрос обрабатывается → `409` с `Retry-After: 2`.
- Ошибка `5xx`/`503` или обрыв клиента освобождает ключ; повтор
  обрабатывается заново.
- `Idempotency-Key` учитывается только на `POST /capture`; у действий
  `/captures/*` его нет.

#### Лимит частоты и квота

- Лимит частоты — скользящее окно 60 секунд на токен,
  `CAPTURE_RATE_LIMIT_PER_MIN` (по умолчанию 10, конфиг 1..1000).
  `429 rate_limited`, `Retry-After` — секунды до освобождения окна.
- Квота LLM — `LLM_DAILY_QUOTA` (по умолчанию 200) вызовов на household за
  скользящие 24 часа. При исчерпании:
  - `auto` + fallback разрешён → `200` с `fallback=true`;
  - `auto` + fallback запрещён → `503 service_unavailable`;
  - `draft` → `429 rate_limited` с `Retry-After: 3600`.
- LLM выключена/не настроена, предохранитель открыт, сбой или таймаут LLM:
  - `auto` + fallback разрешён → `200` с `fallback=true`;
  - иначе → `503 service_unavailable`.

### 7.2 `POST /api/v1/captures/{id}/confirm`

Scope: `capture:write`. Тела нет (переданное тело игнорируется).

Подтверждает внешний черновик: все его элементы создаются одной транзакцией.
Работает только с capture своего household и внешних источников
(`ha_voice`, `ha_automation`, `api`, `shortcut`). Чужой, несуществующий
capture и Telegram-черновик → `404`.

Ответ `200` — та же форма, что `POST /capture` со `status=applied`:

- `capture_id` — id подтверждённого capture;
- `status` — `applied`;
- `fallback` — из сохранённого черновика;
- `speech` — пересчитанная фраза;
- `items` — созданные позиции с непустыми `id`/`list_id`;
- `draft_id` и `expires_at` — `null`;
- `clarifications` — `[]`.

Повторный `confirm` уже применённого capture возвращает те же живые элементы
и ничего не создаёт. Если живых элементов не осталось (capture отменён через
`undo` или они удалены), ответ — `409` (`Capture was undone`).

Коды: `200`; `400`; `401`; `403`; `404`; `409` (черновик отменён, уже
применён и отменён через undo); `410` (черновик старше 15 минут);
`422` (элементы черновика стали невалидными); `500`.

### 7.3 `POST /api/v1/captures/{id}/cancel`

Scope: `capture:write`. Тела нет.

Отменяет ожидающий черновик. Повторная отмена и отмена истёкшего черновика —
успех. Подтверждённый capture отменить нельзя (для него есть `undo`).

Ответ `200`:

```json
{
  "capture_id": "019a1b2c-e5f6-7a7b-8c8d-92a3b4c5d6e7",
  "status": "cancelled",
  "speech": "Отменил черновик."
}
```

| Поле | Тип | Nullable | Описание |
| --- | --- | --- | --- |
| `capture_id` | uuid | нет | Id capture. |
| `status` | string | нет | Всегда `cancelled`. |
| `speech` | string | нет | Русская фраза. |

Коды: `200`; `400`; `401`; `403`; `404`; `409` (capture уже применён);
`500`.

### 7.4 `POST /api/v1/captures/{id}/undo`

Scope: `capture:write`. Тела нет.

Мягко удаляет все живые элементы применённого capture (`auto` или
подтверждённого `draft`). Работает 10 минут после применения и только пока
элементы не менялись (`version = 1`, `status = open`). Повторный `undo` —
`200` с `removed = 0`, более ранние добавления не трогаются.

Ответ `200`:

```json
{
  "capture_id": "019a1b2c-b2c3-7d4e-9f5a-6f708192a3b4",
  "status": "undone",
  "removed": 2,
  "speech": "Отменил добавление: 2 элемента."
}
```

| Поле | Тип | Nullable | Описание |
| --- | --- | --- | --- |
| `capture_id` | uuid | нет | Id capture. |
| `status` | string | нет | Всегда `undone`. |
| `removed` | integer | нет | Сколько позиций мягко удалено этим запросом. |
| `speech` | string | нет | Русская фраза. |

Коды: `200`; `400`; `401`; `403`; `404`; `409` (capture не применён или
отменён, окно 10 минут закрыто, элементы изменены/завершены); `500`.

### 7.5 `POST /api/v1/captures/undo-last`

Scope: `capture:write`. Тела нет.

Отменяет последний применённый capture household с `source=ha_voice` за
последние 10 минут. Учитываются только `status=applied`; ожидающие черновики
и другие источники (включая `ha_automation`, `api`, `shortcut`, Telegram) не
считаются. Выбор — по `applied_at` (для `auto` — момент применения, для
`draft` — момент подтверждения).

Ответ `200` (даже если отменять нечего):

```json
{
  "capture_id": "019a1b2c-b2c3-7d4e-9f5a-6f708192a3b4",
  "status": "undone",
  "removed": 2,
  "speech": "Отменил добавление: 2 элемента."
}
```

| Поле | Тип | Nullable | Описание |
| --- | --- | --- | --- |
| `capture_id` | uuid | да | Id отменённого capture; `null`, если за окно не нашлось голосового capture. |
| `status` | string | нет | Всегда `undone`. |
| `removed` | integer | нет | Сколько позиций мягко удалено. |
| `speech` | string | нет | Русская фраза; при отсутствии capture — «Нечего отменять…». |

Поведение:

- нет голосового capture за 10 минут → `capture_id: null`, `removed: 0`;
- последний уже отменён → тот же `capture_id`, `removed: 0`, более ранние
  capture не отменяются;
- элементы последнего capture изменены/завершены → `409`, ничего не
  удаляется.

Коды: `200`; `401`; `403`; `404` (capture API выключен); `409` (элементы
изменены/завершены); `500`.

## 8. Чего в API нет

Чтобы клиент не изобретал несуществующие вызовы:

| Отсутствует | Что делать клиенту |
| --- | --- |
| `If-Match`/`ETag`/`If-None-Match` | Передавать `version` в теле `PATCH`; обрабатывать `409`. |
| `deleted_list_ids` в снапшоте | Сравнивать полный массив `lists` с локальным набором. |
| `version` у списка | Считать `PATCH /lists/{id}` last-write-wins. |
| `GET /api/v1/lists/{id}` (один список) | Брать список из `GET /lists` или `snapshot.lists`. |
| `GET /api/v1/captures/{id}` (просмотр черновика) | Использовать ответ `POST /capture` (`status=draft`) и действия `confirm`/`cancel`. |
| Пагинация `GET /lists` и `GET /lists/{id}/items` | Ответы возвращаются целиком. |
| `PUT` | Использовать `PATCH`. |
| Server push / webhook из Tasklerq в HA | Только polling (`snapshot`, `calendar`). |
| Отдельный заголовок версии API | Версия зашита в путь `/api/v1`. |
| `Retry-After` на успешных ответах | Только на `429` и `409` с занятым ключом. |

## 9. Минимальный сценарий клиента

1. `GET /household` — получить timezone и `household.id` для `unique_id`.
2. `GET /snapshot?calendar_end=<today+horizon>` без `since` — полная
   синхронизация; сохранить `next_since`.
3. Создать/обновить сущности `todo` по неархивным спискам и `calendar` по
   событиям.
4. Каждые 60 секунд (30–300): `GET /snapshot?since=<cursor>&calendar_end=…`;
   применить `items`, `deleted_item_ids`, заменить набор списков, обновить
   календарь; сохранить новый `next_since`.
5. Запись из HA:
   - создать: `POST /lists/{list_id}/items` с `source=ha_todo`;
   - изменить: `PATCH /items/{id}` с `version` из последнего снапшота;
   - завершить/открыть: `POST /items/{id}/complete|reopen` (можно с
     `version`);
   - удалить: `DELETE /items/{id}`;
   - при `409` — немедленный refresh и показать конфликт, не перезаписывать
     молча.
6. Голос/автоматизация: `POST /capture` с `Idempotency-Key`; для `draft` —
   `confirm`/`cancel` по `capture_id`; отмена последнего голосового —
   `POST /captures/undo-last`.
