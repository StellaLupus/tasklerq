#!/usr/bin/env bash
# Ежедневный дамп PostgreSQL через docker exec.
#
# На проде pg_dump есть только внутри контейнера postgres, поэтому дамп
# снимается через `docker exec`, а не локальным клиентом. Файл сначала
# пишется во временное имя и только после успеха переименовывается: частичный
# дамп не остаётся под итоговым именем.
set -euo pipefail

PG_CONTAINER="${TASKLERQ_PG_CONTAINER:-postgres}"
PG_USER="${TASKLERQ_PG_USER:-postgres}"
DB="${TASKLERQ_DB:-tasklerq}"
BACKUP_DIR="${TASKLERQ_BACKUP_DIR:-/volume2/backups/tasklerq}"
KEEP_DAYS="${TASKLERQ_BACKUP_KEEP_DAYS:-14}"
DOCKER="${DOCKER:-docker}"

fail() {
    printf 'tasklerq-backup: %s\n' "$*" >&2
    exit 1
}

command -v "$DOCKER" >/dev/null 2>&1 || fail "command '$DOCKER' not found"
mkdir -p "$BACKUP_DIR" || fail "cannot create backup directory '$BACKUP_DIR'"

timestamp="$(date -u +%Y-%m-%dT%H%M%S)" || fail "cannot read the current time"
target="$BACKUP_DIR/tasklerq-$timestamp.dump"
partial="$(mktemp "$BACKUP_DIR/.tasklerq-$timestamp.XXXXXX")" ||
    fail "cannot create a temporary file in '$BACKUP_DIR'"

cleanup() {
    rm -f -- "$partial"
}
trap cleanup EXIT HUP INT TERM

"$DOCKER" exec "$PG_CONTAINER" \
    pg_dump --username="$PG_USER" --format=custom "$DB" >"$partial" ||
    fail "pg_dump failed for database '$DB' in container '$PG_CONTAINER'"

mv -f -- "$partial" "$target" || fail "cannot move the dump into place"
trap - EXIT HUP INT TERM

find "$BACKUP_DIR" -maxdepth 1 -type f -name 'tasklerq-*.dump' \
    -mtime "+$KEEP_DAYS" -delete ||
    fail "cannot remove backups older than $KEEP_DAYS days"

printf '%s\n' "$target"
