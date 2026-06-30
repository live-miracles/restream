---
name: rust-layering-audit
description: Use when refactoring Rust architecture in this repository to improve layering, extract orchestration into the application layer, evaluate whether a module boundary is justified, or decide when not to split further. Adds the repo-specific layering ladder, stop rules, and verification workflow learned from the restream layering pass.
---

# Rust Layering Audit

Use this skill when the task is about Rust layering, module ownership, or
possible crate extraction in this repository.

## Goals

- move orchestration out of edge/runtime code only when it removes real coupling
- keep runtime-heavy modules focused on runtime concerns
- keep persistence policy in `application` or `db`, not in `media`
- stop before layering turns into wrapper code and file churn

## Layering Ladder

Prefer the lightest boundary that fixes the coupling:

1. File split for readability or merge pressure.
2. Module for one concept that owns its types, validation, and helpers.
3. Visibility tightening for an already-correct boundary.
4. Trait/port when a layer should depend on a capability instead of storage.
5. Crate only after the module API is already stable and intentionally narrow.

Do not jump to a crate because a module feels busy.

## Good Extractions In This Repo

These moves are good signals for future work:

- repeated pipeline/ingest/runtime orchestration into `application::ingest`
- cross-source settings reads into `application::settings`
- meta-backed transcode profile persistence into `application::transcode_profiles`
- runtime-only profile cache/defaults staying in `media::profiles`

Pattern:

- `api` owns validation, auth checks, and response shaping
- `application` owns orchestration and persistence policy
- `media` owns runtime state, hot-path logic, and cache/defaults
- `db` owns raw SQL

## Stop Rules

Stop when the next move is more conceptual than operational.

A new boundary is justified when it:

- removes duplicated orchestration across handlers or runtime entry points
- hides storage/runtime coupling behind a stable capability
- moves API-shaped or persistence-shaped logic out of runtime internals
- makes tests target behavior at the right layer

Do not extract when it mostly:

- renames code without changing dependency flow
- wraps one DB call in a paper-thin module
- adds ports that only one callsite uses and are unlikely to stabilize
- scatters endpoint-local CRUD into many tiny files

## Review Checklist

When auditing a candidate seam:

1. Find the repeated behavior or wrong dependency direction.
2. State the owner layer in one sentence.
3. Check whether an existing module can own it before creating a new one.
4. Keep the edge layer responsible for transport concerns.
5. Keep the runtime layer responsible for hot-path/runtime concerns.
6. Add or update focused tests that prove the moved behavior still works.
7. Reassess after the change whether another extraction is still justified.

## Verification

- run focused tests first for the touched seam
- prefer application-level tests for orchestration extractions
- keep API contract tests when edge behavior still depends on the seam
- if the change touches hot runtime code, follow the benchmark/proof rules in `CLAUDE.md`

## Read This Reference

- [../../rust-layering-roadmap.md](../../rust-layering-roadmap.md)
