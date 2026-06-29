//! HTTP backend that talks to the existing `/api/v1/agent/*` routes.

use crate::agent_core::backend::{AgentBackend, AgentFuture};
use crate::agent_core::errors::AgentError;
use crate::agent_core::types::{
    ApprovalRequest, InvestigationRequest, OperationCreateRequest, PlanRequest, VerifyRequest,
};
use reqwest::header::{CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone)]
pub enum HttpAgentAuth {
    SessionCookie(String),
    BearerToken(String),
    None,
}

#[derive(Clone)]
pub struct HttpAgentBackend {
    client: reqwest::Client,
    base_url: String,
    auth: HttpAgentAuth,
}

impl HttpAgentBackend {
    pub fn new(base_url: impl Into<String>, auth: HttpAgentAuth) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth,
        }
    }

    async fn get_json(&self, path: &str) -> Result<Value, AgentError> {
        let response = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .headers(self.auth_headers()?)
            .send()
            .await?;
        Self::decode_response(response).await
    }

    async fn post_json<T: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<Value, AgentError> {
        let response = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .headers(self.auth_headers()?)
            .header(CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(body)?)
            .send()
            .await?;
        Self::decode_response(response).await
    }

    fn auth_headers(&self) -> Result<HeaderMap, AgentError> {
        let mut headers = HeaderMap::new();
        match &self.auth {
            HttpAgentAuth::SessionCookie(cookie) => {
                headers.insert(
                    COOKIE,
                    HeaderValue::from_str(cookie).map_err(|err| {
                        AgentError::InvalidRequest(format!("invalid session cookie: {err}"))
                    })?,
                );
            }
            HttpAgentAuth::BearerToken(token) => {
                let mut value =
                    HeaderValue::from_str(&format!("Bearer {token}")).map_err(|err| {
                        AgentError::InvalidRequest(format!("invalid bearer token: {err}"))
                    })?;
                value.set_sensitive(true);
                headers.insert(reqwest::header::AUTHORIZATION, value);
            }
            HttpAgentAuth::None => {}
        }
        Ok(headers)
    }

    async fn decode_response(response: reqwest::Response) -> Result<Value, AgentError> {
        let status = response.status();
        let text = response.text().await?;
        let body =
            serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({ "raw": text }));
        if status.is_success() {
            Ok(body)
        } else {
            Err(AgentError::Upstream {
                status: status.as_u16(),
                body: body.to_string(),
            })
        }
    }
}

impl AgentBackend for HttpAgentBackend {
    fn capabilities(&self) -> AgentFuture<'_, Value> {
        Box::pin(async move { self.get_json("/api/v1/agent/capabilities").await })
    }

    fn context(&self) -> AgentFuture<'_, Value> {
        Box::pin(async move { self.get_json("/api/v1/agent/context").await })
    }

    fn investigate(&self, request: InvestigationRequest) -> AgentFuture<'_, Value> {
        Box::pin(async move {
            self.post_json("/api/v1/agent/investigations", &request)
                .await
        })
    }

    fn plan(&self, request: PlanRequest) -> AgentFuture<'_, Value> {
        Box::pin(async move { self.post_json("/api/v1/agent/plans", &request).await })
    }

    fn validate(&self, request: PlanRequest) -> AgentFuture<'_, Value> {
        Box::pin(async move {
            self.post_json("/api/v1/agent/plans/validate", &request)
                .await
        })
    }

    fn graph_diff(&self, request: PlanRequest) -> AgentFuture<'_, Value> {
        Box::pin(async move {
            self.post_json("/api/v1/agent/graph-diff-preview", &request)
                .await
        })
    }

    fn create_operation(&self, request: OperationCreateRequest) -> AgentFuture<'_, Value> {
        Box::pin(async move { self.post_json("/api/v1/agent/operations", &request).await })
    }

    fn get_operation(&self, operation_id: &str) -> AgentFuture<'_, Value> {
        let operation_id = operation_id.to_string();
        Box::pin(async move {
            self.get_json(&format!("/api/v1/agent/operations/{operation_id}"))
                .await
        })
    }

    fn approve_operation(
        &self,
        operation_id: &str,
        request: ApprovalRequest,
    ) -> AgentFuture<'_, Value> {
        let operation_id = operation_id.to_string();
        Box::pin(async move {
            self.post_json(
                &format!("/api/v1/agent/operations/{operation_id}/approve"),
                &request,
            )
            .await
        })
    }

    fn apply_operation(&self, operation_id: &str) -> AgentFuture<'_, Value> {
        let operation_id = operation_id.to_string();
        Box::pin(async move {
            self.post_json(
                &format!("/api/v1/agent/operations/{operation_id}/apply"),
                &serde_json::json!({}),
            )
            .await
        })
    }

    fn verify_operation(&self, request: VerifyRequest) -> AgentFuture<'_, Value> {
        Box::pin(async move { self.post_json("/api/v1/agent/verify", &request).await })
    }
}
