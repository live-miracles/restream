# Restream

A streaming control plane built on [MediaMTX](https://github.com/bluenviron/mediamtx). Takes RTMP ingest, manages stream keys and pipelines, and drives multiple FFmpeg output jobs to external platforms (YouTube, Facebook, etc.) via a browser dashboard.

## How It Works

1. A publisher pushes an RTMP stream to MediaMTX.
2. Restream's backend manages stream keys, pipelines, and output destinations in SQLite.
3. When an output is started, the backend probes the RTSP relay, spawns an FFmpeg process to pull and push to the destination, and tracks its health.
4. The browser polls `/config` (ETag-gated) and `/health` (live MediaMTX + DB aggregate) to render live status and metrics.

## Project Structure

```
src/
  index.js          — Express app composition and dependency wiring
  api/
    config.js       — Config API registration and ETag helpers
    outputs.js      — Output request validation and API registration
    pipelines.js    — Pipeline CRUD API registration
    metrics.js      — Host CPU, memory, disk, and network metrics API
  services/
    health.js       — Health snapshot collection and aggregation service
    outputs.js      — Output start/stop lifecycle orchestration
    recovery.js     — Output retry/backoff policy service
    bootstrap.js    — App startup and recurring maintenance timers
  utils/
    app.js                — Shared app helpers (logging, validation, HTTP error shaping)
    ffmpeg.js             — FFmpeg arg/progress/media parsing helpers
    health-connection.js — Connection/session indexing and reader correlation helpers
    health-media.js      — Media/status calculation helpers for health snapshots
    health-state.js      — Health snapshot/state assembly helpers
    mediamtx.js           — MediaMTX URL/tag helper utilities
    retry.js              — Retry/backoff helper utilities
  db/
    index.js        — SQLite query helpers and data access methods (data/data.db)
    schema.js       — SQLite schema setup and migration bootstrap
  config/
    index.js        — Config loader with sanitization
    restream.json   — App config: host, serverName, pipelinesLimit, outLimit
public/
  index.html        — Dashboard SPA shell
  stream-keys.html  — Stream key management page
  js/
    core/
      api.js            — All API calls (relative paths, never direct to MediaMTX)
      state.js          — Shared mutable UI state (config/health/pipelines/metrics)
      pipeline.js       — parsePipelinesInfo(): merges config + health into view model
      utils.js          — Shared utilities: formatTime, setServerConfig, copyData
    history/
      state.js          — Shared state + polling constants for history modals
      render.js         — Shared history rendering and timeline helper utilities
      controller.js     — History modal controller and polling orchestration
    features/
      dashboard.js      — Event handlers, modals, polling orchestration
      editor.js         — Output/pipeline modal edit interactions
      pipeline-view.js  — Pipeline detail rendering helpers
      render.js         — DOM rendering: pipeline cards, stats, output tables
      metrics.js        — System metrics fetch + render helpers
      stream-keys-page.js — Stream key page interactions
  output.css        — Compiled Tailwind + DaisyUI (do not edit manually)
input.css           — Tailwind CSS source (compile with `make css`)
docs/               — Architecture, API reference, health mapping, config guide
infra/              — Deployment/runtime configs (MediaMTX, nginx-rtmp test sink)
test/artifacts/     — Reproducible test scripts and session recordings
docker-compose.yml  — Full-stack compose (app + mediamtx + nginx-rtmp test sink)
```

SQLite runtime files are stored under `data/` (for example `data/data.db`).

The compose file uses profiles:
- `host`: MediaMTX in Docker with app on host (`make run-host`)
- `container`: app + MediaMTX share a pod-like namespace via pause (`make run-docker`)

## Documentation

| Document | Description |
|---|---|
| [docs/architecture.md](docs/architecture.md) | System design, data model, call flows, deployment |
| [docs/frontend-modules.md](docs/frontend-modules.md) | Frontend ES module conventions and troubleshooting |
| [docs/api-reference.md](docs/api-reference.md) | All REST endpoints with request/response shapes |
| [docs/health-mapping.md](docs/health-mapping.md) | How input/output health statuses are derived |
| [docs/configuration.md](docs/configuration.md) | All environment variables and config file options |
| [docs/deployment-host.md](docs/deployment-host.md) | Production deployment guide on Linux hosts without containers |

## Installation

All environments use `make deps` to install Node.js dependencies and download MediaMTX:

```sh
make deps           # production (no dev dependencies)
DEV=1 make deps     # development (with nodemon, prettier, tailwindcss)
```

## Run Modes


### Host Mode (no Docker)


Node.js app and MediaMTX both run as host processes. MediaMTX is managed by the project scripts and downloaded automatically if missing.

```sh
make run-host
```

**Hot-reload:**
- By default, `make run-host` runs the app with plain `node`.
- To enable hot-reload (auto-restart on file changes), run with `DEV=1`:
  ```sh
  DEV=1 make run-host
  ```
- This uses `npm run dev` (nodemon) instead of `node` for the app process.


**Details:**
- MediaMTX is started as a background process by `scripts/up.sh`.
- Logs: `log/mediamtx.log` (MediaMTX), `log/app.log` (Node app)
- PID files: `.mediamtx.pid` (MediaMTX), `.app.pid` (Node app)
- To stop all processes and clean up, run:
  ```sh
  make down
  ```

**Optional: RTMP test sink**
- nginx-rtmp runs in DEV mode for local RTMP testing:
  ```sh
  DEV=1 make run-host
  ```


### Docker Mode (all containers)

App, MediaMTX, and nginx-rtmp all run as containers. Use this for a fully containerized stack.

```sh
make run-docker
```
See updated docs/configuration.md for more details.

## Contribute

After cloning, run:

```sh
git config core.hooksPath .githooks
DEV=1 make deps
```

## Links

1. MediaMTX: [github.com/bluenviron/mediamtx](https://github.com/bluenviron/mediamtx)
2. Nginx MLS: [github.com/live-miracles/MLS](https://github.com/live-miracles/MLS)
3. Nginx MLS demo: [youtu.be/yzXuirkmexo](https://youtu.be/yzXuirkmexo)
4. Go-MLS: [github.com/krsna1729/go-mls](https://github.com/krsna1729/go-mls)
5. Go-MLS demo: [www.youtube.com/watch?v=x2x3uSCAX4M](https://www.youtube.com/watch?v=x2x3uSCAX4M)
6. Daisy UI: [daisyui.com/](https://daisyui.com/)
