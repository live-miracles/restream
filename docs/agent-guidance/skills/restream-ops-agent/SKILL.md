---
name: restream-ops-agent
description: Use when working on this repository through the agent-plane workflow: investigating pipeline problems, planning safe output changes, creating approval-gated operations, applying them only after approval, and verifying the result through `/api/v1/agent/*` instead of raw control-plane mutation routes.
---

# Restream Ops Agent

Use this skill when the task is about operating the restream platform through
the agent plane rather than editing application code.

## Goals

- use the task-oriented agent plane, not raw output CRUD, when possible
- read redacted state before proposing changes
- plan and validate before mutation
- require explicit approval before apply
- always verify after apply

## Workflow

### Investigation

For incident-shaped requests:

1. Get agent capabilities.
2. Get agent context.
3. Run an investigation workflow for the target pipeline or output.
4. Summarize evidence, alerts, graph state, and recommended next action.

### Change requests

For add/update/remove/start/stop output requests:

1. Read agent context first.
2. Create a structured plan request.
3. Validate the requested change.
4. If invalid, stop and explain the validation errors in operator language.
5. If valid, summarize graph impact and expected runtime effect before asking
   for approval or creating an operation.

### Mutation

When execution is available:

1. Create an agent operation.
2. Do not apply until approval is explicitly recorded.
3. Apply the operation.
4. Verify the operation.
5. Report whether the result is:
   - persisted only
   - stopped as requested
   - running as requested
   - blocked on missing ingest or other runtime conditions

## Rules

- Prefer `/api/v1/agent/*` over raw `/api/v1/pipelines/*` mutation routes.
- Treat the platform validation result as the source of truth.
- Do not claim success before verification.
- If verification returns `pendingInput`, explain that the configuration change
  succeeded but live runtime activation depends on ingest being on.
- Prefer creating outputs with `desiredState=stopped` unless the user clearly
  asks for immediate live start.

## Read this reference when needed

For tool names, payload shapes, and the recommended MCP wrapper contract, read:

- [references/tool-contract.md](references/tool-contract.md)
