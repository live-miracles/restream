//! Small shared workflow helpers for agent-facing orchestration.

/// Default high-level step order for a change workflow.
pub fn default_change_sequence(execution_enabled: bool) -> Vec<&'static str> {
    let mut steps = vec![
        "get_agent_capabilities",
        "get_agent_context",
        "plan_pipeline_change",
    ];
    if execution_enabled {
        steps.extend([
            "create_agent_operation",
            "approve_agent_operation",
            "apply_agent_operation",
            "verify_agent_operation",
        ]);
    }
    steps
}

/// Returns whether a verification reason means the configuration was applied
/// but live runtime activation still depends on ingest state.
pub fn verification_reason_is_pending_input(reason: &str) -> bool {
    reason == "pendingInput"
}
