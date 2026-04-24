#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

MEDIAMTX_PIDFILE="$ROOT_DIR/.mediamtx.pid"
APP_PIDFILE="$ROOT_DIR/.app.pid"

kill_pid_file() {
  local pidfile="$1"
  local name="$2"
  local port="$3"
  if [[ -f "$pidfile" ]]; then
    local pid="$(cat "$pidfile")"
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      echo "Stopping $name (PID=$pid)"
      kill "$pid" 2>/dev/null || true
      sleep 1
      if kill -0 "$pid" 2>/dev/null; then
        kill -9 "$pid" 2>/dev/null || true
      fi
    fi
    rm -f "$pidfile"
  fi
  if nc -z localhost "$port" 2>/dev/null; then
    echo "Port $port still in use, force-killing..."
    fuser -k "$port/tcp" 2>/dev/null || true
    sleep 1
    if nc -z localhost "$port" 2>/dev/null; then
      echo "WARNING: port $port still in use"
    fi
  fi
}

docker compose --profile "*" down --timeout 1 -v --remove-orphans 2>/dev/null || true

kill_pid_file "$MEDIAMTX_PIDFILE" "MediaMTX" "8888"
kill_pid_file "$APP_PIDFILE" "App" "3030"

echo "Cleaning up database files"
rm -f data/data.db data/data.db-*

echo "Stopping ffmpeg publishers (if present)"
pkill -f "^ffmpeg .* -stream_loop" 2>/dev/null || true

