#!/usr/bin/env bash
# Deploy an updated binary and restart services.
# Run from your dev machine, not the server.
#
# Usage:
#   bash scripts/server-update.sh <instance-name> [gcloud flags]
#   bash scripts/server-update.sh my-vm --zone=us-central1-a
#
# Thin wrapper around deploy.sh.
set -euo pipefail

if [[ $# -lt 1 ]]; then
    echo "Usage: bash scripts/server-update.sh <instance-name> [gcloud flags]" >&2
    exit 1
fi

bash "$(dirname "$0")/deploy.sh" "$@"
