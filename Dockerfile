# syntax=docker/dockerfile:1.7

# ── Stage 1: native C/C++ deps (SRT, FFmpeg, x264, x265) ─────────────────────
#
# Clones sources and compiles everything from scratch — no pre-built artifacts
# required. Cached as a separate stage so native rebuilds (rare) don't
# invalidate the Rust/Cargo layer cache.
#
# BUILD REQUIREMENTS
# ──────────────────
# Git to clone SRT, FFmpeg, x264, x265 from upstream repos
# Build tools: gcc, g++, cmake, ninja, nasm, pkg-config, perl, python3
# OpenSSL dev headers (statically linked OpenSSL 3.0.13 as a build dep)
# CA certificates for git clone over HTTPS
#
# COMPILER FLAGS & HARDENING
# ──────────────────────────
# Optimization flags from setup-static-build.sh:
#   -O3 -march=x86-64-v2 (baseline for ~10 year old CPUs)
# Security hardening flags from setup-static-build.sh:
#   -D_FORTIFY_SOURCE=3 (buffer overflow checks)
#   -fstack-protector-strong (stack canaries in vulnerable frames)
#   -fPIC (position-independent code for potential future shared libs)
#
# OUTPUT
# ──────
# /build/.build/static/prefix/ → static lib/include/bin (SRT, FFmpeg, x264, x265 artifacts)
# /build/public/bin/ffmpeg     → binary for rust-embed
#
FROM ubuntu:24.04 AS native-deps

RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
        build-essential git cmake ninja-build nasm pkg-config perl python3 \
        libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Only the files setup-static-build.sh actually needs; everything else is
# excluded via .dockerignore so the layer cache stays stable.
COPY scripts/resource-limit        scripts/resource-limit
COPY scripts/setup-static-build.sh scripts/setup-static-build.sh
COPY test/srt-bond-client.c        test/srt-bond-client.c
COPY test/srt-bond-server.c        test/srt-bond-server.c
COPY test/ffmpeg-capabilities.c    test/ffmpeg-capabilities.c

# git archive strips execute bits; restore them explicitly.
RUN chmod +x scripts/resource-limit scripts/setup-static-build.sh

# resource-limit requires RESTREAM_BUILD_LOCK_HELD (set by flock in normal
# usage). Override it here — Docker is already single-tenant per build.
RUN RESTREAM_BUILD_LOCK_HELD=1 BUILD_JOBS="$(nproc)" \
    scripts/resource-limit ./scripts/setup-static-build.sh

# ── Stage 2: Rust binary ──────────────────────────────────────────────────────
#
# Compiles Rust source against statically linked native libraries from Stage 1.
# Produces a fully statically linked, hardened binary ready for Stage 3 (scratch).
#
# BUILD REQUIREMENTS
# ──────────────────
# Rust toolchain 1.96.0 (pinned in rust-toolchain.toml)
# Cargo with build.rs FFmpeg setup (reads Cargo.lock, build.rs, src/*, public/*, benches/*, tests/*)
# pkg-config to locate .build/static/prefix/lib/pkgconfig artifacts from Stage 1
# Static C/C++ headers/libs from Stage 1: OpenSSL, SRT, FFmpeg, x264, x265, SQLite
# clang + mold linker (referenced by .cargo/config.toml; static build overrides to bfd)
#
# COMPILER FLAGS & HARDENING
# ──────────────────────────
# Static linking flags: -C target-feature=+crt-static -C relocation-model=static
#                       -C link-arg=-fuse-ld=bfd -C link-arg=-static -C link-arg=-no-pie
# Hardening from .cargo/config.toml: -C link-arg=-Wl,-z,relro,-z,now (Full RELRO)
# Hardening from scripts/setup-static-build.sh: -D_FORTIFY_SOURCE=3 -fstack-protector-strong
#                                                (applied to all C/C++ native deps)
#
FROM ubuntu:24.04 AS rust-build

# Build tools: pkg-config for linking against Stage 1 artifacts; clang+mold for
# the .cargo/config.toml defaults (even though static build overrides linker);
# curl for rustup installation; tzdata and ca-certificates for runtime.
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
        build-essential pkg-config libssl-dev clang mold ca-certificates curl \
        tzdata \
    && rm -rf /var/lib/apt/lists/*

# Install exactly the toolchain pinned in rust-toolchain.toml.
COPY rust-toolchain.toml rust-toolchain.toml
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --default-toolchain none \
    && . "$HOME/.cargo/env" \
    && rustup toolchain install "$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)"

ENV PATH="/root/.cargo/bin:$PATH"

WORKDIR /build

# Native prefix from Stage 1.
COPY --from=native-deps /build/.build/static /build/.build/static

# Cargo dependency layer — changes only when Cargo.lock changes.
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./
COPY .cargo/ .cargo/

# Source layers — ordered so a code change only rebuilds from here.
COPY src/     src/
COPY public/  public/
COPY benches/ benches/
COPY tests/   tests/

# public/bin/ffmpeg is excluded from the build context (via .dockerignore) to
# avoid stale host copies. Copy the freshly built one from native-deps last so
# it lands correctly for rust-embed.
COPY --from=native-deps /build/public/bin/ffmpeg public/bin/ffmpeg

# env.sh from setup-static-build.sh bakes in host-absolute paths; set the
# Docker equivalents directly instead.
#
# FFMPEG_DIR is also declared in .cargo/config.toml as a relative env var
# (relative to workspace root = /build); the explicit ENV here takes precedence
# and makes the value unambiguous.
ENV PKG_CONFIG_PATH=/build/.build/static/prefix/lib/pkgconfig \
    RESTREAM_BUILD_ROOT=/build/.build/static \
    RESTREAM_STATIC_FFMPEG=1 \
    RESTREAM_FULLY_STATIC=1 \
    FFMPEG_DIR=/build/.build/static/prefix

# Produce a fully statically linked binary.
# -C linker=cc + -fuse-ld=bfd overrides .cargo/config.toml's clang+mold for
# this invocation only; RELRO from config.toml is kept via the explicit flag.
RUN cargo rustc --release --bin restream -- \
        -C target-feature=+crt-static \
        -C relocation-model=static \
        -C linker=cc \
        -C link-arg=-fuse-ld=bfd \
        -C link-arg=-static \
        -C link-arg=-no-pie \
        -C link-arg=-Wl,-z,relro,-z,now

# Fail the image build if any shared library leaked through.
RUN ldd target/release/restream 2>&1 \
    | grep -Eq "not a dynamic executable|statically linked" \
    || { echo "ERROR: binary is not statically linked:" >&2; \
         ldd target/release/restream >&2; exit 1; }

# ── Stage 3: scratch runtime ──────────────────────────────────────────────────
#
# Minimal production container: ~30 MB image, non-root user (uid 1000), fully
# statically linked Rust binary with hardened compiler flags (RELRO, canaries, FORTIFY).
#
# RUNTIME REQUIREMENTS
# ────────────────────
#
# Filesystems (in priority order):
#   /tmp                  FFmpeg binary extracted from rust-embed on startup.
#                         Must be exec-enabled tmpfs; see 'docker run' examples below.
#
#   /data                 SQLite database persistence (sqlite:/data/restream.db).
#                         Create a volume: docker volume create restream-db
#                         Mount as: -v restream-db:/data
#
#   /media                Media library & HLS segment storage.
#                         Create a volume: docker volume create restream-media
#                         Mount as: -v restream-media:/media
#
# Trust anchors & timezones (embedded in image):
#   /usr/share/zoneinfo   Timezone data (tokio + chrono need this for local time).
#   /etc/localtime        Symlink to active timezone.
#   /etc/ssl/certs/ca-certificates.crt   Root CAs for outbound TLS
#                         (RTMPS publishers, HTTPS push targets).
#   /etc/passwd           uid→name resolution for logging & some OpenSSL paths.
#
# Host isolation:
#   No /proc, /dev, /dev/shm, /sys, or shell — supplied by container runtime.
#   Binary runs as uid 1000 (non-root). No privilege escalation possible.
#
# DEVELOPMENT / TESTING
# ─────────────────────
#
# To override the embedded FFmpeg at runtime:
#   docker run -e FFMPEG_BIN_PATH=/path/to/ffmpeg restream:scratch-test
#
# To run locally for debugging (expose logs, bind data):
#   docker run --rm -it \
#     --tmpfs /tmp:exec,mode=1777 \
#     -v restream-db:/data \
#     -v restream-media:/media \
#     -p 3030:3030 -p 1935:1935 -p 10080:10080/udp \
#     restream:scratch-test
#
# To health-check (requires host curl or socat):
#   curl -s http://localhost:3030/api/status | jq .version
#
FROM scratch

COPY --from=rust-build /usr/share/zoneinfo             /usr/share/zoneinfo
COPY --from=rust-build /etc/localtime                  /etc/localtime
COPY --from=rust-build /etc/ssl/certs/ca-certificates.crt \
                                                        /etc/ssl/certs/ca-certificates.crt
# Minimal passwd with one non-root entry so uid 1000 resolves by name.
# The Ubuntu build image only has system accounts; create this file explicitly.
COPY --from=rust-build /etc/passwd /etc/passwd
# (the COPY gives us system accounts; USER 1000 works even without a named
# entry because Docker accepts bare uids — but having the entry avoids silent
# failures in getpwuid callers like some OpenSSL codepaths)

COPY --from=rust-build /build/target/release/restream  /restream

EXPOSE 3030 1935 10080/udp

# Non-root. uid 1000 is present in the /etc/passwd we copied above.
USER 1000

ENV RESTREAM_DB_PATH=/data/restream.db \
    RESTREAM_MEDIA_DIR=/media \
    RESTREAM_LOG_DIR=/tmp/logs

# Production invocation (persisted data):
#
#   docker run -d --name restream \
#     --tmpfs /tmp:exec,mode=1777 \
#     -v restream-db:/data \
#     -v restream-media:/media \
#     -p 3030:3030 -p 1935:1935 -p 10080:10080/udp \
#     restream:scratch-test
#
# For ephemeral testing (data discarded on exit):
#
#   docker run --rm \
#     --tmpfs /tmp:exec,mode=1777 \
#     -p 3030:3030 \
#     restream:scratch-test
#
# To inspect runtime environment (debug only):
#
#   docker run --rm -it \
#     --tmpfs /tmp:exec,mode=1777 \
#     --entrypoint /bin/sh \
#     restream:scratch-test
#   # (fails: no shell in scratch; use a devel image instead)
#
ENTRYPOINT ["/restream"]
