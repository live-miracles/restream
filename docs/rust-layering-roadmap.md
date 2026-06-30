# Rust Layering Roadmap

This document turns the architectural audit into an execution order that is
safe for an active repo: narrow seams first, crate splits later.

## Current Shape

The codebase already has promising boundaries:

- `domain` for typed graph vocabulary
- `planner` for backend-selection policy
- `media` for packet/runtime/backend code
- `db` for persistence
- `api` for the HTTP/UI edge

The main remaining issue is not "missing modules." It is cross-layer
dependency flow:

- planner depends on media backend parsing
- runtime core emits API-shaped JSON
- protocol handlers read raw SQL directly
- some config/domain schemas still live inside runtime modules

## What We Already Moved

Two low-risk extractions already landed:

1. Audio-routing grammar now lives in `domain`.
   - `d7467fd` `Move audio routing grammar into domain`
2. Transcode-profile schema now lives in `domain`.
   - `de877b8` `Move transcode profile schema into domain`

These are useful because they move "what this means" out of "how this runs."

Two more narrow seams have now landed:

3. SRT ingest config and validation live in `domain`.
   - `618295c` `Move SRT ingest schema into domain`
4. Ingest security policy config lives in `domain`.
   - `670a41f` `Move ingest security config into domain`
5. Logging DTOs live in `logging::types`.
   - `04ac53a` `Move log DTOs into logging module`

## Layering Ladder

When deciding whether to use a file, module, trait, or crate, prefer the
lightest boundary that prevents the wrong coupling.

### 1. File split

Use when the problem is readability or merge pressure, not ownership.

Good target here:

- split `api.rs` by route family

### 2. Module

Use when one concept should own its types, parsing, validation, and helpers,
but still live in the same crate and dependency graph.

Good examples in this repo:

- `domain::audio_routing`
- `domain::transcode_profile`
- `domain::srt_ingest`
- `domain::ingest_security`

### 3. Visibility boundary

Use `pub`, `pub(crate)`, and `pub(super)` to turn modules into real seams.

Rule of thumb:

- `domain` should expose stable typed meaning
- runtime helpers inside `media` should stay narrow
- `api` should depend on edge-facing models, not internal helper state

### 4. Newtypes and enums

Use them when stringly-typed coupling is the problem.

Good examples:

- stage vocabulary in `domain::stage`
- resolved ingest/security policy enums in `domain`

### 5. Traits

Use traits when one layer should depend on a capability instead of a concrete
implementation.

Best next use in this repo:

- make RTMP/SRT ingest depend on a lookup port instead of raw DB access

That seam has now started:

- `application::ports::PipelineLookup`
- `application::ingest` for stream-key auth orchestration

### 6. Crate

Use a crate only after the module boundary is already stable.

Signals that a crate split is justified:

- the API can be described in one sentence
- it should not depend on `axum`, `sqlx`, or FFmpeg bindings
- compile-time or dependency isolation is actually valuable

That makes crates the last step, not the first.

## Refactor Order

### 1. Finish Domain Schema Extraction

Goal: keep pure typed config and parsing out of runtime backends.

Best next candidates:

- SRT ingest configuration types and validation
- ingest security configuration
- output encoding / stage-resolution request types

Target outcome:

- `domain` owns typed config and validation
- `media` and `api` consume those types
- `types.rs` shrinks toward DB/API DTOs instead of being a catch-all

Progress so far:

- done: SRT ingest configuration types and validation
- done: ingest security configuration
- done: logging DTO ownership
- done: application-owned output-path stage resolution
- next: remaining output/stage resolution request types that still leak through DTO catch-alls

### 2. Add an Application Layer

Goal: centralize orchestration decisions that are currently duplicated or
implicitly spread across `lib`, `planner`, and `media`.

Recommended new module:

- `src/application/`

Suggested contents:

- `output_path.rs`
  - resolves source/video/audio/codec-edge path for an output
- `ports.rs`
  - traits for pipeline lookup, auth lookup, config lookup
- `reconcile.rs`
  - typed reconciler decisions, separated from runtime wiring

Why first:

- it reduces duplication before crate splitting
- it provides a clean home for policy that should not live in `lib.rs`

Progress so far:

- started: `application::ports::PipelineLookup`
- started: `application::ports::PipelineCatalog`
- started: `application::ports::MetaStore`
- started: `application::egress` shared output stage/ring preparation for lib runtime wiring
- started: `application::ingest` stream-key lookup/auth helpers
- started: `application::ingest_security` MetaStore-backed config loading
- started: `application::recording` meta-backed recording enablement helpers for lib/api orchestration
- started: `application::recording` settings load/save extracted from `media::recording`
- started: `application::recording` recording task launch orchestration shared by lib/api
- started: `application::reconcile` output retry/stop, recording plan, and stage-sweep decisions
- started: `application::srt_ingest` global config lookup via port
- started: `application::srt_ingest` shared policy-store load/refresh orchestration for lib/api
- done: `application::output_path` for source/video/audio/codec-edge planning
- next: move more spawn/wiring orchestration from `lib.rs` into `application::reconcile`

### 3. Move Runtime Views Out Of The Engine Core

Goal: `MediaEngine` should return typed state and snapshots, not primarily
`serde_json::Value`.

Recommended split:

- `media::engine`
  - registries, lifecycle, typed snapshots
- `media::engine_views`
  - temporary compatibility layer
- `api::serializers` or `api::view_models`
  - HTTP-facing JSON shape

Success condition:

- engine code no longer needs to know UI/HTTP serialization details
- JSON assembly happens at the edge

Progress so far:

- started: top-level `api_view_models` helpers for egress/probe/ring payload JSON
- started: health snapshot pipeline/input/hls JSON helpers in `api_view_models`
- started: telemetry row, queue, and ring JSON helpers in `api_view_models`
- started: processing graph node/edge serializers in `api_view_models`
- next: move broader graph serialization out of `media::engine_views`

### 4. Replace Raw SQL Lookups In Protocol Handlers

Goal: RTMP and SRT should depend on a lookup port, not query text.

Current smell:

- protocol modules issue direct stream-key SQL queries

Target:

- `application::ports::PipelineLookup`
- `db` implements the trait
- ingest protocols receive the port or a repository wrapper

Benefits:

- better layering
- easier testing
- future crate split for protocol stacks becomes realistic

Progress so far:

- RTMP/SRT can be switched from inline SQL to a shared lookup port
- the next step is extending that port pattern to other ingest-side lookups

### 5. Split `api.rs` By Route Family

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

This is mostly a maintainability move, but it pays off immediately in active
parallel work because it narrows file ownership.

## Crate Candidates

Do this only after the dependency cleanup above.

### Candidate A: `media-core`

Likely contents:

- `ring_buffer`
- `avio`
- `codec`
- `mpegts`
- `feeder`
- `timing`
- `ts_chunk_ring`

Precondition:

- shared metadata types must no longer be anchored in `engine`

### Candidate B: `restream-domain`

Likely contents:

- stage identifiers
- audio-routing grammar
- transcode-profile schema
- future SRT ingest and security config schemas

This can stay as a module for a while; it only becomes a crate when compile
boundaries or reuse justify it.

### Candidate C: `restream-application`

Likely contents:

- output path resolution
- reconciler decisions
- repository/lookup traits
- approval / operation orchestration over domain objects

This crate is optional, but it often becomes the best home once `lib.rs`
stops being the orchestration bucket.

## What Should Not Be Split Yet

### `planner`

Keep it as a module for now.

Reason:

- it is still small
- it is still close to runtime policy
- the bigger win is moving more parsing and decision objects into shared
  domain/application code first

### `db`

Keep it in the main crate until repository traits exist.

Reason:

- splitting persistence before callers stop depending on raw `sqlx` details
  just spreads coupling across crates

## Working Rules

When making layering changes, prefer this order:

1. Move the type or parser.
2. Repoint callers.
3. Preserve compatibility with re-exports if helpful.
4. Only then move files or split crates.

When choosing the next refactor in an active worktree:

- avoid hot files already under parallel edit
- prefer pure-type extractions
- prefer compatibility-preserving moves over signature churn
- commit each seam independently

## Immediate Next Steps

Best next low-risk code steps:

1. Introduce `application::output_path` and remove duplicated output-path
   resolution logic from `lib.rs`.
2. Extend application ports beyond pipeline lookup where ingest/runtime still
   reaches into DB details directly.
3. Start converting engine JSON emitters into typed snapshots plus edge
   serializers.

That sequence keeps progress real without forcing a risky big-bang rewrite.
