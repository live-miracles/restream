#!/usr/bin/env bash
# Pull the latest code, rebuild, deploy configs, and restart services.
#
# Usage (run as root on the VM):
#   sudo bash /opt/restream/scripts/server-update.sh
set -euo pipefail

if [[ "$(id -u)" -ne 0 ]]; then
    echo "ERROR: run as root (sudo bash scripts/server-update.sh)" >&2
    exit 1
fi

APP_DIR=/opt/restream
CONF_DIR=/etc/restream

echo "=== Pull latest code ==="
cd "$APP_DIR"
sudo -u restream git pull

echo
echo "=== Rebuild ==="
sudo -u restream npm ci
sudo -u restream npm run ts-build
sudo -u restream npm prune --omit=dev

echo
echo "=== Deploy configs ==="
cp "$APP_DIR/mediamtx.yml" "$CONF_DIR/mediamtx.yml"
chown restream:restream "$CONF_DIR/mediamtx.yml"
echo "Copied mediamtx.yml to $CONF_DIR/"

echo
echo "=== Restart services ==="
systemctl restart mediamtx.service
systemctl restart restream.service

echo
echo "=== Status ==="
systemctl status mediamtx.service --no-pager -l || true
systemctl status restream.service --no-pager -l || true
echo
echo "Logs: journalctl -u restream.service -n 50 --no-pager"
