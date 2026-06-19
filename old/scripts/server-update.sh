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
PROMETHEUS_CONFIG_DIR=/etc/prometheus
GRAFANA_PROVISIONING_DIR=/etc/grafana/provisioning
GRAFANA_DASHBOARD_DIR=/var/lib/grafana/dashboards

echo "=== Ensure runtime packages ==="
apt-get update -q
apt-get install -y -q iproute2

echo
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
echo "=== Refresh Prometheus and Grafana manifests ==="
install -d -m 0755 "$PROMETHEUS_CONFIG_DIR" \
    "$GRAFANA_PROVISIONING_DIR/datasources" \
    "$GRAFANA_PROVISIONING_DIR/dashboards" \
    "$GRAFANA_DASHBOARD_DIR" \
    /etc/systemd/system/grafana-server.service.d
install -m 0644 "$APP_DIR/monitoring/prometheus.yml" \
    "$PROMETHEUS_CONFIG_DIR/prometheus.yml"
install -m 0644 "$APP_DIR/monitoring/grafana/provisioning/datasources/prometheus.yml" \
    "$GRAFANA_PROVISIONING_DIR/datasources/prometheus.yml"
install -m 0644 "$APP_DIR/monitoring/grafana/provisioning/dashboards/restream.yml" \
    "$GRAFANA_PROVISIONING_DIR/dashboards/restream.yml"
install -m 0644 "$APP_DIR/monitoring/grafana/dashboards/"*.json \
    "$GRAFANA_DASHBOARD_DIR/"
cat > /etc/default/prometheus <<'EOF'
ARGS="--config.file=/etc/prometheus/prometheus.yml --storage.tsdb.path=/var/lib/prometheus --web.console.libraries=/usr/share/prometheus/console_libraries --web.console.templates=/usr/share/prometheus/consoles --web.listen-address=127.0.0.1:9090"
EOF
cat > /etc/systemd/system/grafana-server.service.d/restream.conf <<'EOF'
[Service]
Environment=GF_USERS_ALLOW_SIGN_UP=false
Environment=GF_SERVER_HTTP_ADDR=127.0.0.1
Environment=GF_SERVER_ROOT_URL=%(protocol)s://%(domain)s/grafana/
Environment=GF_SERVER_SERVE_FROM_SUB_PATH=true
EOF
chown prometheus:prometheus "$PROMETHEUS_CONFIG_DIR/prometheus.yml" || true
chown -R grafana:grafana "$GRAFANA_PROVISIONING_DIR" "$GRAFANA_DASHBOARD_DIR" || true

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
systemctl restart prometheus.service
systemctl restart grafana-server.service
systemctl restart mediamtx.service
systemctl restart restream.service

echo
echo "=== Status ==="
systemctl status prometheus.service --no-pager -l || true
systemctl status grafana-server.service --no-pager -l || true
systemctl status mediamtx.service --no-pager -l || true
systemctl status restream.service --no-pager -l || true
echo
echo "Logs: journalctl -u restream.service -n 50 --no-pager"
