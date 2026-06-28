#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_ROOT="${RESTREAM_BUILD_ROOT:-$ROOT/.build/static}"

if [[ -z "${RESTREAM_BUILD_LOCK_HELD:-}" ]]; then
    echo "build-static: run via scripts/resource-limit ./scripts/build-static.sh" >&2
    exit 2
fi

if [[ ! -f "$BUILD_ROOT/env.sh" ]]; then
    "$ROOT/scripts/resource-limit" "$ROOT/scripts/setup-static-build.sh"
fi

# shellcheck source=/dev/null
source "$BUILD_ROOT/env.sh"

PROFILE="${RESTREAM_BUILD_PROFILE:-release}"
if [[ "$PROFILE" != "release" && "$PROFILE" != "fast-release" ]]; then
    echo "RESTREAM_BUILD_PROFILE must be release or fast-release" >&2
    exit 2
fi

cd "$ROOT"
cargo rustc --profile "$PROFILE" --bin restream -- \
    -C target-feature=+crt-static \
    -C relocation-model=static \
    -C linker=cc \
    -C link-arg=-fuse-ld=bfd \
    -C link-arg=-static \
    -C link-arg=-no-pie

BINARY="$CARGO_TARGET_DIR/$PROFILE/restream"
SBOM="$ROOT/sbom/restream-runtime.cdx.json"
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

"$BINARY" --emit-sbom "$SBOM"
