#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_ROOT="${RESTREAM_BUILD_ROOT:-$ROOT/.build/static}"

if [[ ! -f "$BUILD_ROOT/env.sh" ]]; then
    "$ROOT/scripts/setup-static-build.sh"
fi

# shellcheck source=/dev/null
source "$BUILD_ROOT/env.sh"

SERVER="$BUILD_ROOT/prefix/bin/restream-srt-bond-server"
CLIENT="$BUILD_ROOT/prefix/bin/restream-srt-bond-client"

run_mode() {
    local mode="$1"
    local server_log="$BUILD_ROOT/${mode}-server.log"
    local client_log="$BUILD_ROOT/${mode}-client.log"

    local server_pid=""
    local port=""
    for _ in {1..20}; do
        port=$((20000 + RANDOM % 40000))
        : >"$server_log"
        timeout 15s "$SERVER" "$mode" "$port" >"$server_log" 2>&1 &
        server_pid=$!
        trap 'kill "$server_pid" 2>/dev/null || true' RETURN

        for _ in {1..25}; do
            grep -q "^ready port=$port$" "$server_log" && break
            kill -0 "$server_pid" 2>/dev/null || break
            sleep 0.02
        done
        grep -q "^ready port=$port$" "$server_log" && break
        wait "$server_pid" 2>/dev/null || true
        server_pid=""
    done
    if [[ -z "$server_pid" ]]; then
        cat "$server_log" >&2
        return 1
    fi

    if ! timeout 15s "$CLIENT" "$mode" "$port" >"$client_log" 2>&1; then
        cat "$client_log" >&2
        cat "$server_log" >&2
        return 1
    fi
    if ! wait "$server_pid"; then
        cat "$client_log" >&2
        cat "$server_log" >&2
        return 1
    fi
    trap - RETURN

    local expected_failover=0
    local expected_messages=1
    if [[ "$mode" == "backup" ]]; then
        expected_failover=1
        expected_messages=2
    fi
    if ! grep -q "connected_group type=$mode members=2 failover=$expected_failover" "$client_log" ||
        ! grep -q "accepted_group members=2 messages=$expected_messages" "$server_log"; then
        cat "$client_log" >&2
        cat "$server_log" >&2
        return 1
    fi
    echo "SRT $mode bonding: PASS"
}

run_mode broadcast
run_mode backup
