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
APP_PIDFILE=".app.pid"

mkdir -p log

start_mediamtx() {
  if nc -z localhost 8888 2>/dev/null; then
    echo "MediaMTX is already running."
  else
    echo "Starting MediaMTX..."
    nohup setsid "$MEDIAMTX_BINARY" "$MEDIAMTX_CONFIG" > "$MEDIAMTX_LOGFILE" 2>&1 &
    echo $! > "$MEDIAMTX_PIDFILE"
    sleep 2
    if nc -z localhost 8888 2>/dev/null; then
      echo "MediaMTX started (PID=$(cat "$MEDIAMTX_PIDFILE"))."
    else
      echo "Failed to start MediaMTX. Check $MEDIAMTX_LOGFILE for details."
    fi
  fi
}

start_app() {
  if nc -z localhost 3030 2>/dev/null; then
    pid=$(fuser 3030/tcp 2>/dev/null | tr -s ' ' | cut -d' ' -f2)
    echo "App is already running (PID=$pid)."
  else
    echo "Starting app..."
    if [[ "${DEV:-0}" == "1" ]]; then
      nohup setsid npm run dev > log/app.log 2>&1 &
    else
      nohup setsid node src/index.js > log/app.log 2>&1 &
    fi
    echo $! > "$APP_PIDFILE"
    sleep 2
    if nc -z localhost 3030 2>/dev/null; then
      echo "App started (PID=$(cat "$APP_PIDFILE"))."
    else
      echo "Failed to start app. Check log/app.log for details."
    fi
  fi
}

start_mediamtx
start_app

if [[ "${DEV:-0}" == "1" ]]; then
  echo "Starting nginx-rtmp for RTMP testing..."
  docker compose up -d nginx-rtmp
fi
