# Restream Platform Master Plan

## 0. Executive position

This plan treats the product as a **restreaming platform**, not as a showcase for an advanced media engine.

That changes the priorities:
- the **operator** cares about: is ingest healthy, are outputs healthy, what is degraded, what changed, what action is needed now
- the **engineer** cares about: which stage, edge, queue, worker, timeline, or codec path is responsible
- the **administrator** cares about: auth, policies, defaults, storage, retention, and operational controls

The backend can and should be deeply instrumented, graph-aware, and technically elegant, but that depth must be **in service of platform outcomes**:
- reliable ingest
- efficient shared processing
- stable multi-output restreaming
- fast incident detection
- fast diagnosis and recovery
- predictable cost and capacity

The right direction is:

1. **Keep the platform Rust-native.**
2. **Keep FFmpeg where it is strongest: codec-heavy work.**
3. **Refactor around explicit stage contracts and graph identity.**
4. **Make Rust the canonical backend/integration harness.**
5. **Build one canonical telemetry substrate and present it differently to operators and engineers.**
6. **Reset the UI/API as a clean-slate Rust restreaming control plane with no backward-compatibility drag.**

---

## 1. Product north star

### The platform promise

The system should feel like a professional restreaming platform that answers four questions continuously:

1. **Is the source healthy?**
2. **Are all required outputs healthy and fresh?**
3. **If something is degraded, where is the fault?**
4. **What is the least disruptive corrective action?**

### Product principles

- **Operator-first surface:** health, freshness, impact, action
- **Engineer-deep internals:** graph, stages, edges, queues, workers, timing
- **One control plane:** one Rust process owns the application model
- **No compatibility drag:** no old-world vocabulary or route shapes
- **Minimal duplicate work:** share expensive stages aggressively
- **Minimal hot-path tax:** instrumentation must not distort the pipeline
- **Typed correctness:** the runtime model should be encoded in types, not strings

### Non-goals

- preserving legacy Node.js / MediaMTX mental models
- exposing low-level engine detail to operators by default
- rewriting all media logic into pure Rust for ideological reasons
- adding graph richness that does not help operate or debug restreaming

---

## 2. Architecture direction

## 2.1 Recommended processing strategy

The correct architectural direction is:

### **Direction 3 — Rust platform runtime + selective FFmpeg libraries/processes**

Keep in Rust:
- ingest and egress protocols
- pipeline planning and reconciliation
- stage sharing
- ring fanout and packet routing
- packaging and transport orchestration
- API, telemetry, diagnostics, UI serving
- lifecycle management and failure handling

Use FFmpeg for:
- decode / scale / filter / encode
- codec transforms such as HEVC -> H.264 where required
- complex audio manipulation, remap, downmix, filter graphs

Do **not** expand pure-Rust codec processing beyond lightweight framing/normalization.

### Why this is the right platform choice

It gives the best practical combination of:
- maintainability
- CPU and memory efficiency
- selective shared-stage optimization
- operational clarity
- bounded implementation risk

A full external-FFmpeg model simplifies some internals but risks unnecessary memory cost and process-boundary duplication.
A full internal-libav model has the highest upside but also the highest implementation and stability risk.

The current codebase already points toward the correct answer: **Rust owns platform orchestration; FFmpeg owns codec-heavy transforms.**

Current build-out checkpoint, 2026-06-25:
- HLS HTTP/HTTPS upload egress is implemented through the shared HLS segmenter
  and PUTs playlists plus segments to the configured destination.
- Channel-level audio `remap` and `downmix` routes use external FFmpeg filter
  stages; `atrack` remains a packet-only selector.
- The opt-in internal video transcoder has decode/scale/encode coverage for
  every built-in video profile (`h264`, `720p`, `1080p`).
- The remaining release-level proof gap is the full protocol matrix from
  `docs/testing.md`, not missing HLS upload or audio-DSP primitives.

---

## 2.2 Shared-stage strategy

The engine should aggressively share work where the result can be reused safely.

### Share eagerly
- decode/scale/encode outputs for identical presets
- audio route stages when the same selection/remap is required by multiple outputs
- codec edge stages where a common transformed stream can feed multiple downstream outputs
- packaging stages when the final multiplexed output shape is identical

### Keep per-output only when necessary
- protocol-specific sender state
- auth/session/endpoint differences
- last-hop connection retry state
- delivery-specific buffering/state that cannot be safely shared

### Cost principle

The platform should optimize for:
- **one expensive transformation, many cheap sends**
- **one canonical graph, many output edges**
- **visible shared-stage lineage**

---

## 3. Refactor plan

## 3.1 Core refactor thesis

The dominant maintainability problem is not protocol code itself. It is:
- duplicated stage-feeder logic
- a god-object engine
- stringly stage identity and planning
- backend selection mixed into orchestration
- insufficiently explicit stage contracts

## 3.2 Refactor targets

### A. Introduce typed stage identity

Replace stringly stage names like:
- `video:720p`
- `audio:atrack:0:from:720p`
- `hevc_to_h264:from:source`

with typed identities:

```rust
struct PipelineId(String);
struct StageId(String);
struct WorkerId(String);

struct StageKey {
    pipeline: PipelineId,
    upstream: Option<Box<StageKey>>,
    kind: StageKind,
}

enum StageKind {
    Source,
    VideoPreset(VideoPreset),
    AudioRoute(AudioRouteSpec),
    CodecEdge(CodecEdgeSpec),
    Package(PackageSpec),
    Sender(SenderSpec),
}
```

Benefits:
- one canonical graph model
- no stage-semantics parsing from strings
- no silent collisions
- easier testing and serialization
- clearer diagnostics and UI rendering

### B. Extract a shared `StageFeeder`

This is the highest-leverage immediate refactor.

Today near-identical logic exists across:
- `external_transcoder.rs`
- `transcoder.rs`
- `h264_transcoder.rs`
- `recording.rs`
- `hls.rs`

Common responsibilities:
- ring reading
- lazy metadata acquisition
- DTS enforcement
- TS mux preparation
- audio/video conversion into TS
- batch writing to queue, accumulator, or file sink

Refactor into a reusable feeder abstraction:

```rust
struct PacketFeedConfig {
    track_policy: TrackPolicy,
    write_mode: FeedWriteMode,
    codec_hint: Option<CodecHint>,
}

trait FeedSink {
    fn on_ts_bytes(&mut self, bytes: &[u8]) -> FeedAction;
}
```

This should power:
- external FFmpeg stdin feeders
- internal transcoder feeders
- recording feeders
- HLS feeders
- any future packaging feeders

### C. Split `MediaEngine` behind a façade

`MediaEngine` should remain the façade but delegate real ownership to narrower registries:
- `PipelineRegistry`
- `StageRegistry`
- `IngestRegistry`
- `EgressRegistry`
- `HlsRegistry`
- `RecordingRegistry`
- `TelemetryRegistry`
- `DiagnosticsRegistry`

### D. Introduce planner/runtime separation

Move stage construction policy out of the reconciler loop and into a dedicated planner.

Suggested modules:
- `planner/desired_graph.rs`
- `planner/materializer.rs`
- `runtime/stage_registry.rs`
- `runtime/worker_registry.rs`
- `runtime/lifecycle.rs`

### E. Make queues, rings, and workers first-class runtime objects

This is essential for both correctness and telemetry.

The graph must include:
- stage nodes
- queue nodes
- ring nodes
- worker assignments
- transport edges
- shared-stage family membership

---

## 3.3 Refactor phases

### Phase 1 — structural cleanup without behavior change
- extract `StageFeeder`
- introduce typed enums/newtypes for protocol, state, and stage kind
- add canonical stage-key builder
- centralize graph serialization
- centralize backend-selection policy

### Phase 2 — engine decomposition
- split `MediaEngine` into registries
- move graph/state rendering out of engine core
- narrow RTMP/SRT modules to transport concerns
- isolate diagnostics and telemetry collection

### Phase 3 — stage contract hardening
- define `StageSpec`, `StageHandle`, `StageState`, `StageTelemetry`
- make all stage transitions explicit
- remove string parsing as a control mechanism

### Phase 4 — packaging and final-hop optimization
- add package-stage sharing for identical final media shapes
- benchmark sender-side duplication
- keep per-output sender state lightweight

---

## 4. Modularity and seam plan

## 4.1 Audit conclusion

The codebase is modular in low-level media utilities, but not yet modular enough in orchestration and runtime ownership.

### Strong seams
- codec utilities
- ring buffer primitives
- MPEG-TS utilities
- database layer
- some transport helpers

### Weak seams
- `MediaEngine`
- API handlers tied directly to engine internals
- stage orchestration
- graph rendering
- diagnostics ownership
- protocol modules that still mix transport and policy

## 4.2 Target module layout

```text
src/
  domain/
    ids.rs
    protocol.rs
    state.rs
    media.rs
    severity.rs
  planner/
    desired_graph.rs
    sharing.rs
    backend_policy.rs
  runtime/
    engine.rs
    pipeline_registry.rs
    stage_registry.rs
    worker_registry.rs
    lifecycle.rs
  stages/
    feeder.rs
    source.rs
    video_preset.rs
    audio_route.rs
    codec_edge.rs
    package.rs
    sender.rs
  transport/
    rtmp/
    srt/
    hls/
  telemetry/
    counters.rs
    snapshots.rs
    events.rs
    alerts.rs
  diagnostics/
    graph.rs
    health.rs
    probes.rs
    reports.rs
  api/
    routes/
    models/
  testkit/
    fixtures/
    harness/
    assertions/
```

---

## 5. Testing strategy

## 5.1 Testing goals

The platform test strategy should answer five categories of risk:

1. **Media correctness** — timestamps, muxing, codec handling, packaging
2. **Graph correctness** — stage sharing, topology, lifecycle invariants
3. **Operational correctness** — startup, restart, failure handling, backpressure
4. **Surface correctness** — API, UI, telemetry, diagnostics
5. **Performance correctness** — CPU, memory, latency, fanout efficiency

## 5.2 Testing layers

### A. Unit tests
For pure transformations and invariants:
- timestamp math
- DTS monotonicity helpers
- MPEG-TS packetization/muxing helpers
- graph key construction
- response model serialization
- alert derivation logic

### B. Property tests
Use `proptest` for:
- stage-key construction
- graph-sharing invariants
- route/planner normalization
- timestamp monotonicity under batch permutations
- ring lag / overwrite behavior invariants

### C. Concurrency/model tests
Use `loom` for:
- stage create/get-or-create races
- queue close/read/write ordering
- worker shutdown sequencing
- telemetry snapshot safety
- cancellation and restart ordering

### D. Integration tests in Rust
Rust should be the canonical backend integration harness.

Use it for:
- pipeline create/start/stop/reload
- shared-stage fanout scenarios
- mixed codec/protocol scenarios
- HLS + recording coexistence
- diagnostics and graph endpoint correctness
- queue-pressure and stage-restart scenarios

### E. Live scenario tests
Backed by a Rust harness, not bash-heavy orchestration.

Scenarios:
- single-ingest, multi-output fanout
- H.264 input to RTMP + SRT + HLS
- HEVC input with RTMP edge conversion
- mixed-scale shared transcoding
- audio-track selection variants
- output churn while source remains steady
- transient network interruption/recovery

### F. Browser tests
Keep Playwright for:
- HLS playback UX
- operator overview refresh behavior
- engineer graph interactions
- alert list rendering and event flow

Move pure front-end helper logic to lightweight TS unit tests where possible.

### G. Dependency-near probes
Keep direct C/libsrt probes where the point is to validate dependency-level behavior close to the ABI.

### H. Performance benchmarks
Add directional and regression benchmarking for:
- per-stage RSS
- per-stage CPU
- queue pressure under fanout
- package-stage sharing savings
- stage startup time
- HLS segment publish latency
- telemetry overhead on hot path

## 5.3 Test gaps to close

Current likely gaps:
- telemetry contract tests
- operator alert derivation tests
- failure-injection around queue saturation and stage death
- graph invariants as property tests
- concurrency/race coverage for registries
- capacity reporting correctness
- freshness/staleness rendering in UI/API

## 5.4 CI tiers

### PR tier
- unit tests
- property tests subset
- fast integration tests
- API contract tests

### main branch tier
- full Rust integration suite
- browser tests
- selected live scenario tests

### nightly/perf tier
- full scenario matrix
- benchmark regressions
- failure-injection scenarios
- soak tests

---

## 6. Telemetry, diagnostics, and reporting plan

## 6.1 Telemetry design principle

Build **one canonical telemetry substrate** and expose it in two views:
- **operator view**: low-cardinality, health/action focused
- **engineer view**: graph-deep, node/edge/queue/worker focused

Do **not** build two telemetry systems.

## 6.2 Hot-path instrumentation rule

On the hot path, allow only:
- atomics
- cheap counters
- bounded timestamps
- lightweight state transitions

Do not do heavy aggregation, rendering, or report assembly on the media path.
Those should be performed by snapshotters and API/reporting layers.

## 6.3 Canonical telemetry objects

### Engine level
- health
- build/runtime info
- feature availability
- capacity summary
- event rate
- alert summary
- storage state
- ingestion/output totals

### Pipeline level
- desired state
- actual state
- source freshness
- output freshness
- shared-stage health summary
- A/V sync band
- throughput
- recording/HLS state
- current alerts

### Stage level
- lifecycle state
- packets/frames/bytes in/out
- batch counts
- last progress time
- restart count
- last error/warning
- CPU time estimate
- blocked/idle time
- worker assignment

### Edge level
- source node / destination node
- stream kind
- bytes/sec / frames/sec
- queue depth
- lag / residence estimate
- freshness
- drop/overwrite/discontinuity counts

### Queue/ring level
- depth
- high-water mark
- producer blocked count/time
- consumer idle count/time
- overwrite count
- oldest unread age estimate
- reader lag estimate

## 6.4 Diagnostics plane

Diagnostics should provide evidence, not just counters.

Examples:
- recent warnings/errors per stage
- derived anomaly explanations
- stale-output reason chain
- queue-pressure reason chain
- last successful segment / packet / send timestamps
- restart history
- graph diff across recent generations

## 6.5 Alerts model

Use typed alerts with:
- severity
- scope (`engine | pipeline | stage | edge | output`)
- title
- cause summary
- evidence references
- recommended next action
- first seen / last seen / acknowledged state

---

## 7. Operator-level vs engineer-level exposure

The operator does **not** care how fancy the engine is.
The operator cares that the platform is receiving a stream and delivering healthy outputs.

So the platform should present the same telemetry differently for each persona.

## 7.1 Operator experience

### Primary questions
- Is the platform healthy?
- Which pipelines are degraded?
- Which destinations are at risk?
- Is the source stale or unstable?
- What action should I take?

### Operator UI surfaces
- **Overview**
  - total pipelines
  - active pipelines
  - degraded pipelines
  - failed outputs
  - source freshness summary
  - storage/capacity warnings
- **Pipeline list**
  - one row/card per pipeline
  - source state
  - output state rollup
  - freshness age
  - current incidents
  - recommended action
- **Pipeline detail (operator)**
  - source panel
  - outputs panel
  - health timeline
  - current alerts
  - last change
  - preview where useful

### Operator design rules
- no raw graph by default
- no queue depths by default
- no worker IDs by default
- all numbers interpreted into health bands
- every degraded state should have a human action hint

### Operator API surfaces
- `GET /api/v1/overview`
- `GET /api/v1/pipelines`
- `GET /api/v1/pipelines/{id}/summary`
- `GET /api/v1/alerts`
- `GET /api/v1/events?audience=operator`

## 7.2 Engineer experience

### Primary questions
- Which stage or edge is unhealthy?
- Is the problem source-side, transform-side, package-side, or sender-side?
- Is a queue filling, a reader lagging, or a worker stalled?
- Did graph topology change unexpectedly?
- What exactly happened and when?

### Engineer UI surfaces
- **Graph explorer**
  - all instantiated nodes and edges
  - shared-stage families highlighted
  - queue/ring nodes visible
  - worker placement visible
- **Pipeline deep dive**
  - per-stage state
  - throughput
  - queue depth
  - lag
  - errors/restarts
- **Timeline/events**
  - lifecycle transitions
  - warning/error stream
  - output flaps
  - source discontinuities
- **Diagnostics pane**
  - evidence
  - anomaly derivation
  - raw and interpreted counters

### Engineer design rules
- timestamps everywhere
- raw counters available
- freshness shown for every sample
- allow overlays for throughput, lag, queue depth, errors

### Engineer API surfaces
- `GET /api/v1/engine/telemetry`
- `GET /api/v1/pipelines/{id}/graph`
- `GET /api/v1/pipelines/{id}/telemetry`
- `GET /api/v1/pipelines/{id}/diagnostics`
- `GET /api/v1/stages/{id}/telemetry`
- `GET /api/v1/events?audience=engineer`

---

## 8. BESS-style graph richness adapted for media

The goal is not to imitate BESS for its own sake. The goal is to provide the same **live instantiated-graph clarity** that makes debugging intuitive.

What operators need from that model:
- confidence that the platform understands the real runtime topology
- clear incident localization when a restream is degraded

What engineers need from that model:
- instantiated nodes
- explicit edges
- queues/rings as graph objects
- worker/state ownership
- per-node and per-edge live telemetry

## 8.1 The runtime graph should include

### Node types
- ingest source
- demux/router
- video preset transcode stage
- audio route stage
- codec edge stage
- package stage
- recorder
- HLS mux/publisher
- protocol sender
- queue
- ring reader
- worker

### Edge types
- packet edge
- frame edge
- audio edge
- queue edge
- shared-stage output edge
- control edge

## 8.2 Media-specific richness beyond BESS

The graph can exceed packet-processing graph systems by exposing media-native signals:
- A/V skew
- DTS/PTS anomalies
- GOP cadence
- keyframe recency
- segment publish latency
- playlist freshness
- output stale reason chain
- ring reader lag and overwrite behavior

## 8.3 Presentation rule

This richness should be **default-hidden from operators** and **default-visible to engineers**.

---
## 8.4 Engine-native first-class runtime model

To achieve BESS-grade graph richness, the platform must make runtime entities first-class **inside the engine**, not just in the UI.

### Hard requirements
- the runtime graph is owned by the engine as a canonical model
- every node has a stable typed identity (`NodeId`, `StageId`, `QueueId`, `RingId`, `WorkerId`)
- every edge has a stable typed identity and kind
- queues and rings are graph objects, not invisible implementation details
- worker placement and lifecycle ownership are explicit
- graph state is serialized from runtime-owned objects, not reconstructed ad hoc in API handlers
- telemetry is attached to nodes and edges as first-class data, not inferred from page-specific summaries

### First-class runtime object set
- engine
- pipeline
- stage node
- queue node
- ring node
- worker
- edge
- operation
- alert
- incident

### Implementation rule
All runtime creation flows should pass through graph-aware registries so that nothing substantial can exist without:
- an ID
- a lifecycle state
- an owner
- telemetry handles
- API-addressable identity

This is the media-platform equivalent of what made BESS graph introspection feel rich and trustworthy.

---

## 9. Agent-native control plane

The regular API surface is the foundation for dashboards and manual operations, but it is **not enough by itself** for a serious agent that can take user intent, plan safe changes, operate the platform, and debug issues.

The platform should therefore expose a dedicated **agent-native control plane** built on top of the engine runtime model and regular API.

## 9.1 Product goal

An agent should be able to:
- understand platform capabilities
- turn user intent into a typed plan
- validate the plan before acting
- execute safe changes with policy controls
- verify the result
- investigate degradations and incidents
- explain findings in operator language and engineer language

This should feel like operating a restreaming platform, not like stitching together dozens of raw endpoints.

## 9.2 Three-plane model

### Delivery plane
The media path itself:
- RTMP
- SRT
- HLS
- recording outputs

### Control plane
The human/operator/admin API and UI for the platform:
- pipelines
- outputs
- settings
- alerts
- diagnostics
- history

### Agent plane
A task-oriented machine surface that supports:
- capability discovery
- planning
- dry-run and diff
- validation
- execution
- verification
- investigation
- incident explanation
- remediation suggestions

## 9.3 Why raw REST is not enough

A typical CRUD-oriented API tells an agent **what objects exist**.
A serious operator/debugging agent also needs to know:
- what actions are supported in this environment
- what invariants must be preserved
- what the risk of a proposed change is
- how fresh each observation is
- what evidence supports a diagnosis
- which actions are safe vs disruptive vs destructive
- how to verify whether a change actually worked

Without this, the agent has to infer too much from raw JSON and will become brittle.

## 9.4 Agent capabilities API

Add an explicit capability surface:

- `GET /api/v1/agent/capabilities`

It should describe:
- supported ingest/output protocols
- supported transforms and codec edges
- allowed packaging modes
- available presets and route policies
- safe actions enabled in this deployment
- approval requirements by action class
- feature flags relevant to operations
- supported investigation and remediation workflows

## 9.5 Intent -> plan -> validate -> execute -> verify workflow

The control plane should support a canonical five-step loop:

1. **Plan**
2. **Validate**
3. **Execute**
4. **Verify**
5. **Explain**

### Plan
- `POST /api/v1/agent/plan`

Input:
- user/platform intent
- constraints
- desired outputs
- optimization preferences
- risk tolerance

Output:
- normalized intent
- proposed pipeline spec or diff
- predicted graph changes
- expected shared-stage reuse
- estimated CPU/memory impact
- missing inputs/secrets
- risk summary
- confidence notes

### Validate
- `POST /api/v1/agent/validate`
- `POST /api/v1/agent/explain-change`

Output:
- invariant check results
- graph diff preview
- policy violations
- blast-radius summary
- rollback notes where applicable

### Execute
- `POST /api/v1/agent/operations`
- `GET /api/v1/agent/operations/{operation_id}`
- `POST /api/v1/agent/operations/{operation_id}/approve`
- `POST /api/v1/agent/operations/{operation_id}/apply`

Output:
- operation ID
- idempotency handling
- affected objects
- state transitions
- warnings
- progress snapshots

### Verify
- `GET /api/v1/agent/operations/{operation_id}`
- `POST /api/v1/agent/operations/{operation_id}/verify`
- `POST /api/v1/agent/verify`

Output:
- post-change health result
- freshness recovery check
- graph convergence check
- incident/alert delta
- success/failure explanation

## 9.6 Investigation and debugging surface

This is the highest-value agent capability.

An agent should be able to ask:
- why is this output stale?
- which node or edge is first unhealthy?
- what changed recently?
- what is the likely fault domain?
- what evidence supports that conclusion?
- what is the safest likely remediation?

Suggested endpoints:
- `POST /api/v1/agent/investigate`
- `GET /api/v1/agent/incidents/{incident_id}`
- `GET /api/v1/agent/explanations/{explanation_id}`

Expected outputs:
- ranked suspected causes
- evidence references
- relevant graph slice
- affected pipelines/outputs
- recent related events
- recommended next checks
- recommended safe remediations
- confidence level

## 9.7 Action classes and policy gates

Agents should not have undifferentiated write access.

### Action classes
- **Read-only**
- **Safe operational**
- **Disruptive operational**
- **Destructive/admin**

### Example mapping
Read-only:
- inspect graph
- inspect telemetry
- trace output path
- generate incident summary

Safe operational:
- start a stopped pipeline
- attach a non-disruptive output
- refresh a probe
- run a verification pass

Disruptive operational:
- restart a stage
- reconfigure a live transcode stage
- drain or stop a pipeline
- rotate an active route

Destructive/admin:
- delete a pipeline
- purge recordings
- clear state
- alter global defaults

### Approval modes
- auto-allowed
- require operator approval
- require engineer/admin approval
- forbidden

This policy should be enforced by the platform, not left to the prompt layer.

## 9.8 Agent tool catalog

The best surface for LLM-style agents is a compact task-oriented tool catalog, not an explosion of raw endpoints.

### Observability tools
- `list_pipelines`
- `get_pipeline_status`
- `get_pipeline_graph`
- `get_stage_details`
- `get_alerts`
- `get_recent_events`
- `get_output_freshness`
- `get_resource_pressure`
- `get_incident_summary`

### Investigation tools
- `investigate_pipeline_issue`
- `trace_output_path`
- `find_first_unhealthy_node`
- `compare_expected_vs_actual_graph`
- `explain_degradation`
- `estimate_change_impact`

### Action tools
- `plan_pipeline_change`
- `apply_pipeline_change`
- `start_pipeline`
- `stop_pipeline`
- `drain_pipeline`
- `restart_stage`
- `reroute_output`
- `attach_output`
- `detach_output`
- `rotate_stream_key`

### Safety tools
- `validate_change`
- `preview_graph_diff`
- `check_policy_compliance`
- `verify_post_change_health`
- `rollback_operation`

## 9.9 Auditability and trust

Every agent operation should emit:
- actor identity
- agent identity/tool identity
- intent summary
- proposed plan hash
- approval path
- execution result
- verification result
- timestamps
- affected objects

This should feed both:
- audit log
- incident history
- postmortem reporting

## 9.10 Delivery order

### Phase A — read-only agent support
- capabilities
- graph and telemetry reads
- investigation workflows
- incident summaries

### Phase B — planning support
- plan
- validate
- graph diff preview
- change impact estimation

### Phase C — safe execution
- safe operational actions
- verification flows
- approval-gated disruptive actions

### Phase D — guided remediation
- policy-aware remediation suggestions
- selective automated remediation playbooks
- post-action explanation and verification summaries

---

## 10. API plan (clean slate, no backward compatibility)



Use a versioned Rust-native API under `/api/v1`.

## 10.1 System / engine
- `GET /api/v1/engine`
- `GET /api/v1/engine/health`
- `GET /api/v1/engine/capacity`
- `GET /api/v1/engine/features`
- `GET /api/v1/engine/build`
- `GET /api/v1/engine/telemetry`
- `GET /api/v1/engine/diagnostics`

## 10.2 Overview / operator plane
- `GET /api/v1/overview`
- `GET /api/v1/alerts`
- `GET /api/v1/events`

## 10.3 Pipelines
- `GET /api/v1/pipelines`
- `POST /api/v1/pipelines`
- `GET /api/v1/pipelines/{pipeline_id}`
- `PATCH /api/v1/pipelines/{pipeline_id}`
- `DELETE /api/v1/pipelines/{pipeline_id}`
- `POST /api/v1/pipelines/{pipeline_id}:start`
- `POST /api/v1/pipelines/{pipeline_id}:stop`
- `POST /api/v1/pipelines/{pipeline_id}:reload`
- `GET /api/v1/pipelines/{pipeline_id}/summary`
- `GET /api/v1/pipelines/{pipeline_id}/graph`
- `GET /api/v1/pipelines/{pipeline_id}/telemetry`
- `GET /api/v1/pipelines/{pipeline_id}/diagnostics`
- `GET /api/v1/pipelines/{pipeline_id}/history`

## 10.4 Stages / outputs / recordings
- `GET /api/v1/stages/{stage_id}`
- `GET /api/v1/stages/{stage_id}/telemetry`
- `GET /api/v1/stages/{stage_id}/diagnostics`
- `GET /api/v1/outputs`
- `GET /api/v1/recordings`
- `GET /api/v1/previews`

## 10.5 Admin / auth / settings
- `GET /api/v1/settings`
- `PATCH /api/v1/settings`
- `GET /api/v1/auth/session`
- `POST /api/v1/auth/login`
- `POST /api/v1/auth/logout`

## 10.6 Canonical response rules

Every measured or derived value should support:
- `value`
- `recorded_at`
- `freshness`
- `source` (`authoritative | sampled | estimated | derived`)
- `severity` where applicable

Do not create page-specific ad hoc status shapes.

---

## 11. Frontend/UI plan

## 11.1 Frontend direction

Keep the frontend lightweight and Rust-served.
Do **not** reintroduce a separate Node-based app runtime unless there is a compelling product reason.

### Keep
- static asset serving from Rust
- small dependency footprint
- HLS preview
- graph visualization
- diagnostics/event streaming

### Change
- remove legacy vocabulary and pages outright
- stop centering the UI on implementation subsystems
- stop using large page files and broad global mutable state as the default model
- move to a typed API client generated from Rust-owned schemas
- use page-level controllers and reusable rendering units

## 11.2 Product modes

### Operator mode
Purpose: run the platform

Screens:
- overview
- pipelines
- pipeline detail
- alerts/incidents
- capacity/storage

### Engineer mode
Purpose: debug and optimize the platform

Screens:
- graph explorer
- pipeline deep dive
- stage diagnostics
- timeline / event explorer
- telemetry explorer

### Admin mode
Purpose: govern the platform

Screens:
- auth/session
- settings
- retention/storage
- tokens/keys
- audit log

## 11.3 UI rewrite targets

Replace old surface assumptions entirely:
- `status.html` -> `engine.html` or `overview.html`
- mixed dashboard -> product-mode pages
- legacy subsystem labels -> engine-native and platform-native labels

---

## 12. Rust language and correctness plan

## 12.1 Main thesis

Use Rust’s type system to encode the runtime model so the platform becomes inherently harder to misconfigure and easier to reason about.

## 12.2 Concrete changes

### Newtypes and enums
Replace strings for:
- pipeline IDs
- output IDs
- stage IDs
- worker IDs
- protocol types
- lifecycle states
- severity bands
- codec families
- route kinds

### Typed state transitions
Model explicit states:
- `Planned`
- `Starting`
- `Running`
- `Degraded`
- `Draining`
- `Stopped`
- `Failed`

### Structured errors
Use explicit error enums for:
- planner
- runtime
- transport
- diagnostics
- telemetry snapshotting
- API model conversion

### Tighten unsafe boundaries
Isolate unsafe/FFI code, especially around:
- SRT bindings
- AVIO/FFmpeg interactions
- thread/panic containment

### Use zero-cost abstractions deliberately
Prefer:
- enums over dynamic trait objects on hot paths
- generic helpers where the type system buys correctness cheaply
- atomics/snapshots instead of locks in the packet path

Avoid over-abstracting hot code with broad trait hierarchies.

---

## 13. Efficiency and benchmarking plan

## 13.1 What to benchmark

Benchmark architecture choices and regressions around:
- external shared FFmpeg stages
- internal FFmpeg-library stages where justified
- current hybrid stage model
- package-stage sharing
- per-output sender overhead
- queue and ring pressure under fanout
- telemetry overhead

## 13.2 Decisions to validate with benchmarks

- where package-stage sharing pays off
- whether any preset class should move from external to internal FFmpeg
- where the graph can be fused without reducing fault isolation too far
- hot-path cost of queue/ring instrumentation

## 13.3 Benchmark outputs

Every benchmark should report:
- CPU
- RSS
- startup time
- steady-state throughput
- queue depth / lag behavior
- stage sharing ratio
- output freshness under load

---

## 14. Delivery roadmap

## Phase 0 — truth and cleanup
- reconcile stale docs with current code/test reality
- define canonical terminology
- freeze legacy vocabulary from new work

## Phase 1 — structural runtime cleanup
- introduce typed IDs and stage kinds
- extract `StageFeeder`
- centralize stage planning
- centralize graph serialization
- establish engine-native graph registries

Implementation note, 2026-06-25:
- typed stage identity, canonical stage-key building, and encoding-stage planning
  are now implemented in `src/domain/stage.rs`;
- shared packet feeder primitives are now implemented in `src/media/feeder.rs`
  and are used by recording, HLS, the in-process transcoder feeder path,
  external FFmpeg subprocess stdin, and the H.265→H.264 bridge feeder;
- backend-selection policy is now centralized in
  `src/planner/backend_policy.rs`;
- graph serialization now consumes typed `StageKind` parsing/rendering helpers,
  but full engine-native graph registries remain pending.
- `src/media/feeder.rs` has an equivalence test proving `TsPacketFeeder`
  matches the previous manual codec conversion + DTS enforcement + TS mux path;
  `benches/stage_feeder.rs` measures the shared feeder hot path.

Implementation note, 2026-06-25 (continued):
- stage registries now use typed `StageKey` values end-to-end. The intermediate
  legacy string-key helpers were removed during Phase 3; extracting finer-grained
  registry structs remains a future cleanup.

Phase 1 structural items (typed IDs and stage kinds, StageFeeder coverage for
recording, HLS, internal/external transcoder feeders, H.265→H.264 bridge feeder,
stage planning, graph serialization) are complete as of 2026-06-25.

Phase 3 foundation update, 2026-06-25:
- `MediaEngine` now stores transcoder buffers, stage input queues, and
  subprocess pipe metrics in typed `StageKey` maps internally.

Phase 3 typed-key completion, 2026-06-26:
- The legacy string layer has been removed entirely. All engine method
  signatures accept typed `StageKind`/`StageKey` parameters. The reconciler
  and stage tasks pass typed keys end-to-end.

## Phase 2 — telemetry substrate
- queue/ring/stage/edge telemetry
- lifecycle and event model
- alert derivation model
- freshness-aware snapshots

Implementation note, 2026-06-25:
- `MemoryQueue::stats()` now exposes queue depth, capacity, high-water mark,
  blocked write count, blocked write time, and closed state. API/graph surfacing
  remains pending.
- typed alert model implemented in `src/alerts.rs`: `Alert`, `Severity`, `Scope`
  structs and a pure `derive_alerts(&snapshot)` function covering publisher-absent,
  reader-lag, ring-overflow, output-down, and SRT-drop conditions. Served at
  `GET /api/v1/alerts` and `GET /pipelines/:id/alerts`. First-seen/last-seen
  tracking deferred to Phase 3 (requires persistent state).
- stage metrics wired into `external_transcoder` (record_in/record_out per packet)
  and `h264_transcoder` (record_in on muxer side); metrics are removed on stage
  exit. `GET /pipelines/:id/graph` now returns live throughput for transcoder nodes.
- `MemoryQueue` stats surfaced in `processing_graph()` via a new `input_queues`
  registry in `MediaEngine`; `h264_transcoder` and internal transcoder register
  their queues. Each transcoder graph node includes a `queueMetrics` field.
- operator overview (`GET /api/v1/overview`) and pipeline summary
  (`GET /api/v1/pipelines/:id/summary`) endpoints added; both derive data from
  a single `health_snapshot` + `derive_alerts` pass.
- lifecycle event log implemented in `src/events.rs`: bounded 1000-event FIFO
  ring covering IngestConnected/Disconnected, StageStarted/Stopped,
  EgressStarted/Stopped; events carry camelCase fields including `pipelineId`,
  `seq`, and `timestamp`. `GET /api/v1/events` exposes the log.
- `generatedAt` added to `processing_graph()` response; alert endpoints now
  return `{generatedAt, alerts:[...]}` envelope consistent with other v1 responses.
- `PipeMetrics` (`src/media/pipe_metrics.rs`): back-pressure counters for the
  external-subprocess pipe — stdin stalls (pipe buffer full) and stdout idles
  (pipe empty). Separate from `StageMetrics` because only external-subprocess
  stages have a kernel pipe to observe. Surfaced in the graph as `pipeMetrics`.
- `src/media/timing.rs`: portable elapsed-time module used by pipe metrics
  instrumentation. Uses `rdtsc` (≈22 ns) on x86_64 with invariant TSC; falls
  back to `Instant` (≈36 ns) otherwise. Three validation gates (invariant TSC
  CPUID, calibrated cps bounds, minimum window). `calibrate() → bool` allows
  stages to log which backend is active.
- Code organisation: `StageMetrics`, `PipeMetrics`, and the timing module
  extracted from `engine.rs`/`external_transcoder.rs` into dedicated files;
  `engine.rs` re-exports both via `pub use`.
- `benches/stage_metrics.rs`: hot-path cost measurements for record_in/out,
  snapshot, rdtsc vs Instant comparison, and full stdin-instrumentation overhead
  per packet. Verified on 2026-06-25 with `bench-dev`: record_in/out ≈17 ns,
  stage snapshot ≈0.9 µs, pipe metric updates ≈16 ns, and full stdin
  instrumentation ≈60 ns per packet.

Phase 2 is complete as of 2026-06-25 for the telemetry substrate scope:
queue/ring/stage/edge telemetry, lifecycle events, alert derivation, and
freshness-aware snapshots all have live implementation and tests. Recording
StageMetrics gap (graph telemetry node populated with null metrics) was identified
in review and has been fixed: recording.rs now creates StageMetrics, calls
record_in per packet, and removes metrics on exit. StageStarted/StageStopped event
coverage was also extended to h264_transcoder, internal transcoder, and HLS
segmenter to close the lifecycle event gap.

## Phase 3 — API reset
- `/api/v1` clean-slate surface
- canonical response models
- operator and engineer endpoints
- engine-native graph resources

Implementation note, 2026-06-25:
- Phase 3 operator endpoints are partially landed alongside Phase 2 work:
  `GET /api/v1/overview`, `GET /api/v1/alerts`, `GET /api/v1/events`,
  `GET /api/v1/pipelines/:id/summary` are all live and tested.
- `generatedAt` envelope is consistent across all v1 snapshot endpoints.

Phase 3 completion, 2026-06-26:
- Engineer telemetry endpoints landed:
  `GET /api/v1/engine/telemetry` (all ingests, stages, egresses, buffer count),
  `GET /api/v1/pipelines/:id/telemetry` (pipeline-scoped ingest, ring, stages,
  egresses), `GET /api/v1/stages/:key/telemetry` (single-stage throughput and
  pipe metrics, 404 for unknown stages).
- `AlertTracker` with `firstSeen`/`lastSeen` timestamps: stamps alerts on
  first observation, updates `lastSeen` on each subsequent observation, prunes
  resolved conditions. Wired into `/api/v1/alerts`,
  `/pipelines/:id/alerts`, and `/api/v1/pipelines/:id/summary`.
- Legacy string layer removed from `StageKey`/`StageKind`: `parse_legacy_key`,
  `storage_key`, `legacy_key`, `legacy_ref`, `legacy_stage_key`,
  `terminal_stage_ref`, and `Unknown` variant all deleted. All engine method
  signatures accept typed `StageKind`/`StageKey` parameters. `fmt::Display`
  on `StageKind` produces canonical strings for logging and events.
- API tests for all three engineer telemetry endpoints plus auth enforcement.
- AlertTracker unit tests: stamps first/last, preserves first_seen across
  updates, prunes resolved alerts.
- Docs updated: `docs/api-reference.md` and `docs/observability.md` document
  all v1 operator and engineer endpoints.
- Remaining structural follow-up: splitting `MediaEngine` into finer-grained
  registry structs (`StageRegistry`, `IngestRegistry`) is a future cleanup,
  not gating Phase 3 completion.

## Phase 4 — agent read and planning plane
- capability discovery
- read-only investigation workflows
- intent -> plan -> validate
- graph diff and impact previews

Phase 4 implementation checkpoint, 2026-06-26:
- Added an optional `agent-plane` Cargo feature. Default core builds compile
  the agent module out and return an authenticated `404` envelope from
  `/api/v1/agent/*` routes with `compiledIn: false`.
- Added a modular `src/agent_plane.rs` read/planning module with no media-engine
  ownership: capability discovery, investigation evidence envelopes, draft plan
  generation, validation, static graph preview, and impact estimation.
- Wired authenticated v1 routes:
  `GET /api/v1/agent/capabilities`,
  `GET /api/v1/agent/context`,
  `POST /api/v1/agent/investigations`,
  `POST /api/v1/agent/plans`,
  `POST /api/v1/agent/plans/validate`,
  `POST /api/v1/agent/graph-diff-preview`.
- Added the canonical redacted agent context bundle: build/runtime status,
  feature flags, redacted persisted config, health, telemetry, graphs, alerts,
  lifecycle events, media inventory, and diagnostics metadata. Stream keys and
  output URLs are replaced with stable fingerprints and URL scheme/host
  summaries.
- Extended the context bundle into an agent state contract: route/schema
  capability metadata, desired-vs-actual input/output/recording/HLS summaries,
  storage/media capacity, recent job/output history, passive diagnostics
  findings, and HLS/recording/file-ingest/ingest-security dependency summaries.
- Execution is intentionally unavailable (`executionEnabled: false`) so phase 6
  can remain separately gated and removable.

## Phase 5 — UI reset
- Added a mode-based dashboard shell with Overview, Pipeline, Engineer, and
  Admin workspaces. The existing pipeline workflow remains available as the
  canonical manual operations surface.
- Added an operator-first overview that summarizes live inputs, running outputs,
  throughput, recording state, and pipeline health without exposing graph
  internals by default.
- Added an engineer workspace with pipeline selection, redacted output
  summaries, inline processing-graph rendering, graph modal access, and
  diagnostics readiness/deep-dive controls.
- Added an admin workspace for configuration/runtime navigation and compact
  configuration state.
- Updated the frontend build path so `npm run build:frontend` compiles both
  TypeScript modules and Tailwind/DaisyUI CSS before Rust embeds static assets.

## Phase 6 — safe agent execution and verification
- Added a separately removable `agent-execution` Cargo feature that depends on
  `agent-plane`; core builds and read/planning-only builds keep execution
  compiled out.
- Added a modular `src/agent_execution.rs` operation store with idempotency,
  approval state, status transitions, progress snapshots, redacted public
  records, and audit events.
- Wired authenticated operation routes for create/get/approve/apply/verify plus
  `POST /api/v1/agent/verify`.
- Execution is approval-gated: create only records an operation, apply rejects
  until explicit approval, and apply uses the same DB/runtime primitives as the
  core output APIs for add/update/remove/start/stop output changes.
- Verification records post-change health, graph convergence, freshness checks,
  alert delta, per-change success/failure explanations, and stores the result
  back on the operation record.

## Phase 7 — test and benchmark hardening
- Rust live integration harness becomes primary
- property and concurrency testing
- failure injection
- performance regression gates
- agent workflow contract tests
- composable verification stages so large benchmark/integration suites are
  decomposed by behavior, protocol, codec, topology, load shape, and evidence
  type instead of becoming single all-or-nothing blockers

Implementation note, 2026-06-25:
- the Rust unit and integration suite currently has 471 passing non-doctest
  tests;
- HLS PUT upload, FFmpeg-backed remap/downmix, shared-stage cleanup, runtime
  tuning, secured HLS pull routes, and built-in internal video preset coverage
  are now regression-tested;
- the remaining release blocker is a reproducible protocol matrix run covering
  H.265, B-frame timestamps, cross-protocol packaging, and destination restart.

Phase 7 acceleration note, 2026-06-26:
- `test/run-integration.sh` now enforces a disk-safety preflight for live
  artifact runs: `RESTREAM_ARTIFACT_MIN_FREE_MB` fails early when the artifact
  filesystem is low on space, and old ignored `test/artifacts/` runs are
  pruned so only the latest three remain while protecting the active run
  directory.
- The disk guard is exposed as an `artifact-disk` JSON preflight record, so
  protocol-matrix runs can fail before expensive live services start instead
  of filling the filesystem mid-suite.
- Host-network preflight is mode-aware: legacy live modes check the configured
  Restream/MediaMTX service ports, and Rust-only harness modes check the
  concrete harness loopback ports they bind.
- `test/run-protocol-matrix.sh` now handles `--help`, `--list-modes`, and
  invalid `--only-modes` selections without compiling, while real runs use
  `scripts/resource-limit` for the Rust orchestrator build/run step. The
  orchestrator build uses `RESTREAM_PROTOCOL_MATRIX_ONLY=1`, avoiding the
  media-native link prerequisite until a selected live mode actually needs it.
- Static setup/build now completes with the embedded FFmpeg AC-3/x264/x265
  capability surface, and the nine-mode protocol-matrix preflight passes when
  pointed at `.build/static/cargo-target/release/restream`.
- H.265 passthrough, H.265-to-H.264 RTMP edge conversion, and SRT-to-RTMP
  packetization are exposed as first-class `test/run-integration.sh` modes and
  included in the default protocol matrix.

## Phase 8 — platform optimization
- package-stage sharing
- selective backend fusion where benchmarked and justified
- capacity and autoscaling guidance
- guided remediation playbooks where justified

---

## 15. Success criteria

### Operator success
- can identify unhealthy pipelines in seconds
- can understand impact without opening deep diagnostics
- gets actionable incident hints
- trusts freshness and health status

### Engineer success
- can localize faults to a stage, edge, queue, or worker quickly
- can inspect the live graph of instantiated modules and shared stages
- can see evidence, not just symptoms
- can compare runtime behavior over time

### Agent/control-plane success
- can turn user intent into safe, typed plans
- can explain incidents using platform-native evidence
- can operate safely under policy and approval gates
- can verify whether a remediation actually worked

### Platform success
- reduced duplicate work across outputs
- lower CPU/memory at equivalent fanout
- fewer hidden runtime states
- faster recovery from faults
- clearer ownership boundaries in code
- cleaner API/UI product identity

---

## 16. Final recommendation

Build the product as a **Rust-native restreaming platform** with:
- a typed runtime graph
- shared expensive processing stages
- selective FFmpeg usage for codec-heavy work
- Rust-first integration testing
- one telemetry substrate with operator and engineer views
- a clean-slate `/api/v1` control plane
- a UI designed around platform operation, not engine admiration

The operator should experience a reliable restreaming service.
The engineer should have a BESS-grade live graph and deeper media diagnostics when needed.
The codebase should express those two truths cleanly, without forcing either persona to live in the other’s world.


---

## 17. Assumptions and planning basis

This plan is written against the following assumptions.

### Product and platform assumptions
- The product is a **restreaming platform**, not a general-purpose media graph workbench.
- The platform is now a **single Rust backend** and should be presented as such.
- Backward compatibility is **not** a constraint for API, UI, terminology, or internal naming.
- Operators care first about **source health, output health, freshness, incident impact, and recovery actions**.
- Engineers need deep runtime graph and media diagnostics, but that depth should be secondary in the default product surface.
- Agent operation is in scope, but only through a **policy-gated, typed control plane**.

### Runtime and architecture assumptions
- CPU and memory efficiency matter enough that **duplicate work in the processing path should be treated as a defect unless justified**.
- Rust should remain the owner of orchestration, planning, sharing, control plane, telemetry, and diagnostics.
- FFmpeg should remain the owner of codec-heavy transforms unless a benchmarked, lower-risk Rust/libav integration clearly wins.
- Some runtime richness comparable to BESS is desirable, but **adapted to media** and in service of operability rather than engine exhibition.
- Hot-path telemetry overhead must remain small enough that it does not materially distort throughput, latency, or memory shape.

### Planning and evidence assumptions
- This document is based on a static codebase audit plus the supporting artifacts created in this conversation.
- Benchmarks referenced here are **directional planning estimates**, not final validated performance claims.
- A few supporting details remain best preserved in their specialized companion documents rather than duplicated line-for-line here.

---

## 18. Explicit non-goals and exclusions

The following are intentionally out of scope unless later justified by benchmarks or product needs.

### Architecture non-goals
- Rewriting all media processing into pure Rust for ideological reasons.
- Treating external-process elimination as a goal in itself.
- Preserving legacy Node.js / MediaMTX / hybrid-stack mental models.
- Maintaining compatibility aliases, legacy field names, or legacy page models.

### Product non-goals
- Exposing engineer-level graph complexity to operators by default.
- Turning the product into a generic media lab or graph sandbox.
- Prioritizing graph visual sophistication over operational clarity.
- Building a second parallel observability model just for agents.

### Agent non-goals
- Giving agents unrestricted write access from day one.
- Letting agents infer safety or policy from raw metrics alone.
- Requiring agents to scrape dashboards or reverse-engineer intended workflows from human-facing pages.

### Process non-goals
- Treating this master plan as a substitute for milestone-level acceptance criteria, benchmark execution, or implementation tickets.
- Treating rough estimated savings as committed delivery outcomes.

---

## 19. Open questions and decision log

These are the main questions that should remain explicit as work proceeds.

### 19.1 Processing-path questions
- Which preset classes, if any, should move from external FFmpeg stages to internal/libav-backed fused stages after the first benchmark pass?
- How far should package-stage sharing go before fault-isolation concerns outweigh the gains?
- Which audio-route variants deserve dedicated shared stages versus per-output handling?
- Where is the right boundary between reusable upstream graph nodes and per-destination sender state?

### 19.2 Runtime-model questions
- Should queues and rings always appear as graph nodes, or should they sometimes be represented as edge-attached runtime objects?
- What should be the canonical worker model: one worker per stage, pooled workers, or a mixed policy depending on stage kind?
- How much graph history should be retained in memory for live diagnostics before archival or summarization is needed?

### 19.3 Telemetry and diagnostics questions
- Which metrics must be continuously exported, and which can be derived lazily from snapshots?
- What retention window is needed for recent events, state transitions, and graph diffs to support incident review?
- What is the right threshold model for degraded versus failed states in media-specific conditions such as skew, lag, or GOP irregularity?

### 19.4 Agent-control-plane questions
- Should the agent facade live entirely in-process, or should some policy and approval workflows live in a separate companion service?
- How should approvals be represented: synchronous user confirmation, signed operation tokens, role-based quorum, or environment-specific policy bundles?
- Which remediations are safe enough for autonomous execution, and which should remain recommendation-only?

### 19.5 Frontend/API questions
- How lightweight should the frontend remain: server-rendered/static-with-TS versus a more componentized client architecture?
- What is the right API schema generation and versioning approach to keep the UI, agents, and external automation aligned?
- Should the engineer view remain embedded in the main product or be a separate advanced mode with stronger permissions?

### 19.6 Delivery questions
- Which implementation order best reduces risk: runtime-model first, telemetry first, or API first?
- Which success gates must be met before the platform is allowed to retire any remaining legacy operational habits or wrappers?

---

## 20. Estimated savings summary

These are directional estimates based on the earlier code audit and the codebase’s own checked-in documentation, not final benchmark-certified claims.

### 20.1 Directional architecture comparison

| Direction | Estimated CPU impact | Estimated memory impact | Maintainability | Recommendation |
|---|---:|---:|---|---|
| 1. External shared FFmpeg process | Parent CPU may look lower; total system CPU likely roughly flat in many cases | Likely worse in HEVC/RTMP edge cases; an additional shared external stage can cost materially more RSS than the current lighter internal edge stage | Simpler runtime boundary, but less efficient | Do not make this the global direction |
| 2. Internal fused FFmpeg-library stages | Potential 5–15% CPU improvement on some heavy shared preset paths if implemented well | Potential 100–250 MB RSS reduction per shared preset stage versus a heavy external child in favorable cases | Highest implementation and stability risk | Treat as a selective future optimization path |
| 3. Rust platform runtime + selective FFmpeg | Avoids likely regressions; modest near-term CPU wins come from sharing and cleanup rather than backend ideology | Best practical near-term memory shape | Best balance of cost, clarity, and risk | **Recommended** |

### 20.2 Specific directional observations from the earlier audit
- Replacing the current narrower internal HEVC→H.264 edge with a full additional external shared FFmpeg child is likely a **memory regression**, not a win.
- The biggest immediate gains are more likely to come from:
  - removing duplicated feeder logic
  - making package-stage sharing real
  - improving graph/planner clarity
  - reducing invisible queue/ring bottlenecks
- The most valuable “savings” are not only CPU/RSS:
  - fewer branch combinations
  - fewer hidden runtime states
  - faster root-cause localization
  - fewer duplicated transforms across outputs

### 20.3 Confidence level
Confidence is highest for:
- the recommendation to keep the Rust-led hybrid model
- the need for typed stage identity and explicit graph/runtime objects
- the value of queue/ring/worker visibility
- the need for agent-native plan/validate/execute/verify surfaces

Confidence is lower for:
- exact CPU savings from internal fused FFmpeg arrangements
- exact per-stage RSS savings until the benchmark harness is run against representative scenarios

---

## 21. Benchmark plan and directional checks

This section consolidates the earlier benchmark thinking into a program-level plan.

### 21.1 Benchmark purpose
The benchmark program should answer five questions:
1. Where is duplicate work still occurring in the processing path?
2. What is the real cost of external shared stages versus internal fused stages for the scenarios that matter?
3. How much does package-stage sharing save in CPU and RSS?
4. What is the hot-path cost of the richer telemetry substrate?
5. Which architectural choices improve operator outcomes, not just synthetic throughput?

### 21.2 Benchmark scenarios
Run benchmark sets for at least the following classes:

#### Baseline processing scenarios
- one ingest -> one output passthrough
- one ingest -> multiple outputs with identical preset
- one ingest -> mixed presets
- one ingest -> HEVC ingest with RTMP H.264 edge conversion
- one ingest -> multiple outputs sharing audio routing

#### Packaging and delivery scenarios
- one shared transformed stream -> many senders
- package-stage sharing on versus off
- HLS enabled versus disabled
- recording enabled versus disabled

#### Stress and degradation scenarios
- downstream sender slowdown
- queue pressure buildup
- source jitter/discontinuity
- one shared stage feeding multiple outputs under load
- stage restart / failover / worker loss

#### Telemetry-cost scenarios
- minimal telemetry
- operator-grade telemetry
- engineer-grade telemetry
- full graph + queue/ring instrumentation enabled

### 21.2.1 Composable benchmark stages
Benchmark and integration coverage should be assembled from independently
runnable stages, not from one ever-growing command. Each stage should have a
stable name, a focused scenario matrix, and its own machine-readable result
artifact so a failure localizes to a behavior slice.

Recommended stage breakdown:
- **Micro cost:** isolated Criterion groups for touched hot paths.
- **Component contract:** unit/integration tests for stage identity, graph
  invariants, API shape, lifecycle, and alert rules.
- **Protocol smoke:** one ingest/output pair with ffprobe or readback evidence.
- **Topology slice:** shared-stage, package-stage, multi-output, or mixed-preset
  graph shape with expected stage counts.
- **Codec/media slice:** H.264, H.265, B-frames, multi-audio, audio remap, or
  timestamp-specific coverage.
- **Load slice:** small fanout, ramp, queue pressure, downstream restart, or
  soak, each bounded enough to run independently.
- **Release bundle:** a manifest that composes the required stages for a
  milestone while preserving each stage's separate pass/fail result.

Adding a new concern should usually add a selector or stage manifest entry
before it adds a mandatory full-suite step. Full benchmark runs remain useful as
release confidence checks, but they should be the composition of named stages
whose individual signal is visible.

### 21.3 Metrics to capture
Every run should capture:
- total process CPU
- per-stage CPU where possible
- RSS and relevant memory subcomponents
- startup time
- steady-state throughput
- output freshness
- queue depth and high-water
- ring lag / overwrite indicators
- stage sharing ratio
- number of spawned heavy processing stages
- incident recovery time where a failure is induced

### 21.4 Evaluation method
For every benchmarked scenario:
- compare expected graph to actual graph
- compare operator-visible outcome to engineer-visible evidence
- record not only performance but also whether the runtime state was explainable
- prefer architectural choices that preserve diagnosability, not just raw speed

### 21.5 Regression gates
Before accepting major runtime changes, require:
- no unexplained increase in duplicate expensive stages
- no material operator-health regressions
- no unacceptable telemetry overhead on hot paths
- no drop in ability to localize faults via graph/telemetry evidence
- stable or improved memory shape under representative fanout loads

### 21.6 Benchmark deliverables
The benchmark program should produce:
- machine-readable result artifacts
- a human summary comparing architecture directions
- recommended defaults for production profiles
- a change log noting which assumptions were confirmed, weakened, or overturned

Supporting artifacts from this conversation:
- `directional_architecture_bench.rs`
- `directional_architecture_bench_plan.md`

---

## 22. Traceability and supporting artifacts

This master plan is the primary steering document. The following companion artifacts remain useful for detailed support and traceability.

### 22.1 Primary program document
- **`restream_platform_master_plan_v2.md`**  
  Earlier consolidated plan before the appendix-style additions in this v3. Useful as the prior checkpoint.

### 22.2 Architecture and runtime audit artifacts
- **`restream_audit_plan.md`**  
  Detailed codebase audit covering stage-feeder refactor, testing, telemetry, Rust language usage, modularity, and operator/engineer exposure considerations.
- **`directional_architecture_bench.rs`**  
  Benchmark scaffold for directional architecture checks.
- **`directional_architecture_bench_plan.md`**  
  Focused benchmark plan for comparing the three architecture directions.

### 22.3 Frontend and API artifacts
- **`restream_frontend_api_audit_and_mockups.md`**  
  Detailed frontend/API audit and redesign direction aligned to the single Rust backend reality.
- **`restream_frontend_api_clean_slate_plan.md`**  
  Clean-slate UI/API reset framing without backward-compatibility constraints.
- **`restream_engine_ui_mockups.html`**  
  Interactive mockups illustrating operator and engineer views.

### 22.4 How to use the artifact set
Use the documents in this order:
1. **This v3 master plan** for program direction and scope.
2. **Frontend/API audit and mockups** for product-surface design and implementation detail.
3. **Audit plan** for code-structure and runtime refactor detail.
4. **Benchmark artifacts** for architecture validation and optimization work.

### 22.5 What this v3 adds over v2
This v3 explicitly adds:
- planning assumptions
- explicit exclusions and non-goals
- open questions and decision log
- estimated savings summary
- benchmark program summary
- artifact traceability index

---

## 23. Final framing

This program is not about making the engine look sophisticated.
It is about making a **restreaming platform** behave predictably, operate clearly, diagnose quickly, and scale efficiently.

That leads to a simple hierarchy of truth:

1. **Operators need outcomes.**
2. **Engineers need evidence.**
3. **Agents need typed, safe control surfaces.**
4. **The runtime must make the real graph first-class so the evidence is trustworthy.**

If the implementation follows that hierarchy, the result can be:
- operationally simple on the surface
- deeply diagnosable underneath
- efficient in CPU and memory
- safe for future agent operation
- cleanly aligned with a single Rust-owned platform identity
