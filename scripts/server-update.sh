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
DATA_DIR=/var/lib/restream
CONF_DIR=/etc/restream
LOG_DIR=/var/log/restream
SERVICE_USER=restream
PROMETHEUS_CONFIG_DIR=/etc/prometheus
GRAFANA_PROVISIONING_DIR=/etc/grafana/provisioning
GRAFANA_DASHBOARD_DIR=/var/lib/grafana/dashboards

SRT_RELAY_RELEASE_TAG="${SRT_RELAY_RELEASE_TAG:-v2.0.1}"
SRT_RELAY_FILENAME="srt-bonding-relay-linux-x86_64.tar.gz"
SRT_RELAY_URL="${SRT_RELAY_URL:-https://github.com/live-miracles/srt-bonding-relay/releases/download/${SRT_RELAY_RELEASE_TAG}/${SRT_RELAY_FILENAME}}"
SRT_RELAY_SHA256="${SRT_RELAY_SHA256:-2e6e32eb99f9524d33c2021c15b3c70c67f32000a848fc4ce93378ca84637bd4}"

WORK="$(mktemp -d)"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

echo "=== Ensure runtime packages ==="
apt-get update -q
apt-get install -y -q ca-certificates curl iproute2 tar

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
install -d -m 0755 -o "$SERVICE_USER" -g "$SERVICE_USER" "$DATA_DIR" "$LOG_DIR" "$CONF_DIR"
cp "$APP_DIR/mediamtx.yml" "$CONF_DIR/mediamtx.yml"
cp "$APP_DIR/srt-bonding-relay.json" "$CONF_DIR/srt-bonding-relay.json"
chown "$SERVICE_USER:$SERVICE_USER" "$CONF_DIR/mediamtx.yml" "$CONF_DIR/srt-bonding-relay.json"
echo "Copied mediamtx.yml and srt-bonding-relay.json to $CONF_DIR/"

echo
echo "=== Install srt-bonding-relay $SRT_RELAY_RELEASE_TAG ==="
SRT_VERSION_MARKER=/usr/local/bin/.srt-bonding-relay-version
if [[ -x /usr/local/bin/srt-bonding-relay && -f "$SRT_VERSION_MARKER" && "$(cat "$SRT_VERSION_MARKER")" == "$SRT_RELAY_RELEASE_TAG" ]]; then
    echo "srt-bonding-relay $SRT_RELAY_RELEASE_TAG already installed."
else
    SRT_EXTRACT_DIR="$WORK/srt-bonding-relay"
    mkdir -p "$SRT_EXTRACT_DIR"
    curl -fsSL "$SRT_RELAY_URL" -o "$WORK/$SRT_RELAY_FILENAME"
    actual="$(sha256sum "$WORK/$SRT_RELAY_FILENAME" | awk '{print $1}')"
    if [[ -z "$SRT_RELAY_SHA256" || "$SRT_RELAY_SHA256" != "$actual" ]]; then
        echo "ERROR: srt-bonding-relay checksum mismatch" >&2
        exit 1
    fi
    tar -xzf "$WORK/$SRT_RELAY_FILENAME" -C "$SRT_EXTRACT_DIR"
    SRT_BIN="$(find "$SRT_EXTRACT_DIR" -type f -name srt-bonding-relay -perm -111 | head -1)"
    if [[ -z "$SRT_BIN" ]]; then
        echo "ERROR: could not find srt-bonding-relay binary in $SRT_RELAY_FILENAME" >&2
        exit 1
    fi
    if [[ -d "$SRT_EXTRACT_DIR/lib" ]]; then
        install -d -m 0755 /usr/local/lib/restream-srt
        install -m 0755 "$SRT_EXTRACT_DIR"/lib/* /usr/local/lib/restream-srt/
        echo /usr/local/lib/restream-srt > /etc/ld.so.conf.d/restream-srt.conf
        ldconfig
    fi
    install -m 0755 "$SRT_BIN" /usr/local/bin/srt-bonding-relay
    echo "$SRT_RELAY_RELEASE_TAG" > "$SRT_VERSION_MARKER"
    echo "Installed: /usr/local/bin/srt-bonding-relay"
fi

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
chown "$SERVICE_USER:$SERVICE_USER" "$LOG_DIR"
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

echo
echo "=== Configure srt-bonding-relay service ==="
cat > /etc/systemd/system/srt-bonding-relay.service <<EOF
[Unit]
Description=SRT Bonding Relay
After=network-online.target mediamtx.service
Wants=network-online.target
Requires=mediamtx.service

[Service]
Type=simple
User=$SERVICE_USER
Group=$SERVICE_USER
WorkingDirectory=$DATA_DIR
ExecStart=/usr/local/bin/srt-bonding-relay $CONF_DIR/srt-bonding-relay.json
Restart=always
RestartSec=2
LimitNOFILE=1048576
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=full
ReadWritePaths=$DATA_DIR $LOG_DIR $CONF_DIR

[Install]
WantedBy=multi-user.target
EOF
install -d -m 0755 /etc/systemd/system/restream.service.d
cat > /etc/systemd/system/restream.service.d/srt-bonding-relay.conf <<'EOF'
[Unit]
After=srt-bonding-relay.service
Wants=srt-bonding-relay.service
EOF
systemctl daemon-reload
systemctl enable srt-bonding-relay.service

echo
echo "=== Restart services ==="
systemctl restart prometheus.service
systemctl restart grafana-server.service
systemctl restart mediamtx.service
systemctl restart srt-bonding-relay.service
systemctl restart restream.service

echo
echo "=== Status ==="
systemctl status prometheus.service --no-pager -l || true
systemctl status grafana-server.service --no-pager -l || true
systemctl status mediamtx.service --no-pager -l || true
systemctl status srt-bonding-relay.service --no-pager -l || true
systemctl status restream.service --no-pager -l || true
echo
echo "Logs: journalctl -u restream.service -n 50 --no-pager"
