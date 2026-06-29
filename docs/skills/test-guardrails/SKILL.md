---
name: test-guardrails
description: Use when changing tests, benchmarks, harness modes, fixture/media setup, or frontend/backend contract code in this repository. Enforces quiet passing logs, fixture-first media usage, API choke-point discipline, and the correctness-vs-measurement split.
---

# Test Guardrails

Use this skill when the task touches:

- tests, benches, or harness modes
- fixture/media additions or changes
- frontend/backend API contract code
- measurement harness setup or release evidence commands

## Workflow

1. Reuse checked-in assets through `src/test_fixtures.rs`.
2. If a new committed asset is required, register it in `REQUIRED_CHECKED_IN_FIXTURES`.
3. Keep correctness work parallelizable only when isolated; keep measurement work serial and bench-profile only.
4. Route dashboard API calls through `public/ts/core/api.ts` and update contract tests when routes or payloads change.
5. Keep passing logs quiet; suppress expected noise in the helper, not in CI.

## Mandatory Gates

Run the fixture/media guard when touching test assets or harness setup:

```sh
./scripts/check-fixture-discipline.sh
```

Run the frontend/backend contract gate when touching API surface or dashboard callers:

```sh
./scripts/check-api-contract.sh
```

Run the broad quiet-log gate before sign-off on test-heavy changes:

```sh
./scripts/check-test-hygiene.sh
```

## Rules

- Do not add inline media generators to test-facing code when an existing fixture can cover the case.
- Do not add a committed fixture without registering it in the fixture contract.
- Do not run measurement harness modes from `target/debug` or `target/release`; use `./scripts/build-bench-harness.sh` and `target/bench/test_harness`.
- Do not let passing tests emit warnings, panic text, or known FFmpeg noise.

## Read These References

- [../../testing.md](../../testing.md)
- [../../../CLAUDE.md](../../../CLAUDE.md)
- [../../../src/test_fixtures.rs](../../../src/test_fixtures.rs)
