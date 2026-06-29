//! MCP tool catalog for the restream agent plane.

use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct McpToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub mutates: bool,
    pub requires_feature: &'static str,
    pub compiled_in: bool,
}

pub fn tool_catalog() -> Vec<McpToolDefinition> {
    let mut tools = vec![
        McpToolDefinition {
            name: "get_agent_capabilities",
            description: "Discover which agent-plane routes and workflows are available.",
            input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
            mutates: false,
            requires_feature: "agent-plane",
            compiled_in: true,
        },
        McpToolDefinition {
            name: "get_agent_context",
            description: "Fetch one redacted agent context bundle for reasoning.",
            input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
            mutates: false,
            requires_feature: "agent-plane",
            compiled_in: true,
        },
        McpToolDefinition {
            name: "investigate_pipeline_issue",
            description: "Bundle graph, alerts, telemetry, health, and events for incident triage.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "workflow": { "type": "string" },
                    "pipelineId": { "type": "string" },
                    "outputId": { "type": "string" },
                    "eventLimit": { "type": "integer" }
                },
                "additionalProperties": false
            }),
            mutates: false,
            requires_feature: "agent-plane",
            compiled_in: true,
        },
        McpToolDefinition {
            name: "plan_pipeline_change",
            description: "Create a draft plan from user intent and structured changes.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "intent": { "type": "string" },
                    "pipelineId": { "type": "string" },
                    "proposedChanges": { "type": "array" }
                },
                "required": ["intent"],
                "additionalProperties": false
            }),
            mutates: false,
            requires_feature: "agent-plane",
            compiled_in: true,
        },
        McpToolDefinition {
            name: "validate_change",
            description: "Validate a plan request without applying it.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "intent": { "type": "string" },
                    "pipelineId": { "type": "string" },
                    "proposedChanges": { "type": "array" }
                },
                "required": ["intent"],
                "additionalProperties": false
            }),
            mutates: false,
            requires_feature: "agent-plane",
            compiled_in: true,
        },
        McpToolDefinition {
            name: "preview_graph_diff",
            description: "Preview graph impact for a plan request.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "intent": { "type": "string" },
                    "pipelineId": { "type": "string" },
                    "proposedChanges": { "type": "array" }
                },
                "required": ["intent"],
                "additionalProperties": false
            }),
            mutates: false,
            requires_feature: "agent-plane",
            compiled_in: true,
        },
    ];

    tools.extend([
        McpToolDefinition {
            name: "create_agent_operation",
            description: "Create an approval-gated execution record for a validated plan.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "intent": { "type": "string" },
                    "pipelineId": { "type": "string" },
                    "idempotencyKey": { "type": "string" },
                    "actor": { "type": "string" },
                    "agentId": { "type": "string" },
                    "toolIdentity": { "type": "string" },
                    "incidentId": { "type": "string" },
                    "incidentLinks": { "type": "array" },
                    "proposedChanges": { "type": "array" }
                },
                "required": ["intent"],
                "additionalProperties": false
            }),
            mutates: true,
            requires_feature: "agent-execution",
            compiled_in: cfg!(feature = "agent-execution"),
        },
        McpToolDefinition {
            name: "get_agent_operation",
            description:
                "Read operation status, audit log, execution result, and verification result.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "operationId": { "type": "string" }
                },
                "required": ["operationId"],
                "additionalProperties": false
            }),
            mutates: false,
            requires_feature: "agent-execution",
            compiled_in: cfg!(feature = "agent-execution"),
        },
        McpToolDefinition {
            name: "approve_agent_operation",
            description: "Record explicit approval before apply is allowed.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "operationId": { "type": "string" },
                    "approvedBy": { "type": "string" },
                    "reason": { "type": "string" }
                },
                "required": ["operationId", "approvedBy"],
                "additionalProperties": false
            }),
            mutates: true,
            requires_feature: "agent-execution",
            compiled_in: cfg!(feature = "agent-execution"),
        },
        McpToolDefinition {
            name: "apply_agent_operation",
            description: "Apply an approved operation.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "operationId": { "type": "string" }
                },
                "required": ["operationId"],
                "additionalProperties": false
            }),
            mutates: true,
            requires_feature: "agent-execution",
            compiled_in: cfg!(feature = "agent-execution"),
        },
        McpToolDefinition {
            name: "verify_agent_operation",
            description: "Run post-change verification for an operation.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "operationId": { "type": "string" }
                },
                "required": ["operationId"],
                "additionalProperties": false
            }),
            mutates: true,
            requires_feature: "agent-execution",
            compiled_in: cfg!(feature = "agent-execution"),
        },
    ]);

    tools
}

pub fn available_tool_catalog() -> Vec<McpToolDefinition> {
    tool_catalog()
        .into_iter()
        .filter(|tool| tool.compiled_in)
        .collect()
}
