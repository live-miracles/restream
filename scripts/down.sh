#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

MEDIAMTX_PIDFILE="$ROOT_DIR/.mediamtx.pid"
APP_PIDFILE="$ROOT_DIR/.app.pid"
MEDIAMTX_PORT="${MEDIAMTX_PORT:-9997}"
APP_PORT="${APP_PORT:-3030}"

handled_pids='|'

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

mark_handled_pid() {
  handled_pids+="$1|"
}

pid_already_handled() {
  [[ "$handled_pids" == *"|$1|"* ]]
}

get_pgid() {
  local pid="$1"

  if have_cmd ps; then
    ps -o pgid= -p "$pid" 2>/dev/null | tr -d '[:space:]'
  fi
}

wait_for_pid_exit() {
  local pid="$1"
  local timeout_sec="${2:-5}"
  local deadline=$((SECONDS + timeout_sec))

  while kill -0 "$pid" 2>/dev/null; do
    if (( SECONDS >= deadline )); then
      return 1
    fi
    sleep 1
  done

  return 0
}

terminate_pid() {
  local pid="$1"
  local name="$2"
  local pgid

  if pid_already_handled "$pid"; then
    return 0
  fi

  mark_handled_pid "$pid"
  pgid="$(get_pgid "$pid")"

  if [[ -n "$pgid" ]]; then
    echo "Stopping $name (PID=$pid, PGID=$pgid)"
    kill -TERM -- "-$pgid" 2>/dev/null || kill "$pid" 2>/dev/null || true
  else
    echo "Stopping $name (PID=$pid)"
    kill "$pid" 2>/dev/null || true
  fi

  if ! wait_for_pid_exit "$pid" 5; then
    if [[ -n "$pgid" ]]; then
      kill -KILL -- "-$pgid" 2>/dev/null || kill -9 "$pid" 2>/dev/null || true
    else
      kill -9 "$pid" 2>/dev/null || true
    fi
    wait_for_pid_exit "$pid" 2 || true
  fi
}

kill_pid_file() {
  local pidfile="$1"
  local name="$2"
  local role="$3"
  local pid command_line

  if [[ -f "$pidfile" ]]; then
    pid="$(tr -d '[:space:]' < "$pidfile")"
    if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null; then
      if pid_matches_role "$pid" "$role"; then
        terminate_pid "$pid" "$name"
      else
        command_line="$(pid_command "$pid")"
        echo "Skipping stale $name pidfile: PID=$pid now belongs to ${command_line:-<unknown>}" >&2
      fi
    fi
    rm -f "$pidfile"
  fi
}

cleanup_port() {
  local port="$1"
  local name="$2"
  local role="$3"
  local pid command_line

  while read -r pid; do
    [[ -n "$pid" ]] || continue
    if pid_already_handled "$pid"; then
      continue
    fi

    if pid_matches_role "$pid" "$role"; then
      terminate_pid "$pid" "$name"
    else
      command_line="$(pid_command "$pid")"
      echo "Port $port is held by an unrelated process; leaving it alone: PID=$pid ${command_line:-<unknown>}" >&2
    fi
  done < <(list_port_pids "$port")

  if nc -z localhost "$port" 2>/dev/null; then
    echo "WARNING: port $port is still in use after cleanup." >&2
  fi
}

docker compose down --timeout 1 -v --remove-orphans 2>/dev/null || true

kill_pid_file "$MEDIAMTX_PIDFILE" "MediaMTX" mediamtx
kill_pid_file "$APP_PIDFILE" "App" app

cleanup_port "$MEDIAMTX_PORT" "MediaMTX" mediamtx
cleanup_port "$APP_PORT" "App" app

echo "Cleaning up database files"
rm -f data/data.db data/data.db-*

echo "Stopping ffmpeg publishers (if present)"
pkill -f "^ffmpeg .* -stream_loop" 2>/dev/null || true
