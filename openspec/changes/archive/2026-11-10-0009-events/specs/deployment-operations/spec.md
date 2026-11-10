# Spec Delta

## MODIFIED Requirements

### Requirement: Создание и удаление household через CLI

Создание household MUST позволять сразу указать владельца по Telegram ID;
household и владелец создаются атомарно. В режиме `single` создание household
MUST отклоняться, если активный household уже есть. Удаление household через CLI
MUST быть мягким и MUST отзывать токены и приглашения так же, как удаление
владельцем.

#### Scenario: Household без Telegram

- **WHEN** администратор выполняет
  `tasklerq admin household create --name "Семья" --timezone Europe/Moscow` на
  пустой инсталляции
- **THEN** создаются household и списки «Задачи», «Покупки» и «Календарь»
- **AND** CLI выводит идентификатор household для создания токенов
- **AND** CLI выводит подсказку, как добавить владельца командой `admin user add`

#### Scenario: Household с владельцем одной командой

- **WHEN** администратор выполняет
  `tasklerq admin household create --name "Семья" --owner-telegram-id 123456789 --owner-name "Алексей"`
  на пустой инсталляции
- **THEN** в одной транзакции создаются household, три списка и владелец с этим
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
