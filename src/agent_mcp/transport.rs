//! MCP transport adapters for the agent-facing control surface.
//! This file owns the protocol boundary for stdio and streamable-HTTP MCP
//! sessions, including request framing, JSON-RPC flow, initialization, and
//! transport-specific origin/header policy. Tool execution stays in handlers
//! and backends; this layer is only responsible for carrying those operations
//! over MCP transports.

use crate::agent_core::backend::AgentBackend;
use crate::agent_core::errors::AgentError;
use crate::agent_mcp::handlers::AgentToolHandler;
use crate::agent_mcp::tools::available_tool_catalog;
use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::{ALLOW, HeaderMap, HeaderName, HeaderValue, ORIGIN};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use bytes::Bytes;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportMode {
    StreamableHttp,
    Stdio,
}

#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub mode: TransportMode,
    pub bind_addr: String,
    pub allowed_origins: Vec<String>,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            mode: TransportMode::StreamableHttp,
            bind_addr: "127.0.0.1:4040".to_string(),
            allowed_origins: Vec::new(),
        }
    }
}

pub async fn run_server<B: AgentBackend>(
    config: TransportConfig,
    backend: Arc<B>,
) -> Result<(), AgentError>
where
    B: 'static,
{
    match config.mode {
        TransportMode::Stdio => {
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || run_stdio_server(handle, backend))
                .await
                .map_err(|err| AgentError::Transport(format!("stdio server task failed: {err}")))?
        }
        TransportMode::StreamableHttp => run_http_server(config, backend).await,
    }
}

const PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "restream-mcp";
const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";

fn run_stdio_server<B: AgentBackend>(
    runtime: tokio::runtime::Handle,
    backend: Arc<B>,
) -> Result<(), AgentError> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());
    let mut buffer = String::new();
    let mut initialize_seen = false;
    let mut initialized = false;
    let handler = AgentToolHandler::new(backend);

    loop {
        buffer.clear();
        let bytes = reader
            .read_line(&mut buffer)
            .map_err(|err| AgentError::Transport(format!("failed to read stdin: {err}")))?;
        if bytes == 0 {
            writer
                .flush()
                .map_err(|err| AgentError::Transport(format!("failed to flush stdout: {err}")))?;
            return Ok(());
        }

        let message = buffer.trim();
        if message.is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(message) {
            Ok(value) => value,
            Err(err) => {
                write_message(
                    &mut writer,
                    &error_response(
                        Value::Null,
                        -32700,
                        format!("invalid JSON-RPC payload: {err}"),
                        None,
                    ),
                )?;
                continue;
            }
        };

        let response = match runtime.block_on(process_stdio_jsonrpc(
            &handler,
            request,
            &mut initialize_seen,
            &mut initialized,
        )) {
            Ok(Some(response)) => response,
            Ok(None) => continue,
            Err(err) => error_response(Value::Null, -32000, err.to_string(), None),
        };
        write_message(&mut writer, &response)?;
    }
}

async fn process_stdio_jsonrpc<B: AgentBackend>(
    handler: &AgentToolHandler<B>,
    request: Value,
    initialize_seen: &mut bool,
    initialized: &mut bool,
) -> Result<Option<Value>, AgentError> {
    let id = request.get("id").cloned();
    let method = request
        .get("method")
        .and_then(|value| value.as_str())
        .map(str::to_owned);
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let is_notification = id.is_none();

    let Some(method) = method else {
        if request.get("result").is_some() || request.get("error").is_some() {
            return Ok(None);
        }
        return Ok((!is_notification).then(|| {
            error_response(
                id.unwrap_or(Value::Null),
                -32600,
                "missing method".to_string(),
                None,
            )
        }));
    };

    match method.as_str() {
        "initialize" => {
            *initialize_seen = true;
            Ok(Some(initialize_response(id.unwrap_or(Value::Null))))
        }
        "notifications/initialized" => {
            if *initialize_seen {
                *initialized = true;
            }
            Ok(None)
        }
        "ping" => Ok((!is_notification).then(|| {
            json!({
                "jsonrpc": "2.0",
                "id": id.unwrap_or(Value::Null),
                "result": {}
            })
        })),
        "tools/list" => {
            if let Some(response) =
                initialization_guard_response(id.clone(), *initialize_seen, *initialized)
            {
                return Ok(Some(response));
            }
            Ok((!is_notification).then(|| tools_list_response(id.unwrap_or(Value::Null))))
        }
        "tools/call" => {
            if let Some(response) =
                initialization_guard_response(id.clone(), *initialize_seen, *initialized)
            {
                return Ok(Some(response));
            }
            let Some(request_id) = id.clone() else {
                return Ok(None);
            };
            let Some(name) = params
                .get("name")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
            else {
                return Ok(Some(error_response(
                    request_id,
                    -32602,
                    "tools/call requires a string 'name'".to_string(),
                    None,
                )));
            };
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            Ok(Some(tool_call_response(
                request_id,
                handler
                    .dispatch(&name, normalize_tool_arguments(arguments))
                    .await,
            )))
        }
        method if method.starts_with("notifications/") => Ok(None),
        _ => Ok((!is_notification).then(|| {
            error_response(
                id.unwrap_or(Value::Null),
                -32601,
                format!("unknown method '{}'", method),
                None,
            )
        })),
    }
}

#[derive(Clone)]
struct HttpTransportState<B: AgentBackend> {
    handler: Arc<AgentToolHandler<B>>,
    allowed_origins: Arc<Vec<String>>,
}

async fn run_http_server<B: AgentBackend>(
    config: TransportConfig,
    backend: Arc<B>,
) -> Result<(), AgentError>
where
    B: 'static,
{
    let bind_addr = config.bind_addr.clone();
    let state = Arc::new(HttpTransportState {
        handler: Arc::new(AgentToolHandler::new(backend)),
        allowed_origins: Arc::new(config.allowed_origins),
    });

    let app = http_router(state);

    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .map_err(|err| {
            AgentError::Transport(format!(
                "failed to bind MCP HTTP listener on {bind_addr}: {err}"
            ))
        })?;
    serve_http_listener(listener, app).await
}

fn http_router<B: AgentBackend + 'static>(state: Arc<HttpTransportState<B>>) -> Router {
    Router::new()
        .route(
            "/mcp",
            post(http_post_handler::<B>)
                .get(http_get_handler::<B>)
                .delete(http_delete_handler::<B>),
        )
        .route("/", post(http_post_handler::<B>))
        .with_state(state)
}

async fn serve_http_listener(
    listener: tokio::net::TcpListener,
    app: Router,
) -> Result<(), AgentError> {
    axum::serve(listener, app)
        .await
        .map_err(|err| AgentError::Transport(format!("MCP HTTP server failed: {err}")))
}

async fn http_post_handler<B: AgentBackend>(
    State(state): State<Arc<HttpTransportState<B>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = validate_http_headers(&headers, &state.allowed_origins) {
        return response;
    }

    let request: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(err) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                error_response(
                    Value::Null,
                    -32700,
                    format!("invalid JSON-RPC payload: {err}"),
                    None,
                ),
            );
        }
    };

    match process_http_jsonrpc(state.handler.as_ref(), request).await {
        Ok(HttpOutcome::Json(value)) => json_response(StatusCode::OK, value),
        Ok(HttpOutcome::Accepted) => empty_response(StatusCode::ACCEPTED),
        Err(err) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            error_response(Value::Null, -32000, err.to_string(), None),
        ),
    }
}

async fn http_get_handler<B: AgentBackend>(
    State(_state): State<Arc<HttpTransportState<B>>>,
) -> Response {
    method_not_allowed_response("POST")
}

async fn http_delete_handler<B: AgentBackend>(
    State(_state): State<Arc<HttpTransportState<B>>>,
) -> Response {
    method_not_allowed_response("POST")
}

enum HttpOutcome {
    Json(Value),
    Accepted,
}

async fn process_http_jsonrpc<B: AgentBackend>(
    handler: &AgentToolHandler<B>,
    request: Value,
) -> Result<HttpOutcome, AgentError> {
    let id = request.get("id").cloned();
    let method = request
        .get("method")
        .and_then(|value| value.as_str())
        .map(str::to_owned);
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let is_notification = id.is_none();

    let Some(method) = method else {
        if request.get("result").is_some() || request.get("error").is_some() {
            return Ok(HttpOutcome::Accepted);
        }
        if is_notification {
            return Ok(HttpOutcome::Accepted);
        }
        return Ok(HttpOutcome::Json(error_response(
            id.unwrap_or(Value::Null),
            -32600,
            "missing method".to_string(),
            None,
        )));
    };

    match method.as_str() {
        "initialize" => Ok(HttpOutcome::Json(initialize_response(
            id.unwrap_or(Value::Null),
        ))),
        "notifications/initialized" => Ok(HttpOutcome::Accepted),
        "ping" => {
            if is_notification {
                Ok(HttpOutcome::Accepted)
            } else {
                Ok(HttpOutcome::Json(json!({
                    "jsonrpc": "2.0",
                    "id": id.unwrap_or(Value::Null),
                    "result": {}
                })))
            }
        }
        "tools/list" => {
            if is_notification {
                Ok(HttpOutcome::Accepted)
            } else {
                Ok(HttpOutcome::Json(tools_list_response(
                    id.unwrap_or(Value::Null),
                )))
            }
        }
        "tools/call" => {
            if is_notification {
                return Ok(HttpOutcome::Accepted);
            }
            let request_id = id.unwrap_or(Value::Null);
            let Some(name) = params
                .get("name")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
            else {
                return Ok(HttpOutcome::Json(error_response(
                    request_id,
                    -32602,
                    "tools/call requires a string 'name'".to_string(),
                    None,
                )));
            };
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            Ok(HttpOutcome::Json(tool_call_response(
                request_id,
                handler
                    .dispatch(&name, normalize_tool_arguments(arguments))
                    .await,
            )))
        }
        method if method.starts_with("notifications/") => Ok(HttpOutcome::Accepted),
        _ => {
            if is_notification {
                Ok(HttpOutcome::Accepted)
            } else {
                Ok(HttpOutcome::Json(error_response(
                    id.unwrap_or(Value::Null),
                    -32601,
                    format!("unknown method '{}'", method),
                    None,
                )))
            }
        }
    }
}

fn validate_http_headers(headers: &HeaderMap, allowed_origins: &[String]) -> Result<(), Response> {
    if let Some(origin) = headers.get(ORIGIN) {
        let Ok(origin_str) = origin.to_str() else {
            return Err(plain_response(
                StatusCode::FORBIDDEN,
                "invalid Origin header",
            ));
        };
        if !origin_is_allowed(origin_str, allowed_origins) {
            return Err(plain_response(StatusCode::FORBIDDEN, "Origin not allowed"));
        }
    }

    if let Some(version) = headers.get(HeaderName::from_static(MCP_PROTOCOL_VERSION_HEADER)) {
        match version.to_str() {
            Ok(value) if value == PROTOCOL_VERSION => {}
            Ok(_) => {
                return Err(plain_response(
                    StatusCode::BAD_REQUEST,
                    "unsupported MCP protocol version",
                ));
            }
            Err(_) => {
                return Err(plain_response(
                    StatusCode::BAD_REQUEST,
                    "invalid MCP protocol version header",
                ));
            }
        }
    }

    Ok(())
}

fn origin_is_allowed(origin: &str, allowed_origins: &[String]) -> bool {
    if !allowed_origins.is_empty() {
        return allowed_origins.iter().any(|allowed| allowed == origin);
    }

    let Ok(url) = reqwest::Url::parse(origin) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    if host == "localhost" {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

fn write_message(
    writer: &mut BufWriter<std::io::StdoutLock<'_>>,
    message: &Value,
) -> Result<(), AgentError> {
    let encoded = serde_json::to_string(message)?;
    writer
        .write_all(encoded.as_bytes())
        .and_then(|_| writer.write_all(b"\n"))
        .and_then(|_| writer.flush())
        .map_err(|err| AgentError::Transport(format!("failed to write stdout: {err}")))
}

fn initialize_response(id: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {
                "tools": {
                    "listChanged": false
                }
            },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": env!("CARGO_PKG_VERSION")
            },
            "instructions": "Use plan/approve/apply/verify workflows for mutations. Execution tools are only available when compiled in."
        }
    })
}

fn initialization_guard_response(
    id: Option<Value>,
    initialize_seen: bool,
    initialized: bool,
) -> Option<Value> {
    let request_id = id?;
    if !initialize_seen {
        return Some(error_response(
            request_id,
            -32002,
            "server has not completed initialize negotiation".to_string(),
            None,
        ));
    }
    if !initialized {
        return Some(error_response(
            request_id,
            -32002,
            "client must send notifications/initialized before normal operations".to_string(),
            None,
        ));
    }
    None
}

fn error_response(id: Value, code: i64, message: String, data: Option<Value>) -> Value {
    let mut error = json!({
        "code": code,
        "message": message,
    });
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": error
    })
}

fn normalize_tool_arguments(arguments: Value) -> Value {
    if arguments.is_object() {
        arguments
    } else {
        json!({})
    }
}

fn tool_result(result: Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string());
    json!({
        "content": [
            {
                "type": "text",
                "text": text
            }
        ],
        "structuredContent": result,
        "isError": is_error
    })
}

fn tools_list_response(id: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": available_tool_catalog()
                .into_iter()
                .map(|tool| json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": tool.input_schema,
                }))
                .collect::<Vec<_>>()
        }
    })
}

fn tool_call_response(id: Value, result: Result<Value, AgentError>) -> Value {
    match result {
        Ok(result) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": tool_result(result, false)
        }),
        Err(err) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": tool_result(json!({
                "error": err.to_string(),
            }), true)
        }),
    }
}

fn json_response(status: StatusCode, payload: Value) -> Response {
    let mut response = (status, Json(payload)).into_response();
    response.headers_mut().insert(
        HeaderName::from_static(MCP_PROTOCOL_VERSION_HEADER),
        HeaderValue::from_static(PROTOCOL_VERSION),
    );
    response
}

fn empty_response(status: StatusCode) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response.headers_mut().insert(
        HeaderName::from_static(MCP_PROTOCOL_VERSION_HEADER),
        HeaderValue::from_static(PROTOCOL_VERSION),
    );
    response
}

fn plain_response(status: StatusCode, message: &str) -> Response {
    let mut response = Response::new(Body::from(message.to_string()));
    *response.status_mut() = status;
    response
}

fn method_not_allowed_response(allow: &str) -> Response {
    let mut response = plain_response(
        StatusCode::METHOD_NOT_ALLOWED,
        "This MCP endpoint accepts POST only in the current implementation.",
    );
    response.headers_mut().insert(
        ALLOW,
        HeaderValue::from_str(allow).unwrap_or(HeaderValue::from_static("POST")),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_core::backend::{AgentBackend, AgentFuture};
    use std::io::ErrorKind;
    use tokio::time::{Duration, sleep};

    #[derive(Clone, Default)]
    struct DummyBackend;

    impl AgentBackend for DummyBackend {
        fn capabilities(&self) -> AgentFuture<'_, Value> {
            Box::pin(async { Ok(json!({"tool":"capabilities"})) })
        }

        fn context(&self) -> AgentFuture<'_, Value> {
            Box::pin(async { Ok(json!({"tool":"context"})) })
        }

        fn investigate(
            &self,
            _request: crate::agent_core::types::InvestigationRequest,
        ) -> AgentFuture<'_, Value> {
            Box::pin(async { Ok(json!({"tool":"investigate"})) })
        }

        fn plan(&self, _request: crate::agent_core::types::PlanRequest) -> AgentFuture<'_, Value> {
            Box::pin(async { Ok(json!({"tool":"plan"})) })
        }

        fn validate(
            &self,
            _request: crate::agent_core::types::PlanRequest,
        ) -> AgentFuture<'_, Value> {
            Box::pin(async { Ok(json!({"tool":"validate"})) })
        }

        fn graph_diff(
            &self,
            _request: crate::agent_core::types::PlanRequest,
        ) -> AgentFuture<'_, Value> {
            Box::pin(async { Ok(json!({"tool":"graph"})) })
        }

        fn create_operation(
            &self,
            _request: crate::agent_core::types::OperationCreateRequest,
        ) -> AgentFuture<'_, Value> {
            Box::pin(async { Ok(json!({"tool":"create"})) })
        }

        fn get_operation(&self, _operation_id: &str) -> AgentFuture<'_, Value> {
            Box::pin(async { Ok(json!({"tool":"get"})) })
        }

        fn approve_operation(
            &self,
            _operation_id: &str,
            _request: crate::agent_core::types::ApprovalRequest,
        ) -> AgentFuture<'_, Value> {
            Box::pin(async { Ok(json!({"tool":"approve"})) })
        }

        fn apply_operation(&self, _operation_id: &str) -> AgentFuture<'_, Value> {
            Box::pin(async { Ok(json!({"tool":"apply"})) })
        }

        fn verify_operation(
            &self,
            _request: crate::agent_core::types::VerifyRequest,
        ) -> AgentFuture<'_, Value> {
            Box::pin(async { Ok(json!({"tool":"verify"})) })
        }
    }

    #[tokio::test]
    async fn http_initialize_returns_server_info() {
        let handler = AgentToolHandler::new(Arc::new(DummyBackend));
        let response = process_http_jsonrpc(
            &handler,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {}
            }),
        )
        .await
        .unwrap();

        let HttpOutcome::Json(payload) = response else {
            panic!("expected JSON response");
        };
        assert_eq!(payload["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(payload["result"]["serverInfo"]["name"], SERVER_NAME);
    }

    #[tokio::test]
    async fn http_tools_list_does_not_require_stateful_initialize() {
        let handler = AgentToolHandler::new(Arc::new(DummyBackend));
        let response = process_http_jsonrpc(
            &handler,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": {}
            }),
        )
        .await
        .unwrap();

        let HttpOutcome::Json(payload) = response else {
            panic!("expected JSON response");
        };
        assert!(payload["result"]["tools"].as_array().unwrap().len() >= 1);
    }

    #[tokio::test]
    async fn http_transport_accepts_live_loopback_requests_when_binding_is_allowed() {
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(error) if error.kind() == ErrorKind::PermissionDenied => {
                // Some restricted runners deny loopback binds entirely. Keep the
                // smoke test non-flaky there while still exercising real TCP in
                // CI and developer environments that allow it.
                return;
            }
            Err(error) => panic!("failed to bind loopback listener: {error}"),
        };
        let addr = listener.local_addr().expect("local addr");
        let app = http_router(Arc::new(HttpTransportState {
            handler: Arc::new(AgentToolHandler::new(Arc::new(DummyBackend))),
            allowed_origins: Arc::new(Vec::new()),
        }));
        let server = tokio::spawn(async move {
            serve_http_listener(listener, app)
                .await
                .expect("server should run");
        });

        sleep(Duration::from_millis(20)).await;
        let response = reqwest::Client::new()
            .post(format!("http://{addr}/mcp"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(
                serde_json::to_vec(&json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/list",
                    "params": {}
                }))
                .expect("request body"),
            )
            .send()
            .await
            .expect("request should succeed");
        assert_eq!(response.status(), StatusCode::OK);
        let payload: Value = serde_json::from_str(&response.text().await.expect("response text"))
            .expect("json body");
        assert!(payload["result"]["tools"].as_array().unwrap().len() >= 1);

        server.abort();
        let _ = server.await;
    }
}
