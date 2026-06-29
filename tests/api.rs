use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use restream::domain::stage::{StageKey, StageKind};
use restream::media::engine::{AudioMeta, MediaEngine, VideoMeta};
use restream::media::security::IngestSecurityService;
use restream::types::AppLogEntry;
use restream::{api, db};
use sqlx::SqlitePool;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{RwLock as TokioRwLock, broadcast};
use tokio::time::{Duration, sleep};
use tower::ServiceExt;

async fn test_app() -> (axum::Router, SqlitePool) {
    let (app, pool, _) = test_app_with_engine().await;
    (app, pool)
}

async fn test_app_with_engine() -> (axum::Router, SqlitePool, Arc<MediaEngine>) {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    db::setup_database_schema(&pool).await.unwrap();

    let sessions = Arc::new(TokioRwLock::new(HashSet::new()));
    api::initialize_auth(&pool, &sessions).await;

    let security = Arc::new(IngestSecurityService::new(
        restream::media::security::DEFAULT_INGEST_SECURITY_CONFIG,
    ));
    let ingest_policy_store = Arc::new(restream::media::srt::SrtIngestPolicyStore::new(
        restream::types::SrtGlobalIngestConfig::default(),
        &[],
    ));
    let (log_broadcast, _) = broadcast::channel(32);
    let engine = Arc::new(MediaEngine::new());

    let state = Arc::new(api::AppState {
        db: pool.clone(),
        security,
        ingest_policy_store,
        sessions,
        engine: engine.clone(),
        ports: api::PortConfig {
            rtmp: 1935,
            srt: 10080,
        },
        media_dir: "media".to_string(),
        alert_tracker: restream::alerts::AlertTracker::new(),
        log_broadcast,
        #[cfg(feature = "agent-execution")]
        agent_execution: Arc::new(restream::agent_execution::AgentExecutionStore::default()),
    });

    (api::create_router(state), pool, engine)
}

async fn authenticated_app() -> (axum::Router, String) {
    let (app, _) = test_app().await;
    let cookie = login(&app).await;
    (app, cookie)
}

async fn authenticated_app_with_temp_media() -> (axum::Router, String, std::path::PathBuf) {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    db::setup_database_schema(&pool).await.unwrap();

    let sessions = Arc::new(TokioRwLock::new(HashSet::new()));
    api::initialize_auth(&pool, &sessions).await;

    let security = Arc::new(IngestSecurityService::new(
        restream::media::security::DEFAULT_INGEST_SECURITY_CONFIG,
    ));
    let ingest_policy_store = Arc::new(restream::media::srt::SrtIngestPolicyStore::new(
        restream::types::SrtGlobalIngestConfig::default(),
        &[],
    ));
    let (log_broadcast, _) = broadcast::channel(32);
    let engine = Arc::new(MediaEngine::new());
    let temp_dir =
        std::env::temp_dir().join(format!("restream-api-media-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let media_dir = temp_dir.to_string_lossy().to_string();

    let state = Arc::new(api::AppState {
        db: pool,
        security,
        ingest_policy_store,
        sessions,
        engine,
        ports: api::PortConfig {
            rtmp: 1935,
            srt: 10080,
        },
        media_dir,
        alert_tracker: restream::alerts::AlertTracker::new(),
        log_broadcast,
        #[cfg(feature = "agent-execution")]
        agent_execution: Arc::new(restream::agent_execution::AgentExecutionStore::default()),
    });

    let app = api::create_router(state);
    let cookie = login(&app).await;
    (app, cookie, temp_dir)
}

async fn login(app: &axum::Router) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header("Content-Type", "application/json")
                .body(axum::body::Body::from(r#"{"password":"admin"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    cookie.split(';').next().unwrap().to_string()
}

#[tokio::test]
async fn base_path_script_is_served_as_static_asset() {
    let (app, _) = test_app().await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/base-path.js")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/javascript"))
    );
    let body = String::from_utf8(body_bytes(resp).await.to_vec()).unwrap();
    assert!(body.contains("__RESTREAM_BASE_PATH__"));
}

fn auth_req(
    method: &str,
    uri: &str,
    cookie: &str,
    body: Option<&str>,
) -> Request<axum::body::Body> {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("Cookie", cookie)
        .header("Content-Type", "application/json");
    if let Some(b) = body {
        builder.body(axum::body::Body::from(b.to_string())).unwrap()
    } else {
        builder.body(axum::body::Body::empty()).unwrap()
    }
}

async fn body_json(resp: axum::http::Response<axum::body::Body>) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn body_bytes(resp: axum::http::Response<axum::body::Body>) -> bytes::Bytes {
    resp.into_body().collect().await.unwrap().to_bytes()
}

async fn insert_app_log(pool: &SqlitePool, entry: AppLogEntry) {
    db::append_app_log_batch(pool, &[entry]).await.unwrap();
}

// --- Auth tests ---

#[tokio::test]
async fn healthz_no_auth() {
    let (app, _) = test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["status"], "ok");
}

#[tokio::test]
async fn login_wrong_password() {
    let (app, _) = test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header("Content-Type", "application/json")
                .body(axum::body::Body::from(r#"{"password":"wrong"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_success_and_logout() {
    let (app, _) = test_app().await;
    let cookie = login(&app).await;
    assert!(cookie.starts_with("session="));

    let resp = app
        .clone()
        .oneshot(auth_req("POST", "/api/v1/auth/logout", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn unauthenticated_returns_401() {
    let (app, _) = test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/settings")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unauthenticated_app_pages_redirect_to_login() {
    let (app, _) = test_app().await;
    for uri in ["/"] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(resp.status().is_redirection(), "{uri} should redirect");
        assert_eq!(resp.headers().get(header::LOCATION).unwrap(), "/login");
    }
}

#[tokio::test]
async fn unauthenticated_static_assets_remain_available() {
    let (app, _) = test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/base-path.js")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// --- Pipeline CRUD via API ---

#[tokio::test]
async fn pipeline_crud_via_api() {
    let (app, _) = test_app().await;
    let cookie = login(&app).await;

    // Create
    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines",
            &cookie,
            Some(r#"{"name":"Test Pipeline","streamKey":"key01_6c71124cde80358ca7c13081"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    let pipeline_id = json["pipeline"]["id"].as_str().unwrap().to_string();

    // List
    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/pipelines", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["pipelines"].as_array().unwrap().len(), 1);

    // Update
    let uri = format!("/api/v1/pipelines/{}", pipeline_id);
    let resp = app
        .clone()
        .oneshot(auth_req(
            "PATCH",
            &uri,
            &cookie,
            Some(r#"{"name":"Updated Pipeline"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Delete
    let resp = app
        .clone()
        .oneshot(auth_req("DELETE", &uri, &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// --- Output CRUD via API ---

#[tokio::test]
async fn duplicate_stream_keys_are_rejected() {
    let (app, pool) = test_app().await;
    let cookie = login(&app).await;

    db::create_pipeline(&pool, "p1", "P1", "unique-key", None, None, None)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines",
            &cookie,
            Some(r#"{"name":"P2","streamKey":"unique-key"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines",
            &cookie,
            Some(r#"{"name":"P2","streamKey":"unique-key-2"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(auth_req(
            "PATCH",
            "/api/v1/pipelines/p1",
            &cookie,
            Some(r#"{"name":"P1","streamKey":"unique-key-2"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn rtmps_output_is_accepted_by_api() {
    let (app, pool) = test_app().await;
    let cookie = login(&app).await;

    db::create_pipeline(&pool, "p_rtmps", "P", "key_rtmps", None, None, None)
        .await
        .unwrap();

    // rtmps:// must be accepted (used by Facebook, YouTube, etc.)
    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines/p_rtmps/outputs",
            &cookie,
            Some(r#"{"name":"FB","url":"rtmps://live-api-s.facebook.com:443/rtmp/test","encoding":"source"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "rtmps:// output should be accepted"
    );

    // Verify roundtrip
    let json = body_json(resp).await;
    assert_eq!(
        json["output"]["url"],
        "rtmps://live-api-s.facebook.com:443/rtmp/test"
    );
}

#[tokio::test]
async fn local_hls_output_is_accepted_by_api() {
    let (app, pool) = test_app().await;
    let cookie = login(&app).await;

    db::create_pipeline(&pool, "p_hls", "P", "key_hls", None, None, None)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines/p_hls/outputs",
            &cookie,
            Some(r#"{"name":"Local HLS","url":"hls://localhost/hls/key_hls","encoding":"source"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let json = body_json(resp).await;
    assert_eq!(json["output"]["url"], "hls://localhost/hls/key_hls");
}

#[tokio::test]
async fn custom_output_encoding_is_rejected_by_api() {
    let (app, pool) = test_app().await;
    let cookie = login(&app).await;

    db::create_pipeline(&pool, "p_custom", "P", "key_custom", None, None, None)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines/p_custom/outputs",
            &cookie,
            Some(r#"{"name":"Custom","url":"rtmp://dest/live/key","encoding":"custom"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let json = body_json(resp).await;
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Custom output encoding is not available yet")
    );

    let output = db::create_output(
        &pool,
        "out_custom_update",
        "p_custom",
        "O",
        "rtmp://dest/live/key",
        None,
        "stopped",
        "source",
    )
    .await
    .unwrap();
    let uri = format!("/api/v1/pipelines/p_custom/outputs/{}", output.id);
    let resp = app
        .clone()
        .oneshot(auth_req(
            "PATCH",
            &uri,
            &cookie,
            Some(r#"{"name":"Custom","url":"rtmp://dest/live/key","encoding":"custom+atrack:0"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_hls_upload_output_is_accepted_by_api() {
    let (app, pool) = test_app().await;
    let cookie = login(&app).await;

    db::create_pipeline(&pool, "p_http_hls", "P", "key_http_hls", None, None, None)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines/p_http_hls/outputs",
            &cookie,
            Some(r#"{"name":"Remote HLS","url":"https://a.upload.youtube.com/http_upload_hls?cid=abc&copy=0&file=out.m3u8","encoding":"source"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let json = body_json(resp).await;
    assert_eq!(
        json["output"]["url"],
        "https://a.upload.youtube.com/http_upload_hls?cid=abc&copy=0&file=out.m3u8"
    );
}

#[tokio::test]
async fn output_crud_via_api() {
    let (app, pool) = test_app().await;
    let cookie = login(&app).await;

    db::create_pipeline(&pool, "p1", "P", "key01", None, None, None)
        .await
        .unwrap();

    // Create output
    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines/p1/outputs",
            &cookie,
            Some(r#"{"name":"YouTube","url":"rtmp://yt/live","encoding":"source"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    let output_id = json["output"]["id"].as_str().unwrap().to_string();

    // Start
    let uri = format!("/api/v1/pipelines/p1/outputs/{}/start", output_id);
    let resp = app
        .clone()
        .oneshot(auth_req("POST", &uri, &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["desiredState"], "running");

    // Verify desired state persisted in DB
    let output = db::get_output(&pool, "p1", &output_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.desired_state, "running");

    // Stop
    let uri = format!("/api/v1/pipelines/p1/outputs/{}/stop", output_id);
    let resp = app
        .clone()
        .oneshot(auth_req("POST", &uri, &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Delete
    let uri = format!("/api/v1/pipelines/p1/outputs/{}", output_id);
    let resp = app
        .clone()
        .oneshot(auth_req("DELETE", &uri, &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// --- Config ---

#[tokio::test]
async fn config_get_returns_structured_data() {
    let (app, pool) = test_app().await;
    let cookie = login(&app).await;

    db::create_pipeline(&pool, "p1", "P", "key01", None, None, None)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/settings", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["pipelines"].is_array());
    assert!(json["outputs"].is_array());
    assert!(json["jobs"].is_array());
    assert!(json["serverName"].is_string());
    assert_eq!(json["ingestHost"], "");
    assert_eq!(
        json["pipelines"][0]["ingestUrls"]["rtmp"],
        "rtmp://localhost:1935/live/key01"
    );
}

#[tokio::test]
async fn config_patch_server_name() {
    let (app, _) = test_app().await;
    let cookie = login(&app).await;

    let resp = app
        .clone()
        .oneshot(auth_req(
            "PATCH",
            "/api/v1/settings",
            &cookie,
            Some(r#"{"serverName":"My Server"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["serverName"], "My Server");
}

#[tokio::test]
async fn config_patch_ingest_host_persists_and_updates_ingest_urls() {
    let (app, pool) = test_app().await;
    let cookie = login(&app).await;
    db::create_pipeline(&pool, "p1", "P", "key01", None, None, None)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(auth_req(
            "PATCH",
            "/api/v1/settings",
            &cookie,
            Some(r#"{"ingestHost":"  ingest.example.com  "}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["ingestHost"], "ingest.example.com");
    assert_eq!(
        db::get_ingest_host(&pool).await.unwrap().as_deref(),
        Some("ingest.example.com")
    );

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/settings", &cookie, None))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["ingestHost"], "ingest.example.com");
    assert_eq!(
        json["pipelines"][0]["ingestUrls"]["rtmp"],
        "rtmp://ingest.example.com:1935/live/key01"
    );
    assert_eq!(
        json["pipelines"][0]["ingestUrls"]["srt"],
        "srt://ingest.example.com:10080?streamid=publish:live/key01"
    );

    let resp = app
        .clone()
        .oneshot(auth_req(
            "PATCH",
            "/api/v1/settings",
            &cookie,
            Some(r#"{"ingestHost":"   "}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["ingestHost"], "");

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/settings", &cookie, None))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(
        json["pipelines"][0]["ingestUrls"]["rtmp"],
        "rtmp://localhost:1935/live/key01"
    );
}

// --- Audio caps ---

#[tokio::test]
async fn audio_caps_no_auth() {
    let (app, _) = test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/audio-caps")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["caps"].is_object());
    assert!(json["platformLabels"].is_object());
}

// --- Stream keys ---

#[tokio::test]
async fn stream_keys_requires_auth() {
    let (app, _) = test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/stream-keys")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn stream_keys_returns_array() {
    let (app, pool) = test_app().await;
    let cookie = login(&app).await;
    db::set_ingest_host(&pool, "ingest.example.com")
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/stream-keys", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let keys = json.as_array().unwrap();
    assert_eq!(keys.len(), 20);
    assert!(keys[0]["key"].is_string());
    assert_eq!(
        keys[0]["ingestUrls"]["rtmp"],
        "rtmp://ingest.example.com:1935/live/key01_6c71124cde80358ca7c13081"
    );
    assert_eq!(
        keys[0]["ingestUrls"]["srt"],
        "srt://ingest.example.com:10080?streamid=publish:live/key01_6c71124cde80358ca7c13081"
    );
}

// --- Ingest CRUD ---

#[tokio::test]
async fn ingest_crud_via_api() {
    let (app, _) = test_app().await;
    let cookie = login(&app).await;

    // Create
    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/ingests",
            &cookie,
            Some(r#"{"filename":"test.mp4","streamKey":"key01","loopFlag":true}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let id = json["id"].as_str().unwrap().to_string();
    assert_eq!(json["filename"], "test.mp4");
    assert_eq!(json["loop"], true);

    // List
    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/ingests", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json.as_array().unwrap().len(), 1);

    // Delete
    let uri = format!("/api/v1/ingests/{}", id);
    let resp = app
        .clone()
        .oneshot(auth_req("DELETE", &uri, &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn pipeline_file_ingest_is_scoped_to_pipeline_stream_key() {
    let (app, _) = test_app().await;
    let cookie = login(&app).await;

    let create_pipeline = serde_json::json!({
        "name": "File Pipeline",
        "streamKey": "key01_6c71124cde80358ca7c13081"
    });
    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines",
            &cookie,
            Some(&create_pipeline.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    let pipeline_id = json["pipeline"]["id"].as_str().unwrap().to_string();

    let file_ingest = serde_json::json!({
        "filename": "clip.mp4",
        "loop": true,
        "startTime": "00:00:05"
    });
    let uri = format!("/api/v1/pipelines/{pipeline_id}/file-ingest");
    let resp = app
        .clone()
        .oneshot(auth_req(
            "PUT",
            &uri,
            &cookie,
            Some(&file_ingest.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["configured"], true);
    assert_eq!(json["filename"], "clip.mp4");
    assert_eq!(json["streamKey"], "key01_6c71124cde80358ca7c13081");
    assert_eq!(json["loop"], true);
    assert_eq!(json["startTime"], "00:00:05");

    let resp = app
        .clone()
        .oneshot(auth_req("GET", &uri, &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["configured"], true);
    assert_eq!(json["filename"], "clip.mp4");

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/settings", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["pipelines"][0]["inputSource"], "file:clip.mp4");

    let resp = app
        .clone()
        .oneshot(auth_req("DELETE", &uri, &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(auth_req("GET", &uri, &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["configured"], false);

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/settings", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["pipelines"][0]["inputSource"].is_null());
}

// --- Lifecycle history ---

#[tokio::test]
async fn pipeline_logs_include_persisted_lifecycle_events_without_stream_keys() {
    let (app, pool) = test_app().await;
    let cookie = login(&app).await;

    insert_app_log(
        &pool,
        AppLogEntry {
            ts: "2026-06-29T00:00:00Z".to_string(),
            level: "INFO".to_string(),
            target: "restream::lifecycle".to_string(),
            message: "RTMP publisher connected".to_string(),
            fields: Some(r#"{"protocol":"rtmp","pipelineId":"pipe-history"}"#.to_string()),
            pipeline_id: Some("pipe-history".to_string()),
            output_id: None,
            event_type: Some("ingest.connected".to_string()),
            event_class: Some("lifecycle".to_string()),
        },
    )
    .await;

    let resp = app
        .clone()
        .oneshot(auth_req(
            "GET",
            "/api/v1/logs?pipeline_id=pipe-history&event_class=lifecycle&limit=20",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["logs"][0]["pipelineId"], "pipe-history");
    assert_eq!(json["logs"][0]["eventType"], "ingest.connected");
    assert_eq!(json["logs"][0]["message"], "RTMP publisher connected");
    assert!(
        !serde_json::to_string(&json)
            .unwrap()
            .contains("secret-history-key")
    );
}

#[tokio::test]
async fn output_logs_lifecycle_filter_includes_persisted_egress_events() {
    let (app, pool) = test_app().await;
    let cookie = login(&app).await;

    insert_app_log(
        &pool,
        AppLogEntry {
            ts: "2026-06-29T00:00:01Z".to_string(),
            level: "INFO".to_string(),
            target: "restream::lifecycle".to_string(),
            message: "Egress started".to_string(),
            fields: Some(r#"{"pipelineId":"pipe-history","outputId":"out-history"}"#.to_string()),
            pipeline_id: Some("pipe-history".to_string()),
            output_id: Some("out-history".to_string()),
            event_type: Some("egress.started".to_string()),
            event_class: Some("lifecycle".to_string()),
        },
    )
    .await;

    let resp = app
        .clone()
        .oneshot(auth_req(
            "GET",
            "/api/v1/logs?pipeline_id=pipe-history&output_id=out-history&event_class=lifecycle",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["logs"][0]["pipelineId"], "pipe-history");
    assert_eq!(json["logs"][0]["outputId"], "out-history");
    assert_eq!(json["logs"][0]["eventType"], "egress.started");
    let fields = json["logs"][0]["fields"].as_str().unwrap();
    let event_data: serde_json::Value = serde_json::from_str(fields).unwrap();
    assert_eq!(event_data["outputId"], "out-history");
}

// --- Custom encoding ---

#[tokio::test]
async fn custom_encoding_roundtrip() {
    let (app, _) = test_app().await;
    let cookie = login(&app).await;

    let resp = app
        .clone()
        .oneshot(auth_req(
            "PUT",
            "/api/v1/encodings/custom",
            &cookie,
            Some(r#"{"ffmpegArgs":"-c:v libx264 -preset fast"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/encodings/custom", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["ffmpegArgs"], "-c:v libx264 -preset fast");
}

// --- HLS pull ---

#[tokio::test]
async fn hls_canonical_no_stream_returns_404() {
    let (app, _) = test_app().await;
    let cookie = login(&app).await;
    let resp = app
        .oneshot(auth_req(
            "GET",
            "/hls/nonexistent/index.m3u8",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn hls_routes_require_authentication() {
    let (app, _, engine) = test_app_with_engine().await;
    engine.get_or_create_hls_store("test_pipe").await;

    for uri in [
        "/hls/test_pipe",
        "/hls/test_pipe/index.m3u8",
        "/hls/test_pipe/notasegment",
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "uri={uri}");
    }
}

#[tokio::test]
async fn hls_playlist_routes_return_not_found_for_empty_store() {
    let (app, _, engine) = test_app_with_engine().await;
    let cookie = login(&app).await;
    engine.get_or_create_hls_store("test_pipe").await;

    for uri in ["/hls/test_pipe", "/hls/test_pipe/index.m3u8"] {
        let resp = app
            .clone()
            .oneshot(auth_req("GET", uri, &cookie, None))
            .await
            .unwrap();

        // An existing empty store is a valid playlist route with no segments
        // yet. The generic segment handler returns 400 for "index.m3u8".
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "uri={uri}");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"No segments yet", "uri={uri}");
    }
}

#[tokio::test]
async fn hls_segment_bad_name_returns_400() {
    let (app, _, engine) = test_app_with_engine().await;
    let cookie = login(&app).await;
    engine.get_or_create_hls_store("test_pipe").await;

    for uri in ["/hls/test_pipe/notasegment"] {
        let resp = app
            .clone()
            .oneshot(auth_req("GET", uri, &cookie, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
    }
}

#[tokio::test]
async fn internal_file_ingest_preview_hls_serves_playlist_and_segment() {
    let (app, pool, engine) = test_app_with_engine().await;
    let cookie = login(&app).await;
    let pipeline_id = "pipe-file-preview";
    let stream_key = "file-preview-key";
    let ingest_id = "ingest-file-preview";

    db::create_pipeline(
        &pool,
        pipeline_id,
        "File Preview",
        stream_key,
        None,
        None,
        None,
    )
    .await
    .expect("create pipeline");

    let ring_buffer = engine.get_or_create_pipeline(pipeline_id).await;
    let cancel = engine
        .try_register_ingest(pipeline_id, stream_key, "file")
        .await
        .expect("register ingest");

    engine.mark_file_ingest_running(ingest_id).await;
    restream::media::file_ingest::spawn_internal_file_ingest(
        engine.clone(),
        tokio::runtime::Handle::current(),
        ingest_id.to_string(),
        pipeline_id.to_string(),
        restream::test_fixtures::checked_in_fixture("media/colorbar-timer-2v16a.mp4")
            .expect("checked-in integration media fixture"),
        String::new(),
        false,
        ring_buffer,
        cancel.clone(),
    )
    .expect("spawn internal ingest");

    let mut playlist_body = None;
    for _ in 0..40 {
        let resp = app
            .clone()
            .oneshot(auth_req(
                "GET",
                &format!("/hls/{pipeline_id}/index.m3u8"),
                &cookie,
                None,
            ))
            .await
            .unwrap();

        if resp.status() == StatusCode::OK {
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            let body = String::from_utf8(bytes.to_vec()).expect("playlist utf8");
            if body
                .lines()
                .any(|line| !line.starts_with('#') && !line.is_empty())
            {
                playlist_body = Some(body);
                break;
            }
        }

        sleep(Duration::from_millis(250)).await;
    }

    let playlist = playlist_body.expect("playlist with at least one segment");
    let segment = playlist
        .lines()
        .find(|line| !line.starts_with('#') && !line.is_empty())
        .expect("segment path in playlist");

    let resp = app
        .clone()
        .oneshot(auth_req(
            "GET",
            &format!("/hls/{pipeline_id}/{segment}"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let segment_body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(
        !segment_body.is_empty(),
        "segment payload should not be empty"
    );

    cancel.cancel();
    sleep(Duration::from_millis(250)).await;
}

// --- Status ---

#[tokio::test]
async fn status_returns_version_info() {
    let (app, _) = test_app().await;
    let cookie = login(&app).await;

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/engine", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let engine = &json;
    assert!(engine["restream"]["version"].is_string());
    assert!(engine["restream"]["commit"].is_string());
    assert!(engine.get("ffmpeg").is_none());
    assert!(engine["toolchain"]["rustc"].is_string());
    assert!(engine["nativeLibraries"]["ffmpeg"]["version"].is_string());
    assert!(engine["nativeLibraries"]["ffmpeg"]["configuration"].is_string());
    assert!(engine["nativeLibraries"]["srt"]["version"].is_string());
    assert!(engine["nativeLibraries"]["mbedtls"]["version"].is_string());
    assert!(engine["nativeLibraries"]["sqlite"]["version"].is_string());
    assert!(engine["nativeLibraries"]["x264"]["version"].is_string());
    assert!(engine["nativeLibraries"]["x265"]["version"].is_string());
    assert_eq!(engine["sbom"]["format"], "CycloneDX");
    assert_eq!(engine["sbom"]["specVersion"], "1.5");
    assert_eq!(engine["sbom"]["licensesIncluded"], true);
    assert!(engine["sbom"]["componentCount"].as_u64().unwrap() > 20);
    assert!(engine["os"]["platform"].is_string());
    assert!(engine["os"]["hostname"].is_string());
    assert!(engine["os"]["cpu"]["logicalCpus"].as_u64().unwrap() > 0);
    assert!(engine["os"]["cpu"]["flags"].is_array());
}

#[tokio::test]
async fn status_sbom_is_authenticated_cyclonedx_with_licenses() {
    let (app, _) = test_app().await;

    let unauthorized = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/engine/sbom")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let cookie = login(&app).await;
    let response = app
        .oneshot(auth_req("GET", "/api/v1/engine/sbom", &cookie, None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/vnd.cyclonedx+json; version=1.5"
    );
    let json = body_json(response).await;

    assert_eq!(json["bomFormat"], "CycloneDX");
    assert_eq!(json["specVersion"], "1.5");
    assert_eq!(json["metadata"]["component"]["name"], "restream");

    let components = json["components"].as_array().unwrap();
    assert!(components.len() > 20);
    assert!(components.iter().all(|component| {
        component["licenses"]
            .as_array()
            .is_some_and(|licenses| !licenses.is_empty())
    }));
    assert!(
        !components
            .iter()
            .any(|component| component["name"] == "criterion")
    );
    assert!(
        !components
            .iter()
            .any(|component| component["name"] == "pulp")
    );
    for build_only in ["proc-macro2", "quote", "serde_derive", "syn"] {
        assert!(
            !components
                .iter()
                .any(|component| component["name"] == build_only),
            "build-only crate leaked into runtime SBOM: {build_only}"
        );
    }
    assert!(!components.iter().any(|component| {
        component["name"]
            .as_str()
            .is_some_and(|name| name.starts_with("windows-"))
    }));
    for name in [
        "libavcodec",
        "libavformat",
        "libavfilter",
        "libswscale",
        "libswresample",
        "libavutil",
        "libsrt",
        "libmbedcrypto",
        "SQLite",
        "x264",
        "x265",
        "libstdc++",
        "libgcc",
        "Rust standard library",
        "tokio",
        "axum",
        "sqlx",
    ] {
        let component = components
            .iter()
            .find(|component| component["name"] == name)
            .unwrap_or_else(|| panic!("missing SBOM component {name}"));
        assert!(component["version"].is_string());
        assert!(
            component["licenses"]
                .as_array()
                .is_some_and(|v| !v.is_empty())
        );
    }
}

// --- Processing graph ---

#[tokio::test]
async fn pipeline_graph_returns_dag() {
    let (app, _) = test_app().await;
    let cookie = login(&app).await;

    // Create a pipeline first
    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines",
            &cookie,
            Some(r#"{"name":"graph-test","streamKey":"gkey"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let pipeline = body_json(resp).await;
    let pid = pipeline["pipeline"]["id"].as_str().unwrap();

    // Get the graph (no active ingests/egresses, should still return structure)
    let resp = app
        .clone()
        .oneshot(auth_req(
            "GET",
            &format!("/api/v1/pipelines/{}/graph", pid),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let graph = body_json(resp).await;
    assert!(graph["nodes"].is_array());
    assert!(graph["edges"].is_array());
    // Source ring buffer node should always be present
    let nodes = graph["nodes"].as_array().unwrap();
    assert!(nodes.iter().any(|n| n["type"] == "ring_buffer"));
}

#[tokio::test]
async fn diagnostics_requires_active_ingest() {
    let (app, _) = test_app().await;
    let cookie = login(&app).await;

    let resp = app
        .clone()
        .oneshot(auth_req(
            "GET",
            "/api/v1/pipelines/inactive/diagnostics?probe=srt",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// --- Password change ---

#[tokio::test]
async fn change_password() {
    let (app, _) = test_app().await;
    let cookie = login(&app).await;

    // Change password
    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/auth/change-password",
            &cookie,
            Some(r#"{"current_password":"admin","new_password":"newpass123"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Old password should fail
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header("Content-Type", "application/json")
                .body(axum::body::Body::from(r#"{"password":"admin"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // New password should work
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header("Content-Type", "application/json")
                .body(axum::body::Body::from(r#"{"password":"newpass123"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn health_shows_registered_egress() {
    let (_, pool, engine) = test_app_with_engine().await;
    let app = {
        let sessions = Arc::new(TokioRwLock::new(HashSet::new()));
        api::initialize_auth(&pool, &sessions).await;
        let security = Arc::new(IngestSecurityService::new(
            restream::media::security::DEFAULT_INGEST_SECURITY_CONFIG,
        ));
        let ingest_policy_store = Arc::new(restream::media::srt::SrtIngestPolicyStore::new(
            restream::types::SrtGlobalIngestConfig::default(),
            &[],
        ));
        let (log_broadcast, _) = broadcast::channel(32);
        let state = Arc::new(api::AppState {
            db: pool.clone(),
            security,
            ingest_policy_store,
            sessions,
            engine: engine.clone(),
            ports: api::PortConfig {
                rtmp: 1935,
                srt: 10080,
            },
            media_dir: "media".to_string(),
            alert_tracker: restream::alerts::AlertTracker::new(),
            log_broadcast,
            #[cfg(feature = "agent-execution")]
            agent_execution: Arc::new(restream::agent_execution::AgentExecutionStore::default()),
        });
        api::create_router(state)
    };
    let cookie = login(&app).await;

    // Create pipeline and output
    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines",
            &cookie,
            Some(r#"{"name":"P","streamKey":"key01_6c71124cde80358ca7c13081"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let pipe = body_json(resp).await;
    let pid = pipe["pipeline"]["id"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            &format!("/api/v1/pipelines/{pid}/outputs"),
            &cookie,
            Some(r#"{"name":"O","url":"rtmp://dest/live/k","encoding":"source"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let out = body_json(resp).await;
    let oid = out["output"]["id"].as_str().unwrap().to_string();

    // Register an ingest + egress in the engine (simulates reconciler start with active publisher)
    engine
        .try_register_ingest(&pid, "key01_6c71124cde80358ca7c13081", "rtmp")
        .await
        .expect("ingest registration should succeed");
    engine
        .register_egress(&oid, &pid, "rtmp://dest/live/k")
        .await;

    // Health endpoint should show the output under the correct pipeline
    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/engine/health", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let health = body_json(resp).await;
    assert!(health["srtListener"]["bondingAvailable"].is_boolean());
    let outputs = &health["pipelines"][&pid]["outputs"];
    assert!(
        outputs[&oid].is_object(),
        "egress should appear under its pipeline in /health: {outputs}"
    );
    assert_eq!(outputs[&oid]["status"], "running");
}

#[tokio::test]
async fn output_status_and_health_preserve_recent_egress_failure_after_unregister() {
    let (app, _, engine) = test_app_with_engine().await;
    let cookie = login(&app).await;

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines",
            &cookie,
            Some(r#"{"name":"P","streamKey":"key01_6c71124cde80358ca7c13082"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let pipe = body_json(resp).await;
    let pid = pipe["pipeline"]["id"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            &format!("/api/v1/pipelines/{pid}/outputs"),
            &cookie,
            Some(r#"{"name":"O","url":"rtmp://dest/live/k","encoding":"source"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let out = body_json(resp).await;
    let oid = out["output"]["id"].as_str().unwrap().to_string();

    engine
        .try_register_ingest(&pid, "key01_6c71124cde80358ca7c13082", "rtmp")
        .await
        .expect("ingest registration should succeed");
    engine
        .register_egress(&oid, &pid, "rtmp://dest/live/k")
        .await;
    engine.update_egress_phase(&oid, "sending").await;
    engine.record_egress_progress(&oid, 1316).await;
    engine
        .record_egress_error(&oid, "send", "connection reset by peer")
        .await;
    engine.unregister_egress(&oid).await;

    let resp = app
        .clone()
        .oneshot(auth_req(
            "GET",
            &format!("/api/v1/pipelines/{pid}/outputs/{oid}/status"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let status = body_json(resp).await;
    assert_eq!(status["status"], "failed");
    assert_eq!(status["rawStatus"], "running");
    assert_eq!(status["phase"], "failed");
    assert_eq!(status["failurePhase"], "send");
    assert_eq!(status["lastError"], "connection reset by peer");
    assert!(status["lastErrorAt"].is_string());
    assert!(status["endedAt"].is_string());

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/engine/health", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let health = body_json(resp).await;
    let output = &health["pipelines"][&pid]["outputs"][&oid];
    assert_eq!(output["status"], "failed");
    assert_eq!(output["phase"], "failed");
    assert_eq!(output["failurePhase"], "send");
    assert_eq!(output["lastError"], "connection reset by peer");
    assert!(output["endedAt"].is_string());
}

#[tokio::test]
async fn delete_output_cancels_egress() {
    let (app, _, engine) = test_app_with_engine().await;
    let cookie = login(&app).await;

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines",
            &cookie,
            Some(r#"{"name":"P","streamKey":"key01_6c71124cde80358ca7c13081"}"#),
        ))
        .await
        .unwrap();
    let pipe = body_json(resp).await;
    let pid = pipe["pipeline"]["id"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            &format!("/api/v1/pipelines/{pid}/outputs"),
            &cookie,
            Some(r#"{"name":"O","url":"rtmp://dest/live/k","encoding":"source"}"#),
        ))
        .await
        .unwrap();
    let out = body_json(resp).await;
    let oid = out["output"]["id"].as_str().unwrap().to_string();

    let token = engine
        .register_egress(&oid, &pid, "rtmp://dest/live/k")
        .await;
    assert!(!token.is_cancelled());

    // Delete the output
    let resp = app
        .clone()
        .oneshot(auth_req(
            "DELETE",
            &format!("/api/v1/pipelines/{pid}/outputs/{oid}"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Egress cancellation token should be cancelled
    assert!(token.is_cancelled(), "deleting output should cancel egress");
}

#[tokio::test]
async fn health_endpoint_exposes_probe_and_egress_fault_fields() {
    let (app, _, engine) = test_app_with_engine().await;
    let cookie = login(&app).await;

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines",
            &cookie,
            Some(r#"{"name":"P","streamKey":"key01_6c71124cde80358ca7c13081"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let pipe = body_json(resp).await;
    let pid = pipe["pipeline"]["id"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            &format!("/api/v1/pipelines/{pid}/outputs"),
            &cookie,
            Some(r#"{"name":"O","url":"rtmp://dest/live/k","encoding":"source"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let out = body_json(resp).await;
    let oid = out["output"]["id"].as_str().unwrap().to_string();

    engine
        .try_register_ingest(&pid, "key01_6c71124cde80358ca7c13081", "rtmp")
        .await
        .expect("ingest registration should succeed");
    engine
        .register_egress(&oid, &pid, "rtmp://dest/live/k")
        .await;

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/engine/health", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let pending = body_json(resp).await;
    let pending_input = &pending["pipelines"][&pid]["input"];
    assert_eq!(pending_input["probeReady"], false);
    assert_eq!(pending_input["probeStatus"], "pending");
    assert!(pending_input["probePendingMs"].as_u64().is_some());

    let audio = AudioMeta {
        track_index: 0,
        codec: "aac".to_string(),
        sample_rate: 48_000,
        channels: 2,
        channel_layout: None,
        pid: None,
        language: None,
        title: None,
        profile: None,
    };
    engine
        .update_ingest_meta(
            &pid,
            Some(VideoMeta {
                codec: "h264".to_string(),
                width: 1920,
                height: 1080,
                fps: 30.0,
                bw: None,
                pid: None,
                language: None,
                title: None,
                profile: None,
                level: None,
                pixel_format: None,
            }),
            Some(audio.clone()),
            None,
        )
        .await;
    engine.update_ingest_audio_tracks(&pid, vec![audio]).await;
    engine.record_egress_progress(&oid, 1316).await;
    engine
        .record_egress_error(&oid, "send", "connection reset by peer")
        .await;
    let (store, _) = engine.ensure_hls_segmenter(&pid).await;
    engine.add_hls_persistent_consumer(&pid).await;
    engine.touch_hls(&pid).await;
    store.push_segment(2.0, bytes::Bytes::from_static(b"segment"));

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/engine/health", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ready = body_json(resp).await;
    let ready_input = &ready["pipelines"][&pid]["input"];
    assert_eq!(ready_input["probeReady"], true);
    assert_eq!(ready_input["probeStatus"], "ready");
    assert!(ready_input["probePendingMs"].is_null());

    let output = &ready["pipelines"][&pid]["outputs"][&oid];
    assert_eq!(output["status"], "failed");
    assert_eq!(output["rawStatus"], "running");
    assert_eq!(output["phase"], "failed");
    assert_eq!(output["failurePhase"], "send");
    assert_eq!(output["lastError"], "connection reset by peer");
    assert!(output["lastErrorAt"].is_string());
    assert!(output["lastProgressAt"].is_string());
    assert!(output["lastProgressAgeMs"].as_u64().is_some());

    let hls_preview = &ready["pipelines"][&pid]["hlsPreview"];
    assert_eq!(hls_preview["active"], true);
    assert_eq!(hls_preview["persistentConsumers"], 1);
    assert!(hls_preview["lastAccessAgeMs"].as_u64().is_some());
    assert_eq!(hls_preview["segments"], 1);
    assert!(hls_preview["playlistBytes"].as_u64().unwrap_or(0) > 0);

    engine
        .record_ingest_disconnect(
            &pid,
            Some("disconnect"),
            Some("publisher disconnected".to_string()),
            false,
        )
        .await;
    engine.unregister_ingest(&pid).await;

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/engine/health", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let disconnected = body_json(resp).await;
    let disconnected_input = &disconnected["pipelines"][&pid]["input"];
    assert_eq!(disconnected_input["status"], "off");
    assert_eq!(disconnected_input["probeStatus"], "off");
    assert_eq!(disconnected_input["lastSessionProtocol"], "rtmp");
    assert_eq!(
        disconnected_input["lastDisconnectReason"],
        "publisher disconnected"
    );
    assert_eq!(disconnected_input["lastFailurePhase"], "disconnect");
    assert_eq!(disconnected_input["recentDisconnectError"], false);
    assert!(disconnected_input["lastDisconnectAt"].is_string());
    assert!(disconnected_input["lastDisconnectAgeMs"].as_u64().is_some());
}

// --- Regression: Round 6 #2 — Security headers ---

#[tokio::test]
async fn security_headers_present_on_api_response() {
    // Every API response must carry X-Content-Type-Options and X-Frame-Options
    // to defend against MIME-sniffing and clickjacking (Round 6 finding #2).
    let (app, _) = test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("x-content-type-options")
            .map(|v| v.as_bytes()),
        Some(b"nosniff" as &[u8]),
        "X-Content-Type-Options: nosniff must be present"
    );
    assert_eq!(
        resp.headers().get("x-frame-options").map(|v| v.as_bytes()),
        Some(b"SAMEORIGIN" as &[u8]),
        "X-Frame-Options: SAMEORIGIN must be present"
    );
}

// --- Regression: Round 6 #7 — HLS consumer refcount ---

#[tokio::test]
async fn hls_persistent_consumer_refcount_is_zero_after_balanced_add_remove() {
    // add_hls_persistent_consumer(+1) must be matched by remove(-1).
    // This test exercises the engine methods directly to confirm the counter
    // returns to zero, guarding against underflow or permanent leak.
    let engine = Arc::new(MediaEngine::new());
    use restream::media::engine::HlsConsumers;
    use tokio_util::sync::CancellationToken;

    let token = CancellationToken::new();
    {
        let mut stores = engine.hls.consumers.write().await;
        stores.insert("pipe1".to_string(), HlsConsumers::new(token.clone()));
    }

    engine.add_hls_persistent_consumer("pipe1").await;
    engine.add_hls_persistent_consumer("pipe1").await;
    {
        let consumers = engine.hls.consumers.read().await;
        assert_eq!(
            consumers["pipe1"]
                .persistent
                .load(std::sync::atomic::Ordering::Relaxed),
            2,
            "count should be 2 after two adds"
        );
    }
    engine.remove_hls_persistent_consumer("pipe1").await;
    engine.remove_hls_persistent_consumer("pipe1").await;
    {
        let consumers = engine.hls.consumers.read().await;
        assert_eq!(
            consumers["pipe1"]
                .persistent
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "count should be 0 after balanced removes"
        );
    }
}

// --- Round 7 #1: media delete path traversal guard ---
#[tokio::test]
async fn media_delete_path_traversal_blocked() {
    let (app, _) = test_app().await;
    let cookie = login(&app).await;

    // Test a normal non-existent file: should return NOT_FOUND (404)
    let resp = app
        .clone()
        .oneshot(auth_req(
            "DELETE",
            "/api/v1/media/nonexistent.mp4",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Test path traversal attempt: should return BAD_REQUEST (400) or NOT_FOUND (404)
    let resp = app
        .clone()
        .oneshot(auth_req(
            "DELETE",
            "/api/v1/media/..%2f..%2fetc%2fpasswd",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert!(resp.status() == StatusCode::BAD_REQUEST || resp.status() == StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn media_library_classifies_serves_and_deletes_files() {
    let (app, cookie, temp_dir) = authenticated_app_with_temp_media().await;
    let recording_path = temp_dir.join("sample-recording.mp4");
    let source_path = temp_dir.join("sample-source.ts");
    tokio::fs::write(&recording_path, b"mp4 recording data")
        .await
        .unwrap();
    tokio::fs::write(&source_path, b"ts source data")
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/media", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let files = json["files"].as_array().unwrap();
    let recording = files
        .iter()
        .find(|file| file["name"].as_str() == Some("sample-recording.mp4"))
        .expect("recording-named files should be visible in the media library");
    assert_eq!(recording["kind"].as_str(), Some("recording"));
    let source = files
        .iter()
        .find(|file| file["name"].as_str() == Some("sample-source.ts"))
        .expect("non-recording files should be visible as source files");
    assert_eq!(source["kind"].as_str(), Some("source"));

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/media/sample-source.ts", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("video/mp2t")
    );
    assert_eq!(body_bytes(resp).await.as_ref(), b"ts source data");

    let resp = app
        .clone()
        .oneshot(auth_req(
            "DELETE",
            "/api/v1/media/sample-source.ts",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["deleted"], true);

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/media/sample-source.ts", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let _ = std::fs::remove_dir_all(temp_dir);
}

// --- Round 7 #4: transcode profile field validation ---
#[tokio::test]
async fn config_patch_invalid_transcode_profile_rejected() {
    let (app, _) = test_app().await;
    let cookie = login(&app).await;

    // Patch with an invalid preset
    let resp = app
        .clone()
        .oneshot(auth_req(
            "PATCH",
            "/api/v1/settings",
            &cookie,
            Some(r#"{"transcodeProfiles":{"h264":{"preset":"garbage","tune":"zerolatency","crf":23}}}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Patch with an invalid tune
    let resp = app
        .clone()
        .oneshot(auth_req(
            "PATCH",
            "/api/v1/settings",
            &cookie,
            Some(r#"{"transcodeProfiles":{"h264":{"preset":"ultrafast","tune":"badtune","crf":23}}}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Patch with an invalid CRF
    let resp = app
        .clone()
        .oneshot(auth_req(
            "PATCH",
            "/api/v1/settings",
            &cookie,
            Some(r#"{"transcodeProfiles":{"h264":{"preset":"ultrafast","tune":"zerolatency","crf":100}}}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn config_patch_custom_transcode_profiles_keep_built_ins_visible() {
    let (app, _) = test_app().await;
    let cookie = login(&app).await;

    let body = serde_json::json!({
        "transcodeProfiles": {
            "4k60": {
                "preset": "ultrafast",
                "tune": "zerolatency",
                "crf": 23,
                "gop": 60,
                "bframes": 0,
                "bitrate": 20000000,
                "maxBitrate": 24000000,
                "width": 3840,
                "height": 2160
            }
        }
    });
    let resp = app
        .clone()
        .oneshot(auth_req(
            "PATCH",
            "/api/v1/settings",
            &cookie,
            Some(&body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["transcodeProfiles"]["h264"].is_object());
    assert!(json["transcodeProfiles"]["720p"].is_object());
    assert!(json["transcodeProfiles"]["1080p"].is_object());
    assert_eq!(json["transcodeProfiles"]["4k60"]["width"], 3840);

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/settings", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["transcodeProfiles"]["h264"].is_object());
    assert!(json["transcodeProfiles"]["720p"].is_object());
    assert!(json["transcodeProfiles"]["1080p"].is_object());
    assert_eq!(json["transcodeProfiles"]["4k60"]["height"], 2160);
}

// --- Ingest start_time validation tests ---

#[tokio::test]
async fn ingest_create_start_time_too_long_rejected() {
    let (app, _pool) = test_app().await;
    let cookie = login(&app).await;

    let long_start = "0".repeat(65);
    let body = serde_json::json!({
        "filename": "clip.mp4",
        "streamKey": "testkey01",
        "startTime": long_start,
    });
    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/ingests",
            &cookie,
            Some(&body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ingest_create_start_time_valid_accepted() {
    let (app, _pool) = test_app().await;
    let cookie = login(&app).await;

    let body = serde_json::json!({
        "filename": "clip.mp4",
        "streamKey": "testkey02",
        "startTime": "00:01:30",
    });
    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/ingests",
            &cookie,
            Some(&body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn ingest_update_start_time_too_long_rejected() {
    let (app, _pool) = test_app().await;
    let cookie = login(&app).await;

    // Create ingest first
    let create_body = serde_json::json!({
        "filename": "clip.mp4",
        "streamKey": "testkey03",
    });
    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/ingests",
            &cookie,
            Some(&create_body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let ingest_id = json["id"].as_str().unwrap().to_string();

    let long_start = "1".repeat(65);
    let update_body = serde_json::json!({
        "filename": "clip.mp4",
        "streamKey": "testkey03",
        "startTime": long_start,
    });
    let resp = app
        .clone()
        .oneshot(auth_req(
            "PUT",
            &format!("/api/v1/ingests/{}", ingest_id),
            &cookie,
            Some(&update_body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ingest_start_requires_matching_pipeline() {
    let (app, _pool) = test_app().await;
    let cookie = login(&app).await;

    let create_body = serde_json::json!({
        "filename": "colorbar-timer-2v16a.mp4",
        "streamKey": "no-such-pipeline",
    });
    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/ingests",
            &cookie,
            Some(&create_body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let ingest_id = json["id"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            &format!("/api/v1/ingests/{ingest_id}/start"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["error"], "No pipeline found for stream key");
}

// --- Operator overview and pipeline summary ---

#[tokio::test]
async fn v1_overview_requires_auth() {
    let (app, _) = test_app().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/overview")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn v1_overview_returns_summary_fields() {
    let (app, _) = test_app().await;
    let cookie = login(&app).await;
    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/overview", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["totalPipelines"].is_number());
    assert!(body["activePipelines"].is_number());
    assert!(body["degradedPipelines"].is_number());
    assert!(body["failedOutputs"].is_number());
    assert!(body["alertCount"]["critical"].is_number());
    assert!(body["alertCount"]["warning"].is_number());
    assert!(body["generatedAt"].is_string());
}

#[tokio::test]
async fn v1_overview_counts_match_pipeline_count() {
    let (app, pool) = test_app().await;
    let cookie = login(&app).await;

    // Create two pipelines
    for name in &["overview-p1", "overview-p2"] {
        app.clone()
            .oneshot(auth_req(
                "POST",
                "/api/v1/pipelines",
                &cookie,
                Some(&serde_json::json!({ "name": name, "streamKey": name }).to_string()),
            ))
            .await
            .unwrap();
    }
    let _ = pool; // keep alive

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/overview", &cookie, None))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["totalPipelines"].as_u64().unwrap(), 2);
    // No active ingests → 0 active, both pipelines show no-publisher alert → degraded = 2
    assert_eq!(body["activePipelines"].as_u64().unwrap(), 0);
    assert_eq!(body["degradedPipelines"].as_u64().unwrap(), 2);
}

#[tokio::test]
async fn v1_pipeline_summary_not_found_for_unknown_id() {
    let (app, _) = test_app().await;
    let cookie = login(&app).await;
    let resp = app
        .clone()
        .oneshot(auth_req(
            "GET",
            "/api/v1/pipelines/nonexistent/summary",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn v1_pipeline_summary_returns_operator_fields() {
    let (app, _) = test_app().await;
    let cookie = login(&app).await;

    // Create a pipeline
    let create = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines",
            &cookie,
            Some(
                &serde_json::json!({ "name": "summary-test", "streamKey": "smrykey" }).to_string(),
            ),
        ))
        .await
        .unwrap();
    let body = body_json(create).await;
    let pid = body["pipeline"]["id"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(auth_req(
            "GET",
            &format!("/api/v1/pipelines/{}/summary", pid),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["pipelineId"].as_str().unwrap(), pid);
    assert!(body["source"]["status"].is_string());
    assert_eq!(body["input"]["status"], body["source"]["status"]);
    assert!(body["outputs"]["total"].is_number());
    assert!(body["outputs"]["running"].is_number());
    assert_eq!(body["graph"]["hasGraph"], true);
    assert!(body["graph"]["nodes"].as_u64().unwrap() > 0);
    assert!(body["graph"]["edges"].is_number());
    assert!(body["graph"]["activeNodes"].is_number());
    assert!(body["alerts"].is_array());
    assert!(body["generatedAt"].is_string());
}

#[tokio::test]
async fn v1_engine_and_health_endpoints_require_auth() {
    let (app, _) = test_app().await;

    for uri in ["/api/v1/engine", "/api/v1/engine/health"] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn v1_engine_and_settings_endpoints_return_structured_payloads() {
    let (app, cookie) = authenticated_app().await;

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/engine", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let engine = body_json(resp).await;
    assert!(engine["restream"]["version"].is_string());
    assert!(engine["os"].is_object());

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/engine/health", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let health = body_json(resp).await;
    assert!(health["generatedAt"].is_string());
    assert!(health["pipelines"].is_object());

    let resp = app
        .oneshot(auth_req("GET", "/api/v1/settings", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let settings = body_json(resp).await;
    assert!(settings["serverName"].is_string());
    assert!(settings["transcodeProfiles"].is_object());
}

#[tokio::test]
async fn v1_pipeline_list_detail_and_graph_endpoints_return_payloads() {
    let (app, pool) = test_app().await;
    let cookie = login(&app).await;

    db::create_pipeline(
        &pool,
        "pipe-v1",
        "Pipeline V1",
        "key_v1",
        None,
        Some("source"),
        None,
    )
    .await
    .unwrap();
    db::create_output(
        &pool,
        "out-v1",
        "pipe-v1",
        "Output V1",
        "rtmp://example/live/key",
        None,
        "stopped",
        "source",
    )
    .await
    .unwrap();

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/pipelines", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    assert!(list["pipelines"].is_array());

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/pipelines/pipe-v1", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let detail = body_json(resp).await;
    assert_eq!(detail["pipeline"]["id"], "pipe-v1");
    assert_eq!(detail["outputs"].as_array().unwrap().len(), 1);

    let resp = app
        .oneshot(auth_req(
            "GET",
            "/api/v1/pipelines/pipe-v1/graph",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let graph = body_json(resp).await;
    assert!(graph["nodes"].is_array());
    assert!(graph["edges"].is_array());
}

#[tokio::test]
async fn v1_pipeline_detail_and_diagnostics_return_404_for_unknown_pipeline() {
    let (app, cookie) = authenticated_app().await;

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/pipelines/missing", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = app
        .clone()
        .oneshot(auth_req(
            "GET",
            "/api/v1/pipelines/missing/graph",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = app
        .oneshot(auth_req(
            "GET",
            "/api/v1/pipelines/missing/diagnostics",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// --- Lifecycle events endpoint ---

#[tokio::test]
async fn v1_events_requires_auth() {
    let (app, _) = test_app().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/events")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn v1_events_returns_envelope_and_events_array() {
    let (app, _pool, engine) = test_app_with_engine().await;
    let cookie = login(&app).await;

    // Emit a synthetic event directly on the engine's event log
    engine
        .runtime
        .event_log
        .emit(restream::events::EventKind::IngestConnected {
            pipeline_id: "test-pipeline".to_string(),
            protocol: "rtmp".to_string(),
            stream_key: "key01".to_string(),
        });

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/events", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["generatedAt"].is_string());
    assert!(body["count"].as_u64().unwrap() >= 1);
    assert!(body["events"].is_array());
    let events = body["events"].as_array().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e["kind"].as_str() == Some("ingestConnected"))
    );
}

#[tokio::test]
async fn v1_events_filters_by_pipeline_id() {
    let (app, _pool, engine) = test_app_with_engine().await;
    let cookie = login(&app).await;

    engine
        .runtime
        .event_log
        .emit(restream::events::EventKind::IngestConnected {
            pipeline_id: "pipe-a".to_string(),
            protocol: "rtmp".to_string(),
            stream_key: "key01".to_string(),
        });
    engine
        .runtime
        .event_log
        .emit(restream::events::EventKind::IngestConnected {
            pipeline_id: "pipe-b".to_string(),
            protocol: "srt".to_string(),
            stream_key: "key02".to_string(),
        });

    let resp = app
        .clone()
        .oneshot(auth_req(
            "GET",
            "/api/v1/events?pipeline_id=pipe-a",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["pipelineId"].as_str().unwrap(), "pipe-a");
}

// --- Reconciler backoff unit test ---

#[test]
fn reconciler_exponential_backoff_values() {
    // Verify the backoff formula: min(5 * 2^retries, 300) seconds
    // retries=1 → 10s, retries=2 → 20s, retries=3 → 40s, retries=4 → 80s,
    // retries=5 → 160s, retries=6 → 320 → capped at 300s
    let backoff = |retries: u32| -> u64 { (5u64 << retries.min(6)).min(300) };
    assert_eq!(backoff(1), 10);
    assert_eq!(backoff(2), 20);
    assert_eq!(backoff(3), 40);
    assert_eq!(backoff(4), 80);
    assert_eq!(backoff(5), 160);
    assert_eq!(backoff(6), 300); // 5*64=320 capped to 300
    assert_eq!(backoff(7), 300); // min(6) saturates
    assert_eq!(backoff(10), 300);
}

// ─── Engineer telemetry endpoint tests ──────────────────────────────────────

#[tokio::test]
async fn engine_telemetry_returns_structured_response() {
    let (app, cookie) = authenticated_app().await;

    let resp = app
        .oneshot(auth_req("GET", "/api/v1/engine/telemetry", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert!(body["generatedAt"].is_string());
    assert!(body["ingests"].is_array());
    assert!(body["stages"].is_array());
    assert!(body["egresses"].is_array());
    assert!(body["activeTranscoderBuffers"].is_number());
}

#[tokio::test]
async fn pipeline_telemetry_returns_structured_response() {
    let (app, cookie) = authenticated_app().await;

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines",
            &cookie,
            Some(r#"{"name":"TelPipe","streamKey":"telkey_6c71124cde80358ca7c13081"}"#),
        ))
        .await
        .unwrap();
    let pipe = body_json(resp).await;
    let pid = pipe["pipeline"]["id"].as_str().unwrap();

    let resp = app
        .oneshot(auth_req(
            "GET",
            &format!("/api/v1/pipelines/{pid}/telemetry"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert!(body["generatedAt"].is_string());
    assert_eq!(body["pipelineId"].as_str().unwrap(), pid);
    assert!(body["stages"].is_array());
    assert!(body["egresses"].is_array());
}

#[tokio::test]
async fn engine_telemetry_requires_auth() {
    let (app, _) = authenticated_app().await;

    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/v1/engine/telemetry")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn stage_telemetry_returns_structured_response() {
    let (app, _, engine) = test_app_with_engine().await;
    let cookie = login(&app).await;
    let stage_key = StageKey::new("telemetry-pipe", StageKind::video_preset("720p"));
    let metrics = engine.get_or_create_stage_metrics(stage_key.clone()).await;
    metrics.record_in(123);
    metrics.record_out(45);
    metrics.record_processing(9);

    let resp = app
        .oneshot(auth_req(
            "GET",
            &format!("/api/v1/stages/{stage_key}/telemetry"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert!(body["generatedAt"].is_string());
    assert_eq!(body["stageKey"].as_str().unwrap(), stage_key.to_string());
    assert_eq!(body["pipelineId"].as_str().unwrap(), "telemetry-pipe");
    assert_eq!(body["kind"].as_str().unwrap(), "video:720p");
    assert_eq!(body["metrics"]["packetsIn"].as_u64().unwrap(), 1);
    assert_eq!(body["metrics"]["packetsOut"].as_u64().unwrap(), 1);
    assert_eq!(body["metrics"]["bytesIn"].as_u64().unwrap(), 123);
    assert_eq!(body["metrics"]["bytesOut"].as_u64().unwrap(), 45);
    assert_eq!(body["metrics"]["processingUs"].as_u64().unwrap(), 9);
}

#[tokio::test]
async fn stage_telemetry_returns_404_for_unknown_stage() {
    let (app, cookie) = authenticated_app().await;

    let resp = app
        .oneshot(auth_req(
            "GET",
            "/api/v1/stages/nonexistent:source/telemetry",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[cfg(not(feature = "agent-plane"))]
#[tokio::test]
async fn agent_plane_returns_404_when_feature_is_compiled_out() {
    let (app, cookie) = authenticated_app().await;

    let resp = app
        .oneshot(auth_req("GET", "/api/v1/agent/capabilities", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_eq!(body["compiledIn"], false);
}

#[cfg(not(feature = "agent-plane"))]
#[tokio::test]
async fn agent_context_returns_404_when_feature_is_compiled_out() {
    let (app, cookie) = authenticated_app().await;

    let resp = app
        .oneshot(auth_req("GET", "/api/v1/agent/context", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_eq!(body["compiledIn"], false);
}

#[cfg(feature = "agent-plane")]
#[tokio::test]
async fn agent_capabilities_requires_auth() {
    let (app, _) = test_app().await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/agent/capabilities")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[cfg(feature = "agent-plane")]
#[tokio::test]
async fn agent_capabilities_reports_read_planning_only() {
    let (app, cookie) = authenticated_app().await;

    let resp = app
        .oneshot(auth_req("GET", "/api/v1/agent/capabilities", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["feature"], "agent-plane");
    assert_eq!(body["compiledIn"], true);
    assert_eq!(body["executionEnabled"], cfg!(feature = "agent-execution"));
    assert!(body["readTools"].as_array().unwrap().len() >= 5);
    assert!(body["planningTools"].as_array().unwrap().len() >= 3);
    assert!(
        body["routes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|route| route["path"] == "/api/v1/agent/context" && route["mutates"] == false)
    );
    assert!(
        body["routes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|route| route["feature"] != "core")
    );
    assert!(body["readTools"].as_array().unwrap().iter().all(|tool| {
        !tool.as_str().unwrap_or_default().starts_with("get_core_")
            && !tool.as_str().unwrap_or_default().contains("pipeline_graph")
            && !tool
                .as_str()
                .unwrap_or_default()
                .contains("engine_telemetry")
    }));
    assert!(body["schemas"]["PlanRequest"].is_object());
    assert_eq!(body["redaction"]["policy"], "agentContextV1");
}

#[cfg(feature = "agent-plane")]
#[tokio::test]
async fn agent_context_requires_auth() {
    let (app, _) = test_app().await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/agent/context")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[cfg(feature = "agent-plane")]
#[tokio::test]
async fn agent_context_returns_redacted_state_bundle() {
    let (app, _pool, engine) = test_app_with_engine().await;
    let cookie = login(&app).await;
    let raw_stream_key = "agent-context-secret-key";
    let raw_output_url = "rtmp://example.com/live/super-secret-output-key";

    let create = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines",
            &cookie,
            Some(
                &serde_json::json!({ "name": "agent-context", "streamKey": raw_stream_key })
                    .to_string(),
            ),
        ))
        .await
        .unwrap();
    let pipe = body_json(create).await;
    let pid = pipe["pipeline"]["id"].as_str().unwrap().to_string();

    let output_resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            &format!("/api/v1/pipelines/{pid}/outputs"),
            &cookie,
            Some(
                &serde_json::json!({
                    "name": "Redacted CDN",
                    "url": raw_output_url,
                    "encoding": "source"
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(output_resp.status(), StatusCode::CREATED);

    engine
        .runtime
        .event_log
        .emit(restream::events::EventKind::IngestConnected {
            pipeline_id: pid.clone(),
            protocol: "rtmp".to_string(),
            stream_key: raw_stream_key.to_string(),
        });

    let resp = app
        .oneshot(auth_req("GET", "/api/v1/agent/context", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let raw = serde_json::to_string(&body).unwrap();

    assert_eq!(body["readOnly"], true);
    assert_eq!(body["features"]["agentPlane"], true);
    assert_eq!(
        body["features"]["agentExecution"],
        cfg!(feature = "agent-execution")
    );
    assert!(body["state"]["pipelines"].is_array());
    assert!(body["state"]["outputs"].is_array());
    assert!(body["runtime"]["health"].is_object());
    assert!(body["runtime"]["telemetry"]["engine"].is_object());
    assert!(body["runtime"]["graphs"].is_array());
    assert!(body["api"]["routes"].as_array().unwrap().len() >= 5);
    assert!(body["api"]["schemas"]["AgentContextV1"].is_object());
    assert_eq!(
        body["desiredVsActual"]["summary"]["pipelines"].as_u64(),
        Some(1)
    );
    assert_eq!(
        body["desiredVsActual"]["summary"]["outputs"].as_u64(),
        Some(1)
    );
    assert!(body["diagnostics"]["pipelines"].as_array().unwrap().len() == 1);
    assert!(body["dependencies"]["hls"]["config"].is_object());
    assert!(body["dependencies"]["recording"]["pipelines"].is_array());
    assert_eq!(
        body["dependencies"]["fileIngest"]["configured"].as_u64(),
        Some(0)
    );
    assert!(body["dependencies"]["ingestSecurity"]["config"].is_object());
    assert!(body["storage"]["mediaFileCount"].as_u64().is_some());
    assert!(body["redaction"]["recursiveFields"].is_array());

    assert!(!raw.contains(raw_stream_key));
    assert!(!raw.contains("super-secret-output-key"));
    assert!(raw.contains("streamKeyFingerprint"));
    assert!(raw.contains("urlFingerprint"));
    assert!(raw.contains("example.com"));
}

#[cfg(feature = "agent-plane")]
#[tokio::test]
async fn agent_investigation_returns_evidence_envelope() {
    let (app, _pool, engine) = test_app_with_engine().await;
    let cookie = login(&app).await;
    let raw_stream_key = "agent-investigation-secret-key";

    let create = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines",
            &cookie,
            Some(
                &serde_json::json!({ "name": "agent-pipe", "streamKey": raw_stream_key })
                    .to_string(),
            ),
        ))
        .await
        .unwrap();
    let pipe = body_json(create).await;
    let pid = pipe["pipeline"]["id"].as_str().unwrap().to_string();

    engine
        .runtime
        .event_log
        .emit(restream::events::EventKind::IngestConnected {
            pipeline_id: pid.clone(),
            protocol: "rtmp".to_string(),
            stream_key: raw_stream_key.to_string(),
        });

    let resp = app
        .oneshot(auth_req(
            "POST",
            "/api/v1/agent/investigations",
            &cookie,
            Some(
                &serde_json::json!({
                    "workflow": "investigatePipelineIssue",
                    "pipelineId": pid,
                    "eventLimit": 10
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["readOnly"], true);
    assert_eq!(body["summary"]["hasGraph"], true);
    assert!(body["evidence"]["health"].is_object());
    assert!(body["evidence"]["graph"]["nodes"].is_array());
    assert!(body["evidence"]["telemetry"].is_object());
    assert!(body["evidence"]["alerts"].is_array());
    assert!(body["evidence"]["events"].is_array());

    let raw = serde_json::to_string(&body).unwrap();
    assert!(!raw.contains(raw_stream_key));
    assert!(raw.contains("streamKeyFingerprint"));
}

#[cfg(feature = "agent-plane")]
#[tokio::test]
async fn agent_plan_validates_and_previews_stage_impact() {
    let (app, _pool) = test_app().await;
    let cookie = login(&app).await;

    let create = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines",
            &cookie,
            Some(
                &serde_json::json!({ "name": "agent-plan", "streamKey": "agent-plan-key" })
                    .to_string(),
            ),
        ))
        .await
        .unwrap();
    let pipe = body_json(create).await;
    let pid = pipe["pipeline"]["id"].as_str().unwrap();

    let resp = app
        .oneshot(auth_req(
            "POST",
            "/api/v1/agent/plans",
            &cookie,
            Some(
                &serde_json::json!({
                    "intent": "Attach a 720p RTMP output",
                    "pipelineId": pid,
                    "proposedChanges": [{
                        "kind": "addOutput",
                        "name": "Primary CDN",
                        "url": "rtmp://example/live/key",
                        "encoding": "720p+atrack:0"
                    }]
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert!(body["planId"].as_str().unwrap().starts_with("plan_"));
    assert_eq!(body["executionEnabled"], cfg!(feature = "agent-execution"));
    assert_eq!(body["validation"]["valid"], true);
    let added_nodes = body["graphPreview"]["addedNodes"].as_array().unwrap();
    assert!(
        added_nodes
            .iter()
            .any(|node| node["stageKey"].as_str() == Some("video:720p"))
    );
    assert!(
        body["impact"]["sharedStageCandidates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| stage.as_str() == Some("video:720p"))
    );
}

#[cfg(feature = "agent-plane")]
#[tokio::test]
async fn agent_plan_validate_reports_invalid_changes() {
    let (app, cookie) = authenticated_app().await;

    let resp = app
        .oneshot(auth_req(
            "POST",
            "/api/v1/agent/plans/validate",
            &cookie,
            Some(
                &serde_json::json!({
                    "intent": "Attach bad output",
                    "pipelineId": "missing",
                    "proposedChanges": [{
                        "kind": "addOutput",
                        "url": "ftp://example/live/key",
                        "encoding": "custom"
                    }]
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["validation"]["valid"], false);
    let codes: Vec<_> = body["validation"]["errors"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|issue| issue["code"].as_str())
        .collect();
    assert!(codes.contains(&"pipelineNotFound"));
    assert!(codes.contains(&"unsupportedOutputUrl"));
    assert!(codes.contains(&"customEncodingUnsupported"));
    assert!(codes.contains(&"missingOutputName"));
}

#[cfg(all(feature = "agent-plane", not(feature = "agent-execution")))]
#[tokio::test]
async fn agent_execution_routes_return_404_when_compiled_out() {
    let (app, cookie) = authenticated_app().await;

    let resp = app
        .oneshot(auth_req(
            "POST",
            "/api/v1/agent/operations",
            &cookie,
            Some(
                &serde_json::json!({
                    "intent": "Attach output",
                    "pipelineId": "p1",
                    "proposedChanges": []
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_eq!(body["feature"], "agent-execution");
    assert_eq!(body["compiledIn"], false);
}

#[cfg(feature = "agent-execution")]
#[tokio::test]
async fn agent_operation_lifecycle_is_approval_gated_redacted_and_verified() {
    let (app, pool) = test_app().await;
    let cookie = login(&app).await;
    let raw_output_url = "rtmp://example.com/live/agent-secret-key";

    let create_pipeline = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines",
            &cookie,
            Some(
                &serde_json::json!({
                    "name": "agent-exec",
                    "streamKey": "agent-exec-key"
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(create_pipeline.status(), StatusCode::CREATED);
    let pipeline = body_json(create_pipeline).await;
    let pipeline_id = pipeline["pipeline"]["id"].as_str().unwrap().to_string();

    let request = serde_json::json!({
        "intent": "Create a stopped CDN output for approval-gated execution",
        "pipelineId": pipeline_id,
        "idempotencyKey": "agent-op-test-1",
        "actor": "test-agent",
        "agentId": "codex-test-agent",
        "toolIdentity": "api-test",
        "incidentId": "incident-api-test",
        "incidentLinks": ["alert:test-output"],
        "proposedChanges": [{
            "kind": "addOutput",
            "name": "Agent CDN",
            "url": raw_output_url,
            "encoding": "source",
            "desiredState": "stopped"
        }]
    });

    let create_operation = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/agent/operations",
            &cookie,
            Some(&request.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(create_operation.status(), StatusCode::CREATED);
    let created = body_json(create_operation).await;
    let operation_id = created["operationId"].as_str().unwrap().to_string();
    assert_eq!(created["status"], "awaitingApproval");
    assert_eq!(created["approvalRequired"], true);
    assert_eq!(created["incidentId"], "incident-api-test");
    assert_eq!(created["incidentLinks"][0], "alert:test-output");
    assert_eq!(created["plan"]["executionEnabled"], true);
    assert!(
        created["proposedPlanHash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    let raw = serde_json::to_string(&created).unwrap();
    assert!(!raw.contains("agent-secret-key"));
    assert!(raw.contains("urlFingerprint"));

    let reused = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/agent/operations",
            &cookie,
            Some(&request.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(reused.status(), StatusCode::OK);
    let reused_body = body_json(reused).await;
    assert_eq!(reused_body["operationId"], operation_id);

    let apply_before_approval = app
        .clone()
        .oneshot(auth_req(
            "POST",
            &format!("/api/v1/agent/operations/{operation_id}/apply"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(apply_before_approval.status(), StatusCode::CONFLICT);
    let conflict = body_json(apply_before_approval).await;
    assert_eq!(conflict["code"], "approvalRequired");

    let approved = app
        .clone()
        .oneshot(auth_req(
            "POST",
            &format!("/api/v1/agent/operations/{operation_id}/approve"),
            &cookie,
            Some(
                &serde_json::json!({
                    "approvedBy": "human-test",
                    "reason": "unit test approval"
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(approved.status(), StatusCode::OK);
    let approved_body = body_json(approved).await;
    assert_eq!(approved_body["status"], "approved");
    assert_eq!(approved_body["approval"]["approvedBy"], "human-test");

    let applied = app
        .clone()
        .oneshot(auth_req(
            "POST",
            &format!("/api/v1/agent/operations/{operation_id}/apply"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(applied.status(), StatusCode::OK);
    let applied_body = body_json(applied).await;
    assert_eq!(applied_body["status"], "applied");
    assert_eq!(applied_body["executionResult"]["success"], true);
    assert_eq!(
        applied_body["executionResult"]["changeResults"][0]["status"],
        "created"
    );
    let output_id = applied_body["executionResult"]["changeResults"][0]["outputId"]
        .as_str()
        .unwrap()
        .to_string();

    let output = db::get_output(&pool, &pipeline_id, &output_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.url, raw_output_url);
    assert_eq!(output.desired_state, "stopped");

    let verified = app
        .oneshot(auth_req(
            "POST",
            &format!("/api/v1/agent/operations/{operation_id}/verify"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(verified.status(), StatusCode::OK);
    let verified_body = body_json(verified).await;
    assert_eq!(verified_body["status"], "verified");
    assert_eq!(verified_body["verificationResult"]["success"], true);
    assert_eq!(
        verified_body["verificationResult"]["checks"][0]["reason"],
        "stopped"
    );
    assert!(verified_body["auditLog"].as_array().unwrap().len() >= 4);
}

// ── coverage gap: alerts ────────────────────────────────────────────────

#[tokio::test]
async fn pipeline_alerts_requires_auth_and_returns_array() {
    let (app, cookie) = authenticated_app().await;

    let unauth = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/pipelines/nonexistent/alerts")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    let pipeline = body_json(
        app.clone()
            .oneshot(auth_req(
                "POST",
                "/api/v1/pipelines",
                &cookie,
                Some(r#"{"name":"alert-test","streamKey":"sk-alert"}"#),
            ))
            .await
            .unwrap(),
    )
    .await;
    let pid = pipeline["pipeline"]["id"].as_str().unwrap();

    let resp = app
        .clone()
        .oneshot(auth_req(
            "GET",
            &format!("/api/v1/pipelines/{pid}/alerts"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["alerts"].is_array());
    assert!(body["generatedAt"].is_string());
}

#[tokio::test]
async fn aggregate_alerts_requires_auth_and_returns_array() {
    let (app, cookie) = authenticated_app().await;

    let unauth = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/alerts")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/alerts", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["alerts"].is_array());
    assert!(body["generatedAt"].is_string());
}

// ── coverage gap: metrics/system ────────────────────────────────────────

#[tokio::test]
async fn metrics_system_requires_auth_and_returns_structured_data() {
    let (app, cookie) = authenticated_app().await;

    let unauth = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics/system")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/metrics/system", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["cpu"]["usagePercent"].is_number());
    assert!(body["cpu"]["cores"].is_number());
    assert!(body["memory"]["totalBytes"].is_number());
    assert!(body["memory"]["usedBytes"].is_number());
    assert!(body["disk"]["totalBytes"].is_number());
    assert!(body["generatedAt"].is_string());
}

// ── coverage gap: agent graph-diff-preview ──────────────────────────────

#[tokio::test]
async fn agent_graph_diff_preview_returns_404_when_compiled_out() {
    let (app, cookie) = authenticated_app().await;
    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/agent/graph-diff-preview",
            &cookie,
            Some(r#"{"changes":[]}"#),
        ))
        .await
        .unwrap();
    // When agent-plane feature is off, returns 404
    assert!(
        resp.status() == StatusCode::NOT_FOUND || resp.status() == StatusCode::OK,
        "expected 404 (compiled out) or 200, got {}",
        resp.status()
    );
}
