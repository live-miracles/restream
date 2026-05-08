# Backend Services Guide

This document explains the backend module boundaries in the current top-level layout. Use it when
you know the behavior you want to change but are not yet sure which file owns that behavior.

Scope:

- Keep end-to-end request flows, deployment shapes, and system-wide architecture in
   [architecture.md](./architecture.md).
- Keep backend file ownership, startup wiring, runtime-state ownership, and "where should I edit?"
   guidance here.

## 1. Service Map

| File | Owns | Depends On | Edit Here When... |
|---|---|---|---|
| `src/index.js` | App composition root and dependency wiring | all top-level modules | you are changing startup wiring or cross-module dependencies |
| `src/routes-pipeline.js` | `/config`, pipeline CRUD, and stream-key routes | DB, config helpers, HTTP helpers, pipeline runtime state contract | you are changing config response shape, snapshot-version handling, or pipeline/key routes |
| `src/routes-output.js` | output CRUD, history, and lifecycle routes | DB, recovery/output lifecycle, HTTP helpers | you are changing output route behavior or history responses |
| `src/preview.js` | preview proxy routes | fetch, MediaMTX URL helpers | you are changing preview routing or proxy semantics |
| `src/pipeline-runtime-state.js` | shared in-memory input transition history and recovery callback seam | DB, MediaMTX path reads, health-compute, utility helpers | you are changing input transition history, runtime input seeding, or input-loss classification |
| `src/health.js` | live health aggregation and `/health` route registration | DB, MediaMTX APIs, FFmpeg runtime maps, health-compute, pipeline runtime state | you are changing health status calculation or live snapshot behavior |
| `src/health-compute.js` | pure health snapshot and media parsing helpers | plain inputs and utility helpers | you are changing status/media derivation without route wiring |
| `src/outputs.js` | FFmpeg spawn/stop lifecycle and per-job logging | DB, recovery service, FFmpeg helpers | you are changing output start/stop behavior |
| `src/recovery.js` | desired state, retry scheduling, restart suppression, DB/log orchestration | DB, recovery-helpers, lifecycle callbacks | you are changing restart policy or stop/retry coordination |
| `src/recovery-helpers.js` | pure process-stop, restart-state, and recovery decision helpers | child-process objects and plain data | you are changing wait/escalation behavior or pure retry decisions |
| `src/db.js` | SQLite schema, migrations, and persistent queries | better-sqlite3 | you are changing stored state or query behavior |
| `src/http.js` | shared Express response helpers | Express response objects | you are changing JSON/error envelope behavior |
| `src/bootstrap.js` | startup ordering, readiness wait, and shutdown handling | app, health monitor, DB | you are changing startup or periodic cleanup behavior |
| `src/utils.js` | shared validation, logging, MediaMTX, FFmpeg, and redaction helpers | common backend dependencies | you are changing shared helper semantics across routes/services |

## 2. Startup Wiring Sequence

The backend starts in this order:

1. `src/index.js` creates the Express app and runtime registries.
2. `registerConfigApi()` from `src/routes-pipeline.js` is called early because other services use
   its ETag helpers.
3. `createPipelineRuntimeStateService()` is created to own shared input transition history.
4. `createHealthMonitorService()` is created with DB access, MediaMTX fetch access, the runtime
   FFmpeg Maps, and the shared pipeline runtime state service.
5. `createOutputLifecycleService()` is created next and receives the pipeline runtime state's
   input-unavailable classifier.
6. `src/index.js` registers the output lifecycle's
   `restartPipelineOutputsOnInputRecovery()` callback with the shared pipeline runtime state.
7. Route modules are registered, with `src/routes-pipeline.js` receiving only the runtime-state
   facade it actually needs.
8. `startServer()` from `src/bootstrap.js` waits for health bootstrap before opening the HTTP port.

The important part is the shared coordinator boundary:

- `src/health.js` observes live input transitions and records them in `src/pipeline-runtime-state.js`
- `src/outputs.js` and `src/recovery.js` consume that same runtime state to classify clean stops
- `src/index.js` owns the single callback registration that turns input recovery into output restarts

Routes do not need that full service shape. `src/routes-pipeline.js` only receives the narrower
runtime-state contract it actually uses: resolve initial input state, seed in-memory pipeline
status, and clear that runtime state on delete.

## 3. Snapshot Boundaries

Three backend responses are easy to confuse. Treat them as separate products:

### `/config`

- Built from SQLite plus the public config file.
- Used for durable dashboard state and optimistic browser caching.
- Has a stable ETag because `src/routes-pipeline.js` hashes a deterministic DB-backed snapshot.

### `/health`

- Built from live MediaMTX data, runtime process state, and selected DB metadata.
- Used for live status, input transitions, output health, and input-recovery decisions.
- Can legitimately change without any config write.

### `/metrics/system`

- Built from host OS samples.
- Used only for dashboard system metrics panels.
- Does not participate in the config/health snapshot-version coordination.

When a UI bug looks like a stale render, first decide which of these three snapshots is actually
wrong before touching any code.

## 4. Output Lifecycle And Recovery

`src/outputs.js` owns the concrete FFmpeg job lifecycle.

It is responsible for:

- validating pipeline and output existence before start
- checking live input availability before spawning FFmpeg
- constructing FFmpeg arguments and redacted previews
- creating or updating the current `jobs` row
- capturing high-volume FFmpeg progress into in-memory Maps
- turning child-process exit events into DB status and lifecycle logs

`src/recovery.js` layers policy on top of that lifecycle.

It is responsible for:

- storing desired-state changes
- suppressing retries when the operator wants an output stopped
- tracking failure counts and pending restart timers
- classifying stop requests versus unexpected exits
- scheduling retries and input-recovery restarts

`src/recovery-helpers.js` holds the pure helpers used by that policy layer.

It is responsible for:

- process stop/wait primitives
- per-output restart-state bookkeeping helpers
- retry-budget and input-recovery eligibility decisions

That split is intentional:

- `outputs.js` answers "how do we start or stop a worker safely?"
- `recovery.js` answers "should we start it again, and when?"
- `recovery-helpers.js` answers "what is the pure reusable logic behind those choices?"

## 5. Health And Input Recovery

`src/health.js` does more than answer `/health`, but it no longer owns the shared transition maps
directly.

`src/pipeline-runtime-state.js` owns the shared in-memory history that other services depend on:

- the last known input status per pipeline
- when a pipeline most recently transitioned away from `on`

That history lets the system distinguish these cases:

- an output failed even though input was still present
- an output stopped cleanly because the upstream input disappeared
- an input came back and eligible outputs should be restarted

If you change input recovery behavior, review `src/pipeline-runtime-state.js` together with
`src/health.js` and `src/recovery.js`. One module owns the shared runtime history, one observes
the transition, and one decides how output policy reacts.

## 6. Runtime State Ownership

Persistent state lives in SQLite. Runtime state lives in the Maps created by
`createRuntimeRegistries()` in `src/bootstrap.js`.

Those Maps currently hold:

- `processes`: `jobId -> ChildProcess`
- `ffmpegProgressByJobId`: latest FFmpeg `-progress` block per running job
- `ffmpegOutputMediaByJobId`: parsed output media details per running job

Rules of thumb:

- If the value must survive a restart, put it in SQLite.
- If the value only exists while a process is alive, keep it in a runtime registry.
- If the UI only needs a derived view, prefer computing it in `/config` or `/health` instead of
  storing another column.

## 7. Choosing The Right Module

Use these shortcuts when making changes:

- Need a new dashboard field that comes from DB state: start in `src/routes-pipeline.js` and `src/db.js`.
- Need a new live health field: start in `src/health.js` or `src/health-compute.js`.
- Need to change how input transitions are remembered or classified for recovery: start in `src/pipeline-runtime-state.js`.
- Need a new output start/stop validation rule: start in `src/outputs.js`.
- Need a new retry or suppression rule: start in `src/recovery.js`.
- Need to change stop escalation timing or pure retry selection logic: start in `src/recovery-helpers.js`.
- Need to expose or consume a route: start in `src/routes-pipeline.js`, `src/routes-output.js`, or `src/preview.js`.

If you are still unsure, trace the flow from `src/index.js`; it shows which module owns each
public route and which helpers are intentionally shared.