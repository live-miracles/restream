# Improvements Backlog

**Last reviewed:** 2026-05-07

This file is a short current-state backlog, not a changelog. Keep only work that is still relevant
after the current refactor state; use git history and the focused docs as the source of truth for
landed changes.

## Purpose

- Track only work that is still worth doing.
- Keep closed or stale items brief.
- Avoid branch-specific narrative and outdated file-path references.

## Recently Closed Since The 2026-04-19 Review

| Item | Status | Notes |
| --- | --- | --- |
| Add compensating rollback for stream-key create/delete | Fixed | Stream-key create/delete now roll back the MediaMTX path change if the SQLite phase fails after the control-plane mutation succeeds. |
| Replace order-dependent frontend `window.*` handoffs | Fixed | Dashboard and history now boot through entry modules plus explicit callback seams, leaving `window.*` only for markup-bound handlers. |
| Add shared snapshot identity across `/config` and `/health` | Fixed | Both endpoints now expose the same snapshot-version token for config/jobs state, and the dashboard retries until the slices line up. |
| Replace history log-message parsing with typed event payloads | Fixed | Lifecycle and pipeline history now emit stable `eventType` codes plus structured `eventData`. |
| Make delete flows wait for process teardown before final removal | Fixed | Delete routes now stop and wait for running jobs before deleting, and they fail with `409` if teardown does not complete in time. |
| Move system metrics deltas to fixed background sampling | Fixed | The metrics endpoint now reads from a timer-driven sample, so request cadence no longer distorts CPU and network rate calculations. |
| Reduce refresh overlap churn in dashboard/history polling | Fixed | Slow refreshes no longer stack and overwrite newer UI state out of order. |
| Flatten helper boundaries into current top-level modules | Fixed | Shared backend helpers are now consolidated around the top-level service layout plus `src/utils.js`, and frontend state/module seams are explicit. |

## Closed Or Stale Findings

| Item | Status | Notes |
| --- | --- | --- |
| Parallelize `ffprobe` in `/health` | Stale | Background probe refresh is already in place, so the old sequential `/health` warning is no longer an active item. |
| Clean up `processes` + `ffmpegProgressByJobId` Maps | Fixed | Current exit/error handling clears runtime maps; the remaining restart limitation is tracked separately below. |
| Minify CSS build | Rejected for now | `public/output.css` is kept readable in-repo; HTTP compression already captures the practical transfer-size win. |
| Delete `docs/PRD.md` and `docs/RFC.md` | Deferred intentionally | These files stay in place until there is a specific decision to remove them. |

## Active Backlog

| Priority | Item | Why it still matters | Notes |
| --- | --- | --- | --- |
| P0 | Make FFmpeg supervision restart-safe and detach-capable | Running-job truth, stderr, and progress still depend on in-memory maps, so a controller restart cannot reliably verify or observe pre-existing FFmpeg runs. | Short term: add startup reconciliation for stale `running` rows. Long term: move to detached run manifests or leases with file-backed progress and stderr, plus PID+start-time validation on startup. |
| P0 | Add API authentication | The API is still open to anyone who can reach it. | Keep this simple: shared secret header or basic auth is enough. |
| P0 | Add rate limiting | Prevents accidental or hostile request floods against write endpoints. | `express-rate-limit` is still the shortest path. |
| P0 | Mask secrets in API and config responses by default | Frontend masking exists, but API clients can still receive raw ingest and output URLs unless they apply their own redaction. | Add an explicit unmasked or admin path only if needed. |
| P1 | Add FFREPORT support for ffmpeg runs | Cheap operational visibility when an output fails or degrades. | Write logs under `data/ffmpeg/` with bounded retention. |
| P2 | Replace hash-based `recomputeEtag()` with a version counter | `/config` still hashes snapshots on every mutation. | Small backend cleanup, not urgent. |

## Deferred Larger Work

- Diff-based DOM updates instead of full rerenders.
- JS bundling or minification.
- Another scale or perf pass after the next substantial dashboard change.

## Notes

- Use current git history and the focused docs as the source of truth for landed work.
- If an old audit item resurfaces, verify current code before adding it back here.
