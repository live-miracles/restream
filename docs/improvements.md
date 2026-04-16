# Improvements Backlog

**Last reviewed:** 2026-04-16

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
