# Развёртывание и эксплуатация Tasklerq

## 1. Топология

Фактическая среда:

- **Synology NAS** `192.168.10.110` (`StellaLupus-NAS`), SSH на порту `2122`.
- **Home Assistant Container** `homeassistant`, `network_mode: host`, версия
  `2026.10.4`, конфиг `/volume2/docker/homeassistant`.
- **HACS** установлен.
- **PostgreSQL** `postgres:18-alpine`, контейнер `postgres`, сеть `shared-db`,
  host-порт `15433`.
- **Reverse proxy** — встроенный Synology DSM Reverse Proxy.
- **Домен** — `tasklerq.stellalupus.ru`; DNS уже указывает на NAS
  (`94.19.88.123`).

```text
                    Internet
                       │
                       │ 443/TLS
                 ┌─────▼──────────────────┐
                 │ Synology DSM           │
                 │ Reverse Proxy          │
                 │ tasklerq.stellalupus.ru│
                 └─────┬──────────────────┘
                       │ 127.0.0.1:18080
                 ┌─────▼──────────┐        ┌──────────────┐
                 │ tasklerq       │───────▶│ PostgreSQL   │
                 │ container      │ shared │ postgres:18  │
                 │ (bridge)       │  -db   │ shared-db    │
                 └──┬──────────┬──┘        └──────────────┘
                    │          │
        host.docker │          │ HTTPS
        .internal   │          ▼
                    ▼    ┌──────────────┐
             ┌──────────┐│ Telegram API │
             │ HA host  │└──────────────┘
             │ :8123    │
             └────┬─────┘
                  │ ai_task.generate_data
                  ▼
             Google Gemini
```

Принципы:

- Tasklerq не управляет PostgreSQL и не устанавливает его.
- Host-порт `18080` публикуется только на loopback; снаружи — только 443.
- **8080 занят Zigbee2MQTT**, поэтому Tasklerq использует `18080`.
- HA работает в `network_mode: host`; Tasklerq — в bridge-сети и в
  `shared-db`.
- Telegram видит публичный HTTPS webhook через Synology Reverse Proxy.

## 2. Что уже есть

| Компонент | Факт | Что нужно сделать |
| --- | --- | --- |
| DNS | `tasklerq.stellalupus.ru` → `94.19.88.123` | Ничего. |
| TLS | Сертификат Let's Encrypt для `tasklerq.*` получен, но на 443 отдаётся самоподписанный `CN=synology` (проверено 2026-10-29) | Назначить сертификат записи reverse proxy (раздел 3.2, шаг 4). |
| Reverse Proxy | Запись создана: `HTTPS tasklerq.stellalupus.ru:443` → `http://localhost:18080`; без приложения отвечает `502` | Ничего; при `502` после запуска заменить `localhost` на `127.0.0.1`. |
| PostgreSQL | `postgres:18-alpine` (18.6), сеть `shared-db`, порт `15433`; роль `tasklerq` (не суперпользователь) и БД `tasklerq` с `pg_trgm 1.6` созданы 2026-10-29 | Пароль из `DATABASE_URL` положить в `.env` (0600). |
| Home Assistant | `2026.10.4`, host network, HACS | Установить custom component. |
| AI Task | `ai_task.google_ai_task_2` | Проверить вызов в Developer Tools. |
| Telegram | Бот `@tasklerqbot` создан | Получить токен в BotFather и заполнить `.env`. |
| Tasklerq | Кода ещё нет | Реализовать по `tasks.md`. |

## 3. DNS и TLS

DNS уже настроен. Проверка:

```bash
dig +short tasklerq.stellalupus.ru
# 94.19.88.123
```

Сейчас reverse proxy создан и отвечает `502` (приложение не запущено), но
`https://tasklerq.stellalupus.ru` отдаёт самоподписанный сертификат Synology
(`CN=synology`): выпущенный сертификат ещё не назначен записи (шаг 3.2.4).
Telegram такой сертификат отклонит, поэтому webhook зарегистрировать нельзя.

### 3.1. Reverse Proxy в Synology DSM

1. **Панель управления → Портал входа → Дополнительно → Reverse Proxy**.
2. **Создать**:
   - Описание: `Tasklerq`.
   - Источник: `HTTPS`, `tasklerq.stellalupus.ru`, порт `443`.
   - HSTS: включить.
   - Назначение: `HTTP`, `localhost`, порт `18080`.
   - HTTP/1.1: включить.
   - Таймауты: 60 секунд.
   - Пользовательские заголовки: не нужны; websocket не используется.
3. Сохранить.

Не редактируйте `/usr/syno/etc/www/ReverseProxy.json` вручную: DSM
перезаписывает файл и может сломать конфигурацию.

### 3.2. Сертификат

1. **Панель управления → Безопасность → Сертификат**.
2. **Добавить → Добавить новый сертификат → Let's Encrypt**.
3. Домен: `tasklerq.stellalupus.ru`.
4. Назначить сертификат reverse proxy-записи Tasklerq.
5. Убедиться, что порт 80 доступен снаружи для HTTP-01; если нет — использовать
   DNS-01 или импортировать сертификат вручную.

Проверка:

```bash
curl -fsSI https://tasklerq.stellalupus.ru/health/live
```

До реализации сервиса endpoint вернёт `502`/`404` — это нормально; важно, чтобы
TLS уже был валиден.

### 3.3. Если reverse proxy не Synology

Для полноты: подойдут Caddy, nginx или Traefik с тем же правилом
`tasklerq.stellalupus.ru:443 → 127.0.0.1:18080`. Но в этой среде используется
DSM, поэтому основной сценарий — раздел 3.1.

## 4. PostgreSQL

PostgreSQL уже запущен:

```text
container: postgres
image:     postgres:18-alpine
network:   shared-db
host port: 15433 → 5432
user:      postgres
database:  postgres
```

Tasklerq подключается к контейнеру `postgres` по внутренней сети `shared-db`,
поэтому host-порт `15433` ему не нужен. Порт `15433` нужен только для
администрирования с NAS.

### 4.1. Создание роли и базы

```bash
sudo /usr/local/bin/docker exec -it postgres psql -U postgres
```

```sql
CREATE ROLE tasklerq LOGIN PASSWORD 'СГЕНЕРИРУЙТЕ_СЛОЖНЫЙ_ПАРОЛЬ';
CREATE DATABASE tasklerq OWNER tasklerq;

\c tasklerq
CREATE EXTENSION IF NOT EXISTS pg_trgm;
```

`pg_trgm` 1.6 доступен в этом контейнере. Если расширение не удалось
установить, Tasklerq использует `ILIKE`; это медленнее, но функционально.

Проверка:

```bash
sudo /usr/local/bin/docker exec -it postgres psql -U postgres -c "\l"
sudo /usr/local/bin/docker exec -it postgres psql -U tasklerq -d tasklerq -c "select extname from pg_extension;"
```

### 4.2. Строка подключения

Внутри сети `shared-db`:

```dotenv
DATABASE_URL=postgres://tasklerq:ПАРОЛЬ@postgres:5432/tasklerq?sslmode=disable
```

С NAS (администрирование):

```bash
psql "postgres://postgres@127.0.0.1:15433/postgres"
```

Параметры:

- PostgreSQL 18 (поддерживается 15+).
- Пул 5–20 соединений; `statement_timeout` 30 с.
- Роль `tasklerq` не суперпользователь и имеет доступ только к своей БД.
- Не открывайте `15433` в интернет.
- На базу `tasklerq` отозван `CONNECT` у `PUBLIC`: другие роли (например, `ha`)
  к ней не подключатся; роль `tasklerq` к `homeassistant` тоже.
- Для внешней базы используйте `sslmode=require`.

## 5. Telegram-бот

Бот уже создан: **[@tasklerqbot](https://t.me/tasklerqbot)**. Deep links для
приглашений имеют вид `https://t.me/tasklerqbot?start=<code>`.

1. **@BotFather** → `/mybots` → `@tasklerqbot` → **API Token** → получите
   `TELEGRAM_BOT_TOKEN`.
2. Придумайте `TELEGRAM_WEBHOOK_SECRET`:

```bash
openssl rand -hex 32
```

3. Пропишите в `.env`:

```dotenv
TELEGRAM_BOT_TOKEN=123456:AA...
TELEGRAM_WEBHOOK_SECRET=...
PUBLIC_BASE_URL=https://tasklerq.stellalupus.ru
TELEGRAM_WEBHOOK_URL=https://tasklerq.stellalupus.ru/webhooks/telegram
TELEGRAM_MODE=webhook
```

4. После запуска Tasklerq проверьте webhook:

```bash
curl -sS "https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/getWebhookInfo" | jq
```

Ожидаемо:

```json
{
  "ok": true,
  "result": {
    "url": "https://tasklerq.stellalupus.ru/webhooks/telegram",
    "has_custom_certificate": false,
    "pending_update_count": 0,
    "last_error_message": null
  }
}
```

5. Если нужно зарегистрировать webhook вручную:

```bash
curl -sS -X POST \
  "https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/setWebhook" \
  -H "Content-Type: application/json" \
  -d "{
    \"url\": \"${PUBLIC_BASE_URL}/webhooks/telegram\",
    \"secret_token\": \"${TELEGRAM_WEBHOOK_SECRET}\",
    \"allowed_updates\": [\"message\", \"callback_query\"],
    \"drop_pending_updates\": true
  }"
```

`allowed_updates` можно расширить, если появятся новые типы; для MVP
достаточно `message` и `callback_query`.

## 6. Переменные окружения

| Переменная | Обязательна | Пример | Назначение |
| --- | --- | --- | --- |
| `ENV` | да | `production` | `production`/`development`; запрещает polling в prod. |
| `BIND_ADDR` | нет | `0.0.0.0:8080` | Внутренний адрес HTTP-сервера. |
| `PUBLIC_BASE_URL` | да | `https://tasklerq.stellalupus.ru` | Публичный URL. |
| `DATABASE_URL` | да | `postgres://tasklerq:...@postgres:5432/tasklerq?sslmode=disable` | PostgreSQL в сети `shared-db`. |
| `DATABASE_MAX_CONNECTIONS` | нет | `10` | Размер пула. |
| `TELEGRAM_BOT_TOKEN` | при `webhook`/`polling` | `123:AA...` | Токен бота; не нужен при `disabled`. |
| `TELEGRAM_WEBHOOK_SECRET` | при `webhook` | `hex` | Секрет webhook. |
| `TELEGRAM_WEBHOOK_URL` | при `webhook` | `https://tasklerq.stellalupus.ru/webhooks/telegram` | Регистрация webhook. |
| `TELEGRAM_MODE` | нет | `webhook` | `webhook`/`polling` (dev)/`disabled` (headless). |
| `CAPTURE_API_ENABLED` | нет | `true` | Включить `POST /api/v1/capture`. |
| `CAPTURE_VOICE_TIMEOUT_MS` | нет | `8000` | Таймаут LLM для `source=ha_voice`. |
| `CAPTURE_VOICE_FALLBACK_RAW` | нет | `true` | Создавать позицию из текста при сбое LLM. |
| `CAPTURE_RATE_LIMIT_PER_MIN` | нет | `10` | Capture в минуту на токен. |
| `CAPTURE_MAX_TEXT_CHARS` | нет | `2000` | Максимальная длина текста. |
| `HA_BASE_URL` | при LLM | `http://host.docker.internal:8123` | Home Assistant из контейнера Tasklerq. |
| `HA_TOKEN` | при LLM | `eyJ...` | Long-lived token HA. |
| `HA_AI_TASK_ENTITY_ID` | при LLM | `ai_task.google_ai_task_2` | AI Task. |
| `LLM_CAPTURE_ENABLED` | нет | `true` | Включить разбор. |
| `LLM_DAILY_QUOTA` | нет | `200` | Разборов в сутки на household. |
| `QUEUE_MAX_ATTEMPTS` | нет | `10` | Попытки обработки update. |
| `QUEUE_READY_LAG_SECONDS` | нет | `300` | Порог readiness. |
| `RETENTION_UPDATES_DAYS` | нет | `7` | Хранение входящих обновлений. |
| `RETENTION_DRAFTS_DAYS` | нет | `30` | Хранение черновиков и `raw_text`. |
| `RETENTION_LLM_RUNS_DAYS` | нет | `90` | Метаданные LLM. |
| `RETENTION_EVENTS_DAYS` | нет | `365` | События задач. |
| `RETENTION_DELETED_DAYS` | нет | `30` | Мягко удалённые данные. |
| `RETENTION_OUTBOX_DAYS` | нет | `30` | Отправленный outbox. |
| `RETENTION_AUDIT_DAYS` | нет | `365` | События членства и безопасности. |
| `RUST_LOG` | нет | `info,tasklerq=debug` | Логи. |
| `TZ` | нет | `UTC` | Часовой пояс логов. |

`HA_BASE_URL` — это адрес **Home Assistant для Tasklerq**, а не наоборот.
Поскольку HA работает в host-режиме, из bridge-контейнера Tasklerq используйте
`http://host.docker.internal:8123` (нужен `extra_hosts`) или
`http://192.168.10.110:8123`.

Если Telegram не нужен, установите `TELEGRAM_MODE=disabled`; `TELEGRAM_BOT_TOKEN`
и webhook-секрет в этом режиме не требуются. HTTP API, HA-компонент и внешний
capture продолжают работать.

Секреты:

```bash
chmod 600 .env
```

Не коммитьте `.env`. Для production можно использовать Docker secrets.

## 7. Docker Compose

```yaml
services:
  tasklerq:
    build: .
    image: tasklerq:0.1.0
    container_name: tasklerq
    restart: unless-stopped
    env_file: .env
    ports:
      - "127.0.0.1:18080:8080"
    networks:
      - shared-db
    extra_hosts:
      - "host.docker.internal:host-gateway"
    read_only: true
    tmpfs:
      - /tmp
    security_opt:
      - no-new-privileges:true
    user: "10001:10001"
    healthcheck:
      test:
        - CMD-SHELL
        - 'exec /usr/local/bin/tasklerq healthcheck --url "http://127.0.0.1:$${BIND_ADDR##*:}/health/ready"'
      interval: 30s
      timeout: 5s
      retries: 3
      start_period: 20s
    deploy:
      resources:
        limits:
          cpus: "1.0"
          memory: 512M

networks:
  shared-db:
    external: true
```

Особенности:

- Внутренний порт контейнера — `8080`; host-порт — `18080`, потому что `8080`
  занят Zigbee2MQTT.
- Сеть `shared-db` уже существует; в ней работает `postgres`.
- Сеть `ha` не нужна: HA в `network_mode: host`.
- `extra_hosts` нужен, чтобы Tasklerq мог обращаться к HA по
  `host.docker.internal:8123`. Если Synology не поддерживает
  `host-gateway`, используйте `http://192.168.10.110:8123` и удалите
  `extra_hosts`.

Проверка:

```bash
sudo /usr/local/bin/docker compose up -d
sudo /usr/local/bin/docker compose ps
curl -fsS http://127.0.0.1:18080/health/live
curl -fsS http://127.0.0.1:18080/health/ready
sudo /usr/local/bin/docker compose logs -f --tail=100 tasklerq
```

### Проверка порта

```bash
sudo ss -ltnp | grep -E ':18080|:8080'
```

Ожидаемо: `18080` слушает Tasklerq на `127.0.0.1`, `8080` — Zigbee2MQTT на
`0.0.0.0`.

## 8. Миграции

Вариант A — автоматически при старте (по умолчанию):

- Tasklerq применяет миграции под `pg_advisory_lock`.
- Если миграция падает, сервис не переходит в рабочий режим.
- Повторный запуск не применяет миграции повторно.

Вариант B — вручную перед запуском:

```bash
sudo /usr/local/bin/docker compose run --rm tasklerq migrate
sudo /usr/local/bin/docker compose up -d tasklerq
```

Рекомендуется для production, чтобы контролировать момент изменения схемы.

## 9. Первый запуск

1. Создайте БД и роль `tasklerq` (раздел 4.1).
2. Заполните `.env`.
3. Запустите Tasklerq.
4. Проверьте `/health/ready` на `http://127.0.0.1:18080`.
5. Откройте бота, отправьте `/start`, создайте household: токены API
   принадлежат household, поэтому он должен существовать до выдачи токена. Без
   Telegram (`TELEGRAM_MODE=disabled`) или чтобы не проходить онбординг,
   создайте его командой
   `tasklerq admin household create --name "Семья" --timezone Europe/Moscow` и
   добавьте владельца по Telegram ID:
   `tasklerq admin user add --telegram-id <ваш_id> --role owner --name "Имя"`.
   Пока вы не нажали `/start`, бот не сможет написать вам первым.
6. Создайте API-токен для HA:

```bash
sudo /usr/local/bin/docker compose exec tasklerq \
  tasklerq admin token create \
  --name "Home Assistant" \
  --scopes ha:read,ha:write,capture:write
```

`capture:write` нужен сервису `tasklerq.capture` и голосовому сценарию Assist.
Сохраните выданный токен в переменную `TASKLERQ_TOKEN` — он понадобится ниже.

7. Установите custom component в HA (см. [`home-assistant.md`](home-assistant.md)).
8. Настройте интеграцию: URL `http://127.0.0.1:18080`, токен.
9. Проверьте, что появились `todo`-сущности и `calendar.tasklerq`.
10. Пригласите семью: **Настройки → Семья → Пригласить**.
11. Отправьте тестовую фразу и подтвердите черновик.
12. Проверьте capture без бота:

```bash
curl -fsS -X POST http://127.0.0.1:18080/api/v1/capture \
  -H "Authorization: Bearer $TASKLERQ_TOKEN" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: $(date +%s)" \
  -d '{"text":"купить молоко и хлеб","source":"api","mode":"auto","hint":"shopping"}'
```

Ожидаемо: `status=applied`, две покупки и `speech` в ответе.

13. Настройте Assist (пример `conversation.intents` + `intent_script` в
    [`home-assistant.md`](home-assistant.md#71-голосовой-capture-через-assist)) и
    произнесите «купи молоко и хлеб». Убедитесь, что позиции появились в
    `todo.pokupki`, а Assist озвучил ответ.

## 10. Сеть с Home Assistant

HA работает в `network_mode: host`, Tasklerq — в bridge-сети:

| Направление | Адрес | Комментарий |
| --- | --- | --- |
| HA → Tasklerq | `http://127.0.0.1:18080` | HA в host-режиме видит loopback NAS. |
| Tasklerq → HA | `http://host.docker.internal:8123` | Через `extra_hosts`; альтернатива — `http://192.168.10.110:8123`. |
| Tasklerq → PostgreSQL | `postgres:5432` | Общая сеть `shared-db`. |
| Telegram → Tasklerq | `https://tasklerq.stellalupus.ru` | Synology Reverse Proxy → `127.0.0.1:18080`. |

Если HA переедет в bridge-сеть, добавьте общую сеть `ha` и используйте
`http://tasklerq:8080`; но текущая конфигурация — host.

Проверка из контейнера HA:

```bash
sudo /usr/local/bin/docker exec -it homeassistant sh
wget -qO- http://127.0.0.1:18080/health/live
```

Проверка из Tasklerq к HA:

```bash
sudo /usr/local/bin/docker compose exec tasklerq \
  wget -qO- http://host.docker.internal:8123/ | head
```

## 11. Резервное копирование

Ежедневный дамп через контейнер `postgres`:

```bash
mkdir -p /volume2/backups/tasklerq

sudo /usr/local/bin/docker exec postgres \
  pg_dump -U postgres -Fc tasklerq \
  > "/volume2/backups/tasklerq/tasklerq-$(date +%F).dump"

find /volume2/backups/tasklerq -type f -mtime +14 -delete
```

Cron:

```cron
15 3 * * * /usr/local/bin/tasklerq-backup.sh >> /var/log/tasklerq-backup.log 2>&1
```

Восстановление:

```bash
sudo /usr/local/bin/docker exec -it postgres createdb -U postgres tasklerq_restore
sudo /usr/local/bin/docker exec -i postgres \
  pg_restore -U postgres -d tasklerq_restore --no-owner \
  < /volume2/backups/tasklerq/tasklerq-2026-11-03.dump
```

Затем:

1. Остановите Tasklerq.
2. Укажите `DATABASE_URL` на восстановленную базу.
3. Запустите `tasklerq migrate` (должно быть no-op).
4. Запустите сервис.
5. Проверьте `/health/ready` и несколько задач.

Локальные дампы хранятся 14 дней. Копии должны быть вынесены за пределы NAS:
используется **Synology Hyper Backup на внешний диск**.

1. Hyper Backup → **Задание резервного копирования** → источник — папка
   `/volume2/backups/tasklerq` на NAS, назначение — внешний диск.
2. Расписание: ежедневно после `03:15`, например в `04:00`.
3. Включить версионирование и шифрование; пароль хранить вне NAS.
4. Проверить, что задание завершилось без ошибок, и раз в квартал выполнить
   restore drill на чистую базу.

Права на каталог дампов — только `root`/администратор NAS.

## 12. Обновления и откат

```bash
# 1. Backup
/usr/local/bin/tasklerq-backup.sh

# 2. Новый образ
sudo /usr/local/bin/docker compose pull tasklerq
sudo /usr/local/bin/docker compose run --rm tasklerq migrate
sudo /usr/local/bin/docker compose up -d tasklerq

# 3. Проверка
curl -fsS http://127.0.0.1:18080/health/ready
sudo /usr/local/bin/docker compose logs --tail=100 tasklerq
```

Откат:

```bash
sudo /usr/local/bin/docker compose down
# изменить image на предыдущий тег
sudo /usr/local/bin/docker compose up -d tasklerq
```

Схема должна быть совместима с предыдущей версией (expand/contract). Если
миграция разрушительная — откат только через restore backup.

## 13. Мониторинг и логи

```bash
sudo /usr/local/bin/docker compose logs -f --tail=200 tasklerq
curl -fsS http://127.0.0.1:18080/health/live
curl -fsS http://127.0.0.1:18080/health/ready
```

Логи — JSON. Полезные поля:

- `request_id`;
- `update_id`;
- `route`, `status`, `duration_ms`;
- `error_code`;
- `version`.

Метрики (если включены):

- `telegram_updates_total{status}`;
- `telegram_queue_lag_seconds`;
- `outbox_pending`;
- `llm_requests_total{status}`;
- `llm_latency_seconds`;
- `http_requests_total{route,status}`;
- `db_pool_in_use`.

Алерты:

| Условие | Действие |
| --- | --- |
| `/health/ready` ≠ 200 > 2 мин | Проверить БД, очередь, логи. |
| queue lag > 5 мин | Проверить воркеры и БД. |
| LLM errors > 20% за 15 мин | Проверить HA AI Task, квоту, Gemini. |
| webhook 401 > 50 за 5 мин | Проверить секрет и источник запросов. |
| backup failed | Проверить диск, права, PostgreSQL. |
| disk > 80% | Очистить старые backup/логи. |

## 14. Retention

| Данные | Срок | Настройка |
| --- | --- | --- |
| `telegram_updates` | 7 дней | `RETENTION_UPDATES_DAYS` |
| `capture_drafts` | 30 дней | `RETENTION_DRAFTS_DAYS` |
| `llm_runs` | 90 дней | `RETENTION_LLM_RUNS_DAYS` |
| `item_events` | 365 дней | `RETENTION_EVENTS_DAYS` |
| soft-deleted items | 30 дней | `RETENTION_DELETED_DAYS` |
| sent outbox | 30 дней | `RETENTION_OUTBOX_DAYS` |
| `audit_events` | 365 дней | `RETENTION_AUDIT_DAYS` |

Исходный текст LLM-черновика (`raw_text`) хранится **30 дней** (подтверждено).
Фоновое задание запускается ежедневно. Перед изменением сроков проверьте
требования к аудиту и backup.

## 15. Security checklist

- [ ] DNS указывает на NAS; TLS-сертификат `tasklerq.stellalupus.ru` валиден.
- [ ] Synology Reverse Proxy направляет `tasklerq.stellalupus.ru:443` на
  `127.0.0.1:18080`.
- [ ] `18080` доступен только с loopback; `8080` остаётся за Zigbee2MQTT.
- [ ] PostgreSQL `15433` не доступен из интернета.
- [ ] Роль `tasklerq` не суперпользователь.
- [ ] `.env` имеет права `0600`; секреты не в git.
- [ ] `TELEGRAM_WEBHOOK_SECRET` случайный и длинный.
- [ ] Webhook возвращает `401` без секрета.
- [ ] API-токен HA имеет минимальные scopes (`ha:read`, `ha:write`,
  `capture:write`) и не используется в шаблонах.
- [ ] `CAPTURE_API_ENABLED` соответствует политике; при `false` эндпоинты
  capture возвращают `503`.
- [ ] Внешний capture требует `capture:write`; анонимный запрос — `401`.
- [ ] Лимиты capture (`10/мин` на токен, тело 16 КБ, текст 2000) включены.
- [ ] `Idempotency-Key` уникален; повтор не создаёт дубликаты.
- [ ] `TELEGRAM_MODE=webhook` в production; для headless — `disabled` без
  `TELEGRAM_BOT_TOKEN`.
- [ ] AI Task `ai_task.google_ai_task_2` не имеет Assist-инструментов.
- [ ] `LLM_CAPTURE_ENABLED` соответствует политике семьи.
- [ ] Логи не содержат текст сообщений и токены; для capture — только `source`,
  `status`, `capture_id`.
- [ ] Аудио голосового capture не передаётся в Tasklerq и не хранится.
- [ ] `CAPTURE_VOICE_TIMEOUT_MS=8000` проверен на голосовом pipeline.
- [ ] Backup настроен и проверен restore.
- [ ] Обновления зависимостей и образов регулярны.
- [ ] HA long-lived token хранится в секретах, не в шаблонах.
- [ ] Reverse proxy не логирует тело webhook.

## 16. Runbooks

### 16.1. Webhook 401

1. Проверьте `TELEGRAM_WEBHOOK_SECRET` в `.env` и в `setWebhook`.
2. `getWebhookInfo` — нет ли ошибок.
3. Перерегистрируйте webhook.
4. Проверьте, не удаляет ли Synology Reverse Proxy заголовок
   `X-Telegram-Bot-Api-Secret-Token`.
5. Перезапустите Tasklerq.

### 16.2. Очередь растёт

1. `/health/ready` — статус.
2. `tasklerq admin queue stats`.
3. Логи воркеров: ошибки БД/Telegram.
4. Проверьте `telegram_updates` по статусам.
5. Если Telegram 429 — подождите, outbox сам повторит.
6. Если БД медленная — проверьте пул, блокировки, диск.

### 16.3. PostgreSQL недоступен

1. `sudo /usr/local/bin/docker ps --filter name=postgres`.
2. `sudo /usr/local/bin/docker exec postgres pg_isready -U postgres`.
3. Проверьте `DATABASE_URL`, сеть `shared-db`, пароль.
4. Проверьте диск и логи PostgreSQL.
5. Если база потеряна — restore backup.
6. После восстановления — `/health/ready`, проверить задачи.

### 16.4. Ошибка миграции

1. Не запускайте сервис в рабочий режим.
2. Прочитайте лог: имя миграции и SQL-ошибка.
3. Восстановите backup, если схема частично изменена.
4. Исправьте миграцию; проверьте на копии базы.
5. Для production — expand/contract, без разрушительных изменений.

### 16.5. Ротация токенов

**API-токен HA:**

1. Создайте новый токен.
2. Обновите токен в интеграции HA (reauth).
3. Убедитесь, что сущности доступны.
4. Отзовите старый токен.

**Токен бота:**

1. BotFather → `/revoke` → новый токен.
2. Обновите `TELEGRAM_BOT_TOKEN`.
3. Перезапустите Tasklerq.
4. Проверьте `setWebhook`/`getWebhookInfo`.

### 16.6. HA не подключается

1. Проверьте URL из контейнера HA: `http://127.0.0.1:18080`.
2. Проверьте, что Tasklerq слушает `127.0.0.1:18080`.
3. `curl` из HA к `/health/live`.
4. Проверьте токен и scopes.
5. Diagnostics: последняя ошибка.
6. Перезапустите интеграцию.

### 16.7. Tasklerq не видит HA

1. Проверьте `HA_BASE_URL` — это адрес HA из Tasklerq.
2. Для host-режима HA используйте `http://host.docker.internal:8123` или
   `http://192.168.10.110:8123`.
3. Проверьте `extra_hosts` в compose.
4. Проверьте `HA_TOKEN` и срок действия.
5. Вызовите `ai_task.generate_data` в Developer Tools.

### 16.8. LLM не работает

1. Проверьте `HA_AI_TASK_ENTITY_ID=ai_task.google_ai_task_2`.
2. Вызовите `ai_task.generate_data` в Developer Tools.
3. Проверьте `HA_TOKEN` и срок действия.
4. Проверьте квоту Gemini и лимиты.
5. Посмотрите `llm_runs` (без текста).
6. При необходимости `LLM_CAPTURE_ENABLED=false` — ручная форма.

### 16.9. Голосовой capture не работает

1. Проверьте `CAPTURE_API_ENABLED=true` и токен со scope `capture:write`.
2. Вызовите `tasklerq.capture` в **Developer Tools → Actions** с
   `text`, `mode=auto`, `hint=shopping`; посмотрите ответ и ошибку.
3. Проверьте `curl` к `POST /api/v1/capture` из контейнера HA.
4. Проверьте `conversation.intents` и `intent_script`; после правки
   перезапустите HA.
5. Проверьте `response.speech` в ответе сервиса — Assist озвучивает именно его.
6. Если LLM отвечает дольше `CAPTURE_VOICE_TIMEOUT_MS`, должна создаваться
   fallback-позиция; проверьте `fallback=true` и `capture_fallback_total`.
7. Если Tasklerq вернул `429`, проверьте лимит capture и суточную квоту LLM.
8. Для отмены последнего добавления вызовите `tasklerq.undo_last_capture`;
   `409` означает, что элемент уже изменён.

### 16.10. Откат релиза

1. Backup.
2. Остановить Tasklerq.
3. Указать предыдущий image tag.
4. Запустить.
5. Проверить `/health/ready`, задачи, HA.
6. Если схема несовместима — restore backup.

## 17. Локальная разработка

```bash
# PostgreSQL
docker compose -f docker-compose.dev.yml up -d postgres

# Миграции
cargo sqlx migrate run

# .env для dev
cat > .env <<'EOF'
ENV=development
DATABASE_URL=postgres://tasklerq:tasklerq@localhost:5432/tasklerq
TELEGRAM_BOT_TOKEN=test
TELEGRAM_MODE=polling
LLM_CAPTURE_ENABLED=false
RUST_LOG=debug
EOF

# Запуск
cargo run
```

Для webhook локально:

```bash
cloudflared tunnel --url http://localhost:8080
# или
ngrok http 8080
```

Для тестов:

```bash
cargo test
cargo clippy -- -D warnings
cargo fmt --check
pytest custom_components/tasklerq
openspec validate --strict
```

Моки Telegram и HA поднимаются в интеграционных тестах; интернет не нужен.

## 18. Что осталось уточнить

- Бюджет Gemini при квоте `200` разборов в сутки; модель AI Task выбирается в
  HA (в conversation-подынтеграциях видны `models/gemini-3.5-flash-lite` и
  `models/gemini-3.7-flash`).
- Инфраструктура готова: домен, reverse proxy, сертификат, роль и БД `tasklerq`.
  Блокирующих вопросов до реализации нет.
