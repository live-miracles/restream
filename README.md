# Restream

A streaming server control plane that takes RTMP input streams and replicates them to multiple output destinations (YouTube, Facebook, Instagram, Twitch, etc.). Built on top of [MediaMTX](https://github.com/bluenviron/mediamtx) for media routing and FFmpeg for stream encoding/pushing.

<img src="https://github.com/user-attachments/assets/499f3309-eed3-49b3-8eaf-6d3a365751de" align="center" width="500" >

## Architecture

The system consists of three main components:

| Component | Role | Port |
|-----------|------|------|
| **Node.js App** | Control plane — REST API, web UI, FFmpeg process management | `3030` |
| **MediaMTX** | Data plane — RTMP/RTSP/HLS media routing and ingestion | `1935` (RTMP), `9997` (API) |
| **FFmpeg** | Spawned per output — consumes RTMP from MediaMTX and pushes to external platforms | — |

**Data flow:** RTMP input → MediaMTX (port 1935) → FFmpeg (per output) → External platforms (YouTube, etc.)

## Current Features

### Stream Key Management

- Create stream keys (auto-generated UUID or custom) that register paths in MediaMTX
- Update stream key labels
- Delete stream keys (removes from both local DB and MediaMTX)
- List all stream keys

### Pipeline Management

- Create named pipelines linked to a stream key with optional encoding settings
- Update pipeline properties (name, stream key, encoding)
- Delete pipelines (cascading delete of outputs and jobs)
- List all pipelines
- Configurable limit of 25 pipelines per server

### Output Management

- Add multiple output destinations per pipeline (name + RTMP URL)
- Update output properties
- Delete individual outputs
- Configurable limit of 95 outputs per server

### Output Streaming Control

- **Start output** — spawns an FFmpeg child process that reads from MediaMTX and pushes to the output RTMP URL
- **Stop output** — sends SIGTERM to FFmpeg (with 5-second timeout before SIGKILL)
- Dynamic start/stop of individual outputs without affecting other running outputs
- FFmpeg args: copy video codec, synthesize audio (AAC 128k) if input lacks audio, FLV output format

### Monitoring & Metrics

- List active RTMP inputs (proxied from MediaMTX `/v3/paths/list`)
- RTMP connection metrics (proxied from MediaMTX `/v3/rtmpconns/list`)
- FFmpeg job logging — stdout/stderr captured to the database
- Job status tracking: `running`, `stopped`, `failed` with exit codes

### Configuration & Caching

- `GET /config` — full snapshot of stream keys, pipelines, and outputs with ETag support
- `HEAD /config` — ETag-only check for change detection (304 Not Modified)
- SHA-256 based ETag recomputed on every mutation

### Web UI

- Dashboard page for managing pipelines and outputs
- Stream keys management page
- Built with vanilla HTML/JS, Tailwind CSS 4, and DaisyUI 5

## Tech Stack

| Layer | Technology |
|-------|------------|
| Runtime | Node.js (CommonJS) |
| Web server | Express 5.1 |
| Database | SQLite via better-sqlite3 |
| Media server | MediaMTX |
| Media processing | FFmpeg (spawned as child processes) |
| Frontend | Vanilla HTML/JS + Tailwind CSS 4 + DaisyUI 5 |
| Dev tools | Nodemon, Prettier |
| Containerization | Docker Compose |

## REST API

### Stream Keys

| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/stream-keys` | Create a stream key |
| `GET` | `/stream-keys` | List all stream keys |
| `POST` | `/stream-keys/:key` | Update stream key label |
| `DELETE` | `/stream-keys/:key` | Delete a stream key |

### Pipelines

| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/pipelines` | Create a pipeline |
| `GET` | `/pipelines` | List all pipelines |
| `POST` | `/pipelines/:id` | Update a pipeline |
| `DELETE` | `/pipelines/:id` | Delete a pipeline |

### Outputs

| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/pipelines/:pipelineId/outputs` | Create an output |
| `POST` | `/pipelines/:pipelineId/outputs/:outputId` | Update an output |
| `DELETE` | `/pipelines/:pipelineId/outputs/:outputId` | Delete an output |
| `POST` | `/pipelines/:pipelineId/outputs/:outputId/start` | Start streaming (spawn FFmpeg) |
| `POST` | `/pipelines/:pipelineId/outputs/:outputId/stop` | Stop streaming (kill FFmpeg) |

### Monitoring & Config

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/inputs` | List active inputs from MediaMTX |
| `GET` | `/metrics/mediamtx/v3/rtmpconns/list` | RTMP connection metrics |
| `GET` | `/config` | Full config snapshot (with ETag) |
| `HEAD` | `/config` | Check config ETag only |

## Database Schema

Six SQLite tables with foreign key cascading:

- **stream_keys** — RTMP input paths (`key`, `label`, `created_at`)
- **pipelines** — Stream processing pipelines (`id`, `name`, `stream_key`, `encoding`)
- **outputs** — External streaming destinations (`id`, `pipeline_id`, `name`, `url`)
- **jobs** — FFmpeg process tracking (`id`, `pipeline_id`, `output_id`, `pid`, `status`)
- **job_logs** — FFmpeg stdout/stderr capture (`job_id`, `ts`, `message`)
- **meta** — Key-value store for ETag and other metadata

## Setup & Running

### Prerequisites

- Node.js
- Docker (for MediaMTX)
- FFmpeg (for test input and output streaming)

### Development

```sh
# Clone and configure git hooks
git clone https://github.com/live-miracles/restream.git
cd restream
git config core.hooksPath .githooks

# Install dependencies
npm install

# Build Tailwind CSS
make css
```

### Running (3 terminals)

```sh
# Terminal 1: Start MediaMTX
make start-mediamtx

# Terminal 2: Start the Node.js app (http://localhost:3030)
make start-ui

# Terminal 3: Send a test RTMP stream
make start-input
```

### Running with Docker Compose

```sh
docker-compose up
```

### Code Formatting

```sh
make pretty
```

## Project Structure

```
├── index.js             # Express server — all REST API endpoints
├── db.js                # SQLite database layer with prepared statements
├── restream.json        # App config (server name, limits)
├── mediamtx.yml         # MediaMTX configuration
├── docker-compose.yml   # Multi-container dev environment
├── Makefile             # Build and run commands
├── ui/                  # Frontend web application
│   ├── index.html       # Dashboard page
│   ├── stream-keys.html # Stream keys management page
│   ├── dashboard.js     # Dashboard logic
│   ├── stream-keys.js   # Stream keys page logic
│   ├── api.js           # Frontend API client
│   ├── pipeline.js      # Pipeline data structures
│   ├── render.js        # DOM rendering functions
│   └── utils.js         # Utility functions
├── v2/                  # Future version planning docs
│   ├── PRD.md           # Product Requirements Document
│   └── RFC.md           # Architecture specification
└── test/                # Test assets
    └── colorbar-timer.mp4
```

## Contribute

After cloning, run:

```sh
git config core.hooksPath .githooks
```

## Links

1. MediaMTX: [github.com/bluenviron/mediamtx](https://github.com/bluenviron/mediamtx)
2. Nginx MLS: [github.com/live-miracles/MLS](https://github.com/live-miracles/MLS)
3. Nginx MLS demo: [youtu.be/yzXuirkmexo](https://youtu.be/yzXuirkmexo)
4. Go-MLS: [github.com/krsna1729/go-mls](https://github.com/krsna1729/go-mls)
5. Go-MLS demo: [www.youtube.com/watch?v=x2x3uSCAX4M](https://www.youtube.com/watch?v=x2x3uSCAX4M)
6. Daisy UI: [daisyui.com/](https://daisyui.com/)
