## Why

Семейные задачи и покупки сейчас распылены между перепиской, заметками,
календарями и Home Assistant. Нужен один сервис, куда можно быстро добавить
задачу или покупку обычной фразой в Telegram, а затем видеть общий список и
календарь всей семьёй — в том числе через Home Assistant. У пользователя уже
есть PostgreSQL, публичный домен и LLM Google Gemini, подключённая к HA, поэтому
фундамент можно построить без дополнительной инфраструктуры.

## What Changes

- Создаётся Rust-сервис Tasklerq: axum + tokio + sqlx + PostgreSQL, модульный
  монолит с фоновыми воркерами.
- Реализуется Telegram-бот с RichMessage: кнопки внутри сообщений, навигация с
  редактированием, главное меню, списки, карточки задач, формы и уведомления.
- Добавляется доменная модель задач и списков: сроки, приоритеты, теги,
  подзадачи, исполнители, повторения, мягкое удаление, поиск и календарь.
- Реализуется приём Telegram webhook с проверкой секрета, очередью обновлений в
  PostgreSQL и идемпотентной обработкой.
- Добавляется LLM-capture: свободный текст уходит в `ai_task.generate_data`
  Home Assistant, результат валидируется в Rust. Из Telegram применяется после
  подтверждения, из внешних источников — сразу (`auto`) или в черновик
  (`draft`).
- Добавляется capture без участия бота: `POST /api/v1/capture`, сервис
  `tasklerq.capture` в HA-компоненте и голосовой сценарий HA Assist. Задачи
  можно добавлять голосом, автоматизацией или прямым HTTP-запросом; Telegram при
  этом не требуется.
- Создаётся тонкий Python custom component для Home Assistant: `todo`-сущности
  по спискам и одна read-only `calendar`-сущность, синхронизация через REST API.
- Реализуются семейные аккаунты: один household, роли owner/member,
  приглашения по deep link, общие списки и назначение задач.
- Добавляются напоминания, утренняя сводка, уведомления о назначении, тихие
  часы и настройки уведомлений.
- Описывается развёртывание: Docker, внешний PostgreSQL, HTTPS-reverse-proxy,
  webhook, миграции, резервное копирование, health/readiness и runbooks.
- Создаются проектные документы на русском: архитектура, модель данных,
  Telegram UX, LLM-capture, Home Assistant, развёртывание и roadmap.

В рамках изменения создаются только спецификации и документация. Прикладной код
не пишется; реализация начнётся отдельным изменением после ревью.

### Вне рамок MVP

- Веб-интерфейс, мобильное приложение, CalDAV/ICS и телефонный календарь.
- Голосовые сообщения в Telegram и собственное распознавание речи; голосовой
  capture через HA Assist входит в MVP.
- Групповые чаты, несколько household на пользователя.
- Магазины, цены, штрихкоды, рецепты; сложные правила повторения и исключения.
- HA add-on/ingress, OAuth, публичный API для третьих сторон, SaaS.

## Capabilities

### New Capabilities

- `task-management`: жизненный цикл задач, сроки, приоритеты, теги, подзадачи,
  поиск, повторения, мягкое удаление и календарные вхождения.
- `shopping-lists`: совместные списки покупок, количество и единицы, отметка
  «куплено», очистка выполненных, дополнительные списки.
- `telegram-rich-ui`: приватный чат, RichMessage-экраны, навигация с
  редактированием, callback-токены, webhook-очередь и деградация.
- `llm-capture`: разбор свободного текста через Home Assistant AI Task,
  структурированная схема, серверная валидация, черновик и подтверждение, а
  также внешний capture без Telegram с auto-режимом, идемпотентностью и
  fallback-созданием.
- `home-assistant-bridge`: установка custom component, `todo`/`calendar`
  сущности, сервис `tasklerq.capture`, голосовой сценарий Assist,
  polling-синхронизация, конфликты и диагностика.
- `notifications-reminders`: напоминания о сроках, повторяющиеся вхождения,
  утренняя сводка, назначения, тихие часы и надёжная доставка.
- `identity-sharing`: household, роли, приглашения, общие списки, назначение,
  удаление участников и аудит членства.
- `deployment-operations`: конфигурация, PostgreSQL, Docker, webhook/TLS,
  health, фоновые задания, обновления, backup/retention и локальная разработка.
- `security-privacy`: аутентификация webhook и API, авторизация, валидация,
  защита от LLM-инъекций, rate limiting, секреты и приватность.

### Modified Capabilities

Нет: проект создаётся с нуля, существующих спецификаций нет.

## Impact

- **Новый код:** Rust-сервис (`tasklerq`), Python custom component
  (отдельный репозиторий `tasklerq-ha`, каталог
  `custom_components/tasklerq`), миграции PostgreSQL, Docker-образ и compose.
- **Новые зависимости:** axum, tokio, sqlx, reqwest, serde, tracing, chrono,
  uuid, teloxide-core (типы Telegram), testcontainers/wiremock для тестов;
  Python: `homeassistant`, `aiohttp`, `pytest-homeassistant-custom-component`.
- **Новые интерфейсы:** `POST /api/v1/capture` и подтверждение/отмена/undo
  capture; сервисы `tasklerq.capture`, `tasklerq.undo_last_capture`; пример
  `conversation.intents` + `intent_script` для HA Assist.
- **Инфраструктура:** существующий PostgreSQL (новая БД/роль), публичный
  HTTPS-домен, reverse proxy, сеть Docker между Tasklerq и HA Container.
- **Внешние системы:** Telegram Bot API (RichMessage, webhook), Home Assistant
  REST API, `ai_task.generate_data`, Google Gemini, HACS (опционально).
- **Данные:** новая схема household/users/lists/items/tags/subtasks/events,
  очереди Telegram, черновики capture, outbox, уведомления, токены.
- **Эксплуатация:** миграции, backup/restore, retention, мониторинг, ротация
  секретов, обновление образа.
- **Риски:** нестабильность RichMessage API, задержка HA polling, стоимость и
  качество Gemini, сложность повторений и часовых поясов, публичная
  поверхность webhook.
