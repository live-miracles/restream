//! Optional agent execution plane.
//!
//! This module is compiled only with `agent-execution`, which depends on the
//! read/planning `agent-plane` feature. It owns operation state, approval
//! transitions, audit events, idempotency lookups, and redacted public views;
//! API handlers still perform the actual runtime mutations through core APIs.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::agent_plane::{PlanRequest, PlanResponse, ProposedChange};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationCreateRequest {
    pub intent: String,
    pub pipeline_id: Option<String>,
    #[serde(default)]
    pub proposed_changes: Vec<ProposedChange>,
    pub idempotency_key: Option<String>,
    pub actor: Option<String>,
    pub agent_id: Option<String>,
    pub tool_identity: Option<String>,
    pub incident_id: Option<String>,
    #[serde(default)]
    pub incident_links: Vec<String>,
}

impl OperationCreateRequest {
    pub fn plan_request(&self) -> PlanRequest {
        PlanRequest {
            intent: self.intent.clone(),
            pipeline_id: self.pipeline_id.clone(),
            proposed_changes: self.proposed_changes.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequest {
    pub approved_by: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyRequest {
    pub operation_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreCreateResult {
    pub operation: Value,
    pub reused: bool,
}

#[derive(Debug, Clone)]
pub struct OperationRecord {
    pub operation_id: String,
    pub idempotency_key: Option<String>,
    pub status: OperationStatus,
    pub request: OperationCreateRequest,
    pub plan: PlanResponse,
    pub plan_hash: String,
    pub approval: Option<ApprovalState>,
    pub approval_required: bool,
    pub created_at: String,
    pub updated_at: String,
    pub actor: String,
    pub agent_id: String,
    pub tool_identity: String,
    pub affected_objects: Value,
    pub warnings: Vec<String>,
    pub progress_snapshots: Vec<Value>,
    pub state_transitions: Vec<Value>,
    pub audit_log: Vec<Value>,
    pub execution_result: Option<Value>,
    pub verification_result: Option<Value>,
    pub pre_apply_alert_count: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OperationStatus {
    Invalid,
    AwaitingApproval,
    Approved,
    Applying,
    Applied,
    Verified,
    VerificationFailed,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalState {
    pub approved_by: String,
    pub reason: Option<String>,
    pub approved_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationStoreError {
    NotFound,
    Invalid,
    RequiresApproval,
    AlreadyApplying,
    AlreadyTerminal,
}

#[derive(Default)]
pub struct AgentExecutionStore {
    records: Mutex<HashMap<String, OperationRecord>>,
    idempotency: Mutex<HashMap<String, String>>,
}

impl AgentExecutionStore {
    pub fn create(
        &self,
        request: OperationCreateRequest,
        plan: PlanResponse,
        pre_alert_count: usize,
    ) -> StoreCreateResult {
        if let Some(key) = request.idempotency_key.as_deref()
            && let Some(operation_id) = self.idempotency.lock().unwrap().get(key).cloned()
            && let Some(record) = self.records.lock().unwrap().get(&operation_id).cloned()
        {
            return StoreCreateResult {
                operation: public_record(&record),
                reused: true,
            };
        }

        let created_at = now();
        let plan_request = request.plan_request();
        let plan_hash = plan_hash(&plan_request);
        let operation_id = operation_id(&request, &created_at);
        let status = if plan.validation.valid {
            OperationStatus::AwaitingApproval
        } else {
            OperationStatus::Invalid
        };
        let affected_objects = affected_objects(&plan_request);
        let mut audit_log = vec![audit_event(
            "created",
            &created_at,
            "operation object created from agent plan",
            serde_json::json!({
                "planHash": plan_hash,
                "approvalRequired": true,
                "valid": plan.validation.valid,
                "incidentId": request.incident_id,
                "incidentLinks": request.incident_links,
            }),
        )];
        if status == OperationStatus::Invalid {
            audit_log.push(audit_event(
                "invalid",
                &created_at,
                "operation cannot be approved or applied until validation errors are fixed",
                serde_json::json!({"validation": plan.validation}),
            ));
        }
        let warnings = plan
            .validation
            .warnings
            .iter()
            .map(|issue| issue.message.clone())
            .collect();

        let record = OperationRecord {
            operation_id: operation_id.clone(),
            idempotency_key: request.idempotency_key.clone(),
            status,
            plan,
            plan_hash,
            approval: None,
            approval_required: true,
            created_at: created_at.clone(),
            updated_at: created_at,
            actor: request.actor.clone().unwrap_or_else(|| "agent".to_string()),
            agent_id: request
                .agent_id
                .clone()
                .unwrap_or_else(|| "unspecified-agent".to_string()),
            tool_identity: request
                .tool_identity
                .clone()
                .unwrap_or_else(|| "agent-execution-api".to_string()),
            affected_objects,
            warnings,
            progress_snapshots: Vec::new(),
            state_transitions: Vec::new(),
            audit_log,
            execution_result: None,
            verification_result: None,
            pre_apply_alert_count: Some(pre_alert_count),
            request,
        };

        if let Some(key) = record.idempotency_key.clone() {
            self.idempotency
                .lock()
                .unwrap()
                .insert(key, operation_id.clone());
        }
        self.records
            .lock()
            .unwrap()
            .insert(operation_id.clone(), record.clone());

        StoreCreateResult {
            operation: public_record(&record),
            reused: false,
        }
    }

    pub fn get(&self, operation_id: &str) -> Option<OperationRecord> {
        self.records.lock().unwrap().get(operation_id).cloned()
    }

    pub fn approve(
        &self,
        operation_id: &str,
        approval: ApprovalRequest,
    ) -> Result<OperationRecord, OperationStoreError> {
        let mut records = self.records.lock().unwrap();
        let record = records
            .get_mut(operation_id)
            .ok_or(OperationStoreError::NotFound)?;
        match record.status {
            OperationStatus::Invalid => return Err(OperationStoreError::Invalid),
            OperationStatus::AwaitingApproval => {}
            OperationStatus::Applying => return Err(OperationStoreError::AlreadyApplying),
            OperationStatus::Applied
            | OperationStatus::Verified
            | OperationStatus::VerificationFailed
            | OperationStatus::Failed => return Err(OperationStoreError::AlreadyTerminal),
            OperationStatus::Approved => {}
        }

        let approved_at = now();
        record.status = OperationStatus::Approved;
        record.approval = Some(ApprovalState {
            approved_by: approval.approved_by,
            reason: approval.reason,
            approved_at: approved_at.clone(),
        });
        record.updated_at = approved_at.clone();
        record.audit_log.push(audit_event(
            "approved",
            &approved_at,
            "operation approved for application",
            serde_json::json!({"approval": record.approval}),
        ));
        Ok(record.clone())
    }

    pub fn start_apply(&self, operation_id: &str) -> Result<OperationRecord, OperationStoreError> {
        let mut records = self.records.lock().unwrap();
        let record = records
            .get_mut(operation_id)
            .ok_or(OperationStoreError::NotFound)?;
        match record.status {
            OperationStatus::Invalid => return Err(OperationStoreError::Invalid),
            OperationStatus::AwaitingApproval => return Err(OperationStoreError::RequiresApproval),
            OperationStatus::Applying => return Err(OperationStoreError::AlreadyApplying),
            OperationStatus::Applied
            | OperationStatus::Verified
            | OperationStatus::VerificationFailed
            | OperationStatus::Failed => return Err(OperationStoreError::AlreadyTerminal),
            OperationStatus::Approved => {}
        }

        let ts = now();
        record.status = OperationStatus::Applying;
        record.updated_at = ts.clone();
        record.audit_log.push(audit_event(
            "applyStarted",
            &ts,
            "operation application started",
            serde_json::json!({}),
        ));
        Ok(record.clone())
    }

    pub fn complete_apply(
        &self,
        operation_id: &str,
        state_transitions: Vec<Value>,
        progress_snapshots: Vec<Value>,
        execution_result: Value,
    ) -> Option<OperationRecord> {
        self.update(operation_id, |record, ts| {
            record.status = OperationStatus::Applied;
            record.state_transitions.extend(state_transitions);
            record.progress_snapshots.extend(progress_snapshots);
            record.execution_result = Some(execution_result);
            record.audit_log.push(audit_event(
                "applyCompleted",
                &ts,
                "operation application completed",
                serde_json::json!({"result": record.execution_result}),
            ));
        })
    }

    pub fn fail_apply(&self, operation_id: &str, error: String) -> Option<OperationRecord> {
        self.update(operation_id, |record, ts| {
            record.status = OperationStatus::Failed;
            record.execution_result = Some(serde_json::json!({
                "success": false,
                "error": error,
            }));
            record.audit_log.push(audit_event(
                "applyFailed",
                &ts,
                "operation application failed",
                serde_json::json!({"error": error}),
            ));
        })
    }

    pub fn complete_verify(
        &self,
        operation_id: &str,
        verification_result: Value,
    ) -> Option<OperationRecord> {
        self.update(operation_id, |record, ts| {
            record.status = if verification_result["success"].as_bool().unwrap_or(false) {
                OperationStatus::Verified
            } else {
                OperationStatus::VerificationFailed
            };
            record.verification_result = Some(verification_result);
            record.audit_log.push(audit_event(
                "verified",
                &ts,
                "operation post-change verification completed",
                serde_json::json!({"result": record.verification_result}),
            ));
        })
    }

    fn update(
        &self,
        operation_id: &str,
        f: impl FnOnce(&mut OperationRecord, String),
    ) -> Option<OperationRecord> {
        let mut records = self.records.lock().unwrap();
        let record = records.get_mut(operation_id)?;
        let ts = now();
        f(record, ts.clone());
        record.updated_at = ts;
        Some(record.clone())
    }
}

pub fn public_record(record: &OperationRecord) -> Value {
    crate::agent_plane::redact_secrets(serde_json::json!({
        "operationId": record.operation_id,
        "idempotencyKey": record.idempotency_key,
        "status": record.status,
        "approvalRequired": record.approval_required,
        "approval": record.approval,
        "createdAt": record.created_at,
        "updatedAt": record.updated_at,
        "actor": record.actor,
        "agentId": record.agent_id,
        "toolIdentity": record.tool_identity,
        "incidentId": record.request.incident_id,
        "incidentLinks": record.request.incident_links,
        "intentSummary": record.request.intent,
        "proposedPlanHash": record.plan_hash,
        "request": record.request,
        "plan": record.plan,
        "affectedObjects": record.affected_objects,
        "warnings": record.warnings,
        "progressSnapshots": record.progress_snapshots,
        "stateTransitions": record.state_transitions,
        "auditLog": record.audit_log,
        "executionResult": record.execution_result,
        "verificationResult": record.verification_result,
        "preApplyAlertCount": record.pre_apply_alert_count,
    }))
}

fn affected_objects(request: &PlanRequest) -> Value {
    let mut pipelines = Vec::new();
    let mut outputs = Vec::new();
    for change in &request.proposed_changes {
        if let Some(pid) = change
            .pipeline_id
            .as_deref()
            .or(request.pipeline_id.as_deref())
            && !pipelines.iter().any(|existing: &String| existing == pid)
        {
            pipelines.push(pid.to_string());
        }
        if let Some(output_id) = &change.output_id
            && !outputs.iter().any(|existing| existing == output_id)
        {
            outputs.push(output_id.clone());
        }
    }
    pipelines.sort();
    outputs.sort();
    serde_json::json!({
        "pipelineIds": pipelines,
        "outputIds": outputs,
    })
}

fn plan_hash(request: &PlanRequest) -> String {
    let raw = serde_json::to_vec(request).unwrap_or_default();
    let digest = Sha256::digest(raw);
    format!("sha256:{}", hex_prefix(&digest, 32))
}

fn operation_id(request: &OperationCreateRequest, created_at: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(created_at.as_bytes());
    hasher.update(serde_json::to_vec(request).unwrap_or_default());
    hasher.update(rand::random::<[u8; 16]>());
    let digest = hasher.finalize();
    format!("op_{}", hex_prefix(&digest, 16))
}

fn audit_event(kind: &str, at: &str, summary: &str, details: Value) -> Value {
    serde_json::json!({
        "kind": kind,
        "at": at,
        "summary": summary,
        "details": details,
    })
}

fn hex_prefix(bytes: &[u8], len: usize) -> String {
    bytes
        .iter()
        .flat_map(|b| [b >> 4, b & 0x0f])
        .take(len)
        .map(|n| char::from_digit(n as u32, 16).unwrap_or('0'))
        .collect()
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
