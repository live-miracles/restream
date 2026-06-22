#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_ROOT="${RESTREAM_BUILD_ROOT:-$ROOT/.build/static}"
TOOLS="$BUILD_ROOT/tools"
SOURCES="$BUILD_ROOT/src"
PREFIX="$BUILD_ROOT/prefix"
STAMPS="$BUILD_ROOT/stamps"

SRT_VERSION="${SRT_VERSION:-v1.5.5}"
FFMPEG_VERSION="${FFMPEG_VERSION:-n6.1.5}"
X264_COMMIT="${X264_COMMIT:-b35605ace3ddf7c1a5d67a2eb553f034aef41d55}"

mkdir -p "$TOOLS" "$SOURCES" "$PREFIX" "$STAMPS"

if command -v apt-get >/dev/null; then
    missing_packages=()
    command -v cc >/dev/null || missing_packages+=(build-essential)
    command -v git >/dev/null || missing_packages+=(git)
    command -v cmake >/dev/null || missing_packages+=(cmake)
    command -v ninja >/dev/null || missing_packages+=(ninja-build)
    command -v nasm >/dev/null || missing_packages+=(nasm)
    command -v pkg-config >/dev/null || missing_packages+=(pkg-config)
    command -v perl >/dev/null || missing_packages+=(perl)
    command -v python3 >/dev/null || missing_packages+=(python3)
    dpkg-query -W libssl-dev >/dev/null 2>&1 || missing_packages+=(libssl-dev)

    if ((${#missing_packages[@]})); then
        if [[ "$(id -u)" -eq 0 ]]; then
            apt-get update
            apt-get install -y "${missing_packages[@]}"
        elif command -v sudo >/dev/null; then
            sudo apt-get update
            sudo apt-get install -y "${missing_packages[@]}"
        else
            echo "missing build packages: ${missing_packages[*]}" >&2
            echo "install them with apt, or run this script as root" >&2
            exit 1
        fi
    fi
elif ! command -v cmake >/dev/null || ! command -v ninja >/dev/null; then
    python3 -m pip install --upgrade --target "$TOOLS" cmake ninja
    export PYTHONPATH="$TOOLS${PYTHONPATH:+:$PYTHONPATH}"
    export PATH="$TOOLS/bin:$PATH"
fi

for command in git cc c++ make perl python3 pkg-config cmake ninja nasm sha256sum; do
    command -v "$command" >/dev/null || {
        echo "missing required command after setup: $command" >&2
        exit 1
    }
done

clone_tag() {
    local url="$1"
    local tag="$2"
    local destination="$3"
    if [[ ! -d "$destination/.git" ]]; then
        git clone --depth 1 --branch "$tag" "$url" "$destination"
    fi
}

clone_commit() {
    local url="$1"
    local commit="$2"
    local destination="$3"
    if [[ ! -d "$destination/.git" ]]; then
        git clone "$url" "$destination"
        git -C "$destination" checkout --detach "$commit"
    fi
}

clone_tag https://github.com/Haivision/srt.git "$SRT_VERSION" "$SOURCES/srt"
clone_tag https://github.com/FFmpeg/FFmpeg.git "$FFMPEG_VERSION" "$SOURCES/ffmpeg"
clone_commit https://code.videolan.org/videolan/x264.git "$X264_COMMIT" "$SOURCES/x264"

fingerprint() {
    sha256sum | cut -d' ' -f1
}

stamp_matches() {
    local stamp="$1"
    local expected="$2"
    [[ "${RESTREAM_REBUILD_NATIVE:-0}" != "1" &&
        -f "$stamp" &&
        "$(cat "$stamp")" == "$expected" ]]
}

write_stamp() {
    local stamp="$1"
    local value="$2"
    printf '%s\n' "$value" >"$stamp"
}

SRT_FINGERPRINT="$(
    {
        git -C "$SOURCES/srt" rev-parse HEAD
        c++ --version | head -n 1
        cmake --version | head -n 1
        pkg-config --modversion openssl
        printf '%s\n' \
            "-DCMAKE_BUILD_TYPE=Release" \
            "-DCMAKE_INSTALL_PREFIX=$PREFIX" \
            "-DENABLE_BONDING=ON" \
            "-DENABLE_SHARED=OFF" \
            "-DENABLE_STATIC=ON" \
            "-DENABLE_APPS=OFF" \
            "-DENABLE_TESTING=OFF" \
            "-DUSE_ENCLIB=openssl" \
            "-DSRT_USE_OPENSSL_STATIC_LIBS=ON" \
            "-DUSE_OPENSSL_PC=ON"
    } | fingerprint
)"
SRT_STAMP="$STAMPS/srt"
if ! stamp_matches "$SRT_STAMP" "$SRT_FINGERPRINT" ||
    [[ ! -f "$PREFIX/lib/libsrt.a" ]]; then
    cmake -S "$SOURCES/srt" -B "$BUILD_ROOT/srt-build" -G Ninja \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX="$PREFIX" \
        -DENABLE_BONDING=ON \
        -DENABLE_SHARED=OFF \
        -DENABLE_STATIC=ON \
        -DENABLE_APPS=OFF \
        -DENABLE_TESTING=OFF \
        -DUSE_ENCLIB=openssl \
        -DSRT_USE_OPENSSL_STATIC_LIBS=ON \
        -DUSE_OPENSSL_PC=ON
    cmake --build "$BUILD_ROOT/srt-build" --parallel "${BUILD_JOBS:-$(nproc)}"
    cmake --install "$BUILD_ROOT/srt-build"
    write_stamp "$SRT_STAMP" "$SRT_FINGERPRINT"
else
    echo "Using cached SRT build."
fi

for helper in server client; do
    source_file="$ROOT/test/srt-bond-$helper.c"
    output_file="$PREFIX/bin/restream-srt-bond-$helper"
    if [[ ! -x "$output_file" ||
        "$source_file" -nt "$output_file" ||
        "$PREFIX/lib/libsrt.a" -nt "$output_file" ]]; then
        cc -O2 -I"$PREFIX/include" "$source_file" \
            "$PREFIX/lib/libsrt.a" -lstdc++ -lssl -lcrypto -ldl -lpthread -lm \
            -o "$output_file"
    fi
done

X264_FINGERPRINT="$(
    {
        git -C "$SOURCES/x264" rev-parse HEAD
        cc --version | head -n 1
        nasm -v
        printf '%s\n' \
            "--prefix=$PREFIX" \
            --enable-static \
            --enable-pic \
            --disable-opencl \
            --disable-cli
    } | fingerprint
)"
X264_STAMP="$STAMPS/x264"
if ! stamp_matches "$X264_STAMP" "$X264_FINGERPRINT" ||
    [[ ! -f "$PREFIX/lib/libx264.a" ]]; then
    pushd "$SOURCES/x264" >/dev/null
    ./configure \
        --prefix="$PREFIX" \
        --enable-static \
        --enable-pic \
        --disable-opencl \
        --disable-cli
    make -j"${BUILD_JOBS:-$(nproc)}"
    make install
    popd >/dev/null
    write_stamp "$X264_STAMP" "$X264_FINGERPRINT"
else
    echo "Using cached x264 build."
fi

FFMPEG_FINGERPRINT="$(
    {
        git -C "$SOURCES/ffmpeg" rev-parse HEAD
        cc --version | head -n 1
        nasm -v
        printf '%s\n' "$X264_FINGERPRINT" \
            "--prefix=$PREFIX" \
            --pkg-config-flags=--static \
            --enable-static \
            --disable-shared \
            --enable-pic \
            --disable-programs \
            --disable-doc \
            --disable-debug \
            --disable-autodetect \
            --disable-network \
            --enable-x86asm \
            --disable-everything \
            --enable-gpl \
            --enable-libx264 \
            --enable-avcodec \
            --enable-avformat \
            --enable-avfilter \
            --enable-swscale \
            --enable-swresample \
            --enable-protocol=file,pipe \
            --enable-demuxer=mpegts,flv,matroska,mov,aac,h264,hevc \
            --enable-muxer=mpegts,matroska,flv \
            --enable-decoder=h264,hevc,aac,mp3,ac3,eac3 \
            --enable-encoder=aac,ac3,libx264 \
            --enable-parser=h264,hevc,aac,ac3,mpegaudio \
            --enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,aac_adtstoasc \
            --enable-filter=scale,crop,transpose,format,aformat,aresample,pan,volume,null,anull
    } | fingerprint
)"
FFMPEG_STAMP="$STAMPS/ffmpeg"
if ! stamp_matches "$FFMPEG_STAMP" "$FFMPEG_FINGERPRINT" ||
    [[ ! -f "$PREFIX/lib/libavcodec.a" ]]; then
    pushd "$SOURCES/ffmpeg" >/dev/null
    PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" ./configure \
        --prefix="$PREFIX" \
        --pkg-config-flags=--static \
        --enable-static \
        --disable-shared \
        --enable-pic \
        --disable-programs \
        --disable-doc \
        --disable-debug \
        --disable-autodetect \
        --disable-network \
        --enable-x86asm \
        --disable-everything \
        --enable-gpl \
        --enable-libx264 \
        --enable-avcodec \
        --enable-avformat \
        --enable-avfilter \
        --enable-swscale \
        --enable-swresample \
        --enable-protocol=file,pipe \
        --enable-demuxer=mpegts,flv,matroska,mov,aac,h264,hevc \
        --enable-muxer=mpegts,matroska,flv \
        --enable-decoder=h264,hevc,aac,mp3,ac3,eac3 \
        --enable-encoder=aac,ac3,libx264 \
        --enable-parser=h264,hevc,aac,ac3,mpegaudio \
        --enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,aac_adtstoasc \
        --enable-filter=scale,crop,transpose,format,aformat,aresample,pan,volume,null,anull

    if ! grep -q '^#define HAVE_X86ASM 1$' config.h; then
        echo "FFmpeg configured without standalone x86 assembly" >&2
        exit 1
    fi

    make -j"${BUILD_JOBS:-$(nproc)}"
    make install
    popd >/dev/null
    write_stamp "$FFMPEG_STAMP" "$FFMPEG_FINGERPRINT"
else
    echo "Using cached FFmpeg build."
fi

CAPABILITIES="$PREFIX/bin/restream-ffmpeg-capabilities"
if [[ ! -x "$CAPABILITIES" ||
    "$ROOT/test/ffmpeg-capabilities.c" -nt "$CAPABILITIES" ||
    "$PREFIX/lib/libavcodec.a" -nt "$CAPABILITIES" ||
    "$PREFIX/lib/libavutil.a" -nt "$CAPABILITIES" ]]; then
    PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" cc -O2 -static \
        $(PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" pkg-config --cflags libavcodec libavutil) \
        "$ROOT/test/ffmpeg-capabilities.c" \
        $(PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" pkg-config --static --libs libavcodec libavutil) \
        -o "$CAPABILITIES"
fi

NATIVE_BUILD_ID="$(
    sha256sum \
        "$PREFIX/lib/libsrt.a" \
        "$PREFIX/lib/libx264.a" \
        "$PREFIX/lib/libavcodec.a" \
        "$PREFIX/lib/libavformat.a" \
        "$PREFIX/lib/libavfilter.a" \
        "$PREFIX/lib/libswscale.a" \
        "$PREFIX/lib/libswresample.a" \
        "$PREFIX/lib/libavutil.a" |
        sha256sum |
        cut -d' ' -f1
)"

cat >"$BUILD_ROOT/env.sh" <<EOF
export RESTREAM_BUILD_ROOT="$BUILD_ROOT"
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig"
export RESTREAM_STATIC_SRT=1
export RESTREAM_STATIC_FFMPEG=1
export RESTREAM_FULLY_STATIC=1
export RESTREAM_NATIVE_BUILD_ID="$NATIVE_BUILD_ID"
export CARGO_TARGET_DIR="$BUILD_ROOT/cargo-target"
EOF

echo
echo "Static build environment is ready."
echo "Build with: ./scripts/build-static.sh"
echo "Faster iteration: RESTREAM_BUILD_PROFILE=fast-release ./scripts/build-static.sh"
echo "Force native rebuild: RESTREAM_REBUILD_NATIVE=1 ./scripts/setup-static-build.sh"
echo "Environment: $BUILD_ROOT/env.sh"
