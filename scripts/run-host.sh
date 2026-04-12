#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

docker compose --profile host up -d mediamtx nginx-rtmp
"$ROOT_DIR/scripts/wait-mediamtx.sh" "${MEDIAMTX_API_URL:-http://localhost:9997}"
"$ROOT_DIR/scripts/ensure-deps.sh"
npm run dev
