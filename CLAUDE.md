# CLAUDE.md

Use judgment, stay surgical, and ask when unclear. Prefer the smallest change that solves the request.

## Quick Commands
```sh
npm run build
npm run build:backend
npm run build:frontend
npm run dev
npm run watch:frontend
npm run watch:css
npm run format
npm run format:check
npm run test:routes
npm run test:normalization
npm run test:integration
```

## Repo Map
- `src/index.ts` wires Express, health, and output lifecycle.
- `src/services/outputs.ts` owns FFmpeg process management and retries.
- `src/services/health.ts` polls MediaMTX + SQLite and drives recovery.
- `src/utils/ffmpeg.ts` builds FFmpeg args and validates encodings.
- `src/api/outputs.ts` handles output CRUD and running-state guards.
- `src/db/` contains the SQLite schema and queries.

## Working Rules
- Edit `public/ts/`, not generated `public/js/`; run `npm run build:frontend` after frontend TS changes.
- Encoding is stored as a string; `remap:0:1` means left=c0, right=c1.
- No DB migrations; schema changes need manual handling.
- MediaMTX ports are fixed: API `9997`, RTMP `1935`, SRT `10080`, HLS `8888`.
- Ingest URLs shown in the dashboard use the browser hostname, not localhost.
- Pull protocol follows the active ingest protocol; unknown falls back to RTMP.

## Parallel Worktrees
- Use separate git worktrees for parallel agents.
- Keep each worktree isolated with its own `node_modules/` and `data.db`.
- Use a container for integration tests because hardcoded ports collide on the host.
- Rebase onto `master` before merging, then run `npm run build && npm test`.
