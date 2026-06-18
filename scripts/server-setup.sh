#!/usr/bin/env bash
# One-shot setup for a Restream GCP Linux VM.
# Installs Node.js 22, FFmpeg 7.1, MediaMTX 1.17.1, Prometheus, and Grafana,
# builds the app, and registers systemd services that start on boot.
#
# Usage (run as root on the VM):
#   sudo git clone https://github.com/live-miracles/restream /opt/restream
#   sudo bash /opt/restream/scripts/server-setup.sh
#
# To use a fork:
#   REPO_URL=https://github.com/your-fork/restream sudo bash scripts/server-setup.sh
#
# Idempotent: safe to re-run. Already-installed components are skipped.
# If the repo was cloned as root, re-running fixes ownership before building.
set -euo pipefail

if [[ "$(id -u)" -ne 0 ]]; then
    echo "ERROR: run as root (sudo bash scripts/server-setup.sh)" >&2
    exit 1
fi

REPO_URL="${REPO_URL:-https://github.com/live-miracles/restream}"
APP_DIR=/opt/restream
DATA_DIR=/var/lib/restream
LOG_DIR=/var/log/restream
CONF_DIR=/etc/restream
SERVICE_USER=restream
PROMETHEUS_CONFIG_DIR=/etc/prometheus
GRAFANA_PROVISIONING_DIR=/etc/grafana/provisioning
GRAFANA_DASHBOARD_DIR=/var/lib/grafana/dashboards

MEDIAMTX_VERSION=1.17.1
FFMPEG_VERSION=7.1

WORK="$(mktemp -d)"
trap "rm -rf $WORK" EXIT

step() { echo; echo "=== $* ==="; }

# ── 1. System packages ──────────────────────────────────────────────────────

step "1/10 System packages"
apt-get update -q
apt-get install -y -q curl tar xz-utils git ca-certificates gnupg iproute2

# ── 2. Node.js 22 ───────────────────────────────────────────────────────────

step "2/10 Node.js 22"
if node --version 2>/dev/null | grep -q '^v22'; then
    echo "Node.js 22 already installed: $(node --version)"
else
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash -
    apt-get install -y nodejs
    echo "Installed: $(node --version)"
fi

# ── 3. FFmpeg 7.1.4 (BtbN static build) ────────────────────────────────────

step "3/10 FFmpeg $FFMPEG_VERSION"
# Ubuntu 24.04 ships FFmpeg 6.1.x in apt. On 6.1.x, transient loss of an HLS
# upload sink can trigger a retry-path bug: source-copy outputs usually fail
# cleanly, but transcoded HLS outputs can terminate with SIGSEGV before
# Restream retries them. FFmpeg 7.1+ includes the upstream fix.
FFMPEG_FILENAME="ffmpeg-n${FFMPEG_VERSION}-latest-linux64-gpl-${FFMPEG_VERSION}.tar.xz"
FFMPEG_URL="https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/${FFMPEG_FILENAME}"

if /usr/local/bin/ffmpeg -version 2>/dev/null | grep -q "ffmpeg version n${FFMPEG_VERSION}"; then
    echo "FFmpeg $FFMPEG_VERSION already installed."
else
    echo "Downloading $FFMPEG_FILENAME..."
    curl -fsSL "$FFMPEG_URL" -o "$WORK/$FFMPEG_FILENAME"
    tar -xJf "$WORK/$FFMPEG_FILENAME" -C "$WORK"
    FFMPEG_DIR="$(ls -d "$WORK/ffmpeg-"*/  2>/dev/null | head -1)"
    install -m 755 "${FFMPEG_DIR}bin/ffmpeg" /usr/local/bin/ffmpeg
    install -m 755 "${FFMPEG_DIR}bin/ffprobe" /usr/local/bin/ffprobe
    echo "Installed: $(/usr/local/bin/ffmpeg -version 2>&1 | head -1)"
fi

# ── 4. MediaMTX ─────────────────────────────────────────────────────────────

step "4/10 MediaMTX $MEDIAMTX_VERSION"
if /usr/local/bin/mediamtx --version 2>/dev/null | grep -q "$MEDIAMTX_VERSION"; then
    echo "MediaMTX $MEDIAMTX_VERSION already installed."
else
    MEDIAMTX_FILENAME="mediamtx_v${MEDIAMTX_VERSION}_linux_amd64.tar.gz"
    MEDIAMTX_URL="https://github.com/bluenviron/mediamtx/releases/download/v${MEDIAMTX_VERSION}/${MEDIAMTX_FILENAME}"
    CHECKSUMS_URL="https://github.com/bluenviron/mediamtx/releases/download/v${MEDIAMTX_VERSION}/checksums.sha256"
    echo "Downloading MediaMTX v${MEDIAMTX_VERSION}..."
    curl -fsSL "$MEDIAMTX_URL" -o "$WORK/$MEDIAMTX_FILENAME"
    curl -fsSL "$CHECKSUMS_URL" -o "$WORK/checksums.sha256"
    expected="$(grep "$MEDIAMTX_FILENAME" "$WORK/checksums.sha256" | awk '{print $1}')"
    actual="$(sha256sum "$WORK/$MEDIAMTX_FILENAME" | awk '{print $1}')"
    if [[ "$expected" != "$actual" ]]; then
        echo "ERROR: MediaMTX checksum mismatch" >&2; exit 1
    fi
    tar -xzf "$WORK/$MEDIAMTX_FILENAME" -C "$WORK"
    install -m 755 "$WORK/mediamtx" /usr/local/bin/mediamtx
    echo "Installed: $(/usr/local/bin/mediamtx --version 2>&1 | head -1)"
fi

# ── 5. Prometheus and Grafana packages ──────────────────────────────────────

step "5/10 Prometheus and Grafana"
apt-get install -y -q prometheus

if dpkg -s grafana >/dev/null 2>&1; then
    echo "Grafana already installed."
else
    install -d -m 0755 /etc/apt/keyrings
    if [[ ! -f /etc/apt/keyrings/grafana.gpg ]]; then
        curl -fsSL https://apt.grafana.com/gpg.key | gpg --dearmor -o /etc/apt/keyrings/grafana.gpg
        chmod a+r /etc/apt/keyrings/grafana.gpg
    fi
    cat > /etc/apt/sources.list.d/grafana.list <<'EOF'
deb [signed-by=/etc/apt/keyrings/grafana.gpg] https://apt.grafana.com stable main
EOF
    apt-get update -q
    apt-get install -y -q grafana
fi

# ── 6. Service user and directories ─────────────────────────────────────────

step "6/10 Service user and directories"
# restream is a no-login system user; the app and both services run as this
# user so that neither has root privileges at runtime.
if ! id "$SERVICE_USER" &>/dev/null; then
    useradd --system --home "$APP_DIR" --shell /usr/sbin/nologin "$SERVICE_USER"
    echo "Created user: $SERVICE_USER"
else
    echo "User $SERVICE_USER already exists."
fi
mkdir -p "$APP_DIR" "$DATA_DIR" "$LOG_DIR" "$CONF_DIR"
chown "$SERVICE_USER:$SERVICE_USER" "$APP_DIR" "$DATA_DIR" "$LOG_DIR" "$CONF_DIR"

# ── 7. Clone and build ───────────────────────────────────────────────────────

step "7/10 Application"
if [[ ! -d "$APP_DIR/.git" ]]; then
    git clone "$REPO_URL" "$APP_DIR"
else
    echo "Repository already present at $APP_DIR, skipping clone."
fi
cd "$APP_DIR"
npm ci
npm run build
npm prune --omit=dev
echo "Build complete."

# ── 8. Config and data ───────────────────────────────────────────────────────

step "8/10 Config and data"
cp "$APP_DIR/mediamtx.yml" "$CONF_DIR/mediamtx.yml"
chown "$SERVICE_USER:$SERVICE_USER" "$CONF_DIR/mediamtx.yml"
echo "Config written to $CONF_DIR/"

# data.db and media/ live in DATA_DIR so they survive a full re-clone of the app.
# Symlinks in the app root keep the app's default paths working without config changes.
touch "$DATA_DIR/data.db"
chown "$SERVICE_USER:$SERVICE_USER" "$DATA_DIR/data.db"
if [[ ! -L "$APP_DIR/data.db" ]]; then
    ln -sfn "$DATA_DIR/data.db" "$APP_DIR/data.db"
fi

mkdir -p "$DATA_DIR/media"
chown "$SERVICE_USER:$SERVICE_USER" "$DATA_DIR/media"
if [[ ! -L "$APP_DIR/media" ]]; then
    ln -sfn "$DATA_DIR/media" "$APP_DIR/media"
fi

# ── 9. Monitoring manifests ──────────────────────────────────────────────────

step "9/10 Monitoring manifests"
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

# ── 10. Systemd units ────────────────────────────────────────────────────────

step "10/10 Systemd"
# mediamtx.yml keeps apiAddress and hlsAddress bound to 127.0.0.1 so the
# MediaMTX control API and HLS preview are never exposed directly to the network.
# hlsAlwaysRemux is off: HLS muxers spin up on first viewer request, which
# saves CPU/RAM when inputs are idle at the cost of a slower first preview load.
cat > /etc/systemd/system/mediamtx.service <<'EOF'
[Unit]
Description=MediaMTX Streaming Server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=restream
Group=restream
Environment=MTX_LOGDESTINATIONS=stdout,file
Environment=MTX_LOGFILE=/var/log/restream/mediamtx.log
ExecStart=/usr/local/bin/mediamtx /etc/restream/mediamtx.yml
Restart=always
RestartSec=2
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
EOF

cat > /etc/systemd/system/restream.service <<EOF
[Unit]
Description=Restream Control Plane
After=network-online.target mediamtx.service
Wants=network-online.target
Requires=mediamtx.service

[Service]
Type=simple
User=restream
Group=restream
WorkingDirectory=/opt/restream
Environment=NODE_ENV=production
Environment=PORT=3030
Environment=FFMPEG_PATH=/usr/local/bin/ffmpeg
Environment=FFPROBE_PATH=/usr/local/bin/ffprobe
ExecStart=/usr/bin/node /opt/restream/dist/index.js
Restart=always
RestartSec=2
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=full
ReadWritePaths=/var/lib/restream

[Install]
WantedBy=multi-user.target
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
systemctl enable --now prometheus.service
systemctl enable --now grafana-server.service
systemctl enable --now mediamtx.service
systemctl enable --now restream.service

echo
echo "=============================="
echo " Setup complete"
echo "=============================="
echo "Dashboard: http://<VM-external-IP>:3030/"
echo "Settings:  http://<VM-external-IP>:3030/settings.html"
echo "Login:     default password is 'admin' (change it in Settings)"
echo "Data:      $DATA_DIR/data.db"
echo ""
echo "Check status:"
echo "  systemctl status prometheus.service"
echo "  systemctl status grafana-server.service"
echo "  systemctl status mediamtx.service"
echo "  systemctl status restream.service"
echo "  curl -fsS http://127.0.0.1:3030/healthz"
echo "  curl -fsS http://127.0.0.1:9090/-/ready"
echo ""
echo "Follow logs:"
echo "  journalctl -u restream.service -f"
echo "  journalctl -u grafana-server.service -f"
echo ""
echo "Update later:"
echo "  sudo bash $APP_DIR/scripts/server-update.sh"
echo "Reset password:"
echo "  sudo bash $APP_DIR/scripts/server-reset-password.sh"
