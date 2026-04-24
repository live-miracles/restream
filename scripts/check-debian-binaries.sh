#!/usr/bin/env bash
set -euo pipefail

# Checks external binaries required by this repository's make targets
# and scripts (including down.sh, up.sh, and runtime spawning paths).
# MediaMTX binary is downloaded by: make deps (or --install flag).

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

INSTALL_MISSING=0
MEDIAMTX_VERSION="${MEDIAMTX_VERSION:-1.17.1}"

usage() {
  cat <<'EOF'
Usage: scripts/check-debian-binaries.sh [--install|-i] [--help|-h]

Options:
  -i, --install   Install missing packages via apt-get.
  -h, --help      Show this help text.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -i|--install)
      INSTALL_MISSING=1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1"
      usage
      exit 2
      ;;
  esac
  shift
done

ok() {
  printf "%b[OK]%b %s\n" "$GREEN" "$NC" "$1"
}

warn() {
  printf "%b[WARN]%b %s\n" "$YELLOW" "$NC" "$1"
}

fail() {
  printf "%b[MISSING]%b %s\n" "$RED" "$NC" "$1"
}

have_cmd() {
  command -v "$1" >/dev/null 2>&1
}

missing=()
missing_pkgs=()

add_missing() {
  local bin="$1"
  local pkg="$2"
  missing+=("$bin")
  missing_pkgs+=("$pkg")
}

check_cmd() {
  local bin="$1"
  local pkg="$2"
  local note="${3:-}"

  if have_cmd "$bin"; then
    if [[ -n "$note" ]]; then
      ok "$bin ($note)"
    else
      ok "$bin"
    fi
  else
    if [[ -n "$note" ]]; then
      fail "$bin ($note)"
    else
      fail "$bin"
    fi
    add_missing "$bin" "$pkg"
  fi
}

echo "== Debian binary preflight for restream =="
echo

# Core CLI and runtime dependencies.
# Core toolchain.
check_cmd make make
check_cmd node nodejs "project requires Node >=20.19.0"
check_cmd npm npm "project requires npm >=10"
check_cmd npx npm
check_cmd curl curl

echo
# Media processing.
check_cmd ffmpeg ffmpeg
check_cmd ffprobe ffmpeg

echo
# Process management (used by up.sh and down.sh).
check_cmd setsid util-linux
check_cmd nohup coreutils
check_cmd kill procps
check_cmd pkill procps
check_cmd xargs findutils
check_cmd seq coreutils

# Docker + compose plugin are required for run-host/run-docker/run-4x3.
if have_cmd docker; then
  ok docker
  if docker compose version >/dev/null 2>&1; then
    ok "docker compose (plugin)"
  else
    fail "docker compose (plugin)"
    add_missing "docker compose" docker-compose-plugin
  fi
else
  fail docker
  add_missing docker docker.io
  fail "docker compose (plugin)"
  add_missing "docker compose" docker-compose-plugin
fi

# down.sh uses fuser if present, else falls back to lsof.
if have_cmd fuser || have_cmd lsof; then
  if have_cmd fuser; then
    ok "fuser (preferred for port cleanup)"
  fi
  if have_cmd lsof; then
    ok "lsof (fallback for port cleanup)"
  fi
else
  fail "fuser or lsof"
  add_missing "fuser|lsof" psmisc
  add_missing "fuser|lsof" lsof
fi

echo

# Version hints (non-blocking, since exact package sources vary on Debian).
if have_cmd node; then
  node_ver="$(node -p 'process.versions.node' 2>/dev/null || true)"
  if [[ -n "$node_ver" ]]; then
    echo "Node detected: $node_ver"
  fi
fi

if have_cmd npm; then
  npm_ver="$(npm --version 2>/dev/null || true)"
  if [[ -n "$npm_ver" ]]; then
    echo "npm detected:  $npm_ver"
  fi
fi

MEDIAMTX_DIR="$(dirname "$0")/../bin/mediamtx"
MEDIAMTX_BINARY="$MEDIAMTX_DIR/mediamtx"

if [[ "$INSTALL_MISSING" == "1" && ! -x "$MEDIAMTX_BINARY" ]]; then
  ARCH="$(uname -m)"
  case "$ARCH" in
    x86_64) ARCH_NAME="amd64" ;;
    aarch64) ARCH_NAME="arm64" ;;
    armv7l) ARCH_NAME="armv7" ;;
    *)
      fail "mediamtx (unsupported architecture: $ARCH)"
      exit 1
      ;;
  esac
  FILENAME="mediamtx_v${MEDIAMTX_VERSION}_linux_${ARCH_NAME}.tar.gz"
  URL="https://github.com/bluenviron/mediamtx/releases/download/v${MEDIAMTX_VERSION}/${FILENAME}"
  echo "Downloading MediaMTX v${MEDIAMTX_VERSION} for linux/${ARCH_NAME}..."
  mkdir -p "$MEDIAMTX_DIR"
  curl -fsSL "$URL" -o "/tmp/$FILENAME"
  tar -xzf "/tmp/$FILENAME" -C "$MEDIAMTX_DIR"
  rm -f "/tmp/$FILENAME"
  chmod +x "$MEDIAMTX_BINARY"
  ok "mediamtx (downloaded v${MEDIAMTX_VERSION})"
elif [[ -x "$MEDIAMTX_BINARY" ]]; then
  ok "mediamtx (present)"
else
  warn "mediamtx (run --install to download)"
fi

echo
if [[ "${#missing[@]}" -eq 0 ]]; then
  printf "%bAll required binaries are present.%b\n" "$GREEN" "$NC"
  exit 0
fi

if [[ "${#missing[@]}" -gt 0 ]]; then
  printf "%bMissing binaries:%b %s\n" "$RED" "$NC" "${missing[*]}"
fi

# Deduplicate package hints while preserving first-seen order.
seen='|'
uniq_pkgs=()
for pkg in "${missing_pkgs[@]}"; do
  if [[ "$seen" != *"|$pkg|"* ]]; then
    uniq_pkgs+=("$pkg")
    seen+="$pkg|"
  fi
done

echo
warn "Install hints for Debian/Ubuntu:"
echo "  sudo apt-get update"
printf "  sudo apt-get install -y"
for pkg in "${uniq_pkgs[@]}"; do
  printf " %s" "$pkg"
done
printf "\n"

if [[ "$INSTALL_MISSING" == "1" && "${#missing[@]}" -gt 0 ]]; then
  echo
  warn "Installing missing packages..."
  if [[ "$(id -u)" -eq 0 ]]; then
    apt-get update
    apt-get install -y "${uniq_pkgs[@]}"
  elif have_cmd sudo; then
    sudo apt-get update
    sudo apt-get install -y "${uniq_pkgs[@]}"
  else
    fail "sudo is required for --install when not running as root"
    exit 1
  fi
  echo
  warn "Re-running checks after install..."
  exec "$0" --install
fi

echo
warn "Notes:"
echo "  - For Node 20+ on older Debian releases, use NodeSource or nvm if apt is behind."
echo "  - Docker compose plugin package name may vary by distro/repo setup."
echo "  - MediaMTX binary is downloaded by: make deps (or --install flag)."

exit 0
