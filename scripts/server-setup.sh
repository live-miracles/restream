#!/usr/bin/env bash
# One-shot setup for a Restream GCP Linux VM.
# Installs Node.js 22, FFmpeg 7.1.4, MediaMTX 1.17.1, builds the app,
# and registers systemd services that start on boot.
#
# Usage (run as root on the VM):
#   sudo bash /opt/restream/scripts/server-setup.sh
#   REPO_URL=https://github.com/your-fork/restream sudo bash scripts/server-setup.sh
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
SERVER_NAME="${SERVER_NAME:-Restream}"

MEDIAMTX_VERSION=1.17.1
FFMPEG_VERSION=7.1.4

arch="$(uname -m)"
case "$arch" in
    x86_64) MEDIAMTX_ARCH=amd64; FFMPEG_ARCH=linux64 ;;
    aarch64) MEDIAMTX_ARCH=arm64; FFMPEG_ARCH=linuxarm64 ;;
    *) echo "ERROR: unsupported architecture: $arch" >&2; exit 1 ;;
esac

WORK="$(mktemp -d)"
trap "rm -rf $WORK" EXIT

step() { echo; echo "=== $* ==="; }

# ── 1. System packages ──────────────────────────────────────────────────────

step "1/8 System packages"
apt-get update -q
apt-get install -y -q curl tar xz-utils git ca-certificates

# ── 2. Node.js 22 ───────────────────────────────────────────────────────────

step "2/8 Node.js 22"
if node --version 2>/dev/null | grep -q '^v22'; then
    echo "Node.js 22 already installed: $(node --version)"
else
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash -
    apt-get install -y nodejs
    echo "Installed: $(node --version)"
fi

# ── 3. FFmpeg 7.1.4 (BtbN static build) ────────────────────────────────────

step "3/8 FFmpeg $FFMPEG_VERSION"
FFMPEG_FILENAME="ffmpeg-n${FFMPEG_VERSION}-${FFMPEG_ARCH}-gpl.tar.xz"
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

step "4/8 MediaMTX $MEDIAMTX_VERSION"
if /usr/local/bin/mediamtx --version 2>/dev/null | grep -q "$MEDIAMTX_VERSION"; then
    echo "MediaMTX $MEDIAMTX_VERSION already installed."
else
    MEDIAMTX_FILENAME="mediamtx_v${MEDIAMTX_VERSION}_linux_${MEDIAMTX_ARCH}.tar.gz"
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

# ── 5. Service user and directories ─────────────────────────────────────────

step "5/8 Service user and directories"
if ! id "$SERVICE_USER" &>/dev/null; then
    useradd --system --home "$APP_DIR" --shell /usr/sbin/nologin "$SERVICE_USER"
    echo "Created user: $SERVICE_USER"
else
    echo "User $SERVICE_USER already exists."
fi
mkdir -p "$DATA_DIR" "$LOG_DIR" "$CONF_DIR"
chown "$SERVICE_USER:$SERVICE_USER" "$DATA_DIR" "$LOG_DIR" "$CONF_DIR"
echo "Server name: $SERVER_NAME"

# ── 6. Clone and build ───────────────────────────────────────────────────────

step "6/8 Application"
if [[ ! -d "$APP_DIR/.git" ]]; then
    sudo -u "$SERVICE_USER" git clone "$REPO_URL" "$APP_DIR"
else
    echo "Repository already present at $APP_DIR, skipping clone."
fi
cd "$APP_DIR"
sudo -u "$SERVICE_USER" npm ci
sudo -u "$SERVICE_USER" npm run ts-build
sudo -u "$SERVICE_USER" npm prune --omit=dev
echo "Build complete."

# ── 7. Config and data ───────────────────────────────────────────────────────

step "7/8 Config and data"
cp "$APP_DIR/mediamtx.yml" "$CONF_DIR/mediamtx.yml"
chown "$SERVICE_USER:$SERVICE_USER" "$CONF_DIR/mediamtx.yml"
echo "Config written to $CONF_DIR/"

sudo -u "$SERVICE_USER" touch "$DATA_DIR/data.db"
if [[ ! -L "$APP_DIR/data.db" ]]; then
    sudo -u "$SERVICE_USER" ln -sfn "$DATA_DIR/data.db" "$APP_DIR/data.db"
fi

# ── 8. Systemd units ─────────────────────────────────────────────────────────

step "8/8 Systemd"
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
ExecStart=/usr/bin/node /opt/restream/src/index.js
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
systemctl enable --now mediamtx.service
systemctl enable --now restream.service

# Seed server name in database (wait briefly for the app to initialize the DB)
sleep 2
curl -s -X PATCH http://127.0.0.1:3030/config \
    -H 'Content-Type: application/json' \
    -d "{\"serverName\": \"$SERVER_NAME\"}" > /dev/null && echo "Server name set to: $SERVER_NAME"

echo
echo "=============================="
echo " Setup complete"
echo "=============================="
echo "Dashboard: http://<VM-external-IP>:3030/"
echo "Data:      $DATA_DIR/data.db"
echo ""
echo "Change server name:"
echo "  curl -X PATCH http://127.0.0.1:3030/config -H 'Content-Type: application/json' -d '{\"serverName\": \"My Server\"}'"
echo ""
echo "Check status:"
echo "  systemctl status mediamtx.service"
echo "  systemctl status restream.service"
echo "  curl -fsS http://127.0.0.1:3030/healthz"
echo ""
echo "Follow logs:"
echo "  journalctl -u restream.service -f"
echo ""
echo "Update later:"
echo "  sudo bash $APP_DIR/scripts/server-update.sh"
