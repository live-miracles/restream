#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

API_URL="${API_URL:-http://localhost:3030}"
MANIFEST_PATH="${MANIFEST_PATH:-test/artifacts/session-4x3-last.json}"
LOG_DIR="${LOG_DIR:-test/artifacts/logs}"
APP_LOG_PATH="${APP_LOG_PATH:-test/artifacts/logs/app-under-test.log}"

# CLEAN_START=1 (default): tear down stale state and relaunch media services +
# app before running.  Set to 0 to reuse an already-running stack.
CLEAN_START="${CLEAN_START:-1}"
# KEEP_RUNNING=1: skip teardown on exit so the app and input publishers stay
# alive after the run completes (useful for inspecting the dashboard/logs).
KEEP_RUNNING="${KEEP_RUNNING:-0}"

app_pid=""

cleanup_inputs() {
    shopt -s nullglob
    for pid_file in "$LOG_DIR"/input-*.pid "$LOG_DIR"/input-tee.pid; do
        pid="$(cat "$pid_file" 2>/dev/null || true)"
        if [[ -n "${pid:-}" ]] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
        fi
        rm -f "$pid_file"
    done
}

cleanup_app() {
    if [[ -n "$app_pid" ]] && kill -0 "$app_pid" 2>/dev/null; then
        kill "$app_pid" 2>/dev/null || true
    fi
}

cleanup_on_exit() {
    if [[ "$KEEP_RUNNING" == "1" ]]; then
        echo "== KEEP_RUNNING=1: leaving input publishers and app running =="
        return
    fi
    cleanup_inputs
    cleanup_app
}

trap cleanup_on_exit EXIT

command -v curl >/dev/null || { echo "curl is required"; exit 1; }
command -v jq >/dev/null || { echo "jq is required"; exit 1; }

if [[ "$CLEAN_START" == "1" ]]; then
    echo "== Clean start: tear down stale processes and state =="
    pkill -f "ffmpeg -re -stream_loop" 2>/dev/null || true
    make down || true
    rm -f data.db
    docker compose up -d mediamtx nginx-rtmp
    bash scripts/wait-mediamtx.sh "${MEDIAMTX_API_URL:-http://localhost:9997}" "${VERIFY_MEDIAMTX_RETRIES:-15}"

    mkdir -p "$LOG_DIR"
    : > "$APP_LOG_PATH"
    echo "== Clean start: launch backend and wait for health =="
    node src/index.js >"$APP_LOG_PATH" 2>&1 &
    app_pid="$!"

    ready=0
    for _ in $(seq 1 30); do
        if curl -sf "$API_URL/health" >/dev/null; then
            ready=1
            break
        fi
        sleep 1
    done

    if [[ "$ready" -ne 1 ]]; then
        echo "API did not become healthy at $API_URL/health"
        echo "Recent app log:"
        tail -n 120 "$APP_LOG_PATH" || true
        exit 1
    fi
fi

if ! curl -sf "$API_URL/health" >/dev/null; then
    echo "API is not reachable at $API_URL. Start app first (for example: make run-host)."
    exit 1
fi

echo "== Step 1: Configure 4x3 copy test setup =="
bash test/artifacts/setup-4x3-copy.sh

echo "== Step 2: Start mixed-protocol input publishers (RTMP/RTSP/SRT) =="
bash test/artifacts/start-inputs-from-manifest.sh "$MANIFEST_PATH"

echo "== Step 3: Start outputs from manifest =="
bash test/artifacts/start-outputs-from-manifest.sh "$MANIFEST_PATH"

echo "== Step 4: Wait for all inputs/outputs active =="
bash test/artifacts/wait-all-active.sh

echo "== Step 5: Capture health snapshot =="
bash test/artifacts/health-snapshot.sh

echo "== 4x3 run complete =="
