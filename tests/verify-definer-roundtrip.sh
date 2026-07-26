#!/usr/bin/env bash
# End-to-end proof that DEFINER stripping works on a real mysqldump.
#
# Unit tests cover the filter against synthetic input. This covers the thing
# that actually matters: a dump taken from a live server, containing routines,
# views and triggers with DEFINER clauses *and* row data that merely mentions
# "DEFINER=", must restore cleanly as a user without SUPER — and must not have
# had its row data mangled in the process.
#
# Requires: docker compose -f docker-compose.test.yml up -d --wait
# Usage:    tests/verify-definer-roundtrip.sh

set -euo pipefail

MYSQL_CONTAINER="${MYSQL_CONTAINER:-db-sync-mysql-1}"
DBSYNC_BIN="${DBSYNC_BIN:-./target/debug/dbsync}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

failures=0
check() {
  local label=$1 expected=$2 actual=$3
  if [ "$actual" = "$expected" ]; then
    printf '  ok       %-40s %s\n' "$label" "$actual"
  else
    printf '  FAILED   %-40s expected %s, got %s\n' "$label" "$expected" "$actual"
    failures=$((failures + 1))
  fi
}

# --default-character-set is required: without it the client mangles the
# non-ASCII literals used in the assertions below.
my() {
  docker exec "$MYSQL_CONTAINER" mysql -uroot -ptestroot --default-character-set=utf8mb4 \
    -N -B -e "$1" 2>/dev/null
}

echo "Taking a dump from the fixture database"
docker exec "$MYSQL_CONTAINER" mysqldump -uroot -ptestroot \
  --single-transaction --hex-blob --routines --triggers --events \
  --set-gtid-purged=OFF --default-character-set=utf8mb4 \
  fixture > "$WORK/raw.sql" 2>/dev/null

raw_definers=$(grep -c 'DEFINER=' "$WORK/raw.sql" || true)
check "dump contains DEFINER clauses" "7" "$raw_definers"

echo "Stripping DEFINER clauses"
"$DBSYNC_BIN" strip-definers < "$WORK/raw.sql" > "$WORK/stripped.sql"

# The two survivors are inside string literals in INSERT statements: they are
# row data, and corrupting them is the classic `sed 's/DEFINER=[^ ]* //g'` bug.
remaining=$(grep -c 'DEFINER=' "$WORK/stripped.sql" || true)
check "row data mentioning DEFINER preserved" "2" "$remaining"
check "no DEFINER clause survives on a CREATE" "0" \
  "$(grep -c '^\s*\(CREATE\|/\*!5[0-9]*\s*CREATE\).*DEFINER=' "$WORK/stripped.sql" || true)"

echo "Restoring both dumps as a user without SUPER"
my "DROP DATABASE IF EXISTS definer_raw;   CREATE DATABASE definer_raw   CHARACTER SET utf8mb4;
    DROP DATABASE IF EXISTS definer_strip; CREATE DATABASE definer_strip CHARACTER SET utf8mb4;
    GRANT ALL PRIVILEGES ON definer_raw.*   TO 'dbsync'@'%';
    GRANT ALL PRIVILEGES ON definer_strip.* TO 'dbsync'@'%';
    FLUSH PRIVILEGES;"

raw_result=ok
docker exec -i "$MYSQL_CONTAINER" mysql -udbsync -ptestpass definer_raw \
  < "$WORK/raw.sql" 2>"$WORK/raw.err" || true
grep -q '1227' "$WORK/raw.err" && raw_result=denied
check "raw dump is rejected without SUPER" "denied" "$raw_result"

strip_result=ok
docker exec -i "$MYSQL_CONTAINER" mysql -udbsync -ptestpass definer_strip \
  < "$WORK/stripped.sql" 2>"$WORK/strip.err" || strip_result=failed
grep -q 'ERROR' "$WORK/strip.err" && strip_result=failed
check "stripped dump restores without SUPER" "ok" "$strip_result"

echo "Checking the restored data survived intact"
check "all tables restored" "20" \
  "$(my "SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA='definer_strip' AND TABLE_TYPE='BASE TABLE';")"
check "views restored" "2" \
  "$(my "SELECT COUNT(*) FROM information_schema.VIEWS WHERE TABLE_SCHEMA='definer_strip';")"
check "triggers restored" "1" \
  "$(my "SELECT COUNT(*) FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA='definer_strip';")"
check "binary payload byte-identical" "yes" \
  "$(my "SELECT IF(HEX(payload)='DEADBEEF00FF00FFC3289F','yes','NO') FROM definer_strip.attachments WHERE filename='binary.bin';")"
check "unicode data intact" "yes" \
  "$(my "SELECT IF(\`名前\`='テスト','yes','NO') FROM definer_strip.\`日本語テーブル\` LIMIT 1;")"
check "DEFINER text in row data intact" "yes" \
  "$(my "SELECT IF(\`value\`='DEFINER=\`root\`@\`localhost\`','yes','NO') FROM definer_strip.settings WHERE \`key\`='definer_note';")"
check "escaped apostrophe intact" "yes" \
  "$(my "SELECT IF(action LIKE '%it''s fine%','yes','NO') FROM definer_strip.audit_log WHERE id=2;")"
check "foreign-key cycle restored" "1" \
  "$(my "SELECT head_id FROM definer_strip.departments WHERE id=1;")"

echo
if [ "$failures" -gt 0 ]; then
  echo "$failures check(s) failed"
  exit 1
fi
echo "all DEFINER round-trip checks passed"
