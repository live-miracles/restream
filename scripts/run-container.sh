#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

APP_URL="${APP_URL:-http://localhost:3030/health}"
APP_RETRIES="${VERIFY_APP_RETRIES:-30}"
MEDIAMTX_API_URL="${MEDIAMTX_API_URL:-http://localhost:9997}"

docker compose --profile container up -d --build --force-recreate --renew-anon-volumes pause mediamtx-pod nginx-rtmp app
"$ROOT_DIR/scripts/wait-mediamtx.sh" "$MEDIAMTX_API_URL"

for i in $(seq 1 "$APP_RETRIES"); do
  if curl -fsS "$APP_URL" >/dev/null 2>&1; then
    echo "Container app is ready: $APP_URL"
    docker compose ps
    exit 0
  fi
  sleep 1
done

echo "Container app did not become ready in time: $APP_URL"
docker compose logs app --tail=60 || true
exit 1
