# Restream

A streaming control plane built on [MediaMTX](https://github.com/bluenviron/mediamtx). Takes RTMP ingest, manages stream keys and pipelines, and drives multiple FFmpeg output jobs to external platforms over RTMP, RTSP, SRT, and HLS upload endpoints via a browser dashboard.

## How It Works

1. A publisher pushes an RTMP stream to MediaMTX.
2. Restream's backend manages stream keys, pipelines, and output destinations in SQLite.
3. When an output is started, the backend probes the RTSP relay, spawns an FFmpeg process to pull and push to the destination, and tracks its health.
4. The browser polls `/config` (ETag-gated) and `/health` (live MediaMTX + DB aggregate) to render live status and metrics.

## Project Structure

```
src/
  index.js          — Express app composition and dependency wiring
  bootstrap.js      — Startup ordering, runtime registries, and shutdown behavior
  config.js         — Config loading, normalization, and public-config shaping
  db.js             — SQLite schema, migrations, and persistent query layer
  health.js         — Live health monitor and `/health` route registration
  health-compute.js — Pure health/media parsing and snapshot helpers
  http.js           — Shared Express response helpers
  outputs.js        — Output start/stop lifecycle orchestration
  pipeline-runtime-state.js — Shared input transition history and recovery coordination state
  preview.js        — HLS preview proxy routes
  recovery.js       — Output retry/restart policy and desired-state orchestration
  recovery-helpers.js — Pure process/retry/restart-state helpers for recovery flows
  routes-output.js  — Output CRUD, control, and history routes
  routes-pipeline.js — `/config`, pipeline CRUD, and stream-key routes
  utils.js          — Shared validation, logging, MediaMTX, FFmpeg, and redaction helpers
  config/
    restream.json   — App config: host, serverName, pipelinesLimit, outLimit
public/
  index.html        — Dashboard SPA shell
  stream-keys.html  — Stream key management page
  js/
    client.js        — Shared mutable UI state plus API/ETag/polling helpers
    history.js       — Output/pipeline history modal control, polling, and rendering
    pipeline.js      — parsePipelinesInfo(): merges config + health into view model
    utils.js         — Shared UI utilities: formatters, DOM helpers, masking, copy helpers
    history/
      classify.mjs      — Pure history classification helpers
    features/
      dashboard-actions.js — Dashboard refresh/baseline/visibility coordinator callbacks
      dashboard-entry.js — Dashboard page composition root
      dashboard.js      — Dashboard polling orchestration and snapshot reconciliation
      dashboard-view.js — Dashboard DOM rendering, metrics, and banner state
      editor.js         — Output/pipeline modal edit interactions
      input-preview-state.mjs — Pure preview URL and runtime helpers
      output-url.js     — Pure output URL parse/build helpers
      pipeline-view-actions.js — Action adapter for pipeline detail/history/editor handlers
      stream-keys-page.js — Stream key page interactions
      stream-keys-state.mjs — Stream-key page pure state/render helpers
      publisher-quality.js — Publisher quality classification helpers
      view.js           — Pipeline detail, ingest detail, preview, and output column rendering
  output.css        — Compiled Tailwind + DaisyUI (do not edit manually)
input.css           — Tailwind CSS source (compile with `make css`)
docs/               — Architecture, API reference, health mapping, config guide
mediamtx.yml        — MediaMTX runtime config used by host mode and Docker mode
test/nginx-rtmp.conf — nginx-rtmp validation sink config used by Docker workflows
test/artifacts/     — Reproducible test scripts and session recordings
test/frontend/      — Browserless dashboard smoke tests
docker-compose.yml  — Optional container stack for app, MediaMTX, and the nginx-rtmp test sink
```

SQLite runtime files are stored under `data/` (for example `data/data.db`).

The main local runtime entry point is `npm start` or `npm run dev` from `src/index.js`.
If you specifically want the disposable container stack, `make run-docker` still brings up the
app, MediaMTX, and `nginx-rtmp` services defined in `docker-compose.yml`.

Host mode remains the default edit/test path. The tracked repo still does not ship a supported
`make run-host` / `make down` wrapper, so if you are not using `make run-docker`, start MediaMTX
separately using your own supervisor or a direct local command such as
`bin/mediamtx/mediamtx mediamtx.yml`.

## Documentation

| Document | Description |
|---|---|
| [docs/onboarding.md](docs/onboarding.md) | Reading order, mental model, and first-day debugging guide for new contributors |
| [docs/backend-services.md](docs/backend-services.md) | Backend service ownership, startup wiring, and module-selection guide |
| [docs/architecture.md](docs/architecture.md) | System design, data model, call flows, deployment |
| [docs/frontend-modules.md](docs/frontend-modules.md) | Frontend ES module conventions and troubleshooting |
| [docs/api-reference.md](docs/api-reference.md) | All REST endpoints with request/response shapes |
| [docs/health-mapping.md](docs/health-mapping.md) | How input/output health statuses are derived |
| [docs/configuration.md](docs/configuration.md) | All environment variables and config file options |
| [docs/deployment-host.md](docs/deployment-host.md) | Production deployment guide on Linux hosts without containers |

## Start Here

For onboarding, do not start by reading route files in isolation. Use this order instead:

1. `README.md` for run modes and the repo map.
2. [docs/onboarding.md](docs/onboarding.md) for the control-plane vs media-plane mental model.
3. [docs/architecture.md](docs/architecture.md) for the full request and runtime flows.
4. [docs/backend-services.md](docs/backend-services.md) and [docs/frontend-modules.md](docs/frontend-modules.md) for module ownership.
5. [docs/api-reference.md](docs/api-reference.md) and [docs/configuration.md](docs/configuration.md) when you need exact contracts.

If you want a fast confidence check before editing behavior, run `DEV=1 make deps` once and then
`npm test`. If you need to verify the end-to-end media path, start MediaMTX separately, start the
app with `npm start` or `npm run dev`, and then run `KEEP_RUNNING=1 make run-4x3`.

## Installation

For Linux workflows, `make deps` runs the preflight helper, installs Node.js dependencies, and
downloads MediaMTX into `bin/mediamtx/`:

```sh
make deps           # production-only dependencies
DEV=1 make deps     # development + test dependencies
```

Re-run `make deps` whenever `package.json` or `package-lock.json` changes. Commands that rely on
dev dependencies, including `npm run dev`, `npm test`, `make format`, and `make css`, require the
`DEV=1 make deps` install.

`make deps` may invoke `apt-get`/`sudo` to install missing OS packages. It targets Debian/Ubuntu-style Linux hosts because it installs Linux packages and downloads a Linux MediaMTX binary.

Docker is optional unless you use the disposable container stack (`make run-docker`) or the
`nginx-rtmp` sink used by `make run-4x3`.

`make run-4x3` directly requires host `node`, host `ffmpeg`, Docker with the compose plugin for
`nginx-rtmp`, and an already-running app plus MediaMTX stack.

## Run Modes

### App Process

Start the Node control plane directly:

```sh
npm start
```

For live reload during development:

```sh
DEV=1 make deps
npm run dev
```

This starts only the Node app. Start MediaMTX separately before testing ingest, preview, or output
lifecycle flows. One direct local option after `make deps` is:

```sh
bin/mediamtx/mediamtx mediamtx.yml
```

### Optional Docker Stack

`make run-docker` is available when you want a disposable containerized app + MediaMTX stack.
It is optional and not the primary development path.

```sh
make run-docker
```

This starts the app, MediaMTX, and the `nginx-rtmp` validation sink from `docker-compose.yml`.
Stop it with `docker compose down` when finished.

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
