#!/usr/bin/env bash
# One-shot infrastructure setup for a Restream GCP Linux VM.
# Installs FFmpeg 7.1, MediaMTX 1.17.1, creates the service user,
# directories, and systemd units.
#
# Does NOT build the app. Build the binary on your dev machine first:
#   make build-linux          # cross-compiles for Linux amd64
#   bash scripts/deploy.sh    # copies binary to server and starts it
#
# Usage (run as root on the VM):
#   sudo bash /opt/restream/scripts/server-setup.sh
#
# Idempotent: safe to re-run.
set -euo pipefail

if [[ "$(id -u)" -ne 0 ]]; then
    echo "ERROR: run as root (sudo bash scripts/server-setup.sh)" >&2
    exit 1
fi

APP_DIR=/opt/restream
DATA_DIR=/var/lib/restream
LOG_DIR=/var/log/restream
CONF_DIR=/etc/restream
SERVICE_USER=restream

MEDIAMTX_VERSION=1.17.1
FFMPEG_VERSION=7.1

WORK="$(mktemp -d)"
trap "rm -rf $WORK" EXIT

step() { echo; echo "=== $* ==="; }

# ── 1. System packages ──────────────────────────────────────────────────────

step "1/6 System packages"
apt-get update -q
apt-get install -y -q curl tar xz-utils git ca-certificates

# ── 2. FFmpeg 7.1.4 (BtbN static build) ────────────────────────────────────

step "2/6 FFmpeg $FFMPEG_VERSION"
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

# ── 3. MediaMTX ─────────────────────────────────────────────────────────────

step "3/6 MediaMTX $MEDIAMTX_VERSION"
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

# ── 4. Service user and directories ─────────────────────────────────────────

step "4/6 Service user and directories"
if ! id "$SERVICE_USER" &>/dev/null; then
    useradd --system --home "$APP_DIR" --shell /usr/sbin/nologin "$SERVICE_USER"
    echo "Created user: $SERVICE_USER"
else
    echo "User $SERVICE_USER already exists."
fi
mkdir -p "$APP_DIR/dist" "$DATA_DIR" "$LOG_DIR" "$CONF_DIR"
chown -R "$SERVICE_USER:$SERVICE_USER" "$APP_DIR" "$DATA_DIR" "$LOG_DIR" "$CONF_DIR"

# data.db symlink so the app's default path always resolves to DATA_DIR.
sudo -u "$SERVICE_USER" touch "$DATA_DIR/data.db"
if [[ ! -L "$APP_DIR/data.db" ]]; then
    ln -sfn "$DATA_DIR/data.db" "$APP_DIR/data.db"
fi

# ── 5. MediaMTX config ───────────────────────────────────────────────────────

step "5/6 MediaMTX config"
if [[ -f "$APP_DIR/mediamtx.yml" ]]; then
    cp "$APP_DIR/mediamtx.yml" "$CONF_DIR/mediamtx.yml"
    chown "$SERVICE_USER:$SERVICE_USER" "$CONF_DIR/mediamtx.yml"
    echo "Copied mediamtx.yml → $CONF_DIR/mediamtx.yml"
else
    echo "WARNING: $APP_DIR/mediamtx.yml not found — copy it manually before starting mediamtx.service"
fi

# ── 6. Systemd units ─────────────────────────────────────────────────────────

step "6/6 Systemd"
cat > /etc/systemd/system/mediamtx.service <<'EOF'
[Unit]
Description=MediaMTX Streaming Server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=restream
Group=restream
ExecStart=/usr/local/bin/mediamtx /etc/restream/mediamtx.yml
Restart=always
RestartSec=2
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
EOF

cat > /etc/systemd/system/restream.service <<'EOF'
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
Environment=PORT=3030
Environment=FFMPEG_PATH=/usr/local/bin/ffmpeg
Environment=FFPROBE_PATH=/usr/local/bin/ffprobe
ExecStart=/opt/restream/dist/restream
Restart=always
RestartSec=2
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=full
ReadWritePaths=/var/lib/restream /opt/restream

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable mediamtx.service restream.service

echo
echo "=============================="
echo " Infrastructure setup complete"
echo "=============================="
echo ""
echo "Next: deploy the app binary from your dev machine:"
echo "  bash scripts/deploy.sh <user@server>"
echo ""
echo "Or manually copy and start:"
echo "  scp dist/restream <user@server>:$APP_DIR/dist/restream"
echo "  ssh <user@server> 'sudo cp mediamtx.yml $CONF_DIR/ && sudo systemctl start mediamtx restream'"
echo ""
echo "Data:    $DATA_DIR/data.db"
echo "Logs:    journalctl -u restream.service -f"
echo "Health:  curl http://127.0.0.1:3030/healthz"
