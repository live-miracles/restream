# CLAUDE.md

## Build & Test Commands
Standard cargo commands (`cargo build`, `cargo test`, `cargo fmt`, `cargo clippy`) work as expected.
- **Frontend Compile**: `npx tsc -p tsconfig.frontend.json` (Always edit `public/ts/` - never the generated `public/js/`).
- **Tailwind Build**: `npx tailwindcss -i public/input.css -o public/output.css`
- **Integration Tests**: `./test/run-2x3.sh` (Requires isolated network namespace to avoid port conflicts).
  - *Docker alternative*:
    ```sh
    docker run --rm \
      -v "$(pwd)":/app -w /app \
      rust:1-bookworm bash -c '
        apt-get update -qq && apt-get install -y -qq ffmpeg jq libavformat-dev libavcodec-dev libavutil-dev libswresample-dev libswscale-dev libavfilter-dev libavdevice-dev pkg-config clang > /dev/null 2>&1
        cargo build --release
        ./target/release/restream &
        until curl -sf http://localhost:3030/healthz > /dev/null 2>&1; do sleep 1; done
        ./test/run-2x3.sh
      '
    ```
  - *Linux Unshare (lighter)*:
    ```sh
    unshare --net --map-root-user bash -c '
      ip link set lo up
      ./target/release/restream &
      until curl -sf http://localhost:3030/healthz > /dev/null 2>&1; do sleep 1; done
      ./test/run-2x3.sh
    '
    ```

## Key Constraints
- **Ports**: Defaults are HTTP=3030, RTMP=1935, SRT=10080. Override via `RESTREAM_HTTP_PORT`, `RESTREAM_RTMP_PORT`, `RESTREAM_SRT_PORT` env vars.
- **Database**: SQLite (no migrations; schema created with `CREATE TABLE IF NOT EXISTS` at startup).
- **HLS Segmenter**: In-memory storage only (`VecDeque<Bytes>` inside `HlsStore`), no disk I/O.
- **Frontend Assets**: Statically embedded in the binary via `rust-embed` (disk-first fallback in dev).

## Hotpath & Performance Guidelines
When modifying hotpath code (such as files under the [src/media/](src/media/) directory or other performance-critical components):

1. **Always Benchmark Before & After**: Run the relevant Criterion benchmarks (e.g., `cargo bench --bench <bench_name>` using the appropriate suite under [benches/](benches/)) before making changes to establish a baseline, and run them again after to verify the performance impact.
2. **Adhere to High-Performance Principles**: Follow the principles outlined in [high-performance-data-path.md](docs/high-performance-data-path.md):
   - **Burst-Oriented Design**: Change the unit of work from one packet, one lookup, and one wakeup to a bounded burst (e.g., `push_batch` / `pull_burst`).
   - **Direct Hot Handles**: Cache atomic variables, rings, and handles. Avoid registry map/lock lookups on the packet loop.
   - **Run-to-Completion**: Perform packet-local operations (parse, classify, normalize, account, publish) on the worker thread itself instead of spawning new tasks/channels.
   - **Zero-Copy Optimization**: Avoid payload copies. Transfer `Bytes`/`BytesMut` ownership or reuse vectors where possible.
   - **No Regressions**: Verify correctness gates (PTS/DTS ordering, keyframe alignment, and format compliance via probes/ffprobe) after any change. Performance must not come at the cost of protocol correctness.
