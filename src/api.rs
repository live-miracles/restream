//! Axum HTTP API — REST endpoints for the dashboard, pipeline/output CRUD,
//! health monitoring, diagnostics SSE, and embedded frontend asset serving.
//! Static assets are compiled into the binary via `rust-embed` and served with
//! disk-first fallback for development hot-reload.

use axum::extract::DefaultBodyLimit;
use axum::http::HeaderValue;
use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::{delete, get, patch, post, put},
};
use reqwest::Url;
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path as FsPath, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use sysinfo::{Disks, Networks, System};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::RwLock as TokioRwLock;
use tokio_util::sync::CancellationToken;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tracing::{error, warn};

use crate::alerts;
use crate::db;
use crate::diag;
use crate::events;
use crate::media::engine::MediaEngine;
use crate::media::hls::{HlsSegmentVariant, HlsStore};
use crate::media::mpegts::{TsSegmentView, remux_segment_view};
use crate::media::security::IngestSecurityService;
use crate::media::srt::{
    SRT_INGEST_GLOBAL_CONFIG_META_KEY, SrtIngestPolicyStore, load_global_srt_ingest_config,
    parse_pipeline_srt_ingest_policy, serialize_pipeline_srt_ingest_policy,
};
use crate::types::*;

/// Maximum byte lengths for user-supplied string fields stored in SQLite.
/// These prevent both memory exhaustion and bloated DB rows.
pub const MAX_NAME_LEN: usize = 256;
pub const MAX_URL_LEN: usize = 2048;
pub const MAX_ENCODING_LEN: usize = 512;
pub const MAX_STREAM_KEY_LEN: usize = 256;
pub const MAX_FFMPEG_ARGS_LEN: usize = 4096;
pub const MAX_PASSWORD_LEN: usize = 1024;

#[derive(Clone, Copy)]
struct EngineCpuSample {
    total_ticks: u64,
    restream_ticks: u64,
    external_ffmpeg_ticks: u64,
}

static ENGINE_CPU_SAMPLE: OnceLock<Mutex<Option<EngineCpuSample>>> = OnceLock::new();

/// Returns a 400 Bad Request response if `s` exceeds `max` bytes, else `None`.
fn check_field_len(field: &str, s: &str, max: usize) -> Option<axum::response::Response> {
    if s.len() > max {
        Some(
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("{} exceeds maximum length of {} bytes", field, max)
                })),
            )
                .into_response(),
        )
    } else {
        None
    }
}

#[derive(RustEmbed)]
#[folder = "public/"]
pub struct EmbeddedAssets;

const SESSION_COOKIE_NAME: &str = "session";
const SESSION_MAX_AGE_SECONDS: i64 = 30 * 24 * 60 * 60;
const PASSWORD_META_KEY: &str = "dashboardPasswordHash";
const INGEST_SECURITY_CONFIG_META_KEY: &str = "ingest_security_config";
const DEFAULT_INGEST_HOST: &str = "localhost";

// Hardcoded stream keys matching mediamtx.yml compatibility
const STREAM_KEYS: &[(&str, &str)] = &[
    ("key01_6c71124cde80358ca7c13081", "key01"),
    ("key02_fff2adcf55a26d31ae93464b", "key02"),
    ("key03_c8087d1adb6b3bdf8e806d8f", "key03"),
    ("key04_4a1fe99ef35b0d0768076be7", "key04"),
    ("key05_ea839930dce5e021c629751d", "key05"),
    ("key06_48355e726bdc24afb9d08214", "key06"),
    ("key07_19eb3db7cb3d3f0831335701", "key07"),
    ("key08_3d4c645db62dac4449bbcea5", "key08"),
    ("key09_dc3f631793cadc287a509bf8", "key09"),
    ("key10_5d0f9109044f0cfb15d73ff8", "key10"),
    ("key11_c714ec6d94055e4e0175c9fd", "key11"),
    ("key12_0920bf2ce11eb518726ba3f7", "key12"),
    ("key13_88408b620477bc316f692c31", "key13"),
    ("key14_22893f11de0be7f49813dd8c", "key14"),
    ("key15_c1499536bc52e16281345ee8", "key15"),
    ("key16_794d51b9d1af088c00c2b5c1", "key16"),
    ("key17_b36de7b3fcaec34947a29d27", "key17"),
    ("key18_b301a17694098473a6bd2513", "key18"),
    ("key19_522561d0ec2e70bc79dda155", "key19"),
    ("key20_f6b326ffccc2f5a22477f1f9", "key20"),
];

pub struct PortConfig {
    pub rtmp: u16,
    pub srt: u16,
}

pub struct AppState {
    pub db: SqlitePool,
    pub security: Arc<IngestSecurityService>,
    pub ingest_policy_store: Arc<SrtIngestPolicyStore>,
    pub sessions: Arc<TokioRwLock<HashSet<String>>>,
    pub engine: Arc<MediaEngine>,
    pub ports: PortConfig,
    /// Directory for recordings and file-ingest sources.
    /// Defaults to `"media"`. Override via `RESTREAM_MEDIA_DIR`.
    pub media_dir: String,
    pub alert_tracker: alerts::AlertTracker,
    /// Broadcast sender for the /api/v1/logs/stream SSE endpoint.
    pub log_broadcast: tokio::sync::broadcast::Sender<crate::logging::LogBroadcast>,
    #[cfg(feature = "agent-execution")]
    pub agent_execution: Arc<crate::agent_execution::AgentExecutionStore>,
}

impl AppState {
    pub async fn is_authenticated(&self, token: &str) -> bool {
        let token_hash = hash_session_token(token);
        let sessions = self.sessions.read().await;
        sessions.contains(&token_hash)
    }
}

fn get_session_token_from_headers(headers: &HeaderMap) -> Option<String> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    for cookie in cookie_header.split(';') {
        let mut parts = cookie.trim().splitn(2, '=');
        let name = parts.next()?;
        if name == SESSION_COOKIE_NAME {
            return parts.next().map(|s| s.to_string());
        }
    }
    None
}

async fn request_is_authenticated(state: &AppState, headers: &HeaderMap) -> bool {
    if let Some(token) = get_session_token_from_headers(headers) {
        state.is_authenticated(&token).await
    } else {
        false
    }
}

async fn require_authenticated(
    state: &AppState,
    headers: &HeaderMap,
) -> Option<axum::response::Response> {
    // Cleanup note: newer handlers should use this helper, and the broader API
    // should eventually move auth into an extractor or route middleware instead
    // of repeating per-handler cookie checks.
    if request_is_authenticated(state, headers).await {
        None
    } else {
        Some((StatusCode::UNAUTHORIZED, "Unauthorized").into_response())
    }
}

async fn require_hls_access(
    state: &AppState,
    headers: &HeaderMap,
    _uri: &axum::http::Uri,
) -> Option<axum::response::Response> {
    require_authenticated(state, headers).await
}

// Session cookie helper
fn make_session_cookie(token: &str, max_age: i64) -> String {
    format!(
        "{}={}; HttpOnly; Path=/; SameSite=Strict; Max-Age={}",
        SESSION_COOKIE_NAME, token, max_age
    )
}

fn clear_session_cookie() -> String {
    format!(
        "{}={}; HttpOnly; Path=/; SameSite=Strict; Max-Age=0",
        SESSION_COOKIE_NAME, ""
    )
}

async fn get_ingest_host(db_pool: &SqlitePool) -> Result<String, sqlx::Error> {
    Ok(db::get_ingest_host(db_pool)
        .await?
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| DEFAULT_INGEST_HOST.to_string()))
}

async fn refresh_srt_ingest_policy_store(state: &AppState) {
    let global = load_global_srt_ingest_config(&state.db).await;
    let pipelines = db::list_pipelines(&state.db).await.unwrap_or_default();
    state.ingest_policy_store.replace(global, &pipelines);
}

fn pipeline_response_json(
    pipeline: &Pipeline,
    effective_ingest_host: &str,
    rtmp_port: u16,
    srt_port: u16,
) -> serde_json::Value {
    serde_json::json!({
        "id": pipeline.id,
        "name": pipeline.name,
        "streamKey": pipeline.stream_key,
        "inputSource": pipeline.input_source,
        "encoding": pipeline.encoding,
        "srtIngestPolicy": parse_pipeline_srt_ingest_policy(
            pipeline.srt_ingest_policy.as_deref()
        ),
        "ingestUrls": {
            "rtmp": format!("rtmp://{}:{}/live/{}", effective_ingest_host, rtmp_port, pipeline.stream_key),
            "srt": format!("srt://{}:{}?streamid=publish:live/{}", effective_ingest_host, srt_port, pipeline.stream_key)
        }
    })
}

// Hex encoding helper
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

// One-way hash of a session token for safe DB storage.
// Cookie holds the raw token; DB holds only SHA-256(token).
// A DB dump cannot recover valid session tokens.
fn hash_session_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    to_hex(&digest)
}

// Scrypt password hashing matching TS crypto.scryptSync implementation
fn hash_password(password: &str) -> String {
    use rand::RngCore;
    use scrypt::Params;

    let mut salt_bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt_bytes);
    let salt = to_hex(&salt_bytes);

    let mut hash_bytes = [0u8; 32];
    let params = Params::new(14, 8, 1, 32).unwrap();
    scrypt::scrypt(
        password.as_bytes(),
        salt.as_bytes(),
        &params,
        &mut hash_bytes,
    )
    .unwrap();
    let hash = to_hex(&hash_bytes);
    format!("{}:{}", salt, hash)
}

fn verify_password(password: &str, stored: &str) -> bool {
    use scrypt::Params;

    let parts: Vec<&str> = stored.split(':').collect();
    if parts.len() != 2 {
        return false;
    }
    let salt = parts[0];
    let stored_hash = parts[1];

    let mut new_hash = [0u8; 32];
    let params = Params::new(14, 8, 1, 32).unwrap();
    if scrypt::scrypt(password.as_bytes(), salt.as_bytes(), &params, &mut new_hash).is_err() {
        return false;
    }
    let hex_hash = to_hex(&new_hash);
    hex_hash == stored_hash
}

// Initialize Authentication logic on startup
pub async fn initialize_auth(db_pool: &SqlitePool, sessions_set: &TokioRwLock<HashSet<String>>) {
    // Check password
    if let Ok(None) = db::get_meta(db_pool, PASSWORD_META_KEY).await {
        let admin_hash = hash_password("admin");
        let _ = db::set_meta(db_pool, PASSWORD_META_KEY, &admin_hash).await;
    }

    // Prune and list sessions
    let _ = db::prune_expired_sessions(db_pool, 30 * 24 * 60 * 60 * 1000).await;
    if let Ok(active_sessions) = db::list_sessions(db_pool).await {
        let mut guard = sessions_set.write().await;
        for token in active_sessions {
            guard.insert(token);
        }
    }
}

// Web routing setup
pub fn create_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/login", get(login_get_handler))
        .route("/login.html", get(login_html_redirect_handler))
        .route("/settings.html", get(settings_html_redirect_handler))
        .route("/status.html", get(status_html_redirect_handler))
        .route("/logo.png", get(logo_handler))
        .route("/output.css", get(css_handler))
        .route("/api/v1/auth/login", post(login_post_handler))
        .route("/api/v1/auth/logout", post(logout_handler))
        .route(
            "/api/v1/auth/change-password",
            post(change_password_handler),
        )
        .route("/api/v1/audio-caps", get(audio_caps_handler))
        .route(
            "/api/v1/settings",
            get(config_get_handler).patch(config_patch_handler),
        )
        .route("/api/v1/stream-keys", get(stream_keys_handler))
        .route(
            "/api/v1/monitoring/youtube-status",
            get(youtube_monitoring_status_handler),
        )
        .route(
            "/api/v1/pipelines",
            get(pipelines_get_handler).post(pipelines_post_handler),
        )
        .route(
            "/api/v1/pipelines/:id",
            get(pipeline_detail_handler)
                .patch(pipelines_update_handler)
                .delete(pipelines_delete_handler),
        )
        .route(
            "/api/v1/pipelines/:pipeline_id/file-ingest",
            get(pipeline_file_ingest_get_handler)
                .put(pipeline_file_ingest_put_handler)
                .delete(pipeline_file_ingest_delete_handler),
        )
        .route(
            "/api/v1/pipelines/:pipeline_id/outputs",
            post(outputs_create_handler),
        )
        .route(
            "/api/v1/pipelines/:pipeline_id/outputs/:output_id",
            patch(outputs_update_handler).delete(outputs_delete_handler),
        )
        .route(
            "/api/v1/pipelines/:pipeline_id/outputs/:output_id/start",
            post(outputs_start_handler),
        )
        .route(
            "/api/v1/pipelines/:pipeline_id/outputs/:output_id/stop",
            post(outputs_stop_handler),
        )
        .route(
            "/api/v1/pipelines/:pipeline_id/outputs/:output_id/status",
            get(output_status_handler),
        )
        .route(
            "/api/v1/pipelines/:pipeline_id/probe",
            get(pipeline_probe_handler),
        )
        .route(
            "/api/v1/pipelines/:pipeline_id/graph",
            get(pipeline_graph_handler),
        )
        .route(
            "/api/v1/pipelines/:pipeline_id/alerts",
            get(pipeline_alerts_handler),
        )
        .route("/api/v1/logs", get(logs_handler))
        .route("/api/v1/logs/stream", get(logs_stream_handler))
        .route("/api/v1/alerts", get(aggregate_alerts_handler))
        .route("/api/v1/events", get(v1_events_handler))
        .route("/api/v1/overview", get(v1_overview_handler))
        .route("/api/v1/engine/telemetry", get(v1_engine_telemetry_handler))
        .route(
            "/api/v1/agent/capabilities",
            get(agent_capabilities_handler),
        )
        .route("/api/v1/agent/context", get(agent_context_handler))
        .route(
            "/api/v1/agent/investigations",
            post(agent_investigation_handler),
        )
        .route("/api/v1/agent/plans", post(agent_plan_handler))
        .route(
            "/api/v1/agent/plans/validate",
            post(agent_plan_validate_handler),
        )
        .route(
            "/api/v1/agent/graph-diff-preview",
            post(agent_graph_diff_preview_handler),
        )
        .route(
            "/api/v1/agent/operations",
            post(agent_operation_create_handler),
        )
        .route(
            "/api/v1/agent/operations/:operation_id",
            get(agent_operation_get_handler),
        )
        .route(
            "/api/v1/agent/operations/:operation_id/approve",
            post(agent_operation_approve_handler),
        )
        .route(
            "/api/v1/agent/operations/:operation_id/apply",
            post(agent_operation_apply_handler),
        )
        .route(
            "/api/v1/agent/operations/:operation_id/verify",
            post(agent_operation_verify_handler),
        )
        .route("/api/v1/agent/verify", post(agent_verify_handler))
        .route(
            "/api/v1/pipelines/:pipeline_id/telemetry",
            get(v1_pipeline_telemetry_handler),
        )
        .route(
            "/api/v1/stages/:stage_key/telemetry",
            get(v1_stage_telemetry_handler),
        )
        .route(
            "/api/v1/pipelines/:pipeline_id/summary",
            get(v1_pipeline_summary_handler),
        )
        .route(
            "/api/v1/pipelines/:pipeline_id/diagnostics",
            get(pipeline_diagnostics_sse_handler),
        )
        .route(
            "/api/v1/pipelines/:pipeline_id/recording/start",
            post(recording_start_handler),
        )
        .route(
            "/api/v1/pipelines/:pipeline_id/recording/stop",
            post(recording_stop_handler),
        )
        .route(
            "/api/v1/encodings/custom",
            get(custom_encoding_get).put(custom_encoding_put),
        )
        .route(
            "/api/v1/ingests",
            get(ingests_get_handler).post(ingests_post_handler),
        )
        .route(
            "/api/v1/ingests/:id",
            put(ingests_update_handler).delete(ingests_delete_handler),
        )
        .route("/api/v1/ingests/:id/start", post(ingests_start_handler))
        .route("/api/v1/ingests/:id/stop", post(ingests_stop_handler))
        .route("/api/v1/engine", get(status_get_handler))
        .route("/api/v1/engine/sbom", get(status_sbom_get_handler))
        .route("/api/v1/engine/health", get(v1_engine_health_handler))
        .route("/api/v1/media", get(media_list_handler))
        .route("/api/v1/media/:filename", delete(media_delete_handler))
        .route("/media/:filename", get(media_file_handler))
        // HLS routes are registered with CORS headers in the merged sub-router below.
        .route("/healthz", get(healthz_get_handler))
        .route("/metrics/system", get(metrics_system_handler))
        .fallback(get(spa_fallback_handler))
        .layer(CompressionLayer::new())
        .layer(DefaultBodyLimit::max(4 * 1024 * 1024)) // 4 MB global cap
        // Security headers: applied to all responses from this router.
        // X-Content-Type-Options prevents MIME-sniffing attacks where a
        // browser executes a response as a different type than declared.
        // X-Frame-Options blocks the dashboard from being embedded in a
        // cross-origin <iframe> (clickjacking).
        .layer(SetResponseHeaderLayer::if_not_present(
            header::HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::HeaderName::from_static("x-frame-options"),
            HeaderValue::from_static("SAMEORIGIN"),
        ))
        // HLS sub-router: allow any origin so browser-based players (hls.js,
        // Video.js) on a different origin can fetch playlists and segments.
        // Merged last so the CORS layer only applies to /hls/.
        .merge(
            Router::new()
                .route("/hls/:pipeline_id", get(hls_playlist_handler))
                .route("/hls/:pipeline_id/master.m3u8", get(hls_master_handler))
                .route("/hls/:pipeline_id/index.m3u8", get(hls_playlist_handler))
                .route(
                    "/hls/:pipeline_id/video/index.m3u8",
                    get(hls_video_playlist_handler),
                )
                .route(
                    "/hls/:pipeline_id/video/:segment",
                    get(hls_video_segment_handler),
                )
                .route(
                    "/hls/:pipeline_id/audio/:track_index/index.m3u8",
                    get(hls_audio_playlist_handler),
                )
                .route(
                    "/hls/:pipeline_id/audio/:track_index/:segment",
                    get(hls_audio_segment_handler),
                )
                .route("/hls/:pipeline_id/:segment", get(hls_segment_handler))
                .layer(
                    CorsLayer::new()
                        .allow_origin(AllowOrigin::any())
                        .allow_methods([axum::http::Method::GET, axum::http::Method::OPTIONS])
                        .allow_headers([header::CONTENT_TYPE, header::RANGE]),
                )
                .with_state(state.clone()),
        )
        .with_state(state)
}

// Handler implementations

fn serve_embedded(path: &str) -> impl IntoResponse {
    let content_type = match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css",
        Some("js") => "application/javascript",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    };

    // Disk-first fallback is only active in debug builds (dev hot-reload).
    // In production the binary's embedded assets are always used, preventing
    // any filesystem access from this path.
    //
    // Path traversal guard: reject any path component that is ".." or starts
    // with "/" to prevent `public/../../../etc/passwd` style reads even in dev.
    #[cfg(debug_assertions)]
    {
        // Use canonicalize to resolve symlinks and ".." before the prefix
        // check — this is the only traversal guard that is actually correct.
        let public_root = match std::fs::canonicalize("public") {
            Ok(p) => p,
            Err(_) => std::path::PathBuf::new(),
        };
        if let Ok(candidate) = std::fs::canonicalize(format!("public/{}", path))
            && candidate.starts_with(&public_root)
            && let Ok(data) = std::fs::read(&candidate)
        {
            return (StatusCode::OK, [(header::CONTENT_TYPE, content_type)], data).into_response();
        }
    }

    match EmbeddedAssets::get(path) {
        Some(file) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, content_type)],
            file.data.to_vec(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn login_get_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers)
        && state.is_authenticated(&token).await
    {
        return Redirect::to("/").into_response();
    }
    serve_embedded("login.html").into_response()
}

async fn login_html_redirect_handler() -> impl IntoResponse {
    Redirect::to("/login")
}

async fn settings_html_redirect_handler() -> impl IntoResponse {
    Redirect::to("/?mode=settings")
}

async fn status_html_redirect_handler() -> impl IntoResponse {
    Redirect::to("/?mode=status")
}

async fn logo_handler() -> impl IntoResponse {
    serve_embedded("logo.png")
}

async fn css_handler() -> impl IntoResponse {
    serve_embedded("output.css")
}

async fn spa_fallback_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    if !path.is_empty() && path.contains('.') {
        if path.ends_with(".html") && !request_is_authenticated(&state, &headers).await {
            return Redirect::to("/login").into_response();
        }
        return serve_embedded(path).into_response();
    }
    if !request_is_authenticated(&state, &headers).await {
        return Redirect::to("/login").into_response();
    }
    serve_embedded("index.html").into_response()
}

#[derive(Deserialize)]
struct LoginPayload {
    password: Option<String>,
}

async fn login_post_handler(
    State(state): State<Arc<AppState>>,
    connect_info: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>,
    Json(payload): Json<LoginPayload>,
) -> impl IntoResponse {
    // In production the server runs with into_make_service_with_connect_info,
    // so connect_info is always Some. In unit tests it may be None; fall back
    // to loopback which the security service never bans.
    let client_ip = connect_info
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    // Brute-force protection: reject IPs that have exceeded the failure threshold.
    // Reuses the same IngestSecurityService as RTMP/SRT ingest.
    if let Some(ban_remaining) = state.security.is_ip_banned(&client_ip) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({
                "error": format!("Too many failed attempts. Try again in {} seconds.",
                                 ban_remaining.as_secs())
            })),
        )
            .into_response();
    }

    let password = payload.password.unwrap_or_default();
    if let Some(r) = check_field_len("password", &password, MAX_PASSWORD_LEN) {
        return r;
    }
    let stored_hash = match db::get_meta(&state.db, PASSWORD_META_KEY).await {
        Ok(Some(hash)) => hash,
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Incorrect password"})),
            )
                .into_response();
        }
    };

    // scrypt is CPU-bound (~120 ms at N=2^14). Calling it on the async
    // executor thread would block all other tasks during that window,
    // enabling a trivial DoS via concurrent login requests. spawn_blocking
    // offloads the work to a dedicated thread pool.
    let verified = tokio::task::spawn_blocking(move || verify_password(&password, &stored_hash))
        .await
        .unwrap_or(false);

    if !verified {
        state.security.record_failure(&client_ip);
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Incorrect password"})),
        )
            .into_response();
    }
    state.security.record_success(&client_ip);

    // Create session — cookie carries the raw token; DB and in-memory set
    // store only SHA-256(token) so a DB dump cannot forge sessions.
    use rand::RngCore;
    let mut token_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut token_bytes);
    let token = to_hex(&token_bytes);
    let token_hash = hash_session_token(&token);

    let ts = chrono::Utc::now().timestamp_millis();
    if db::create_session(&state.db, &token_hash, ts)
        .await
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create session",
        )
            .into_response();
    }

    state.sessions.write().await.insert(token_hash);

    let cookie = make_session_cookie(&token, SESSION_MAX_AGE_SECONDS);
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({"ok": true})),
    )
        .into_response()
}

async fn logout_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        let token_hash = hash_session_token(&token);
        state.sessions.write().await.remove(&token_hash);
        if let Err(e) = db::delete_session(&state.db, &token_hash).await {
            warn!(err = %e, "failed to delete session from DB");
        }
    }
    let cookie = clear_session_cookie();
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({"ok": true})),
    )
}

#[derive(Deserialize)]
struct ChangePasswordPayload {
    current_password: Option<String>,
    new_password: Option<String>,
}

async fn change_password_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<ChangePasswordPayload>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let current_password = payload.current_password.unwrap_or_default();
    let new_password = payload.new_password.unwrap_or_default();
    if let Some(r) = check_field_len("current_password", &current_password, MAX_PASSWORD_LEN) {
        return r;
    }
    if let Some(r) = check_field_len("new_password", &new_password, MAX_PASSWORD_LEN) {
        return r;
    }

    if new_password.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "New password cannot be empty"})),
        )
            .into_response();
    }

    let stored_hash = match db::get_meta(&state.db, PASSWORD_META_KEY).await {
        Ok(Some(hash)) => hash,
        _ => {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "Current password is incorrect"})),
            )
                .into_response();
        }
    };

    // Offload scrypt verification to blocking thread pool (same rationale as login handler).
    let verified =
        tokio::task::spawn_blocking(move || verify_password(&current_password, &stored_hash))
            .await
            .unwrap_or(false);

    if !verified {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Current password is incorrect"})),
        )
            .into_response();
    }

    let new_hash = tokio::task::spawn_blocking(move || hash_password(&new_password))
        .await
        .unwrap_or_default();
    if new_hash.is_empty() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to hash new password",
        )
            .into_response();
    }
    if db::set_meta(&state.db, PASSWORD_META_KEY, &new_hash)
        .await
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to update password",
        )
            .into_response();
    }

    Json(serde_json::json!({"ok": true})).into_response()
}

async fn audio_caps_handler() -> impl IntoResponse {
    // Exact audio caps matching audio-caps.ts structure
    Json(serde_json::json!({
        "caps": {
            "facebook:hls": {"codecs": ["aac"], "maxChannels": 2, "maxTracks": 1},
            "facebook:rtmp": {"codecs": ["aac"], "maxChannels": 2, "maxTracks": 1},
            "facebook:rtmps": {"codecs": ["aac"], "maxChannels": 2, "maxTracks": 1},
            "facebook:srt": {"codecs": ["aac"], "maxChannels": 2, "maxTracks": 1},
            "generic:hls": {"codecs": ["aac", "ac3", "eac3"], "maxChannels": null, "maxTracks": null},
            "generic:rtmp": {"codecs": ["aac", "mp3"], "maxChannels": 6, "maxTracks": 1},
            "generic:rtmps": {"codecs": ["aac", "mp3"], "maxChannels": 6, "maxTracks": 1},
            "generic:srt": {"codecs": "any", "maxChannels": null, "maxTracks": null},
            "vdocipher:hls": {"codecs": ["aac"], "maxChannels": 2, "maxTracks": 1},
            "vdocipher:rtmp": {"codecs": ["aac"], "maxChannels": 2, "maxTracks": 1},
            "vdocipher:rtmps": {"codecs": ["aac"], "maxChannels": 2, "maxTracks": 1},
            "vdocipher:srt": {"codecs": ["aac"], "maxChannels": 2, "maxTracks": 1},
            "youtube:hls": {"codecs": ["aac", "ac3", "eac3"], "maxChannels": 6, "maxTracks": 1},
            "youtube:rtmp": {"codecs": ["aac", "mp3"], "maxChannels": 2, "maxTracks": 1},
            "youtube:rtmps": {"codecs": ["aac", "mp3"], "maxChannels": 2, "maxTracks": 1},
            "youtube:srt": {"codecs": ["aac", "mp3"], "maxChannels": 2, "maxTracks": 1}
        },
        "platformLabels": {
            "facebook": "Facebook Live",
            "generic": "Generic",
            "vdocipher": "VdoCipher",
            "youtube": "YouTube"
        }
    }))
}

async fn stream_keys_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let host = match get_ingest_host(&state.db).await {
        Ok(host) => host,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let mut keys = Vec::new();
    for &(key, label) in STREAM_KEYS {
        keys.push(serde_json::json!({
            "key": key,
            "label": label,
            "ingestUrls": {
                "rtmp": format!("rtmp://{}:{}/live/{}", host, state.ports.rtmp, key),
                "srt": format!("srt://{}:{}?streamid=publish:live/{}", host, state.ports.srt, key)
            }
        }));
    }
    Json(keys).into_response()
}

async fn config_get_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let ingest_host = match db::get_ingest_host(&state.db).await {
        Ok(host) => host.unwrap_or_default(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let effective_ingest_host = if ingest_host.is_empty() {
        DEFAULT_INGEST_HOST
    } else {
        &ingest_host
    };
    let raw_pipelines = match db::list_pipelines(&state.db).await {
        Ok(p) => p,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let pipelines = raw_pipelines
        .iter()
        .map(|pipeline| {
            pipeline_response_json(
                pipeline,
                effective_ingest_host,
                state.ports.rtmp,
                state.ports.srt,
            )
        })
        .collect::<Vec<_>>();

    let outputs = db::list_outputs(&state.db).await.unwrap_or_default();
    let jobs = db::list_jobs(&state.db).await.unwrap_or_default();
    let server_name = db::get_meta(&state.db, "server_name")
        .await
        .unwrap_or(Some("Name".to_string()))
        .unwrap_or("Name".to_string());
    let sec = state.security.get_config();
    let srt_ingest = load_global_srt_ingest_config(&state.db).await;

    // Transcode profiles from runtime cache, with built-ins exposed when unset.
    let transcode_profiles = crate::media::profiles::current_effective().await;

    Json(serde_json::json!({
        "serverName": server_name,
        "ingestHost": ingest_host,
        "ingestSecurity": sec,
        "srtIngest": srt_ingest,
        "transcodeProfiles": transcode_profiles,
        "pipelines": pipelines,
        "outputs": outputs,
        "jobs": jobs
    }))
    .into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigPatchPayload {
    server_name: Option<String>,
    ingest_host: Option<String>,
    ingest_security: Option<IngestSecurityConfig>,
    srt_ingest: Option<SrtGlobalIngestConfig>,
    transcode_profiles: Option<crate::media::profiles::TranscodeProfiles>,
}

async fn config_patch_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<ConfigPatchPayload>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    if let Some(ref name) = payload.server_name {
        if name.trim().is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                "serverName must be a non-empty string",
            )
                .into_response();
        }
        let _ = db::set_meta(&state.db, "server_name", name).await;
    }

    if let Some(ref host) = payload.ingest_host
        && db::set_ingest_host(&state.db, host).await.is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    if let Some(ref sec) = payload.ingest_security {
        state.security.update_config(sec.clone());
        if let Ok(raw_json) = serde_json::to_string(sec) {
            let _ = db::set_meta(&state.db, INGEST_SECURITY_CONFIG_META_KEY, &raw_json).await;
        }
    }

    if let Some(mut srt_ingest) = payload.srt_ingest.clone() {
        if let Err(error) = srt_ingest.validate() {
            return (StatusCode::BAD_REQUEST, error).into_response();
        }
        let raw_json = match serde_json::to_string(&srt_ingest) {
            Ok(value) => value,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        if db::set_meta(&state.db, SRT_INGEST_GLOBAL_CONFIG_META_KEY, &raw_json)
            .await
            .is_err()
        {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        refresh_srt_ingest_policy_store(&state).await;
    }

    if let Some(ref profiles) = payload.transcode_profiles {
        for (name, profile) in profiles {
            if let Err(err) = profile.validate() {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("Invalid profile '{}': {}", name, err),
                )
                    .into_response();
            }
        }
        if let Err(e) = crate::media::profiles::save_to_db(&state.db, profiles).await {
            warn!(err = %e, "failed to save transcode profiles");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to save profiles").into_response();
        }
    }

    let server_name = db::get_meta(&state.db, "server_name")
        .await
        .unwrap_or(Some("Name".to_string()))
        .unwrap_or("Name".to_string());
    let ingest_host = match db::get_ingest_host(&state.db).await {
        Ok(host) => host.unwrap_or_default(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let sec = state.security.get_config();
    let srt_ingest = load_global_srt_ingest_config(&state.db).await;
    let transcode_profiles = crate::media::profiles::current_effective().await;

    Json(serde_json::json!({
        "serverName": server_name,
        "ingestHost": ingest_host,
        "ingestSecurity": sec,
        "srtIngest": srt_ingest,
        "transcodeProfiles": transcode_profiles
    }))
    .into_response()
}

async fn pipelines_get_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    match db::list_pipelines(&state.db).await {
        Ok(pipelines) => {
            let ingest_host = get_ingest_host(&state.db)
                .await
                .unwrap_or_else(|_| DEFAULT_INGEST_HOST.to_string());
            let pipelines = pipelines
                .iter()
                .map(|pipeline| {
                    pipeline_response_json(
                        pipeline,
                        &ingest_host,
                        state.ports.rtmp,
                        state.ports.srt,
                    )
                })
                .collect::<Vec<_>>();
            Json(serde_json::json!({ "pipelines": pipelines })).into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn pipeline_detail_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }

    let pipeline = match db::get_pipeline(&state.db, &id).await {
        Ok(Some(pipeline)) => pipeline,
        Ok(None) => return (StatusCode::NOT_FOUND, "Pipeline not found").into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let outputs = match db::list_outputs_for_pipeline(&state.db, &id).await {
        Ok(outputs) => outputs,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let ingest_host = get_ingest_host(&state.db)
        .await
        .unwrap_or_else(|_| DEFAULT_INGEST_HOST.to_string());

    Json(serde_json::json!({
        "pipeline": pipeline_response_json(
            &pipeline,
            &ingest_host,
            state.ports.rtmp,
            state.ports.srt
        ),
        "outputs": outputs,
    }))
    .into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PipelinePayload {
    name: String,
    stream_key: Option<String>,
    input_source: Option<Option<String>>,
    encoding: Option<String>,
    srt_ingest_policy: Option<SrtPipelineIngestConfig>,
}

async fn pipelines_post_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<PipelinePayload>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    if let Some(r) = check_field_len("name", &payload.name, MAX_NAME_LEN) {
        return r;
    }
    if let Some(ref k) = payload.stream_key
        && let Some(r) = check_field_len("stream_key", k, MAX_STREAM_KEY_LEN)
    {
        return r;
    }
    if let Some(Some(ref source)) = payload.input_source
        && let Some(r) = check_field_len("input_source", source, MAX_URL_LEN)
    {
        return r;
    }
    if let Some(ref e) = payload.encoding
        && let Some(r) = check_field_len("encoding", e, MAX_ENCODING_LEN)
    {
        return r;
    }
    if let Some(mut policy) = payload.srt_ingest_policy.clone()
        && let Err(error) = policy.validate()
    {
        return (StatusCode::BAD_REQUEST, error).into_response();
    }
    if payload.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Pipeline name cannot be empty"})),
        )
            .into_response();
    }

    // Auto-select stream key if not provided
    let stream_key = if let Some(ref key) = payload.stream_key {
        key.clone()
    } else {
        // Choose first unused stream key
        let active_pipelines = db::list_pipelines(&state.db).await.unwrap_or_default();
        let used: HashSet<String> = active_pipelines.into_iter().map(|p| p.stream_key).collect();
        let found = STREAM_KEYS.iter().find(|&&(key, _)| !used.contains(key));
        match found {
            Some(&(key, _)) => key.to_string(),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "No available stream keys"})),
                )
                    .into_response();
            }
        }
    };

    if let Ok(active_pipelines) = db::list_pipelines(&state.db).await
        && active_pipelines.iter().any(|p| p.stream_key == stream_key)
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "A pipeline with this stream key already exists"})),
        )
            .into_response();
    }

    let id = format!("pipeline_{}", to_hex(&rand::random::<[u8; 8]>()));

    let input_source = payload
        .input_source
        .as_ref()
        .and_then(|source| source.as_deref());
    let srt_ingest_policy = match payload.srt_ingest_policy.as_ref() {
        Some(policy) => match serialize_pipeline_srt_ingest_policy(policy) {
            Ok(value) => Some(value),
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        None => None,
    };

    match db::create_pipeline(
        &state.db,
        &id,
        &payload.name,
        &stream_key,
        input_source,
        payload.encoding.as_deref(),
        srt_ingest_policy.as_deref(),
    )
    .await
    {
        Ok(pipeline) => {
            refresh_srt_ingest_policy_store(&state).await;
            let ingest_host = get_ingest_host(&state.db)
                .await
                .unwrap_or_else(|_| DEFAULT_INGEST_HOST.to_string());
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "message": "Pipeline created",
                    "pipeline": pipeline_response_json(
                        &pipeline,
                        &ingest_host,
                        state.ports.rtmp,
                        state.ports.srt
                    )
                })),
            )
                .into_response()
        }
        Err(err) => {
            if err.to_string().contains("duplicate stream key") {
                (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({"error": "A pipeline with this stream key already exists"})),
                )
                    .into_response()
            } else {
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}

async fn pipelines_update_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(payload): Json<PipelinePayload>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    if let Some(r) = check_field_len("name", &payload.name, MAX_NAME_LEN) {
        return r;
    }
    if let Some(ref k) = payload.stream_key
        && let Some(r) = check_field_len("stream_key", k, MAX_STREAM_KEY_LEN)
    {
        return r;
    }
    if let Some(Some(ref source)) = payload.input_source
        && let Some(r) = check_field_len("input_source", source, MAX_URL_LEN)
    {
        return r;
    }
    if let Some(ref e) = payload.encoding
        && let Some(r) = check_field_len("encoding", e, MAX_ENCODING_LEN)
    {
        return r;
    }
    if let Some(mut policy) = payload.srt_ingest_policy.clone()
        && let Err(error) = policy.validate()
    {
        return (StatusCode::BAD_REQUEST, error).into_response();
    }
    if payload.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Pipeline name cannot be empty"})),
        )
            .into_response();
    }

    let existing = match db::get_pipeline(&state.db, &id).await {
        Ok(Some(p)) => p,
        _ => return (StatusCode::NOT_FOUND, "Pipeline not found").into_response(),
    };

    let existing_stream_key = existing.stream_key.clone();
    let existing_input_source = existing.input_source.clone();
    let existing_encoding = existing.encoding.clone();
    let existing_srt_ingest_policy = existing.srt_ingest_policy.clone();

    let stream_key = payload.stream_key.unwrap_or(existing_stream_key);
    let input_source = payload.input_source.unwrap_or(existing_input_source);
    let encoding = payload.encoding.or(existing_encoding);
    let srt_ingest_policy = match payload
        .srt_ingest_policy
        .as_ref()
        .map(serialize_pipeline_srt_ingest_policy)
        .transpose()
    {
        Ok(Some(value)) => Some(value),
        Ok(None) => existing_srt_ingest_policy,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    if let Ok(active_pipelines) = db::list_pipelines(&state.db).await
        && active_pipelines
            .iter()
            .any(|p| p.id != id && p.stream_key == stream_key)
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "A pipeline with this stream key already exists"})),
        )
            .into_response();
    }

    match db::update_pipeline(
        &state.db,
        &id,
        &payload.name,
        &stream_key,
        input_source.as_deref(),
        encoding.as_deref(),
        srt_ingest_policy.as_deref(),
    )
    .await
    {
        Ok(Some(updated)) => {
            refresh_srt_ingest_policy_store(&state).await;
            let ingest_host = get_ingest_host(&state.db)
                .await
                .unwrap_or_else(|_| DEFAULT_INGEST_HOST.to_string());
            Json(serde_json::json!({
                "message": "Pipeline updated",
                "pipeline": pipeline_response_json(
                    &updated,
                    &ingest_host,
                    state.ports.rtmp,
                    state.ports.srt
                )
            }))
            .into_response()
        }
        Err(err) => {
            if err.to_string().contains("duplicate stream key") {
                (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({"error": "A pipeline with this stream key already exists"})),
                )
                    .into_response()
            } else {
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn pipelines_delete_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    // Cancel all running egresses for this pipeline's outputs before deleting
    if let Ok(outputs) = db::list_outputs(&state.db).await {
        for output in outputs.iter().filter(|o| o.pipeline_id == id) {
            state.engine.unregister_egress(&output.id).await;
        }
    }

    // Kill any file-ingest FFmpeg subprocesses that push into this pipeline's
    // stream key.  Without this, the subprocess keeps running and retrying
    // RTMP pushes even after the pipeline row is gone.
    if let Ok(Some(pipeline)) = db::get_pipeline(&state.db, &id).await
        && let Ok(ingests) = db::list_ingests(&state.db).await
    {
        for ingest in ingests
            .iter()
            .filter(|i| i.stream_key == pipeline.stream_key)
        {
            let _ = state.engine.stop_file_ingest_child(&ingest.id).await;
        }
    }

    // Remove the pipeline record first so any racing ingest reconnect
    // finds no pipeline to publish into (closes the TOCTOU window between
    // unregister_ingest and remove_pipeline where an orphaned ring buffer
    // could be registered).
    state.engine.remove_pipeline(&id).await;
    state.engine.unregister_ingest(&id).await;
    // Free the source ring buffer, all transcoder stage ring buffers, and the
    // HLS segmenter+store.  Without these calls the engine maps would retain
    // Arc<RingBuffer> references after the pipeline record is gone from the DB.
    state.engine.cleanup_pipeline_stages(&id).await;
    state.engine.shutdown_hls_segmenter(&id).await;

    match db::delete_pipeline(&state.db, &id).await {
        Ok(true) => {
            refresh_srt_ingest_policy_store(&state).await;
            Json(serde_json::json!({"message": format!("Pipeline {} deleted", id)})).into_response()
        }
        _ => (StatusCode::NOT_FOUND, "Pipeline not found").into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OutputPayload {
    name: String,
    url: String,
    encoding: String,
    monitoring_url: Option<String>,
}

fn is_supported_output_url(url: &str) -> bool {
    url.starts_with("rtmp://")
        || url.starts_with("rtmps://")
        || url.starts_with("srt://")
        || url.starts_with("hls://")
        || url.starts_with("http://")
        || url.starts_with("https://")
}

const OUTPUT_URL_SCHEME_ERROR: &str = "Invalid URL scheme. Supported schemes are rtmp://, rtmps://, srt://, hls://, http://, and https://";
const MONITORING_URL_SCHEME_ERROR: &str =
    "Invalid monitoring URL scheme. Supported schemes are http://, https://, and srt://";
const CUSTOM_OUTPUT_ENCODING_ERROR: &str =
    "Custom output encoding is not available yet; choose source or a preset encoding";

fn is_custom_output_encoding(encoding: &str) -> bool {
    encoding
        .split('+')
        .next()
        .map(|video| video.trim().eq_ignore_ascii_case("custom"))
        .unwrap_or(false)
}

fn normalize_monitoring_url(url: Option<&str>) -> Option<String> {
    let trimmed = url.unwrap_or_default().trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn is_supported_monitoring_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://") || url.starts_with("srt://")
}

#[derive(Deserialize)]
struct YoutubeMonitoringStatusQuery {
    url: String,
}

#[derive(Serialize)]
struct YoutubeMonitoringStatusResponse {
    canonical_watch_url: String,
    live_now: bool,
    live_content: bool,
    upcoming: bool,
    title: Option<String>,
}

fn normalize_youtube_watch_url(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let host = parsed
        .host_str()?
        .trim_start_matches("www.")
        .to_ascii_lowercase();
    let path_parts = parsed
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();
    let video_id = if host == "youtu.be" {
        path_parts
            .first()
            .copied()
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    } else if host.ends_with("youtube.com") {
        parsed
            .query_pairs()
            .find(|(key, _)| key == "v")
            .map(|(_, value)| value.into_owned())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                if matches!(
                    path_parts.first().copied(),
                    Some("live" | "embed" | "shorts")
                ) {
                    path_parts.get(1).map(|value| (*value).to_string())
                } else {
                    None
                }
            })
    } else {
        None
    }?;
    Some(format!("https://www.youtube.com/watch?v={video_id}"))
}

fn youtube_watch_page_contains_flag(html: &str, flag: &str) -> bool {
    html.contains(flag)
}

fn extract_html_title(html: &str) -> Option<String> {
    let start = html.find("<title>")?;
    let rest = &html[start + "<title>".len()..];
    let end = rest.find("</title>")?;
    let title = rest[..end].trim();
    (!title.is_empty()).then(|| {
        title
            .replace("&amp;", "&")
            .replace("&#39;", "'")
            .replace("&quot;", "\"")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
    })
}

fn parse_youtube_monitoring_status(
    canonical_watch_url: String,
    html: &str,
) -> YoutubeMonitoringStatusResponse {
    YoutubeMonitoringStatusResponse {
        canonical_watch_url,
        live_now: youtube_watch_page_contains_flag(html, "\"isLiveNow\":true"),
        live_content: youtube_watch_page_contains_flag(html, "\"isLiveContent\":true"),
        upcoming: youtube_watch_page_contains_flag(html, "\"isUpcoming\":true"),
        title: extract_html_title(html).map(|title| title.replace(" - YouTube", "")),
    }
}

async fn youtube_monitoring_status_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<YoutubeMonitoringStatusQuery>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    if let Some(response) = check_field_len("url", &query.url, MAX_URL_LEN) {
        return response;
    }
    let canonical_watch_url = match normalize_youtube_watch_url(query.url.trim()) {
        Some(url) => url,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Expected a YouTube monitoring URL"})),
            )
                .into_response();
        }
    };

    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36")
        .build();
    let Ok(client) = client else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let response = match client.get(&canonical_watch_url).send().await {
        Ok(response) => response,
        Err(_) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": "Failed to fetch YouTube metadata"})),
            )
                .into_response();
        }
    };
    let html = match response.text().await {
        Ok(html) => html,
        Err(_) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": "Failed to read YouTube metadata"})),
            )
                .into_response();
        }
    };
    Json(parse_youtube_monitoring_status(canonical_watch_url, &html)).into_response()
}

async fn outputs_create_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(pipeline_id): Path<String>,
    Json(payload): Json<OutputPayload>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    if let Some(r) = check_field_len("name", &payload.name, MAX_NAME_LEN) {
        return r;
    }
    if let Some(r) = check_field_len("url", &payload.url, MAX_URL_LEN) {
        return r;
    }
    if let Some(r) = check_field_len("encoding", &payload.encoding, MAX_ENCODING_LEN) {
        return r;
    }
    if let Some(monitoring_url) = payload.monitoring_url.as_deref()
        && let Some(r) = check_field_len("monitoring_url", monitoring_url, MAX_URL_LEN)
    {
        return r;
    }
    if is_custom_output_encoding(&payload.encoding) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": CUSTOM_OUTPUT_ENCODING_ERROR
            })),
        )
            .into_response();
    }
    let url = payload.url.trim();
    if !is_supported_output_url(url) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": OUTPUT_URL_SCHEME_ERROR
            })),
        )
            .into_response();
    }
    let monitoring_url = normalize_monitoring_url(payload.monitoring_url.as_deref());
    if let Some(ref url) = monitoring_url
        && !is_supported_monitoring_url(url)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": MONITORING_URL_SCHEME_ERROR
            })),
        )
            .into_response();
    }

    let id = format!("output_{}", to_hex(&rand::random::<[u8; 8]>()));

    match db::create_output(
        &state.db,
        &id,
        &pipeline_id,
        &payload.name,
        &payload.url,
        monitoring_url.as_deref(),
        "stopped",
        &payload.encoding,
    )
    .await
    {
        Ok(output) => (
            StatusCode::CREATED,
            Json(serde_json::json!({"message": "Output created", "output": output})),
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn outputs_update_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((pipeline_id, output_id)): Path<(String, String)>,
    Json(payload): Json<OutputPayload>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    if let Some(r) = check_field_len("name", &payload.name, MAX_NAME_LEN) {
        return r;
    }
    if let Some(r) = check_field_len("url", &payload.url, MAX_URL_LEN) {
        return r;
    }
    if let Some(r) = check_field_len("encoding", &payload.encoding, MAX_ENCODING_LEN) {
        return r;
    }
    if let Some(monitoring_url) = payload.monitoring_url.as_deref()
        && let Some(r) = check_field_len("monitoring_url", monitoring_url, MAX_URL_LEN)
    {
        return r;
    }
    if is_custom_output_encoding(&payload.encoding) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": CUSTOM_OUTPUT_ENCODING_ERROR
            })),
        )
            .into_response();
    }
    let url = payload.url.trim();
    if !is_supported_output_url(url) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": OUTPUT_URL_SCHEME_ERROR
            })),
        )
            .into_response();
    }
    let monitoring_url = normalize_monitoring_url(payload.monitoring_url.as_deref());
    if let Some(ref url) = monitoring_url
        && !is_supported_monitoring_url(url)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": MONITORING_URL_SCHEME_ERROR
            })),
        )
            .into_response();
    }
    let existing = match db::get_output(&state.db, &pipeline_id, &output_id).await {
        Ok(Some(output)) => output,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Output not found" })),
            )
                .into_response();
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if existing.desired_state == "running"
        && (existing.url != payload.url || existing.encoding != payload.encoding)
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "Cannot change output transport URL or encoding while the output is running"
            })),
        )
            .into_response();
    }

    match db::update_output(
        &state.db,
        &pipeline_id,
        &output_id,
        &payload.name,
        &payload.url,
        monitoring_url.as_deref(),
        &payload.encoding,
    )
    .await
    {
        Ok(Some(updated)) => {
            Json(serde_json::json!({"message": "Output updated", "output": updated}))
                .into_response()
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn outputs_delete_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((pipeline_id, output_id)): Path<(String, String)>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    state.engine.unregister_egress(&output_id).await;
    match db::delete_output(&state.db, &pipeline_id, &output_id).await {
        Ok(true) => Json(serde_json::json!({"message": "Output deleted"})).into_response(),
        _ => (StatusCode::NOT_FOUND, "Output not found").into_response(),
    }
}

async fn outputs_start_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((pipeline_id, output_id)): Path<(String, String)>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    match db::set_output_desired_state(&state.db, &pipeline_id, &output_id, "running").await {
        Ok(output) => Json(serde_json::json!({"message": "Output started", "desiredState": "running", "output": output})).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn outputs_stop_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((pipeline_id, output_id)): Path<(String, String)>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    match db::set_output_desired_state(&state.db, &pipeline_id, &output_id, "stopped").await {
        Ok(output) => Json(serde_json::json!({"message": "Output stopped", "desiredState": "stopped", "output": output})).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn output_status_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((_pipeline_id, output_id)): Path<(String, String)>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    match state.engine.output_status(&output_id).await {
        Some(status) => Json(status).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "output not active"})),
        )
            .into_response(),
    }
}

// ── /api/v1/logs — paginated query ───────────────────────────────────────────

#[derive(Deserialize)]
struct LogsQuery {
    level: Option<String>,
    since: Option<String>,
    until: Option<String>,
    target: Option<String>,
    pipeline_id: Option<String>,
    output_id: Option<String>,
    event_class: Option<String>,
    prefix: Option<String>,
    limit: Option<i64>,
    order: Option<String>,
}

async fn logs_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<LogsQuery>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let filters = AppLogFilters {
        level: query.level,
        since: query.since,
        until: query.until,
        target: query.target,
        pipeline_id: query.pipeline_id,
        output_id: query.output_id,
        event_class: query.event_class,
        prefix: query.prefix,
        limit: query.limit,
        order: query.order,
    };

    let logs = db::list_app_logs(&state.db, &filters)
        .await
        .unwrap_or_default();
    let has_more = logs.len() >= filters.limit.unwrap_or(200).clamp(1, 1000) as usize;

    Json(serde_json::json!({
        "logs": logs,
        "total": logs.len(),
        "hasMore": has_more,
    }))
    .into_response()
}

// ── /api/v1/logs/stream — SSE live tail ──────────────────────────────────────

#[derive(Deserialize)]
struct LogsStreamQuery {
    level: Option<String>,
    target: Option<String>,
    pipeline_id: Option<String>,
    output_id: Option<String>,
    last_event_id: Option<i64>,
}

async fn logs_stream_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<LogsStreamQuery>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    // Last-Event-ID from header takes priority over query param (browser reconnect path).
    let resume_from: Option<i64> = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .or(query.last_event_id);

    let min_level = query.level.unwrap_or_else(|| "info".to_string());
    let filter_target = query.target;
    let filter_pipeline = query.pipeline_id;
    let filter_output = query.output_id;

    let (tx, rx) = tokio::sync::mpsc::channel::<String>(64);

    let db_pool = state.db.clone();
    let mut broadcast_rx = state.log_broadcast.subscribe();

    tokio::spawn(async move {
        let level_passes = |level: &str| -> bool {
            match min_level.as_str() {
                "error" => level == "ERROR",
                "warn" => matches!(level, "ERROR" | "WARN"),
                "debug" => matches!(level, "ERROR" | "WARN" | "INFO" | "DEBUG"),
                _ => matches!(level, "ERROR" | "WARN" | "INFO"),
            }
        };
        // 1. Backfill entries missed since last_event_id.
        if let Some(since_id) = resume_from {
            let backfill = db::list_app_logs(
                &db_pool,
                &AppLogFilters {
                    level: Some(min_level.clone()),
                    since: None,
                    until: None,
                    target: filter_target.clone(),
                    pipeline_id: filter_pipeline.clone(),
                    output_id: filter_output.clone(),
                    event_class: None,
                    prefix: None,
                    limit: Some(200),
                    order: Some("asc".to_string()),
                },
            )
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.id > since_id);

            for row in backfill {
                let data = serde_json::json!({
                    "id": row.id, "ts": row.ts, "level": row.level,
                    "target": row.target, "message": row.message,
                    "fields": row.fields, "pipelineId": row.pipeline_id,
                    "outputId": row.output_id,
                });
                let frame = format!("id: {}\nevent: log\ndata: {}\n\n", row.id, data);
                if tx.send(frame).await.is_err() {
                    return;
                }
            }
        }

        // 2. Live tail from broadcast channel + heartbeat every 20 s.
        let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(20));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                entry = broadcast_rx.recv() => {
                    match entry {
                        Ok(e) => {
                            if !level_passes(&e.level) { continue; }
                            if let Some(ref t) = filter_target {
                                if !e.target.starts_with(t.as_str()) { continue; }
                            }
                            if let Some(ref p) = filter_pipeline {
                                if e.pipeline_id.as_deref() != Some(p.as_str()) { continue; }
                            }
                            if let Some(ref o) = filter_output {
                                if e.output_id.as_deref() != Some(o.as_str()) { continue; }
                            }
                            let data = serde_json::to_string(&e).unwrap_or_default();
                            let frame = format!("id: {}\nevent: log\ndata: {}\n\n", e.id, data);
                            if tx.send(frame).await.is_err() { return; }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // Receiver fell behind — close; client will reconnect with Last-Event-ID.
                            return;
                        }
                        Err(_) => return,
                    }
                }
                _ = heartbeat.tick() => {
                    if tx.send(": ping\n\n".to_string()).await.is_err() { return; }
                }
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = axum::body::Body::from_stream(futures_util::StreamExt::map(stream, |s| {
        Ok::<_, std::convert::Infallible>(s)
    }));

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        body,
    )
        .into_response()
}

async fn custom_encoding_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let args = db::get_meta(&state.db, "custom_encoding")
        .await
        .unwrap_or(None)
        .unwrap_or_default();
    Json(serde_json::json!({ "ffmpegArgs": args })).into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CustomEncodingPayload {
    ffmpeg_args: String,
}

async fn custom_encoding_put(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<CustomEncodingPayload>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    if let Some(r) = check_field_len("ffmpeg_args", &payload.ffmpeg_args, MAX_FFMPEG_ARGS_LEN) {
        return r;
    }
    let _ = db::set_meta(&state.db, "custom_encoding", &payload.ffmpeg_args).await;
    Json(serde_json::json!({ "ffmpegArgs": payload.ffmpeg_args })).into_response()
}

async fn ingests_get_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let ingests = db::list_ingests(&state.db).await.unwrap_or_default();
    let mut res = Vec::new();
    for i in ingests {
        let running = state.engine.is_file_ingest_running(&i.id).await;
        res.push(serde_json::json!({
            "id": i.id,
            "filename": i.filename,
            "streamKey": i.stream_key,
            "loop": i.loop_flag,
            "startTime": i.start_time,
            "running": running
        }));
    }
    Json(res).into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IngestPayload {
    filename: String,
    stream_key: String,
    #[serde(alias = "loop")]
    loop_flag: Option<bool>,
    start_time: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PipelineFileIngestPayload {
    filename: String,
    #[serde(alias = "loop")]
    loop_flag: Option<bool>,
    start_time: Option<String>,
}

fn file_ingest_response(ingest: Option<Ingest>, running: bool) -> serde_json::Value {
    match ingest {
        Some(ingest) => serde_json::json!({
            "configured": true,
            "id": ingest.id,
            "filename": ingest.filename,
            "streamKey": ingest.stream_key,
            "loop": ingest.loop_flag,
            "startTime": ingest.start_time,
            "running": running
        }),
        None => serde_json::json!({
            "configured": false,
            "running": false
        }),
    }
}

struct SpawnedFileIngestChild {
    child: Child,
    stdout: ChildStdout,
    stderr: ChildStderr,
}

fn build_file_ingest_args(ingest: &Ingest, file_path: &FsPath) -> Vec<String> {
    let mut args = vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
        "-re".into(),
    ];
    if ingest.loop_flag {
        args.extend(["-stream_loop".into(), "-1".into()]);
    }
    if !ingest.start_time.is_empty() {
        args.extend(["-ss".into(), ingest.start_time.clone()]);
    }
    args.extend(["-i".into(), file_path.to_string_lossy().into_owned()]);
    args.extend([
        "-map".into(),
        "0".into(),
        "-c".into(),
        "copy".into(),
        "-f".into(),
        "mpegts".into(),
        "pipe:1".into(),
    ]);
    args
}

fn spawn_file_ingest_child(
    ingest: &Ingest,
    file_path: &FsPath,
) -> Result<SpawnedFileIngestChild, String> {
    let ffmpeg_bin = crate::ffmpeg_extract::ensure_ffmpeg_extracted();
    let args = build_file_ingest_args(ingest, file_path);
    let mut child = Command::new(ffmpeg_bin)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn ffmpeg: {e}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture ffmpeg stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture ffmpeg stderr".to_string())?;

    Ok(SpawnedFileIngestChild {
        child,
        stdout,
        stderr,
    })
}

async fn stop_file_ingest_child(engine: &MediaEngine, ingest_id: &str) {
    let _ = engine.stop_file_ingest_child(ingest_id).await;
}

async fn unregister_file_ingest_for_stream_key(state: &AppState, stream_key: &str) {
    if let Ok(Some(pipeline)) = db::get_pipeline_by_stream_key(&state.db, stream_key).await {
        state.engine.unregister_ingest(&pipeline.id).await;
    }
}

async fn stop_file_ingests_for_stream_key(state: &AppState, stream_key: &str) {
    unregister_file_ingest_for_stream_key(state, stream_key).await;
    if let Ok(ingests) = db::list_ingests_for_stream_key(&state.db, stream_key).await {
        for ingest in ingests {
            stop_file_ingest_child(&state.engine, &ingest.id).await;
            state.engine.clear_file_ingest_running(&ingest.id).await;
        }
    }
}

async fn pipeline_file_ingest_get_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(pipeline_id): Path<String>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let pipeline = match db::get_pipeline(&state.db, &pipeline_id).await {
        Ok(Some(pipeline)) => pipeline,
        _ => return (StatusCode::NOT_FOUND, "Pipeline not found").into_response(),
    };

    let ingest = match db::get_ingest_by_stream_key(&state.db, &pipeline.stream_key).await {
        Ok(ingest) => ingest,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let running = match ingest.as_ref() {
        Some(ingest) => state.engine.is_file_ingest_running(&ingest.id).await,
        None => false,
    };

    Json(file_ingest_response(ingest, running)).into_response()
}

async fn pipeline_file_ingest_put_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(pipeline_id): Path<String>,
    Json(payload): Json<PipelineFileIngestPayload>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    if let Some(r) = check_field_len("filename", &payload.filename, MAX_NAME_LEN) {
        return r;
    }
    if let Some(ref start_time) = payload.start_time
        && let Some(r) = check_field_len("start_time", start_time, 64)
    {
        return r;
    }
    if payload.filename.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Filename cannot be empty"})),
        )
            .into_response();
    }

    let pipeline = match db::get_pipeline(&state.db, &pipeline_id).await {
        Ok(Some(pipeline)) => pipeline,
        _ => return (StatusCode::NOT_FOUND, "Pipeline not found").into_response(),
    };

    stop_file_ingests_for_stream_key(&state, &pipeline.stream_key).await;

    let loop_val = payload.loop_flag.unwrap_or(false);
    let start_time = payload.start_time.unwrap_or_default();
    let existing = match db::get_ingest_by_stream_key(&state.db, &pipeline.stream_key).await {
        Ok(ingest) => ingest,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let saved = match existing {
        Some(ingest) => match db::update_ingest(
            &state.db,
            &ingest.id,
            &payload.filename,
            &pipeline.stream_key,
            loop_val,
            &start_time,
        )
        .await
        {
            Ok(Some(updated)) => updated,
            _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        None => {
            let id = format!("ingest_{}", to_hex(&rand::random::<[u8; 8]>()));
            match db::create_ingest(
                &state.db,
                &id,
                &payload.filename,
                &pipeline.stream_key,
                loop_val,
                &start_time,
            )
            .await
            {
                Ok(created) => created,
                Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
    };

    if let Ok(ingests) = db::list_ingests_for_stream_key(&state.db, &pipeline.stream_key).await {
        for ingest in ingests.into_iter().filter(|ingest| ingest.id != saved.id) {
            let _ = db::delete_ingest(&state.db, &ingest.id).await;
        }
    }

    let input_source = format!("file:{}", payload.filename);
    if db::update_pipeline(
        &state.db,
        &pipeline.id,
        &pipeline.name,
        &pipeline.stream_key,
        Some(&input_source),
        pipeline.encoding.as_deref(),
        pipeline.srt_ingest_policy.as_deref(),
    )
    .await
    .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    Json(file_ingest_response(Some(saved), false)).into_response()
}

async fn pipeline_file_ingest_delete_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(pipeline_id): Path<String>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let pipeline = match db::get_pipeline(&state.db, &pipeline_id).await {
        Ok(Some(pipeline)) => pipeline,
        _ => return (StatusCode::NOT_FOUND, "Pipeline not found").into_response(),
    };

    stop_file_ingests_for_stream_key(&state, &pipeline.stream_key).await;
    if let Ok(ingests) = db::list_ingests_for_stream_key(&state.db, &pipeline.stream_key).await {
        for ingest in ingests {
            let _ = db::delete_ingest(&state.db, &ingest.id).await;
        }
    }

    if db::update_pipeline(
        &state.db,
        &pipeline.id,
        &pipeline.name,
        &pipeline.stream_key,
        None,
        pipeline.encoding.as_deref(),
        pipeline.srt_ingest_policy.as_deref(),
    )
    .await
    .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    Json(serde_json::json!({"deleted": true})).into_response()
}

async fn pump_file_ingest_stdout(
    state: Arc<AppState>,
    pipeline: Pipeline,
    ring_buffer: Arc<crate::media::ring_buffer::RingBuffer>,
    mut stdout: ChildStdout,
    cancel: CancellationToken,
    timestamps: &mut crate::media::file_ingest::ContinuousTimestampState,
) -> Result<(), String> {
    let (bytes_received, ingest_metrics, cached_keyframe_times) = {
        state
            .engine
            .with_active_ingest(&pipeline.id, |ingest| {
                (
                    ingest.bytes_received.clone(),
                    ingest.metrics.clone(),
                    ingest.keyframe_times.clone(),
                )
            })
            .await
            .ok_or_else(|| format!("Active ingest missing for pipeline {}", pipeline.id))?
    };

    let mut demuxer = crate::media::mpegts::TsDemuxer::new();
    let mut packets = Vec::with_capacity(16);
    let mut probe_sent = false;
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        let read = tokio::select! {
            _ = cancel.cancelled() => break,
            res = stdout.read(&mut buf) => res,
        }
        .map_err(|e| format!("Failed to read ffmpeg stdout: {e}"))?;

        if read == 0 {
            break;
        }

        demuxer.feed(&buf[..read]);
        if demuxer.drain_into(&mut packets) > 0 {
            for pkt in &mut packets {
                timestamps.apply(pkt);
                if pkt.media_type == crate::media::ring_buffer::MediaType::Video && pkt.is_keyframe
                {
                    let mut times = cached_keyframe_times
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    times.push(pkt.pts);
                    if times.len() > 30 {
                        times.remove(0);
                    }
                }
            }
            ring_buffer.push_batch(packets.drain(..));
        }

        if !probe_sent && let Some(probe) = demuxer.take_probe() {
            probe_sent = true;
            let first_audio = probe.audio_tracks.first().cloned();
            state
                .engine
                .update_ingest_meta(&pipeline.id, probe.video, first_audio, None)
                .await;
            if !probe.audio_tracks.is_empty() {
                state
                    .engine
                    .update_ingest_audio_tracks(&pipeline.id, probe.audio_tracks)
                    .await;
            }
        }

        bytes_received.fetch_add(read as u64, std::sync::atomic::Ordering::Relaxed);
        ingest_metrics.record_in(read as u64);
    }

    Ok(())
}

async fn log_file_ingest_stderr(
    ingest_id: &str,
    mut stderr: ChildStderr,
) -> Result<(), std::io::Error> {
    const STDERR_CAP: usize = 64 * 1024;
    let mut buf = [0u8; 4096];
    let mut all = Vec::new();
    let mut truncated = false;

    loop {
        match stderr.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let remaining = STDERR_CAP.saturating_sub(all.len());
                if remaining > 0 {
                    all.extend_from_slice(&buf[..n.min(remaining)]);
                } else if !truncated {
                    truncated = true;
                    warn!(ingest_id = %ingest_id, cap = STDERR_CAP, "ffmpeg stderr truncated");
                }
            }
            Err(e) => return Err(e),
        }
    }

    if !all.is_empty() {
        warn!(ingest_id = %ingest_id, stderr = %String::from_utf8_lossy(&all).trim(), "ffmpeg stderr");
    }

    Ok(())
}

async fn run_file_ingest_task(
    state: Arc<AppState>,
    ingest: Ingest,
    pipeline: Pipeline,
    file_path: PathBuf,
    ring_buffer: Arc<crate::media::ring_buffer::RingBuffer>,
    cancel: CancellationToken,
    mut spawned: SpawnedFileIngestChild,
) {
    let mut timestamps = crate::media::file_ingest::ContinuousTimestampState::default();
    loop {
        state
            .engine
            .file_ingests
            .children
            .write()
            .await
            .insert(ingest.id.clone(), spawned.child);

        let stderr_id = ingest.id.clone();
        let stdout_fut = pump_file_ingest_stdout(
            state.clone(),
            pipeline.clone(),
            ring_buffer.clone(),
            spawned.stdout,
            cancel.clone(),
            &mut timestamps,
        );
        let stderr_fut = log_file_ingest_stderr(&stderr_id, spawned.stderr);
        let (stdout_res, stderr_res) = tokio::join!(stdout_fut, stderr_fut);

        let mut exit_status = None;
        if let Some(mut child) = state.engine.take_file_ingest_child(&ingest.id).await {
            exit_status = child.wait().await.ok();
        }

        if let Err(err) = stdout_res
            && !cancel.is_cancelled()
        {
            error!(ingest_id = %ingest.id, err = %err, "file-ingest stdout reader failed");
        }
        if let Err(err) = stderr_res
            && !cancel.is_cancelled()
        {
            error!(ingest_id = %ingest.id, err = %err, "file-ingest stderr reader failed");
        }

        if let Some(status) = exit_status
            && !status.success()
            && !cancel.is_cancelled()
        {
            warn!(ingest_id = %ingest.id, status = %status, "ffmpeg exited unsuccessfully");
        }

        if cancel.is_cancelled() || !ingest.loop_flag {
            break;
        }

        match spawn_file_ingest_child(&ingest, &file_path) {
            Ok(next) => spawned = next,
            Err(err) => {
                error!(ingest_id = %ingest.id, err = %err, "file-ingest restart failed");
                break;
            }
        }
    }

    state.engine.clear_file_ingest_running(&ingest.id).await;
    state.engine.unregister_ingest(&pipeline.id).await;
}

async fn ingests_post_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<IngestPayload>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    if let Some(r) = check_field_len("filename", &payload.filename, MAX_NAME_LEN) {
        return r;
    }
    if let Some(r) = check_field_len("stream_key", &payload.stream_key, MAX_STREAM_KEY_LEN) {
        return r;
    }
    if let Some(ref s) = payload.start_time
        && let Some(r) = check_field_len("start_time", s, 64)
    {
        return r;
    }
    let id = format!("ingest_{}", to_hex(&rand::random::<[u8; 8]>()));
    let loop_val = payload.loop_flag.unwrap_or(false);
    let start_time = payload.start_time.unwrap_or_default();

    match db::create_ingest(
        &state.db,
        &id,
        &payload.filename,
        &payload.stream_key,
        loop_val,
        &start_time,
    )
    .await
    {
        Ok(ingest) => Json(serde_json::json!({
            "id": ingest.id,
            "filename": ingest.filename,
            "streamKey": ingest.stream_key,
            "loop": ingest.loop_flag,
            "startTime": ingest.start_time,
            "running": false
        }))
        .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn ingests_update_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(payload): Json<IngestPayload>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    if let Some(ref s) = payload.start_time
        && let Some(r) = check_field_len("start_time", s, 64)
    {
        return r;
    }
    let loop_val = payload.loop_flag.unwrap_or(false);
    let start_time = payload.start_time.unwrap_or_default();

    match db::update_ingest(
        &state.db,
        &id,
        &payload.filename,
        &payload.stream_key,
        loop_val,
        &start_time,
    )
    .await
    {
        Ok(Some(ingest)) => {
            let running = state.engine.is_file_ingest_running(&ingest.id).await;
            Json(serde_json::json!({
                "id": ingest.id,
                "filename": ingest.filename,
                "streamKey": ingest.stream_key,
                "loop": ingest.loop_flag,
                "startTime": ingest.start_time,
                "running": running
            }))
            .into_response()
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn ingests_delete_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    if let Ok(Some(ingest)) = db::get_ingest(&state.db, &id).await {
        unregister_file_ingest_for_stream_key(&state, &ingest.stream_key).await;
    }
    stop_file_ingest_child(&state.engine, &id).await;
    state.engine.clear_file_ingest_running(&id).await;

    let _ = db::delete_ingest(&state.db, &id).await;
    Json(serde_json::json!({"deleted": true})).into_response()
}

async fn ingests_start_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let ingest = match db::get_ingest(&state.db, &id).await {
        Ok(Some(i)) => i,
        _ => return (StatusCode::NOT_FOUND, "Ingest not found").into_response(),
    };

    if state.engine.is_file_ingest_running(&id).await {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "Ingest already running"})),
        )
            .into_response();
    }

    let file_path = FsPath::new(&state.media_dir).join(&ingest.filename);
    if !file_path.exists() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Media file not found"})),
        )
            .into_response();
    }

    let pipeline = match db::get_pipeline_by_stream_key(&state.db, &ingest.stream_key).await {
        Ok(Some(pipeline)) => pipeline,
        Ok(None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "No pipeline found for stream key"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Failed to resolve pipeline: {e}")})),
            )
                .into_response();
        }
    };

    let ring_buffer = state.engine.get_or_create_pipeline(&pipeline.id).await;
    let Some(cancel) = state
        .engine
        .try_register_ingest(&pipeline.id, &ingest.stream_key, "file")
        .await
    else {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "Pipeline already has an active ingest"})),
        )
            .into_response();
    };

    state.engine.mark_file_ingest_running(&ingest.id).await;

    if crate::media::file_ingest::use_internal_file_ingest() {
        if let Err(e) = crate::media::file_ingest::spawn_internal_file_ingest(
            state.engine.clone(),
            tokio::runtime::Handle::current(),
            ingest.id.clone(),
            pipeline.id.clone(),
            file_path,
            ingest.start_time.clone(),
            ingest.loop_flag,
            ring_buffer,
            cancel,
        ) {
            state.engine.clear_file_ingest_running(&ingest.id).await;
            state.engine.unregister_ingest(&pipeline.id).await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e})),
            )
                .into_response();
        }
    } else {
        let spawned = match spawn_file_ingest_child(&ingest, &file_path) {
            Ok(child) => child,
            Err(e) => {
                state.engine.clear_file_ingest_running(&ingest.id).await;
                state.engine.unregister_ingest(&pipeline.id).await;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e})),
                )
                    .into_response();
            }
        };

        tokio::spawn(run_file_ingest_task(
            state.clone(),
            ingest.clone(),
            pipeline,
            file_path,
            ring_buffer,
            cancel,
            spawned,
        ));
    }

    Json(serde_json::json!({
        "id": ingest.id,
        "filename": ingest.filename,
        "streamKey": ingest.stream_key,
        "loop": ingest.loop_flag,
        "startTime": ingest.start_time,
        "running": true
    }))
    .into_response()
}

async fn ingests_stop_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let ingest = match db::get_ingest(&state.db, &id).await {
        Ok(Some(i)) => i,
        _ => return (StatusCode::NOT_FOUND, "Ingest not found").into_response(),
    };

    unregister_file_ingest_for_stream_key(&state, &ingest.stream_key).await;
    stop_file_ingest_child(&state.engine, &id).await;
    state.engine.clear_file_ingest_running(&id).await;

    Json(serde_json::json!({
        "id": ingest.id,
        "filename": ingest.filename,
        "streamKey": ingest.stream_key,
        "loop": ingest.loop_flag,
        "startTime": ingest.start_time,
        "running": false
    }))
    .into_response()
}

async fn status_get_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }

    let sys = System::new_all();
    let bonding_available = state.engine.bonding_available();
    let (mut status, _) = crate::runtime_info::status_and_sbom(bonding_available);
    status["os"] = system_status(&sys);

    Json(status).into_response()
}

async fn build_health_snapshot(state: &AppState) -> serde_json::Value {
    let pipeline_ids: Vec<String> = match db::list_pipelines(&state.db).await {
        Ok(rows) => rows.into_iter().map(|r| r.id).collect(),
        Err(_) => vec![],
    };
    let mut recording_enabled = std::collections::HashMap::new();
    for pid in &pipeline_ids {
        let rec_key = format!("recording_enabled:{}", pid);
        let rec = db::get_meta(&state.db, &rec_key)
            .await
            .ok()
            .flatten()
            .map(|v| v == "1")
            .unwrap_or(false);
        recording_enabled.insert(pid.clone(), rec);
    }
    state
        .engine
        .health_snapshot(&pipeline_ids, &recording_enabled)
        .await
}

async fn v1_engine_health_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    Json(build_health_snapshot(&state).await).into_response()
}

fn system_status(sys: &System) -> serde_json::Value {
    serde_json::json!({
        "platform": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "hostname": System::host_name().unwrap_or_default(),
        "kernelVersion": System::kernel_version(),
        "uptime": System::uptime(),
        "totalMem": sys.total_memory(),
        "cpu": cpu_status(sys),
    })
}

fn cpu_status(sys: &System) -> serde_json::Value {
    let cpuinfo = read_cpuinfo_summary();
    let first_cpu = sys.cpus().first();
    let logical_cpus = sys.cpus().len();
    let physical_cores = sys.physical_core_count();
    let threads_per_core = physical_cores
        .filter(|cores| *cores > 0)
        .map(|cores| logical_cpus as f64 / cores as f64);
    let flags = cpuinfo
        .get("flags")
        .map(|value| selected_cpu_flags(value))
        .unwrap_or_default();
    let hypervisor_detected = flags.iter().any(|flag| flag == "hypervisor");
    let virtualization = if flags.iter().any(|flag| flag == "vmx") {
        Some("VT-x")
    } else if flags.iter().any(|flag| flag == "svm") {
        Some("AMD-V")
    } else {
        None
    };

    serde_json::json!({
        "modelName": cpuinfo
            .get("model name")
            .or_else(|| cpuinfo.get("hardware"))
            .or_else(|| cpuinfo.get("processor"))
            .cloned()
            .or_else(|| first_cpu.map(|cpu| cpu.brand().to_string())),
        "logicalCpus": logical_cpus,
        "physicalCores": physical_cores,
        "threadsPerCore": threads_per_core,
        "virtualization": virtualization,
        "hypervisorDetected": hypervisor_detected,
        "hypervisorVendor": if hypervisor_detected { detect_hypervisor_vendor() } else { None },
        "flags": flags,
    })
}

fn read_cpuinfo_summary() -> HashMap<String, String> {
    let mut summary = HashMap::new();
    let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") else {
        return summary;
    };
    for line in text.lines() {
        if line.trim().is_empty() && !summary.is_empty() {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        summary
            .entry(key.trim().to_ascii_lowercase())
            .or_insert_with(|| value.trim().to_string());
    }
    summary
}

fn selected_cpu_flags(flags: &str) -> Vec<String> {
    const USEFUL_FLAGS: &[&str] = &[
        "sse4_1",
        "sse4_2",
        "avx",
        "avx2",
        "avx512f",
        "avx_vnni",
        "fma",
        "aes",
        "sha_ni",
        "vaes",
        "vpclmulqdq",
        "bmi1",
        "bmi2",
        "vmx",
        "svm",
        "hypervisor",
    ];
    let available = flags.split_whitespace().collect::<BTreeSet<_>>();
    USEFUL_FLAGS
        .iter()
        .filter(|flag| available.contains(**flag))
        .map(|flag| (*flag).to_string())
        .collect()
}

fn detect_hypervisor_vendor() -> Option<String> {
    for path in [
        "/sys/hypervisor/type",
        "/sys/class/dmi/id/sys_vendor",
        "/sys/class/dmi/id/product_name",
    ] {
        let Some(value) = read_trimmed_file(path) else {
            continue;
        };
        let lower = value.to_ascii_lowercase();
        if lower.contains("microsoft") {
            return Some("Microsoft".to_string());
        }
        if lower.contains("vmware") {
            return Some("VMware".to_string());
        }
        if lower.contains("kvm") || lower.contains("qemu") {
            return Some("KVM/QEMU".to_string());
        }
        if lower.contains("virtualbox") {
            return Some("VirtualBox".to_string());
        }
    }
    None
}

fn read_trimmed_file(path: impl AsRef<FsPath>) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

async fn status_sbom_get_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let bonding_available = state.engine.bonding_available();
    let (_, sbom) = crate::runtime_info::status_and_sbom(bonding_available);
    (
        [
            (
                header::CONTENT_TYPE,
                "application/vnd.cyclonedx+json; version=1.5",
            ),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"restream-sbom.cdx.json\"",
            ),
        ],
        Json(sbom),
    )
        .into_response()
}

async fn media_list_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let mut files = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&state.media_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if (name.ends_with(".ts")
                || name.ends_with(".mkv")
                || name.ends_with(".mp4")
                || name.ends_with(".mov"))
                && let Ok(metadata) = entry.metadata().await
            {
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .and_then(|d| chrono::DateTime::from_timestamp_millis(d.as_millis() as i64))
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default();

                let ingests = db::list_ingests_for_filename(&state.db, &name)
                    .await
                    .unwrap_or_default();
                let lower_name = name.to_ascii_lowercase();
                let kind = if lower_name.contains("recording") {
                    "recording"
                } else {
                    "source"
                };
                files.push(serde_json::json!({
                    "name": name,
                    "size": metadata.len(),
                    "modifiedAt": modified,
                    "ingestCount": ingests.len(),
                    "kind": kind
                }));
            }
        }
    }

    Json(serde_json::json!({ "files": files })).into_response()
}

fn media_content_type(filename: &str) -> &'static str {
    match filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "ts" => "video/mp2t",
        "mkv" => "video/x-matroska",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        _ => "application/octet-stream",
    }
}

fn media_path_under_root(
    media_dir: &str,
    filename: &str,
) -> Result<std::path::PathBuf, StatusCode> {
    let _ = std::fs::create_dir_all(media_dir);
    let media_root =
        std::fs::canonicalize(media_dir).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let path = std::path::Path::new(media_dir).join(filename);
    let canonical_path = std::fs::canonicalize(&path).map_err(|_| StatusCode::NOT_FOUND)?;
    if !canonical_path.starts_with(&media_root) {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(canonical_path)
}

async fn media_file_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(filename): Path<String>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let path = match media_path_under_root(&state.media_dir, &filename) {
        Ok(path) => path,
        Err(status) => return status.into_response(),
    };
    match tokio::fs::read(path).await {
        Ok(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, media_content_type(&filename))],
            bytes,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "File not found").into_response(),
    }
}

async fn media_delete_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(filename): Path<String>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let ingests = db::list_ingests_for_filename(&state.db, &filename)
        .await
        .unwrap_or_default();
    if !ingests.is_empty() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "Cannot delete: file has configured ingests"})),
        )
            .into_response();
    }

    let canonical_path = match media_path_under_root(&state.media_dir, &filename) {
        Ok(path) => path,
        Err(StatusCode::INTERNAL_SERVER_ERROR) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "Media directory error").into_response();
        }
        Err(StatusCode::NOT_FOUND) => {
            return (StatusCode::NOT_FOUND, "File not found").into_response();
        }
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid path").into_response(),
    };

    match tokio::fs::remove_file(canonical_path).await {
        Ok(_) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "File not found").into_response(),
    }
}

async fn healthz_get_handler() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

fn proc_total_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let cpu = stat.lines().find(|line| line.starts_with("cpu "))?;
    Some(
        cpu.split_whitespace()
            .skip(1)
            .filter_map(|value| value.parse::<u64>().ok())
            .sum(),
    )
}

fn proc_process_ticks(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let end_comm = stat.rfind(')')?;
    let fields: Vec<&str> = stat[end_comm + 2..].split_whitespace().collect();
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    Some(utime + stime)
}

fn engine_process_pids(sys: &System) -> Vec<u32> {
    let own_pid = std::process::id();
    let own_sys_pid = sysinfo::Pid::from_u32(own_pid);
    let mut pids = vec![own_pid];

    for (pid, process) in sys.processes() {
        let name = process.name().to_ascii_lowercase();
        if process.parent() == Some(own_sys_pid) && name.contains("ffmpeg") {
            pids.push(pid.as_u32());
        }
    }

    pids.sort_unstable();
    pids.dedup();
    pids
}

fn engine_metrics(sys: &System, core_count: usize) -> serde_json::Value {
    let own_pid = std::process::id();
    let own_sys_pid = sysinfo::Pid::from_u32(own_pid);
    let pids = engine_process_pids(sys);

    let restream_memory = sys
        .process(own_sys_pid)
        .map(|process| process.memory())
        .unwrap_or(0);
    let mut external_ffmpeg_count = 0u64;
    let mut external_ffmpeg_memory = 0u64;
    let mut total_memory = 0u64;
    let mut external_ffmpeg_ticks = 0u64;

    for pid in &pids {
        if let Some(process) = sys.process(sysinfo::Pid::from_u32(*pid)) {
            let memory = process.memory();
            total_memory = total_memory.saturating_add(memory);
            if *pid != own_pid && process.name().to_ascii_lowercase().contains("ffmpeg") {
                external_ffmpeg_count += 1;
                external_ffmpeg_memory = external_ffmpeg_memory.saturating_add(memory);
                external_ffmpeg_ticks =
                    external_ffmpeg_ticks.saturating_add(proc_process_ticks(*pid).unwrap_or(0));
            }
        }
    }

    let restream_ticks = proc_process_ticks(own_pid).unwrap_or(0);
    let total_ticks = proc_total_ticks();
    let mut cpu_sample_ready = false;
    let (cpu_percent, restream_cpu_percent, external_ffmpeg_cpu_percent) = total_ticks
        .and_then(|total_ticks| {
            let sample = EngineCpuSample {
                total_ticks,
                restream_ticks,
                external_ffmpeg_ticks,
            };
            let lock = ENGINE_CPU_SAMPLE.get_or_init(|| Mutex::new(None));
            let mut previous = lock.lock().ok()?;
            let cpu = previous.map(|prev| {
                cpu_sample_ready = true;
                let total_delta = sample.total_ticks.saturating_sub(prev.total_ticks);
                if total_delta == 0 {
                    return (0.0, 0.0, 0.0);
                }
                let scale = core_count.max(1) as f64 * 100.0 / total_delta as f64;
                let restream_delta = sample.restream_ticks.saturating_sub(prev.restream_ticks);
                let ffmpeg_delta = sample
                    .external_ffmpeg_ticks
                    .saturating_sub(prev.external_ffmpeg_ticks);
                let restream_cpu = restream_delta as f64 * scale;
                let ffmpeg_cpu = ffmpeg_delta as f64 * scale;
                (restream_cpu + ffmpeg_cpu, restream_cpu, ffmpeg_cpu)
            });
            *previous = Some(sample);
            cpu
        })
        .unwrap_or((0.0, 0.0, 0.0));

    serde_json::json!({
        "cpuPercent": cpu_percent,
        "cpuSampleReady": cpu_sample_ready,
        "restreamCpuPercent": restream_cpu_percent,
        "externalFfmpegCpuPercent": external_ffmpeg_cpu_percent,
        "memoryBytes": restream_memory,
        "restreamMemoryBytes": restream_memory,
        "totalMemoryBytes": total_memory,
        "externalFfmpegCount": external_ffmpeg_count,
        "externalFfmpegMemoryBytes": external_ffmpeg_memory,
    })
}

async fn metrics_system_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let mut sys = System::new_all();
    sys.refresh_all();

    let cpu_pct = sys.global_cpu_info().cpu_usage() as f64;
    let total_mem = sys.total_memory();
    let used_mem = sys.used_memory();
    let free_mem = total_mem.saturating_sub(used_mem);
    let mem_pct = if total_mem > 0 {
        (used_mem as f64 / total_mem as f64) * 100.0
    } else {
        0.0
    };
    let core_count = sys.cpus().len();
    let load_avg = System::load_average();
    let engine = engine_metrics(&sys, core_count);

    let media_root = {
        let configured = FsPath::new(&state.media_dir);
        let absolute = if configured.is_absolute() {
            configured.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(configured)
        };
        std::fs::canonicalize(&absolute).unwrap_or(absolute)
    };
    let disks = Disks::new_with_refreshed_list();

    fn disk_usage_for_path(disks: &Disks, path: &FsPath) -> Option<(u64, u64, String)> {
        disks
            .iter()
            .filter_map(|disk| {
                let mount = disk.mount_point();
                path.starts_with(mount)
                    .then_some((disk, mount.components().count()))
            })
            .max_by_key(|(_, depth)| *depth)
            .map(|(disk, _)| {
                let total = disk.total_space();
                let used = total.saturating_sub(disk.available_space());
                (total, used, disk.mount_point().display().to_string())
            })
    }

    let system_root = {
        #[cfg(unix)]
        {
            PathBuf::from("/")
        }
        #[cfg(not(unix))]
        {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        }
    };
    let (total_disk, used_disk, disk_mount) =
        if let Some((total, used, mount)) = disk_usage_for_path(&disks, &system_root) {
            (total, used, Some(mount))
        } else {
            let (total, used) = disks.iter().fold((0u64, 0u64), |(t, u), d| {
                (
                    t + d.total_space(),
                    u + (d.total_space() - d.available_space()),
                )
            });
            (total, used, None)
        };
    let free_disk = total_disk.saturating_sub(used_disk);
    let disk_pct = if total_disk > 0 {
        (used_disk as f64 / total_disk as f64) * 100.0
    } else {
        0.0
    };
    let media_disk = disk_usage_for_path(&disks, &media_root).map(|(total, used, mount)| {
        let free = total.saturating_sub(used);
        let used_pct = if total > 0 {
            (used as f64 / total as f64) * 100.0
        } else {
            0.0
        };
        serde_json::json!({
            "totalBytes": total,
            "usedBytes": used,
            "freeBytes": free,
            "usedPercent": used_pct,
            "scope": "mediaDir",
            "mountPoint": mount,
            "mediaDir": state.media_dir,
            "mediaRoot": media_root.display().to_string()
        })
    });

    fn is_external_interface(name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        if lower == "lo" || lower.starts_with("lo:") {
            return false;
        }
        let virtual_prefixes = [
            "docker",
            "br-",
            "veth",
            "virbr",
            "vmnet",
            "zt",
            "tailscale",
            "tun",
            "tap",
            "wg",
        ];
        !virtual_prefixes
            .iter()
            .any(|prefix| lower.starts_with(prefix))
    }

    // Collect a short network sample. The navbar reports external interfaces so
    // local RTMP/SRT test traffic on loopback does not drown out host egress.
    let nets1 = Networks::new_with_refreshed_list();
    tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
    let nets2 = Networks::new_with_refreshed_list();
    let mut total_rx = 0u64;
    let mut total_tx = 0u64;
    let mut external_interfaces = Vec::new();
    let mut ignored_interfaces = Vec::new();
    for (iface, n2) in nets2.iter() {
        if let Some(n1) = nets1.get(iface) {
            let rx = n2.total_received().saturating_sub(n1.total_received());
            let tx = n2
                .total_transmitted()
                .saturating_sub(n1.total_transmitted());
            let active = rx > 0 || tx > 0;
            if is_external_interface(iface) {
                total_rx += rx;
                total_tx += tx;
                if active {
                    external_interfaces.push(serde_json::json!({
                        "name": iface,
                        "downloadBytesPerSec": rx * 4,
                        "uploadBytesPerSec": tx * 4,
                        "downloadKbps": (rx * 4 * 8) as f64 / 1000.0,
                        "uploadKbps": (tx * 4 * 8) as f64 / 1000.0,
                    }));
                }
            } else if active {
                ignored_interfaces.push(iface.to_string());
            }
        }
    }
    // Scale 250ms sample to per-second
    let dl_bytes_sec = total_rx * 4;
    let ul_bytes_sec = total_tx * 4;
    let dl_kbps = (dl_bytes_sec * 8) as f64 / 1000.0;
    let ul_kbps = (ul_bytes_sec * 8) as f64 / 1000.0;

    let now = chrono::Utc::now().to_rfc3339();
    Json(serde_json::json!({
        "generatedAt": now,
        "cpu": {
            "usagePercent": cpu_pct,
            "cores": core_count,
            "load1": load_avg.one
        },
        "memory": {
            "totalBytes": total_mem,
            "usedBytes": used_mem,
            "freeBytes": free_mem,
            "usedPercent": mem_pct
        },
        "engine": engine,
        "disk": {
            "totalBytes": total_disk,
            "usedBytes": used_disk,
            "freeBytes": free_disk,
            "usedPercent": disk_pct,
            "scope": "systemRoot",
            "mountPoint": disk_mount,
            "root": system_root.display().to_string()
        },
        "mediaDisk": media_disk,
        "network": {
            "scope": "external",
            "downloadBytesPerSec": dl_bytes_sec,
            "uploadBytesPerSec": ul_bytes_sec,
            "downloadKbps": dl_kbps,
            "uploadKbps": ul_kbps,
            "interfaces": external_interfaces,
            "ignoredInterfaces": ignored_interfaces,
            "sampleMs": 250
        }
    }))
    .into_response()
}

/// Server-Sent Events endpoint for per-pipeline diagnostics.
/// The frontend `diagnostics.js` opens an EventSource to this URL.
#[derive(Deserialize)]
struct DiagnosticsQuery {
    probe: Option<String>,
    #[allow(dead_code)]
    publisher: Option<String>,
    #[allow(dead_code)]
    since: Option<String>,
}

async fn pipeline_probe_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(pipeline_id): Path<String>,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    match state.engine.probe_snapshot(&pipeline_id).await {
        Some(probe) => Json(probe).into_response(),
        None => (StatusCode::NOT_FOUND, "No active ingest for this pipeline").into_response(),
    }
}

async fn pipeline_graph_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(pipeline_id): Path<String>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    if !db::list_pipelines(&state.db)
        .await
        .unwrap_or_default()
        .iter()
        .any(|pipeline| pipeline.id == pipeline_id)
    {
        return (StatusCode::NOT_FOUND, "Pipeline not found").into_response();
    }

    let outputs = db::list_outputs(&state.db).await.unwrap_or_default();
    let graph = state.engine.processing_graph(&pipeline_id, &outputs).await;
    Json(graph).into_response()
}

/// Returns derived alerts for a single pipeline. Auth required.
async fn pipeline_alerts_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(pipeline_id): Path<String>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }

    let mut recording_enabled = std::collections::HashMap::new();
    let rec_key = format!("recording_enabled:{}", pipeline_id);
    let rec = db::get_meta(&state.db, &rec_key)
        .await
        .ok()
        .flatten()
        .map(|v| v == "1")
        .unwrap_or(false);
    recording_enabled.insert(pipeline_id.clone(), rec);

    let snapshot = state
        .engine
        .health_snapshot(std::slice::from_ref(&pipeline_id), &recording_enabled)
        .await;
    let generated_at = snapshot["generatedAt"].as_str().unwrap_or("").to_string();
    let mut alert_list = alerts::derive_alerts(&snapshot);
    state
        .alert_tracker
        .track_pipeline(&pipeline_id, &mut alert_list);
    Json(serde_json::json!({
        "generatedAt": generated_at,
        "alerts": alert_list,
    }))
    .into_response()
}

/// Returns derived alerts across all pipelines. Auth required.
async fn aggregate_alerts_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }

    let pipeline_ids: Vec<String> = match db::list_pipelines(&state.db).await {
        Ok(rows) => rows.into_iter().map(|r| r.id).collect(),
        Err(_) => vec![],
    };
    let mut recording_enabled = std::collections::HashMap::new();
    for pid in &pipeline_ids {
        let rec_key = format!("recording_enabled:{}", pid);
        let rec = db::get_meta(&state.db, &rec_key)
            .await
            .ok()
            .flatten()
            .map(|v| v == "1")
            .unwrap_or(false);
        recording_enabled.insert(pid.clone(), rec);
    }
    let snapshot = state
        .engine
        .health_snapshot(&pipeline_ids, &recording_enabled)
        .await;
    let generated_at = snapshot["generatedAt"].as_str().unwrap_or("").to_string();
    let mut alert_list = alerts::derive_alerts(&snapshot);
    state.alert_tracker.track(&mut alert_list);
    Json(serde_json::json!({
        "generatedAt": generated_at,
        "alerts": alert_list,
    }))
    .into_response()
}

/// Query params for GET /api/v1/events.
#[derive(Debug, serde::Deserialize)]
struct EventsQuery {
    pipeline_id: Option<String>,
    #[serde(default = "default_events_limit")]
    limit: usize,
}

fn default_events_limit() -> usize {
    100
}

/// Returns recent lifecycle events. Auth required.
/// Optional query params: pipeline_id, limit (default 100, max 1000).
async fn v1_events_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<EventsQuery>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }

    let limit = query.limit.min(events::MAX_EVENTS);
    let pipeline_filter = query.pipeline_id.as_deref();
    let event_list = state.engine.recent_events(limit, pipeline_filter);

    Json(serde_json::json!({
        "generatedAt": chrono::Utc::now().to_rfc3339(),
        "count": event_list.len(),
        "events": event_list,
    }))
    .into_response()
}

/// Operator overview: total/active/degraded pipelines, failed outputs, alert counts.
/// Auth required. Derives all numbers from a single health_snapshot pass.
async fn v1_overview_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }

    let pipelines = db::list_pipelines(&state.db).await.unwrap_or_default();
    let pipeline_ids: Vec<String> = pipelines.iter().map(|p| p.id.clone()).collect();
    let mut recording_enabled = std::collections::HashMap::new();
    for pid in &pipeline_ids {
        let rec_key = format!("recording_enabled:{}", pid);
        let rec = db::get_meta(&state.db, &rec_key)
            .await
            .ok()
            .flatten()
            .map(|v| v == "1")
            .unwrap_or(false);
        recording_enabled.insert(pid.clone(), rec);
    }
    let snapshot = state
        .engine
        .health_snapshot(&pipeline_ids, &recording_enabled)
        .await;

    let alert_list = alerts::derive_alerts(&snapshot);
    let critical = alert_list
        .iter()
        .filter(|a| matches!(a.severity, alerts::Severity::Critical))
        .count();
    let warning = alert_list
        .iter()
        .filter(|a| matches!(a.severity, alerts::Severity::Warning))
        .count();

    let snap_pipelines = snapshot["pipelines"].as_object();

    let total = pipeline_ids.len();
    let mut active = 0usize;
    let mut degraded = 0usize;
    let mut failed_outputs = 0usize;

    if let Some(pip_map) = snap_pipelines {
        for (pip_id, pip) in pip_map {
            let is_live = pip["input"]["status"].as_str() == Some("on");
            if is_live {
                active += 1;
            }
            let has_alerts = alert_list
                .iter()
                .any(|a| a.pipeline_id.as_deref() == Some(pip_id.as_str()));
            if has_alerts {
                degraded += 1;
            }
            if is_live && let Some(outputs) = pip["outputs"].as_object() {
                for output in outputs.values() {
                    if output["status"].as_str().unwrap_or("") != "running" {
                        failed_outputs += 1;
                    }
                }
            }
        }
    }

    let generated_at = snapshot["generatedAt"].as_str().unwrap_or("").to_string();

    Json(serde_json::json!({
        "generatedAt": generated_at,
        "totalPipelines": total,
        "activePipelines": active,
        "degradedPipelines": degraded,
        "failedOutputs": failed_outputs,
        "alertCount": { "critical": critical, "warning": warning },
        "srtListener": snapshot["srtListener"],
    }))
    .into_response()
}

async fn v1_engine_telemetry_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    Json(state.engine.engine_telemetry().await).into_response()
}

async fn v1_pipeline_telemetry_handler(
    State(state): State<Arc<AppState>>,
    Path(pipeline_id): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    Json(state.engine.pipeline_telemetry(&pipeline_id).await).into_response()
}

async fn v1_stage_telemetry_handler(
    State(state): State<Arc<AppState>>,
    Path(stage_key): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    match state.engine.stage_telemetry_by_display(&stage_key).await {
        Some(val) => Json(val).into_response(),
        None => (StatusCode::NOT_FOUND, "Stage not found").into_response(),
    }
}

#[cfg(feature = "agent-plane")]
async fn agent_capabilities_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    Json(crate::agent_plane::capabilities()).into_response()
}

#[cfg(not(feature = "agent-plane"))]
async fn agent_capabilities_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    agent_plane_unavailable()
}

#[cfg(feature = "agent-plane")]
async fn agent_context_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }

    let context = build_agent_context(&state).await;
    Json(context).into_response()
}

#[cfg(not(feature = "agent-plane"))]
async fn agent_context_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    agent_plane_unavailable()
}

#[cfg(feature = "agent-plane")]
async fn agent_investigation_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<crate::agent_plane::InvestigationRequest>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }

    let pipelines = db::list_pipelines(&state.db).await.unwrap_or_default();
    let outputs = db::list_outputs(&state.db).await.unwrap_or_default();
    let pipeline_exists = request
        .pipeline_id
        .as_deref()
        .is_none_or(|pid| pipelines.iter().any(|p| p.id == pid));
    let output_exists = request.output_id.as_deref().is_none_or(|oid| {
        outputs.iter().any(|output| {
            output.id == oid
                && request
                    .pipeline_id
                    .as_deref()
                    .is_none_or(|pid| output.pipeline_id == pid)
        })
    });

    let pipeline_ids: Vec<String> = request
        .pipeline_id
        .clone()
        .map(|pid| vec![pid])
        .unwrap_or_else(|| pipelines.iter().map(|p| p.id.clone()).collect());
    let recording_enabled = recording_enabled_map(&state, &pipeline_ids).await;
    let health = state
        .engine
        .health_snapshot(&pipeline_ids, &recording_enabled)
        .await;
    let alerts = alerts::derive_alerts(&health);
    let graph = if let Some(pid) = request.pipeline_id.as_deref()
        && pipeline_exists
    {
        Some(state.engine.processing_graph(pid, &outputs).await)
    } else {
        None
    };
    let telemetry = if let Some(pid) = request.pipeline_id.as_deref() {
        state.engine.pipeline_telemetry(pid).await
    } else {
        state.engine.engine_telemetry().await
    };
    let events = state.engine.recent_events(
        request.event_limit.min(events::MAX_EVENTS),
        request.pipeline_id.as_deref(),
    );

    Json(crate::agent_plane::investigation_response(
        request,
        pipeline_exists,
        output_exists,
        health,
        graph,
        telemetry,
        alerts,
        events,
    ))
    .into_response()
}

#[cfg(not(feature = "agent-plane"))]
async fn agent_investigation_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(_request): Json<serde_json::Value>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    agent_plane_unavailable()
}

#[cfg(feature = "agent-plane")]
async fn agent_plan_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<crate::agent_plane::PlanRequest>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }

    let response = build_agent_plan(&state, request).await;
    Json(response).into_response()
}

#[cfg(not(feature = "agent-plane"))]
async fn agent_plan_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(_request): Json<serde_json::Value>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    agent_plane_unavailable()
}

#[cfg(feature = "agent-plane")]
async fn agent_plan_validate_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<crate::agent_plane::PlanRequest>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }

    let response = build_agent_plan(&state, request).await;
    Json(serde_json::json!({
        "generatedAt": response.generated_at,
        "planId": response.plan_id,
        "validation": response.validation,
    }))
    .into_response()
}

#[cfg(not(feature = "agent-plane"))]
async fn agent_plan_validate_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(_request): Json<serde_json::Value>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    agent_plane_unavailable()
}

#[cfg(feature = "agent-plane")]
async fn agent_graph_diff_preview_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<crate::agent_plane::PlanRequest>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }

    let response = build_agent_plan(&state, request).await;
    Json(serde_json::json!({
        "generatedAt": response.generated_at,
        "planId": response.plan_id,
        "graphPreview": response.graph_preview,
        "impact": response.impact,
    }))
    .into_response()
}

#[cfg(not(feature = "agent-plane"))]
async fn agent_graph_diff_preview_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(_request): Json<serde_json::Value>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    agent_plane_unavailable()
}

#[cfg(feature = "agent-execution")]
async fn agent_operation_create_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<crate::agent_execution::OperationCreateRequest>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }

    let plan = build_agent_plan(&state, request.plan_request()).await;
    let pre_alert_count = current_agent_alert_count(&state).await;
    let result = state.agent_execution.create(request, plan, pre_alert_count);
    let status = if result.reused {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    (status, Json(result.operation)).into_response()
}

#[cfg(not(feature = "agent-execution"))]
async fn agent_operation_create_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(_request): Json<serde_json::Value>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    agent_execution_unavailable()
}

#[cfg(feature = "agent-execution")]
async fn agent_operation_get_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(operation_id): Path<String>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    match state.agent_execution.get(&operation_id) {
        Some(record) => Json(crate::agent_execution::public_record(&record)).into_response(),
        None => (StatusCode::NOT_FOUND, "Operation not found").into_response(),
    }
}

#[cfg(not(feature = "agent-execution"))]
async fn agent_operation_get_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(_operation_id): Path<String>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    agent_execution_unavailable()
}

#[cfg(feature = "agent-execution")]
async fn agent_operation_approve_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(operation_id): Path<String>,
    Json(request): Json<crate::agent_execution::ApprovalRequest>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    match state.agent_execution.approve(&operation_id, request) {
        Ok(record) => Json(crate::agent_execution::public_record(&record)).into_response(),
        Err(err) => agent_operation_store_error(err),
    }
}

#[cfg(not(feature = "agent-execution"))]
async fn agent_operation_approve_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(_operation_id): Path<String>,
    Json(_request): Json<serde_json::Value>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    agent_execution_unavailable()
}

#[cfg(feature = "agent-execution")]
async fn agent_operation_apply_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(operation_id): Path<String>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }

    let record = match state.agent_execution.start_apply(&operation_id) {
        Ok(record) => record,
        Err(err) => return agent_operation_store_error(err),
    };

    match execute_agent_operation(&state, &record).await {
        Ok(result) => match state.agent_execution.complete_apply(
            &operation_id,
            result.state_transitions,
            result.progress_snapshots,
            result.execution_result,
        ) {
            Some(record) => Json(crate::agent_execution::public_record(&record)).into_response(),
            None => (StatusCode::NOT_FOUND, "Operation not found").into_response(),
        },
        Err(err) => match state.agent_execution.fail_apply(&operation_id, err.clone()) {
            Some(record) => (
                StatusCode::BAD_REQUEST,
                Json(crate::agent_execution::public_record(&record)),
            )
                .into_response(),
            None => (StatusCode::BAD_REQUEST, err).into_response(),
        },
    }
}

#[cfg(not(feature = "agent-execution"))]
async fn agent_operation_apply_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(_operation_id): Path<String>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    agent_execution_unavailable()
}

#[cfg(feature = "agent-execution")]
async fn agent_operation_verify_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(operation_id): Path<String>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    verify_agent_operation_by_id(&state, &operation_id).await
}

#[cfg(not(feature = "agent-execution"))]
async fn agent_operation_verify_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(_operation_id): Path<String>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    agent_execution_unavailable()
}

#[cfg(feature = "agent-execution")]
async fn agent_verify_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<crate::agent_execution::VerifyRequest>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    verify_agent_operation_by_id(&state, &request.operation_id).await
}

#[cfg(not(feature = "agent-execution"))]
async fn agent_verify_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(_request): Json<serde_json::Value>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }
    agent_execution_unavailable()
}

#[cfg(feature = "agent-plane")]
async fn build_agent_context(state: &AppState) -> serde_json::Value {
    let pipelines = db::list_pipelines(&state.db).await.unwrap_or_default();
    let pipeline_ids: Vec<String> = pipelines.iter().map(|p| p.id.clone()).collect();
    let outputs = db::list_outputs(&state.db).await.unwrap_or_default();
    let jobs = db::list_jobs(&state.db).await.unwrap_or_default();
    let ingests = db::list_ingests(&state.db).await.unwrap_or_default();
    let recording_enabled = recording_enabled_map(state, &pipeline_ids).await;
    let health = state
        .engine
        .health_snapshot(&pipeline_ids, &recording_enabled)
        .await;
    let alerts = alerts::derive_alerts(&health);
    let events = state.engine.recent_events(events::MAX_EVENTS, None);
    let engine_telemetry = state.engine.engine_telemetry().await;
    let mut pipeline_telemetry = Vec::new();
    let mut graphs = Vec::new();
    for pipeline_id in &pipeline_ids {
        pipeline_telemetry.push(state.engine.pipeline_telemetry(pipeline_id).await);
        graphs.push(state.engine.processing_graph(pipeline_id, &outputs).await);
    }
    let desired_vs_actual = agent_desired_vs_actual(
        &pipelines,
        &outputs,
        &ingests,
        &jobs,
        &recording_enabled,
        &health,
    );
    let diagnostics = agent_diagnostics_summary(&pipelines, &outputs, &health, &graphs);
    let dependencies = agent_dependency_summary(
        state,
        &pipelines,
        &outputs,
        &ingests,
        &recording_enabled,
        &health,
    )
    .await;

    let bonding_available = state.engine.bonding_available();
    let (mut status, _) = crate::runtime_info::status_and_sbom(bonding_available);
    let sys = System::new_all();
    status["os"] = system_status(&sys);

    let server_name = db::get_meta(&state.db, "server_name")
        .await
        .unwrap_or(Some("Name".to_string()))
        .unwrap_or("Name".to_string());
    let ingest_host = db::get_ingest_host(&state.db)
        .await
        .unwrap_or_default()
        .unwrap_or_default();
    let custom_encoding_len = db::get_meta(&state.db, "custom_encoding")
        .await
        .ok()
        .flatten()
        .map(|value| value.len())
        .unwrap_or(0);
    let configuration = serde_json::json!({
        "serverName": server_name,
        "ingestHost": ingest_host,
        "ingestSecurity": state.security.get_config(),
        "transcodeProfiles": crate::media::profiles::cache().read().await.clone(),
        "customEncoding": {
            "configured": custom_encoding_len > 0,
            "byteLength": custom_encoding_len,
        },
        "ports": {
            "rtmp": state.ports.rtmp,
            "srt": state.ports.srt,
        }
    });
    let media = agent_media_inventory(state).await;
    let storage = agent_storage_summary(state, &media).await;

    crate::agent_plane::redacted_context(
        &pipelines,
        &outputs,
        &jobs,
        &ingests,
        status,
        health,
        engine_telemetry,
        pipeline_telemetry,
        graphs,
        alerts,
        events,
        configuration,
        media,
        desired_vs_actual,
        diagnostics,
        dependencies,
        storage,
    )
}

#[cfg(feature = "agent-execution")]
struct AgentOperationApplyOutcome {
    state_transitions: Vec<serde_json::Value>,
    progress_snapshots: Vec<serde_json::Value>,
    execution_result: serde_json::Value,
}

#[cfg(feature = "agent-execution")]
async fn execute_agent_operation(
    state: &AppState,
    record: &crate::agent_execution::OperationRecord,
) -> Result<AgentOperationApplyOutcome, String> {
    let request = record.request.plan_request();
    let pipelines = db::list_pipelines(&state.db)
        .await
        .map_err(|err| format!("failed to list pipelines: {err}"))?;
    let outputs = db::list_outputs(&state.db)
        .await
        .map_err(|err| format!("failed to list outputs: {err}"))?;
    let validation = crate::agent_plane::validate_plan(&request, &pipelines, &outputs);
    if !validation.valid {
        return Err(format!(
            "operation plan is no longer valid: {}",
            serde_json::to_string(&validation.errors).unwrap_or_default()
        ));
    }

    let mut state_transitions = Vec::new();
    let mut progress_snapshots = Vec::new();
    let mut change_results = Vec::new();
    let total = request.proposed_changes.len();

    for (idx, change) in request.proposed_changes.iter().enumerate() {
        let pipeline_id = change
            .pipeline_id
            .as_deref()
            .or(request.pipeline_id.as_deref())
            .ok_or_else(|| "change is missing pipelineId".to_string())?;

        let result = apply_agent_change(state, pipeline_id, change).await?;
        state_transitions.push(serde_json::json!({
            "at": chrono::Utc::now().to_rfc3339(),
            "kind": change.kind,
            "pipelineId": pipeline_id,
            "outputId": result["outputId"],
            "from": result["from"],
            "to": result["to"],
        }));
        progress_snapshots.push(serde_json::json!({
            "at": chrono::Utc::now().to_rfc3339(),
            "completed": idx + 1,
            "total": total,
            "currentChange": change.kind,
            "pipelineId": pipeline_id,
            "outputId": result["outputId"],
        }));
        change_results.push(result);
    }

    Ok(AgentOperationApplyOutcome {
        state_transitions,
        progress_snapshots,
        execution_result: crate::agent_plane::redact_secrets(serde_json::json!({
            "success": true,
            "appliedAt": chrono::Utc::now().to_rfc3339(),
            "changeCount": total,
            "changeResults": change_results,
        })),
    })
}

#[cfg(feature = "agent-execution")]
async fn apply_agent_change(
    state: &AppState,
    pipeline_id: &str,
    change: &crate::agent_plane::ProposedChange,
) -> Result<serde_json::Value, String> {
    match change.kind.as_str() {
        "addOutput" => apply_agent_add_output(state, pipeline_id, change).await,
        "updateOutput" => apply_agent_update_output(state, pipeline_id, change).await,
        "removeOutput" => apply_agent_remove_output(state, pipeline_id, change).await,
        "startOutput" => apply_agent_desired_state(state, pipeline_id, change, "running").await,
        "stopOutput" => apply_agent_desired_state(state, pipeline_id, change, "stopped").await,
        other => Err(format!("unsupported change kind '{other}'")),
    }
}

#[cfg(feature = "agent-execution")]
async fn apply_agent_add_output(
    state: &AppState,
    pipeline_id: &str,
    change: &crate::agent_plane::ProposedChange,
) -> Result<serde_json::Value, String> {
    let name = required_change_field(change.name.as_deref(), "name")?;
    let url = required_change_field(change.url.as_deref(), "url")?.trim();
    let monitoring_url = normalize_monitoring_url(change.monitoring_url.as_deref());
    let encoding = change.encoding.as_deref().unwrap_or("source").trim();
    let desired_state = change.desired_state.as_deref().unwrap_or("stopped").trim();
    validate_output_fields(
        name,
        url,
        monitoring_url.as_deref(),
        encoding,
        desired_state,
    )?;

    let output_id = change
        .output_id
        .clone()
        .unwrap_or_else(|| format!("output_agent_{}", to_hex(&rand::random::<[u8; 8]>())));
    let output = db::create_output(
        &state.db,
        &output_id,
        pipeline_id,
        name.trim(),
        url,
        monitoring_url.as_deref(),
        desired_state,
        encoding,
    )
    .await
    .map_err(|err| format!("failed to create output: {err}"))?;

    Ok(serde_json::json!({
        "kind": "addOutput",
        "pipelineId": pipeline_id,
        "outputId": output.id,
        "status": "created",
        "from": null,
        "to": output,
    }))
}

#[cfg(feature = "agent-execution")]
async fn apply_agent_update_output(
    state: &AppState,
    pipeline_id: &str,
    change: &crate::agent_plane::ProposedChange,
) -> Result<serde_json::Value, String> {
    let output_id = required_change_field(change.output_id.as_deref(), "outputId")?;
    let existing = db::get_output(&state.db, pipeline_id, output_id)
        .await
        .map_err(|err| format!("failed to read output: {err}"))?
        .ok_or_else(|| format!("output '{output_id}' not found on pipeline '{pipeline_id}'"))?;
    let name = change.name.as_deref().unwrap_or(&existing.name);
    let url = change.url.as_deref().unwrap_or(&existing.url).trim();
    let monitoring_url = change
        .monitoring_url
        .as_deref()
        .map(str::trim)
        .map(str::to_string)
        .or_else(|| existing.monitoring_url.clone());
    let encoding = change
        .encoding
        .as_deref()
        .unwrap_or(&existing.encoding)
        .trim();
    let desired_state = change
        .desired_state
        .as_deref()
        .unwrap_or(&existing.desired_state)
        .trim();
    validate_output_fields(
        name,
        url,
        monitoring_url.as_deref(),
        encoding,
        desired_state,
    )?;

    let mut updated = db::update_output(
        &state.db,
        pipeline_id,
        output_id,
        name.trim(),
        url,
        monitoring_url.as_deref(),
        encoding,
    )
    .await
    .map_err(|err| format!("failed to update output: {err}"))?
    .ok_or_else(|| format!("output '{output_id}' not found on pipeline '{pipeline_id}'"))?;
    if desired_state != existing.desired_state {
        updated = db::set_output_desired_state(&state.db, pipeline_id, output_id, desired_state)
            .await
            .map_err(|err| format!("failed to update desired state: {err}"))?;
    }

    Ok(serde_json::json!({
        "kind": "updateOutput",
        "pipelineId": pipeline_id,
        "outputId": output_id,
        "status": "updated",
        "from": existing,
        "to": updated,
    }))
}

#[cfg(feature = "agent-execution")]
async fn apply_agent_remove_output(
    state: &AppState,
    pipeline_id: &str,
    change: &crate::agent_plane::ProposedChange,
) -> Result<serde_json::Value, String> {
    let output_id = required_change_field(change.output_id.as_deref(), "outputId")?;
    let existing = db::get_output(&state.db, pipeline_id, output_id)
        .await
        .map_err(|err| format!("failed to read output: {err}"))?
        .ok_or_else(|| format!("output '{output_id}' not found on pipeline '{pipeline_id}'"))?;
    state.engine.unregister_egress(output_id).await;
    let deleted = db::delete_output(&state.db, pipeline_id, output_id)
        .await
        .map_err(|err| format!("failed to delete output: {err}"))?;
    if !deleted {
        return Err(format!(
            "output '{output_id}' not found on pipeline '{pipeline_id}'"
        ));
    }
    Ok(serde_json::json!({
        "kind": "removeOutput",
        "pipelineId": pipeline_id,
        "outputId": output_id,
        "status": "deleted",
        "from": existing,
        "to": null,
    }))
}

#[cfg(feature = "agent-execution")]
async fn apply_agent_desired_state(
    state: &AppState,
    pipeline_id: &str,
    change: &crate::agent_plane::ProposedChange,
    desired_state: &str,
) -> Result<serde_json::Value, String> {
    let output_id = required_change_field(change.output_id.as_deref(), "outputId")?;
    let existing = db::get_output(&state.db, pipeline_id, output_id)
        .await
        .map_err(|err| format!("failed to read output: {err}"))?
        .ok_or_else(|| format!("output '{output_id}' not found on pipeline '{pipeline_id}'"))?;
    let output = db::set_output_desired_state(&state.db, pipeline_id, output_id, desired_state)
        .await
        .map_err(|err| format!("failed to set desired state: {err}"))?;
    Ok(serde_json::json!({
        "kind": change.kind,
        "pipelineId": pipeline_id,
        "outputId": output_id,
        "status": "desiredStateUpdated",
        "from": existing,
        "to": output,
    }))
}

#[cfg(feature = "agent-execution")]
fn required_change_field<'a>(value: Option<&'a str>, field: &str) -> Result<&'a str, String> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("change is missing required field '{field}'"))
}

#[cfg(feature = "agent-execution")]
fn validate_output_fields(
    name: &str,
    url: &str,
    monitoring_url: Option<&str>,
    encoding: &str,
    desired_state: &str,
) -> Result<(), String> {
    validate_len("name", name, MAX_NAME_LEN)?;
    validate_len("url", url, MAX_URL_LEN)?;
    validate_len("encoding", encoding, MAX_ENCODING_LEN)?;
    if let Some(monitoring_url) = monitoring_url {
        validate_len("monitoring_url", monitoring_url, MAX_URL_LEN)?;
    }
    if is_custom_output_encoding(encoding) {
        return Err(CUSTOM_OUTPUT_ENCODING_ERROR.to_string());
    }
    if !is_supported_output_url(url) {
        return Err(OUTPUT_URL_SCHEME_ERROR.to_string());
    }
    if let Some(monitoring_url) = monitoring_url
        && !is_supported_monitoring_url(monitoring_url)
    {
        return Err(MONITORING_URL_SCHEME_ERROR.to_string());
    }
    if !matches!(desired_state, "running" | "stopped") {
        return Err("desiredState must be either 'running' or 'stopped'".to_string());
    }
    Ok(())
}

#[cfg(feature = "agent-execution")]
fn validate_len(field: &str, value: &str, max: usize) -> Result<(), String> {
    if value.len() > max {
        Err(format!("{field} exceeds maximum length of {max} bytes"))
    } else {
        Ok(())
    }
}

#[cfg(feature = "agent-execution")]
async fn verify_agent_operation_by_id(
    state: &AppState,
    operation_id: &str,
) -> axum::response::Response {
    let record = match state.agent_execution.get(operation_id) {
        Some(record) => record,
        None => return (StatusCode::NOT_FOUND, "Operation not found").into_response(),
    };
    let verification = verify_agent_operation(state, &record).await;
    match state
        .agent_execution
        .complete_verify(operation_id, verification)
    {
        Some(record) => Json(crate::agent_execution::public_record(&record)).into_response(),
        None => (StatusCode::NOT_FOUND, "Operation not found").into_response(),
    }
}

#[cfg(feature = "agent-execution")]
async fn verify_agent_operation(
    state: &AppState,
    record: &crate::agent_execution::OperationRecord,
) -> serde_json::Value {
    let pipelines = db::list_pipelines(&state.db).await.unwrap_or_default();
    let pipeline_ids: Vec<String> = pipelines
        .iter()
        .map(|pipeline| pipeline.id.clone())
        .collect();
    let outputs = db::list_outputs(&state.db).await.unwrap_or_default();
    let recording_enabled = recording_enabled_map(state, &pipeline_ids).await;
    let health = state
        .engine
        .health_snapshot(&pipeline_ids, &recording_enabled)
        .await;
    let alerts = alerts::derive_alerts(&health);
    let mut checks = Vec::new();
    let mut success = true;

    for change in &record.request.proposed_changes {
        let pipeline_id = change
            .pipeline_id
            .as_deref()
            .or(record.request.pipeline_id.as_deref())
            .unwrap_or_default();
        let output_id = agent_change_output_id(record, change);
        let output = output_id.as_deref().and_then(|oid| {
            outputs
                .iter()
                .find(|output| output.pipeline_id == pipeline_id && output.id == oid)
        });
        let runtime = output_id
            .as_deref()
            .map(|oid| &health["pipelines"][pipeline_id]["outputs"][oid]);
        let (passed, reason) = match change.kind.as_str() {
            "addOutput" | "updateOutput" => {
                if let Some(output) = output {
                    if let Some(desired) = change.desired_state.as_deref()
                        && output.desired_state != desired
                    {
                        (false, "desiredStateMismatch")
                    } else if change.desired_state.as_deref() == Some("running") {
                        let status = runtime.and_then(|runtime| runtime["status"].as_str());
                        let input_status = health["pipelines"][pipeline_id]["input"]["status"]
                            .as_str()
                            .unwrap_or("off");
                        if status == Some("running") {
                            (true, "running")
                        } else if input_status != "on" {
                            (false, "pendingInput")
                        } else {
                            (false, "notRunning")
                        }
                    } else if change.desired_state.as_deref() == Some("stopped") {
                        let status = runtime.and_then(|runtime| runtime["status"].as_str());
                        if status != Some("running") {
                            (true, "stopped")
                        } else {
                            (false, "stillRunning")
                        }
                    } else {
                        (true, "persisted")
                    }
                } else {
                    (false, "outputMissing")
                }
            }
            "removeOutput" => {
                if output.is_none() {
                    (true, "removed")
                } else {
                    (false, "stillPresent")
                }
            }
            "startOutput" => {
                let status = runtime.and_then(|runtime| runtime["status"].as_str());
                let input_status = health["pipelines"][pipeline_id]["input"]["status"]
                    .as_str()
                    .unwrap_or("off");
                if output.is_some_and(|output| output.desired_state == "running")
                    && status == Some("running")
                {
                    (true, "running")
                } else if input_status != "on" {
                    (false, "pendingInput")
                } else {
                    (false, "notRunning")
                }
            }
            "stopOutput" => {
                let status = runtime.and_then(|runtime| runtime["status"].as_str());
                if output.is_some_and(|output| output.desired_state == "stopped")
                    && status != Some("running")
                {
                    (true, "stopped")
                } else {
                    (false, "stillRunning")
                }
            }
            _ => (false, "unsupportedChangeKind"),
        };
        success &= passed;
        checks.push(serde_json::json!({
            "kind": change.kind,
            "pipelineId": pipeline_id,
            "outputId": output_id,
            "passed": passed,
            "reason": reason,
            "runtime": runtime.cloned().unwrap_or(serde_json::Value::Null),
        }));
    }

    let mut graphs = Vec::new();
    for pipeline_id in &pipeline_ids {
        graphs.push(state.engine.processing_graph(pipeline_id, &outputs).await);
    }
    let active_graph_nodes = graphs
        .iter()
        .filter_map(|graph| graph["nodes"].as_array())
        .flatten()
        .filter(|node| node["active"].as_bool().unwrap_or(false))
        .count();
    let alert_delta = alerts.len() as isize - record.pre_apply_alert_count.unwrap_or(0) as isize;

    crate::agent_plane::redact_secrets(serde_json::json!({
        "success": success,
        "verifiedAt": chrono::Utc::now().to_rfc3339(),
        "postChangeHealth": health,
        "freshnessRecovery": {
            "checked": true,
            "pipelineCount": pipeline_ids.len(),
        },
        "graphConvergence": {
            "checked": true,
            "graphCount": graphs.len(),
            "activeNodes": active_graph_nodes,
        },
        "incidentDelta": {
            "preApplyAlertCount": record.pre_apply_alert_count,
            "postApplyAlertCount": alerts.len(),
            "delta": alert_delta,
        },
        "checks": checks,
        "explanation": if success {
            "All operation checks matched persisted state and runtime expectations."
        } else {
            "One or more operation checks did not match persisted state or runtime expectations."
        },
    }))
}

#[cfg(feature = "agent-execution")]
fn agent_change_output_id(
    record: &crate::agent_execution::OperationRecord,
    change: &crate::agent_plane::ProposedChange,
) -> Option<String> {
    if let Some(output_id) = &change.output_id {
        return Some(output_id.clone());
    }
    let change_results = record
        .execution_result
        .as_ref()
        .and_then(|result| result["changeResults"].as_array())?;
    change_results
        .iter()
        .find(|result| {
            result["kind"].as_str() == Some(change.kind.as_str())
                && result["pipelineId"].as_str()
                    == change
                        .pipeline_id
                        .as_deref()
                        .or(record.request.pipeline_id.as_deref())
        })
        .and_then(|result| result["outputId"].as_str())
        .map(ToOwned::to_owned)
}

#[cfg(feature = "agent-execution")]
async fn current_agent_alert_count(state: &AppState) -> usize {
    let pipelines = db::list_pipelines(&state.db).await.unwrap_or_default();
    let pipeline_ids: Vec<String> = pipelines
        .iter()
        .map(|pipeline| pipeline.id.clone())
        .collect();
    let recording_enabled = recording_enabled_map(state, &pipeline_ids).await;
    let health = state
        .engine
        .health_snapshot(&pipeline_ids, &recording_enabled)
        .await;
    alerts::derive_alerts(&health).len()
}

#[cfg(feature = "agent-plane")]
async fn agent_media_inventory(state: &AppState) -> serde_json::Value {
    let mut files = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&state.media_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if (name.ends_with(".ts")
                || name.ends_with(".mkv")
                || name.ends_with(".mp4")
                || name.ends_with(".mov"))
                && let Ok(metadata) = entry.metadata().await
            {
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .and_then(|d| chrono::DateTime::from_timestamp_millis(d.as_millis() as i64))
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default();

                let ingests = db::list_ingests_for_filename(&state.db, &name)
                    .await
                    .unwrap_or_default();
                let lower_name = name.to_ascii_lowercase();
                let kind = if lower_name.ends_with(".ts") || lower_name.ends_with(".mkv") {
                    "recording"
                } else {
                    "source"
                };
                files.push(serde_json::json!({
                    "name": name,
                    "size": metadata.len(),
                    "modifiedAt": modified,
                    "ingestCount": ingests.len(),
                    "kind": kind
                }));
            }
        }
    }
    serde_json::json!({
        "mediaDir": state.media_dir,
        "files": files,
    })
}

#[cfg(feature = "agent-plane")]
fn agent_desired_vs_actual(
    pipelines: &[Pipeline],
    outputs: &[Output],
    ingests: &[Ingest],
    jobs: &[Job],
    recording_enabled: &std::collections::HashMap<String, bool>,
    health: &serde_json::Value,
) -> serde_json::Value {
    let mut pipeline_reports = Vec::new();
    let mut drift_count = 0usize;
    let mut converged_count = 0usize;
    let mut pending_count = 0usize;

    for pipeline in pipelines {
        let pipeline_health = &health["pipelines"][&pipeline.id];
        let input_status = pipeline_health["input"]["status"].as_str().unwrap_or("off");
        let file_ingests: Vec<_> = ingests
            .iter()
            .filter(|ingest| ingest.stream_key == pipeline.stream_key)
            .collect();
        let input_desired = if file_ingests.is_empty() {
            "externalPublisherOptional"
        } else {
            "fileIngestConfigured"
        };

        let pipeline_outputs: Vec<_> = outputs
            .iter()
            .filter(|output| output.pipeline_id == pipeline.id)
            .collect();
        let mut output_reports = Vec::new();
        for output in pipeline_outputs {
            let runtime = &pipeline_health["outputs"][&output.id];
            let actual = runtime["status"].as_str().unwrap_or("stopped");
            let reason = if output.desired_state == "running" && input_status != "on" {
                pending_count += 1;
                "pendingInput"
            } else if output.desired_state == "running" && actual == "running" {
                converged_count += 1;
                "converged"
            } else if output.desired_state == "stopped" && actual != "running" {
                converged_count += 1;
                "converged"
            } else {
                drift_count += 1;
                "desiredActualMismatch"
            };
            let recent_jobs: Vec<_> = jobs
                .iter()
                .filter(|job| job.pipeline_id == pipeline.id && job.output_id == output.id)
                .take(5)
                .map(crate::agent_plane::redact_secrets_from_serializable)
                .collect();
            output_reports.push(serde_json::json!({
                "outputId": output.id,
                "name": output.name,
                "desiredState": output.desired_state,
                "actualStatus": actual,
                "actualPhase": runtime["phase"],
                "converged": reason == "converged",
                "reason": reason,
                "encoding": output.encoding,
                "recentJobs": recent_jobs,
            }));
        }

        let recording_desired = recording_enabled
            .get(&pipeline.id)
            .copied()
            .unwrap_or(false);
        let recording_active = pipeline_health["recording"]["active"]
            .as_bool()
            .unwrap_or(false);
        let recording_reason = if !recording_desired && !recording_active {
            "converged"
        } else if recording_desired && recording_active {
            "converged"
        } else if recording_desired && input_status != "on" {
            "pendingInput"
        } else {
            "desiredActualMismatch"
        };

        pipeline_reports.push(serde_json::json!({
            "pipelineId": pipeline.id,
            "name": pipeline.name,
            "input": {
                "desired": input_desired,
                "actualStatus": input_status,
                "fileIngestCount": file_ingests.len(),
                "externalPublishersAllowed": true
            },
            "outputs": output_reports,
            "recording": {
                "desiredEnabled": recording_desired,
                "actualActive": recording_active,
                "converged": recording_reason == "converged",
                "reason": recording_reason
            },
            "hlsPreview": {
                "desired": "onDemand",
                "actualActive": pipeline_health["hlsPreview"]["active"].as_bool().unwrap_or(false)
            }
        }));
    }

    serde_json::json!({
        "summary": {
            "pipelines": pipelines.len(),
            "outputs": outputs.len(),
            "convergedOutputs": converged_count,
            "pendingOutputs": pending_count,
            "driftedOutputs": drift_count,
        },
        "pipelines": pipeline_reports,
    })
}

#[cfg(feature = "agent-plane")]
fn agent_diagnostics_summary(
    pipelines: &[Pipeline],
    outputs: &[Output],
    health: &serde_json::Value,
    graphs: &[serde_json::Value],
) -> serde_json::Value {
    let mut pipeline_reports = Vec::new();
    for pipeline in pipelines {
        let pipeline_health = &health["pipelines"][&pipeline.id];
        let graph = graphs
            .iter()
            .find(|graph| graph["pipelineId"].as_str() == Some(pipeline.id.as_str()));
        let inactive_nodes = graph
            .and_then(|graph| graph["nodes"].as_array())
            .map(|nodes| {
                nodes
                    .iter()
                    .filter(|node| !node["active"].as_bool().unwrap_or(false))
                    .map(|node| {
                        serde_json::json!({
                            "id": node["id"],
                            "type": node["type"],
                            "label": node["label"],
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let desired_running_outputs = outputs
            .iter()
            .filter(|output| output.pipeline_id == pipeline.id && output.desired_state == "running")
            .count();
        let actual_running_outputs = pipeline_health["outputs"]
            .as_object()
            .map(|outputs| {
                outputs
                    .values()
                    .filter(|output| output["status"].as_str() == Some("running"))
                    .count()
            })
            .unwrap_or(0);
        let mut findings = Vec::new();
        if pipeline_health["input"]["status"].as_str() != Some("on") {
            findings.push(serde_json::json!({
                "severity": "critical",
                "code": "noActivePublisher",
                "message": "Pipeline has no active publisher."
            }));
        }
        if actual_running_outputs < desired_running_outputs {
            findings.push(serde_json::json!({
                "severity": "warning",
                "code": "desiredOutputsNotRunning",
                "message": "One or more desired running outputs are not active.",
                "desiredRunningOutputs": desired_running_outputs,
                "actualRunningOutputs": actual_running_outputs
            }));
        }
        pipeline_reports.push(serde_json::json!({
            "pipelineId": pipeline.id,
            "passive": true,
            "activeProbeEndpoint": format!("/api/v1/pipelines/{}/diagnostics", pipeline.id),
            "supportedProbeQueryValues": ["rtmp", "srt"],
            "includedActiveProbeResults": false,
            "reason": "The context endpoint is read-only and does not open active SSE diagnostics probes.",
            "inactiveGraphNodes": inactive_nodes,
            "findings": findings,
        }));
    }

    serde_json::json!({
        "streamingEndpointTemplate": "/api/v1/pipelines/:pipeline_id/diagnostics?probe=:probe",
        "includedActiveProbeResults": false,
        "pipelines": pipeline_reports,
    })
}

#[cfg(feature = "agent-plane")]
async fn agent_dependency_summary(
    state: &AppState,
    pipelines: &[Pipeline],
    outputs: &[Output],
    ingests: &[Ingest],
    recording_enabled: &std::collections::HashMap<String, bool>,
    health: &serde_json::Value,
) -> serde_json::Value {
    let hls_config = crate::media::hls::HlsConfig::from_env();
    let mut hls = Vec::new();
    let mut recordings = Vec::new();
    for pipeline in pipelines {
        let snapshot = state.engine.hls_dependency_snapshot(&pipeline.id).await;
        hls.push(serde_json::json!({
            "pipelineId": pipeline.id,
            "storeExists": snapshot.store_exists,
            "active": snapshot.active,
            "persistentConsumers": snapshot.persistent_consumers,
            "lastAccessAgeMs": snapshot.last_access_age_ms,
            "segments": snapshot.segments,
            "playlistBytes": snapshot.playlist_bytes,
        }));

        let desired_enabled = recording_enabled
            .get(&pipeline.id)
            .copied()
            .unwrap_or(false);
        let active = state.engine.is_recording_active(&pipeline.id).await;
        recordings.push(serde_json::json!({
            "pipelineId": pipeline.id,
            "desiredEnabled": desired_enabled,
            "active": active,
            "inputStatus": health["pipelines"][&pipeline.id]["input"]["status"],
        }));
    }

    let mut file_ingest = Vec::new();
    for ingest in ingests {
        let media_path = FsPath::new(&state.media_dir).join(&ingest.filename);
        let runtime = state
            .engine
            .file_ingest_dependency_snapshot(&ingest.id)
            .await;
        file_ingest.push(serde_json::json!({
            "id": ingest.id,
            "filename": ingest.filename,
            "mediaExists": media_path.exists(),
            "markedActive": runtime.marked_active,
            "childRegistered": runtime.child_registered,
            "backend": if crate::media::file_ingest::use_internal_file_ingest() { "internal" } else { "ffmpeg-subprocess" },
            "loop": ingest.loop_flag,
            "startTime": ingest.start_time,
            "streamKey": ingest.stream_key,
        }));
    }

    let hls_output_count = outputs
        .iter()
        .filter(|output| {
            output.url.starts_with("hls://")
                || output.url.starts_with("http://")
                || output.url.starts_with("https://")
        })
        .count();

    serde_json::json!({
        "hls": {
            "config": {
                "minSegmentSecs": hls_config.min_segment_secs,
                "segmentCapacity": hls_config.segment_capacity,
                "maxSegments": hls_config.max_segments,
            },
            "outputCount": hls_output_count,
            "pipelines": hls,
        },
        "recording": {
            "pipelines": recordings,
        },
        "fileIngest": {
            "configured": file_ingest.len(),
            "backend": if crate::media::file_ingest::use_internal_file_ingest() { "internal" } else { "ffmpeg-subprocess" },
            "ingests": file_ingest,
        },
        "ingestSecurity": {
            "config": state.security.get_config(),
            "loopbackExempt": true,
            "trackedIpRuntimeStateRedacted": true,
        }
    })
}

#[cfg(feature = "agent-plane")]
async fn agent_storage_summary(state: &AppState, media: &serde_json::Value) -> serde_json::Value {
    let media_bytes = media["files"]
        .as_array()
        .map(|files| {
            files
                .iter()
                .filter_map(|file| file["size"].as_u64())
                .sum::<u64>()
        })
        .unwrap_or(0);
    let media_file_count = media["files"]
        .as_array()
        .map(|files| files.len())
        .unwrap_or(0);
    let media_root = std::fs::canonicalize(&state.media_dir)
        .ok()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| state.media_dir.clone());

    let disks = Disks::new_with_refreshed_list();
    let mut selected_disk = None;
    for disk in disks.list() {
        if FsPath::new(&media_root).starts_with(disk.mount_point()) {
            selected_disk = Some(serde_json::json!({
                "mountPoint": disk.mount_point().display().to_string(),
                "totalBytes": disk.total_space(),
                "availableBytes": disk.available_space(),
            }));
        }
    }

    serde_json::json!({
        "mediaDir": state.media_dir,
        "mediaRoot": media_root,
        "mediaFileCount": media_file_count,
        "mediaBytes": media_bytes,
        "disk": selected_disk,
        "databasePath": std::env::var("RESTREAM_DB_PATH").unwrap_or_else(|_| "data.db".to_string()),
    })
}

#[cfg(feature = "agent-plane")]
async fn build_agent_plan(
    state: &AppState,
    request: crate::agent_plane::PlanRequest,
) -> crate::agent_plane::PlanResponse {
    let pipelines = db::list_pipelines(&state.db).await.unwrap_or_default();
    let outputs = db::list_outputs(&state.db).await.unwrap_or_default();
    let current_graph = if let Some(pid) = request.pipeline_id.as_deref()
        && pipelines.iter().any(|p| p.id == pid)
    {
        Some(state.engine.processing_graph(pid, &outputs).await)
    } else {
        None
    };
    crate::agent_plane::plan_response(request, &pipelines, &outputs, current_graph.as_ref())
}

#[cfg(feature = "agent-plane")]
async fn recording_enabled_map(
    state: &AppState,
    pipeline_ids: &[String],
) -> std::collections::HashMap<String, bool> {
    let mut recording_enabled = std::collections::HashMap::new();
    for pid in pipeline_ids {
        let rec_key = format!("recording_enabled:{}", pid);
        let rec = db::get_meta(&state.db, &rec_key)
            .await
            .ok()
            .flatten()
            .map(|v| v == "1")
            .unwrap_or(false);
        recording_enabled.insert(pid.clone(), rec);
    }
    recording_enabled
}

#[cfg(not(feature = "agent-plane"))]
fn agent_plane_unavailable() -> axum::response::Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": "agent-plane feature is not compiled in",
            "feature": "agent-plane",
            "compiledIn": false
        })),
    )
        .into_response()
}

#[cfg(feature = "agent-execution")]
fn agent_operation_store_error(
    err: crate::agent_execution::OperationStoreError,
) -> axum::response::Response {
    use crate::agent_execution::OperationStoreError;

    let (status, code, message) = match err {
        OperationStoreError::NotFound => (
            StatusCode::NOT_FOUND,
            "operationNotFound",
            "Operation not found",
        ),
        OperationStoreError::Invalid => (
            StatusCode::CONFLICT,
            "operationInvalid",
            "Operation is invalid and cannot advance",
        ),
        OperationStoreError::RequiresApproval => (
            StatusCode::CONFLICT,
            "approvalRequired",
            "Operation must be approved before apply",
        ),
        OperationStoreError::AlreadyApplying => (
            StatusCode::CONFLICT,
            "alreadyApplying",
            "Operation is already applying",
        ),
        OperationStoreError::AlreadyTerminal => (
            StatusCode::CONFLICT,
            "alreadyTerminal",
            "Operation has already reached a terminal state",
        ),
    };
    (
        status,
        Json(serde_json::json!({
            "error": message,
            "code": code,
        })),
    )
        .into_response()
}

#[cfg(not(feature = "agent-execution"))]
fn agent_execution_unavailable() -> axum::response::Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": "agent-execution feature is not compiled in",
            "feature": "agent-execution",
            "compiledIn": false
        })),
    )
        .into_response()
}

/// Operator pipeline summary: source state, output rollup, alert list.
/// Auth required. Returns a focused view without raw graph or queue data.
async fn v1_pipeline_summary_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(pipeline_id): Path<String>,
) -> impl IntoResponse {
    if let Some(response) = require_authenticated(&state, &headers).await {
        return response;
    }

    let rec_key = format!("recording_enabled:{}", pipeline_id);
    let rec = db::get_meta(&state.db, &rec_key)
        .await
        .ok()
        .flatten()
        .map(|v| v == "1")
        .unwrap_or(false);
    let mut recording_enabled = std::collections::HashMap::new();
    recording_enabled.insert(pipeline_id.clone(), rec);

    let snapshot = state
        .engine
        .health_snapshot(std::slice::from_ref(&pipeline_id), &recording_enabled)
        .await;

    let generated_at = snapshot["generatedAt"].as_str().unwrap_or("").to_string();

    // Verify the pipeline exists in the DB before using snapshot data.
    // health_snapshot always produces an entry for every requested pipeline_id
    // (with input.status=off) even if the pipeline doesn't exist in the DB.
    let exists = db::list_pipelines(&state.db)
        .await
        .unwrap_or_default()
        .iter()
        .any(|p| p.id == pipeline_id);
    if !exists {
        return (StatusCode::NOT_FOUND, "Pipeline not found").into_response();
    }

    let pip = &snapshot["pipelines"][&pipeline_id];
    let pipeline_outputs = db::list_outputs(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|output| output.pipeline_id == pipeline_id)
        .collect::<Vec<_>>();
    let graph = state
        .engine
        .processing_graph(&pipeline_id, &pipeline_outputs)
        .await;
    let graph_nodes = graph["nodes"]
        .as_array()
        .map(|nodes| nodes.len())
        .unwrap_or(0);
    let graph_edges = graph["edges"]
        .as_array()
        .map(|edges| edges.len())
        .unwrap_or(0);
    let graph_active_nodes = graph["nodes"]
        .as_array()
        .map(|nodes| {
            nodes
                .iter()
                .filter(|node| node["active"].as_bool().unwrap_or(false))
                .count()
        })
        .unwrap_or(0);

    let mut alert_list = alerts::derive_alerts(&snapshot);
    state
        .alert_tracker
        .track_pipeline(&pipeline_id, &mut alert_list);

    let input_status = pip["input"]["status"].as_str().unwrap_or("off");
    let bitrate_kbps = pip["input"]["bitrateKbps"].as_f64();

    let outputs = pip["outputs"].as_object().map(|map| {
        map.iter()
            .map(|(id, v)| {
                serde_json::json!({
                    "id": id,
                    "status": v["status"].as_str().unwrap_or("unknown"),
                    "bitrateKbps": v["bitrateKbps"],
                })
            })
            .collect::<Vec<_>>()
    });

    let total_outputs = outputs.as_ref().map(|o| o.len()).unwrap_or(0);
    let running_outputs = outputs
        .as_ref()
        .map(|o| {
            o.iter()
                .filter(|v| v["status"].as_str() == Some("running"))
                .count()
        })
        .unwrap_or(0);

    Json(serde_json::json!({
        "generatedAt": generated_at,
        "pipelineId": pipeline_id,
        "input": pip["input"],
        "source": {
            "status": input_status,
            "bitrateKbps": bitrate_kbps,
            "protocol": pip["input"]["publisher"]["protocol"],
            "readers": pip["input"]["readers"],
        },
        "outputs": {
            "total": total_outputs,
            "running": running_outputs,
            "list": outputs,
        },
        "recording": pip["recording"],
        "hlsPreview": pip["hlsPreview"],
        "graph": {
            "nodes": graph_nodes,
            "edges": graph_edges,
            "activeNodes": graph_active_nodes,
            "inactiveNodes": graph_nodes.saturating_sub(graph_active_nodes),
            "hasGraph": graph_nodes > 0,
        },
        "alerts": alert_list,
    }))
    .into_response()
}

async fn pipeline_diagnostics_sse_handler(
    State(state): State<Arc<AppState>>,
    Path(pipeline_id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<DiagnosticsQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let probe_protocol = match state
        .engine
        .active_ingest_protocol_for_probe(&pipeline_id)
        .await
    {
        Some(protocol) => protocol,
        None => {
            return (StatusCode::NOT_FOUND, "No active ingest for this pipeline").into_response();
        }
    };

    if let Some(requested_protocol) = query.probe
        && requested_protocol != probe_protocol
    {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "Probe protocol must match active ingest protocol ({})",
                probe_protocol
            ),
        )
            .into_response();
    }
    let engine = state.engine.clone();

    // Acquire per-pipeline semaphore to prevent concurrent diagnostics on the same pipeline
    let sem = engine.get_or_create_diag_semaphore(&pipeline_id).await;
    let permit = match sem.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                "A diagnostic is already running for this pipeline",
            )
                .into_response();
        }
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<String>(32);
    tokio::spawn(async move {
        let _permit = permit;
        diag::run_diagnostics(engine, pipeline_id, probe_protocol, tx).await;
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = axum::body::Body::from_stream(futures_util::StreamExt::map(stream, |s| {
        Ok::<_, std::convert::Infallible>(s)
    }));

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        body,
    )
        .into_response()
}

async fn recording_start_handler(
    State(state): State<Arc<AppState>>,
    Path(pipeline_id): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let pipeline = match db::get_pipeline(&state.db, &pipeline_id).await {
        Ok(Some(p)) => p,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Pipeline not found"})),
            )
                .into_response();
        }
    };

    let meta_key = format!("recording_enabled:{}", pipeline_id);
    let _ = db::set_meta(&state.db, &meta_key, "1").await;

    let has_ingest = state
        .engine
        .ingests
        .active
        .read()
        .await
        .contains_key(&pipeline_id);
    if has_ingest && !state.engine.is_recording_active(&pipeline_id).await {
        let ring_buf = state.engine.get_or_create_pipeline(&pipeline_id).await;
        let cancel_token = state.engine.register_recording(&pipeline_id).await;
        let engine = state.engine.clone();
        let pid = pipeline_id.clone();
        let pipe_name = pipeline.name.clone();
        let input_source = pipeline.input_source.clone();
        let engine_rec = engine.clone();
        let media_dir = state.media_dir.clone();
        tokio::spawn(async move {
            crate::media::recording::start_recording(
                pipe_name,
                pid.clone(),
                input_source,
                media_dir,
                ring_buf,
                engine_rec,
                cancel_token,
            )
            .await;
            engine.unregister_recording(&pid).await;
        });
    }

    let active = state.engine.is_recording_active(&pipeline_id).await;
    Json(serde_json::json!({ "enabled": true, "active": active })).into_response()
}

async fn recording_stop_handler(
    State(state): State<Arc<AppState>>,
    Path(pipeline_id): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    match db::get_pipeline(&state.db, &pipeline_id).await {
        Ok(Some(_)) => {}
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Pipeline not found"})),
            )
                .into_response();
        }
    };

    let meta_key = format!("recording_enabled:{}", pipeline_id);
    let _ = db::set_meta(&state.db, &meta_key, "0").await;

    state.engine.unregister_recording(&pipeline_id).await;

    Json(serde_json::json!({ "enabled": false, "active": false })).into_response()
}

async fn get_or_start_hls_store(
    state: &Arc<AppState>,
    pipeline_id: &str,
) -> Result<Arc<HlsStore>, Response> {
    let has_ingest = state
        .engine
        .ingests
        .active
        .read()
        .await
        .contains_key(pipeline_id);
    if has_ingest {
        let (store, already_running) = state.engine.ensure_hls_segmenter(pipeline_id).await;
        if !already_running {
            let engine_c = state.engine.clone();
            let pid = pipeline_id.to_string();
            let ring_buf = state.engine.get_or_create_pipeline(pipeline_id).await;
            let cancel_token = state
                .engine
                .get_hls_cancel_token(pipeline_id)
                .await
                .unwrap();
            let store_c = store.clone();
            tokio::spawn(async move {
                crate::media::hls::start_hls_segmenter(
                    pid.clone(),
                    store_c,
                    ring_buf,
                    engine_c.clone(),
                    cancel_token,
                )
                .await;
                engine_c.shutdown_hls_segmenter(&pid).await;
            });
        }
        state.engine.touch_hls(pipeline_id).await;
        return Ok(store);
    }

    let Some(store) = state.engine.get_hls_store(pipeline_id).await else {
        return Err((StatusCode::NOT_FOUND, "No HLS stream").into_response());
    };
    state.engine.touch_hls(pipeline_id).await;
    Ok(store)
}

fn quote_hls_attr(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn build_hls_master_playlist(
    video: Option<&crate::media::engine::VideoMeta>,
    audio_tracks: &[crate::media::engine::AudioMeta],
) -> String {
    let mut playlist = "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-INDEPENDENT-SEGMENTS\n".to_string();
    let bandwidth = estimate_hls_master_bandwidth(video, audio_tracks);
    let mut stream_attrs = vec![
        format!("BANDWIDTH={bandwidth}"),
        format!("AVERAGE-BANDWIDTH={bandwidth}"),
    ];
    if let Some(video) = video {
        if video.width > 0 && video.height > 0 {
            stream_attrs.push(format!("RESOLUTION={}x{}", video.width, video.height));
        }
        if video.fps.is_finite() && video.fps > 0.0 {
            stream_attrs.push(format!("FRAME-RATE={:.3}", video.fps));
        }
    }
    if let Some(codecs) = build_hls_codec_list(video, audio_tracks) {
        stream_attrs.push(format!("CODECS={}", quote_hls_attr(&codecs)));
    }
    playlist.push_str(&format!("#EXT-X-STREAM-INF:{}\n", stream_attrs.join(",")));
    playlist.push_str("index.m3u8\n");
    playlist
}

fn estimate_hls_master_bandwidth(
    video: Option<&crate::media::engine::VideoMeta>,
    audio_tracks: &[crate::media::engine::AudioMeta],
) -> u64 {
    let video_bw = video
        .and_then(|meta| meta.bw)
        .filter(|bw| bw.is_finite() && *bw > 0.0)
        .map(|bw| bw.round() as u64);
    let audio_bw = audio_tracks
        .iter()
        .map(estimate_audio_bandwidth)
        .sum::<u64>();
    let fallback_bw = 8_000_000u64.saturating_add(audio_bw);
    video_bw
        .map(|bw| bw.saturating_add(audio_bw))
        .unwrap_or(fallback_bw)
        .max(1)
}

fn estimate_audio_bandwidth(track: &crate::media::engine::AudioMeta) -> u64 {
    match track.codec.to_ascii_lowercase().as_str() {
        "aac" => match track.channels {
            0 | 1 => 96_000,
            2 => 128_000,
            _ => 192_000,
        },
        "mp3" => 128_000,
        "opus" => match track.channels {
            0 | 1 => 64_000,
            2 => 128_000,
            _ => 160_000,
        },
        _ => 128_000,
    }
}

fn build_hls_codec_list(
    video: Option<&crate::media::engine::VideoMeta>,
    audio_tracks: &[crate::media::engine::AudioMeta],
) -> Option<String> {
    let mut codecs = Vec::new();
    if let Some(video) = video.and_then(build_hls_video_codec) {
        codecs.push(video);
    }
    for codec in audio_tracks.iter().filter_map(build_hls_audio_codec) {
        if !codecs.iter().any(|existing| existing == &codec) {
            codecs.push(codec);
        }
    }
    (!codecs.is_empty()).then(|| codecs.join(","))
}

fn build_hls_video_codec(video: &crate::media::engine::VideoMeta) -> Option<String> {
    let codec = video.codec.trim().to_ascii_lowercase();
    match codec.as_str() {
        "h264" | "avc" => build_h264_codec_string(video),
        "hevc" | "h265" => Some(build_hevc_codec_string(video)),
        "av1" => Some("av01.0.08M.08".to_string()),
        _ => None,
    }
}

fn build_h264_codec_string(video: &crate::media::engine::VideoMeta) -> Option<String> {
    let profile_idc = match video.profile.as_deref().map(str::trim) {
        Some("Baseline") => 66u8,
        Some("Main") => 77u8,
        Some("Extended") => 88u8,
        Some("High") => 100u8,
        Some("High 10") => 110u8,
        Some("High 4:2:2") => 122u8,
        Some("High 4:4:4 Predictive") => 244u8,
        _ => return Some("avc1".to_string()),
    };
    let level = parse_h264_level_idc(video.level.as_deref()).unwrap_or(31);
    Some(format!("avc1.{profile_idc:02x}00{level:02x}"))
}

fn parse_h264_level_idc(level: Option<&str>) -> Option<u8> {
    let level = level?.trim();
    if level.is_empty() {
        return None;
    }
    let (major, minor) = match level.split_once('.') {
        Some((major, minor)) => (major.trim(), minor.trim()),
        None => (level, "0"),
    };
    let major: u8 = major.parse().ok()?;
    let minor: u8 = minor.parse().ok()?;
    Some(major.saturating_mul(10).saturating_add(minor))
}

fn build_hevc_codec_string(video: &crate::media::engine::VideoMeta) -> String {
    let profile = match video.profile.as_deref().map(str::trim) {
        Some("Main") => 1u8,
        Some("Main 10") => 2u8,
        Some("Main Still Picture") => 3u8,
        _ => 1u8,
    };
    let level_tenths = video
        .level
        .as_deref()
        .and_then(parse_h265_level_tenths)
        .unwrap_or(120);
    let general_level_idc = level_tenths.saturating_mul(3);
    format!("hvc1.{profile}.6.L{general_level_idc}.B0")
}

fn parse_h265_level_tenths(level: &str) -> Option<u8> {
    let level = level.trim();
    if level.is_empty() {
        return None;
    }
    let (major, minor) = match level.split_once('.') {
        Some((major, minor)) => (major.trim(), minor.trim()),
        None => (level, "0"),
    };
    let major: u8 = major.parse().ok()?;
    let minor: u8 = minor.parse().ok()?;
    Some(major.saturating_mul(10).saturating_add(minor))
}

fn build_hls_audio_codec(track: &crate::media::engine::AudioMeta) -> Option<String> {
    let codec = track.codec.trim().to_ascii_lowercase();
    match codec.as_str() {
        "aac" => Some(match track.profile.as_deref().map(str::trim) {
            Some("Main") => "mp4a.40.1".to_string(),
            Some("SSR") => "mp4a.40.3".to_string(),
            Some("LTP/Reserved") => "mp4a.40.4".to_string(),
            _ => "mp4a.40.2".to_string(),
        }),
        "mp3" => Some("mp4a.40.34".to_string()),
        "opus" => Some("opus".to_string()),
        _ => None,
    }
}

fn parse_hls_segment_name(segment: &str) -> Option<u64> {
    segment
        .strip_prefix("seg")
        .and_then(|s| s.strip_suffix(".ts"))
        .and_then(|s| s.parse::<u64>().ok())
}

fn hls_variant_segment_response(
    store: &HlsStore,
    index: u64,
    variant: HlsSegmentVariant,
    view: TsSegmentView,
) -> Response {
    if let Some(data) = store.get_variant_segment(index, variant) {
        return (StatusCode::OK, [(header::CONTENT_TYPE, "video/mp2t")], data).into_response();
    }

    let Some(source) = store.get_segment(index) else {
        return (StatusCode::NOT_FOUND, "Segment not found").into_response();
    };
    let (video, audio_tracks) = store.stream_metadata();
    let Some(data) = remux_segment_view(&source, video.as_ref(), &audio_tracks, view) else {
        return (StatusCode::NOT_FOUND, "Variant segment not found").into_response();
    };
    store.put_variant_segment(index, variant, data.clone());
    (StatusCode::OK, [(header::CONTENT_TYPE, "video/mp2t")], data).into_response()
}

async fn hls_playlist_handler(
    State(state): State<Arc<AppState>>,
    Path(pipeline_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(response) = require_hls_access(&state, &headers, &uri).await {
        return response;
    }

    let store = match get_or_start_hls_store(&state, &pipeline_id).await {
        Ok(store) => store,
        Err(response) => return response,
    };
    match store.get_playlist() {
        Some(playlist) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
            playlist,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "No segments yet").into_response(),
    }
}

async fn hls_master_handler(
    State(state): State<Arc<AppState>>,
    Path(pipeline_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(response) = require_hls_access(&state, &headers, &uri).await {
        return response;
    }
    let store = match get_or_start_hls_store(&state, &pipeline_id).await {
        Ok(store) => store,
        Err(response) => return response,
    };
    if store.get_playlist().is_none() {
        return (StatusCode::NOT_FOUND, "No segments yet").into_response();
    }
    let (video, audio_tracks) = store.stream_metadata();
    let playlist = build_hls_master_playlist(video.as_ref(), &audio_tracks);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
        playlist,
    )
        .into_response()
}

async fn hls_video_playlist_handler(
    State(state): State<Arc<AppState>>,
    Path(pipeline_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(response) = require_hls_access(&state, &headers, &uri).await {
        return response;
    }
    let store = match get_or_start_hls_store(&state, &pipeline_id).await {
        Ok(store) => store,
        Err(response) => return response,
    };
    match store.get_playlist() {
        Some(playlist) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
            playlist,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "No segments yet").into_response(),
    }
}

async fn hls_audio_playlist_handler(
    State(state): State<Arc<AppState>>,
    Path((pipeline_id, track_index)): Path<(String, u32)>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(response) = require_hls_access(&state, &headers, &uri).await {
        return response;
    }
    let store = match get_or_start_hls_store(&state, &pipeline_id).await {
        Ok(store) => store,
        Err(response) => return response,
    };
    let (_, audio_tracks) = store.stream_metadata();
    if !audio_tracks
        .iter()
        .any(|track| track.track_index == track_index)
    {
        return (StatusCode::NOT_FOUND, "Audio track not found").into_response();
    }
    match store.get_playlist() {
        Some(playlist) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
            playlist,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "No segments yet").into_response(),
    }
}

async fn hls_segment_handler(
    State(state): State<Arc<AppState>>,
    Path((pipeline_id, segment)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(response) = require_hls_access(&state, &headers, &uri).await {
        return response;
    }

    state.engine.touch_hls(&pipeline_id).await;
    let Some(store) = state.engine.get_hls_store(&pipeline_id).await else {
        return (StatusCode::NOT_FOUND, "No HLS stream").into_response();
    };
    let index = segment
        .strip_prefix("seg")
        .and_then(|s| s.strip_suffix(".ts"))
        .and_then(|s| s.parse::<u64>().ok());
    let Some(index) = index else {
        return (StatusCode::BAD_REQUEST, "Invalid segment name").into_response();
    };
    match store.get_segment(index) {
        Some(data) => {
            (StatusCode::OK, [(header::CONTENT_TYPE, "video/mp2t")], data).into_response()
        }
        None => (StatusCode::NOT_FOUND, "Segment not found").into_response(),
    }
}

async fn hls_video_segment_handler(
    State(state): State<Arc<AppState>>,
    Path((pipeline_id, segment)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(response) = require_hls_access(&state, &headers, &uri).await {
        return response;
    }
    state.engine.touch_hls(&pipeline_id).await;
    let Some(store) = state.engine.get_hls_store(&pipeline_id).await else {
        return (StatusCode::NOT_FOUND, "No HLS stream").into_response();
    };
    let Some(index) = parse_hls_segment_name(&segment) else {
        return (StatusCode::BAD_REQUEST, "Invalid segment name").into_response();
    };
    hls_variant_segment_response(
        &store,
        index,
        HlsSegmentVariant::Video,
        TsSegmentView::Video,
    )
}

async fn hls_audio_segment_handler(
    State(state): State<Arc<AppState>>,
    Path((pipeline_id, track_index, segment)): Path<(String, u32, String)>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(response) = require_hls_access(&state, &headers, &uri).await {
        return response;
    }
    state.engine.touch_hls(&pipeline_id).await;
    let Some(store) = state.engine.get_hls_store(&pipeline_id).await else {
        return (StatusCode::NOT_FOUND, "No HLS stream").into_response();
    };
    let Some(index) = parse_hls_segment_name(&segment) else {
        return (StatusCode::BAD_REQUEST, "Invalid segment name").into_response();
    };
    hls_variant_segment_response(
        &store,
        index,
        HlsSegmentVariant::Audio(track_index),
        TsSegmentView::Audio(track_index),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Regression: issue #3 (Round 5) — API string length limits ---
    #[test]
    fn check_field_len_accepts_within_limit() {
        assert!(check_field_len("name", "hello", MAX_NAME_LEN).is_none());
        assert!(check_field_len("url", "rtmp://host/app/key", MAX_URL_LEN).is_none());
        assert!(check_field_len("encoding", "720p", MAX_ENCODING_LEN).is_none());
    }

    #[test]
    fn monitoring_url_validation_accepts_supported_schemes() {
        assert!(is_supported_monitoring_url(
            "https://example.com/live/master.m3u8"
        ));
        assert!(is_supported_monitoring_url("http://example.com/player"));
        assert!(is_supported_monitoring_url(
            "srt://127.0.0.1:9000?streamid=read:live/demo"
        ));
        assert_eq!(normalize_monitoring_url(Some("   ")), None);
    }

    #[test]
    fn monitoring_url_validation_rejects_unsupported_schemes() {
        assert!(!is_supported_monitoring_url("rtmp://example.com/live/key"));
        assert_eq!(
            normalize_monitoring_url(Some(" https://example.com/watch?v=abc ")),
            Some("https://example.com/watch?v=abc".to_string())
        );
    }

    #[test]
    fn normalize_youtube_watch_url_accepts_live_watch_and_short_urls() {
        assert_eq!(
            normalize_youtube_watch_url("https://www.youtube.com/watch?v=0CD5-dEB8LY"),
            Some("https://www.youtube.com/watch?v=0CD5-dEB8LY".to_string())
        );
        assert_eq!(
            normalize_youtube_watch_url("https://www.youtube.com/live/0CD5-dEB8LY?feature=share"),
            Some("https://www.youtube.com/watch?v=0CD5-dEB8LY".to_string())
        );
        assert_eq!(
            normalize_youtube_watch_url("https://youtu.be/0CD5-dEB8LY"),
            Some("https://www.youtube.com/watch?v=0CD5-dEB8LY".to_string())
        );
        assert_eq!(
            normalize_youtube_watch_url("https://example.com/video"),
            None
        );
    }

    #[test]
    fn parse_youtube_monitoring_status_uses_watch_page_flags() {
        let html = r#"
            <html>
                <head><title>Sample Live - YouTube</title></head>
                <body>
                    {"isLiveContent":true,"isLiveNow":true,"isUpcoming":false}
                </body>
            </html>
        "#;
        let meta = parse_youtube_monitoring_status(
            "https://www.youtube.com/watch?v=0CD5-dEB8LY".to_string(),
            html,
        );
        assert!(meta.live_content);
        assert!(meta.live_now);
        assert!(!meta.upcoming);
        assert_eq!(meta.title.as_deref(), Some("Sample Live"));
    }

    #[test]
    fn hls_master_playlist_points_to_combined_media_playlist() {
        let video = crate::media::engine::VideoMeta {
            codec: "h264".to_string(),
            width: 1920,
            height: 1080,
            fps: 30.0,
            bw: None,
            pid: None,
            language: None,
            title: None,
            profile: Some("High".to_string()),
            level: Some("4.0".to_string()),
            pixel_format: None,
        };
        let audio_tracks = vec![
            crate::media::engine::AudioMeta {
                codec: "aac".to_string(),
                sample_rate: 48000,
                channels: 1,
                channel_layout: None,
                track_index: 0,
                pid: Some(0x101),
                language: Some("eng".to_string()),
                title: None,
                profile: None,
            },
            crate::media::engine::AudioMeta {
                codec: "aac".to_string(),
                sample_rate: 48000,
                channels: 2,
                channel_layout: None,
                track_index: 15,
                pid: Some(0x110),
                language: Some("spa".to_string()),
                title: None,
                profile: None,
            },
        ];

        let playlist = build_hls_master_playlist(Some(&video), &audio_tracks);

        assert!(playlist.starts_with("#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-INDEPENDENT-SEGMENTS\n"));
        assert!(playlist.contains("#EXT-X-STREAM-INF:"));
        assert!(playlist.contains("AVERAGE-BANDWIDTH="));
        assert!(playlist.contains("CODECS=\"avc1.640028,mp4a.40.2\""));
        assert!(playlist.contains("RESOLUTION=1920x1080"));
        assert!(playlist.ends_with("index.m3u8\n"));
    }

    #[test]
    fn hls_codec_list_deduplicates_audio_codecs() {
        let video = crate::media::engine::VideoMeta {
            codec: "h264".to_string(),
            width: 1280,
            height: 720,
            fps: 30.0,
            bw: Some(4_000_000.0),
            pid: None,
            language: None,
            title: None,
            profile: Some("High".to_string()),
            level: Some("3.1".to_string()),
            pixel_format: None,
        };
        let audio_tracks = vec![
            crate::media::engine::AudioMeta {
                codec: "aac".to_string(),
                sample_rate: 48000,
                channels: 2,
                channel_layout: None,
                track_index: 0,
                pid: None,
                language: None,
                title: None,
                profile: Some("LC".to_string()),
            },
            crate::media::engine::AudioMeta {
                codec: "aac".to_string(),
                sample_rate: 48000,
                channels: 2,
                channel_layout: None,
                track_index: 1,
                pid: None,
                language: None,
                title: None,
                profile: Some("LC".to_string()),
            },
        ];

        let codecs = build_hls_codec_list(Some(&video), &audio_tracks);

        assert_eq!(codecs.as_deref(), Some("avc1.64001f,mp4a.40.2"));
    }

    #[test]
    fn check_field_len_rejects_over_limit() {
        let over = "x".repeat(MAX_NAME_LEN + 1);
        assert!(
            check_field_len("name", &over, MAX_NAME_LEN).is_some(),
            "name over {} bytes must be rejected",
            MAX_NAME_LEN
        );

        let over_url = "u".repeat(MAX_URL_LEN + 1);
        assert!(
            check_field_len("url", &over_url, MAX_URL_LEN).is_some(),
            "url over {} bytes must be rejected",
            MAX_URL_LEN
        );

        let over_args = "a".repeat(MAX_FFMPEG_ARGS_LEN + 1);
        assert!(
            check_field_len("ffmpeg_args", &over_args, MAX_FFMPEG_ARGS_LEN).is_some(),
            "ffmpeg_args over {} bytes must be rejected",
            MAX_FFMPEG_ARGS_LEN
        );
    }

    #[test]
    fn check_field_len_accepts_exactly_at_limit() {
        let exact = "x".repeat(MAX_NAME_LEN);
        assert!(
            check_field_len("name", &exact, MAX_NAME_LEN).is_none(),
            "exactly {} bytes must be accepted",
            MAX_NAME_LEN
        );
    }

    #[test]
    fn to_hex_output() {
        assert_eq!(to_hex(&[]), "");
        assert_eq!(to_hex(&[0x00]), "00");
        assert_eq!(to_hex(&[0xFF]), "ff");
        assert_eq!(to_hex(&[0xDE, 0xAD, 0xBE, 0xEF]), "deadbeef");
    }

    #[test]
    fn get_session_token_finds_cookie() {
        use axum::http::header;
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, "session=abc123token".parse().unwrap());
        assert_eq!(
            get_session_token_from_headers(&headers).as_deref(),
            Some("abc123token")
        );
    }

    #[test]
    fn get_session_token_skips_other_cookies() {
        use axum::http::header;
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "other=value; session=mytoken; foo=bar".parse().unwrap(),
        );
        assert_eq!(
            get_session_token_from_headers(&headers).as_deref(),
            Some("mytoken")
        );
    }

    #[test]
    fn get_session_token_no_cookie_header() {
        let headers = HeaderMap::new();
        assert!(get_session_token_from_headers(&headers).is_none());
    }

    #[test]
    fn get_session_token_wrong_cookie_name() {
        use axum::http::header;
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, "wrong=somevalue".parse().unwrap());
        assert!(get_session_token_from_headers(&headers).is_none());
    }

    #[test]
    fn make_session_cookie_format() {
        let cookie = make_session_cookie("token123", 3600);
        assert!(cookie.contains("session=token123"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Max-Age=3600"));
        assert!(cookie.contains("SameSite=Strict"));
    }

    #[test]
    fn clear_session_cookie_expires_immediately() {
        let cookie = clear_session_cookie();
        assert!(cookie.contains("session="));
        assert!(cookie.contains("Max-Age=0"));
        assert!(cookie.contains("HttpOnly"));
    }

    // --- Session token hashing ---

    #[test]
    fn hash_session_token_known_vector() {
        // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(
            hash_session_token(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hash_session_token_different_tokens_produce_different_hashes() {
        let h1 = hash_session_token("token_a");
        let h2 = hash_session_token("token_b");
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_session_token_is_deterministic() {
        let token = "abc1234567890def";
        assert_eq!(hash_session_token(token), hash_session_token(token));
    }

    #[test]
    fn hash_session_token_output_is_64_hex_chars() {
        // SHA-256 produces 32 bytes = 64 hex digits
        let h = hash_session_token("any_token_value");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_session_token_not_equal_to_raw() {
        // Sanity: the hash must differ from the input
        let token = "my_raw_session_token";
        assert_ne!(hash_session_token(token), token);
    }
}
