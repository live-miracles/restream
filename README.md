# Restream

A streaming control plane built on [MediaMTX](https://github.com/bluenviron/mediamtx). Takes RTMP ingest, manages stream keys and pipelines, and drives multiple FFmpeg output jobs to external platforms over RTMP, RTSP, SRT, and HLS upload endpoints via a browser dashboard.

## How It Works

1. A publisher pushes an RTMP stream to MediaMTX.
2. Restream's backend manages stream keys, pipelines, and output destinations in SQLite.
3. When an output is started, the backend probes the RTSP relay, spawns an FFmpeg process to pull and push to the destination, and tracks its health.
4. The browser polls `/config` (ETag-gated) and `/health` (live MediaMTX + DB aggregate) to render live status and metrics.

## Installation

### Prod Mode

The only two dependancies are node modules and MediaMTX. For the later you can get it from the [GitHub Releases](https://github.com/bluenviron/mediamtx/releases).
```sh
npm ci  # node modules
```

### Dev Mode (to enable testing)
For Linux host-mode workflows, `make deps` runs the preflight helper, installs Node.js dependencies, and downloads MediaMTX into `bin/mediamtx/`:

```sh
make deps           # production (no dev dependencies)
DEV=1 make deps     # development (with nodemon, prettier, tailwindcss)
```

Re-run `make deps` whenever `package.json` or `package-lock.json` changes. Commands that rely on dev dependencies, including `DEV=1 make run-host`, `make format`, and `make css`, require the `DEV=1 make deps` install.

`make deps` may invoke `apt-get`/`sudo` to install missing OS packages. It targets Debian/Ubuntu-style Linux hosts because it installs Linux packages and downloads a Linux MediaMTX binary.

If you only use Docker mode, skip `make deps` and go straight to `make run-docker`.

`make run-4x3` directly requires host `node`, host `ffmpeg`, Docker with the compose plugin for `nginx-rtmp`, and an already-running app stack. If that stack is running in host mode, it also depends on the `make deps` outputs (`node_modules/` and `bin/mediamtx/mediamtx`); a Docker-mode stack does not.

## Run Modes

### Prod Mode (on host)

MediaMTX executable can be placed in the root so it automatically takes the custom `mediamtx.yml` config.

```sh
npm start  # starts Node.js app (default port 3030)
./mediamtx  # Or double clicking on mediamtx.exe on Windows
```

### Dev Mode (on host)
```sh
npm run dev  # enables hot reload
npm run css-watch  # hot reload for css syles
  ```

### Docker Mode (all containers)

App, MediaMTX, and nginx-rtmp all run as containers. `pause`, `app`, and `mediamtx` share a pod-like network namespace so the app can keep using `localhost` for MediaMTX.

```sh
make run-docker
```

Docker mode does not require the host `node_modules/` tree or the host-downloaded `bin/mediamtx/mediamtx` binary.

See updated docs/configuration.md for more details.


## Logs

They will be stored in `log/mediamtx.log` (MediaMTX), `log/app.log` (Node app).

## Contribute

After cloning, run:

```sh
git config core.hooksPath .githooks
DEV=1 make deps
```

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
test/artifacts/     — Reproducible test scripts and session recordings
docker-compose.yml  — Full-stack compose (app + mediamtx + nginx-rtmp test sink)
```

SQLite runtime files are stored under `data/` (for example `data/data.db`).

The repository supports two runtime layouts:
- `make run-host`: `scripts/up.sh` launches MediaMTX and the Node app as host processes.
- `make run-docker`: `docker-compose.yml` launches `pause`, `app`, `mediamtx`, and `nginx-rtmp` as containers.

## Documentation

| Document | Description |
|---|---|
| [docs/architecture.md](docs/architecture.md) | System design, data model, call flows, deployment |
| [docs/frontend-modules.md](docs/frontend-modules.md) | Frontend ES module conventions and troubleshooting |
| [docs/api-reference.md](docs/api-reference.md) | All REST endpoints with request/response shapes |
| [docs/health-mapping.md](docs/health-mapping.md) | How input/output health statuses are derived |
| [docs/configuration.md](docs/configuration.md) | All environment variables and config file options |
| [docs/deployment-host.md](docs/deployment-host.md) | Production deployment guide on Linux hosts without containers |

## Links

1. MediaMTX: [github.com/bluenviron/mediamtx](https://github.com/bluenviron/mediamtx)
2. Nginx MLS: [github.com/live-miracles/MLS](https://github.com/live-miracles/MLS)
3. Nginx MLS demo: [youtu.be/yzXuirkmexo](https://youtu.be/yzXuirkmexo)
4. Go-MLS: [github.com/krsna1729/go-mls](https://github.com/krsna1729/go-mls)
5. Go-MLS demo: [www.youtube.com/watch?v=x2x3uSCAX4M](https://www.youtube.com/watch?v=x2x3uSCAX4M)
6. Daisy UI: [daisyui.com/](https://daisyui.com/)
