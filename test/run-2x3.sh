#!/usr/bin/env bash
#
# 2x3 integration test for the Rust restream binary.
# Requires: running restream binary + ffmpeg on PATH.
#
# Usage:
#   ./test/run-2x3.sh                # run against localhost:3030
#   API_URL=http://host:3030 ./test/run-2x3.sh
#   KEEP_RUNNING=1 ./test/run-2x3.sh # leave resources after run
#
set -euo pipefail

API_URL="${API_URL:-http://localhost:3030}"
MANIFEST="${MANIFEST:-test/artifacts/session-2x3-manifest.json}"
LOG_DIR="${LOG_DIR:-test/artifacts/logs}"
INPUT_FILE="${INPUT_FILE:-media/colorbar-timer.mp4}"
TIMEOUT_SEC="${TIMEOUT_SEC:-120}"
POLL_SEC="${POLL_SEC:-2}"
KEEP_RUNNING="${KEEP_RUNNING:-0}"

OWNED_PIDS=()
CREATED_PIPELINES=()
CREATED_OUTPUTS=()

cleanup() {
    if [[ "$KEEP_RUNNING" == "1" ]]; then
        echo "== KEEP_RUNNING=1: leaving resources in place =="
        return
    fi
    echo "== Cleanup =="
    for pid in "${OWNED_PIDS[@]}"; do
        kill "$pid" 2>/dev/null && wait "$pid" 2>/dev/null || true
    done
    for entry in "${CREATED_OUTPUTS[@]}"; do
        local pipe_id="${entry%%:*}"
        local out_id="${entry#*:}"
        curl -sf -X DELETE "$API_URL/pipelines/$pipe_id/outputs/$out_id" \
            -b "$COOKIE_JAR" >/dev/null 2>&1 || true
    done
    for pipe_id in "${CREATED_PIPELINES[@]}"; do
        curl -sf -X DELETE "$API_URL/pipelines/$pipe_id" \
            -b "$COOKIE_JAR" >/dev/null 2>&1 || true
    done
    rm -f "$COOKIE_JAR"
}
trap cleanup EXIT

COOKIE_JAR=$(mktemp)

api() {
    local method="$1" path="$2"
    shift 2
    curl -sf -X "$method" "$API_URL$path" \
        -H "Content-Type: application/json" \
        -b "$COOKIE_JAR" -c "$COOKIE_JAR" \
        "$@"
}

fail() { echo "FAIL: $*" >&2; exit 1; }

echo "== Prerequisites =="
command -v ffmpeg >/dev/null 2>&1 || fail "ffmpeg not found"
command -v curl >/dev/null 2>&1 || fail "curl not found"
command -v jq >/dev/null 2>&1 || fail "jq not found"
[[ -f "$INPUT_FILE" ]] || fail "Input file not found: $INPUT_FILE"
[[ -f "$MANIFEST" ]] || fail "Manifest not found: $MANIFEST"

echo "== Verify app is reachable =="
for i in $(seq 1 30); do
    if curl -sf "$API_URL/healthz" >/dev/null 2>&1; then break; fi
    if [[ $i -eq 30 ]]; then fail "App not reachable at $API_URL/healthz"; fi
    sleep 1
done
echo "App reachable at $API_URL"

echo "== Login =="
api POST /api/auth/login -d '{"password":"admin"}' >/dev/null
echo "Logged in"

echo "== Step 1: Ensure manifest resources =="
PIPE_COUNT=$(jq '.pipelines | length' "$MANIFEST")
for pi in $(seq 0 $((PIPE_COUNT - 1))); do
    PIPE_NAME=$(jq -r ".pipelines[$pi].name" "$MANIFEST")
    PIPE_KEY=$(jq -r ".pipelines[$pi].streamKey" "$MANIFEST")

    EXISTING_ID=$(api GET /config | jq -r \
        --arg name "$PIPE_NAME" --arg key "$PIPE_KEY" \
        '.pipelines[] | select(.name == $name and .streamKey == $key) | .id // empty')

    if [[ -z "$EXISTING_ID" ]]; then
        PIPE_ID=$(api POST /pipelines \
            -d "{\"name\":\"$PIPE_NAME\",\"streamKey\":\"$PIPE_KEY\"}" | jq -r '.pipeline.id')
        echo "Created pipeline $PIPE_NAME: $PIPE_ID"
        CREATED_PIPELINES+=("$PIPE_ID")
    else
        PIPE_ID="$EXISTING_ID"
        echo "Pipeline exists $PIPE_NAME: $PIPE_ID"
    fi

    OUT_COUNT=$(jq ".pipelines[$pi].outputs | length" "$MANIFEST")
    for oi in $(seq 0 $((OUT_COUNT - 1))); do
        OUT_NAME=$(jq -r ".pipelines[$pi].outputs[$oi].name" "$MANIFEST")
        OUT_URL=$(jq -r ".pipelines[$pi].outputs[$oi].url" "$MANIFEST")
        OUT_ENC=$(jq -r ".pipelines[$pi].outputs[$oi].encoding" "$MANIFEST")

        EXISTING_OUT=$(api GET /config | jq -r \
            --arg pid "$PIPE_ID" --arg name "$OUT_NAME" \
            '.outputs[] | select(.pipelineId == $pid and .name == $name) | .id // empty')

        if [[ -z "$EXISTING_OUT" ]]; then
            OUT_ID=$(api POST "/pipelines/$PIPE_ID/outputs" \
                -d "{\"name\":\"$OUT_NAME\",\"url\":\"$OUT_URL\",\"encoding\":\"$OUT_ENC\"}" | jq -r '.output.id')
            echo "  Created output $OUT_NAME: $OUT_ID"
            CREATED_OUTPUTS+=("$PIPE_ID:$OUT_ID")
        else
            OUT_ID="$EXISTING_OUT"
            echo "  Output exists $OUT_NAME: $OUT_ID"
        fi
    done
done

echo "== Step 2: Start input publishers =="
mkdir -p "$LOG_DIR"
PROTO_LIST=(rtmp srt)
for pi in $(seq 0 $((PIPE_COUNT - 1))); do
    PIPE_KEY=$(jq -r ".pipelines[$pi].streamKey" "$MANIFEST")
    PROTO="${PROTO_LIST[$((pi % ${#PROTO_LIST[@]}))]}"

    if [[ "$PROTO" == "rtmp" ]]; then
        TARGET="rtmp://localhost:1935/live/$PIPE_KEY"
        ffmpeg -nostdin -re -stream_loop -1 -i "$INPUT_FILE" \
            -map 0:v -map 0:a:0 -c copy -f flv "$TARGET" \
            >"$LOG_DIR/input-$((pi+1))-$PROTO.log" 2>&1 &
    else
        TARGET="srt://localhost:10080?streamid=publish:live/$PIPE_KEY"
        ffmpeg -nostdin -re -stream_loop -1 -i "$INPUT_FILE" \
            -map 0 -c copy -f mpegts "$TARGET" \
            >"$LOG_DIR/input-$((pi+1))-$PROTO.log" 2>&1 &
    fi
    OWNED_PIDS+=($!)
    echo "[$((pi+1))/$PIPE_COUNT] protocol=$PROTO streamKey=$PIPE_KEY pid=$!"
done

echo "== Step 3: Start all outputs =="
CONFIG_JSON=$(api GET /config)
OUTPUT_IDS=$(echo "$CONFIG_JSON" | jq -r '.outputs[] | "\(.pipelineId):\(.id)"')
for entry in $OUTPUT_IDS; do
    PIPE_ID="${entry%%:*}"
    OUT_ID="${entry#*:}"
    RESULT=$(api POST "/pipelines/$PIPE_ID/outputs/$OUT_ID/start" || true)
    echo "Started $entry"
done

echo "== Step 4: Wait for all inputs/outputs active =="
DEADLINE=$((SECONDS + TIMEOUT_SEC))
while [[ $SECONDS -lt $DEADLINE ]]; do
    HEALTH=$(api GET /health 2>/dev/null || echo '{}')

    INPUTS_ON=0
    OUTPUTS_ON=0
    TOTAL_OUTPUTS=$(echo "$CONFIG_JSON" | jq '.outputs | length')

    for pi in $(seq 0 $((PIPE_COUNT - 1))); do
        PIPE_NAME=$(jq -r ".pipelines[$pi].name" "$MANIFEST")
        PIPE_KEY=$(jq -r ".pipelines[$pi].streamKey" "$MANIFEST")
        PIPE_ID=$(echo "$CONFIG_JSON" | jq -r \
            --arg name "$PIPE_NAME" --arg key "$PIPE_KEY" \
            '.pipelines[] | select(.name == $name and .streamKey == $key) | .id // empty')
        [[ -z "$PIPE_ID" ]] && continue

        INPUT_STATUS=$(echo "$HEALTH" | jq -r ".pipelines.\"$PIPE_ID\".input.status // empty")
        [[ "$INPUT_STATUS" == "on" ]] && INPUTS_ON=$((INPUTS_ON + 1))
    done

    for entry in $OUTPUT_IDS; do
        PIPE_ID="${entry%%:*}"
        OUT_ID="${entry#*:}"
        OUT_STATUS=$(echo "$HEALTH" | jq -r ".pipelines.\"$PIPE_ID\".outputs.\"$OUT_ID\".status // empty")
        [[ "$OUT_STATUS" == "on" ]] && OUTPUTS_ON=$((OUTPUTS_ON + 1))
    done

    echo "Status: inputs on=$INPUTS_ON/$PIPE_COUNT | outputs on=$OUTPUTS_ON/$TOTAL_OUTPUTS"

    if [[ $INPUTS_ON -eq $PIPE_COUNT && $OUTPUTS_ON -eq $TOTAL_OUTPUTS ]]; then
        echo "All streams green"
        break
    fi

    sleep "$POLL_SEC"
done

echo "== Step 5: Stop all outputs =="
for entry in $OUTPUT_IDS; do
    PIPE_ID="${entry%%:*}"
    OUT_ID="${entry#*:}"
    api POST "/pipelines/$PIPE_ID/outputs/$OUT_ID/stop" >/dev/null
    echo "Stopped $entry"
done

echo "== Step 6: Verify outputs stopped =="
STOP_DEADLINE=$((SECONDS + 60))
while [[ $SECONDS -lt $STOP_DEADLINE ]]; do
    ALL_STOPPED=true
    CONFIG_NOW=$(api GET /config)
    for entry in $OUTPUT_IDS; do
        PIPE_ID="${entry%%:*}"
        OUT_ID="${entry#*:}"
        JOB_STATUS=$(echo "$CONFIG_NOW" | jq -r \
            --arg pid "$PIPE_ID" --arg oid "$OUT_ID" \
            '.jobs[] | select(.pipelineId == $pid and .outputId == $oid) | .status // empty')
        if [[ -n "$JOB_STATUS" && "$JOB_STATUS" != "stopped" ]]; then
            ALL_STOPPED=false
        fi
    done
    if $ALL_STOPPED; then break; fi
    sleep 1
done

echo "== 2x3 integration test complete =="
