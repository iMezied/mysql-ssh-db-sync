#!/usr/bin/env bash
# Assert that the test fixtures contain the hazards they are supposed to.
#
# The fixtures exist to catch the failures that broke the bash predecessor:
# DEFINER clauses, binary payloads, FK cycles, reserved-word and unicode
# identifiers, and non-transactional tables. If a fixture silently loses one of
# those, the tests built on it become theatre. This runs in CI after the
# containers come up.
#
# Usage: tests/verify-fixtures.sh

set -euo pipefail

MYSQL_CONTAINER="${MYSQL_CONTAINER:-db-sync-mysql-1}"
PG_CONTAINER="${PG_CONTAINER:-db-sync-postgres-1}"

failures=0

check() {
  local label=$1 expected=$2 actual=$3
  if [ "$actual" = "$expected" ]; then
    printf '  ok       %-38s %s\n' "$label" "$actual"
  else
    printf '  FAILED   %-38s expected %s, got %s\n' "$label" "$expected" "$actual"
    failures=$((failures + 1))
  fi
}

check_contains() {
  local label=$1 needle=$2 haystack=$3
  if [[ "$haystack" == *"$needle"* ]]; then
    printf '  ok       %-38s %s\n' "$label" "$needle"
  else
    printf '  FAILED   %-38s expected to contain %s, got %s\n' "$label" "$needle" "$haystack"
    failures=$((failures + 1))
  fi
}

my() {
  docker exec "$MYSQL_CONTAINER" mysql -uroot -ptestroot fixture -N -B -e "$1" 2>/dev/null
}

pg() {
  docker exec "$PG_CONTAINER" psql -U dbsync -d fixture -t -A -c "$1" 2>/dev/null | tr -d '[:space:]'
}

echo "MySQL fixture"
check "base tables" "20" "$(my "SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA='fixture' AND TABLE_TYPE='BASE TABLE';")"
check "views" "2" "$(my "SELECT COUNT(*) FROM information_schema.VIEWS WHERE TABLE_SCHEMA='fixture';")"
check "routines" "2" "$(my "SELECT COUNT(*) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA='fixture';")"
check "triggers" "1" "$(my "SELECT COUNT(*) FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA='fixture';")"
check "routines carry a DEFINER" "2" "$(my "SELECT COUNT(*) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA='fixture' AND DEFINER <> '';")"
check "non-transactional table present" "1" "$(my "SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA='fixture' AND ENGINE='MyISAM';")"
check "foreign-key cycle present" "2" "$(my "SELECT COUNT(*) FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA='fixture' AND REFERENCED_TABLE_NAME IS NOT NULL AND TABLE_NAME IN ('employees','departments');")"
check "reserved-word table" "1" "$(my "SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA='fixture' AND TABLE_NAME='order';")"
check "unicode table" "1" "$(my "SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA='fixture' AND TABLE_NAME='日本語テーブル';")"
check "invalid-utf8 blob intact" "DEADBEEF00FF00FFC3289F" "$(my "SELECT HEX(payload) FROM attachments WHERE filename='binary.bin';")"
check_contains "DEFINER text stored as row data" 'DEFINER=`root`@`localhost`' "$(my "SELECT \`value\` FROM settings WHERE \`key\`='definer_note';")"

echo
echo "PostgreSQL fixture"
check "public tables" "20" "$(pg "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema='public' AND table_type='BASE TABLE';")"
check "non-public schema table" "1" "$(pg "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema='reporting';")"
check "views" "2" "$(pg "SELECT COUNT(*) FROM information_schema.views WHERE table_schema='public';")"
check "functions" "3" "$(pg "SELECT COUNT(*) FROM information_schema.routines WHERE routine_schema='public';")"
check "SECURITY DEFINER function" "1" "$(pg "SELECT COUNT(*) FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='public' AND p.prosecdef;")"
check "enum type" "1" "$(pg "SELECT COUNT(*) FROM pg_type WHERE typname='order_status';")"
check "deferrable cycle" "2" "$(pg "SELECT COUNT(*) FROM pg_constraint WHERE contype='f' AND condeferrable AND conrelid::regclass::text IN ('employees','departments');")"
check "reserved-word tables" "2" "$(pg "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema='public' AND table_name IN ('order','select');")"
check "unicode table" "1" "$(pg "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema='public' AND table_name='日本語テーブル';")"
check "invalid-utf8 bytea intact" "deadbeef00ff00ffc3289f" "$(pg "SELECT encode(payload,'hex') FROM attachments WHERE filename='binary.bin';")"

echo
if [ "$failures" -gt 0 ]; then
  echo "$failures fixture check(s) failed"
  exit 1
fi
echo "all fixture checks passed"
