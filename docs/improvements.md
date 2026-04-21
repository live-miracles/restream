# Improvements Backlog

**Last reviewed:** 2026-04-19

This file is now a short current-state backlog. The earlier audit turned into a running changelog while the `improvements` branch landed most of the fixes, so the detailed narrative is no longer useful here. The source of truth for implemented work is the code plus the commits already on this branch.

## Purpose

- Track only work that is still relevant.
- Mark stale findings explicitly instead of carrying forward outdated audit text.
- Avoid duplicating implementation detail that already lives in git history.

## Implemented In This Branch

- Output start/stop debounce fix and backend start-race mitigation.
- `/health` refactor: helper extraction, cached snapshots, background probe refresh, and conditional dashboard polling.
- Targeted database lookups for pipeline outputs instead of list-then-filter reads.
- Validation hardening for pipeline/output names and output URLs at start time.
- Shared frontend secret masking helper.
- FFmpeg output argument builder extraction.
- Backend helper-boundary refactor: shared runtime helpers extracted into `src/utils/{app,ffmpeg,mediamtx,retry}.js`.
- Frontend ES-module migration across dashboard/history/stream-keys with explicit imports and shared state in `public/js/core/state.js`.
- Stream-key create/delete now uses compensating MediaMTX rollback so DB write failures do not leave path config mutated on their own.
- Dashboard/history frontend loading now uses page entry modules plus explicit import/callback wiring instead of ordered `<script type="module">` tags and internal `window.*` handoffs.
- `/config` and `/health` now share an explicit snapshot-version token, and the dashboard retries refreshes until those slices agree before rendering.
- History endpoints and stored `job_logs` entries now carry stable `eventType` codes plus structured `eventData`, so timeline rendering no longer depends on parsing backend prose.
- Output and pipeline delete routes now wait for process teardown before removing DB state, returning `409` instead of deleting underneath a still-running ffmpeg process.
- `/metrics/system` now serves a fixed background sample, so CPU and network deltas are stable across concurrent clients instead of depending on the last request.
- Dashboard refreshes and pipeline-history live polling now coalesce overlapping requests, so slow refreshes do not stack and overwrite newer UI state out of order.

## Architecture Follow-Ups From 2026-04-19 Review

| Item | Status | Notes |
| --- | --- | --- |
| 2. Reconcile MediaMTX + SQLite stream-key mutations | Fixed | Create/delete now roll back the MediaMTX path change if the DB phase fails after the control-plane mutation succeeds. |
| 3. Replace order-dependent frontend `window.*` module handoffs | Fixed | Dashboard and history features now load through page entry modules; internal module calls use imports or registered callbacks, leaving `window.*` only for markup-bound handlers. |
| 4. Add shared snapshot identity across `/config` and `/health` | Fixed | Both endpoints now expose the same snapshot-version token for config/jobs state, health refreshes its cache when that token is stale, and the dashboard retries until the slices line up. |
| 5. Replace history log-message parsing with typed event payloads | Fixed | Lifecycle and pipeline history now emit stable event codes with structured payloads, and the UI consumes those fields with a fallback only for older stored rows. |
| 6. Make delete flows wait for process teardown before final removal | Fixed | Delete routes now stop and wait for running jobs before deleting, and they fail with `409` if teardown does not complete in time. |
| 7. Move system metrics deltas to fixed background sampling | Fixed | The metrics endpoint now reads from a timer-driven sample, so request cadence no longer distorts CPU and network rate calculations. |

## Closed Or Stale Findings

| Item | Status | Notes |
| --- | --- | --- |
| Parallelize `ffprobe` in `/health` | Stale | Background probe refresh is already in place, so the old sequential `/health` warning is no longer an active item. |
| Clean up `processes` + `ffmpegProgressByJobId` Maps | Fixed | Verified in `src/index.js`: both maps are cleared on child `error` and `exit`. |
| Minify CSS build | Rejected for now | `public/output.css` is kept readable in-repo; HTTP compression already captures the practical transfer-size win. |
| Delete `docs/PRD.md` and `docs/RFC.md` | Deferred intentionally | These files stay in place until there is a specific decision to remove them. |

## Active Backlog

| Priority | Item | Why it still matters | Notes |
| --- | --- | --- | --- |
| P0 | Make FFmpeg supervision restart-safe and detach-capable | Running-job truth, stderr, and progress still depend on in-memory maps, so a controller restart cannot reliably verify or observe pre-existing FFmpeg runs. | Short term: add startup reconciliation for stale `running` rows. Long term: move to detached run manifests / leases with file-backed progress and stderr, plus PID+start-time validation on startup. |
| P0 | Add API authentication | The API is still open to anyone who can reach it. | Keep this simple: shared secret header or basic auth is enough. |
| P0 | Add rate limiting | Prevents accidental or hostile request floods against write endpoints. | `express-rate-limit` is still the shortest path. |
| P0 | Mask secrets in API/config responses by default | Frontend masking exists, but raw secret exposure should not be the default server behavior. | Add an explicit unmasked/admin path only if needed. |
| P1 | Add FFREPORT support for ffmpeg runs | Cheap operational visibility when an output fails or degrades. | Write logs under `data/ffmpeg/` with bounded retention. |
| P2 | Replace hash-based `recomputeEtag()` with a version counter | Removes unnecessary hash work on every mutation. | Small backend cleanup, not urgent. |

## Deferred Larger Work

- Diff-based DOM updates instead of full rerenders.
- JS bundling/minification.
- Another scale/perf pass after the next substantial dashboard change.

## Notes

- Use git history on this branch as the source of truth for already-landed work.
- If an old audit item resurfaces, verify current code before adding it back here.
