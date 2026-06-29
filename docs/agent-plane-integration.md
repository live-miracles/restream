# Agent Plane, MCP, and Skill Integration

This document turns the existing agent-plane API into a practical agent
integration design for this repository.

## Recommended layering

```text
human request
  -> Codex / Claude / Copilot agent
  -> repo-specific skill
  -> MCP server wrapper
  -> /api/v1/agent/*
  -> core control-plane services and media runtime
```

Keep business logic in the product:

- the agent plane owns redaction, validation, approval, apply, and verify
- the MCP server owns transport and tool exposure
- the skill owns agent behavior and sequencing

Do not put plan/apply safety policy in the MCP wrapper. That would duplicate
logic that already exists in the application.

## Why use the agent plane instead of raw APIs

The control plane is object-oriented:

- pipelines
- outputs
- settings
- alerts
- diagnostics

The agent plane is task-oriented:

- capability discovery
- redacted state context
- investigation bundles
- plan and validation
- approval-gated execution
- post-change verification

An MCP wrapper should therefore prefer `/api/v1/agent/*` over raw
`/api/v1/pipelines/*` mutation routes.

## Minimal MCP tool catalog

Start with a thin wrapper around the agent routes that already exist.

| MCP tool | Method | Route |
|---|---|---|
| `get_agent_capabilities` | `GET` | `/api/v1/agent/capabilities` |
| `get_agent_context` | `GET` | `/api/v1/agent/context` |
| `investigate_pipeline_issue` | `POST` | `/api/v1/agent/investigations` |
| `plan_pipeline_change` | `POST` | `/api/v1/agent/plans` |
| `validate_change` | `POST` | `/api/v1/agent/plans/validate` |
| `preview_graph_diff` | `POST` | `/api/v1/agent/graph-diff-preview` |
| `create_agent_operation` | `POST` | `/api/v1/agent/operations` |
| `get_agent_operation` | `GET` | `/api/v1/agent/operations/:operation_id` |
| `approve_agent_operation` | `POST` | `/api/v1/agent/operations/:operation_id/approve` |
| `apply_agent_operation` | `POST` | `/api/v1/agent/operations/:operation_id/apply` |
| `verify_agent_operation` | `POST` | `/api/v1/agent/operations/:operation_id/verify` |

## Suggested MCP tool shapes

These tool schemas are intentionally thin. The wrapper should mostly pass
through request and response bodies.

### `get_agent_context`

Input:

```json
{}
```

Output:

- direct pass-through of `AgentContextV1`

### `investigate_pipeline_issue`

Input:

```json
{
  "workflow": "investigatePipelineIssue",
  "pipelineId": "p1",
  "outputId": "out_123",
  "eventLimit": 25
}
```

Output:

- direct pass-through of `InvestigationResponse`

### `plan_pipeline_change`

Input:

```json
{
  "intent": "Attach a stopped YouTube RTMP output",
  "pipelineId": "p1",
  "proposedChanges": [{
    "kind": "addOutput",
    "name": "YouTube Primary",
    "url": "rtmp://a.rtmp.youtube.com/live2/xxxx-xxxx",
    "encoding": "source",
    "desiredState": "stopped"
  }]
}
```

Output:

- direct pass-through of `PlanResponse`

### `create_agent_operation`

Input:

```json
{
  "intent": "Attach a stopped YouTube RTMP output",
  "pipelineId": "p1",
  "idempotencyKey": "req-123",
  "actor": "ops-agent",
  "agentId": "codex-restream",
  "toolIdentity": "restream-mcp",
  "proposedChanges": [{
    "kind": "addOutput",
    "name": "YouTube Primary",
    "url": "rtmp://a.rtmp.youtube.com/live2/xxxx-xxxx",
    "encoding": "source",
    "desiredState": "stopped"
  }]
}
```

Output:

- direct pass-through of `OperationRecord`

### `approve_agent_operation`

Input:

```json
{
  "operationId": "op_123",
  "approvedBy": "human-operator",
  "reason": "Reviewed graph and blast radius"
}
```

Wrapper behavior:

- extract `operationId` from tool input
- send `{ "approvedBy": "...", "reason": "..." }` to the route body

### `apply_agent_operation`

Input:

```json
{
  "operationId": "op_123"
}
```

### `verify_agent_operation`

Input:

```json
{
  "operationId": "op_123"
}
```

## Wrapper rules

The MCP server should enforce only transport-level conventions:

- require authenticated session/cookie or bearer-style gateway credential
- return raw platform validation failures instead of rewriting them
- preserve `compiledIn: false` and feature-gated `404` responses
- expose operation IDs and plan IDs unchanged
- avoid logging raw URLs or stream keys

It should not:

- invent new approval semantics
- bypass `/api/v1/agent/*` to call raw mutation routes
- reinterpret verification results into success when the platform says failure

## Example workflow

User request:

> Add a YouTube output to pipeline `p1`, keep it stopped, ask before applying,
> and verify afterward.

Expected agent sequence:

1. `get_agent_capabilities`
2. `get_agent_context`
3. `plan_pipeline_change`
4. If `validation.valid == false`, stop and explain errors.
5. `create_agent_operation`
6. Wait for human approval.
7. `approve_agent_operation`
8. `apply_agent_operation`
9. `verify_agent_operation`
10. Summarize:
   - what changed
   - whether verification passed
   - whether runtime is stopped/running/pending input

## Suggested production path

If this repository promotes the pattern beyond docs:

1. Keep the existing in-process agent plane as the source of truth.
2. Add a small authenticated MCP gateway process or module.
3. Expose only the minimal tool catalog above.
4. Ship a repo-specific skill so agents follow the right workflow.

That keeps the control-plane API human-friendly while making the platform
usable by serious tool-calling agents without asking them to improvise safety.
