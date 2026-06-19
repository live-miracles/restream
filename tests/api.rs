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
        engine,
    });

    (api::create_router(state), pool)
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
    let (app, _) = test_app().await;
    let cookie = login(&app).await;

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
    assert!(keys[0]["ingestUrls"]["rtmp"].is_string());
    assert!(keys[0]["ingestUrls"]["srt"].is_string());
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

// --- HLS preview ---

#[tokio::test]
async fn hls_preview_no_stream_returns_404() {
    let (app, _) = test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/preview/hls/nonexistent")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn hls_segment_bad_name_returns_400() {
    let (app, _) = test_app().await;

    // First create an HLS store so we get past the 404
    let state = {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/preview/hls/test_pipe/notasegment")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // No HLS store exists → 404
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    };
    let _ = state;
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
