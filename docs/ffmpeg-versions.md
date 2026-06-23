# FFmpeg Version Configuration

This document explains how to build restream with different FFmpeg versions (6.x, 7.x, 8.x).

## Quick Start

To switch FFmpeg versions, run the version selector script:

```bash
# Build with FFmpeg 6.1.5 (default, LTS)
./scripts/set-ffmpeg-version.sh 6.1

# Build with FFmpeg 7.0 (stable)
./scripts/set-ffmpeg-version.sh 7

# Build with FFmpeg 8.0 (latest)
./scripts/set-ffmpeg-version.sh 8
```

This updates both:
1. **External FFmpeg** (subprocess binary): via `FFMPEG_VERSION` in `scripts/setup-static-build.sh`
2. **Internal FFmpeg** (library): via `ffmpeg-next` crate version in `Cargo.toml`

## Build Workflow

### 1. Select FFmpeg Version
```bash
./scripts/set-ffmpeg-version.sh 8    # Example: switch to FFmpeg 8.x
```

### 2. Build Static FFmpeg Binary
```bash
./scripts/setup-static-build.sh
```
This downloads, patches, and compiles FFmpeg from source with optimizations:
- `x86-64-v3` baseline (AVX2 on Haswell 2013+)
- `-O3 -ffast-math` optimizations
- x86 ASM enabled via nasm
- Minimal configuration (only codec/filter/protocol essentials)

### 3. Build Rust Binary
```bash
cargo build --release
```
Compiles with:
- Matching `ffmpeg-next` crate version
- LTO enabled
- x86-64-v3 CPU baseline with runtime fallback

### 4. (Optional) Embed in Binary
If using Scenario 2 (lazy embedding), copy the binary to `public/bin/ffmpeg`:
```bash
mkdir -p public/bin
cp .build/static/prefix/bin/ffmpeg public/bin/ffmpeg
```

## Version Information

| Version | Release Date | Status | LTS | Notes |
|---------|--------------|--------|-----|-------|
| **6.1.5** | 2023-10-29 | Maintained | ✅ | Current default, stable |
| **7.0** | 2024-07-08 | Stable | ❌ | Latest 7.x, recommended for new deployments |
| **8.0** | 2025-01-15 | Latest | ❌ | Latest major, cutting-edge features |

### Compatibility Matrix

| External | Internal | Rust Binary | Notes |
|----------|----------|-------------|-------|
| 6.1.5 | ffmpeg-next 6.0 | ✅ Current | Tested & stable |
| 7.0 | ffmpeg-next 7.0 | ✅ Supported | Should work (not yet tested) |
| 8.0 | ffmpeg-next 8.0 | ⚠ Beta | Latest, may have edge cases |

## Breaking Changes

### FFmpeg 6.x → 7.x
- Minor codec API changes (some deprecated options removed)
- Transcoding logic remains stable
- Risk: **Low**

### FFmpeg 7.x → 8.x
- Profile/level handling changes
- Some encoding option deprecated
- Risk: **Medium** (may need codec option adjustments)

## Environment Variables

When building, you can optionally set these directly (though the script is recommended):

```bash
# External FFmpeg version (binary)
export FFMPEG_VERSION=n8.0

# Build root location
export RESTREAM_BUILD_ROOT=/custom/build/path

# Force rebuild
export RESTREAM_REBUILD_NATIVE=1

# Then build
./scripts/setup-static-build.sh
```

## Manual Version Update

If you prefer to configure manually:

### Update External FFmpeg Version
Edit `scripts/setup-static-build.sh`:
```bash
FFMPEG_VERSION="${FFMPEG_VERSION:-n8.0}"    # Change to desired version
```

### Update Internal FFmpeg Version
Edit `Cargo.toml`:
```toml
ffmpeg-next = { version = "8.0", default-features = false, features = ["codec", "filter", "format", "software-resampling", "software-scaling"] }
```

## Troubleshooting

### "FFmpeg binary not found in PATH"
If external transcoding fails:
```bash
# Verify binary was built
ls -lh .build/static/prefix/bin/ffmpeg

# Or specify explicitly
export FFMPEG_BIN_PATH=/path/to/ffmpeg
```

### "ffmpeg-next crate compilation failed"
Verify the crate version matches your FFmpeg binary:
```bash
# Check what version you're trying to use
grep "ffmpeg-next" Cargo.toml
grep "FFMPEG_VERSION" scripts/setup-static-build.sh
```

### Transcoding failures with new FFmpeg version
- Check FFmpeg version compatibility: `ffmpeg -version`
- Verify codec support: `ffmpeg -codecs | grep h264`
- Enable verbose logging for transcoder (see [observability.md](../observability.md))

## See Also

- [High-Performance Data Path](../high-performance-data-path.md) — CPU optimization strategy
- [Media Pipeline](../media-pipeline.md) — Architecture and threading model
- [Configuration](./configuration.md) — Other environment variables
