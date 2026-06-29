# MCP Tool Contract

This reference file supports the `restream-ops-agent` skill.

## Tool mapping

| Tool | Route | Use for |
|---|---|---|
| `get_agent_capabilities` | `GET /api/v1/agent/capabilities` | discover whether read/planning/execution are compiled in |
| `get_agent_context` | `GET /api/v1/agent/context` | fetch one redacted state bundle for reasoning |
| `investigate_pipeline_issue` | `POST /api/v1/agent/investigations` | incident triage and evidence collection |
| `plan_pipeline_change` | `POST /api/v1/agent/plans` | generate a draft plan plus graph and impact preview |
| `validate_change` | `POST /api/v1/agent/plans/validate` | return validation only |
| `preview_graph_diff` | `POST /api/v1/agent/graph-diff-preview` | show graph impact when the client wants preview only |
| `create_agent_operation` | `POST /api/v1/agent/operations` | create an approval-gated execution object |
| `get_agent_operation` | `GET /api/v1/agent/operations/:operation_id` | read operation status, audit, execution, and verification |
| `approve_agent_operation` | `POST /api/v1/agent/operations/:operation_id/approve` | record explicit human approval |
| `apply_agent_operation` | `POST /api/v1/agent/operations/:operation_id/apply` | apply approved changes |
| `verify_agent_operation` | `POST /api/v1/agent/operations/:operation_id/verify` | perform post-change verification |

## Common request patterns

### Investigate

```json
{
  "workflow": "investigatePipelineIssue",
  "pipelineId": "p1",
  "outputId": "out_123",
  "eventLimit": 25
}
```

### Plan a new output

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

### Create an operation

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

## Interpretation guidance

- `validation.valid == false`
  - Do not proceed to apply. Explain the errors and stop.
- `executionEnabled == false`
  - Planning is available, but apply/verify routes may be compiled out.
- `approvalRequired == true`
  - Do not call apply until approval is recorded.
- verification reason `pendingInput`
  - Config is present, but runtime cannot be live until ingest is on.
- verification reason `stopped`
  - Desired stopped state is satisfied.

## Output change kinds currently supported

- `addOutput`
- `updateOutput`
- `removeOutput`
- `startOutput`
- `stopOutput`

Do not invent unsupported change kinds.
