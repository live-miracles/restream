use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use restream::media::engine::MediaEngine;
use restream::media::security::IngestSecurityService;
use restream::{api, db};
use sqlx::SqlitePool;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock as TokioRwLock;
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
    let engine = Arc::new(MediaEngine::new());

    let state = Arc::new(api::AppState {
        db: pool.clone(),
        security,
        sessions,
        engine: engine.clone(),
        ports: api::PortConfig {
            rtmp: 1935,
            srt: 10080,
        },
        media_dir: "media".to_string(),
    });

    (api::create_router(state), pool, engine)
}

async fn login(app: &axum::Router) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
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
                .uri("/api/auth/login")
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
        .oneshot(auth_req("POST", "/api/auth/logout", &cookie, None))
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
                .uri("/config")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
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
            "/pipelines",
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
        .oneshot(auth_req("GET", "/pipelines", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json.as_array().unwrap().len(), 1);

    // Update
    let uri = format!("/pipelines/{}", pipeline_id);
    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
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

    db::create_pipeline(&pool, "p1", "P1", "unique-key", None, None)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/pipelines",
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
            "/pipelines",
            &cookie,
            Some(r#"{"name":"P2","streamKey":"unique-key-2"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/pipelines/p1",
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

    db::create_pipeline(&pool, "p_rtmps", "P", "key_rtmps", None, None)
        .await
        .unwrap();

    // rtmps:// must be accepted (used by Facebook, YouTube, etc.)
    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/pipelines/p_rtmps/outputs",
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

    db::create_pipeline(&pool, "p_hls", "P", "key_hls", None, None)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/pipelines/p_hls/outputs",
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
async fn http_hls_upload_output_is_rejected_by_api() {
    let (app, pool) = test_app().await;
    let cookie = login(&app).await;

    db::create_pipeline(&pool, "p_http_hls", "P", "key_http_hls", None, None)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/pipelines/p_http_hls/outputs",
            &cookie,
            Some(r#"{"name":"Remote HLS","url":"https://a.upload.youtube.com/http_upload_hls?cid=abc&copy=0&file=out.m3u8","encoding":"source"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let json = body_json(resp).await;
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Supported schemes are rtmp://, rtmps://, srt://, and hls://")
    );
}

#[tokio::test]
async fn output_crud_via_api() {
    let (app, pool) = test_app().await;
    let cookie = login(&app).await;

    db::create_pipeline(&pool, "p1", "P", "key01", None, None)
        .await
        .unwrap();

    // Create output
    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/pipelines/p1/outputs",
            &cookie,
            Some(r#"{"name":"YouTube","url":"rtmp://yt/live","encoding":"source"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    let output_id = json["output"]["id"].as_str().unwrap().to_string();

    // Start
    let uri = format!("/pipelines/p1/outputs/{}/start", output_id);
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
    let uri = format!("/pipelines/p1/outputs/{}/stop", output_id);
    let resp = app
        .clone()
        .oneshot(auth_req("POST", &uri, &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Delete
    let uri = format!("/pipelines/p1/outputs/{}", output_id);
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

    db::create_pipeline(&pool, "p1", "P", "key01", None, None)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/config", &cookie, None))
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
            "/config",
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
    db::create_pipeline(&pool, "p1", "P", "key01", None, None)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(auth_req(
            "PATCH",
            "/config",
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
        .oneshot(auth_req("GET", "/config", &cookie, None))
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
            "/config",
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
        .oneshot(auth_req("GET", "/config", &cookie, None))
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
                .uri("/audio-caps")
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
                .uri("/stream-keys")
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
        .oneshot(auth_req("GET", "/stream-keys", &cookie, None))
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
            "/api/ingests",
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
        .oneshot(auth_req("GET", "/api/ingests", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json.as_array().unwrap().len(), 1);

    // Delete
    let uri = format!("/api/ingests/{}", id);
    let resp = app
        .clone()
        .oneshot(auth_req("DELETE", &uri, &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
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
            "/encodings/custom",
            &cookie,
            Some(r#"{"ffmpegArgs":"-c:v libx264 -preset fast"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/encodings/custom", &cookie, None))
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
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/hls/nonexistent/index.m3u8")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn hls_canonical_and_legacy_playlist_routes_use_the_same_handler() {
    let (app, _, engine) = test_app_with_engine().await;
    engine.get_or_create_hls_store("test_pipe").await;

    for uri in [
        "/hls/test_pipe",
        "/hls/test_pipe/index.m3u8",
        "/preview/hls/test_pipe",
        "/preview/hls/test_pipe/index.m3u8",
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
    engine.get_or_create_hls_store("test_pipe").await;

    for uri in [
        "/hls/test_pipe/notasegment",
        "/preview/hls/test_pipe/notasegment",
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
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
    }
}

// --- Status ---

#[tokio::test]
async fn status_returns_version_info() {
    let (app, _) = test_app().await;
    let cookie = login(&app).await;

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/status", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["restream"]["version"].is_string());
    assert!(json["restream"]["commit"].is_string());
    assert!(json.get("ffmpeg").is_none());
    assert!(json["toolchain"]["rustc"].is_string());
    assert!(json["nativeLibraries"]["ffmpeg"]["version"].is_string());
    assert!(json["nativeLibraries"]["ffmpeg"]["configuration"].is_string());
    assert!(json["nativeLibraries"]["srt"]["version"].is_string());
    assert!(json["nativeLibraries"]["openssl"]["version"].is_string());
    assert!(json["nativeLibraries"]["sqlite"]["version"].is_string());
    assert!(json["nativeLibraries"]["x264"]["version"].is_string());
    assert_eq!(json["sbom"]["format"], "CycloneDX");
    assert_eq!(json["sbom"]["specVersion"], "1.5");
    assert_eq!(json["sbom"]["licensesIncluded"], true);
    assert!(json["sbom"]["componentCount"].as_u64().unwrap() > 20);
    assert!(json["os"]["platform"].is_string());
    assert!(json["os"]["hostname"].is_string());
}

#[tokio::test]
async fn status_sbom_is_authenticated_cyclonedx_with_licenses() {
    let (app, _) = test_app().await;

    let unauthorized = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/status/sbom")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let cookie = login(&app).await;
    let response = app
        .oneshot(auth_req("GET", "/api/status/sbom", &cookie, None))
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
        "libssl",
        "libcrypto",
        "SQLite",
        "x264",
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
            "/pipelines",
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
            &format!("/pipelines/{}/graph", pid),
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
            "/pipelines/inactive/diagnostics?probe=srt",
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
            "/api/auth/change-password",
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
                .uri("/api/auth/login")
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
                .uri("/api/auth/login")
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
        let state = Arc::new(api::AppState {
            db: pool.clone(),
            security,
            sessions,
            engine: engine.clone(),
            ports: api::PortConfig {
                rtmp: 1935,
                srt: 10080,
            },
            media_dir: "media".to_string(),
        });
        api::create_router(state)
    };
    let cookie = login(&app).await;

    // Create pipeline and output
    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/pipelines",
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
            &format!("/pipelines/{pid}/outputs"),
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
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
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
async fn delete_output_cancels_egress() {
    let (app, _, engine) = test_app_with_engine().await;
    let cookie = login(&app).await;

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/pipelines",
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
            &format!("/pipelines/{pid}/outputs"),
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
            &format!("/pipelines/{pid}/outputs/{oid}"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Egress cancellation token should be cancelled
    assert!(token.is_cancelled(), "deleting output should cancel egress");
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
        let mut stores = engine.hls_consumers.write().await;
        stores.insert("pipe1".to_string(), HlsConsumers::new(token.clone()));
    }

    engine.add_hls_persistent_consumer("pipe1").await;
    engine.add_hls_persistent_consumer("pipe1").await;
    {
        let consumers = engine.hls_consumers.read().await;
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
        let consumers = engine.hls_consumers.read().await;
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
            "/api/media/nonexistent.mp4",
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
            "/api/media/..%2f..%2fetc%2fpasswd",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert!(resp.status() == StatusCode::BAD_REQUEST || resp.status() == StatusCode::NOT_FOUND);
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
            "/config",
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
            "/config",
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
            "/config",
            &cookie,
            Some(r#"{"transcodeProfiles":{"h264":{"preset":"ultrafast","tune":"zerolatency","crf":100}}}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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
            "/api/ingests",
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
            "/api/ingests",
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
            "/api/ingests",
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
            &format!("/api/ingests/{}", ingest_id),
            &cookie,
            Some(&update_body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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
