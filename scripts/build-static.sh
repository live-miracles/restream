#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_ROOT="${RESTREAM_BUILD_ROOT:-$ROOT/.build/static}"

if [[ ! -f "$BUILD_ROOT/env.sh" ]]; then
    "$ROOT/scripts/setup-static-build.sh"
fi

# shellcheck source=/dev/null
source "$BUILD_ROOT/env.sh"

cd "$ROOT"
cargo rustc --release --bin restream -- \
    -C target-feature=+crt-static \
    -C relocation-model=static \
    -C linker=cc \
    -C link-arg=-fuse-ld=bfd \
    -C link-arg=-static \
    -C link-arg=-no-pie

BINARY="$CARGO_TARGET_DIR/release/restream"
file "$BINARY"
"$BUILD_ROOT/prefix/bin/restream-ffmpeg-capabilities"

ldd_output="$(ldd "$BINARY" 2>&1 || true)"
if grep -Eq "not a dynamic executable|statically linked" <<<"$ldd_output"; then
    echo "Verified: $BINARY is statically linked."
else
    echo "Static verification failed:" >&2
    echo "$ldd_output" >&2
    exit 1
fi
