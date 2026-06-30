---
name: layering-audit
description: Use when refactoring Rust or frontend TypeScript architecture in this repository to improve layering, move orchestration into the right owner layer, evaluate whether a module boundary is justified, or decide when not to split further. Adds the repo-specific layering ladder, stop rules, and verification workflow learned from the restream layering passes.
---

# Layering Audit

Use this skill when the task is about layering, module ownership, or boundary
splits in this repository.

## Goals

- move orchestration out of edge/runtime-heavy code only when it removes real coupling
- keep hot-path or render-hot modules focused on runtime/UI concerns
- keep persistence policy in `application` or `db`, not in `media`
- keep frontend composition in `app` and feature-local behavior in bounded feature modules
- stop before layering turns into wrapper code and file churn

## Layering Ladder

Prefer the lightest boundary that fixes the coupling:

1. File split for readability or merge pressure.
2. Module for one concept that owns its types, validation, helpers, and local state.
3. Visibility tightening for an already-correct boundary.
4. Capability/port/interface when a layer should depend on a stable contract instead of a concrete implementation.
5. Crate or package boundary only after the module API is already stable and intentionally narrow.

Do not jump to a crate, package, or new top-level folder because a module feels busy.

## Good Extractions In This Repo

Backend signals:

- repeated pipeline/ingest/runtime orchestration into `application::ingest`
- cross-source settings reads into `application::settings`
- meta-backed transcode profile persistence into `application::transcode_profiles`
- runtime-only profile cache/defaults staying in `media::profiles`

Frontend signals:

- dashboard feature wiring moving into `public/ts/app/`
- output-list rendering and delegated actions moving out of `pipeline-view.ts`
- shared fetch/state/URL helpers staying in `public/ts/core/`
- history-specific render/controller state staying inside `public/ts/history/`

Ownership pattern:

- backend `api` owns validation, auth checks, and response shaping
- backend `application` owns orchestration and persistence policy
- backend `media` owns runtime state, hot-path logic, and cache/defaults
- backend `db` owns raw SQL
- frontend `app` owns composition/bootstrap wiring
- frontend `core` owns shared transport, shared state, and pure transforms/helpers
- frontend `features` own bounded UI rendering and feature-local interaction logic
- frontend `history` owns history-specific state, polling, and rendering

## Stop Rules

Stop when the next move is more conceptual than operational.

A new boundary is justified when it:

- removes duplicated orchestration across handlers, runtime entry points, or frontend composition roots
- hides storage/runtime/UI coupling behind a stable capability or seam
- moves API-shaped or persistence-shaped logic out of runtime internals
- moves cross-feature composition out of feature modules and into a frontend app layer
- makes tests target behavior at the right layer

Do not extract when it mostly:

- renames code without changing dependency flow
- wraps one DB call or one DOM call in a paper-thin module
- adds ports that only one callsite uses and are unlikely to stabilize
- scatters endpoint-local CRUD or feature-local UI into many tiny files

## Review Checklist

When auditing a candidate seam:

1. Find the repeated behavior or wrong dependency direction.
2. State the owner layer in one sentence.
3. Check whether an existing module can own it before creating a new one.
4. Keep the edge layer responsible for transport or composition concerns.
5. Keep the runtime or render-hot layer responsible for hot-path/runtime/UI concerns.
6. Add or update focused tests that prove the moved behavior still works.
7. Reassess after the change whether another extraction is still justified.

## Verification

- run focused tests first for the touched seam
- prefer application-level tests for backend orchestration extractions
- keep API contract tests when edge behavior still depends on the seam
- keep frontend DOM/render tests around refactored UI seams
- if the change touches hot runtime code or high-frequency frontend refresh paths, follow the benchmark/proof rules in `AGENTS.md`

## Read This Reference

- [../../layering-roadmap.md](../../layering-roadmap.md)
