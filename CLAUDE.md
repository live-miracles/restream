# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

All common workflows live in the `Makefile`. Frequently used:

| Command | Purpose |
|---|---|
| `make run-host` | MediaMTX in Docker, Node app on host with `nodemon` (primary dev loop) |
| `make run-docker` | App + MediaMTX both in Docker, sharing a pod-like namespace |
| `make down` | Tear down compose stacks |
| `make css` | Recompile Tailwind/DaisyUI from `input.css` to `public/output.css`. Required after changing `input.css` or any class names referenced from HTML/JS |
| `make format` | Run Prettier across the repo. The pre-commit hook only checks formatting; it does not auto-fix |
| `make start-input` | Push a looping color-bar test stream to MediaMTX (override with `INGEST_URL=...`) |
| `make probe-output` | `ffprobe` against an output URL to verify a relay |
| `npm run test:routes` | Node built-in test runner over `test/routes/*.test.js` |
| `npm run test:4x3` | End-to-end scenario test in `test/artifacts/run-4x3.mjs` |

After cloning, run `git config core.hooksPath .githooks` once to enable the pre-commit Prettier check.

## Architecture

Restream is a control plane on top of MediaMTX. **MediaMTX handles all media routing; this app handles all orchestration.** The app does not touch RTMP/RTSP packets directly — it spawns FFmpeg children that pull from MediaMTX's RTSP relay and push to external destinations.

```
Publisher ──► MediaMTX RTMP :1935 ──► RTSP :8554 ──► FFmpeg child ──► YouTube/Facebook/...
                    ▲                                        ▲
                    │ MediaMTX API :9997                     │ spawned per output
                    │                                        │
Browser ──► Node API :3030 ◄──── SQLite (data/data.db) ──────┘
```

### Cross-system invariants

Two stores must stay consistent: SQLite and MediaMTX path config. The code defends this in two places — preserve these patterns when editing:

1. **Stream key create/delete (`src/api/pipelines.js`)** is two-phase with compensating rollback: MediaMTX path is mutated first, SQLite second. If the SQLite write fails, the MediaMTX mutation is reverted.
2. **Pipeline/output delete (`src/api/pipelines.js`, `src/api/outputs.js`)** waits for FFmpeg teardown via the recovery service before removing rows. If teardown times out, the API returns 409 rather than orphaning a process. DB rows are never deleted while a child process is still alive.

### State authority

- **DB is authoritative** for `jobs.status`. The in-memory `processes` Map (`Map<jobId, ChildProcess>`) is runtime-only and lost on restart; bootstrap reseeds health from MediaMTX, not from this map.
- **`outputs.desired_state`** ("operator wants this running") is separate from `jobs.status` ("is it actually running"). Recovery logic reconciles the two — never collapse them.
- **Output encoding lives on `outputs.encoding` only.** Pipelines select an input stream key; they do not carry encoding/profile settings.
- **`job_logs`** is an append-only event stream keyed by `pipeline_id` and `output_id`. Use `event_type` codes (`lifecycle.*`, `pipeline.input_state.*`, etc.) — do not parse `message` text.

### Health & recovery flow

`src/services/health.js` polls MediaMTX every few seconds, indexes paths/connections/readers, and runs cached `ffprobe` against the RTSP relay. The aggregated snapshot drives both `GET /health` and the input-recovery callback in `src/services/recovery.js`. Input-loss vs output-failure is distinguished by a millisecond-level grace window between job exit and input-off transition; do not "simplify" that window away without understanding why it exists.

### Frontend module contract

All frontend is unbundled ES modules loaded via `<script type="module">` from per-page entry files (`public/js/features/dashboard-entry.js`, `public/js/features/stream-keys-page.js`). See `docs/frontend-modules.md` for the full contract. Key rules:

- `public/js/core/state.js` is the **only** shared mutable state container (`state.config`, `state.health`, `state.pipelines`, `state.metrics`). Do not introduce parallel globals.
- Orchestration/fetch modules write state; render/interaction modules read it.
- `window.*` exposure is reserved for functions invoked directly from HTML attributes (e.g. `onclick="selectPipeline(...)"`). Don't use it as a generic IPC channel between modules.

### Config & versioning

`src/config/index.js` loads `src/config/restream.json` with env-var overrides and a public subset (`toPublicConfig()`) that ships to the browser. The dashboard polls `/config` (ETag-gated) plus `/health` and merges them via `parsePipelinesInfo()` in `public/js/core/pipeline.js` into the view model.

## Documentation

When making non-trivial changes, the `docs/` directory has the long-form context:

- `docs/architecture.md` — data model, call flows, deployment shapes
- `docs/api-reference.md` — REST endpoints with request/response shapes
- `docs/health-mapping.md` — how on/warning/off/error states are derived
- `docs/configuration.md` — every env var and config field
- `docs/frontend-modules.md` — frontend ES module conventions (read before refactoring dashboard JS)
- `docs/deployment-host.md` — production deploy on Linux without containers
