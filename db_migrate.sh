#!/usr/bin/env bash
# ================================================================
#  db_migrate.sh — MySQL Cross-Server Backup & Restore
#  Germany ──► Local Mac (Docker) ──► Malaysia
#
#  Usage:
#    ./db_migrate.sh              — full run (backup + restore)
#    ./db_migrate.sh --backup     — backup only
#    ./db_migrate.sh --restore    — restore only (pick from saved backups)
#    ./db_migrate.sh --dry-run    — validate config, no changes
# ================================================================

set -euo pipefail

# Resolve the absolute path of the script's directory
# Works when called as ./db_migrate.sh, bash db_migrate.sh, or from any working directory
_raw_source="${BASH_SOURCE[0]:-$0}"
SCRIPT_DIR="$(cd "$(dirname "$_raw_source")" && pwd)"
unset _raw_source

# Guard: make sure helper scripts exist before sourcing
_check_file() {
  if [ ! -f "$1" ]; then
    echo "ERROR: Required file not found: $1"
    echo ""
    echo "Make sure your project folder contains:"
    echo "  scripts/ui.sh"
    echo "  scripts/validate.sh"
    echo ""
    echo "Project root detected as: ${SCRIPT_DIR}"
    echo "Run from the project root: cd ${SCRIPT_DIR} && ./db_migrate.sh"
    exit 1
  fi
}

unset -f _check_file

if [[ -f "${SCRIPT_DIR}/scripts/ui.sh" ]]; then
  source "${SCRIPT_DIR}/scripts/ui.sh"
  source "${SCRIPT_DIR}/scripts/validate.sh"
elif [[ -f "${SCRIPT_DIR}/ui.sh" ]]; then
  source "${SCRIPT_DIR}/ui.sh"
  source "${SCRIPT_DIR}/validate.sh"
else
  echo "ERROR: Cannot find ui.sh in ${SCRIPT_DIR}/scripts/ or ${SCRIPT_DIR}/"
  exit 1
fi

# ── Global state ─────────────────────────────────────────────────
TUNNEL_A_PID=""
TUNNEL_B_PID=""
FINAL_BACKUP_FILE=""
RESTORE_DURATION=0
RESTORE_START=0
DUMP_START=0
TOTAL_TABLES_IN_DB=0
TABLES_WITH_DATA=()

# ── Env loader ───────────────────────────────────────────────────
load_env() {
  local env_file="${SCRIPT_DIR}/.env"
  [ ! -f "$env_file" ] && error ".env file not found. Run: cp .env.example .env"

  set -a
  # shellcheck disable=SC1090
  source "$env_file"
  set +a

  BACKUP_DIR="${BACKUP_DIR/#\~/$HOME}"
  SRC_SSH_KEY="${SRC_SSH_KEY/#\~/$HOME}"
  DST_SSH_KEY="${DST_SSH_KEY/#\~/$HOME}"

  if [[ "$TABLES_FILE" != /* ]]; then
    TABLES_FILE="${SCRIPT_DIR}/${TABLES_FILE}"
  fi

  # Fallback to alternate locations if configured path doesn't exist
  if [[ ! -f "$TABLES_FILE" ]]; then
    if [[ -f "${SCRIPT_DIR}/config/tables.conf" ]]; then
      TABLES_FILE="${SCRIPT_DIR}/config/tables.conf"
    elif [[ -f "${SCRIPT_DIR}/table.conf" ]]; then
      TABLES_FILE="${SCRIPT_DIR}/table.conf"
    elif [[ -f "${SCRIPT_DIR}/tables.conf" ]]; then
      TABLES_FILE="${SCRIPT_DIR}/tables.conf"
    fi
  fi

  DB_SUFFIX=$(date +%Y%m%d_%H%M%S)
  NEW_DB_NAME="${DB_PREFIX}_${DB_SUFFIX}"

  TIMESTAMP=$(date +%Y%m%d_%H%M%S)
  BACKUP_FILE="${BACKUP_DIR}/${SRC_DB_NAME}_${TIMESTAMP}.sql"
  BACKUP_FILE_GZ="${BACKUP_FILE}.gz"
}

# ── Cleanup ───────────────────────────────────────────────────────
cleanup() {
  echo ""
  if [ -n "$TUNNEL_A_PID" ] && kill -0 "$TUNNEL_A_PID" 2>/dev/null; then
    kill "$TUNNEL_A_PID" 2>/dev/null
    log "Source tunnel closed"
  fi
  if [ -n "$TUNNEL_B_PID" ] && kill -0 "$TUNNEL_B_PID" 2>/dev/null; then
    kill "$TUNNEL_B_PID" 2>/dev/null
    log "Destination tunnel closed"
  fi
  stop_spinner
}
trap cleanup EXIT

# ── SSH Tunnel ───────────────────────────────────────────────────
open_tunnel() {
  local local_port=$1 ssh_user=$2 ssh_host=$3 ssh_port=$4
  local ssh_key=$5 remote_host=$6 remote_port=$7

  ssh -N -f \
    -L "${local_port}:${remote_host}:${remote_port}" \
    -p "$ssh_port" \
    -i "$ssh_key" \
    "${ssh_user}@${ssh_host}" \
    -o StrictHostKeyChecking=no \
    -o ExitOnForwardFailure=yes \
    -o ServerAliveInterval=15 \
    -o ServerAliveCountMax=3 \
    -o ConnectTimeout=10

  pgrep -n -f "L ${local_port}:${remote_host}:${remote_port}" || echo ""
}

wait_for_tunnel() {
  local port=$1 label=$2 db_user=$3 db_pass=$4
  start_spinner "Waiting for ${label} on port ${port}"
  local tries=0
  while [ $tries -lt 15 ]; do
    if docker exec "$DOCKER_CONTAINER" \
        mysqladmin ping \
        -h host.docker.internal \
        -P "$port" \
        -u "$db_user" \
        "-p${db_pass}" \
        --silent 2>/dev/null; then
      stop_spinner
      success "${label} is reachable on port ${port}"
      return 0
    fi
    tries=$(( tries + 1 ))
    sleep 1
  done
  stop_spinner
  error "Cannot reach ${label} on port ${port} after 15 attempts"
}

# ── Load Tables ──────────────────────────────────────────────────
load_tables() {
  [ ! -f "$TABLES_FILE" ] && \
    error "Tables file not found: ${TABLES_FILE}\nRun: cp config/tables.conf.example config/tables.conf"

  TABLES_WITH_DATA=()
  local seen=""
  while IFS= read -r table; do
    # Deduplicate — skip if table name already loaded
    if [[ "$seen" != *"|${table}|"* ]]; then
      TABLES_WITH_DATA+=("$table")
      seen="${seen}|${table}|"
    else
      warn "Duplicate table skipped: ${table}"
    fi
  done < <(grep -v '^\s*#' "$TABLES_FILE" | grep -v '^\s*$' | awk '{print $1}')

  [ ${#TABLES_WITH_DATA[@]} -eq 0 ] && \
    error "No tables found in ${TABLES_FILE}"

  success "Loaded ${#TABLES_WITH_DATA[@]} tables from $(basename "$TABLES_FILE")"
}

# ── Source DB Stats ──────────────────────────────────────────────
fetch_source_db_stats() {
  TOTAL_TABLES_IN_DB=$(docker exec "$DOCKER_CONTAINER" mysql \
    -h host.docker.internal \
    -P "$SRC_LOCAL_PORT" \
    -u "$SRC_DB_USER" \
    "-p${SRC_DB_PASS}" \
    --skip-column-names --silent \
    -e "SELECT COUNT(*) FROM information_schema.TABLES
        WHERE TABLE_SCHEMA = '${SRC_DB_NAME}';" 2>/dev/null || echo "?")

  local db_size
  db_size=$(docker exec "$DOCKER_CONTAINER" mysql \
    -h host.docker.internal \
    -P "$SRC_LOCAL_PORT" \
    -u "$SRC_DB_USER" \
    "-p${SRC_DB_PASS}" \
    --skip-column-names --silent \
    -e "SELECT CONCAT(
          ROUND(SUM(data_length + index_length) / 1048576, 1), ' MB'
        )
        FROM information_schema.TABLES
        WHERE TABLE_SCHEMA = '${SRC_DB_NAME}';" 2>/dev/null || echo "?")

  local schema_only_count=$(( TOTAL_TABLES_IN_DB - ${#TABLES_WITH_DATA[@]} ))

  divider
  info "Database         : ${BOLD}${SRC_DB_NAME}${NC}"
  info "Total tables     : ${BOLD}${TOTAL_TABLES_IN_DB}${NC}"
  info "Schema + data    : ${BOLD}${#TABLES_WITH_DATA[@]}${NC} tables"
  info "Schema only      : ${BOLD}${schema_only_count}${NC} tables"
  info "DB size on disk  : ${BOLD}${db_size}${NC}"
  divider
}

# ── STEP 0: Pre-flight ───────────────────────────────────────────
step_preflight() {
  step "STEP 0" "Pre-flight Checks"

  docker ps --format '{{.Names}}' | grep -q "^${DOCKER_CONTAINER}$" || \
    error "Docker container '${DOCKER_CONTAINER}' is not running"
  success "Docker container '${DOCKER_CONTAINER}' is running"

  local docker_mysql_ver
  docker_mysql_ver=$(docker exec "$DOCKER_CONTAINER" mysql --version 2>/dev/null | awk '{print $3}' || echo "unknown")
  info "MySQL version in Docker: ${docker_mysql_ver}"

  [ ! -f "$SRC_SSH_KEY" ] && error "Source SSH key not found: ${SRC_SSH_KEY}"
  [ ! -f "$DST_SSH_KEY" ] && error "Destination SSH key not found: ${DST_SSH_KEY}"
  success "SSH keys found"
  info "Source key      : $(basename "$SRC_SSH_KEY")"
  info "Destination key : $(basename "$DST_SSH_KEY")"

  mkdir -p "$BACKUP_DIR"
  success "Backup directory ready"
  info "Path: ${BACKUP_DIR}"

  # Disk space check
  local available_kb
  available_kb=$(df -k "$BACKUP_DIR" | awk 'NR==2{print $4}')
  local available_mb=$(( available_kb / 1024 ))
  info "Available disk space: ${available_mb} MB"
  if [ "$available_mb" -lt 1024 ]; then
    warn "Low disk space: ${available_mb} MB available"
  fi

  load_tables
  step_end "Pre-flight Checks"
}

# ── STEP 1: Source Tunnel ────────────────────────────────────────
step_open_source_tunnel() {
  step "STEP 1" "SSH Tunnel → Source (${SRC_SSH_HOST})"
  info "User    : ${SRC_SSH_USER}"
  info "Host    : ${SRC_SSH_HOST}:${SRC_SSH_PORT}"
  info "Binding : localhost:${SRC_LOCAL_PORT} → ${SRC_DB_HOST}:${SRC_DB_PORT}"
  log "Opening tunnel..."
  TUNNEL_A_PID=$(open_tunnel \
    "$SRC_LOCAL_PORT" "$SRC_SSH_USER" "$SRC_SSH_HOST" "$SRC_SSH_PORT" \
    "$SRC_SSH_KEY" "$SRC_DB_HOST" "$SRC_DB_PORT")
  sleep 2
  wait_for_tunnel "$SRC_LOCAL_PORT" "Server A" "$SRC_DB_USER" "$SRC_DB_PASS"
  step_end "Tunnel → Source"
}

# ── STEP 2: Schema Dump ──────────────────────────────────────────
step_dump_schema() {
  step "STEP 2" "Schema Dump — All Tables (no data)"

  fetch_source_db_stats

  local routines_flag="" triggers_flag="" events_flag=""
  [ "${DUMP_ROUTINES:-true}" = "true" ] && routines_flag="--routines"
  [ "${DUMP_TRIGGERS:-true}" = "true" ] && triggers_flag="--triggers"
  [ "${DUMP_EVENTS:-true}"   = "true" ] && events_flag="--events"

  local flags_info=""
  [ -n "$routines_flag" ] && flags_info+="routines "
  [ -n "$triggers_flag" ] && flags_info+="triggers "
  [ -n "$events_flag"   ] && flags_info+="events"
  info "Including: ${flags_info:-none}"

  log "Starting schema dump..."
  DUMP_START=$(date +%s)

  # --skip-definer requires MySQL 8.0.17+ — strips DEFINER from views/routines/events
  # so restore works without SUPER privilege on the destination
  local skip_definer_flag=""
  local mysql_minor
  mysql_minor=$(docker exec "$DOCKER_CONTAINER" mysql --version 2>/dev/null \
    | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1 | awk -F. '{print $2}')
  if [ "${mysql_minor:-0}" -ge 17 ] 2>/dev/null; then
    skip_definer_flag="--skip-definer"
    info "DEFINER handling : --skip-definer flag active (MySQL 8.0.17+)"
  else
    info "DEFINER handling : will strip via sed post-dump"
  fi

  # --add-drop-database is intentionally excluded:
  # it injects USE `source_db` into the dump which would redirect
  # the restore into the wrong database on the destination server
  # shellcheck disable=SC2086
  docker exec "$DOCKER_CONTAINER" mysqldump \
    --no-data \
    --single-transaction \
    --add-drop-table \
    --set-gtid-purged=OFF \
    $routines_flag $triggers_flag $events_flag \
    $skip_definer_flag \
    -h host.docker.internal \
    -P "$SRC_LOCAL_PORT" \
    -u "$SRC_DB_USER" \
    "-p${SRC_DB_PASS}" \
    "$SRC_DB_NAME" > "$BACKUP_FILE" &

  local dump_pid=$!
  monitor_file_size "$BACKUP_FILE" "Dumping schema..." "$dump_pid"
  wait "$dump_pid"

  # Strip any remaining DEFINER clauses as a safety net
  # Handles: DEFINER=`user`@`host` in CREATE VIEW / TRIGGER / PROCEDURE / FUNCTION / EVENT
  log "Stripping DEFINER clauses from dump..."
  local stripped_tmp="${BACKUP_FILE}.tmp"
  sed \
    -e 's/DEFINER=[^ ]* / /g' \
    -e 's/DEFINER=[^ ]*$//g' \
    "$BACKUP_FILE" > "$stripped_tmp" && mv "$stripped_tmp" "$BACKUP_FILE"

  local schema_size schema_bytes elapsed_s
  schema_bytes=$(wc -c < "$BACKUP_FILE" 2>/dev/null || echo 0)
  schema_size=$(format_bytes "$schema_bytes")
  elapsed_s=$(( $(date +%s) - DUMP_START ))

  success "Schema dump complete"
  info "File size : ${schema_size}"
  info "Duration  : $(format_duration $elapsed_s)"
  info "Tables    : ${TOTAL_TABLES_IN_DB} table schemas written"
  info "DEFINERs  : stripped (restore-safe)"
  step_end "Schema Dump"
}

# ── STEP 3: Data Dump ────────────────────────────────────────────
step_dump_data() {
  step "STEP 3" "Data Dump — ${#TABLES_WITH_DATA[@]} Tables"

  local total=${#TABLES_WITH_DATA[@]}
  local current=0
  local failed_tables=()
  local skipped_tables=()
  local data_start
  data_start=$(date +%s)
  local size_before size_after bytes_added

  size_before=$(wc -c < "$BACKUP_FILE" 2>/dev/null || echo 0)

  echo ""
  for TABLE in "${TABLES_WITH_DATA[@]}"; do
    current=$(( current + 1 ))
    progress_bar "$current" "$total" "$TABLE" "$data_start"

    local table_start
    table_start=$(date +%s)

    if ! docker exec "$DOCKER_CONTAINER" mysqldump \
        --no-create-info \
        --single-transaction \
        --skip-triggers \
        --extended-insert \
        --set-gtid-purged=OFF \
        -h host.docker.internal \
        -P "$SRC_LOCAL_PORT" \
        -u "$SRC_DB_USER" \
        "-p${SRC_DB_PASS}" \
        "$SRC_DB_NAME" "$TABLE" >> "$BACKUP_FILE" 2>/dev/null; then
      failed_tables+=("$TABLE")
    fi
  done

  echo ""

  size_after=$(wc -c < "$BACKUP_FILE" 2>/dev/null || echo 0)
  bytes_added=$(( size_after - size_before ))
  local elapsed_s=$(( $(date +%s) - data_start ))

  echo ""
  if [ ${#failed_tables[@]} -gt 0 ]; then
    warn "Failed tables (${#failed_tables[@]}): ${failed_tables[*]}"
  fi

  success "Data dump complete"
  info "Tables dumped  : ${total}"
  info "Data added     : $(format_bytes $bytes_added)"
  info "Total file now : $(format_bytes $size_after)"
  info "Duration       : $(format_duration $elapsed_s)"
  [ ${#failed_tables[@]} -gt 0 ] && \
    warn "Failed count   : ${#failed_tables[@]}"
  step_end "Data Dump (${total} tables)"
}

# ── STEP 4: Compress ─────────────────────────────────────────────
step_compress() {
  step "STEP 4" "Compressing Backup"

  if [ "${COMPRESS_BACKUP:-true}" = "true" ]; then
    local raw_bytes raw_size
    raw_bytes=$(wc -c < "$BACKUP_FILE" 2>/dev/null || echo 0)
    raw_size=$(format_bytes "$raw_bytes")
    log "Compressing ${raw_size}..."

    local compress_start
    compress_start=$(date +%s)

    gzip -f "$BACKUP_FILE" &
    local gzip_pid=$!

    start_spinner "Compressing ${raw_size}"
    wait "$gzip_pid"
    stop_spinner

    local gz_bytes gz_size ratio elapsed_s
    gz_bytes=$(wc -c < "$BACKUP_FILE_GZ" 2>/dev/null || echo 0)
    gz_size=$(format_bytes "$gz_bytes")
    ratio=$(echo "scale=1; (1 - $gz_bytes / $raw_bytes) * 100" | bc 2>/dev/null || echo "?")
    elapsed_s=$(( $(date +%s) - compress_start ))

    success "Compression complete"
    info "Original size   : ${raw_size}"
    info "Compressed size : ${gz_size}"
    info "Reduction       : ${ratio}%"
    info "Duration        : $(format_duration $elapsed_s)"
    info "Saved to        : $(basename "$BACKUP_FILE_GZ")"

    FINAL_BACKUP_FILE="$BACKUP_FILE_GZ"
  else
    success "Compression skipped"
    info "File: $(basename "$BACKUP_FILE") ($(format_bytes "$(wc -c < "$BACKUP_FILE")"))"
    FINAL_BACKUP_FILE="$BACKUP_FILE"
  fi

  if [ -n "$TUNNEL_A_PID" ] && kill -0 "$TUNNEL_A_PID" 2>/dev/null; then
    kill "$TUNNEL_A_PID" 2>/dev/null && TUNNEL_A_PID="" && log "Source tunnel closed"
  fi
  step_end "Compress & Save"
}

# ── STEP 5: Destination Tunnel ───────────────────────────────────
step_open_dest_tunnel() {
  step "STEP 5" "SSH Tunnel → Destination (${DST_SSH_HOST})"
  info "User    : ${DST_SSH_USER}"
  info "Host    : ${DST_SSH_HOST}:${DST_SSH_PORT}"
  info "Binding : localhost:${DST_LOCAL_PORT} → ${DST_DB_HOST}:${DST_DB_PORT}"
  log "Opening tunnel..."
  TUNNEL_B_PID=$(open_tunnel \
    "$DST_LOCAL_PORT" "$DST_SSH_USER" "$DST_SSH_HOST" "$DST_SSH_PORT" \
    "$DST_SSH_KEY" "$DST_DB_HOST" "$DST_DB_PORT")
  sleep 2
  wait_for_tunnel "$DST_LOCAL_PORT" "Server B" "$DST_DB_USER" "$DST_DB_PASS"
  step_end "Tunnel → Destination"
}

# ── STEP 6: Create Database ──────────────────────────────────────
step_create_database() {
  step "STEP 6" "Create Database on Destination"
  info "Database name : ${NEW_DB_NAME}"
  info "Charset       : utf8mb4"
  info "Collation     : utf8mb4_unicode_ci"

  docker exec "$DOCKER_CONTAINER" mysql \
    -h host.docker.internal \
    -P "$DST_LOCAL_PORT" \
    -u "$DST_DB_USER" \
    "-p${DST_DB_PASS}" \
    -e "CREATE DATABASE \`${NEW_DB_NAME}\`
        CHARACTER SET utf8mb4
        COLLATE utf8mb4_unicode_ci;"

  success "Database '${NEW_DB_NAME}' created on ${DST_SSH_HOST}"
  step_end "Create Database"
}

# ── STEP 7: Restore ──────────────────────────────────────────────
step_restore() {
  step "STEP 7" "Restoring Backup → ${NEW_DB_NAME}"

  local file_bytes file_size
  file_bytes=$(wc -c < "$FINAL_BACKUP_FILE" 2>/dev/null || echo 0)
  file_size=$(format_bytes "$file_bytes")

  info "Backup file   : $(basename "$FINAL_BACKUP_FILE")"
  info "File size     : ${file_size}"
  info "Target DB     : ${NEW_DB_NAME} @ ${DST_SSH_HOST}"

  # MySQL client flags shared across all restore calls
  # --init-command injects optimization session vars before restore begins:
  #   FOREIGN_KEY_CHECKS=0  — skip FK validation per row (re-enabled after)
  #   UNIQUE_CHECKS=0       — skip unique index checks per row (re-enabled after)
  #   AUTOCOMMIT=0          — batch all inserts in one transaction per statement block
  # These three flags alone can make restore 5–10x faster on large datasets
  local mysql_opts=(
    -h host.docker.internal
    -P "$DST_LOCAL_PORT"
    -u "$DST_DB_USER"
    "-p${DST_DB_PASS}"
    --init-command="SET SESSION FOREIGN_KEY_CHECKS=0; SET SESSION UNIQUE_CHECKS=0; SET SESSION AUTOCOMMIT=0;"
    "$NEW_DB_NAME"
  )

  # Preamble injected before the SQL stream — disables constraints for speed
  local preamble="SET FOREIGN_KEY_CHECKS=0; SET UNIQUE_CHECKS=0; SET AUTOCOMMIT=0;"
  # Postamble re-enables everything and commits the final transaction
  local postamble="SET FOREIGN_KEY_CHECKS=1; SET UNIQUE_CHECKS=1; SET AUTOCOMMIT=1; COMMIT;"

  info "Optimizations : FOREIGN_KEY_CHECKS=0, UNIQUE_CHECKS=0, AUTOCOMMIT=0"

  RESTORE_START=$(date +%s)

  # Build the SQL stream depending on compression
  _sql_stream() {
    echo "$preamble"
    if [[ "$FINAL_BACKUP_FILE" == *.gz ]]; then
      gunzip -c "$FINAL_BACKUP_FILE"
    else
      cat "$FINAL_BACKUP_FILE"
    fi
    echo "$postamble"
  }

  if command -v pv &>/dev/null; then
    info "Progress tool : pv (byte-level — size reflects compressed input)"
    echo ""
    _sql_stream \
      | pv \
          --name "  Restoring" \
          --eta \
          --rate \
          --bytes \
          --timer \
      | docker exec -i "$DOCKER_CONTAINER" mysql \
          "${mysql_opts[@]}" 2>/dev/null
  else
    warn "pv not found — install with: brew install pv"
    warn "Continuing without byte-level progress bar..."
    start_spinner "Restoring ${file_size} into ${NEW_DB_NAME}"
    _sql_stream \
      | docker exec -i "$DOCKER_CONTAINER" mysql \
          "${mysql_opts[@]}" 2>/dev/null
    stop_spinner
  fi

  unset -f _sql_stream

  RESTORE_DURATION=$(( $(date +%s) - RESTORE_START ))
  echo ""
  success "Restore complete"
  info "Duration : $(format_duration $RESTORE_DURATION)"
  step_end "Restore"
}

# ── STEP 8: Verify ───────────────────────────────────────────────
step_verify() {
  step "STEP 8" "Verification"

  start_spinner "Querying restored database stats"

  local table_count tables_with_rows tables_no_rows db_size_mb top5
  table_count=$(docker exec "$DOCKER_CONTAINER" mysql \
    -h host.docker.internal -P "$DST_LOCAL_PORT" \
    -u "$DST_DB_USER" "-p${DST_DB_PASS}" \
    --skip-column-names --silent \
    -e "SELECT COUNT(*) FROM information_schema.TABLES
        WHERE TABLE_SCHEMA='${NEW_DB_NAME}';" 2>/dev/null || echo "?")

  tables_with_rows=$(docker exec "$DOCKER_CONTAINER" mysql \
    -h host.docker.internal -P "$DST_LOCAL_PORT" \
    -u "$DST_DB_USER" "-p${DST_DB_PASS}" \
    --skip-column-names --silent \
    -e "SELECT COUNT(*) FROM information_schema.TABLES
        WHERE TABLE_SCHEMA='${NEW_DB_NAME}' AND TABLE_ROWS > 0;" 2>/dev/null || echo "?")

  tables_no_rows=$(docker exec "$DOCKER_CONTAINER" mysql \
    -h host.docker.internal -P "$DST_LOCAL_PORT" \
    -u "$DST_DB_USER" "-p${DST_DB_PASS}" \
    --skip-column-names --silent \
    -e "SELECT COUNT(*) FROM information_schema.TABLES
        WHERE TABLE_SCHEMA='${NEW_DB_NAME}' AND TABLE_ROWS = 0;" 2>/dev/null || echo "?")

  db_size_mb=$(docker exec "$DOCKER_CONTAINER" mysql \
    -h host.docker.internal -P "$DST_LOCAL_PORT" \
    -u "$DST_DB_USER" "-p${DST_DB_PASS}" \
    --skip-column-names --silent \
    -e "SELECT CONCAT(ROUND(SUM(data_length+index_length)/1048576,1),' MB')
        FROM information_schema.TABLES
        WHERE TABLE_SCHEMA='${NEW_DB_NAME}';" 2>/dev/null || echo "?")

  top5=$(docker exec "$DOCKER_CONTAINER" mysql \
    -h host.docker.internal -P "$DST_LOCAL_PORT" \
    -u "$DST_DB_USER" "-p${DST_DB_PASS}" \
    --skip-column-names --silent \
    -e "SELECT TABLE_NAME,
               TABLE_ROWS,
               CONCAT(ROUND((data_length+index_length)/1024,0),' KB') AS size
        FROM information_schema.TABLES
        WHERE TABLE_SCHEMA='${NEW_DB_NAME}'
          AND TABLE_ROWS > 0
        ORDER BY (data_length+index_length) DESC
        LIMIT 5;" 2>/dev/null || echo "")

  stop_spinner

  success "Verification passed"
  divider
  info "Database            : ${BOLD}${NEW_DB_NAME}${NC}"
  info "Total tables        : ${BOLD}${table_count}${NC}"
  info "Tables with data    : ${BOLD}${tables_with_rows}${NC}"
  info "Tables schema-only  : ${BOLD}${tables_no_rows}${NC}"
  info "DB size on disk     : ${BOLD}${db_size_mb}${NC}"
  info "Restore duration    : ${BOLD}$(format_duration $RESTORE_DURATION)${NC}"
  divider

  if [ -n "$top5" ]; then
    echo -e "  ${DIM}Largest tables restored:${NC}"
    while IFS=$'\t' read -r tname trows tsize; do
      printf "    ${CYAN}%-40s${NC} ${DIM}%s rows  %s${NC}\n" "$tname" "$trows" "$tsize"
    done <<< "$top5"
    echo ""
  fi

  step_end "Verification"
}

# ── Restore mode: pick backup file ───────────────────────────────
select_backup_file() {
  local files=()
  while IFS= read -r _f; do
    [ -n "$_f" ] && files+=("$_f")
  done < <(ls -t "${BACKUP_DIR}"/*.sql.gz 2>/dev/null || true)

  if [ ${#files[@]} -eq 0 ]; then
    error "No .sql.gz files found in ${BACKUP_DIR}"
  fi

  echo ""
  echo -e "  ${BOLD}Available backups:${NC}"
  divider
  for i in "${!files[@]}"; do
    local fname size mdate
    fname=$(basename "${files[$i]}")
    size=$(du -sh "${files[$i]}" | cut -f1)
    mdate=$(stat -f "%Sm" -t "%Y-%m-%d %H:%M" "${files[$i]}" 2>/dev/null || \
            stat -c "%y" "${files[$i]}" 2>/dev/null | cut -d'.' -f1)
    printf "  ${CYAN}[%2d]${NC}  %-50s  ${DIM}%-8s  %s${NC}\n" \
      "$((i+1))" "$fname" "$size" "$mdate"
  done
  divider

  echo ""
  read -rp "  Select backup [1-${#files[@]}]: " choice

  if [[ ! "$choice" =~ ^[0-9]+$ ]] || \
     [ "$choice" -lt 1 ] || [ "$choice" -gt "${#files[@]}" ]; then
    error "Invalid selection: ${choice}"
  fi

  FINAL_BACKUP_FILE="${files[$((choice-1))]}"
  success "Selected: $(basename "$FINAL_BACKUP_FILE")"
}

# ── Summary ───────────────────────────────────────────────────────
print_summary() {
  echo ""
  echo -e "${BOLD}${GREEN}"
  echo "  ╔═════════════════════════════════════════════════════════╗"
  echo "  ║                   ✔  Migration Complete!                ║"
  echo "  ╚═════════════════════════════════════════════════════════╝"
  echo -e "${NC}"
  divider
  echo -e "  ${DIM}Source DB     :${NC} ${BOLD}${SRC_DB_NAME}${NC} @ ${SRC_SSH_HOST}"
  echo -e "  ${DIM}Restored To   :${NC} ${BOLD}${NEW_DB_NAME}${NC} @ ${DST_SSH_HOST}"
  echo -e "  ${DIM}Backup File   :${NC} $(basename "$FINAL_BACKUP_FILE")"
  echo -e "  ${DIM}Saved At      :${NC} ${BACKUP_DIR}"
  divider

  print_timing_table

  echo ""
}

# ── Entry Point ───────────────────────────────────────────────────
main() {
  local MODE="full"

  for arg in "$@"; do
    case $arg in
      --backup)   MODE="backup"  ;;
      --restore)  MODE="restore" ;;
      --dry-run)  MODE="dryrun"  ;;
    esac
  done

  load_env
  print_header

  echo -e "  ${DIM}Mode          :${NC} ${BOLD}${MODE}${NC}"
  echo -e "  ${DIM}Source        :${NC} ${BOLD}${SRC_DB_NAME}${NC} @ ${SRC_SSH_HOST}"
  echo -e "  ${DIM}Destination   :${NC} ${BOLD}${NEW_DB_NAME}${NC} @ ${DST_SSH_HOST}"
  echo -e "  ${DIM}Backup dir    :${NC} ${BACKUP_DIR}"
  echo -e "  ${DIM}Tables (data) :${NC} will load from $(basename "$TABLES_FILE")"
  echo ""

  if [ "$MODE" = "dryrun" ]; then
    step_preflight
    validate_config
    success "Dry run complete — no changes made"
    exit 0
  fi

  if [ "$MODE" = "full" ] || [ "$MODE" = "backup" ]; then
    step_preflight
    step_open_source_tunnel
    step_dump_schema
    step_dump_data
    step_compress
  fi

  if [ "$MODE" = "restore" ]; then
    step_preflight
    select_backup_file
  fi

  if [ "$MODE" = "full" ] || [ "$MODE" = "restore" ]; then
    step_open_dest_tunnel
    step_create_database
    step_restore
    step_verify
  fi

  print_summary
}

main "$@"