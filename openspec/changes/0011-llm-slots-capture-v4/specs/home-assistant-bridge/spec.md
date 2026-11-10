# Spec Delta

## MODIFIED Requirements

### Requirement: Голосовой сценарий Assist

Система SHALL поддерживать сценарий Home Assistant Assist: пользовательские
предложения Assist (custom sentences) вызывают `intent_script`, который вызывает
`tasklerq.capture` и озвучивает `speech` из ответа сервиса. Пример конфигурации
MUST поставляться в документации и примере пакета. Сценарий MUST работать без
Telegram и MUST NOT требовать ни сущности AI Task в Home Assistant, ни
включения Assist-инструментов (`llm_hass_api`): разбор выполняет Tasklerq через
собственные слоты LLM.

#### Scenario: Покупка голосом

- **WHEN** пользователь говорит «купи молоко и хлеб»
- **THEN** Assist вызывает `tasklerq.capture` с `hint=shopping`
- **AND** Tasklerq создаёт две покупки
- **AND** Assist озвучивает «Добавил две покупки»

#### Scenario: Задача голосом

- **WHEN** пользователь говорит «добавь задачу позвонить маме завтра»
- **THEN** создаётся задача со сроком «завтра»
- **AND** ответ озвучивается без упоминания Telegram

#### Scenario: LLM недоступна

- **WHEN** ни один слот LLM Tasklerq не отвечает
- **THEN** Tasklerq создаёт одну позицию из исходного текста
- **AND** Assist озвучивает «Записал как есть»

#### Scenario: Без Assist-инструментов

- **WHEN** в Home Assistant нет ни одной сущности AI Task
- **THEN** голосовой capture всё равно работает
- **AND** LLM не получает инструментов управления HA

#### Scenario: LLM-агент в Assist

- **WHEN** в Assist выбран LLM-агент, который не использует пользовательские предложения (custom sentences) Assist
- **THEN** документация предлагает отдельный pipeline с агентом Home Assistant
  или явное действие агента
- **AND** разбор Tasklerq продолжает работать без Assist-инструментов
