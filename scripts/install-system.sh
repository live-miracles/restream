#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

BUILD_DIR="${BUILD_DIR:-build/deploy}"

if [[ "$BUILD_DIR" = /* ]]; then
  STAGE_DIR="$BUILD_DIR"
else
  STAGE_DIR="$ROOT_DIR/$BUILD_DIR"
fi

APP_STAGE_DIR="$STAGE_DIR/opt/restream"
CONFIG_STAGE_DIR="$STAGE_DIR/etc/restream"
SYSTEMD_STAGE_DIR="$STAGE_DIR/etc/systemd/system"

APP_TARGET_DIR="/opt/restream"
CONFIG_TARGET_DIR="/etc/restream"
SYSTEMD_TARGET_DIR="/etc/systemd/system"
DATA_TARGET_DIR="/var/lib/restream"
SERVICE_USER="restream"
SERVICE_GROUP="restream"

if [[ ! -d "$APP_STAGE_DIR" || ! -d "$CONFIG_STAGE_DIR" || ! -d "$SYSTEMD_STAGE_DIR" ]]; then
  echo "Staged bundle not found in: $STAGE_DIR"
  echo "Run 'make build' first."
  exit 1
fi

if [[ "$(id -u)" -eq 0 ]]; then
  SUDO=()
else
  if ! command -v sudo >/dev/null 2>&1; then
    echo "sudo is required to install into system directories."
    exit 1
  fi
  SUDO=(sudo)
fi

"${SUDO[@]}" install -d -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$APP_TARGET_DIR"
"${SUDO[@]}" install -d -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$CONFIG_TARGET_DIR"
"${SUDO[@]}" install -d -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$DATA_TARGET_DIR"
"${SUDO[@]}" install -d "$SYSTEMD_TARGET_DIR"

if [[ ! -e "$APP_TARGET_DIR/data" ]]; then
  "${SUDO[@]}" install -d -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$APP_TARGET_DIR/data"
fi

"${SUDO[@]}" rm -rf \
  "$APP_TARGET_DIR/src" \
  "$APP_TARGET_DIR/public" \
  "$APP_TARGET_DIR/node_modules"
"${SUDO[@]}" rm -f \
  "$APP_TARGET_DIR/package.json" \
  "$APP_TARGET_DIR/package-lock.json"

"${SUDO[@]}" cp -a "$APP_STAGE_DIR/src" "$APP_TARGET_DIR/src"
"${SUDO[@]}" cp -a "$APP_STAGE_DIR/public" "$APP_TARGET_DIR/public"
"${SUDO[@]}" cp -a "$APP_STAGE_DIR/node_modules" "$APP_TARGET_DIR/node_modules"

"${SUDO[@]}" chown -R "$SERVICE_USER:$SERVICE_GROUP" \
  "$APP_TARGET_DIR/src" \
  "$APP_TARGET_DIR/public" \
  "$APP_TARGET_DIR/node_modules"

"${SUDO[@]}" install -o "$SERVICE_USER" -g "$SERVICE_GROUP" -m 0644 \
  "$APP_STAGE_DIR/package.json" "$APP_TARGET_DIR/package.json"
"${SUDO[@]}" install -o "$SERVICE_USER" -g "$SERVICE_GROUP" -m 0644 \
  "$APP_STAGE_DIR/package-lock.json" "$APP_TARGET_DIR/package-lock.json"

if [[ -f "$APP_STAGE_DIR/data/.gitkeep" && -d "$APP_TARGET_DIR/data" ]]; then
  "${SUDO[@]}" install -o "$SERVICE_USER" -g "$SERVICE_GROUP" -m 0644 \
    "$APP_STAGE_DIR/data/.gitkeep" "$APP_TARGET_DIR/data/.gitkeep"
fi

"${SUDO[@]}" install -o "$SERVICE_USER" -g "$SERVICE_GROUP" -m 0644 \
  "$CONFIG_STAGE_DIR/restream.json" "$CONFIG_TARGET_DIR/restream.json"
"${SUDO[@]}" install -o "$SERVICE_USER" -g "$SERVICE_GROUP" -m 0644 \
  "$CONFIG_STAGE_DIR/mediamtx.yml" "$CONFIG_TARGET_DIR/mediamtx.yml"

"${SUDO[@]}" install -m 0644 \
  "$SYSTEMD_STAGE_DIR/restream.service" "$SYSTEMD_TARGET_DIR/restream.service"
"${SUDO[@]}" install -m 0644 \
  "$SYSTEMD_STAGE_DIR/mediamtx.service" "$SYSTEMD_TARGET_DIR/mediamtx.service"

echo "Installed deployment bundle from: $STAGE_DIR"
echo "  - $APP_TARGET_DIR"
echo "  - $CONFIG_TARGET_DIR"
echo "  - $SYSTEMD_TARGET_DIR"
echo
echo "Next steps:"
echo "  - sudo systemctl daemon-reload"
echo "  - sudo systemctl enable --now mediamtx.service restream.service"