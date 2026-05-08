# Restream Onboarding

This guide is for developers who need a working mental model of the codebase before making
changes. Read it once before opening files at random; the project becomes much easier to navigate
when you keep the control-plane and media-plane boundaries separate.

## 1. Mental Model First

Restream is not the media server. MediaMTX moves media packets. Restream is the control plane that
stores operator intent, starts and stops FFmpeg jobs, and turns MediaMTX runtime state into an API
that the dashboard can stream and resync.

Keep these boundaries in mind:

- SQLite stores durable control-plane state: stream keys, pipelines, outputs, jobs, and history.
- MediaMTX stores live ingest and connection state: publishers, RTSP readers, and path readiness.
- FFmpeg processes are runtime-only workers. They are tracked in memory and summarized into DB rows
  and health snapshots.
- The dashboard renders a merged view of `/config`, `/health`, and `/metrics/system`.

Two concepts matter everywhere in the code:

- `outputs.desiredState` is operator intent. It answers: should this output be running?
- `jobs.status` is process outcome. It answers: what happened to the last FFmpeg process?

Those values intentionally diverge during retries, input outages, and manual stops.

## 2. First-Day Commands

Use these commands to get a reliable edit-and-test environment:

```sh
DEV=1 make deps
npm run dev
npm test
```

When you need the full media-path validation that exercises the 4x3 normalization flow, run
MediaMTX and the app separately, then start the validation runner:

```sh
bin/mediamtx/mediamtx mediamtx.yml
npm start
KEEP_RUNNING=1 make run-4x3
```

Notes:

- Re-run `make deps` whenever `package.json` or `package-lock.json` changes.
- `npm run dev` and `npm test` both require the `DEV=1 make deps` install.
- `make run-4x3` starts the sink container itself, but it still expects the app and MediaMTX to
  already be running.

## 3. Recommended Reading Order

Read the repository in this order:

1. `README.md` for runtime modes, repo layout, and operational commands.
2. `docs/architecture.md` for the system-level view and main request flows.
3. `docs/backend-services.md` for the backend ownership map.
4. `docs/frontend-modules.md` for dashboard module ownership and browser-side patterns.
5. `docs/api-reference.md` and `docs/configuration.md` when you need exact route or config details.

Then move into code in this order:

1. `src/index.js`
2. `src/pipeline-runtime-state.js`
3. `src/routes-pipeline.js`
4. `src/health.js`
5. `src/outputs.js`
6. `src/recovery.js`
7. `public/js/features/dashboard.js`
8. `public/js/history.js`

That sequence mirrors how the app starts, streams updates, and reacts to live stream state.

## 4. How Data Moves Through The System

### Config path

`/config` is the dashboard's durable snapshot. It is built mostly from SQLite rows plus the public
server config and carries a deterministic snapshot version used by the dashboard SSE stream and
recovery sync.

Relevant files:

- `src/routes-pipeline.js`
- `src/config.js`
- `src/db.js`

### Health path

`/health` is the live runtime snapshot. It merges MediaMTX APIs, FFmpeg progress, probe cache data,
and DB metadata into one response. This is where input state transitions, output health, reader
matching, and input-recovery triggers are decided.

Relevant files:

- `src/health.js`
- `src/health-compute.js`
- `src/pipeline-runtime-state.js`

### Output lifecycle path

Starting an output eventually becomes `spawn(ffmpeg, args)`. Stopping an output becomes
`SIGTERM` followed by timed `SIGKILL` escalation when needed. Recovery logic layers on top of that
base lifecycle instead of bypassing it.

Relevant files:

- `src/pipeline-runtime-state.js`
- `src/outputs.js`
- `src/recovery.js`
- `src/recovery-helpers.js`

### Dashboard path

The browser does not talk directly to MediaMTX. It consumes Node API snapshots/events, merges
config and health into a view model, and then renders cards, detail panels, modals, and metrics.

The main dashboard uses `/dashboard/events` (SSE) for live updates and falls back to one-shot
snapshot fetches (`/config`, `/health`, `/metrics/system`) during recovery.

Relevant files:

- `public/js/client.js`
- `public/js/pipeline.js`
- `public/js/features/dashboard.js`
- `public/js/features/dashboard-view.js`
- `public/js/features/view.js`
- `public/js/history.js`

## 5. Where To Put Changes

Use this routing guide before creating new files:

- Add or change REST routes in `src/routes-pipeline.js`, `src/routes-output.js`, or `src/preview.js`.
- Put long-lived orchestration in `src/pipeline-runtime-state.js`, `src/health.js`, `src/outputs.js`, `src/recovery.js`, or
  `src/bootstrap.js`.
- Put pure calculation or protocol helpers in `src/health-compute.js`, `src/recovery-helpers.js`,
  or `src/utils.js`.
- Keep `src/index.js` as wiring code, not business logic.
- Keep persistent queries and schema changes in `src/db.js`.
- Put frontend fetch/state helpers in `public/js/client.js` and `public/js/pipeline.js`.
- Put DOM-heavy feature behavior in `public/js/features/` or `public/js/history.js`.
- Protect regressions with route/service tests under `test/` and browserless smoke tests under
  `test/frontend/`.

## 6. Common Debugging Checklist

When something looks wrong, check these in order:

1. `GET /config` to verify the stored control-plane state.
2. `GET /health` to verify MediaMTX readiness, input state, and output status.
3. Pipeline/output history in the UI or `job_logs` to see why the system made a decision.
4. App/MediaMTX process logs for the runtime you are using.
5. The latest frontend smoke tests if the dashboard behavior regressed during a refactor.

Specific cases:

- Output refuses to start: inspect desired state, input availability, and validation errors.
- Output keeps restarting: inspect recovery logs and the output-recovery config.
- Input shows `warning` or `off`: inspect MediaMTX path readiness and publisher presence.
- Dashboard looks stale: compare `/dashboard/events` payloads, `/config` snapshot version, and
  `/health` snapshot version.

## 7. Safe First Tasks For New Contributors

Good starter tasks:

- Add a small API field and thread it through `/config` or `/health`.
- Extend the frontend smoke tests for a new dashboard interaction.
- Add structured `eventType` or `eventData` to an existing history log line.
- Tighten validation or masking logic without changing public route shapes.

Tasks that require extra care:

- Changing FFmpeg command assembly.
- Changing output retry policy or input-recovery rules.
- Changing how `/config` and `/health` snapshot versions are computed.
- Changing dashboard/history orchestration, which is sensitive to subtle coupling between the
  stream updates, render timing, and selection state.