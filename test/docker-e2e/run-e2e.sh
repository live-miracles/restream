#!/usr/bin/env bash
# End-to-end Docker test for srt-bonding-relay integration.
#
# Spins up: MediaMTX, srt-bonding-relay, Restream app, and an ffmpeg sender.
# Verifies: relay status flows through health API, bonding data appears per-pipeline.
#
# Usage:
#   bash test/docker-e2e/run-e2e.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
RELAY_SRC_DIR="/tmp/srt-bonding-relay-src"
APP_URL="http://127.0.0.1:3031"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

pass() { echo -e "${GREEN}✓ $*${NC}"; }
fail() { echo -e "${RED}✗ $*${NC}"; FAILURES=$((FAILURES+1)); }
info() { echo -e "${YELLOW}» $*${NC}"; }

FAILURES=0

cleanup() {
    info "Tearing down containers..."
    docker compose -f "$SCRIPT_DIR/docker-compose.yml" down --remove-orphans -t 5 2>/dev/null || true
}
trap cleanup EXIT

# ── 0. Prepare relay source for Docker build ────────────────────────────────

info "Preparing srt-bonding-relay source..."
if [[ -d "$RELAY_SRC_DIR/.git" ]]; then
    git -C "$RELAY_SRC_DIR" pull --ff-only 2>/dev/null || true
else
    rm -rf "$RELAY_SRC_DIR"
    git clone --depth 1 https://github.com/live-miracles/srt-bonding-relay.git "$RELAY_SRC_DIR"
fi

# ── 1. Build and start all services ─────────────────────────────────────────

info "Building and starting containers (this may take a few minutes on first run)..."
docker compose -f "$SCRIPT_DIR/docker-compose.yml" build --quiet
docker compose -f "$SCRIPT_DIR/docker-compose.yml" up -d

# ── 2. Wait for services to be ready ────────────────────────────────────────

info "Waiting for MediaMTX + relay + app (shared network namespace)..."
for i in $(seq 1 45); do
    if curl -fsS "$APP_URL/healthz" >/dev/null 2>&1; then
        break
    fi
    sleep 1
done

info "Waiting for Restream app..."
for i in $(seq 1 45); do
    if curl -fsS "$APP_URL/healthz" >/dev/null 2>&1; then
        break
    fi
    sleep 1
done

if ! curl -fsS "$APP_URL/healthz" >/dev/null 2>&1; then
    fail "App did not start within 45s"
    docker compose -f "$SCRIPT_DIR/docker-compose.yml" logs
    exit 1
fi
pass "All services are up"

# ── 3. Login and create a pipeline ──────────────────────────────────────────

COOKIE_JAR="$(mktemp)"
info "Logging in..."
curl -s -c "$COOKIE_JAR" -X POST "$APP_URL/api/auth/login" \
    -H 'Content-Type: application/json' -d '{"password":"admin"}' >/dev/null

info "Creating test pipeline..."
PIPELINE_JSON=$(curl -s -b "$COOKIE_JAR" -X POST "$APP_URL/pipelines" \
    -H 'Content-Type: application/json' -d '{"name":"E2E Bonding Test"}')
PIPELINE_ID=$(echo "$PIPELINE_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['pipeline']['id'])")
STREAM_KEY=$(echo "$PIPELINE_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['pipeline']['streamKey'])")
pass "Pipeline created: $PIPELINE_ID (key: ${STREAM_KEY:0:10}...)"

# ── 4. Check health API: relay running, bonding empty ───────────────────────

info "Waiting for relay poller to sync..."
sleep 8

HEALTH=$(curl -s -b "$COOKIE_JAR" "$APP_URL/health")
RELAY_STATUS=$(echo "$HEALTH" | python3 -c "import json,sys; print(json.load(sys.stdin)['srtRelay']['status'])")
BONDING_ACTIVE=$(echo "$HEALTH" | python3 -c "
import json,sys
d = json.load(sys.stdin)
p = d.get('pipelines',{}).get('$PIPELINE_ID',{})
print(p.get('srtBonding',{}).get('inputActive', False))
")

if [[ "$RELAY_STATUS" == "running" ]]; then
    pass "Relay status = running"
else
    fail "Relay status = $RELAY_STATUS (expected running)"
fi

if [[ "$BONDING_ACTIVE" == "False" ]]; then
    pass "Bonding inputActive = False (no stream yet)"
else
    fail "Bonding inputActive should be False before sending"
fi

# ── 5. Send SRT via relay ───────────────────────────────────────────────────

STREAMID="publish:live/$STREAM_KEY"
info "Sending SRT test stream via relay (streamid=$STREAMID)..."

docker compose -f "$SCRIPT_DIR/docker-compose.yml" exec -T -d sender \
    ffmpeg -re -f lavfi -i "testsrc2=size=320x240:rate=25" \
           -f lavfi -i "sine=frequency=440:sample_rate=48000" \
           -c:v libx264 -preset ultrafast -tune zerolatency -b:v 500k \
           -c:a aac -b:a 64k -ac 2 \
           -f mpegts "srt://172.30.0.10:10081?streamid=$STREAMID&transtype=live&latency=200"

info "Waiting for stream to establish (10s)..."
sleep 10

# ── 6. Check health API: bonding active ─────────────────────────────────────

HEALTH2=$(curl -s -b "$COOKIE_JAR" "$APP_URL/health")
E2E_RESULT=0
python3 - "$HEALTH2" "$PIPELINE_ID" <<'PYEOF' || E2E_RESULT=$?
import json, sys

health = json.loads(sys.argv[1])
pid = sys.argv[2]

relay = health.get("srtRelay", {})
pipeline = health.get("pipelines", {}).get(pid, {})
bonding = pipeline.get("srtBonding", {})
inp = pipeline.get("input", {})

results = []

# Relay still running
if relay.get("status") == "running":
    results.append(("PASS", "Relay still running during stream"))
else:
    results.append(("FAIL", f"Relay status = {relay.get('status')} during stream"))

# Bonding shows activity
if bonding.get("inputActive"):
    results.append(("PASS", f"Bonding inputActive = True"))
else:
    results.append(("FAIL", f"Bonding inputActive = {bonding.get('inputActive')}"))

# Forwarded packets
fwd = bonding.get("forwardedPackets", 0)
if fwd > 0:
    results.append(("PASS", f"Forwarded {fwd} packets"))
else:
    results.append(("FAIL", f"forwardedPackets = {fwd}"))

# Forwarded bytes
fwd_bytes = bonding.get("forwardedBytes", 0)
if fwd_bytes > 0:
    results.append(("PASS", f"Forwarded {fwd_bytes} bytes"))
else:
    results.append(("FAIL", f"forwardedBytes = {fwd_bytes}"))

# Legs (single non-bonded SRT has no per-leg entries; only bonded groups report legs)
legs = bonding.get("legs", [])
if len(legs) > 0:
    results.append(("PASS", f"Leg count = {len(legs)}"))
    for leg in legs:
        results.append(("PASS", f"  Leg {leg['ip']}:{leg['port']} state={leg['state']} rtt={leg.get('rttMs')}ms"))
else:
    results.append(("PASS", f"No legs (expected for non-bonded single SRT connection)"))

# Input RTT
rtt = bonding.get("inputRttMs")
if rtt is not None and rtt > 0:
    results.append(("PASS", f"Input RTT = {rtt:.1f} ms"))
else:
    results.append(("FAIL", f"Input RTT = {rtt}"))

# MediaMTX accepted the stream
if inp.get("status") == "on":
    results.append(("PASS", f"MediaMTX input status = on (stream forwarded successfully)"))
else:
    results.append(("FAIL", f"MediaMTX input status = {inp.get('status')} (stream not reaching MediaMTX)"))

# Output connected
if bonding.get("outputConnected"):
    results.append(("PASS", f"Output connected to MediaMTX"))
else:
    results.append(("FAIL", f"outputConnected = {bonding.get('outputConnected')}"))

# Health snapshot attributes the MediaMTX publisher to the relay
if bonding.get("acceptedByMediamtx") and not bonding.get("publishConflict"):
    results.append(("PASS", "MediaMTX publisher attributed to relay"))
else:
    results.append(("FAIL", f"acceptedByMediamtx={bonding.get('acceptedByMediamtx')} publishConflict={bonding.get('publishConflict')}"))

for status, msg in results:
    if status == "PASS":
        print(f"\033[0;32m✓ {msg}\033[0m")
    else:
        print(f"\033[0;31m✗ {msg}\033[0m")

fails = sum(1 for s, _ in results if s == "FAIL")
sys.exit(fails)
PYEOF
FAILURES=$((FAILURES + E2E_RESULT))

# ── 7. Stop sender and verify bonding goes inactive ─────────────────────────

info "Stopping sender..."
docker compose -f "$SCRIPT_DIR/docker-compose.yml" exec -T sender pkill -f ffmpeg 2>/dev/null || true
sleep 8

HEALTH3=$(curl -s -b "$COOKIE_JAR" "$APP_URL/health")
BONDING_ACTIVE3=$(echo "$HEALTH3" | python3 -c "
import json,sys
d = json.load(sys.stdin)
p = d.get('pipelines',{}).get('$PIPELINE_ID',{})
print(p.get('srtBonding',{}).get('inputActive', 'missing'))
")
if [[ "$BONDING_ACTIVE3" == "False" ]]; then
    pass "Bonding inputActive = False after sender stopped"
else
    fail "Bonding inputActive = $BONDING_ACTIVE3 after sender stopped (expected False)"
fi

# ── 8. Rejected stream IDs release their relay session ─────────────────────

UNKNOWN_STREAMID="publish:live/unknown_e2e_probe"
info "Sending an unknown stream ID and checking that its relay session is released..."
docker compose -f "$SCRIPT_DIR/docker-compose.yml" exec -T -d sender \
    ffmpeg -re -f lavfi -i "testsrc2=size=160x120:rate=10" \
           -f lavfi -i "sine=frequency=880:sample_rate=48000" \
           -c:v libx264 -preset ultrafast -tune zerolatency -b:v 200k \
           -c:a aac -b:a 48k -ac 2 -t 15 \
           -f mpegts "srt://172.30.0.10:10081?streamid=$UNKNOWN_STREAMID&transtype=live&latency=200"
sleep 4

RELAY_RAW=$(docker compose -f "$SCRIPT_DIR/docker-compose.yml" exec -T app \
    node -e "fetch('http://127.0.0.1:8081/status').then(r => r.text()).then(console.log)")
RELAY_LOGS=$(docker compose -f "$SCRIPT_DIR/docker-compose.yml" logs relay)

if echo "$RELAY_LOGS" | grep -Fq "Closing input after downstream rejection streamid=$UNKNOWN_STREAMID"; then
    pass "Unknown stream ID was classified as a terminal downstream rejection"
else
    fail "Relay did not log terminal rejection for the unknown stream ID"
fi

if echo "$RELAY_RAW" | python3 -c '
import json, sys
d = json.load(sys.stdin)
target = "publish:live/unknown_e2e_probe"
active = d.get("activeStreamIds", [])
states = [s.get("streamId") for s in d.get("streamStates", [])]
raise SystemExit(0 if target not in active and target not in states else 1)
'; then
    pass "Unknown stream ID left no active relay session"
else
    fail "Unknown stream ID remained in relay status after rejection"
fi

docker compose -f "$SCRIPT_DIR/docker-compose.yml" exec -T sender pkill -f ffmpeg 2>/dev/null || true

# ── Summary ─────────────────────────────────────────────────────────────────

echo
if [[ "$FAILURES" -eq 0 ]]; then
    echo -e "${GREEN}All e2e tests passed!${NC}"
else
    echo -e "${RED}$FAILURES test(s) failed.${NC}"
fi

rm -f "$COOKIE_JAR"
exit "$FAILURES"
