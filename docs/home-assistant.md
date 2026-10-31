# Home Assistant и Tasklerq

## 1. Зачем нужен custom component

Home Assistant не умеет создавать сущности `todo` и `calendar` через MQTT
discovery. Шаблонные сенсоры и `rest`-интеграция дают данные, но не нативные
сервисы `todo.*` и `calendar.*`. Поэтому Tasklerq поставляет тонкий Python-
компонент `tasklerq`, который:

- создаёт `todo`-сущность на каждый список;
- создаёт одну read-only `calendar`-сущность для задач со сроком;
- синхронизирует изменения через REST API Tasklerq;
- отдаёт дополнительные поля (приоритет, теги, исполнитель, повторение) в
  атрибуте `tasklerq_items`.

Компонент не хранит данные и не является source of truth. Все изменения
происходят в PostgreSQL Tasklerq.

## 2. Совместимость

| Требование | Значение |
| --- | --- |
| Home Assistant | 2026.10.4 (проверено); минимум 2026.10.0 |
| Тип установки | HA Container (Docker), `network_mode: host` |
| Python | Версия, поддерживаемая вашей версией HA |
| HACS | Установлен |
| Tasklerq | 0.1.0+ |
| Сеть | HA → Tasklerq: `http://127.0.0.1:18080`; Tasklerq → HA: `http://host.docker.internal:8123` |

Минимум `2026.10.0` задан в `hacs.json` ключом `homeassistant` и
проверяется при установке компонента в репозитории
[`tasklerq-ha`](https://github.com/StellaLupus/tasklerq-ha); проверено на
2026.10.4.

Компонент **не требует** Supervisor, add-on или ingress. Это важно для HA
Container.

## 3. Установка

### Вариант A. HACS (рекомендуется)

1. HACS → **Integrations** → меню → **Custom repositories**.
2. Добавьте репозиторий `https://github.com/StellaLupus/tasklerq-ha`,
   категория **Integration**.
3. Установите **Tasklerq**.
4. Перезапустите Home Assistant.
5. **Настройки → Устройства и службы → Добавить интеграцию → Tasklerq**.

HACS уже установлен в этой инсталляции HA. Если он недоступен, используйте
ручной вариант.

### Вариант B. Вручную

1. Скопируйте каталог `custom_components/tasklerq` из репозитория
   [`tasklerq-ha`](https://github.com/StellaLupus/tasklerq-ha) (или из его
   релиза) в
   `/volume2/docker/homeassistant/custom_components/tasklerq` на NAS
   (внутри контейнера HA это `/config/custom_components/tasklerq`).
2. Проверьте структуру:

```text
/config/custom_components/tasklerq/
├── __init__.py
├── manifest.json
├── config_flow.py
├── api.py
├── coordinator.py
├── todo.py
├── calendar.py
├── diagnostics.py
└── translations/ru.json
```

3. Перезапустите Home Assistant.
4. Добавьте интеграцию через UI.

### Обновление

- HACS: обновить и перезапустить HA.
- Вручную: заменить каталог и перезапустить HA.
- После обновления проверьте **Настройки → Устройства и службы → Tasklerq →
  Диагностика**.

### Headless-режим без Telegram

Компонент работает с сервисом, запущенным в `TELEGRAM_MODE=disabled` без
`TELEGRAM_BOT_TOKEN` и webhook-секрета. В этом режиме доступны `todo`,
`calendar`, внешний capture и отмена последнего голосового capture
(`POST /api/v1/captures/undo-last`); Telegram-бот и исходящие
Telegram-уведомления отключены. Серверная часть сценария из
[`ha-api.md`](ha-api.md#9-минимальный-сценарий-клиента) проверяется
интеграционным тестом [`tests/headless_ha.rs`](../tests/headless_ha.rs).

## 4. Настройка

В config flow:

| Поле | Пример | Комментарий |
| --- | --- | --- |
| URL Tasklerq | `http://127.0.0.1:18080` | HA работает в `network_mode: host`; без слэша в конце. |
| API-токен | `tlq_...` | Создаётся в Tasklerq CLI. |
| Интервал polling | `60` | 30–300 секунд. |
| Горизонт календаря | `30` | 7–90 дней. |

URL зависит от сети:

| Сценарий | URL |
| --- | --- |
| HA в `network_mode: host`, Tasklerq на loopback (текущая среда) | `http://127.0.0.1:18080` |
| HA и Tasklerq в одной bridge-сети | `http://tasklerq:8080` |
| HA в bridge, Tasklerq на хосте | `http://host.docker.internal:18080` (нужен `extra_hosts`) |
| Tasklerq за Synology Reverse Proxy | `https://tasklerq.stellalupus.ru` |

Host-порт `18080` выбран потому, что `8080` на NAS занят Zigbee2MQTT.
Внутренний порт контейнера Tasklerq остаётся `8080`.

Токен создаётся один раз:

```bash
tasklerq admin token create \
  --name "Home Assistant" \
  --scopes ha:read,ha:write,capture:write
```

Сохраните токен: повторно он не показывается. `capture:write` нужен сервису
`tasklerq.capture` и голосовому сценарию. Для read-only дашборда достаточно
`ha:read`.

### Reauth

Если Tasklerq вернул `401`, HA покажет **Переаутентифицироваться**. Введите
новый токен. Старый можно отозвать:

```bash
tasklerq admin token revoke --name "Home Assistant"
```

### Options

После настройки можно изменить:

- интервал polling (30–300 с);
- горизонт календаря (7–90 дней).

Изменения применяются без перезапуска HA.

## 5. Сущности

### 5.1 `todo` по спискам

Для каждого неархивированного списка создаётся сущность:

```text
todo.zadachi
todo.pokupki
todo.dacha
```

`unique_id`: `tasklerq:<household_uuid>:<list_uuid>`.

Поддерживаются:

- создание позиции;
- редактирование названия, описания, срока;
- завершение и возврат;
- удаление (мягкое, с возможностью восстановления в Tasklerq).

Маппинг:

| Home Assistant | Tasklerq |
| --- | --- |
| `summary` | `title` |
| `description` | `notes` |
| `due` (date) | `due_date` (all-day) |
| `due` (datetime) | `due_at` |
| `status` | `open` / `done` |

Дополнительные поля:

```yaml
tasklerq_items:
  - id: "019..."
    title: "Оплатить интернет"
    status: "open"
    priority: "high"
    due: "2026-11-05T18:00:00+03:00"
    assignee: "Алексей"
    tags: ["дом", "финансы"]
    recurrence:
      freq: "monthly"
      interval: 1
      until: null
```

Эти поля доступны в шаблонах и автоматизациях, но не отображаются в стандартной
карточке `todo-list`. Это ограничение HA, а не потеря данных.

Позиции, созданные голосом или автоматизацией, появляются в тех же `todo`-
сущностях. В `tasklerq_items` есть `source` (`telegram`, `ha_todo`,
`ha_voice`, `ha_automation`, `api`) — по нему можно строить фильтры и
уведомления.

### 5.2 `calendar.tasklerq`

Одна read-only сущность. События:

- задачи с `due_at` — timed events;
- задачи с `due_date` — all-day events;
- повторяющиеся задачи — отдельные события на горизонте;
- завершённые задачи могут отображаться с признаком статуса; фильтрация зависит
  от карточки.

Создание, редактирование и удаление событий через календарь не поддерживаются.
Редактируйте задачу через `todo` или Telegram.

### 5.3 Атрибуты и статусы

- `todo` state — количество незавершённых позиций.
- `calendar` state — `on`/`off` в зависимости от текущего события.
- При недоступности Tasklerq сущности становятся `unavailable`.
- Ошибки записи возвращаются как `HomeAssistantError` с русским текстом.

## 6. Синхронизация

```text
HA coordinator ──GET /api/v1/snapshot?since=cursor──▶ Tasklerq
      │                                                   │
      │◀──────── lists, items, deleted_item_ids, next_since ────┘
      │
      ├── обновляет todo/calendar
      │
      └── PATCH/POST/DELETE ──▶ Tasklerq (version)
```

- Polling по умолчанию 60 с.
- Инкрементальный снапшот по курсору; полная синхронизация при старте, смене
  токена и раз в сутки.
- Запись из HA инициирует внеочередной refresh.
- Конфликт версий: `409` → refresh + ошибка, без молчаливой перезаписи.
- Удалённые в Tasklerq позиции удаляются из HA по `deleted_item_ids`.

Задержка:

- изменения из Telegram → HA: до 60 с (или сразу после записи из HA);
- изменения из HA → Telegram: сразу после успешного PATCH;
- календарь: обновляется вместе с координатором.

## 7. LLM через Home Assistant

Tasklerq вызывает `ai_task.generate_data` в HA. В этой инсталляции уже есть:

- интеграция **OpenRouter**;
- AI Task `ai_task.anthropic_claude_haiku_5_5` (подынтеграция `ai_task_data`,
  модель `anthropic/claude-haiku-5.5`);
- запасная интеграция **Google Generative AI** с AI Task
  `ai_task.google_ai_task_2` (Gemini 3.7 Flash): точная, но медленнее
  голосового таймаута 8 с.

Модель AI Task выбирается в HA, а не в Tasklerq. Убедитесь, что у
подынтеграции `ai_task_data` **не включены Assist-инструменты**
(`llm_hass_api`): LLM-capture не должна управлять устройствами HA.

Настройка:

1. Проверьте сущность `ai_task.anthropic_claude_haiku_5_5` в **Developer Tools → States**.
2. Проверьте действие **Developer Tools → Actions → `ai_task.generate_data`**.
3. Создайте long-lived token HA.
4. Укажите в Tasklerq:

```dotenv
HA_BASE_URL=http://host.docker.internal:8123
HA_TOKEN=eyJ...
HA_AI_TASK_ENTITY_ID=ai_task.anthropic_claude_haiku_5_5
LLM_CAPTURE_ENABLED=true
```

`HA_BASE_URL` — это адрес HA **из контейнера Tasklerq**. Так как HA работает в
`network_mode: host`, используйте `http://host.docker.internal:8123` (нужен
`extra_hosts`) или `http://192.168.10.110:8123`.

5. Отправьте боту тестовую фразу и проверьте черновик.

Если AI Task не настроена, Tasklerq продолжает работать: свободный текст
открывает ручную форму.

Подробно о промпте и схеме: [`llm-capture.md`](llm-capture.md).

## 7.1. Голосовой capture через Assist

Tasklerq можно использовать голосом без Telegram. HA Assist распознаёт речь,
вызывает сервис `tasklerq.capture`, а Tasklerq разбирает текст через AI Task и
создаёт задачи или покупки. Ответ озвучивается. Это основной голосовой сценарий;
если Assist уже настроен, добавьте только пользовательские предложения (custom
sentences) и `intent_script` ниже.

### Сервис

```yaml
action: tasklerq.capture
data:
  text: "завтра купить молоко и хлеб"
  source: ha_voice
  mode: auto
  hint: shopping
response_variable: result
```

Поля:

| Поле | Обязательно | Значение |
| --- | --- | --- |
| `text` | да | Фраза для разбора. |
| `source` | нет | `ha_voice` или `ha_automation`; по умолчанию `ha_automation`. |
| `mode` | нет | `auto` (по умолчанию) или `draft`. |
| `hint` | нет | `tasks`, `shopping`, `checklist`. |
| `list_id` | нет | Конкретный список. |
| `idempotency_key` | нет | Обычно генерируется компонентом. |

Ответ: `status`, `speech`, `items`, `draft_id`, `capture_id`. `speech` —
готовая фраза для озвучивания.

### Подключение к Assist

Нужны два файла: предложения в `custom_sentences` и пакет с `intent_script`.
Объявить фразы в `conversation: intents:` нельзя: `{tasklerq_text}` остался бы
ссылкой на несуществующий слот-лист, и hassil завершил бы разбор с
`MissingListError`. В HA 2026.10 `speech` — это mapping с ключом `text`, а
шаблон рендерится после действий: ответ сервиса доступен только как
`action_response` (его возвращает `stop` с `response_variable`).

`/config/custom_sentences/ru/tasklerq.yaml`:

```yaml
language: ru
intents:
  TasklerqAddTask:
    data:
      - sentences:
          - "добавь задачу {tasklerq_text}"
          - "запиши задачу {tasklerq_text}"
          - "напомни {tasklerq_text}"

  TasklerqAddShopping:
    data:
      - sentences:
          - "добавь в покупки {tasklerq_text}"
          - "купи {tasklerq_text}"
          - "в список покупок {tasklerq_text}"

  TasklerqUndo:
    data:
      - sentences:
          - "отмени последнее добавление"
          - "убери последнюю задачу"

lists:
  tasklerq_text:
    wildcard: true
```

Подключите каталог пакетов в `configuration.yaml`:

```yaml
homeassistant:
  packages: !include_dir_named packages
```

`/config/packages/tasklerq_assist.yaml`:

```yaml
intent_script:
  TasklerqAddTask:
    speech:
      text: >-
        {{ action_response.speech if action_response is defined and action_response.speech is defined and action_response.speech else 'Не удалось добавить задачу. Возможно, она не сохранена: проверьте список в Tasklerq.' }}
    action:
      - action: tasklerq.capture
        data:
          text: "{{ tasklerq_text }}"
          source: ha_voice
          mode: auto
          hint: tasks
        response_variable: response
        continue_on_error: true
      - stop: ""
        response_variable: response

  TasklerqAddShopping:
    speech:
      text: >-
        {{ action_response.speech if action_response is defined and action_response.speech is defined and action_response.speech else 'Не удалось добавить покупки. Возможно, они не сохранены: проверьте список в Tasklerq.' }}
    action:
      - action: tasklerq.capture
        data:
          text: "{{ tasklerq_text }}"
          source: ha_voice
          mode: auto
          hint: shopping
        response_variable: response
        continue_on_error: true
      - stop: ""
        response_variable: response

  TasklerqUndo:
    speech:
      text: >-
        {{ action_response.speech if action_response is defined and action_response.speech is defined and action_response.speech else 'Не удалось отменить последнее добавление. Проверьте Tasklerq.' }}
    action:
      - action: tasklerq.undo_last_capture
        response_variable: response
        continue_on_error: true
      - stop: ""
        response_variable: response
```

Если Tasklerq вернул ошибку, `continue_on_error` пропускает шаг,
`action_response` не появляется, и озвучивается запасная фраза; причина
остаётся в журнале и трассировке HA.

Готовые файлы: [`tasklerq-ha/examples/assist`](https://github.com/StellaLupus/tasklerq-ha/tree/main/examples/assist).

Перезапустите HA и проверьте в **Developer Tools → Assist**:

- «купи молоко и хлеб» → две позиции в «Покупках»;
- «добавь задачу позвонить маме завтра» → задача со сроком;
- «отмени последнее добавление» → отмена последнего голосового capture.

### Поведение

- `auto` создаёт элементы сразу; подтверждение в Telegram не отправляется.
- `tasklerq.undo_last_capture` вызывает `POST /api/v1/captures/undo-last` и
  отменяет последний голосовой capture за 10 минут.
- При недоступности LLM создаётся одна позиция из исходного текста; Assist
  отвечает «Записал как есть».
- Таймаут голосового разбора — 8 секунд, чтобы pipeline успел ответить.
- Если Tasklerq недоступен, Assist озвучивает запасную фразу («Не удалось
  добавить …»), причина остаётся в журнале и трассировке HA; частичные данные
  не создаются.
- `todo.add_item` остаётся простым путём без разбора: он создаёт позицию с
  названием, но без дат, тегов и повторений.
- Сценарий с пользовательскими предложениями (custom sentences) использует
  встроенный распознаватель намерений Home Assistant. Если в Assist выбран
  LLM-агент, создайте отдельный pipeline с агентом Home Assistant или добавьте
  вызов `tasklerq.capture` как действие, доступное агенту. AI Task разбора при
  этом остаётся без `llm_hass_api`.

### Другие клиенты

- **Автоматизации HA** — вызывайте `tasklerq.capture` из `action`; это тот же
  путь, что у голоса.
- **Внешние HTTP-клиенты** — вызывайте `POST /api/v1/capture` напрямую с
  Bearer-токеном; HA не обязателен.

Подробно об API: [`llm-capture.md`](llm-capture.md#21-внешний-capture-без-telegram).

## 8. Примеры

### Дашборд

```yaml
type: vertical-stack
cards:
  - type: todo-list
    entity: todo.zadachi
    title: Задачи
  - type: todo-list
    entity: todo.pokupki
    title: Покупки
  - type: calendar
    entities:
      - calendar.tasklerq
    title: Календарь задач
```

### Автоматизация: напоминание о событии

```yaml
alias: "Tasklerq: напомнить о задаче"
trigger:
  - platform: calendar
    entity_id: calendar.tasklerq
    event: start
    offset: "-00:15:00"
action:
  - service: notify.mobile_app_phone
    data:
      title: "Скоро задача"
      message: "{{ trigger.calendar_event.summary }}"
mode: queued
```

### Автоматизация: уведомление о просроченных

```yaml
alias: "Tasklerq: просроченные задачи"
trigger:
  - platform: time
    at: "09:00:00"
condition:
  - condition: numeric_state
    entity_id: todo.zadachi
    above: 0
action:
  - service: notify.mobile_app_phone
    data:
      message: "В списке задач есть незавершённые дела."
```

### Шаблон: задачи с высоким приоритетом

```yaml
{% set items = state_attr('todo.zadachi', 'tasklerq_items') or [] %}
{% for item in items if item.priority == 'high' and item.status == 'open' %}
- {{ item.title }}{% if item.due %} ({{ item.due }}){% endif %}
{% endfor %}
```

## 9. Ограничения

- **Приоритет, теги, исполнитель, повторение** не отображаются в стандартной
  карточке `todo-list`; доступны в `tasklerq_items`.
- **Календарь read-only**: нельзя создать событие и превратить его в задачу.
- **Повторения** нельзя редактировать через HA; изменения — в Telegram.
- **Голосовой capture** зависит от HA Assist и качества STT; Tasklerq получает
  только текст.
- **Свободный текст через внешние клиенты** ограничен их интеграциями; HA Assist
  и HTTP API дают полный сценарий.
- **Polling**: задержка до 60 с; webhook из Tasklerq в HA не используется.
- **Нет приватных списков**: все списки household видны всем участникам и HA.
- **Нет MQTT discovery** для `todo`/`calendar`; компонент обязателен.
- **Нет add-on/ingress**; HA Container поддерживается через custom component.
- **Один household** на пользователя.
- **Завершённые задачи** в календаре могут отображаться; фильтрация зависит от
  карточки.

## 10. Диагностика

**Настройки → Устройства и службы → Tasklerq → Диагностика** содержит:

- URL Tasklerq;
- интервал polling и горизонт;
- версию компонента;
- список сущностей;
- время последней успешной синхронизации;
- количество позиций;
- последнюю ошибку (код и время).

Токен, названия задач и имена участников в diagnostics не попадают.

### 10.1. Проверка на живой установке

Проверено 2026-10-31: Home Assistant 2026.10.4, HA Container, amd64, Python
3.14.9; сервер — образ `stellalupus/tasklerq:pr-3` в Docker на том же NAS, URL
интеграции `http://127.0.0.1:18080`. Компонент 0.1.0 установлен через HACS
(custom repository `StellaLupus/tasklerq-ha`). Сервер работал с
`TELEGRAM_MODE=disabled`: бот выключен, токена бота в окружении сервера не
было.

- [x] Интеграция добавлена через UI (URL и токен), состояние `loaded`, заголовок — имя household.
- [x] Сущности `todo.zadachi`, `todo.pokupki` и `calendar.tasklerq` созданы и доступны.
- [x] HA → Tasklerq: создание задачи со сроком-датой и описанием (позиция получила id сервера), переименование, перенос на дату со временем (10:30 МСК сохранено как 07:30Z), завершение и возврат, удаление.
- [x] Tasklerq → HA: `tasklerq.capture` «купить молоко и хлеб» через LLM (AI Task, без fallback) создал две позиции в `todo.pokupki`; после `undo_last_capture` для capture с `source: ha_voice` удалённая на сервере позиция исчезла из HA.
- [x] Календарь: all-day событие для срока-даты, timed-событие 10:30–11:30 после переноса на время.
- [x] Diagnostics: URL, интервал 60 с, горизонт 30 дней, время синхронизации, `last_sync_error: null`; токена, названий задач и имён нет.
- [x] Логи HA: ошибок компонента нет.
- [x] Assist (агент Home Assistant, `conversation.process`, язык ru) с файлами из `tasklerq-ha/examples/assist`: «купи молоко и хлеб» → «Добавил 2 покупки: Молоко, Хлеб.» (две позиции с `source: ha_voice`); «добавь задачу позвонить маме завтра» → «Добавил задачу: Позвонить маме.», срок — завтрашняя дата; «отмени последнее добавление» → «Отменил добавление: 1 элемент.», задача удалена.
- [ ] Ручная установка копированием каталога: отдельно не проверялась.
- [ ] Подхват изменений, сделанных на сервере через бота, плановым опросом: не проверено — бот на момент проверки был выключен (`TELEGRAM_MODE=disabled`).

## 11. Troubleshooting

| Симптом | Причина | Решение |
| --- | --- | --- |
| Интеграция не появляется | Компонент не скопирован/не перезапущен | Проверить `/config/custom_components/tasklerq`, перезапустить HA, смотреть логи. |
| `401 Unauthorized` | Неверный/отозванный токен | Создать новый токен, reauth. |
| `404` | Неверный URL или префикс | Проверить `http://127.0.0.1:18080`, без `/api`. |
| `cannot connect` | Tasklerq не слушает `127.0.0.1:18080` | Проверить `ports: "127.0.0.1:18080:8080"` и `docker compose ps`. |
| Сущности `unavailable` | Tasklerq недоступен | Проверить `/health/ready`, логи, сеть. |
| Нет событий в календаре | Нет задач со сроком или горизонт мал | Добавить срок, увеличить горизонт. |
| Дубли `unique_id` | Старая интеграция не удалена | Удалить старую запись, перезапустить HA. |
| Нет `ai_task` | Подынтеграция OpenRouter `ai_task_data` не настроена | Проверить `ai_task.anthropic_claude_haiku_5_5` в Developer Tools. |
| LLM всегда fallback | `HA_AI_TASK_ENTITY_ID` или `HA_BASE_URL` неверный | Проверить `ai_task.anthropic_claude_haiku_5_5`, `http://host.docker.internal:8123` и `HA_TOKEN`. |
| Время сдвинуто | Разные timezone HA и Tasklerq | Проверить timezone household/пользователя и HA. |
| Нет priority/tags в карточке | Ограничение HA | Использовать `tasklerq_items`. |
| Конфликт при записи | Позиция изменена в Telegram | Обновить и повторить. |

Логи HA:

```bash
sudo /usr/local/bin/docker logs homeassistant 2>&1 | grep -i tasklerq
```

## 12. Удаление

1. **Настройки → Устройства и службы → Tasklerq → Удалить**.
2. Удалить `/volume2/docker/homeassistant/custom_components/tasklerq`
   (внутри HA — `/config/custom_components/tasklerq`).
3. Перезапустить Home Assistant.
4. При необходимости отозвать API-токен в Tasklerq.

Данные в Tasklerq при удалении компонента не удаляются.

## 13. Безопасность

- API-токен — только `Authorization: Bearer`; не в URL.
- Для чтения достаточно `ha:read`; для записи — `ha:write`.
- В локальной сети допустим HTTP; за её пределами — HTTPS.
- Токен отзывается через CLI; после отзыва HA получит `401` и запустит reauth.
- Diagnostics не содержит токен и контент задач.
- Не передавайте токен в шаблоны, автоматизации и логи.
