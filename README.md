# Restream

Restream is a host-run streaming control plane built on [MediaMTX](https://github.com/bluenviron/mediamtx). It manages stream keys, pipelines, output destinations, and FFmpeg jobs from a browser dashboard.

## How It Works

1. A publisher sends RTMP/RTSP/SRT ingest to MediaMTX.
2. Restream stores stream keys, pipelines, outputs, job state, and logs in local SQLite (`data.db`).
3. When an output starts, Restream probes the MediaMTX RTSP relay, spawns FFmpeg, and tracks the process.
4. The dashboard reads `/config` and `/health` to show pipeline state, output state, system metrics, and logs.

MediaMTX owns media routing. Restream owns orchestration and state.

## Local Host Run

Install Node dependencies:

```sh
npm ci
```

Start MediaMTX with the checked-in config:

```sh
./mediamtx
```

On Windows, run `mediamtx.exe` from the project root instead.

In another terminal, start the app:

```sh
npm start
```

For development mode:

```sh
npm run dev
npm run css-watch
```

Useful commands:

```sh
make css          # rebuild public/output.css from public/input.css
make format       # run prettier
npm run test:routes
```

The dashboard runs on `http://localhost:3030/` by default.

## Runtime Files

- `data.db`: local SQLite database
- `public/output.css`: generated CSS

These are runtime or generated artifacts and should not be edited directly unless noted.

## Code Map

- `src/index.js`: Express app composition and route wiring
- `src/api/`: REST route modules
- `src/services/`: health collection, output lifecycle, recovery, and startup timers
- `src/db/`: SQLite schema and query helpers
- `src/config/`: runtime config loading and defaults
- `src/utils/`: shared backend helpers for FFmpeg, MediaMTX, retries, validation, and health shaping
- `public/`: dashboard pages, ES modules, and compiled CSS
- `test/`: route and behavior tests

## Docs

- [Architecture](docs/architecture.md): short system map and core behavior
- [Configuration](docs/configuration.md): environment variables and app config
- [API Reference](docs/api-reference.md): REST endpoints
- [Health Mapping](docs/health-mapping.md): how statuses are derived
- [Host Deployment](docs/deployment-host.md): systemd-style Linux deployment
