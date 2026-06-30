## Current Priorities

This document replaces the old rewrite/status/master-plan documents.
The Rust rewrite is no longer the main story; this file keeps only the work
that still appears worth pursuing from those plans.

## Current State

The repository already has:

- a Rust-native control plane and media runtime
- native RTMP/SRT ingest and egress
- shared runtime graph/telemetry surfaces
- an application layer for orchestration and persistence policy
- domain-owned typed config for key control-plane schemas
- agent-plane and MCP scaffolding

The main remaining work is not "finish the rewrite." It is focused hardening,
cleanup, and selective platform improvements.

## Worth Pursuing

### 1. Keep tightening layer boundaries

Continue only where there is still real coupling to remove:

- shrink large edge/runtime files when ownership becomes clearer
- keep moving persistence policy out of runtime-heavy modules
- keep JSON/view shaping close to the API edge
- avoid new modules or crates unless they remove real complexity

Primary references:

- [architecture.md](architecture.md)
- [layering-roadmap.md](layering-roadmap.md)
- [agent-guidance/skills/layering-audit/SKILL.md](agent-guidance/skills/layering-audit/SKILL.md)

### 2. Finish runtime/view separation

The engine should keep owning typed runtime state, while API-facing JSON stays
in edge/view-model code.

Still-useful direction:

- keep `api_runtime_views` and `api_view_models` as the HTTP-facing shape layer
- avoid pushing more `serde_json::Value` assembly back into runtime internals

### 3. Continue selective stage-sharing and planner cleanup

The intended direction still stands:

- share expensive transforms aggressively
- keep per-output state only for the last-hop sender concerns
- keep stage identity and planning typed rather than stringly

This matters more than any crate split.

### 4. Preserve the Rust-platform plus selective-FFmpeg strategy

The architectural choice remains sound:

- Rust owns orchestration, lifecycle, telemetry, and transport control
- FFmpeg remains the right place for codec-heavy transforms

Do not treat "remove FFmpeg" as an active goal.

### 5. Harden quality, diagnostics, and operational safety

The still-relevant operational themes are:

- media-quality regression protection
- safe defaults for transforms and compatibility behavior
- strong diagnostics for source, stage, and output faults
- safe control-plane mutation flows with auditability

Primary references:

- [testing.md](testing.md)
- [testing-strategy.md](testing-strategy.md)
- [observability.md](observability.md)
- [agent-plane-integration.md](agent-plane-integration.md)

### 6. Treat custom/advanced paths conservatively

Keep the current standard:

- do not advertise custom encoding/runtime paths as fully supported without
  profiling and matrix evidence
- keep inactive or incomplete advanced paths explicitly gated or rejected

## Not Current Priorities

These older themes should not drive work by themselves:

- "finish the rewrite" as a broad program
- preserve old Node.js or MediaMTX mental models
- split crates for their own sake
- expand pure-Rust codec work for ideological reasons
- keep obsolete v2/v3 plan artifact trees alive

## How To Use This

Use this file as the replacement for the old top-level planning/status docs.

- For current architecture truth: read [architecture.md](architecture.md)
- For layering decisions: read [layering-roadmap.md](layering-roadmap.md)
- For testing/proof gates: read [testing.md](testing.md) and [AGENTS.md](../AGENTS.md)
