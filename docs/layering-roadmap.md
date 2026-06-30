# Layering Roadmap

This document turns the layering audit into an execution order that is safe for
an active repo: narrow seams first, broader packaging later.

## Current Shape

The backend already has promising boundaries:

- `domain` for typed graph vocabulary
- `planner` for backend-selection policy
- `media` for packet/runtime/backend code
- `db` for persistence
- `api` for the HTTP/UI edge

The frontend now also has a clearer shape:

- `public/ts/app` for dashboard composition/bootstrap
- `public/ts/core` for shared transport, state, and pure transforms
- `public/ts/features` for bounded UI modules
- `public/ts/history` for history-specific controller/rendering behavior

The remaining issue is not "missing modules." It is cross-layer dependency flow.

Backend examples:

- planner depends on media backend parsing
- runtime core emits API-shaped JSON
- protocol handlers read raw SQL directly
- some config/domain schemas still live inside runtime modules

Frontend examples:

- large feature modules still mix rendering, async coordination, and cross-feature wiring
- some feature modules still import peer features because the composition owner is not yet narrow enough
- globals/window hooks remain as a compatibility surface that should stay edge-facing

## Ownership Matrix

Use this matrix before extracting a new module, trait, crate, or frontend app boundary.

### Backend `domain`

Owns:

- meaning
- validation
- parsing
- shared typed vocabulary

Does not own:

- SQL
- runtime caches
- HTTP response shape

### Backend `application`

Owns:

- orchestration
- persistence policy
- shared multi-step workflows
- ports/capabilities that isolate storage from orchestration

Does not own:

- raw SQL
- packet-level runtime behavior
- HTTP transport details

### Backend `db`

Owns:

- raw queries
- schema-aware CRUD

Does not own:

- workflow policy
- cross-layer orchestration

### Backend `media`

Owns:

- runtime state
- protocol loops
- hot-path transforms
- caches/defaults used directly by runtime consumers

Does not own:

- persistence serialization policy
- API-facing JSON contracts
- duplicated control-plane orchestration

### Backend `api`

Owns:

- request validation
- auth checks
- status codes
- edge/view shaping

Does not own:

- reusable orchestration
- runtime internals
- persistence policy

### Frontend `app`

Owns:

- bootstrap/composition wiring
- feature dependency assembly
- page-level mode orchestration

Does not own:

- low-level fetch helpers
- reusable render-hot widget logic
- feature-local DOM details

### Frontend `core`

Owns:

- shared transport helpers
- shared state
- URL/session helpers
- pure transforms and formatting shared across features

Does not own:

- cross-feature composition
- feature-local DOM ownership
- dashboard mode orchestration

### Frontend `features`

Owns:

- bounded UI rendering
- feature-local interaction logic
- feature-local transient state

Does not own:

- app-wide composition wiring
- shared transport primitives that multiple features depend on
- unrelated peer-feature orchestration

### Frontend `history`

Owns:

- history polling state
- history-specific render models
- history modal rendering and controls

Does not own:

- unrelated dashboard composition
- shared transport primitives beyond what it consumes from `core`

## What We Already Moved

Backend low-risk extractions already landed:

1. Audio-routing grammar now lives in `domain`.
2. Transcode-profile schema now lives in `domain`.
3. SRT ingest config and validation live in `domain`.
4. Ingest security policy config lives in `domain`.
5. Logging DTOs live in `logging::types`.

Frontend low-risk extractions already landed:

1. Dashboard feature wiring now has an `app` composition root.
2. Pipeline output-list rendering and delegated actions now live outside `pipeline-view.ts`.

These moves are useful because they move "how the app is composed" away from
"how one feature renders."

## Layering Ladder

When deciding whether to use a file, module, trait/interface, crate, or frontend
app boundary, prefer the lightest boundary that prevents the wrong coupling.

### 1. File split

Use when the problem is readability or merge pressure, not ownership.

Good targets here:

- split `api.rs` by route family
- split oversized frontend feature files by one real concept

### 2. Module

Use when one concept should own its types, parsing, validation, helpers, and
local state, but still live in the same crate/folder and dependency graph.

Good backend examples in this repo:

- `domain::audio_routing`
- `domain::transcode_profile`
- `domain::srt_ingest`
- `domain::ingest_security`

Good frontend examples in this repo:

- `public/ts/features/pipeline-output-list`
- `public/ts/features/pipeline-dependencies`

### 3. Visibility boundary

Use `pub`, `pub(crate)`, folder exports, and narrow import surfaces to turn
modules into real seams.

Rule of thumb:

- `domain` should expose stable typed meaning
- runtime helpers inside `media` should stay narrow
- frontend `core` should expose stable helpers, not feature internals
- frontend `features` should depend on `core` or `app`, not many peer features

### 4. Newtypes, contracts, ports, and interfaces

Use them when stringly-typed or concrete-implementation coupling is the problem.

Backend examples:

- stage vocabulary in `domain::stage`
- resolved ingest/security policy enums in `domain`
- lookup traits in `application::ports`

Frontend examples:

- explicit dependency bags for feature actions
- typed state envelopes and shared feature contracts

### 5. Crate or package boundary

Use a crate or package boundary only after the module boundary is already stable.

Signals that a split is justified:

- the API can be described in one sentence
- it should not depend on `axum`, `sqlx`, FFmpeg bindings, or unrelated feature DOM code
- compile-time, bundling, or dependency isolation is actually valuable

That makes crate/package splits the last step, not the first.

## Refactor Order

### 1. Finish backend domain and application ownership cleanup

Goal: keep pure typed config and orchestration out of runtime backends.

Still-useful next candidates:

- remaining output/stage-resolution request types
- more spawn/wiring orchestration from `lib.rs` into `application::reconcile`
- remaining inline ingest-side DB lookups behind `application` ports

### 2. Keep runtime views out of the engine core

Goal: `MediaEngine` should return typed state and snapshots, not primarily
`serde_json::Value`.

Success condition:

- engine code no longer needs to know UI/HTTP serialization details
- JSON assembly happens at the edge

### 3. Continue frontend composition cleanup

Goal: keep cross-feature coordination in `public/ts/app`, not in oversized
feature modules.

Still-useful next candidates:

- move additional dashboard mode orchestration into focused app-owned helpers when that removes real coupling
- split oversized feature modules only when one concept clearly owns its state and render path
- keep hot refresh paths such as output cards and high-frequency dashboard rerenders optimized for DOM reuse

### 4. Replace raw SQL lookups in protocol handlers

Goal: RTMP and SRT should depend on lookup ports, not query text.

Target:

- `application::ports::PipelineLookup`
- `application::ports::IngestLookup`
- `db` implements the trait
- ingest protocols receive the port or a repository wrapper

### 5. Split `api.rs` by route family

Goal: make edge-layer ownership obvious and reduce merge collisions.

Suggested structure:

- `api/mod.rs`
- `api/auth.rs`
- `api/pipelines.rs`
- `api/outputs.rs`
- `api/history.rs`
- `api/telemetry.rs`
- `api/agent.rs`
- `api/hls.rs`

## What Should Not Be Split Yet

### Backend `planner`

Keep it as a module for now.

Reason:

- it is still small
- it is still close to runtime policy
- the bigger win is moving more parsing and decision objects into shared
  domain/application code first

### Backend `db`

Keep it in the main crate until repository traits exist.

Reason:

- splitting persistence before callers stop depending on raw `sqlx` details
  just spreads coupling across crates

### Frontend features under active UI churn

Keep them whole until the next move removes real dependency flow.

Reason:

- splitting a large feature without changing ownership just creates wrapper files
- render-hot code needs proof that DOM churn or refresh cadence did not regress

## Working Rules

When making layering changes, prefer this order:

1. Move the type, helper, or owner concept.
2. Repoint callers.
3. Preserve compatibility with re-exports if helpful.
4. Only then move files, split crates, or add app-level composition seams.

When choosing the next refactor in an active worktree:

- avoid hot files already under parallel edit
- prefer pure-type or pure-helper extractions
- prefer compatibility-preserving moves over signature churn
- benchmark runtime hot paths and high-frequency frontend refresh paths when touched
- commit each seam independently

## Immediate Next Steps

Best next low-risk code steps:

1. Extend backend application ports where ingest/runtime still reaches into DB details directly.
2. Continue converting engine JSON emitters into typed snapshots plus edge serializers.
3. Keep moving dashboard composition concerns into `public/ts/app` only when that removes real cross-feature coupling.
4. Split additional frontend feature modules only where one concept clearly owns the moved state and tests can prove behavior/performance stayed intact.

That sequence keeps progress real without forcing a risky big-bang rewrite.
