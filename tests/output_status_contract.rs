//! Focused contract tests for live output runtime status payloads.

use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use restream::domain::ingest_security::DEFAULT_INGEST_SECURITY_CONFIG;
use restream::domain::srt_ingest::SrtGlobalIngestConfig;
use restream::media::engine::MediaEngine;
use restream::media::security::IngestSecurityService;
use restream::{api, db};
use sqlx::SqlitePool;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::{RwLock as TokioRwLock, broadcast};
use tower::ServiceExt;

async fn test_app_with_engine() -> (axum::Router, SqlitePool, Arc<MediaEngine>) {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    db::setup_database_schema(&pool).await.unwrap();

    let sessions = Arc::new(TokioRwLock::new(HashSet::new()));
    api::initialize_auth(&pool, &sessions).await;

    let security = Arc::new(IngestSecurityService::new(DEFAULT_INGEST_SECURITY_CONFIG));
    let ingest_policy_store = Arc::new(restream::media::srt::SrtIngestPolicyStore::new(
        SrtGlobalIngestConfig::default(),
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
        ingest_disconnect_grace_ms: restream::RuntimeTuning::default().ingest_disconnect_grace_ms,
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

#[tokio::test]
async fn active_output_status_matches_health_runtime_fields() {
    let (app, _, engine) = test_app_with_engine().await;
    let cookie = login(&app).await;

    let resp = app
        .clone()
        .oneshot(auth_req(
            "POST",
            "/api/v1/pipelines",
            &cookie,
            Some(r#"{"name":"P","streamKey":"key01_6c71124cde80358ca7c13086"}"#),
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
        .try_register_ingest(&pid, "key01_6c71124cde80358ca7c13086", "rtmp")
        .await
        .expect("ingest registration should succeed");
    engine
        .register_egress(&oid, &pid, "rtmp://dest/live/k")
        .await;
    engine
        .update_egress_target_addr(&oid, "203.0.113.10:1935".to_string())
        .await;
    engine.update_egress_phase(&oid, "sending").await;
    engine.record_egress_progress(&oid, 4096).await;
    {
        let egresses = engine.egresses.active.read().await;
        let active = egresses.get(&oid).expect("active egress");
        active.prev_bytes_sent.store(0, Ordering::Relaxed);
        *active
            .prev_sample_time
            .lock()
            .unwrap_or_else(|e| e.into_inner()) =
            std::time::Instant::now() - std::time::Duration::from_secs(1);
    }

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
    assert_eq!(status["status"], "running");
    assert_eq!(status["phase"], "sending");
    assert_eq!(status["targetAddr"], "203.0.113.10:1935");
    assert_eq!(status["bytesOut"], 4096);
    assert_eq!(status["totalSize"], 4096);
    assert!(status["startedAt"].is_string());
    assert!(status["lastProgressAt"].is_string());
    assert!(status["lastProgressAgeMs"].as_u64().is_some());
    assert!(
        status["bitrateKbps"].as_f64().unwrap_or_default() > 0.0,
        "live output status should surface a positive bitrate once progress has been sampled"
    );

    let resp = app
        .clone()
        .oneshot(auth_req("GET", "/api/v1/engine/health", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let health = body_json(resp).await;
    let output = &health["pipelines"][&pid]["outputs"][&oid];
    assert_eq!(output["status"], "running");
    assert_eq!(output["phase"], "sending");
    assert_eq!(output["targetAddr"], "203.0.113.10:1935");
    assert_eq!(output["bytesOut"], 4096);
    assert_eq!(output["totalSize"], 4096);
    assert_eq!(output["startedAt"], status["startedAt"]);
    assert_eq!(output["lastProgressAt"], status["lastProgressAt"]);
    assert_eq!(output["bitrateKbps"], status["bitrateKbps"]);
}
