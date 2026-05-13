# CLAUDE.md

Behavioral guidelines to reduce common LLM coding mistakes. Merge with project-specific instructions as needed.

**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks, use judgment.

## 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them - don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

## 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

## 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:
- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:
- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

## 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:
- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:
```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

---

## Repo Specifics

**These guidelines are working if:** fewer unnecessary changes in diffs, fewer rewrites due to overcomplication, and clarifying questions come before implementation rather than after mistakes.
This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
npm run build               # compile backend (src/ → dist/) + frontend (public/ts/ → public/js/) + Tailwind CSS
npm run build:backend       # backend only
npm run build:frontend      # frontend TS + Tailwind CSS

npm run dev                 # backend with live reload (tsx watch, no compile step)
npm run watch:frontend      # frontend TS in watch mode
npm run watch:css           # Tailwind CSS in watch mode

npm run format              # run Prettier
npm run format:check        # check formatting (used in CI)

npm run test:routes         # unit tests for REST routes (no external services needed)
npm run test:normalization  # unit tests for URL normalization helpers
npm run test:integration    # 2x3 end-to-end test (requires running app + MediaMTX)
```

**Always edit `public/ts/` not `public/js/`** — `public/js/` is generated. Run `npm run build:frontend` after any frontend TS change.

## Architecture

```
Publisher -> MediaMTX (RTMP/SRT ingest) -> FFmpeg (one child process per output) -> External destinations
Browser dashboard -> Node/Express API -> SQLite (data.db) + MediaMTX control API
```

MediaMTX owns media transport. Restream owns orchestration, state, and UI.

### Backend (`src/`)

- **`src/index.ts`** — app composition: wires Express, `healthMonitor`, and `outputLifecycle`, and registers all route modules. The circular dependency between the two services (health needs lifecycle for recovery callbacks, lifecycle needs health for input state) is resolved explicitly here.
- **`src/services/outputs.ts`** — `createOutputLifecycleService`: all FFmpeg process management. Spawns FFmpeg via `spawn`, tracks child processes in a shared `Map<string, ChildProcess>`, reads progress from fd3 (`-progress pipe:3`), and drives the retry/recovery state machine. Desired state (`running` | `stopped`) is persisted in SQLite; in-memory state (failure counts, retry timers, stop promise maps) is rebuilt naturally.
- **`src/services/health.ts`** — polls MediaMTX API + SQLite + in-memory FFmpeg progress on a 5 s cycle; exposes `/health` and SSE `/health/stream`. Drives input-recovery restarts by calling back into `outputLifecycle`.
- **`src/utils/ffmpeg.ts`** — `buildFfmpegOutputArgs`: the single function that constructs the full FFmpeg argv. Encoding types: `source` (copy), named presets (`720p`, `1080p`, `vertical-crop`, `vertical-rotate`), `custom` (raw args from DB), and `remap:L:R` (pan filter for channel remapping). Validation via `isValidOutputEncoding`.
- **`src/api/outputs.ts`** — output CRUD + start/stop + history. Encoding and URL changes are blocked while an output is running (409); the `desiredState` and reconciliation are driven through `outputLifecycle`.
- **`src/db/`** — `setupDatabaseSchema` creates tables on startup; all queries are raw `better-sqlite3` prepared statements typed against the `Db` interface in `src/types.ts`.

### Frontend (`public/ts/`)

The frontend is a plain TypeScript/ES-module SPA with no framework. There is no bundler — files are served as ES modules from `public/js/`.

- **`public/ts/core/state.ts`** — single shared `state` object holding all pipeline/output view models. All rendering reads from this.
- **`public/ts/core/api.ts`** — typed fetch wrappers for all REST calls. Mutations show a saving badge and trigger a dashboard refresh.
- **`public/ts/features/dashboard.ts`** — poll loop: `/health` + `/metrics/system` every 5 s; `/config` every other tick (~10 s) or immediately after mutation/tab focus. Drops to 30 s when tab is hidden.
- **`public/ts/features/editor.ts`** — all output/pipeline modal logic: `openOutModal`, `editOutFormBtn`, `addOutBtn`. Output encoding `remap:L:R` is stored as a string in the DB; the modal parses it to show separate Left/Right channel dropdowns populated from `pipe.input.audio?.channels`.
- **`public/ts/features/pipeline-view.ts`** — renders the output list. Functions exposed to `window` (e.g., `editOutBtn`, `addOutBtn`, `pipeFormBtn`) are declared in `public/ts/global.d.ts`.

### Key design constraints

- **Encoding stored as string**: `remap:0:1` means left=c0, right=c1 (0-indexed). Other encodings are plain keys (`source`, `720p`, etc.) or `custom`.
- **No DB migrations**: `CREATE TABLE IF NOT EXISTS` is run at startup. Changing schema requires manual handling.
- **MediaMTX ports are hardcoded**: API=9997, RTMP=1935, SRT=8890, HLS=8888 — all localhost. No env override.
- **Ingest URLs shown in dashboard** use the browser's current hostname (not localhost), resolved in the frontend.
- **FFmpeg pull protocol** is selected by output destination: RTMP destinations pull via RTMP; SRT and HLS destinations pull via SRT.
