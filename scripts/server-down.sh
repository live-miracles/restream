#!/usr/bin/env bash
# Stop all Restream services (restream + mediamtx) without disabling them.
# Services will restart automatically on next boot.
# To permanently disable, use: sudo systemctl disable restream.service mediamtx.service
#
# Usage (run as root on the VM):
#   sudo bash /opt/restream/scripts/server-down.sh
set -euo pipefail

if [[ "$(id -u)" -ne 0 ]]; then
    echo "ERROR: run as root (sudo bash scripts/server-down.sh)" >&2
    exit 1
fi

echo "=== Stopping services ==="
systemctl stop restream.service
echo "  restream.service stopped"
systemctl stop mediamtx.service
echo "  mediamtx.service stopped"

echo
echo "=== Status ==="
systemctl status restream.service --no-pager -l || true
systemctl status mediamtx.service --no-pager -l || true
