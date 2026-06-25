//! Axum HTTP API — REST endpoints for the dashboard, pipeline/output CRUD,
//! health monitoring, diagnostics SSE, and embedded frontend asset serving.
//! Static assets are compiled into the binary via `rust-embed` and served with
//! disk-first fallback for development hot-reload.

use axum::extract::DefaultBodyLimit;
use axum::http::HeaderValue;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect},
    routing::{delete, get, post, put},
};
use rust_embed::RustEmbed;
use serde::Deserialize;
use sqlx::SqlitePool;
use std::collections::HashSet;
use std::sync::Arc;
use sysinfo::{Disks, Networks, System};
use tokio::sync::RwLock as TokioRwLock;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::alerts;
use crate::db;
use crate::diag;
use crate::media::engine::MediaEngine;
use crate::media::security::IngestSecurityService;
use crate::types::*;

/// Maximum byte lengths for user-supplied string fields stored in SQLite.
/// These prevent both memory exhaustion and bloated DB rows.
pub const MAX_NAME_LEN: usize = 256;
pub const MAX_URL_LEN: usize = 2048;
pub const MAX_ENCODING_LEN: usize = 512;
pub const MAX_STREAM_KEY_LEN: usize = 256;
pub const MAX_FFMPEG_ARGS_LEN: usize = 4096;
pub const MAX_PASSWORD_LEN: usize = 1024;

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
    pub sessions: Arc<TokioRwLock<HashSet<String>>>,
    pub engine: Arc<MediaEngine>,
    pub ports: PortConfig,
    /// Directory for recordings and file-ingest sources.
    /// Defaults to `"media"`. Override via `RESTREAM_MEDIA_DIR`.
    pub media_dir: String,
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
        .route("/logo.png", get(logo_handler))
        .route("/output.css", get(css_handler))
        .route("/api/auth/login", post(login_post_handler))
        .route("/api/auth/logout", post(logout_handler))
        .route("/api/auth/change-password", post(change_password_handler))
        .route("/audio-caps", get(audio_caps_handler))
        .route(
            "/config",
            get(config_get_handler).patch(config_patch_handler),
        )
        .route("/stream-keys", get(stream_keys_handler))
        .route(
            "/pipelines",
            get(pipelines_get_handler).post(pipelines_post_handler),
        )
        .route(
            "/pipelines/:id",
            post(pipelines_update_handler).delete(pipelines_delete_handler),
        )
        .route(
            "/pipelines/:pipeline_id/outputs",
            post(outputs_create_handler),
        )
        .route(
            "/pipelines/:pipeline_id/outputs/:output_id",
            post(outputs_update_handler).delete(outputs_delete_handler),
        )
        .route(
            "/pipelines/:pipeline_id/outputs/:output_id/start",
            post(outputs_start_handler),
        )
        .route(
            "/pipelines/:pipeline_id/outputs/:output_id/stop",
            post(outputs_stop_handler),
        )
        .route(
            "/pipelines/:pipeline_id/outputs/:output_id/history",
            get(outputs_history_handler),
        )
        .route(
            "/pipelines/:pipeline_id/history",
            get(pipeline_history_handler),
        )
        .route("/pipelines/:pipeline_id/probe", get(pipeline_probe_handler))
        .route("/pipelines/:pipeline_id/graph", get(pipeline_graph_handler))
        .route(
            "/pipelines/:pipeline_id/alerts",
            get(pipeline_alerts_handler),
        )
        .route("/api/v1/alerts", get(aggregate_alerts_handler))
        .route(
            "/pipelines/:pipeline_id/diagnostics",
            get(pipeline_diagnostics_sse_handler),
        )
        .route(
            "/pipelines/:pipeline_id/recording/start",
            post(recording_start_handler),
        )
        .route(
            "/pipelines/:pipeline_id/recording/stop",
            post(recording_stop_handler),
        )
        .route(
            "/encodings/custom",
            get(custom_encoding_get).put(custom_encoding_put),
        )
        .route(
            "/api/ingests",
            get(ingests_get_handler).post(ingests_post_handler),
        )
        .route(
            "/api/ingests/:id",
            put(ingests_update_handler).delete(ingests_delete_handler),
        )
        .route("/api/ingests/:id/start", post(ingests_start_handler))
        .route("/api/ingests/:id/stop", post(ingests_stop_handler))
        .route("/api/status", get(status_get_handler))
        .route("/api/status/sbom", get(status_sbom_get_handler))
        .route("/api/media", get(media_list_handler))
        .route("/api/media/:filename", delete(media_delete_handler))
        // HLS routes are registered with CORS headers in the merged sub-router below.
        // Deprecated compatibility aliases. New clients should use /hls/.
        .route("/health", get(health_get_handler))
        .route("/healthz", get(healthz_get_handler))
        .route("/metrics/system", get(metrics_system_handler))
        .fallback(get(spa_fallback_handler))
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
        // Merged last so the CORS layer only applies to /hls/ and /preview/hls/.
        .merge(
            Router::new()
                .route("/hls/:pipeline_id", get(hls_playlist_handler))
                .route("/hls/:pipeline_id/index.m3u8", get(hls_playlist_handler))
                .route("/hls/:pipeline_id/:segment", get(hls_segment_handler))
                .route("/preview/hls/:pipeline_id", get(hls_playlist_handler))
                .route(
                    "/preview/hls/:pipeline_id/index.m3u8",
                    get(hls_playlist_handler),
                )
                .route(
                    "/preview/hls/:pipeline_id/:segment",
                    get(hls_segment_handler),
                )
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

async fn logo_handler() -> impl IntoResponse {
    serve_embedded("logo.png")
}

async fn css_handler() -> impl IntoResponse {
    serve_embedded("output.css")
}

async fn spa_fallback_handler(uri: axum::http::Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    if !path.is_empty() && path.contains('.') {
        return serve_embedded(path).into_response();
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
            eprintln!("[logout] Failed to delete session from DB: {}", e);
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

    let mut pipelines = Vec::new();
    for p in raw_pipelines {
        pipelines.push(serde_json::json!({
            "id": p.id,
            "name": p.name,
            "streamKey": p.stream_key,
            "inputSource": p.input_source,
            "encoding": p.encoding,
            "ingestUrls": {
                "rtmp": format!("rtmp://{}:{}/live/{}", effective_ingest_host, state.ports.rtmp, p.stream_key),
                "srt": format!("srt://{}:{}?streamid=publish:live/{}", effective_ingest_host, state.ports.srt, p.stream_key)
            }
        }));
    }

    let outputs = db::list_outputs(&state.db).await.unwrap_or_default();
    let jobs = db::list_jobs(&state.db).await.unwrap_or_default();
    let server_name = db::get_meta(&state.db, "server_name")
        .await
        .unwrap_or(Some("Name".to_string()))
        .unwrap_or("Name".to_string());
    let sec = state.security.get_config();

    // Transcode profiles from runtime cache
    let transcode_profiles = crate::media::profiles::cache().read().await.clone();

    Json(serde_json::json!({
        "serverName": server_name,
        "ingestHost": ingest_host,
        "ingestSecurity": sec,
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
            eprintln!("[api] Failed to save transcode profiles: {e}");
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
    let transcode_profiles = crate::media::profiles::cache().read().await.clone();

    Json(serde_json::json!({
        "serverName": server_name,
        "ingestHost": ingest_host,
        "ingestSecurity": sec,
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
        Ok(pipelines) => Json(pipelines).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PipelinePayload {
    name: String,
    stream_key: Option<String>,
    input_source: Option<String>,
    encoding: Option<String>,
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
    if let Some(ref e) = payload.encoding
        && let Some(r) = check_field_len("encoding", e, MAX_ENCODING_LEN)
    {
        return r;
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

    match db::create_pipeline(
        &state.db,
        &id,
        &payload.name,
        &stream_key,
        payload.input_source.as_deref(),
        payload.encoding.as_deref(),
    )
    .await
    {
        Ok(pipeline) => (
            StatusCode::CREATED,
            Json(serde_json::json!({"message": "Pipeline created", "pipeline": pipeline})),
        )
            .into_response(),
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

    let existing = match db::get_pipeline(&state.db, &id).await {
        Ok(Some(p)) => p,
        _ => return (StatusCode::NOT_FOUND, "Pipeline not found").into_response(),
    };

    let stream_key = payload.stream_key.unwrap_or(existing.stream_key);
    let input_source = payload.input_source.or(existing.input_source);
    let encoding = payload.encoding.or(existing.encoding);

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
    )
    .await
    {
        Ok(Some(updated)) => {
            Json(serde_json::json!({"message": "Pipeline updated", "pipeline": updated}))
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
        let mut children = state.engine.file_ingest_children.write().await;
        for ingest in ingests
            .iter()
            .filter(|i| i.stream_key == pipeline.stream_key)
        {
            if let Some(mut child) = children.remove(&ingest.id) {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
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
            Json(serde_json::json!({"message": format!("Pipeline {} deleted", id)})).into_response()
        }
        _ => (StatusCode::NOT_FOUND, "Pipeline not found").into_response(),
    }
}

#[derive(Deserialize)]
struct OutputPayload {
    name: String,
    url: String,
    encoding: String,
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
const CUSTOM_OUTPUT_ENCODING_ERROR: &str =
    "Custom output encoding is not available yet; choose source or a preset encoding";

fn is_custom_output_encoding(encoding: &str) -> bool {
    encoding
        .split('+')
        .next()
        .map(|video| video.trim().eq_ignore_ascii_case("custom"))
        .unwrap_or(false)
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

    let id = format!("output_{}", to_hex(&rand::random::<[u8; 8]>()));

    match db::create_output(
        &state.db,
        &id,
        &pipeline_id,
        &payload.name,
        &payload.url,
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

    match db::update_output(
        &state.db,
        &pipeline_id,
        &output_id,
        &payload.name,
        &payload.url,
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

async fn outputs_history_handler(
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

    let logs = db::list_job_logs_by_output(&state.db, &pipeline_id, &output_id)
        .await
        .unwrap_or_default();
    Json(serde_json::json!({
        "pipelineId": pipeline_id,
        "outputId": output_id,
        "logs": logs
    }))
    .into_response()
}

async fn pipeline_history_handler(
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

    let logs = db::list_job_logs_by_pipeline(&state.db, &pipeline_id)
        .await
        .unwrap_or_default();
    Json(serde_json::json!({
        "pipelineId": pipeline_id,
        "logs": logs
    }))
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
    loop_flag: Option<bool>,
    start_time: Option<String>,
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

    let mut children = state.engine.file_ingest_children.write().await;
    if let Some(mut child) = children.remove(&id) {
        let _ = child.kill().await;
        // Reap the child so it does not linger as a zombie process.
        let _ = child.wait().await;
    }
    drop(children);

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

    // Check if already running
    if state
        .engine
        .file_ingest_children
        .read()
        .await
        .contains_key(&id)
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "Ingest already running"})),
        )
            .into_response();
    }

    let file_path = format!("media/{}", ingest.filename);
    if !std::path::Path::new(&file_path).exists() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Media file not found"})),
        )
            .into_response();
    }

    let rtmp_url = format!(
        "rtmp://localhost:{}/live/{}",
        state.ports.rtmp, ingest.stream_key
    );
    let mut args: Vec<String> = vec![
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
    args.extend([
        "-i".into(),
        file_path,
        "-map".into(),
        "0".into(),
        "-c".into(),
        "copy".into(),
        "-flvflags".into(),
        "no_duration_filesize".into(),
        "-f".into(),
        "flv".into(),
        rtmp_url,
    ]);

    match tokio::process::Command::new("ffmpeg")
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => {
            state
                .engine
                .file_ingest_children
                .write()
                .await
                .insert(id.clone(), child);
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
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to spawn ffmpeg: {}", e)})),
        )
            .into_response(),
    }
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

    let mut children = state.engine.file_ingest_children.write().await;
    if let Some(mut child) = children.remove(&id) {
        let _ = child.kill().await;
        // Reap the child so it does not linger as a zombie process.
        let _ = child.wait().await;
    }

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
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let sys = System::new_all();
    let bonding_available = state
        .engine
        .srt_listener_stats
        .bonding_available
        .load(std::sync::atomic::Ordering::Relaxed);
    let (mut status, _) = crate::runtime_info::status_and_sbom(&state.db, bonding_available).await;
    status["os"] = serde_json::json!({
        "platform": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "hostname": System::host_name().unwrap_or_default(),
        "kernelVersion": System::kernel_version(),
        "uptime": System::uptime(),
        "totalMem": sys.total_memory(),
    });

    Json(status).into_response()
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

    let bonding_available = state
        .engine
        .srt_listener_stats
        .bonding_available
        .load(std::sync::atomic::Ordering::Relaxed);
    let (_, sbom) = crate::runtime_info::status_and_sbom(&state.db, bonding_available).await;
    (
        [(
            header::CONTENT_TYPE,
            "application/vnd.cyclonedx+json; version=1.5",
        )],
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
            if (name.ends_with(".mkv") || name.ends_with(".mp4") || name.ends_with(".mov"))
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
                files.push(serde_json::json!({
                    "name": name,
                    "size": metadata.len(),
                    "modifiedAt": modified,
                    "ingestCount": ingests.len()
                }));
            }
        }
    }

    Json(serde_json::json!({ "files": files })).into_response()
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

    let _ = std::fs::create_dir_all(&state.media_dir);
    let media_root = match std::fs::canonicalize(&state.media_dir) {
        Ok(p) => p,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "Media directory error").into_response();
        }
    };
    let path = std::path::Path::new(&state.media_dir).join(&filename);
    let canonical_path = match std::fs::canonicalize(&path) {
        Ok(p) => p,
        Err(_) => return (StatusCode::NOT_FOUND, "File not found").into_response(),
    };
    if !canonical_path.starts_with(&media_root) {
        return (StatusCode::BAD_REQUEST, "Invalid path").into_response();
    }

    match tokio::fs::remove_file(canonical_path).await {
        Ok(_) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "File not found").into_response(),
    }
}

async fn health_get_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
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
    Json(snapshot)
}

async fn healthz_get_handler() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
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

    let disks = Disks::new_with_refreshed_list();
    let (total_disk, used_disk) = disks.iter().fold((0u64, 0u64), |(t, u), d| {
        (
            t + d.total_space(),
            u + (d.total_space() - d.available_space()),
        )
    });
    let free_disk = total_disk.saturating_sub(used_disk);
    let disk_pct = if total_disk > 0 {
        (used_disk as f64 / total_disk as f64) * 100.0
    } else {
        0.0
    };

    // Collect a 1-second network sample
    let nets1 = Networks::new_with_refreshed_list();
    tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
    let nets2 = Networks::new_with_refreshed_list();
    let mut total_rx = 0u64;
    let mut total_tx = 0u64;
    for (iface, n2) in nets2.iter() {
        if let Some(n1) = nets1.get(iface) {
            total_rx += n2.total_received().saturating_sub(n1.total_received());
            total_tx += n2
                .total_transmitted()
                .saturating_sub(n1.total_transmitted());
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
        "disk": {
            "totalBytes": total_disk,
            "usedBytes": used_disk,
            "freeBytes": free_disk,
            "usedPercent": disk_pct
        },
        "network": {
            "downloadBytesPerSec": dl_bytes_sec,
            "uploadBytesPerSec": ul_bytes_sec,
            "downloadKbps": dl_kbps,
            "uploadKbps": ul_kbps
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
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
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
    if let Some(token) = get_session_token_from_headers(&headers) {
        if !state.is_authenticated(&token).await {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
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
        .health_snapshot(&[pipeline_id], &recording_enabled)
        .await;
    let alert_list = alerts::derive_alerts(&snapshot);
    Json(alert_list).into_response()
}

/// Returns derived alerts across all pipelines. Auth required.
async fn aggregate_alerts_handler(
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
    let alert_list = alerts::derive_alerts(&snapshot);
    Json(alert_list).into_response()
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
        .active_ingests
        .read()
        .await
        .get(&pipeline_id)
        .map(|ingest| match ingest.protocol.as_str() {
            "file" => "rtmp".to_string(),
            protocol => protocol.to_string(),
        }) {
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
    let sem = {
        let mut map = engine.diag_semaphores.write().await;
        map.entry(pipeline_id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(1)))
            .clone()
    };
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
        .active_ingests
        .read()
        .await
        .contains_key(&pipeline_id);
    if has_ingest && !state.engine.is_recording_active(&pipeline_id).await {
        let ring_buf = state.engine.get_or_create_pipeline(&pipeline_id).await;
        let cancel_token = state.engine.register_recording(&pipeline_id).await;
        let engine = state.engine.clone();
        let pid = pipeline_id.clone();
        let pipe_name = pipeline.name.clone();
        let engine_rec = engine.clone();
        let media_dir = state.media_dir.clone();
        tokio::spawn(async move {
            crate::media::recording::start_recording(
                pipe_name,
                pid.clone(),
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

async fn hls_playlist_handler(
    State(state): State<Arc<AppState>>,
    Path(pipeline_id): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !request_is_authenticated(&state, &headers).await {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    // Auto-start segmenter on first request if ingest is active
    let has_ingest = state
        .engine
        .active_ingests
        .read()
        .await
        .contains_key(&pipeline_id);
    if has_ingest {
        let (store, already_running) = state.engine.ensure_hls_segmenter(&pipeline_id).await;
        if !already_running {
            let engine_c = state.engine.clone();
            let pid = pipeline_id.clone();
            let ring_buf = state.engine.get_or_create_pipeline(&pipeline_id).await;
            let cancel_token = state
                .engine
                .get_hls_cancel_token(&pipeline_id)
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
        state.engine.touch_hls(&pipeline_id).await;
        match store.get_playlist() {
            Some(playlist) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
                playlist,
            )
                .into_response(),
            None => (StatusCode::NOT_FOUND, "No segments yet").into_response(),
        }
    } else {
        // No ingest — serve from existing store if any
        let Some(store) = state.engine.get_hls_store(&pipeline_id).await else {
            return (StatusCode::NOT_FOUND, "No HLS stream").into_response();
        };
        state.engine.touch_hls(&pipeline_id).await;
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
}

async fn hls_segment_handler(
    State(state): State<Arc<AppState>>,
    Path((pipeline_id, segment)): Path<(String, String)>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !request_is_authenticated(&state, &headers).await {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
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
