#!/usr/bin/env bash
# Восстановление дампа в новую базу через docker exec.
#
# Рабочая база не затрагивается: скрипт отказывается работать, если база с
# указанным именем уже существует, и никогда не передаёт pg_restore --clean.
set -euo pipefail

if [ "$#" -ne 2 ]; then
    printf 'usage: %s <dump-file> <new-database>\n' "$(basename "$0")" >&2
    exit 2
fi

DUMP_FILE="$1"
NEW_DB="$2"
PG_CONTAINER="${TASKLERQ_PG_CONTAINER:-postgres}"
PG_USER="${TASKLERQ_PG_USER:-postgres}"
DOCKER="${DOCKER:-docker}"

fail() {
    printf 'tasklerq-restore: %s\n' "$*" >&2
    exit 1
}

[ -f "$DUMP_FILE" ] || fail "dump file '$DUMP_FILE' not found"
[[ "$NEW_DB" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] ||
    fail "invalid database name '$NEW_DB'"
command -v "$DOCKER" >/dev/null 2>&1 || fail "command '$DOCKER' not found"

if ! existing="$("$DOCKER" exec "$PG_CONTAINER" \
    psql --username="$PG_USER" --dbname=postgres --tuples-only --no-align \
    --command="SELECT 1 FROM pg_database WHERE datname = '$NEW_DB'")"; then
    fail "cannot inspect databases in container '$PG_CONTAINER'"
fi
existing="${existing//[[:space:]]/}"
[ -z "$existing" ] || fail "database '$NEW_DB' already exists"

"$DOCKER" exec "$PG_CONTAINER" createdb --username="$PG_USER" "$NEW_DB" ||
    fail "cannot create database '$NEW_DB'"

"$DOCKER" exec -i "$PG_CONTAINER" \
    pg_restore --username="$PG_USER" --dbname="$NEW_DB" --no-owner \
    --exit-on-error <"$DUMP_FILE" ||
    fail "pg_restore failed for '$DUMP_FILE' into '$NEW_DB'; database '$NEW_DB' was created and remains, remove it with: $DOCKER exec $PG_CONTAINER dropdb --username=$PG_USER $NEW_DB"

printf 'restored %s into %s\n' "$DUMP_FILE" "$NEW_DB"
