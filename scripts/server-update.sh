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
LOG_DIR=/var/log/restream

echo "=== Pull latest code ==="
cd "$APP_DIR"
git pull

echo
echo "=== Rebuild ==="
npm ci
npm run build
npm prune --omit=dev

echo
echo "=== Deploy configs ==="
cp "$APP_DIR/mediamtx.yml" "$CONF_DIR/mediamtx.yml"
chown restream:restream "$CONF_DIR/mediamtx.yml"
echo "Copied mediamtx.yml to $CONF_DIR/"

echo
echo "=== Configure MediaMTX diagnostics logging ==="
mkdir -p "$LOG_DIR" /etc/systemd/system/mediamtx.service.d
chown restream:restream "$LOG_DIR"
cat > /etc/systemd/system/mediamtx.service.d/restream-logging.conf <<'EOF'
[Service]
Environment=MTX_LOGDESTINATIONS=stdout,file
Environment=MTX_LOGFILE=/var/log/restream/mediamtx.log
EOF
cat > /etc/logrotate.d/restream-mediamtx <<'EOF'
/var/log/restream/mediamtx.log {
    daily
    copytruncate
    rotate 7
    compress
    delaycompress
    missingok
    notifempty
}
EOF
systemctl daemon-reload

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
