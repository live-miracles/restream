# Parallel Agent Framework

This document proposes a repo-specific framework for letting multiple agents
work in parallel without corrupting each other's code, artifacts, ports, or
runtime measurements.

It is a planning document, not a claim that every item below is already
implemented.

## Why This Repo Needs A Specific Framework

This repository already has strong building blocks for isolation:

- `git worktree` clones are already being used under `worktrees/`.
- `scripts/resource-limit` serializes heavy commands and sizes build jobs.
- `src/bin/test_harness.rs` already isolates live correctness modes with
  `unshare --net --user --map-root-user` when available.
- `test_harness suite` already parallelizes correctness-only modes and keeps
  bench/measurement modes serial.
- the root [Dockerfile](/home/krsna1729/code/github/live-miracles/restream/Dockerfile)
  already has a reusable `native-deps` stage with Rust, Node, FFmpeg, and
  MediaMTX installed.

The repo also has one important hazard that makes ad hoc parallel work unsafe:

- `scripts/resource-limit` locks per repo root, not per host. In a multi-worktree
  setup, each worktree gets its own `.build-lock`, so two agents can still start
  heavy `cargo` work at the same time on the same 8 GB WSL2 host.

That means we should treat parallel-agent work as four separate coordination
problems:

1. source isolation
2. build isolation
3. live-runtime isolation
4. artifact ownership

## Goals

- Keep agents from editing the same checkout.
- Prevent concurrent heavy Rust builds from exhausting WSL2 memory.
- Allow correctness-oriented live harness runs to overlap when safely isolated.
- Keep measurement runs comparable by enforcing serial execution.
- Make one-off debug sessions reproducible instead of "who has port 3030 now?"

## Workload Classes

| Workload | Default isolation | Parallel policy | Notes |
|---|---|---|---|
| Code reading / editing | Git worktree | Many in parallel | One worktree per agent/task |
| Rust / frontend unit tests | Git worktree + host-global build lock | Parallel only for light scoped tests after build, otherwise queue | Never mix with a live compile-heavy run on this host |
| Live harness correctness | Docker if available, else native netns | Small bounded parallel batches | Use unique `WORK_DIR` roots always |
| Live measurement / sweep / bench harness | Dedicated worktree or container | Serial only | Bench profile only, no overlap |
| One-off UI/debug sessions | Docker with published ports if available, else host net with reserved port block | At most one host-net session per person | Prefer long-lived named work dirs |

## Standard Layout

Every agent gets its own worktree and runtime root.

### Worktree naming

Use:

```text
worktrees/<task-or-agent-id>
```

Examples:

```text
worktrees/api-contract-opt
worktrees/frontend-layering
worktrees/agent-hls-recovery
```

### Runtime roots

Inside each worktree, reserve these paths:

```text
worktrees/<id>/
  test/artifacts/agents/<run-id>/
  .agent-state/
```

Use:

- `WORK_ROOT=<worktree>/test/artifacts/agents/<run-id>` for aggregate harness runs
- `WORK_DIR=<worktree>/test/artifacts/agents/<run-id>/<mode>` for single-mode runs
- `.agent-state/` for local notes such as port reservations or claimed files

The key rule is simple: no two agents share a `WORK_DIR` or `WORK_ROOT`.
When the worktree is no longer needed, the owning agent should remove it with:

```sh
scripts/agent-worktree.sh --cleanup <id>
```

## Framework Rules

## 1. Source Isolation: One Agent, One Worktree

Each agent works in its own git worktree and branch. No agent edits the main
checkout directly unless it is the final integrator.

Recommended flow:

```sh
git worktree add worktrees/<id> -b codex/<id> HEAD
```

Rules:

- one task branch per worktree
- one agent per worktree
- never run destructive cleanup across all worktrees
- use hunk-based merges/cherry-picks when integrating overlapping changes

## 2. Build Isolation: One Host-Wide Heavy Cargo Lane

This is the most important repo-specific rule.

Because the WSL2 note in `AGENTS.md` warns that a Rust build plus live pipeline
can crash the VM, the host should have a single heavy-build lane across all
worktrees.

### Required policy

- only one heavy `cargo build`, `cargo test`, `cargo clippy`, or bench compile
  at a time across the whole host
- do not run heavy compile work while a live pipeline or live harness stack is
  active
- use scoped tests first; do not fan out multiple full `cargo test` commands
  from different worktrees

### Repo change to make this enforceable

Add a shared lock override to `scripts/resource-limit`, for example:

```text
RESTREAM_BUILD_LOCK_FILE=/tmp/restream-build.lock
```

Then all worktrees can opt into the same lock file instead of separate
`<worktree>/.build-lock` files.

If we want "safe by default", the better version is:

- teach `scripts/resource-limit` to prefer `/tmp/restream-build.lock`
- allow an override only when someone explicitly wants a different lock

### Practical scheduling policy

On this host:

- heavy Rust compile lane: `1`
- bench compile lane: `1` and it is the same lane as heavy Rust compile
- frontend `npm run test:frontend`: can run in parallel with editing, but avoid
  overlapping it with heavy Rust compiles when memory is tight

### Cache reuse policy

To keep new worktrees cheap, split build outputs into two buckets:

- shared native/static outputs that are expensive and rarely change
- per-worktree Rust incremental state that should be copied from a warm tree at
  setup time

#### Share static native outputs when the task does not touch them

Agents should reuse the repo's static native build outputs when the task does
not modify the native toolchain or static dependency layer.

Safe-to-share examples:

- FFmpeg static prefix
- SRT static prefix
- x264/x265 static outputs
- other outputs produced by `scripts/setup-static-build.sh` and
  `scripts/build-static.sh`

Do not assume these shared outputs are reusable if the task edits files such as:

- `scripts/setup-static-build.sh`
- `scripts/build-static.sh`
- `scripts/bootstrap-dev.sh`
- `Dockerfile`
- `build.rs`
- native fixture/probe sources under `test/*.c`
- dependency manifests or flags that affect native linkage

Recommended policy:

- keep one warm authoritative static build tree
- expose that static output to agent worktrees as read-only or by explicit sync
- rebuild it only when a task actually changes the native/static layer

In practice, that means a worktree setup helper should prefer reusing:

```text
.build/static/
public/bin/ffmpeg
```

instead of rebuilding FFmpeg/SRT/x264/x265 for every agent branch.

#### Copy incremental caches into a new worktree

When creating a new worktree, copy the warm incremental build caches from an
existing healthy worktree so the first agent compile is cheap.

Recommended seed set:

```text
target/debug/deps
target/debug/build
target/debug/.fingerprint
.cargo/
node_modules/
```

Most important subtrees:

- built dependency artifacts under `target/debug/deps/`
- build-script outputs under `target/debug/build/`
- Cargo fingerprint state under `target/debug/.fingerprint/`
- optional, pruned incremental slices under `target/debug/incremental/`

Recommended approach:

- copy or `rsync` caches immediately after `git worktree add`
- do this before the new agent starts compiling
- default to a pruned high-value debug subset instead of cloning the entire
  `target/` tree
- treat the copied caches as a starting point owned by the new worktree after
  setup; do not have multiple worktrees writing into the exact same `target/`
  directory concurrently

Example setup shape:

```sh
git worktree add worktrees/<id> -b codex/<id> HEAD
rsync -a --delete <source-worktree>/target/debug/deps/ worktrees/<id>/target/debug/deps/
rsync -a --delete <source-worktree>/target/debug/build/ worktrees/<id>/target/debug/build/
rsync -a --delete <source-worktree>/target/debug/.fingerprint/ worktrees/<id>/target/debug/.fingerprint/
rsync -a <source-worktree>/.cargo/ worktrees/<id>/.cargo/
rsync -a <source-worktree>/node_modules/ worktrees/<id>/node_modules/
```

For the static/native layer, prefer sharing or one-way syncing from the warm
source tree instead of rebuilding it in each new worktree.

## 3. Live Harness Isolation: Docker First, Native Netns Second

For live correctness runs, use Docker when available because it gives stronger
process, filesystem, and port isolation than a shared host shell.

When Docker is unavailable, fall back to the repo's existing network-namespace
flow via `unshare`.

### Docker-first model

Reuse the existing root Dockerfile's `native-deps` stage as the dev/harness
image:

```sh
docker build --target native-deps -t restream/dev .
```

Then run harness work inside a container backed by one worktree:

```sh
RESTREAM_BUILD_LOCK_FILE=/tmp/restream-build.lock \
scripts/resource-limit cargo build --bin restream --bin test_harness

docker run --rm \
  --network none \
  --tmpfs /tmp:exec,mode=1777 \
  -v "$PWD":/workspace \
  -w /workspace \
  restream/dev \
  bash -lc 'WORK_DIR=test/artifacts/agents/run-1/mixed-h264-srt-single target/bench/test_harness mixed-h264-srt-single'
```

Why `--network none` works well here:

- live harness traffic is loopback-only inside the container
- no host port collisions
- no accidental cross-talk with another agent's MediaMTX or restream process

Important:

- use Docker primarily for runtime isolation, not as a way to bypass the
  host-wide build lane
- until `scripts/resource-limit` supports a shared host lock file, do not let
  multiple containers compile Rust concurrently

### Native fallback

When Docker is not available, rely on the current repo behavior:

- `test_harness` re-execs into a private network namespace unless `--no-netns`
  is set
- `suite` already parallelizes only correctness modes when namespace isolation
  exists

Native fallback command shape:

```sh
scripts/resource-limit target/bench/test_harness mixed-h264-srt-single
```

or:

```sh
WORK_DIR=test/artifacts/agents/run-1/mixed-h264-srt-single \
target/bench/test_harness mixed-h264-srt-single
```

## 4. Measurement Isolation: Never Parallelize Bench-Like Runs

The repo already encodes this rule and the framework should keep it strict.

These modes stay serial:

- `ramp-family`
- `mixed-h264-rtmp-single`
- `mixed-h264-srt-single`
- `mixed-h265-srt-single`
- `mixed-h264-srt-multi`
- `mixed-h265-srt-multi`
- `mixed-input-matrix`
- `resource-sweep`
- `bitrate-sweep`
- `branch-matrix`
- `srt-crypto-matrix`

Reasons:

- they require bench-profile binaries for valid comparison
- CPU and RSS numbers stop being meaningful under overlap
- they often spawn FFmpeg and MediaMTX trees that are large enough to distort
  nearby runs

Framework rule:

- correctness agents may overlap in bounded batches
- measurement agents get exclusive access to the host runtime lane

## 5. One-Off Debug Sessions: Treat Them As Named Environments

Ad hoc debugging is where agents usually step on each other.

Do not run "some restream on 3030 somewhere." Instead, each debug session gets:

- its own worktree
- its own named runtime dir
- its own explicit port block
- its own shutdown/cleanup command

### Docker debug session

Use Docker when the goal is "I want a stable UI and live traffic without
fighting the host."

Recommended pattern:

```sh
docker run --rm \
  --name restream-debug-<id> \
  --tmpfs /tmp:exec,mode=1777 \
  -p 39280:39280 \
  -p 32080:32080 \
  -p 31280:31280/udp \
  -p 33080:33080 \
  -p 34080:34080/udp \
  -p 35080:35080 \
  -v "$PWD":/workspace \
  -w /workspace \
  restream/dev \
  bash -lc 'WORK_DIR=/tmp/restream-live-<id> RESTREAM_HTTP_PORT=39280 RESTREAM_RTMP_PORT=32080 RESTREAM_SRT_PORT=31280 cargo run'
```

This mirrors the "Manual Dashboard Live Env" documented in
[docs/testing.md](/home/krsna1729/code/github/live-miracles/restream/docs/testing.md).

### Native debug fallback

If Docker is unavailable, reserve a port block before starting:

| Session type | HTTP | RTMP | SRT | MediaMTX RTMP | MediaMTX SRT | MediaMTX HLS |
|---|---:|---:|---:|---:|---:|---:|
| Debug A | 39280 | 32080 | 31280 | 33080 | 34080 | 35080 |
| Debug B | 39281 | 32081 | 31281 | 33081 | 34081 | 35081 |

Store the reservation in:

```text
worktrees/<id>/.agent-state/ports.json
```

## Agent Roles

Use four explicit roles rather than letting every agent do everything.

### Role A: Editor agent

Allowed:

- read code
- edit code
- run light scoped tests

Avoid:

- long live harness runs
- measurement runs

### Role B: Unit verifier

Allowed:

- filtered `cargo test`
- `npm run test:frontend`
- contract scripts such as `scripts/check-api-contract.sh`

Rules:

- acquire the host-global build lock
- prefer one filtered verification command, not several overlapping ones

### Role C: Live harness verifier

Allowed:

- `test_harness` correctness modes

Rules:

- Docker container if available
- otherwise native netns
- dedicated `WORK_DIR`
- no concurrent heavy Rust compile

### Role D: Debug operator

Allowed:

- long-lived restream + MediaMTX + FFmpeg session
- manual UI inspection
- browser-driven debugging

Rules:

- explicit port block
- named runtime dir
- one host-net debug session per operator unless Docker-isolated

## Recommended Parallelism Limits For This Host

Start conservative on the current WSL2 machine:

- code-edit agents: many
- heavy Rust compile jobs: `1`
- correctness live harness containers/namespaces: `2` max, and only when no
  compile-heavy job is active
- measurement runs: `1`
- host-network debug sessions: `1`

The correctness cap of `2` is intentionally lower than the harness's theoretical
parallelism because this repo spawns real FFmpeg and MediaMTX processes and the
machine budget is more constrained than the harness API surface.

## Concrete Command Patterns

## Worktree setup

```sh
git worktree add worktrees/agent-rtmp -b codex/agent-rtmp HEAD
git worktree add worktrees/agent-hls -b codex/agent-hls HEAD
```

## Scoped unit verification

```sh
RESTREAM_BUILD_LOCK_FILE=/tmp/restream-build.lock \
scripts/resource-limit cargo test --test api health_endpoint_exposes_probe_and_egress_fault_fields -- --nocapture
```

## Correctness live harness in Docker

```sh
RESTREAM_BUILD_LOCK_FILE=/tmp/restream-build.lock \
scripts/resource-limit cargo build --bin restream --bin test_harness

docker run --rm \
  --network none \
  --tmpfs /tmp:exec,mode=1777 \
  -v "$PWD":/workspace \
  -w /workspace \
  restream/dev \
  bash -lc 'WORK_DIR=test/artifacts/agents/run-mixed-h264-srt-single target/bench/test_harness mixed-h264-srt-single'
```

## Correctness live harness with native netns fallback

```sh
WORK_DIR=test/artifacts/agents/run-fault \
target/debug/test_harness fault-resilience
```

## Serial measurement run

```sh
RESTREAM_BUILD_LOCK_FILE=/tmp/restream-build.lock \
scripts/resource-limit ./scripts/build-bench-harness.sh

WORK_DIR=test/artifacts/agents/run-sweep \
target/bench/test_harness resource-sweep
```

## Helper Scripts

The framework becomes much easier to follow if we add small helper scripts
instead of relying on tribal knowledge.

### 1. `scripts/agent-worktree.sh`

Status: implemented.

Responsibilities:

- create `worktrees/<id>`
- remove `worktrees/<id>` when the task is complete
- create branch `codex/<id>`
- initialize `.agent-state/`
- seed the new worktree from a warm source tree by copying a pruned high-value
  debug cache plus `.cargo/` and `node_modules/`
- reuse or sync shared static native outputs when the task does not touch that
  layer
- print recommended `WORK_DIR` and `WORK_ROOT`

### 2. `scripts/agent-lock-env.sh`

Status: suggested.

Responsibilities:

- export `RESTREAM_BUILD_LOCK_FILE=/tmp/restream-build.lock`
- optionally export shared Cargo and npm cache locations

### 3. `scripts/agent-harness.sh`

Status: suggested.

Responsibilities:

- detect Docker
- run correctness modes in Docker if available
- otherwise fall back to native namespace mode
- create a unique artifact root automatically

### 4. `scripts/agent-debug-env.sh`

Status: suggested.

Responsibilities:

- allocate a known port block
- write `.agent-state/ports.json`
- boot the repo's documented manual live environment

## Suggested Rollout

### Phase 1: Safe coordination defaults

1. Add host-global build lock support to `scripts/resource-limit`.
2. Document the "one worktree per agent" rule.
3. Standardize `WORK_DIR` and `WORK_ROOT` naming under `test/artifacts/agents/`.

### Phase 2: Wrapper scripts

1. `scripts/agent-worktree.sh` is in place; use it as the default agent
   worktree entrypoint.
2. Add `scripts/agent-harness.sh`.
3. Add `scripts/agent-debug-env.sh`.

### Phase 3: Docker-first live workflows

1. bless `docker build --target native-deps -t restream/dev .` as the standard
   harness image build
2. run correctness harness jobs in `--network none` containers
3. keep native `unshare` as the fallback path

### Phase 4: Optional scheduling automation

If agent concurrency grows, add a tiny queue/lease file under `/tmp` for:

- build lane ownership
- measurement lane ownership
- reserved debug port blocks

## Decision Summary

The framework for this repo should be:

- worktree-per-agent for source isolation
- host-global build lock for all heavy Rust work
- Docker-first live correctness and debug isolation when available
- native `unshare` fallback for live correctness when Docker is unavailable
- strict serial execution for measurement-oriented harness modes
- explicit artifact and port ownership per agent run

That keeps parallel editing fast while respecting the repo's real constraints:
heavy Rust builds, FFmpeg child processes, live loopback protocol tests, and
measurement sensitivity.
