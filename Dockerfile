# syntax=docker/dockerfile:1.7

# Multi-stage build that keeps the build logic in repo scripts and uses Docker
# layers only for cache boundaries:
#
#   1. native-deps  → OS packages, pinned Rust toolchain, static C/C++ prefix
#   2. rust-build   → Cargo dependency warm-up, then the real application build
#   3. scratch      → only the shipped binary plus the runtime files it expects
#
# The native/static artifacts come from scripts/setup-static-build.sh, the Rust
# toolchain bootstrap comes from scripts/bootstrap-dev.sh, and the final static
# binary build comes from scripts/build-static.sh.

# ── Stage 1: native-deps ─────────────────────────────────────────────────────
#
# Everything here changes rarely and is expensive to rebuild:
#   - fresh-Ubuntu bootstrap packages
#   - pinned Rust toolchain
#   - static SRT/FFmpeg/x264/x265 prefix under .build/static/
#
# Docker intentionally does not duplicate package or toolchain logic here.
# scripts/bootstrap-dev.sh is the source of truth for what a fresh Ubuntu 24.04
# machine needs, and Docker consumes that script directly.
FROM ubuntu:24.04 AS native-deps

ENV DEBIAN_FRONTEND=noninteractive

WORKDIR /workspace

# Only copy inputs that affect bootstrap/native compilation so app source edits
# do not invalidate this expensive stage.
COPY package.json package-lock.json ./
COPY rust-toolchain.toml rust-toolchain.toml
COPY scripts/bootstrap-dev.sh scripts/bootstrap-dev.sh
COPY scripts/resource-limit scripts/resource-limit
COPY scripts/setup-static-build.sh scripts/setup-static-build.sh
COPY test/srt-bond-client.c test/srt-bond-client.c
COPY test/srt-bond-server.c test/srt-bond-server.c
COPY test/ffmpeg-capabilities.c test/ffmpeg-capabilities.c

RUN chmod +x scripts/bootstrap-dev.sh scripts/resource-limit scripts/setup-static-build.sh

# bootstrap-dev owns the fresh-Ubuntu dependency contract, including Node/npm
# plus npm ci for the committed frontend toolchain dependencies.
RUN scripts/bootstrap-dev.sh

ENV PATH="/root/.cargo/bin:${PATH}"

# ── Stage 2: frontend-build ──────────────────────────────────────────────────
#
# Frontend edits should have their own cache boundary. This stage reuses the
# Node/npm + node_modules state prepared by bootstrap-dev.sh, then rebuilds the
# generated browser assets under public/.
FROM native-deps AS frontend-build

WORKDIR /workspace

COPY public/ public/
COPY tsconfig.json tsconfig.json

RUN npm run build:frontend

# ── Stage 3: rust-build ──────────────────────────────────────────────────────
#
# This stage is split into two cache boundaries:
#   - manifest/config only + dummy src/main.rs → compile Cargo dependencies
#   - real src/ + built public/ assets         → relink just the app crate
#
# scripts/build-static.sh is used for both so Docker never re-implements the
# static-link flags or verification logic.
FROM native-deps AS rust-build

WORKDIR /workspace

COPY scripts/build-static.sh scripts/build-static.sh
RUN chmod +x scripts/build-static.sh

# Warm the release dependency graph without copying the real application code.
# The dummy main compiles the full dependency set into .build/static/cargo-target
# so ordinary src/ edits only need to rebuild our crate in the next layer.
COPY Cargo.toml Cargo.lock build.rs ./
COPY .cargo/ .cargo/
RUN mkdir -p benches src \
    && awk '/^\[\[bench\]\]$/ { in_bench = 1; next } in_bench && /^name = "/ { name = $0; sub(/^name = "/, "", name); sub(/"$/, "", name); printf "fn main() {}\\n" > ("benches/" name ".rs"); in_bench = 0 }' Cargo.toml \
    && printf 'fn main() {}\n' > src/main.rs
RUN scripts/resource-limit ./scripts/build-static.sh

# Inner-loop layer: copy the actual application sources, then bring in the
# built frontend assets from the frontend stage. Rust-only edits therefore skip
# frontend rebuilds, while frontend edits reuse the warmed Cargo dependency
# target directory above.
COPY src/ src/
COPY --from=frontend-build /workspace/public public
COPY --from=native-deps /workspace/public/bin/ffmpeg public/bin/ffmpeg
RUN scripts/resource-limit ./scripts/build-static.sh

# Build the minimal filesystem tree that the shipped binary expects at runtime.
# /tmp must remain writable and executable because the embedded FFmpeg binary is
# extracted there on startup; operators should still prefer --tmpfs /tmp:exec.
RUN mkdir -p \
        /runtime/data \
        /runtime/etc/ssl/certs \
        /runtime/media \
        /runtime/tmp/logs \
        /runtime/usr/share/zoneinfo \
    && cp -a /usr/share/zoneinfo/. /runtime/usr/share/zoneinfo/ \
    && cp -a /etc/localtime /runtime/etc/localtime \
    && cp /etc/ssl/certs/ca-certificates.crt /runtime/etc/ssl/certs/ca-certificates.crt \
    && printf 'restream:x:1000:1000:restream:/nonexistent:/sbin/nologin\n' > /runtime/etc/passwd \
    && printf 'restream:x:1000:\n' > /runtime/etc/group \
    && chmod 1777 /runtime/tmp \
    && chown -R 1000:1000 /runtime/data /runtime/media /runtime/tmp

# ── Stage 4: scratch runtime ─────────────────────────────────────────────────
#
# Runtime requirements:
#   /tmp    exec-enabled writable tmpfs for embedded FFmpeg extraction
#   /data   SQLite database persistence
#   /media  HLS/media persistence
#
# Example:
#   docker run -d \
#     --tmpfs /tmp:exec,mode=1777 \
#     -v restream-db:/data \
#     -v restream-media:/media \
#     -p 3030:3030 -p 1935:1935 -p 10080:10080/udp \
#     restream:scratch
FROM scratch

COPY --from=rust-build /runtime/ /
COPY --from=rust-build /workspace/.build/static/cargo-target/release/restream /restream

EXPOSE 3030 1935 10080/udp

USER 1000:1000

ENV RESTREAM_DB_PATH=/data/restream.db \
    RESTREAM_MEDIA_DIR=/media \
    RESTREAM_LOG_DIR=/tmp/logs

ENTRYPOINT ["/restream"]
