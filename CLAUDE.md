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
**Always run the formatter (`npm run format`) before pushing a branch or creating a PR.**

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
- **MediaMTX ports are hardcoded**: API=9997, RTMP=1935, SRT=10080, HLS=8888 — all localhost. No env override.
- **Ingest URLs shown in dashboard** use the browser's current hostname (not localhost), resolved in the frontend.
- **FFmpeg pull protocol** follows the active ingest protocol when health state identifies it: RTMP ingest pulls via RTMP; SRT ingest pulls via SRT. Unknown protocol falls back to RTMP.

---

## Parallel Agents

Multiple Claude Code agents can work on this repo simultaneously using git worktrees for code isolation and containers for integration testing.

### Worktrees for code isolation

Each parallel agent MUST work in its own git worktree. Never have two agents editing files in the same working directory.

```sh
# Create a worktree for a feature branch
git worktree add ../restream-<short-name> -b feat/<short-name>

# When done, clean up
git worktree remove ../restream-<short-name>
```

**Rules:**
- Worktree directory goes in the parent of the repo (e.g., `../restream-add-srt-stats`).
- Branch name must match the worktree purpose. Use `feat/`, `fix/`, or `refactor/` prefixes.
- Each worktree has its own `node_modules/` — run `npm ci` after creating one.
- Each worktree gets its own `data.db` (SQLite creates it on first run), so no DB locking conflicts.
- `public/js/` is gitignored and generated — run `npm run build:frontend` in each worktree after `npm ci`.
- Don't commit from a worktree to a branch another agent is using.

### Test tiers

Pick the lightest tier that covers your change.

| Tier | Command | Needs | Safe in parallel? |
|------|---------|-------|--------------------|
| **Unit** | `npm run test:routes` | Node.js only | Yes — always |
| **Unit** | `npm run test:normalization` | Node.js only | Yes — always |
| **Build** | `npm run build` | Node.js + tsc | Yes — always |
| **Format** | `npm run format:check` | Node.js + prettier | Yes — always |
| **Integration** | `npm run test:integration` | App + MediaMTX + FFmpeg | No — use container |

**Backend-only changes** (anything under `src/`): run unit tests + build. No container needed.

**Frontend-only changes** (anything under `public/ts/`): run `npm run build:frontend`. No container needed.

**Integration tests**: require a container because ports are hardcoded and cannot be shared.

### Containers for integration testing

The app hardcodes ports (3030, 9997, 1935, 10080, 8888) with no env-var overrides. Two instances cannot coexist on the same host. Each container gets its own network namespace, so hardcoded ports don't clash.

```sh
# From the worktree directory — npm ci must have been run on the host first.
docker run --rm \
  -v "$(pwd)":/app -w /app \
  node:22 bash -c '
    set -e
    apt-get update -qq && apt-get install -y -qq ffmpeg > /dev/null 2>&1
    ARCH=$(dpkg --print-architecture)
    curl -fsSL "https://github.com/bluenviron/mediamtx/releases/download/v1.17.1/mediamtx_v1.17.1_linux_${ARCH}.tar.gz" \
      | tar -xz -C /usr/local/bin mediamtx
    npm run build:backend
    mediamtx mediamtx.yml &
    node dist/index.js &
    until curl -sf http://localhost:3030/healthz > /dev/null 2>&1; do sleep 1; done
    npm run test:integration
  '
```

**Port isolation**: each container has its own network namespace even with default bridge networking. Multiple containers can all bind to port 3030 simultaneously without conflict. No `--network=none` or port mapping needed.

**Lighter alternative** (Linux only, no Docker needed):

```sh
unshare --net --map-root-user bash -c '
  ip link set lo up
  # start mediamtx, app, run tests — all on isolated localhost
'
```

### Parallel work patterns

**Safe to parallelize (no coordination needed):**
- Agent A edits `src/api/outputs.ts`, Agent B edits `public/ts/features/editor.ts` — different layers, separate worktrees, unit tests only.
- Agent A adds a new API route, Agent B fixes a CSS issue — no file overlap.
- Multiple agents all running `npm run build` or unit tests in their own worktrees.

**Requires coordination (talk to the orchestrator):**
- Two agents both modifying `src/db/schema.ts` — schema changes affect everything.
- Two agents both changing `src/types.ts` — shared type definitions will conflict.
- Any agent changing `src/index.ts` — app composition wiring, high conflict risk.
- Integration tests — only one can run on bare metal at a time; use containers for parallel runs.

**Merge strategy:**
- Each agent works on a feature branch in its own worktree.
- Rebase onto `master` before merging: `git rebase master` from the worktree.
- If two agents touched adjacent files, the second to merge resolves conflicts.
- Run `npm run build && npm test` after rebase to verify nothing broke.

### Checklist for spinning up a parallel agent

1. Create a worktree: `git worktree add ../restream-<name> -b <branch>`
2. `cd ../restream-<name> && npm ci`
3. `npm run build` — verify clean build
4. Make changes
5. `npm run build && npm test` — verify in worktree
6. If integration test needed: run in container (see above)
7. Rebase onto master, resolve conflicts
8. Clean up: `git worktree remove ../restream-<name>`
