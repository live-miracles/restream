# FFmpeg Version Configuration

This document explains how the static FFmpeg version is selected for restream
release builds.

## Quick Start

The current static build default is FFmpeg `n8.1.2`, matching the
`ffmpeg-next = "8.0"` Rust binding family in `Cargo.toml`.

```bash
scripts/resource-limit ./scripts/setup-static-build.sh
```

To test a different FFmpeg tag, override `FFMPEG_VERSION` for the external
FFmpeg library and embedded subprocess binary, then update `ffmpeg-next` in
`Cargo.toml` only when crossing Rust binding families.

## Build Workflow

### 1. Select FFmpeg Version
```bash
FFMPEG_VERSION=n8.1.2 scripts/resource-limit ./scripts/setup-static-build.sh
```

### 2. Build Static FFmpeg Binary
```bash
scripts/resource-limit ./scripts/setup-static-build.sh
```
This downloads, patches, and compiles FFmpeg from source with optimizations:
- `x86-64-v3` baseline (AVX2 on Haswell 2013+)
- `-O3 -ffast-math` optimizations
- x86 ASM enabled via nasm
- Minimal configuration (only codec/filter/protocol essentials, including
  static x264/x265 encoders)

### 3. Build Rust Binary
```bash
scripts/resource-limit ./scripts/build-static.sh
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

| Version | Role | Notes |
|---------|------|-------|
| **8.1.2** | Current default | Static build tag `n8.1.2` |
| **8.0** | Binding family | Rust crate family used by `ffmpeg-next = "8.0"` |
| **6.1.5** | Historical validation | Previous static release validation baseline |

### Compatibility Matrix

| External | Internal | Rust Binary | Notes |
|----------|----------|-------------|-------|
| 8.1.2 | ffmpeg-next 8.0 | ✅ Current | Current build-script default |
| 6.1.5 | ffmpeg-next 6.0 | Historical | Previous validation baseline |

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

When building, you can optionally set these directly on the resource-limited script:

```bash
# External FFmpeg version (library and embedded binary)
export FFMPEG_VERSION=n8.1.2

# Build root location
export RESTREAM_BUILD_ROOT=/custom/build/path

# Force rebuild
export RESTREAM_REBUILD_NATIVE=1

# Then build
scripts/resource-limit ./scripts/setup-static-build.sh
```

## Manual Version Update

If you prefer to configure manually:

### Update External FFmpeg Version
Edit `scripts/setup-static-build.sh`:
```bash
FFMPEG_VERSION="${FFMPEG_VERSION:-n8.1.2}"    # Change to desired tag
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
