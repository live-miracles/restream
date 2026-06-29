# MCP Rust Architecture

This document describes how to add MCP support to `restream` while keeping all
three deployment modes open:

- embedded in the main `restream` binary
- sidecar `restream-mcp` service
- central MCP gateway for multiple `restream` instances

The design goal is simple: keep agent logic in shared Rust code, and keep
transport concerns thin.

## Design principles

- Put business logic in shared Rust modules, not in the MCP transport.
- Keep `/api/v1/agent/*` as the product-native control surface.
- Make both HTTP handlers and MCP tools call the same shared Rust code.
- Support both in-process execution and HTTP-adapter execution.
- Keep auth, redaction, validation, approval, apply, and verify semantics owned
  by the platform.

## Target layering

```text
shared Rust agent core
  -> tool handlers
  -> execution backend trait
     -> in-process backend
     -> HTTP backend

transports
  -> Axum /api/v1/agent/*
  -> MCP server transport
```

That gives one logical implementation with two transport adapters.

## Recommended module layout

This can start as internal modules and later become separate crates if needed.

```text
src/
  agent_plane.rs          # existing HTTP-facing planner/read models and helpers
  agent_execution.rs      # existing operation lifecycle and verification logic
  agent_mcp/
    mod.rs                # public MCP-facing entry points
    tools.rs              # MCP tool catalog and schemas
    handlers.rs           # tool handlers using shared core traits
    auth.rs               # MCP auth/session acquisition strategy
    transport.rs          # streamable-http / stdio adapter glue
  agent_core/
    mod.rs
    types.rs              # shared request/response structs
    backend.rs            # trait for invoking agent operations
    workflows.rs          # plan/apply/verify orchestration helpers
    errors.rs             # typed errors, feature-gated unavailable errors
    audit.rs              # common audit/event helpers when useful
  agent_backends/
    mod.rs
    in_process.rs         # direct AppState / shared-function backend
    http.rs               # calls /api/v1/agent/* over reqwest

src/bin/
  restream-mcp.rs         # standalone MCP server binary
```

## Shared trait boundary

The key abstraction is the execution backend. MCP should not know whether it is
talking to the local process or to a remote `restream` instance.

Example shape:

```rust
#[async_trait::async_trait]
pub trait AgentBackend: Send + Sync {
    async fn capabilities(&self) -> Result<AgentCapabilities, AgentError>;
    async fn context(&self) -> Result<serde_json::Value, AgentError>;
    async fn investigate(
        &self,
        req: InvestigationRequest,
    ) -> Result<serde_json::Value, AgentError>;
    async fn plan(&self, req: PlanRequest) -> Result<PlanResponse, AgentError>;
    async fn validate(&self, req: PlanRequest) -> Result<ValidationResult, AgentError>;
    async fn graph_diff(&self, req: PlanRequest) -> Result<GraphPreview, AgentError>;
    async fn create_operation(
        &self,
        req: OperationCreateRequest,
    ) -> Result<serde_json::Value, AgentError>;
    async fn get_operation(&self, operation_id: &str) -> Result<serde_json::Value, AgentError>;
    async fn approve_operation(
        &self,
        operation_id: &str,
        req: ApprovalRequest,
    ) -> Result<serde_json::Value, AgentError>;
    async fn apply_operation(&self, operation_id: &str) -> Result<serde_json::Value, AgentError>;
    async fn verify_operation(&self, operation_id: &str) -> Result<serde_json::Value, AgentError>;
}
```

This keeps the handlers transport-agnostic.

## Two backend implementations

### 1. In-process backend

Use this when MCP is embedded in the main binary.

Implementation idea:

- hold `Arc<AppState>`
- call shared functions already used by HTTP handlers
- avoid unnecessary loopback HTTP

Use this for:

- lowest latency
- local development
- simplest single-binary deployment

### 2. HTTP backend

Use this for a sidecar or central gateway.

Implementation idea:

- use `reqwest`
- call `/api/v1/agent/*`
- preserve route status codes and response envelopes closely

Use this for:

- process isolation
- independent scaling or restart policy
- fleet-wide central gateway mode

## MCP tool layer

The MCP server should be very thin. Its job is:

- expose tool names
- validate tool input shape
- call an `AgentBackend`
- return the platform result

It should not:

- reinterpret verification success
- infer unsupported actions
- create new approval logic
- bypass the agent plane for raw mutation routes

Recommended initial tool names:

- `get_agent_capabilities`
- `get_agent_context`
- `investigate_pipeline_issue`
- `plan_pipeline_change`
- `validate_change`
- `preview_graph_diff`
- `create_agent_operation`
- `get_agent_operation`
- `approve_agent_operation`
- `apply_agent_operation`
- `verify_agent_operation`

## Feature flags

Current flags:

- `agent-plane`
- `agent-execution`

Recommended additions:

- `mcp-core`
  - compiles shared MCP-facing tool definitions and traits
- `mcp-server`
  - enables MCP transport server code
- `mcp-http-backend`
  - enables the reqwest-based backend
- `mcp-embedded`
  - enables the in-process backend for the main binary

Suggested relationships:

```text
agent-execution -> agent-plane
mcp-core -> agent-plane
mcp-server -> mcp-core
mcp-http-backend -> mcp-core
mcp-embedded -> mcp-core + agent-plane
```

Do not make `mcp-server` imply `agent-execution`. Read-only and planning-only
MCP should still be possible.

## Binary and deployment modes

By default, the main `restream` build stays free of agent-plane MCP code. The
recommended release split is:

- `restream`
  - build with default features
- `restream-mcp`
  - build separately with `--features mcp-server,mcp-http-backend`

That means the production media/control-plane binary does not carry the MCP
transport unless we explicitly opt into an embedded mode later.

The current sidecar startup path also performs a compatibility probe against
the target `restream` instance before serving MCP traffic. By default it
expects the same application version and git commit on both sides. Override for
development only with `RESTREAM_MCP_VERSION_CHECK=warn` or
`RESTREAM_MCP_VERSION_CHECK=off`.

### Embedded mode

```text
restream binary
  - Axum UI/API
  - /api/v1/agent/*
  - MCP transport
```

Pros:

- simplest deployment
- no extra network hop
- easiest for local use

Cons:

- MCP traffic shares process fate with the media/control plane
- weaker separation for auth, rate limiting, and hardening

### Sidecar mode

```text
restream         :3030
restream-mcp     :4040
```

Pros:

- clean operational boundary
- separate rate limiting, auth, and audit knobs
- easiest production default

Cons:

- one extra network hop
- small amount of duplicated deployment plumbing

### Central gateway mode

```text
restream-mcp-gateway
  -> target restream instance A
  -> target restream instance B
  -> target restream instance C
```

Pros:

- one agent endpoint for many deployments
- useful for fleet operations and policy centralization

Cons:

- requires target discovery and routing
- stronger auth and tenancy model required

## Release examples

Build the main product binary:

```bash
cargo build --release --bin restream
```

Build the sidecar MCP binary:

```bash
cargo build --release --bin restream-mcp --features mcp-server,mcp-http-backend
```

Run the sidecar against a colocated `restream` instance:

```bash
RESTREAM_AGENT_BASE_URL=http://127.0.0.1:3030 \
cargo run --release --bin restream-mcp --features mcp-server,mcp-http-backend -- --bind 127.0.0.1:4040
```

## Deferred work

Deliberately deferred for a later pass:

- release/deploy automation that always ships `restream` and `restream-mcp`
  together as one rollout unit

The architecture assumes colocated releases, but the repo does not enforce
that packaging workflow yet.

## Auth model

Keep auth strategy separate from business logic.

Recommended approach:

- shared handler layer accepts an auth/session context object
- in-process backend uses local trusted identity
- HTTP backend uses:
  - dashboard session cookie, or
  - dedicated service credential, if introduced later

Do not bake a browser-only session assumption into the MCP core.

## Migration path from current code

### Phase 1: extract shared core

- move reusable request/response structs into `agent_core::types`
- move common workflow helpers behind `AgentBackend`
- keep existing HTTP behavior unchanged

### Phase 2: add in-process backend

- implement `agent_backends::in_process`
- have MCP handlers call it directly

### Phase 3: add standalone binary

- add `src/bin/restream-mcp.rs`
- implement streamable HTTP transport
- use the HTTP backend first if that is simpler operationally

### Phase 4: optional embedded transport

- mount MCP transport in the main binary behind `mcp-server + mcp-embedded`

## What not to do

- Do not duplicate validation rules in the MCP server.
- Do not make the MCP layer call raw pipeline/output mutation routes directly.
- Do not hardwire deployment to a single port model.
- Do not couple tool names to browser/dashboard concepts.
- Do not let sidecar mode drift semantically from embedded mode.

## Recommendation for this repository

Best near-term path:

1. Keep `/api/v1/agent/*` as the canonical product surface.
2. Extract shared Rust logic behind an `AgentBackend` trait.
3. Implement a standalone `restream-mcp` Rust binary first.
4. Preserve an embedded mode as a later feature-flagged option.

That keeps production deployment conservative while preserving the shared-code
architecture needed for all three modes.
