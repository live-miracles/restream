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
  index.js          — Express REST API + FFmpeg lifecycle management
  db.js             — SQLite schema, migrations, and query helpers (data.db)
  config/
    index.js        — Config loader with sanitization
    restream.json   — App config: server-name, pipelines-limit, out-limit
public/
  index.html        — Dashboard SPA shell
  stream-keys.html  — Stream key management page
  api.js            — All API calls (relative paths, never direct to MediaMTX)
  dashboard.js      — Event handlers, modals, polling orchestration
  pipeline.js       — parsePipelinesInfo(): merges config + health into view model
  render.js         — DOM rendering: pipeline cards, stats, output tables
  utils.js          — Shared utilities: formatTime, setServerConfig, copyData
  output.css        — Compiled Tailwind + DaisyUI (do not edit manually)
input.css           — Tailwind CSS source (compile with `make css`)
docs/               — Architecture, API reference, health mapping, config guide
infra/              — Deployment/runtime configs (MediaMTX, nginx-rtmp test sink)
test/artifacts/     — Reproducible test scripts and session recordings
docker-compose.yml  — Full-stack compose (app + mediamtx + nginx-rtmp test sink)
```

## Documentation

| Document | Description |
|---|---|
| [docs/architecture.md](docs/architecture.md) | System design, data model, call flows, deployment |
| [docs/api-reference.md](docs/api-reference.md) | All REST endpoints with request/response shapes |
| [docs/health-mapping.md](docs/health-mapping.md) | How input/output health statuses are derived |
| [docs/configuration.md](docs/configuration.md) | All environment variables and config file options |

## Installation

### Production Environment
For production deployments, install only runtime dependencies:

```sh
npm ci --omit=dev
```

This installs:
- `better-sqlite3` - Database
- `body-parser` - HTTP middleware
- `cors` - CORS support
- `express` - Web framework

### Development Environment
For local development with code formatting, CSS building, and live reload:

```sh
npm ci
```

This additionally installs:
- `nodemon` - Auto-reload on file changes
- `prettier` - Code formatter
- `tailwindcss` - CSS framework
- `@tailwindcss/cli` - Tailwind CLI
- `daisyui` - UI component library

### CI/Testing Environment
For continuous integration and testing pipelines:

```sh
npm ci
```

This additionally installs everything above plus:
- Browser tooling for dashboard screenshot capture tests (if enabled in your CI setup)

## Run Modes

### 1) Host Node + Docker MediaMTX

Node runs on host and talks to MediaMTX via localhost.

```sh
make run-host
```

### 2) Full Docker (Node + MediaMTX)

Node and MediaMTX both run in Docker and communicate via Docker service network.

```sh
make run-docker
```

## Clean Verification Targets

For clean startup checks of the containerized app:

```sh
make verify
```

## Contribute

After cloning, run:

```sh
git config core.hooksPath .githooks
```

Then install dependencies:

```sh
npm ci
```

## Links

1. MediaMTX: [github.com/bluenviron/mediamtx](https://github.com/bluenviron/mediamtx)
2. Nginx MLS: [github.com/live-miracles/MLS](https://github.com/live-miracles/MLS)
3. Nginx MLS demo: [youtu.be/yzXuirkmexo](https://youtu.be/yzXuirkmexo)
4. Go-MLS: [github.com/krsna1729/go-mls](https://github.com/krsna1729/go-mls)
5. Go-MLS demo: [www.youtube.com/watch?v=x2x3uSCAX4M](https://www.youtube.com/watch?v=x2x3uSCAX4M)
6. Daisy UI: [daisyui.com/](https://daisyui.com/)
