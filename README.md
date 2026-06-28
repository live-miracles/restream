# Restream

Restream is a Rust live-stream routing service. One process owns the dashboard,
API, SQLite state, RTMP/SRT ingest, RTMP/SRT egress, HLS preview, recording,
and the media-stage orchestration around transcoding.

This README is intentionally short. It should get a new developer from clone to
useful context without making them read the whole system on day one.

## Start Here

On Debian/Ubuntu, the fastest setup path is:

```sh
./scripts/bootstrap-dev.sh
scripts/resource-limit ./scripts/build-native.sh
cargo run
```

Then open `http://localhost:3030`.

Default ports:

- `3030` for the dashboard and API
- `1935` for RTMP ingest/play
- `10080` for SRT ingest/read

First-run dashboard password: `admin`

## Running A Built Binary

If you already have a release binary produced by
`scripts/resource-limit ./scripts/build-static.sh`, you can run it directly:

```sh
./restream
```

That static release artifact does not require FFmpeg, SRT, or other shared
runtime dependencies to be installed on the host. The source-build and `cargo`
paths are different: they do require the build dependencies described in
[docs/development.md](docs/development.md).

## Daily Loop

Most backend work stays in this loop:

```sh
scripts/resource-limit ./scripts/build-native.sh
scripts/resource-limit cargo test
scripts/resource-limit cargo clippy
cargo fmt
```

If you edit frontend assets:

```sh
npm run build:frontend
```

## Codebase Map

- `src/api.rs` and `src/lib.rs`: app startup, routes, runtime wiring
- `src/media/`: ingest, egress, mux/demux, ring buffers, HLS, transcoding
- `src/domain/`: persisted models and business logic
- `src/planner/`: pipeline planning/orchestration helpers
- `public/`: dashboard assets
- `tests/` and `test/`: integration tests and live test harness
- `scripts/`: bootstrap and native build helpers

## Read Next

- [Developer Guide](docs/development.md): setup, inner loop, tests, benchmarks, static build
- [Architecture](docs/architecture.md): runtime shape and major moving parts
- [Configuration](docs/configuration.md): env vars, ports, paths, persisted settings
- [API Reference](docs/api-reference.md): route-level behavior
- [Testing](docs/testing.md): verification strategy and live test entry points
- [Observability](docs/observability.md): health, diagnostics, telemetry
- [Rewrite Status](REWRITE-STATUS.md): current implementation status and open gaps

## Expectations

The repository includes deep reference docs because the runtime is doing real
media work, but you do not need all of them to start contributing. Begin with
the developer guide and architecture doc, then pull in the more specific docs
only when your change touches those areas.
