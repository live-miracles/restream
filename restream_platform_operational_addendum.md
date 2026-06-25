# Addendum to `restream_platform_master_plan_v3.md`

## Purpose

This addendum captures the operational gaps surfaced by the currently open GitHub issues that are **not yet explicit enough** in the v3 master plan. It is intentionally narrow: it does not restate the main architecture, telemetry, API, frontend, or agent-control-plane strategy. It adds the missing operational workstreams needed so the master plan addresses the issue tracker more completely.

This addendum should be read together with:
- `restream_platform_master_plan_v3.md`
- `restream_frontend_api_audit_and_mockups.md`
- `restream_audit_plan.md`

---

## 1. Security and control-plane hardening addendum

### Why this needs to be explicit
The open issues show that security and abuse-resistance are not just generic platform concerns; they are concrete operational asks:
- built-in API authentication
- API rate limiting
- SRT passphrase support and safe handling
- safer mutation and write controls

The v3 plan mentions policy, approvals, and agent safety, but it does not yet define a first-class **security hardening workstream** for the platform itself.

### Goals
- Protect the platform API and UI with built-in auth from day one.
- Protect write and high-cost endpoints against abuse and accidental hammering.
- Make protocol secrets and passphrases first-class configuration objects with safe display, storage, rotation, and audit.
- Ensure all agent and human write actions pass through the same authorization and audit policy model.

### Plan

#### 1.1 Built-in API authentication
Implement a simple built-in control-plane auth layer as the default deployment posture:
- shared secret header and/or HTTP basic auth as the initial implementation
- optional expansion path to token/session auth later
- explicit separation between:
  - read-only operator access
  - engineer diagnostics access
  - admin/mutation access
  - agent execution access

Requirements:
- all write endpoints protected
- all sensitive read endpoints protected
- local/dev mode documented explicitly
- no implicit trust based on deployment topology alone

#### 1.2 API rate limiting and abuse control
Add per-class rate limiting:
- mutation endpoints: strict
- expensive diagnostics / graph queries: moderate
- status polling / SSE: tuned for expected dashboard behavior
- auth failures: aggressively limited

Requirements:
- limits visible in config
- defaults safe for a single-node restream platform
- operator dashboard polling should not trip protection
- audit events emitted when limiting occurs

#### 1.3 Secret and passphrase management
Treat protocol credentials as first-class secure config:
- SRT publish/read passphrases
- stream keys
- output credentials
- future destination tokens

Requirements:
- encrypted at rest where practical
- masked in operator UI by default
- explicit reveal action with authorization
- rotation support
- write-only handling where possible
- audit log on create/update/reveal/rotate/delete

#### 1.4 Transport security policy
Add protocol-specific safety validation:
- warn or block insecure SRT configuration where policy requires encryption
- validate passphrase strength and compatibility constraints
- expose transport security posture in diagnostics and agent capability surfaces

#### 1.5 Unified authorization and audit
Human UI, API clients, and future agents must all go through the same policy gate:
- action class
- actor identity
- approval requirement
- reason/context
- resulting operation record

Deliverables:
- authn/authz design
- rate limiting policy
- secrets/passphrase data model
- audit event schema
- endpoint coverage matrix

---

## 2. Media-quality policy and regression program addendum

### Why this needs to be explicit
The open issues surface media-quality concerns directly, not just platform architecture concerns:
- audio crackling in auto audio encoding mode
- request to make passthrough the default for audio encoding
- operational expectation that the product prefers “do no harm” defaults

The v3 plan covers testing and diagnostics broadly, but it does not yet define a **media-quality policy** with default behaviors and regression gates.

### Goals
- Make media-quality policy explicit and product-level.
- Prefer safe defaults that minimize unnecessary transforms.
- Turn quality regressions into first-class release blockers, not just ad hoc bug tickets.

### Policy direction

#### 2.1 Default transform policy
Adopt a conservative default:
- **audio passthrough by default** unless a specific user requirement or compatibility constraint requires re-encode
- video passthrough where possible and where destination constraints permit
- automatic transcode only when the platform can explain why it is necessary

#### 2.2 Transform justification
For every encode/remap/downmix/scale decision, the runtime should be able to answer:
- why this transform exists
- whether it is required
- whether it is shared
- which outputs depend on it

This should appear in engineer diagnostics and agent plan/explain responses.

#### 2.3 Media-quality regression matrix
Create explicit regression coverage for:
- audio passthrough vs auto encode
- stereo/mono/downmix/remap cases
- codec edge cases likely to crackle or drift
- ingest profiles from real sources such as vMix / ABL / similar environments
- mixed-output fanout with shared stages

#### 2.4 Quality acceptance criteria
Define release gates for:
- no audible crackle introduced by default paths
- acceptable A/V skew bounds
- no unexplained timestamp discontinuity regressions
- no increased artifacting due to unnecessary transform insertion
- stable long-run quality under shared-stage fanout

#### 2.5 Quality telemetry
Expose media-quality evidence at the engine layer:
- skew bands
- discontinuity counters
- detected restarts / underruns
- audio-route and encode-mode selection
- recent warnings tied to specific stages and outputs

Deliverables:
- media-quality policy document
- transform-defaults table
- regression matrix
- release-blocking acceptance criteria
- quality telemetry additions

---

## 3. Configuration model, validation, and lifecycle addendum

### Why this needs to be explicit
The issue tracker shows concrete operational asks around:
- duplicate destination warning
- import/export of pipelines and outputs
- safe configuration ergonomics

The v3 plan includes API/UI redesign, but it does not yet fully define a **configuration lifecycle model**.

### Goals
- Make configuration declarative, validated, explainable, and portable.
- Prevent common operator mistakes before they become runtime incidents.
- Support bulk movement of configuration into and out of the platform.

### Plan

#### 3.1 Canonical declarative config model
Define a canonical portable schema for:
- sources / ingest definitions
- pipelines
- outputs
- credentials references
- presets / transport overrides
- failover and recovery policy
- telemetry/retention overrides where allowed

#### 3.2 Validation and linting
Add pre-apply config validation with warnings and errors.

Examples:
- duplicate RTMP destination URL/key combinations
- incompatible codec/output combinations
- unsafe or contradictory transport settings
- missing or malformed credential references
- redundant transforms
- likely duplicate outputs differing only in non-functional fields

Severity model:
- error: cannot apply
- warning: apply allowed but operator must understand the risk
- info: optimization or cleanup suggestion

#### 3.3 Import/export
Support:
- export selected pipelines/outputs
- import bundles with validation
- dry-run import preview
- duplicate/conflict handling
- secret redaction or reference-only export modes

#### 3.4 Config diff and history
Every applied change should produce:
- normalized diff
- validation results
- graph impact prediction
- operation record
- rollback or reverse patch where feasible

#### 3.5 Agent compatibility
The configuration model should be usable by:
- human UI
- direct API automation
- agent plan/apply workflows

Deliverables:
- canonical config schema
- validation/lint engine
- import/export format
- diff/history model
- UX/API preview flows

---

## 4. Operational distribution, maintenance, and supply-chain addendum

### Why this needs to be explicit
Some open issues are not about runtime behavior, but they still matter operationally:
- Prometheus/Grafana installation and packaging expectations
- Dependabot / code quality scan setup
- maintainable deployment/update paths

The v3 plan does not yet treat these as a formal workstream.

### Goals
- Make the platform operationally maintainable on real servers.
- Reduce friction in deploying, upgrading, monitoring, and auditing the platform.
- Bring supply-chain hygiene into the operating model.

### Plan

#### 4.1 Monitoring stack distribution
Define the supported monitoring posture:
- what is built into the platform
- what is optional external observability
- how Prometheus/Grafana are installed in supported environments
- whether package-manager installation is preferred over Docker Compose for Debian-class deployments

#### 4.2 Install/update pathways
Document and support:
- fresh install
- in-place upgrade
- config migration where needed
- backup/restore of critical state
- post-upgrade verification checklist

#### 4.3 Bounded retention for operational artifacts
For logs, FFmpeg/diagnostic artifacts, reports, and snapshots:
- bounded directories
- rotation / retention policy
- cleanup jobs
- visibility in diagnostics

#### 4.4 Supply-chain and maintenance automation
Add explicit maintenance controls:
- dependency update scanning
- code quality / security scan integration
- CI checks for operational scripts and packaging docs
- release checklist including observability and rollback verification

#### 4.5 Documentation as product surface
Treat operator documentation as part of the platform:
- install guides
- monitoring guides
- recovery guides
- security setup guides
- transport tuning guides
- soak-test / long-run runbook

Deliverables:
- distribution/support matrix
- monitoring deployment guide
- release/upgrade checklist
- retention policy
- supply-chain automation checklist

---

## 5. Transport tuning and failover clarification addendum

### Why this needs to be explicit
The open issues also point to concrete transport/runtime knobs:
- UDP socket buffer sizing
- SRT latency configuration
- packet loss investigation
- RTMP failover expectations

These are not fully missing from v3, but they are too implicit.

### Plan

#### 5.1 Transport tuning surface
Expose transport controls as validated platform settings, not hidden implementation details:
- SRT latency
- UDP buffer sizing hints
- retry/backoff policy
- input flap thresholds
- destination reconnect policy

These must be:
- bounded
- documented
- diagnosable
- visible in the plan/explain and diagnostics surfaces

#### 5.2 Transport diagnostics
For transport-heavy incidents, the platform should expose:
- current effective transport settings
- freshness and recovery timing
- packet-loss / retry / reconnect evidence where available
- recommended next checks
- whether current behavior matches configured policy

#### 5.3 Failover policy
Treat failover as an explicit platform feature:
- source loss handling policy
- fallback media policy
- recovery criteria
- operator-visible state and countdowns
- post-recovery verification

Deliverables:
- transport tuning schema
- transport diagnostics extensions
- failover policy model
- regression scenarios for jitter / burst / loss / recovery

---

## 6. What this addendum changes in practice

With this addendum, the combined planning set should now be read as covering these operational issue classes:

### Explicitly covered after this addendum
- long-run stability and cleanup
- transient recovery and restart-storm avoidance
- operator/engineer telemetry and diagnostics
- graph-rich runtime observability
- testing depth and failure-injection strategy
- security hardening for API and protocol credentials
- media-quality policy and transform defaults
- configuration validation, deduplication, import/export
- transport tuning and failover policy
- operational packaging, monitoring distribution, and supply-chain hygiene

### Still intentionally left for milestone-level planning
- exact sequencing of every issue into epics/sprints
- final benchmark-confirmed CPU/RSS numbers
- precise UX copy and permission wording
- environment-specific deployment nuances not yet decided

---

## 7. Recommended use

Keep `restream_platform_master_plan_v3.md` as the main steering document.

Use this addendum when:
- checking whether the master plan covers the currently visible operational issue inventory
- translating open issues into implementation epics
- ensuring the platform plan is not too architecture-heavy and still grounded in operating reality
