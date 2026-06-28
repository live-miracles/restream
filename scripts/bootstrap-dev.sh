#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_TOOLCHAIN="$(sed -n 's/^channel = "\(.*\)"/\1/p' "$ROOT/rust-toolchain.toml")"
FRONTEND_NODE_MAJOR="${RESTREAM_FRONTEND_NODE_MAJOR:-22}"
FRONTEND_NODE_MIN_MAJOR=20
WITH_FRONTEND=1
RUN_NATIVE_SETUP=1

usage() {
    cat <<'EOF'
Usage: scripts/bootstrap-dev.sh [options]

Bootstraps a Debian/Ubuntu development environment for this repo:
  - installs required apt packages
  - installs rustup if needed and the pinned Rust toolchain
  - installs frontend npm dependencies
  - builds the pinned native dependency prefix via scripts/setup-static-build.sh

Options:
  --skip-frontend      skip nodejs/npm install and npm ci
  --skip-native-setup  skip scripts/setup-static-build.sh
  -h, --help           show this help
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-frontend)
            WITH_FRONTEND=0
            shift
            ;;
        --skip-native-setup)
            RUN_NATIVE_SETUP=0
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "bootstrap-dev: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "bootstrap-dev: this script currently supports Linux hosts only" >&2
    exit 1
fi

if ! command -v apt-get >/dev/null; then
    echo "bootstrap-dev: apt-get is required; install dependencies manually on this distro" >&2
    exit 1
fi

APT_PACKAGES=(
    build-essential
    ca-certificates
    clang
    cmake
    curl
    ffmpeg
    file
    git
    jq
    libavcodec-dev
    libavdevice-dev
    libavfilter-dev
    libavformat-dev
    libavutil-dev
    libssl-dev
    libswresample-dev
    libswscale-dev
    mold
    nasm
    ninja-build
    perl
    pkg-config
    tzdata
)

run_as_root() {
    if [[ "$(id -u)" -eq 0 ]]; then
        "$@"
    elif command -v sudo >/dev/null; then
        sudo "$@"
    else
        echo "bootstrap-dev: need sudo to install: $*" >&2
        exit 1
    fi
}

ensure_frontend_node_toolchain() {
    local current_major=""
    if command -v node >/dev/null 2>&1; then
        current_major="$(node -p 'process.versions.node.split(".")[0]')"
    fi

    if command -v npm >/dev/null 2>&1 &&
        [[ -n "$current_major" ]] &&
        (( current_major >= FRONTEND_NODE_MIN_MAJOR )); then
        echo "bootstrap-dev: Node.js $(node --version) already satisfies frontend toolchain"
        return
    fi

    echo "bootstrap-dev: installing Node.js ${FRONTEND_NODE_MAJOR}.x frontend toolchain"
    run_as_root bash -lc "curl -fsSL https://deb.nodesource.com/setup_${FRONTEND_NODE_MAJOR}.x | bash -"
    run_as_root apt-get install -y nodejs
}

missing_packages=()
for package in "${APT_PACKAGES[@]}"; do
    if ! dpkg-query -W "$package" >/dev/null 2>&1; then
        missing_packages+=("$package")
    fi
done

if ((${#missing_packages[@]})); then
    echo "bootstrap-dev: installing apt packages: ${missing_packages[*]}"
    run_as_root apt-get update
    run_as_root apt-get install -y "${missing_packages[@]}"
else
    echo "bootstrap-dev: apt packages already present"
fi

export PATH="$HOME/.cargo/bin:$PATH"

if ! command -v rustup >/dev/null; then
    echo "bootstrap-dev: installing rustup"
    curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain none
fi

export PATH="$HOME/.cargo/bin:$PATH"

if [[ -z "$RUST_TOOLCHAIN" ]]; then
    echo "bootstrap-dev: failed to read rust-toolchain.toml" >&2
    exit 1
fi

echo "bootstrap-dev: installing Rust toolchain $RUST_TOOLCHAIN"
rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal --component rustfmt --component clippy
(cd "$ROOT" && rustup override set "$RUST_TOOLCHAIN" >/dev/null)

if (( WITH_FRONTEND )); then
    ensure_frontend_node_toolchain
    echo "bootstrap-dev: installing frontend npm dependencies"
    npm ci --include=optional --prefix "$ROOT"
    if ! (cd "$ROOT" && npx tailwindcss --help >/dev/null); then
        echo "bootstrap-dev: frontend toolchain check failed after npm ci" >&2
        exit 1
    fi
fi

if (( RUN_NATIVE_SETUP )); then
    echo "bootstrap-dev: building pinned native dependency prefix"
    "$ROOT/scripts/resource-limit" "$ROOT/scripts/setup-static-build.sh"
fi

cat <<EOF
bootstrap-dev: done

Next steps:
  scripts/resource-limit ./scripts/build-native.sh
  cargo run
EOF
