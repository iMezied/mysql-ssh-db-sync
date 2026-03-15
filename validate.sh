#!/usr/bin/env bash
# ================================================================
#  scripts/validate.sh — Config & environment validation
# ================================================================

validate_config() {
  step "VALIDATE" "Configuration Check"

  local errors=0

  check_required() {
    local key=$1
    local val=$2
    if [ -z "$val" ]; then
      warn "Missing required config: ${key}"
      errors=$((errors + 1))
    else
      success "${key} = ${val}"
    fi
  }

  check_required "DOCKER_CONTAINER"  "${DOCKER_CONTAINER:-}"
  check_required "SRC_SSH_HOST"      "${SRC_SSH_HOST:-}"
  check_required "SRC_SSH_USER"      "${SRC_SSH_USER:-}"
  check_required "SRC_DB_NAME"       "${SRC_DB_NAME:-}"
  check_required "SRC_DB_USER"       "${SRC_DB_USER:-}"
  check_required "SRC_DB_PASS"       "(set)"  # don't print actual password
  check_required "DST_SSH_HOST"      "${DST_SSH_HOST:-}"
  check_required "DST_SSH_USER"      "${DST_SSH_USER:-}"
  check_required "DST_DB_USER"       "${DST_DB_USER:-}"
  check_required "DB_PREFIX"         "${DB_PREFIX:-}"
  check_required "TABLES_FILE"       "${TABLES_FILE:-}"

  # Port conflict check
  if [ "${SRC_LOCAL_PORT:-}" = "${DST_LOCAL_PORT:-}" ]; then
    warn "SRC_LOCAL_PORT and DST_LOCAL_PORT are the same (${SRC_LOCAL_PORT})"
    errors=$((errors + 1))
  fi

  # SSH key file check
  if [ ! -f "${SRC_SSH_KEY:-}" ]; then
    warn "SRC_SSH_KEY not found: ${SRC_SSH_KEY:-}"
    errors=$((errors + 1))
  fi

  if [ ! -f "${DST_SSH_KEY:-}" ]; then
    warn "DST_SSH_KEY not found: ${DST_SSH_KEY:-}"
    errors=$((errors + 1))
  fi

  # Tables file check
  if [ ! -f "${TABLES_FILE:-}" ]; then
    warn "TABLES_FILE not found: ${TABLES_FILE:-}"
    warn "Run: cp config/tables.conf.example config/tables.conf"
    errors=$((errors + 1))
  fi

  # Docker container check
  if ! docker ps --format '{{.Names}}' | grep -q "^${DOCKER_CONTAINER}$" 2>/dev/null; then
    warn "Docker container '${DOCKER_CONTAINER}' is not running"
    errors=$((errors + 1))
  fi

  echo ""
  if [ "$errors" -gt 0 ]; then
    error "Found ${errors} configuration error(s). Fix them in .env before running."
  else
    success "All configuration checks passed"
  fi
}
