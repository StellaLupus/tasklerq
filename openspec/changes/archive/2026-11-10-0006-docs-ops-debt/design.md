# Design

## Context

Миграции встроены в бинарник: статический `MIGRATOR` в
`src/storage/migrations.rs` строится через `sqlx::migrate!`. `run()`
вызывается из `src/main.rs` в трёх местах: `serve` (стр. 316), `migrate`
(368) и `seed` (383). `all_applied()` для readiness проверяет, что каждая
встроенная версия есть в `_sqlx_migrations` и что там нет записей с
`success = false`. `tests/migrations.rs` использует `MIGRATOR` напрямую:
`.locking`, `.iter()`, `.run()`.

sqlx 0.8.6 по умолчанию сверяет применённые миграции со встроенными. Версия
в базе, которой нет в образе, даёт `VersionMissing`. `set_ignore_missing(true)`
отключает только эту сверку. Проверка dirty-версии и контрольных сумм
известных миграций остаётся (`sqlx-core migrate/migrator.rs:155-178`).

На Postgres sqlx пишет строку в `_sqlx_migrations` только с `success = TRUE`,
в той же транзакции, что и SQL миграции (`sqlx-postgres migrate.rs:286-288`).
Состояния «незавершённая миграция» на практике нет, это совпадает с
`docs/deployment.md` §16.4.

## Goals / Non-Goals

**Goals:**

- Откат тегом после аддитивной миграции работает без restore, если целевой
  образ содержит это изменение.
- Оператор видит в логе, что база новее образа.
- CI на `main` снова зелёный.

**Non-Goals:**

- Down-миграции и автоматическая проверка совместимости схемы.
- Откат на образы без этого изменения, включая v0.1.0. Для них остаётся
  §16.10 с restore.
- Обновление `actions/checkout` и `Swatinem/rust-cache`. GitHub уже
  принудительно запускает их на Node 24, и это только предупреждение в
  аннотациях. Обновим, когда появится дедлайн или поломка.
- Переменная `TZ`. `docs/deployment.md` §6 уже прямо пишет, что сервис её
  не читает. Это осознанная документация, а не долг.

## Decisions

### 1. `MIGRATOR` как `LazyLock<Migrator>` с `set_ignore_missing(true)`

```rust
pub static MIGRATOR: LazyLock<Migrator> = LazyLock::new(|| {
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.set_ignore_missing(true);
    migrator
});
```

Используется публичный API sqlx. Поля `Migrator` скрыты (`#[doc(hidden)]`) и
не подпадают под semver, поэтому литерал вида `Migrator { ignore_missing:
true, .. }` не подходит. Благодаря `Deref` имя и использование `MIGRATOR` в
`run()`, `all_applied()` и `tests/migrations.rs` не меняются. `locking`
остаётся по умолчанию (`true`).

Альтернатива: функция, которая строит `Migrator`. Отклонена: пришлось бы
менять семь мест в тестах ради того же результата.

### 2. Предупреждение после применения

После успешного `MIGRATOR.run(pool)` таблица `_sqlx_migrations` точно
существует, и все встроенные версии применены. `run()` читает
`SELECT version FROM _sqlx_migrations` и отбирает версии, для которых
`!MIGRATOR.version_exists(v)`. Если список непуст, пишется один
`tracing::warn!` с полем `unknown_versions`. Ветка «таблицы нет» не нужна.

### 3. Readiness не меняется

`all_applied()` проверяет только встроенные версии и `success = false`.
Ничего менять не нужно.

### 4. `cargo audit` вместо `rustsec/audit-check`

Job `audit` падает на push: action создаёт check run, а прав `checks: write`
нет. Job запускает:
- `cargo install cargo-audit --locked`;
- `cargo audit --ignore RUSTSEC-2023-0071`.

Action и сейчас делает `cargo install` (около 3 минут), так что время не
меняется. Предупреждения вида unmaintained (сейчас RUSTSEC-2026-0440 у
`astral-tokio-tar` от `testcontainers`) `cargo audit` по умолчанию выводит,
но кодом ошибки не завершается. Уязвимости — завершается, как и требует
security checklist. Права job сужаются до `contents: read`.

Альтернатива: закрепить action на неопубликованном коммите `main` с Node 24
и выдать `checks: write`. Отклонена: зависимость от неопубликованного
коммита и лишние права ради отчёта, который никто не читает.

## Risks / Trade-offs

- [Старый образ стартует на несовместимой схеме и падает уже на запросах] →
  правило expand/contract в «Обновлениях и откате». Предупреждение называет
  новые версии, §16.10 описывает, что делать при ошибках БД после отката.
- [Опечатка в имени файла миграции тихо игнорируется] → на CI база
  создаётся с нуля тем же набором, на инсталляции версия видна в
  предупреждении.
- [`cargo install cargo-audit` без кэша медленный] → столько же, сколько
  сейчас внутри action.

## Migration Plan

Миграций нет. Откат тегом без restore начнёт работать с образа, который
содержит это изменение. Откат на более ранние образы, включая v0.1.0,
по-прежнему идёт через restore по §16.10.
