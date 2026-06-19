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
cargo build                 # debug build
cargo build --release       # optimized release build (LTO, single codegen unit)
cargo test                  # run all unit + integration tests (DB + API, in-memory SQLite)
cargo fmt                   # format Rust code
cargo fmt --check           # check formatting (used in CI / pre-commit hook)
cargo clippy                # lint

# Frontend (plain TypeScript SPA, no bundler)
# public/ts/ → public/js/ via tsc
npx tsc -p tsconfig.frontend.json          # one-shot compile
npx tsc -p tsconfig.frontend.json --watch  # watch mode
npx tailwindcss -i public/input.css -o public/output.css  # Tailwind CSS

# Integration test (requires running restream binary + ffmpeg)
./test/run-2x3.sh
```

**Always edit `public/ts/` not `public/js/`** — `public/js/` is generated.
**Always run `cargo fmt` before pushing a branch or creating a PR.**

## Architecture

```
Publisher (RTMP/SRT) → restream binary (single process) → External destinations (RTMP/SRT/HLS egress)
Browser dashboard → Axum REST API → SQLite (data.db)
```

Single Rust binary replacing the old Node.js + MediaMTX + spawned FFmpeg architecture. All media transport (RTMP/SRT ingest and egress), HLS segmenting, transcoding, and recording happen in-process.

### Backend (`src/`)

- **`src/lib.rs`** — app composition: wires Axum, MediaEngine, reconciliation loop (1s tick), RTMP/SRT servers.
- **`src/api.rs`** — Axum router, all REST handlers, embedded frontend asset serving via `rust-embed`, auth (scrypt password hashing, session cookies).
- **`src/db.rs`** — SQLite schema and queries via `sqlx`. Schema created via `CREATE TABLE IF NOT EXISTS` at startup (no migrations).
- **`src/diag.rs`** — streaming SSE diagnostics.
- **`src/types.rs`** — domain types (Pipeline, Output, Job, Ingest) with `sqlx::FromRow` + `serde` derives.

### Media layer (`src/media/`)

- **`engine.rs`** — central state: active ingests, egresses, ring buffers, HLS stores, recording tokens.
- **`ring_buffer.rs`** — lock-free SPMC ring buffer (4096 slots, `ArcSwap`, 64-byte aligned).
- **`avio.rs`** — in-memory FFmpeg I/O via `MemoryQueue` (replaces TCP loopback sockets).
- **`rtmp.rs`** — RTMP ingest/egress via `rml_rtmp`.
- **`srt.rs`** — SRT ingest/egress via libsrt FFI.
- **`hls.rs`** — in-memory HLS segmenter: muxes to MPEG-TS, splits on keyframe boundaries, stores segments in `HlsStore`. No disk I/O.
- **`recording.rs`** — MKV recording muxer.
- **`transcoder.rs`** — in-process H.264/H.265 transcoder.
- **`simd.rs`** — SIMD-accelerated memcpy and sync byte scan (AVX-512/AVX2/SSE2 dispatch).
- **`security.rs`** — ingest rate limiter.

### Frontend (`public/ts/`)

Plain TypeScript/ES-module SPA with no framework. Files are served as ES modules from `public/js/`.

- **`public/ts/core/state.ts`** — single shared `state` object holding all pipeline/output view models.
- **`public/ts/core/api.ts`** — typed fetch wrappers for REST calls.
- **`public/ts/features/dashboard.ts`** — poll loop: `/health` + `/metrics/system` every 5s.
- **`public/ts/features/editor.ts`** — output/pipeline modal logic.
- **`public/ts/features/pipeline-view.ts`** — renders the output list.

### Key design constraints

- **Encoding stored as string**: `remap:0:1` means left=c0, right=c1 (0-indexed). Other encodings are plain keys (`source`, `720p`, etc.) or `custom`.
- **No DB migrations**: `CREATE TABLE IF NOT EXISTS` is run at startup.
- **Ports are hardcoded**: HTTP=3030, RTMP=1935, SRT=10080. No env override.
- **HLS is fully in-memory**: `HlsStore` keeps segments in a `VecDeque<Bytes>`, served directly by Axum handlers. No disk writes.
- **Frontend asset embedding**: `rust-embed` compiles `public/` into the binary. Disk-first fallback for dev hot-reload.

### Old codebase

The original Node.js/TypeScript codebase is archived in `old/` for reference. It is not built or tested.

---

## Testing

### Test tiers

| Tier | Command | Needs | Safe in parallel? |
|------|---------|-------|--------------------|
| **Unit + API** | `cargo test` | Rust toolchain only | Yes — uses in-memory SQLite |
| **Build** | `cargo build` | Rust toolchain + FFmpeg libs | Yes |
| **Format** | `cargo fmt --check` | Rust toolchain | Yes |
| **Lint** | `cargo clippy` | Rust toolchain | Yes |
| **Integration** | `./test/run-2x3.sh` | Running restream binary + FFmpeg | No — use container |

**Backend-only changes** (anything under `src/`): run `cargo test && cargo build`. No container needed.

**Frontend-only changes** (anything under `public/ts/`): run `npx tsc`. No container needed.

**Integration tests**: require a container because ports are hardcoded and cannot be shared.

### Containers for integration testing

The app hardcodes ports (3030, 1935, 10080) with no env-var overrides. Each container gets its own network namespace.

```sh
docker run --rm \
  -v "$(pwd)":/app -w /app \
  rust:1-bookworm bash -c '
    set -e
    apt-get update -qq && apt-get install -y -qq ffmpeg jq libavformat-dev libavcodec-dev libavutil-dev libswresample-dev libswscale-dev libavfilter-dev libavdevice-dev pkg-config clang > /dev/null 2>&1
    cargo build --release
    ./target/release/restream &
    until curl -sf http://localhost:3030/healthz > /dev/null 2>&1; do sleep 1; done
    ./test/run-2x3.sh
  '
```

**Lighter alternative** (Linux only, no Docker needed):

```sh
unshare --net --map-root-user bash -c '
  ip link set lo up
  ./target/release/restream &
  until curl -sf http://localhost:3030/healthz > /dev/null 2>&1; do sleep 1; done
  ./test/run-2x3.sh
'
```

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
- Each worktree gets its own `data.db` (SQLite creates it on first run), so no DB locking conflicts.
- `public/js/` is gitignored and generated.
- Don't commit from a worktree to a branch another agent is using.

### Parallel work patterns

**Safe to parallelize (no coordination needed):**
- Agent A edits `src/api.rs` handler logic, Agent B edits `public/ts/features/editor.ts` — different layers.
- Multiple agents all running `cargo test` or `cargo build` in their own worktrees.

**Requires coordination (talk to the orchestrator):**
- Two agents both modifying `src/db.rs` — schema changes affect everything.
- Two agents both changing `src/types.rs` — shared type definitions will conflict.
- Any agent changing `src/lib.rs` — app composition wiring, high conflict risk.
- Integration tests — only one can run on bare metal at a time; use containers for parallel runs.

**Merge strategy:**
- Each agent works on a feature branch in its own worktree.
- Rebase onto `master` before merging: `git rebase master` from the worktree.
- If two agents touched adjacent files, the second to merge resolves conflicts.
- Run `cargo test && cargo build` after rebase to verify nothing broke.

### Checklist for spinning up a parallel agent

1. Create a worktree: `git worktree add ../restream-<name> -b <branch>`
2. `cd ../restream-<name>`
3. `cargo build` — verify clean build
4. Make changes
5. `cargo test && cargo build` — verify in worktree
6. If integration test needed: run in container (see above)
7. Rebase onto master, resolve conflicts
8. Clean up: `git worktree remove ../restream-<name>`
