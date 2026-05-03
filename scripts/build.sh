#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

BUILD_DIR="${BUILD_DIR:-build/deploy}"

if ! command -v npm >/dev/null 2>&1; then
  echo "npm is required to build the deployment bundle."
  exit 1
fi

if [[ "$BUILD_DIR" = /* ]]; then
  OUT_DIR="$BUILD_DIR"
else
  OUT_DIR="$ROOT_DIR/$BUILD_DIR"
fi

if [[ -z "$OUT_DIR" || "$OUT_DIR" = "/" ]]; then
  echo "Refusing to use output directory: $OUT_DIR"
  exit 1
fi

APP_DIR="$OUT_DIR/opt/restream"
CONFIG_DIR="$OUT_DIR/etc/restream"
SYSTEMD_DIR="$OUT_DIR/etc/systemd/system"

rm -rf "$APP_DIR" "$OUT_DIR/etc"
mkdir -p "$APP_DIR" "$APP_DIR/data" "$CONFIG_DIR" "$SYSTEMD_DIR"

install -m 0644 "$ROOT_DIR/package.json" "$APP_DIR/package.json"
install -m 0644 "$ROOT_DIR/package-lock.json" "$APP_DIR/package-lock.json"
cp -a "$ROOT_DIR/src" "$APP_DIR/src"
cp -a "$ROOT_DIR/public" "$APP_DIR/public"

if [[ -f "$ROOT_DIR/data/.gitkeep" ]]; then
  install -m 0644 "$ROOT_DIR/data/.gitkeep" "$APP_DIR/data/.gitkeep"
fi

(
  cd "$APP_DIR"
  npm ci --omit=dev --no-audit --no-fund
)

install -m 0644 "$ROOT_DIR/src/config/restream.json" "$CONFIG_DIR/restream.json"
install -m 0644 "$ROOT_DIR/infra/mediamtx.yml" "$CONFIG_DIR/mediamtx.yml"
install -m 0644 "$ROOT_DIR/infra/mediamtx.service" "$SYSTEMD_DIR/mediamtx.service"
install -m 0644 "$ROOT_DIR/infra/restream.service" "$SYSTEMD_DIR/restream.service"

echo "Staged deployment bundle in: $OUT_DIR"
echo "  - opt/restream/ (src, public, package files, production node_modules)"
echo "  - etc/restream/restream.json"
echo "  - etc/restream/mediamtx.yml"
echo "  - etc/systemd/system/restream.service"
echo "  - etc/systemd/system/mediamtx.service"