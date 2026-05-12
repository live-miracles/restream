#!/usr/bin/env bash
# Build a Linux binary locally and deploy it to a GCP VM via IAP tunnel.
# The server needs no Go, no Node, no npm — only FFmpeg and MediaMTX.
#
# Usage:
#   bash scripts/deploy.sh <instance-name> [gcloud flags]
#   bash scripts/deploy.sh my-vm --zone=us-central1-a --project=my-project
#   bash scripts/deploy.sh my-vm --restart-only [gcloud flags]   # skip build
#
# Prerequisites (first time on the server):
#   gcloud compute ssh <instance-name> --tunnel-through-iap -- sudo bash /opt/restream/scripts/server-setup.sh
set -euo pipefail

if [[ $# -lt 1 ]]; then
    echo "Usage: bash scripts/deploy.sh <instance-name> [--restart-only] [gcloud flags]" >&2
    exit 1
fi

INSTANCE="$1"
shift

RESTART_ONLY=""
GCLOUD_FLAGS=()
for arg in "$@"; do
    if [[ "$arg" == "--restart-only" ]]; then
        RESTART_ONLY=1
    else
        GCLOUD_FLAGS+=("$arg")
    fi
done

APP_DIR=/opt/restream

if [[ -z "$RESTART_ONLY" ]]; then
    echo "=== Building frontend assets ==="
    npm ci
    npm run ts-build
    npm run css
    npm run vendor-hls

    echo
    echo "=== Cross-compiling for Linux amd64 ==="
    mkdir -p dist
    GOOS=linux GOARCH=amd64 go build -o dist/restream ./cmd/server
    echo "Built: dist/restream ($(du -sh dist/restream | cut -f1))"
fi

echo
echo "=== Copying binary to $INSTANCE via IAP ==="
gcloud compute scp dist/restream "$INSTANCE:$APP_DIR/dist/restream" \
    --tunnel-through-iap "${GCLOUD_FLAGS[@]+"${GCLOUD_FLAGS[@]}"}"

echo
echo "=== Restarting restream.service ==="
gcloud compute ssh "$INSTANCE" --tunnel-through-iap \
    "${GCLOUD_FLAGS[@]+"${GCLOUD_FLAGS[@]}"}" \
    --command="sudo systemctl restart restream.service && systemctl is-active restream.service"

echo
echo "Done. Logs: gcloud compute ssh $INSTANCE --tunnel-through-iap -- journalctl -u restream.service -f"
