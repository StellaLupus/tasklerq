# Roadmap Tasklerq

## 1. Принципы

1. **Сначала фундамент.** Домен, безопасность и надёжность важнее количества
   экранов.
2. **Один сценарий — до конца.** «Добавить → подтвердить → увидеть в HA →
   завершить» должен работать целиком, прежде чем расширять охват.
3. **LLM не источник истины.** Автоматизация не должна блокировать ручные
   сценарии.
4. **Семейный масштаб.** Не оптимизируем под тысячи household и миллионы
   задач.
5. **Обратная совместимость.** Миграции expand/contract, версионированные
   образы, откат.

## 2. MVP v0.1

| Блок | Входит | Приоритет |
| --- | --- | --- |
| Telegram | Приватный чат, RichMessage, меню, списки, карточки, формы | P0 |
| Задачи | CRUD, срок, приоритет, теги, подзадачи, исполнитель, поиск | P0 |
| Покупки | Позиции, количество, единицы, отметка, очистка, дубликаты | P0 |
| Повторения | daily/weekly/monthly/yearly, interval, weekdays, until | P1 |
| Календарь | Вхождения задач, all-day/timed | P1 |
| LLM-capture | HA AI Task, схема, валидация, черновик, подтверждение, fallback | P1 |
| Capture без бота | `POST /api/v1/capture`, `tasklerq.capture`, HA Assist, auto-режим, идемпотентность, undo | P0 |
| Home Assistant | `todo` по спискам, read-only `calendar`, polling, конфликты | P1 |
| Семья | Household, owner/member, приглашения, общие списки | P0 |
| Уведомления | Напоминания, сводка, назначения, тихие часы, snooze | P1 |
| Эксплуатация | Docker, PostgreSQL, webhook, health, backup, retention | P0 |
| Безопасность | Секрет webhook, токены, tenant-изоляция, rate limits | P0 |

### Критерии приёмки MVP

- [x] Новый пользователь проходит онбординг и создаёт household
  (`tests/identity_households.rs::onboarding_creates_household_owner_and_default_lists`).
- [x] Владелец приглашает второго участника по deep link
  (`tests/identity_households.rs::invite_lets_a_new_member_join`).
- [x] Задача создаётся из Telegram и появляется в HA ≤60 с
  (`docs/deployment.md` §19, «HA sync»).
- [ ] Задача завершается в HA и исчезает из открытых в Telegram
  (`docs/deployment.md` §19, «HA sync»: показаны только события `completed` с
  actor `home_assistant`; «6→5» относится к направлению Telegram→HA. Отдельного
  теста на исчезновение задачи из открытых после завершения из HA нет).
- [x] Свободный текст разбирается в черновик; подтверждение создаёт элементы
  (`tests/telegram_capture.rs::save_all_creates_every_item_and_undo_removes_them`).
- [x] При недоступности LLM работает ручная форма
  (`tests/telegram_capture.rs::failed_parse_opens_the_manual_form_with_the_source_text`,
  `tests/telegram_capture.rs::daily_quota_stops_parsing_and_opens_the_manual_form`).
- [x] Голосовая команда в HA Assist создаёт задачу или покупку без Telegram
  (`docs/deployment.md` §19, «Голосовой capture через Assist без Telegram»).
- [x] Голосовой capture идемпотентен: повтор не создаёт дубликаты
  (`tests/capture_idempotency.rs::an_auto_repeat_returns_the_first_result_without_duplicates`).
- [x] При недоступности LLM голосовой capture создаёт fallback-позицию
  (`tests/capture_fallback.rs::ha_unavailability_in_auto_creates_one_fallback_item`).
- [x] Capture можно отменить в течение 10 минут (`undo` / `undo_last_capture`)
  (`tests/capture_api.rs::undo_last_removes_only_the_latest_voice_capture`).
- [x] Повторяющаяся задача не создаёт дублей напоминаний после рестарта
  (`tests/notifications.rs::restart_does_not_duplicate_and_next_occurrence_gets_its_own_delivery`).
- [x] Покупка с количеством и единицей проходит полный цикл
  (`docs/deployment.md` §19, «Покупка»).
- [x] Webhook без секрета возвращает 401
  (`tests/telegram_webhook.rs::wrong_or_missing_secret_is_rejected_without_storing`).
- [x] `/health/ready` отражает БД и очередь
  (`tests/health.rs::readiness_requires_every_embedded_migration_to_be_successful`,
  `tests/telegram_webhook.rs::readiness_fails_while_an_update_waits_longer_than_the_limit`).
- [x] Backup восстанавливается на чистую базу
  (`tests/backup_restore.rs::backup_then_restore_into_a_new_database_keeps_migrations_and_data`).
- [ ] `openspec validate --strict` проходит.

## 3. Этапы

### Этап 0. Спецификация (завершён)

**Результат:** OpenSpec change `0001-foundation`, проектные документы.

- [x] `openspec/project.md`.
- [x] 9 capability specs.
- [x] `design.md`.
- [x] `tasks.md`.
- [x] README и docs.
- [x] Ревью и уточнение параметров развёртывания.

**Выход:** согласованные спецификации, готовые к реализации.

### Этап 1. Foundation (завершён)

**Результат:** собираемый сервис, БД, health, Docker, CI.

- Cargo-проект, конфигурация, ошибки, request ID.
- Миграции households/users/lists/items.
- Dockerfile, compose, healthcheck.
- CI: fmt, clippy, test, audit.

**Выход:** `cargo test` и `docker build` проходят; `/health/ready` отвечает.

### Этап 2. Домен и Telegram (завершён)

**Результат:** рабочие задачи и покупки в боте.

- Репозитории, транзакции, версии, события.
- Повторения и календарные вхождения.
- Webhook, очередь, RichMessage, callback-токены.
- Экраны меню, списков, карточек, форм, покупок, поиска.
- Онбординг, приглашения, роли.

**Выход:** e2e-сценарии «создать/завершить/найти/пригласить» проходят.

### Этап 3. LLM-capture и внешний capture (завершён)

**Результат:** свободный текст → черновик/сразу созданные элементы, с
Telegram и без него.

- Клиент HA AI Task, промпт, схема.
- Валидация, черновик, уточнения.
- `POST /api/v1/capture`: `auto`/`draft`, `Idempotency-Key`, undo.
- Fallback, circuit breaker, квоты, голосовой таймаут 8 с.
- Golden set и метрики по источникам.

**Выход:** ≥90% точности на golden set; подтверждение идемпотентно; голосовой
capture создаёт элементы без Telegram; fallback работает.

### Этап 4. Home Assistant (завершён)

**Результат:** нативные сущности, синхронизация и голосовой вход.

- Custom component, config flow, reauth.
- `todo` по спискам, `calendar`.
- Сервисы `tasklerq.capture`, `confirm_capture`, `cancel_capture`,
  `undo_last_capture`.
- Пример пользовательских предложений (custom sentences) + `intent_script` для
  HA Assist.
- Snapshot, конфликты, diagnostics.
- Установка через HACS.

**Выход:** задача из Telegram появляется в HA; завершение в HA отражается в
Telegram (на инсталляции подтверждены только события `completed` с actor
`home_assistant`; исчезновение задачи из открытых в Telegram отдельным тестом
не проверено, поэтому критерий MVP остаётся `[ ]` — см. «Критерии приёмки
MVP»); конфликт версий обрабатывается; голосовая команда в Assist создаёт
задачу или покупку и озвучивает ответ.

### Этап 5. Уведомления (завершён)

**Результат:** напоминания и сводки.

- Планировщик, дедупликация, quiet hours.
- Reminder/digest/assignment.
- Snooze и действия.
- Outbox, retry, dead-letter.

**Выход:** напоминание приходит один раз; после рестарта нет дублей; тихие часы
переносят доставку.

### Этап 6. Hardening и релиз (текущий: релиз v0.1.0, пункты 9.x)

**Результат:** production-ready v0.1.0.

Реализация этапов 0–5 закончена; идёт релиз: документы, приёмка MVP и архив
change `0001-foundation` (пункты 9.x в `tasks.md`).

- Rate limits, security checklist.
- Load test, backup/restore drill.
- Runbooks, staging acceptance.
- Релизный образ, release notes.

**Выход:** развёртывание с нуля по документации; приёмка MVP; архив change.

## 4. Post-MVP

### v0.2 — Удобство

- Веб-UI для управления household и токенами.
- Экспорт/импорт JSON.
- Расширенные фильтры и сохранённые представления.
- Массовые операции (перенести, назначить, удалить).
- Категории покупок и сортировка по магазину.
- Настройка порогов и более гибкие напоминания.
- Доступ к незавершённым черновикам и ручным формам, вытесненным новым
  разбором: сейчас они остаются `pending` до TTL, но из бота недоступны.

### v0.3 — Календарь и интеграции

- CalDAV/ICS (двусторонняя синхронизация или экспорт).
- Нативная интеграция с телефонным календарём.
- Webhook из Tasklerq в HA (события `item.created`, `item.completed`).
- Публичный read-only API и OAuth.
- Импорт из Todoist/Google Tasks.

### v0.4 — Семья и права

- Несколько household на пользователя.
- Частные списки.
- Гибкие роли и права на списки.
- Гостевой доступ.
- Аудит с фильтрами и экспортом.

### Задел: пользователи HA и Telegram, алиасы

Цель: голос «добавь мне задачу» назначает говорящего; «жена», «мама» разбираются
как конкретный участник; человек без Telegram может быть исполнителем. В MVP
ничего из этого не строится, но путь открыт. Все миграции аддитивны:

- **Идентичности.** Таблица `user_identities` (`user_id`, `kind`:
  `telegram`/`ha_person`, `external_id`; UNIQUE `(kind, external_id)` среди
  неудалённых). С `users.telegram_user_id` и `telegram_chat_id` снимается
  NOT NULL, значения переносятся в идентичности.
- **Алиасы.** Таблица `user_aliases` (`user_id`, `alias`; UNIQUE
  `(household_id, lower(alias))`). Алиасы попадают в контекст промпта новой
  версией (после `capture-v3`); сервер по-прежнему сам сопоставляет `assignee` с
  участниками, при неоднозначности работает уточняющий вопрос.
- **Говорящий.** В `POST /api/v1/capture` необязательное поле (например,
  `ha_user_id`); `tasklerq.capture` берёт пользователя HA из контекста вызова (у
  контекста HA есть `user_id`; заполняется ли он для голосовых сателлитов,
  проверить при реализации). В текущем HA уже есть `person.*`, к которым можно
  привязывать идентичности. Это атрибут запроса, `actor_type=voice` остаётся.
- **Токен «от имени человека».** `api_tokens.user_id` с NULL.
- **Уведомления без Telegram.** Канал доставки (например, `notify.*` через HA):
  в `outbox` добавляется `channel`, у пользователя — предпочитаемый канал.

Инварианты MVP, которые нельзя ломать, чтобы путь не закрылся:

- исполнитель и автор — всегда ссылка на `users.id`, а не строка и не Telegram
  ID;
- идентификаторы и имена из LLM остаются недоверенными: сопоставление и проверка
  принадлежности household делает сервер;
- Telegram ID не используется как ключ в доменных связях, только в `users` (и
  позже в идентичностях) и в транспортном слое Telegram;
- системные акторы (`voice`, `home_assistant`) остаются отдельным типом, а не
  подменяются участником.

Таблицы не создаются, пока нет второго потребителя.

### Later — AI и автоматизация

- Голосовые сообщения в Telegram (voice → STT → capture); голосовой capture
  через HA Assist уже в MVP.
- Групповые чаты с упоминаниями.
- Предложения расписания («когда лучше сделать?»).
- Автоматическое разбиение крупных задач.
- Умные напоминания по контексту.
- Рецепты и генерация списка покупок.
- Магазины, цены, штрихкоды.

### Явно не планируется

- SaaS и мультитенантность.
- Федерация между серверами.
- Собственное мобильное приложение в ближайшем горизонте.
- Полноценная замена специализированным project management системам.

## 5. Метрики

### Продукт

- Время от сообщения до созданной задачи (цель <15 с).
- Доля подтверждённых LLM-черновиков (цель >70%).
- Доля успешных разборов без ручной правки (цель >60%).
- Доля задач, созданных из свободного текста (цель >50%).
- Доля capture из Home Assistant и голоса (наблюдательная).
- Доля fallback-созданий среди голосовых capture (цель <10%).
- Доля задач, завершённых из HA (наблюдательная).
- Возврат пользователей на 2-й и 7-й день.

### Надёжность

- Webhook p95 <100 мс (без LLM).
- Очередь не растёт >5 минут.
- Дубли напоминаний = 0.
- Ошибки доставки <1%.
- Время восстановления после сбоя <30 минут.
- Успешность backup/restore = 100%.

### Качество LLM

- Точность извлечения элементов ≥90% на golden set.
- Ошибки валидации по полям <10%.
- Latency p95 <8 с.
- Стоимость на 100 разборов в пределах бюджета.

## 6. Риски

| Риск | Вероятность | Влияние | Митигация |
| --- | --- | --- | --- |
| RichMessage API меняется | Средняя | Высокое | Адаптер, contract-тесты. |
| LLM ошибается/дорого | Высокая | Среднее | Валидация, подтверждение, квоты, golden set. |
| HA polling задерживает | Высокая | Низкое | 60 с, refresh после записи, документация. |
| Повторения/DST сложны | Средняя | Среднее | Простые правила, якорь, тесты. |
| Webhook публично доступен | Средняя | Высокое | TLS, секрет, лимиты, мониторинг. |
| PostgreSQL — единая точка | Низкая | Высокое | Backup, restore drill, readiness. |
| Расползание scope | Высокая | Среднее | MVP-таблица, non-goals, этапы. |
| Сложность установки HA | Средняя | Среднее | HACS + ручная инструкция, diagnostics. |
| Утечка PII в LLM | Средняя | Высокое | Минимизация, раскрытие, отключение. |

## 7. Журнал решений

| Дата | Решение | Причина |
| --- | --- | --- |
| 2026-10 | Rust, модульный монолит | Требование, семейный масштаб. |
| 2026-10 | PostgreSQL, без SQLite | Уже есть на сервере; очереди и JSONB. |
| 2026-10 | Telegram webhook, не polling | Требование; публичный домен есть. |
| 2026-10 | RichMessage + свой адаптер | Кнопки в контенте; teloxide не поддерживает. |
| 2026-10 | LLM через HA AI Task | Gemini уже в HA; не хранить ключи в Tasklerq. |
| 2026-10 | LLM только разбор | Безопасность и контроль. |
| 2026-10 | HA custom component | MQTT не умеет todo/calendar; HA Container. |
| 2026-10 | Один household | Семейный сценарий, простая авторизация. |
| 2026-10 | Без частных списков в MVP | Упрощение прав; все списки общие. |
| 2026-10 | Без веб-UI в MVP | Telegram + HA достаточно. |
| 2026-10 | Мягкое удаление 30 дней | Безопасность и undo. |
| 2026-10 | UUIDv7 | Сортировка по времени, безопасные URL. |
| 2026-10 | Исполнитель — всегда `users.id` | Задел под связь HA/Telegram и алиасы без смены модели. |

## 8. Открытые вопросы

Среда определена: Synology NAS, Synology Reverse Proxy, домен
`tasklerq.example.com`, PostgreSQL 18 в сети `shared-db`, HA Container
`2026.10.4` в `network_mode: host`, HACS, AI Task
`ai_task.anthropic_claude_haiku_5_5`, бот `@tasklerq_example_bot`, часовой пояс `Europe/Moscow`,
хранение `raw_text` 30 дней, backup — Hyper Backup на внешний диск.

Осталось уточнить:

- бюджет LLM при квоте 200 разборов в сутки;
- нужен ли веб-UI уже в v0.2 или позже;
- нужна ли интеграция с CalDAV в v0.3 или достаточно ICS-экспорта.
