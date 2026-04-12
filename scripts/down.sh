#!/usr/bin/env bash
set -euo pipefail

APP_PORT="${APP_PORT:-3030}"

if command -v fuser >/dev/null 2>&1; then
  echo "Stopping backend on :$APP_PORT (if present)"
  fuser -k "$APP_PORT"/tcp 2>/dev/null || true
else
  pids="$(lsof -ti tcp:"$APP_PORT" 2>/dev/null || true)"
  if [[ -n "$pids" ]]; then
    echo "Stopping backend PIDs: $pids"
    echo "$pids" | xargs -r kill || true
  fi
fi

echo "Stopping ffmpeg publishers (if present)"
pkill -f "^ffmpeg .* -stream_loop" 2>/dev/null || true

docker compose --profile host --profile container stop mediamtx mediamtx-pod pause nginx-rtmp app 2>/dev/null || true
