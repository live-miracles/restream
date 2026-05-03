#!/usr/bin/env bash
set -euo pipefail

# This script starts the necessary services for host-mode development.
# It starts MediaMTX and the Node app as background processes.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

MEDIAMTX_DIR="bin/mediamtx"
MEDIAMTX_BINARY="$MEDIAMTX_DIR/mediamtx"
MEDIAMTX_CONFIG="infra/mediamtx.yml"
MEDIAMTX_LOGFILE="log/mediamtx.log"
MEDIAMTX_PIDFILE=".mediamtx.pid"
MEDIAMTX_PORT="${MEDIAMTX_PORT:-9997}"
APP_LOGFILE="log/app.log"
APP_PIDFILE=".app.pid"
APP_PORT="${APP_PORT:-3030}"
APP_HEALTH_URL="${APP_HEALTH_URL:-http://127.0.0.1:${APP_PORT}/healthz}"
STARTUP_TIMEOUT_SEC="${STARTUP_TIMEOUT_SEC:-20}"

mediamtx_started_here=0

mkdir -p log

have_cmd() {
  command -v "$1" >/dev/null 2>&1
}

list_port_pids() {
  local port="$1"
  local token raw

  if have_cmd fuser; then
    raw="$(fuser "${port}/tcp" 2>/dev/null || true)"
    for token in $raw; do
      if [[ "$token" =~ ^[0-9]+$ ]]; then
        printf '%s\n' "$token"
      fi
    done
    return 0
  fi

  if have_cmd lsof; then
    lsof -ti tcp:"$port" 2>/dev/null || true
    return 0
  fi

  return 1
}

pid_command() {
  local pid="$1"

  if [[ -r "/proc/$pid/cmdline" ]]; then
    tr '\0' ' ' < "/proc/$pid/cmdline"
  elif have_cmd ps; then
    ps -p "$pid" -o args= 2>/dev/null || true
  fi
}

is_mediamtx_command() {
  local command_line="$1"
  [[ "$command_line" == *"mediamtx"* ]]
}

is_app_command() {
  local command_line="$1"
  [[ "$command_line" == *"src/index.js"* || "$command_line" == *"npm run dev"* || "$command_line" == *"nodemon"* ]]
}

pid_matches_role() {
  local pid="$1"
  local role="$2"
  local command_line

  command_line="$(pid_command "$pid")"
  [[ -n "$command_line" ]] || return 1

  case "$role" in
    mediamtx)
      is_mediamtx_command "$command_line"
      ;;
    app)
      is_app_command "$command_line"
      ;;
    *)
      return 1
      ;;
  esac
}

find_matching_port_pid() {
  local port="$1"
  local role="$2"
  local pid

  while read -r pid; do
    [[ -n "$pid" ]] || continue
    if pid_matches_role "$pid" "$role"; then
      printf '%s\n' "$pid"
      return 0
    fi
  done < <(list_port_pids "$port")

  return 1
}

port_has_any_pid() {
  local port="$1"
  local pid

  while read -r pid; do
    [[ -n "$pid" ]] && return 0
  done < <(list_port_pids "$port")

  return 1
}

describe_port_conflict() {
  local port="$1"
  local role_name="$2"
  local pid command_line found=0

  echo "Port $port is already in use by a non-$role_name process:" >&2
  while read -r pid; do
    [[ -n "$pid" ]] || continue
    found=1
    command_line="$(pid_command "$pid")"
    echo "  PID=$pid ${command_line:-<unknown>}" >&2
  done < <(list_port_pids "$port")

  if [[ "$found" -eq 0 ]]; then
    echo '  Unable to determine the owning PID.' >&2
  fi
}

wait_for_matching_port() {
  local port="$1"
  local role="$2"
  local timeout_sec="$3"
  local started_pid="${4:-}"
  local deadline=$((SECONDS + timeout_sec))
  local pid

  while (( SECONDS < deadline )); do
    pid="$(find_matching_port_pid "$port" "$role" 2>/dev/null || true)"
    if [[ -n "$pid" ]]; then
      printf '%s\n' "$pid"
      return 0
    fi

    if [[ -n "$started_pid" ]] && ! kill -0 "$started_pid" 2>/dev/null; then
      break
    fi

    sleep 1
  done

  return 1
}

wait_for_app_ready() {
  local timeout_sec="$1"
  local started_pid="${2:-}"
  local deadline=$((SECONDS + timeout_sec))

  if ! have_cmd curl; then
    wait_for_matching_port "$APP_PORT" app "$timeout_sec" "$started_pid" >/dev/null
    return $?
  fi

  while (( SECONDS < deadline )); do
    if curl -fsS --max-time 2 "$APP_HEALTH_URL" >/dev/null 2>&1; then
      return 0
    fi

    if [[ -n "$started_pid" ]] && ! kill -0 "$started_pid" 2>/dev/null; then
      break
    fi

    sleep 1
  done

  return 1
}

show_recent_log() {
  local logfile="$1"

  if [[ -f "$logfile" ]]; then
    echo "Recent log output from $logfile:" >&2
    tail -n 20 "$logfile" >&2 || true
  fi
}

cleanup_mediamtx_if_started_here() {
  local pid

  if [[ "$mediamtx_started_here" != '1' || ! -f "$MEDIAMTX_PIDFILE" ]]; then
    return 0
  fi

  pid="$(tr -d '[:space:]' < "$MEDIAMTX_PIDFILE")"
  if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null; then
    echo 'Stopping MediaMTX after app startup failure...' >&2
    kill "$pid" 2>/dev/null || true
  fi
  rm -f "$MEDIAMTX_PIDFILE"
}

start_mediamtx() {
  local existing_pid started_pid

  existing_pid="$(find_matching_port_pid "$MEDIAMTX_PORT" mediamtx || true)"
  if [[ -n "$existing_pid" ]]; then
    echo "MediaMTX is already running (PID=$existing_pid)."
    return 0
  fi

  if port_has_any_pid "$MEDIAMTX_PORT"; then
    describe_port_conflict "$MEDIAMTX_PORT" 'MediaMTX'
    return 1
  fi

  if [[ ! -x "$MEDIAMTX_BINARY" ]]; then
    echo "MediaMTX binary not found at $MEDIAMTX_BINARY. Run 'make deps' first." >&2
    return 1
  fi

  echo 'Starting MediaMTX...'
  nohup setsid "$MEDIAMTX_BINARY" "$MEDIAMTX_CONFIG" > "$MEDIAMTX_LOGFILE" 2>&1 &
  started_pid=$!
  echo "$started_pid" > "$MEDIAMTX_PIDFILE"

  existing_pid="$(wait_for_matching_port "$MEDIAMTX_PORT" mediamtx "$STARTUP_TIMEOUT_SEC" "$started_pid" || true)"
  if [[ -n "$existing_pid" ]]; then
    mediamtx_started_here=1
    echo "MediaMTX started (PID=$existing_pid)."
    return 0
  fi

  rm -f "$MEDIAMTX_PIDFILE"
  echo "Failed to start MediaMTX. Check $MEDIAMTX_LOGFILE for details." >&2
  show_recent_log "$MEDIAMTX_LOGFILE"
  return 1
}

start_app() {
  local existing_pid started_pid

  existing_pid="$(find_matching_port_pid "$APP_PORT" app || true)"
  if [[ -n "$existing_pid" ]]; then
    if wait_for_app_ready 5 "$existing_pid"; then
      echo "App is already running (PID=$existing_pid)."
      return 0
    fi

    echo "App process is already bound to :$APP_PORT but not healthy." >&2
    show_recent_log "$APP_LOGFILE"
    cleanup_mediamtx_if_started_here
    return 1
  fi

  if port_has_any_pid "$APP_PORT"; then
    describe_port_conflict "$APP_PORT" 'app'
    cleanup_mediamtx_if_started_here
    return 1
  fi

  echo 'Starting app...'
  if [[ "${DEV:-0}" == '1' ]]; then
    nohup setsid npm run dev > "$APP_LOGFILE" 2>&1 &
  else
    nohup setsid node src/index.js > "$APP_LOGFILE" 2>&1 &
  fi
  started_pid=$!
  echo "$started_pid" > "$APP_PIDFILE"

  if wait_for_app_ready "$STARTUP_TIMEOUT_SEC" "$started_pid"; then
    echo "App started (PID=$started_pid)."
    return 0
  fi

  rm -f "$APP_PIDFILE"
  echo "Failed to start app. Check $APP_LOGFILE for details." >&2
  show_recent_log "$APP_LOGFILE"
  cleanup_mediamtx_if_started_here
  return 1
}

start_mediamtx
start_app
