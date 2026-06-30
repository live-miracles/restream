---
name: concurrency-proof
description: Use when changing concurrency primitives, cancellation/wake paths, shared stage registries, thread-hop boundaries, or teardown/recovery status behavior in this repository. Adds the required proof workflow, gate commands, and status-contract checks that must land with the code change.
---

# Concurrency Proof

Use this skill when the task touches:

- task ↔ thread handoff
- wait/cancel or close/wake behavior
- shared stage create/reuse/cancel/recreate logic
- live teardown/recovery semantics
- operator-visible runtime status after cleanup

## Workflow

1. Identify the narrowest synchronization rule that can fail.
2. Add the right proof layer:
   - unit/regression test for visible lifecycle or status
   - loom model for wake/cancel or registry ordering
   - harness test for real sockets/processes/threads
3. Extend the mandatory gate if the new proof must stay enforced.
4. If runtime status semantics changed, update API/frontend-facing contract tests and docs in the same change.

## Mandatory Gates

Run the focused proof gate first:

```sh
bash ./scripts/check-concurrency-proof-fast.sh
```

Run the full live contract gate before sign-off:

```sh
bash ./scripts/check-concurrency-contract.sh
```

## Rules

- Do not rely on only one proof layer when the bug spans more than one boundary.
- Do not change teardown/recovery behavior without updating the live harness assertion.
- Do not add a new proof artifact and leave it outside the mandatory gate.

## Read this reference when needed

- [../../concurrency-proofing.md](../../concurrency-proofing.md)
