#!/usr/bin/env bash
# ================================================================
#  scripts/ui.sh — Terminal UI helpers
# ================================================================

# ── Colors ───────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
MAGENTA='\033[0;35m'
WHITE='\033[0;37m'
BOLD='\033[1m'
DIM='\033[2m'
NC='\033[0m'

# ── Log Levels ───────────────────────────────────────────────────
log()     { echo -e "${CYAN}  ◆ [$(date '+%H:%M:%S')]${NC} $1"; }
success() { echo -e "${GREEN}  ✔ [$(date '+%H:%M:%S')]${NC} ${GREEN}$1${NC}"; }
warn()    { echo -e "${YELLOW}  ⚠ [$(date '+%H:%M:%S')]${NC} ${YELLOW}$1${NC}"; }
error()   { echo -e "${RED}  ✘ [$(date '+%H:%M:%S')]${NC} ${RED}$1${NC}"; exit 1; }
info()    { echo -e "${WHITE}  ℹ [$(date '+%H:%M:%S')]${NC} ${DIM}$1${NC}"; }

# ── Step Timing ──────────────────────────────────────────────────
STEP_START_TIME=0
STEP_TIMINGS_STR=""   # "name|secs,name|secs,..." — avoids array compat issues

step_start() {
  STEP_START_TIME=$(date +%s)
}

step_end() {
  local name=$1
  local elapsed=$(( $(date +%s) - STEP_START_TIME ))
  STEP_TIMINGS_STR="${STEP_TIMINGS_STR}${name}|${elapsed},"
}

# ── Step Header ──────────────────────────────────────────────────
step() {
  local id=$1
  local title=$2
  echo ""
  echo -e "${BOLD}${BLUE}  ┌─────────────────────────────────────────────────────┐${NC}"
  printf "${BOLD}${BLUE}  │  %-3s  %-47s│${NC}\n" "$id" "$title"
  echo -e "${BOLD}${BLUE}  └─────────────────────────────────────────────────────┘${NC}"
  step_start
}

# ── Divider ──────────────────────────────────────────────────────
divider() {
  echo -e "  ${DIM}───────────────────────────────────────────────────────${NC}"
}

# ── Format Helpers ───────────────────────────────────────────────
format_duration() {
  local secs=$1
  if   [ "$secs" -lt 60 ];   then echo "${secs}s"
  elif [ "$secs" -lt 3600 ]; then printf "%dm %02ds" $((secs/60)) $((secs%60))
  else printf "%dh %02dm %02ds" $((secs/3600)) $(( (secs%3600)/60 )) $((secs%60))
  fi
}

format_bytes() {
  local bytes=$1
  if   [ "$bytes" -lt 1024 ];       then echo "${bytes} B"
  elif [ "$bytes" -lt 1048576 ];    then printf "%.1f KB" "$(echo "scale=1; $bytes/1024" | bc 2>/dev/null || echo 0)"
  elif [ "$bytes" -lt 1073741824 ]; then printf "%.1f MB" "$(echo "scale=1; $bytes/1048576" | bc 2>/dev/null || echo 0)"
  else printf "%.2f GB" "$(echo "scale=2; $bytes/1073741824" | bc 2>/dev/null || echo 0)"
  fi
}

# ── Progress Bar ─────────────────────────────────────────────────
# Usage: progress_bar current total label [start_epoch]
progress_bar() {
  local current=$1
  local total=$2
  local label=$3
  local start_epoch=${4:-0}

  local percent=$(( current * 100 / total ))
  local filled=$(( percent / 4 ))
  local bar="" elapsed_str="" eta_str=""

  for ((i = 0; i < 25; i++)); do
    if   [ "$i" -lt "$filled" ];       then bar+="█"
    elif [ "$i" -eq "$filled" ];       then bar+="▓"
    else bar+="░"
    fi
  done

  if [ "$start_epoch" -gt 0 ] && [ "$current" -gt 0 ]; then
    local now elapsed_secs
    now=$(date +%s)
    elapsed_secs=$(( now - start_epoch ))
    elapsed_str=" ${elapsed_secs}s"
    if [ "$elapsed_secs" -gt 0 ] && [ "$current" -lt "$total" ]; then
      local remaining
      remaining=$(echo "scale=0; $elapsed_secs * ($total - $current) / $current" | bc 2>/dev/null || echo "0")
      if [ "$remaining" -gt 0 ]; then
        eta_str=" ETA:$(format_duration $remaining)"
      fi
    fi
  fi

  printf "\r  \033[2m[\033[0m\033[32m%s\033[0m\033[2m]\033[0m \033[1m%3d%%\033[0m \033[2m(%d/%d)\033[0m — %-30s\033[2m%s%s\033[0m" \
    "$bar" "$percent" "$current" "$total" "$label" "$elapsed_str" "$eta_str"
}

# ── Spinner ──────────────────────────────────────────────────────
SPINNER_PID=""

start_spinner() {
  local label=$1
  (
    local i=0
    local frames='⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏'
    while true; do
      local len=${#frames}
      local pos=$(( i % len ))
      local frame="${frames:$pos:1}"
      printf "\r  \033[36m%s\033[0m \033[2m%s...\033[0m" "$frame" "$label"
      i=$(( i + 1 ))
      sleep 0.12
    done
  ) &
  SPINNER_PID=$!
  disown "$SPINNER_PID" 2>/dev/null || true
}

stop_spinner() {
  local pid=${1:-$SPINNER_PID}
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null || true
  fi
  printf "\r\033[K"
  SPINNER_PID=""
}

# ── Live File Size Monitor ────────────────────────────────────────
# Watches a file grow while a bg process runs, shows live written size
# Usage: monitor_file_size "filepath" "label" bg_pid
monitor_file_size() {
  local filepath=$1
  local label=$2
  local bg_pid=$3
  local start
  start=$(date +%s)
  local i=0
  local frames='⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏'

  while kill -0 "$bg_pid" 2>/dev/null; do
    local len=${#frames}
    local pos=$(( i % len ))
    local frame="${frames:$pos:1}"
    local size="–"
    local elapsed_s=$(( $(date +%s) - start ))
    if [ -f "$filepath" ]; then
      local bytes
      bytes=$(wc -c < "$filepath" 2>/dev/null || echo 0)
      size=$(format_bytes "$bytes")
    fi
    printf "\r  \033[36m%s\033[0m %-40s \033[2m%s written  %s elapsed\033[0m" \
      "$frame" "$label" "$size" "$(format_duration $elapsed_s)"
    i=$(( i + 1 ))
    sleep 0.4
  done
  printf "\r\033[K"
}

# ── App Header ───────────────────────────────────────────────────
print_header() {
  clear
  echo ""
  echo -e "${BOLD}${BLUE}"
  echo "  ╔═════════════════════════════════════════════════════════╗"
  echo "  ║         MySQL Cross-Server Backup & Restore             ║"
  echo "  ║         Germany  ──────────────────────►  Malaysia      ║"
  echo "  ╚═════════════════════════════════════════════════════════╝"
  echo -e "${NC}"
}

# ── Timing Summary Table ──────────────────────────────────────────
print_timing_table() {
  [ -z "$STEP_TIMINGS_STR" ] && return
  echo ""
  echo -e "  ${BOLD}${DIM}Step Timings:${NC}"
  divider
  local total_secs=0
  local IFS_BAK="$IFS"
  IFS=','
  for entry in $STEP_TIMINGS_STR; do
    IFS='|' read -r name secs <<< "$entry"
    [ -z "$name" ] && continue
    total_secs=$(( total_secs + secs ))
    printf "  ${DIM}%-42s${NC} ${CYAN}%s${NC}\n" "$name" "$(format_duration $secs)"
  done
  IFS="$IFS_BAK"
  divider
  printf "  ${BOLD}%-42s${NC} ${GREEN}%s${NC}\n" "Total" "$(format_duration $total_secs)"
}