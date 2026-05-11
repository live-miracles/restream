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
NODE_MIN_VERSION="${NODE_MIN_VERSION:-20.19.0}"
NODE_MAX_EXCLUSIVE_MAJOR="${NODE_MAX_EXCLUSIVE_MAJOR:-26}"
NPM_MIN_VERSION="${NPM_MIN_VERSION:-10.0.0}"
MEDIAMTX_NEEDS_INSTALL=0

MEDIAMTX_DIR="$(dirname "$0")/../bin/mediamtx"
MEDIAMTX_BINARY="$MEDIAMTX_DIR/mediamtx"

usage() {
  cat <<'EOF_USAGE'
Usage: scripts/check-debian-binaries.sh [--install|-i] [--help|-h]

Options:
  -i, --install   Install missing packages via apt-get.
  -h, --help      Show this help text.
EOF_USAGE
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

error() {
  printf "%b[ERROR]%b %s\n" "$RED" "$NC" "$1"
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

semver_to_parts() {
  local version="${1#v}"
  local major minor patch

  IFS=. read -r major minor patch _ <<<"$version"
  printf '%s %s %s\n' "${major:-0}" "${minor:-0}" "${patch:-0}"
}

compare_semver() {
  local left_major left_minor left_patch right_major right_minor right_patch
  local left_part right_part

  read -r left_major left_minor left_patch <<<"$(semver_to_parts "$1")"
  read -r right_major right_minor right_patch <<<"$(semver_to_parts "$2")"

  for pair in \
    "$left_major $right_major" \
    "$left_minor $right_minor" \
    "$left_patch $right_patch"
  do
    read -r left_part right_part <<<"$pair"
    if (( 10#$left_part > 10#$right_part )); then
      echo 1
      return 0
    fi
    if (( 10#$left_part < 10#$right_part )); then
      echo -1
      return 0
    fi
  done

  echo 0
}

enforce_node_version() {
  local node_ver node_major

  node_ver="$(node -p 'process.versions.node' 2>/dev/null || true)"
  if [[ -z "$node_ver" ]]; then
    error 'Unable to determine the installed Node.js version.'
    exit 1
  fi

  read -r node_major _ _ <<<"$(semver_to_parts "$node_ver")"
  if (( $(compare_semver "$node_ver" "$NODE_MIN_VERSION") < 0 )) || \
     (( 10#$node_major >= 10#$NODE_MAX_EXCLUSIVE_MAJOR )); then
    error "Unsupported Node.js version: $node_ver"
    echo "This project requires Node.js >= $NODE_MIN_VERSION and < ${NODE_MAX_EXCLUSIVE_MAJOR}.0.0." >&2
    exit 1
  fi

  ok "node version $node_ver"
}

enforce_npm_version() {
  local npm_ver

  npm_ver="$(npm --version 2>/dev/null || true)"
  if [[ -z "$npm_ver" ]]; then
    error 'Unable to determine the installed npm version.'
    exit 1
  fi

  if (( $(compare_semver "$npm_ver" "$NPM_MIN_VERSION") < 0 )); then
    error "Unsupported npm version: $npm_ver"
    echo "This project requires npm >= $NPM_MIN_VERSION." >&2
    exit 1
  fi

  ok "npm version $npm_ver"
}

download_mediamtx() {
  local arch arch_name filename url checksums_url tmpdir archive_path checksum_path
  local expected_checksum checksum_entry actual_name actual_checksum

  arch="$(uname -m)"
  case "$arch" in
    x86_64) arch_name='amd64' ;;
    aarch64) arch_name='arm64' ;;
    armv6l) arch_name='armv6' ;;
    armv7l) arch_name='armv7' ;;
    *)
      error "Unsupported MediaMTX architecture: $arch"
      exit 1
      ;;
  esac

  filename="mediamtx_v${MEDIAMTX_VERSION}_linux_${arch_name}.tar.gz"
  url="https://github.com/bluenviron/mediamtx/releases/download/v${MEDIAMTX_VERSION}/${filename}"
  checksums_url="https://github.com/bluenviron/mediamtx/releases/download/v${MEDIAMTX_VERSION}/checksums.sha256"

  tmpdir="$(mktemp -d)"
  archive_path="$tmpdir/$filename"
  checksum_path="$tmpdir/checksums.sha256"
  trap "rm -rf '$tmpdir'" RETURN

  echo "Downloading MediaMTX v${MEDIAMTX_VERSION} for linux/${arch_name}..."
  curl -fsSL "$url" -o "$archive_path"
  curl -fsSL "$checksums_url" -o "$checksum_path"

  expected_checksum=''
  while read -r checksum_entry actual_name; do
    actual_name="${actual_name#\*}"
    actual_name="${actual_name%$'\r'}"
    if [[ "$actual_name" == "$filename" ]]; then
      expected_checksum="$checksum_entry"
      break
    fi
  done < "$checksum_path"

  if [[ -z "$expected_checksum" ]]; then
    error "Could not find checksum entry for $filename in checksums.sha256"
    exit 1
  fi

  set -- $(sha256sum "$archive_path")
  actual_checksum="$1"
  if [[ "$actual_checksum" != "$expected_checksum" ]]; then
    error "Checksum verification failed for $filename"
    echo "Expected: $expected_checksum" >&2
    echo "Actual:   $actual_checksum" >&2
    exit 1
  fi

  rm -rf "$MEDIAMTX_DIR"
  mkdir -p "$MEDIAMTX_DIR"
  tar -xzf "$archive_path" -C "$MEDIAMTX_DIR"
  chmod +x "$MEDIAMTX_BINARY"
  ok "mediamtx (downloaded v${MEDIAMTX_VERSION}, checksum verified)"
}

get_mediamtx_version() {
  local raw

  if [[ ! -x "$MEDIAMTX_BINARY" ]]; then
    return 1
  fi

  raw="$("$MEDIAMTX_BINARY" --version 2>/dev/null || "$MEDIAMTX_BINARY" version 2>/dev/null || true)"
  raw="${raw%%$'\n'*}"
  if [[ "$raw" =~ ([0-9]+\.[0-9]+\.[0-9]+) ]]; then
    printf '%s\n' "${BASH_REMATCH[1]}"
    return 0
  fi

  return 1
}

ensure_mediamtx_binary() {
  local installed_version=''

  if [[ ! -x "$MEDIAMTX_BINARY" ]]; then
    if [[ "$INSTALL_MISSING" == "1" ]]; then
      download_mediamtx
    else
      warn "mediamtx (run --install to download v${MEDIAMTX_VERSION})"
      MEDIAMTX_NEEDS_INSTALL=1
    fi
    return 0
  fi

  installed_version="$(get_mediamtx_version || true)"
  if [[ -z "$installed_version" ]]; then
    if [[ "$INSTALL_MISSING" == "1" ]]; then
      warn "mediamtx is present but its version could not be determined; replacing it with v${MEDIAMTX_VERSION}"
      download_mediamtx
    else
      fail "mediamtx (present, but version could not be determined)"
      MEDIAMTX_NEEDS_INSTALL=1
    fi
    return 0
  fi

  if [[ "$installed_version" != "$MEDIAMTX_VERSION" ]]; then
    if [[ "$INSTALL_MISSING" == "1" ]]; then
      warn "mediamtx version mismatch (found v${installed_version}, expected v${MEDIAMTX_VERSION}); replacing it"
      download_mediamtx
    else
      fail "mediamtx (found v${installed_version}, expected v${MEDIAMTX_VERSION})"
      MEDIAMTX_NEEDS_INSTALL=1
    fi
    return 0
  fi

  ok "mediamtx v${installed_version}"
}

echo "== Debian binary preflight for restream =="
echo

# Core CLI and runtime dependencies.
# Core toolchain.
check_cmd make make
check_cmd node nodejs
check_cmd npm npm
check_cmd npx npm
check_cmd curl curl
check_cmd tar tar
check_cmd sha256sum coreutils
check_cmd mktemp coreutils

echo
# Media processing.
check_cmd ffmpeg ffmpeg
check_cmd ffprobe ffmpeg

echo
# Process management (used by up.sh, down.sh, and run-2x3.mjs).
check_cmd setsid util-linux
check_cmd nohup coreutils
check_cmd ps procps
check_cmd kill procps
check_cmd pkill procps
check_cmd xargs findutils
check_cmd seq coreutils
check_cmd nc netcat-openbsd

# Docker is optional here and only needed for Docker-backed workflows.
if have_cmd docker; then
  ok "docker (optional: run-docker/run-2x3)"
  if docker compose version >/dev/null 2>&1; then
    ok "docker compose (optional: run-docker/run-2x3)"
  else
    warn "docker compose (optional: install it for run-docker/run-2x3)"
  fi
else
  warn "docker (optional: install it for run-docker/run-2x3)"
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
# Deduplicate package hints while preserving first-seen order.
seen='|'
uniq_pkgs=()
for pkg in "${missing_pkgs[@]}"; do
  if [[ "$seen" != *"|$pkg|"* ]]; then
    uniq_pkgs+=("$pkg")
    seen+="$pkg|"
  fi
done

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
if have_cmd node; then
  enforce_node_version
fi

if have_cmd npm; then
  enforce_npm_version
fi

ensure_mediamtx_binary

echo
if [[ "${#missing[@]}" -eq 0 && "$MEDIAMTX_NEEDS_INSTALL" -eq 0 ]]; then
  printf "%bAll required binaries are present.%b\n" "$GREEN" "$NC"
  exit 0
fi

if [[ "${#missing[@]}" -gt 0 ]]; then
  printf "%bMissing binaries:%b %s\n" "$RED" "$NC" "${missing[*]}"
fi

echo
warn "Install hints for Debian/Ubuntu:"
echo "  sudo apt-get update"
if [[ "${#uniq_pkgs[@]}" -gt 0 ]]; then
  printf "  sudo apt-get install -y"
  for pkg in "${uniq_pkgs[@]}"; do
    printf " %s" "$pkg"
  done
  printf "\n"
fi
if [[ "$MEDIAMTX_NEEDS_INSTALL" -eq 1 ]]; then
  echo "  scripts/check-debian-binaries.sh --install    # downloads MediaMTX v${MEDIAMTX_VERSION}"
fi

echo
warn "Notes:"
echo "  - For Node 20+ on older Debian releases, use NodeSource or nvm if apt is behind."
echo "  - Docker is optional for make deps; install it for make run-docker or make run-2x3."
echo "  - Docker compose plugin package name may vary by distro/repo setup."
echo "  - MediaMTX binary is downloaded by: make deps (or --install flag)."

exit 1
