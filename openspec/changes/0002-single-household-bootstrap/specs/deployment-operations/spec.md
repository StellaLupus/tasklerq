# Spec Delta

## MODIFIED Requirements

### Requirement: Конфигурация через окружение

Система SHALL читать конфигурацию из переменных окружения и MUST завершать
запуск с понятной ошибкой, если отсутствует обязательный параметр или он
невалиден. Обязательные параметры MUST включать `DATABASE_URL` и
`PUBLIC_BASE_URL`; при `TELEGRAM_MODE=webhook` — `TELEGRAM_BOT_TOKEN` и
`TELEGRAM_WEBHOOK_SECRET`; при включённом LLM — параметры подключения к Home
Assistant. Режим `TELEGRAM_MODE=disabled` MUST позволять запуск без Telegram и
без `TELEGRAM_BOT_TOKEN`. Параметры внешнего capture MUST включать
`CAPTURE_API_ENABLED`, `CAPTURE_VOICE_TIMEOUT_MS`, `CAPTURE_VOICE_FALLBACK_RAW`,
`CAPTURE_RATE_LIMIT_PER_MIN` и `CAPTURE_MAX_TEXT_CHARS`. Режим размещения MUST
задаваться `TENANCY_MODE` со значениями `single` (по умолчанию) и `multi`.
Необязательный `BOOTSTRAP_OWNER_TELEGRAM_ID` MUST быть положительным целым; если
он не задан и Telegram включён, при старте MUST выводиться предупреждение, что
первым владельцем станет первый написавший боту. Секреты MUST NOT
храниться в репозитории и MUST NOT выводиться в лог при старте. Значения по
умолчанию MUST быть безопасными: сервис не должен запускаться в production без
webhook-секрета, если Telegram включён.

#### Scenario: Отсутствует DATABASE_URL

- **WHEN** сервис запускается без `DATABASE_URL`
- **THEN** процесс завершается с кодом ошибки
- **AND** сообщение указывает отсутствующий параметр
- **AND** секреты не выводятся

#### Scenario: Невалидный URL

- **WHEN** `PUBLIC_BASE_URL` не является HTTPS-URL в production
- **THEN** сервис отказывается запускаться
- **AND** сообщает ожидаемый формат

#### Scenario: Секреты не логируются

- **WHEN** сервис стартует с корректной конфигурацией
- **THEN** лог содержит имена параметров и версию
- **AND** не содержит значений токенов и паролей

#### Scenario: Headless-режим без Telegram

- **WHEN** сервис запускается с `TELEGRAM_MODE=disabled` без
  `TELEGRAM_BOT_TOKEN`
- **THEN** сервис стартует
- **AND** HTTP API и внешний capture работают
- **AND** webhook и исходящие сообщения отключены

#### Scenario: Capture отключён

- **WHEN** `CAPTURE_API_ENABLED=false`
- **THEN** эндпоинты capture возвращают `503`
- **AND** остальные API продолжают работать

#### Scenario: Режим размещения по умолчанию

- **WHEN** сервис запускается без `TENANCY_MODE`
- **THEN** используется режим `single`

#### Scenario: Невалидный режим размещения

- **WHEN** `TENANCY_MODE=saas`
- **THEN** сервис отказывается запускаться
- **AND** сообщение перечисляет допустимые значения `single` и `multi`

#### Scenario: Невалидный Telegram ID первого владельца

- **WHEN** `BOOTSTRAP_OWNER_TELEGRAM_ID=abc`
- **THEN** сервис отказывается запускаться
- **AND** сообщение указывает параметр и ожидаемый формат

#### Scenario: Первый владелец не ограничен

- **WHEN** Telegram включён, а `BOOTSTRAP_OWNER_TELEGRAM_ID` не задан
- **THEN** сервис стартует
- **AND** лог содержит предупреждение о том, что первым владельцем станет первый
  написавший боту

### Requirement: Health и наблюдаемость

Система SHALL предоставлять `/health/live` для проверки процесса и
`/health/ready` для проверки базы, применённых миграций и допустимой задержки
очередей. Если в режиме `single` активных household больше одного,
`/health/ready` MUST оставаться успешным и MUST содержать предупреждение о
деградации без названий и идентификаторов household. Логи MUST быть
структурированными JSON и MUST содержать `request_id`,
`update_id` (для webhook), длительность и результат операции. Логи MUST NOT
содержать текст сообщений, токены и персональные данные. Система MUST
предоставлять метрики количества обновлений, ошибок, задержки очередей, LLM и
доставки; метрики MUST быть защищены от публичного доступа.

#### Scenario: Успешная readiness

- **WHEN** база доступна, миграции применены, очередь не переполнена
- **THEN** `/health/ready` возвращает `200`
- **AND** содержит версию и время проверки

#### Scenario: Переполненная очередь

- **WHEN** задержка очереди превышает настроенный порог
- **THEN** `/health/ready` возвращает ошибку
- **AND** метрика задержки отражает значение

#### Scenario: Логи без PII

- **WHEN** пользователь отправляет задачу с названием «Купить лекарство»
- **THEN** лог содержит идентификатор обновления и результат
- **AND** не содержит название задачи

#### Scenario: Readiness в деградированном режиме

- **WHEN** в режиме `single` в базе два активных household, а остальные
  проверки пройдены
- **THEN** `/health/ready` возвращает `200`
- **AND** ответ содержит предупреждение `multiple_households`

### Requirement: Административный CLI

Система SHALL предоставлять CLI `tasklerq admin` для создания и отзыва API-
токенов, просмотра статистики очередей, проверки webhook, списка household,
создания и удаления household и добавления пользователя по Telegram ID с ролью
`owner` или `member` (для режима без онбординга в боте). Создание household
MUST позволять сразу указать владельца по Telegram ID; household и владелец
создаются атомарно. В режиме `single` создание household MUST отклоняться, если
активный household уже есть. Токены и пользователи принадлежат household: в
режиме `single` `--household` MAY опускаться, если household единственный; в
режиме `multi` `--household` MUST быть указан явно. Добавление
пользователя MUST записывать событие `member_joined`, отклонять Telegram ID,
уже состоящий в другом household, и второго `owner`. Команды MUST
работать с той же конфигурацией, что и сервис. Опасные команды
(отзыв токена, удаление household) MUST требовать явного подтверждения.
Удаление household через CLI MUST быть мягким и MUST отзывать токены и
приглашения так же, как удаление владельцем.
Вывод MUST NOT содержать секреты и полные токены; новый токен показывается
один раз.

#### Scenario: Создание токена

- **WHEN** администратор выполняет `tasklerq admin token create --name "Home Assistant" --scopes ha:read,ha:write`
- **THEN** CLI выводит токен один раз
- **AND** в базе сохраняется только хеш
- **AND** повторный вывод невозможен

#### Scenario: Household без Telegram

- **WHEN** администратор выполняет
  `tasklerq admin household create --name "Семья" --timezone Europe/Moscow` на
  пустой инсталляции
- **THEN** создаются household и списки «Задачи» и «Покупки»
- **AND** CLI выводит идентификатор household для создания токенов
- **AND** CLI выводит подсказку, как добавить владельца командой `admin user add`

#### Scenario: Household с владельцем одной командой

- **WHEN** администратор выполняет
  `tasklerq admin household create --name "Семья" --owner-telegram-id 123456789 --owner-name "Алексей"`
  на пустой инсталляции
- **THEN** в одной транзакции создаются household, два списка и владелец с этим
  Telegram ID
- **AND** при первом обращении к боту владелец видит главное меню без онбординга

#### Scenario: Второй household в режиме single

- **WHEN** в режиме `single` активный household уже есть и администратор
  выполняет `tasklerq admin household create`
- **THEN** команда завершается ошибкой `409`
- **AND** ничего не меняется

#### Scenario: Список household

- **WHEN** администратор выполняет `tasklerq admin household list`
- **THEN** CLI показывает для каждого активного household идентификатор,
  название, часовой пояс, Telegram ID владельца (или его отсутствие), число
  участников, число открытых задач и число активных токенов
- **AND** не показывает содержимое задач и токены

#### Scenario: Удаление household

- **WHEN** администратор выполняет
  `tasklerq admin household delete --household <id> --yes`
- **THEN** household и его участники помечаются удалёнными
- **AND** токены и приглашения household отзываются
- **AND** в аудите есть событие удаления с системным актором

#### Scenario: Удаление без подтверждения

- **WHEN** администратор выполняет `tasklerq admin household delete --household <id>`
  без `--yes`
- **THEN** CLI запрашивает подтверждение
- **AND** без подтверждения ничего не меняет

#### Scenario: Режим multi без household

- **WHEN** в режиме `multi` администратор выполняет `tasklerq admin token create`
  без `--household`
- **THEN** команда завершается ошибкой с требованием указать `--household`
- **AND** ничего не меняется

#### Scenario: Добавление владельца по Telegram ID

- **WHEN** администратор выполняет
  `tasklerq admin user add --telegram-id 123456789 --role owner --name "Алексей"`
- **THEN** в единственном household появляется владелец с этим Telegram ID
- **AND** в журнале членства есть событие `member_joined` с источником CLI

#### Scenario: Telegram ID уже занят

- **WHEN** администратор добавляет Telegram ID, который состоит в другом household
- **THEN** CLI завершается ошибкой и ничего не меняет

#### Scenario: Статистика очередей

- **WHEN** администратор выполняет `tasklerq admin queue stats`
- **THEN** CLI показывает количество ожидающих, обрабатываемых и ошибочных
  заданий
- **AND** не показывает содержимое сообщений

#### Scenario: Опасная команда

- **WHEN** администратор пытается отозвать токен без флага подтверждения
- **THEN** CLI запрашивает подтверждение
- **AND** без подтверждения ничего не меняет
