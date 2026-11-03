# Changelog

Формат — [Keep a Changelog](https://keepachangelog.com/ru/1.1.0/), версии —
[SemVer](https://semver.org/lang/ru/). Раздел `0.1.0` помечен «не выпущен» до
постановки тега `v0.1.0`.

## [0.1.0] — не выпущен

Первый релиз MVP: задачи и покупки в Telegram, Home Assistant и календаре,
разбор свободного текста через Home Assistant AI Task и capture без бота.

### Добавлено

- **Задачи.** Создание, чтение, изменение и удаление; срок (дата или дата и
  время), приоритет, теги, подзадачи, исполнитель, поиск, повторения
  (`src/storage/repositories/items/`, `src/http/api/items.rs`,
  `docs/domain-model.md`; тесты `tests/item_lifecycle.rs`, `tests/search.rs`).
  Удалённая задача скрывается из всех интерфейсов и возвращается отменой в
  течение 10 минут; через 30 дней удаляется физически.
- **Покупки.** Позиции с количеством и единицей измерения, отметка «куплено»,
  повторное открытие, очистка выполненных, несколько списков и чек-листов
  (`src/telegram/bot/shopping.rs`, `docs/telegram-ux.md` §4.6; тесты
  `tests/telegram_screens.rs`, `tests/telegram_steps.rs`).
- **Календарь.** Задачи со сроком как вхождения: вседневные и со временем,
  включая повторяющиеся (`src/http/api/calendar.rs`, `docs/home-assistant.md`;
  тесты `tests/calendar.rs`).
- **Telegram-интерфейс.** Webhook с очередью в PostgreSQL, RichMessage с
  кнопками в тексте, callback-токены, экраны меню, списков, карточек, форм и
  поиска; обработанные входящие сообщения удаляются, `/start` и `/menu`
  присылают экран заново (`src/telegram/`, `docs/telegram-ux.md`; тесты
  `tests/telegram_screens.rs`, `tests/telegram_queue.rs`,
  `tests/telegram_webhook.rs`).
- **LLM-capture в Telegram.** Свободный текст разбирается через
  `ai_task.generate_data` Home Assistant по фиксированной схеме; ответ
  валидируется на сервере, показывается черновик с подтверждением; при сбое,
  таймауте или исчерпанной квоте открывается ручная форма; есть суточная квота
  и предохранитель (`src/capture/`, `docs/llm-capture.md`; тесты
  `tests/telegram_capture.rs`, `tests/capture_fallback.rs`).
- **Capture без бота.** `POST /api/v1/capture` в режимах `auto` и `draft`,
  сервис `tasklerq.capture` и голосовой сценарий HA Assist, идемпотентность по
  `Idempotency-Key`, подтверждение, отмена и undo (`src/http/api/captures.rs`,
  `docs/llm-capture.md`, `docs/home-assistant.md`; тесты `tests/capture_api.rs`,
  `tests/capture_idempotency.rs`, `tests/headless_ha.rs`). При `503` с
  выключенным fallback (`CAPTURE_VOICE_FALLBACK_RAW=false`) позиция и черновик
  не создаются, исходный текст не сохраняется, а резерв `Idempotency-Key`
  освобождается.
- **Интеграция с Home Assistant.** Custom component: `todo`-сущность на каждый
  неархивированный список, read-only `calendar`, polling, обработка конфликтов
  версий, diagnostics и reauth; установка через HACS
  (`docs/home-assistant.md`, `docs/ha-api.md`; контракт проверяется тестами
  `tests/http_contract.rs`, `tests/home_assistant_client.rs`).
- **Семья и приглашения.** Household, роли owner/member, приглашения по deep
  link, общие списки, назначение задач; владелец выпускает и отзывает
  API-токены в экране «Интеграции» (`src/storage/repositories/households.rs`,
  `src/storage/repositories/invites.rs`, `docs/deployment.md` §9.1; тесты
  `tests/identity_households.rs`, `tests/identity_members.rs`).
- **Уведомления.** Напоминания, утренняя сводка, уведомления о назначении,
  тихие часы с переносом доставки, snooze и действия из уведомления; доставка
  через outbox с повторами и dead-letter, дедупликация после перезапуска
  (`src/jobs/`, `src/telegram/outbox.rs`, `docs/telegram-ux.md` §4.10; тесты
  `tests/notifications.rs`, `tests/notification_actions.rs`,
  `tests/notification_outbox.rs`).
- **Эксплуатация.** Docker-образ и compose, `/health/live` и `/health/ready`,
  JSON-логи и метрики Prometheus, административный CLI (`token`, `household`,
  `user`, `queue`, `webhook`, `export`), миграции под advisory lock,
  backup/restore и retention (`docs/deployment.md`, `scripts/backup.sh`,
  `scripts/restore.sh`, `src/admin/`, `src/metrics.rs`; тесты
  `tests/backup_restore.rs`, `tests/retention.rs`, `tests/admin_cli.rs`).
- **Безопасность.** Проверка секрета webhook за постоянное время, хранение
  API-токенов только в виде хеша, scopes и отзыв, tenant-изоляция с `404` на
  чужие ресурсы, rate limits, аудит событий безопасности и redaction секретов
  в логах и ошибках (`docs/deployment.md` §15.1, `src/auth/`,
  `src/rate_limit.rs`, `src/security_audit.rs`; тесты `tests/idor.rs`,
  `tests/secret_redaction.rs`, `tests/security_audit.rs`).

### Изменено

- **Single-household и bootstrap владельца (change 0002).** `TENANCY_MODE`
  со значением `single` по умолчанию: активным может быть не больше одного
  household, второй не создаёт ни бот, ни CLI. На пустой инсталляции владельцем
  становится `BOOTSTRAP_OWNER_TELEGRAM_ID`, если переменная задана. Если
  household уже есть и владелец назначен, неизвестный пользователь получает
  просьбу о приглашении; без владельца бот подсказывает команду
  `admin user add`. При нескольких активных household сервис работает в
  деградированном режиме с предупреждением `multiple_households` в
  `/health/ready`. Добавлены
  `admin household create --owner-telegram-id`, `admin household delete` и
  экран «Интеграции» (`openspec/changes/0002-single-household-bootstrap/`,
  `docs/deployment.md` §9, §16.11; тесты `tests/first_contact.rs`,
  `tests/degradation_logs.rs`).
- **Видимость экранов Telegram (change 0003).** Активное сообщение экрана
  правится, только если оно видно пользователю; иначе экран приходит новым
  сообщением, а старый якорь удаляется. Входящие текстовые сообщения бот
  удаляет после чтения. Экран разбора и карточка черновика показывают исходный
  текст цитатой. Обновления сверх лимита 20/мин откладываются до освобождения
  слота; сверх 20 отложенных — отбрасываются без ответа. В outbox добавлен вид
  `delete` (миграция `0011`; `openspec/changes/0003-telegram-screen-visibility/`,
  `docs/telegram-ux.md` §6–7; тесты `tests/telegram_outbox.rs`,
  `tests/telegram_queue.rs`).
- **Ошибки обработки обновлений Telegram.** При переходе обновления в
  dead-letter пользователь получает одно нейтральное сообщение («Не получилось
  выполнить действие. Попробуйте ещё раз чуть позже.»), а нажатие кнопки
  получает пустой ответ, чтобы убрать «часики». На повторах сообщения нет,
  текст пользователя и детали ошибки не раскрываются (`src/telegram/worker.rs`,
  `src/telegram/ru.rs`, `docs/telegram-ux.md` §4.11; тесты
  `tests/telegram_queue.rs`).

### Известные ограничения

- В быстрой ручной форме нет выбора списка: позиция попадает в список задач по
  умолчанию (`docs/llm-capture.md` §15).
- Критерии приёмки MVP, которые остались `[ ]` в `docs/roadmap.md`:
  - «Задача завершается в HA и исчезает из открытых в Telegram» — подтверждены
    только события `completed` с actor `home_assistant`, отдельного теста на
    исчезновение задачи из открытых нет;
  - «`openspec validate --strict` проходит».
- Откат на предыдущий образ после применения новой миграции невозможен: старый
  образ не запустится с ошибкой о применённой миграции. В этом случае помогает
  только restore из backup в новую базу (`docs/deployment.md` §12, §16.10).

### Требования

- PostgreSQL 18 (`docs/deployment.md` §4).
- Home Assistant с AI Task, минимум `2026.10.0`; проверено на `2026.10.4`
  (`docs/home-assistant.md`).
- Образ `stellalupus/tasklerq:0.1.0`.

[0.1.0]: https://github.com/StellaLupus/tasklerq/releases/tag/v0.1.0
