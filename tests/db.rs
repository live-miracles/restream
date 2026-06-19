use restream::db;
use sqlx::SqlitePool;

async fn test_pool() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    db::setup_database_schema(&pool).await.unwrap();
    pool
}

#[tokio::test]
async fn pipeline_crud() {
    let pool = test_pool().await;

    let p = db::create_pipeline(&pool, "p1", "Test Pipeline", "key01", None, None)
        .await
        .unwrap();
    assert_eq!(p.id, "p1");
    assert_eq!(p.name, "Test Pipeline");
    assert_eq!(p.stream_key, "key01");
    assert!(p.input_source.is_none());

    let fetched = db::get_pipeline(&pool, "p1").await.unwrap().unwrap();
    assert_eq!(fetched.name, "Test Pipeline");

    let all = db::list_pipelines(&pool).await.unwrap();
    assert_eq!(all.len(), 1);

    let updated = db::update_pipeline(&pool, "p1", "Renamed", "key02", Some("file.mp4"), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.name, "Renamed");
    assert_eq!(updated.stream_key, "key02");
    assert_eq!(updated.input_source.as_deref(), Some("file.mp4"));

    assert!(db::delete_pipeline(&pool, "p1").await.unwrap());
    assert!(db::get_pipeline(&pool, "p1").await.unwrap().is_none());
}

#[tokio::test]
async fn update_nonexistent_pipeline_returns_none() {
    let pool = test_pool().await;
    let result = db::update_pipeline(&pool, "nope", "x", "k", None, None)
        .await
        .unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn output_crud() {
    let pool = test_pool().await;
    db::create_pipeline(&pool, "p1", "P", "key01", None, None)
        .await
        .unwrap();

    let o = db::create_output(
        &pool,
        "o1",
        "p1",
        "YouTube",
        "rtmp://yt/live",
        "stopped",
        "source",
    )
    .await
    .unwrap();
    assert_eq!(o.id, "o1");
    assert_eq!(o.desired_state, "stopped");

    let fetched = db::get_output(&pool, "p1", "o1").await.unwrap().unwrap();
    assert_eq!(fetched.name, "YouTube");

    let all = db::list_outputs_for_pipeline(&pool, "p1").await.unwrap();
    assert_eq!(all.len(), 1);

    let updated = db::update_output(&pool, "p1", "o1", "Twitch", "rtmp://tw/live", "720p")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.name, "Twitch");
    assert_eq!(updated.encoding, "720p");

    let started = db::set_output_desired_state(&pool, "p1", "o1", "running")
        .await
        .unwrap();
    assert_eq!(started.desired_state, "running");

    assert!(db::delete_output(&pool, "p1", "o1").await.unwrap());
    assert!(db::get_output(&pool, "p1", "o1").await.unwrap().is_none());
}

#[tokio::test]
async fn cascade_delete_removes_outputs() {
    let pool = test_pool().await;
    db::create_pipeline(&pool, "p1", "P", "key01", None, None)
        .await
        .unwrap();
    db::create_output(&pool, "o1", "p1", "Out", "rtmp://x", "stopped", "source")
        .await
        .unwrap();

    db::delete_pipeline(&pool, "p1").await.unwrap();
    let outputs = db::list_outputs(&pool).await.unwrap();
    assert!(outputs.is_empty());
}

#[tokio::test]
async fn job_lifecycle() {
    let pool = test_pool().await;
    db::create_pipeline(&pool, "p1", "P", "key01", None, None)
        .await
        .unwrap();
    db::create_output(&pool, "o1", "p1", "Out", "rtmp://x", "stopped", "source")
        .await
        .unwrap();

    let job = db::create_job(
        &pool,
        "j1",
        "p1",
        "o1",
        Some(1234),
        "running",
        "2024-01-01T00:00:00Z",
    )
    .await
    .unwrap();
    assert_eq!(job.status, "running");
    assert_eq!(job.pid, Some(1234));

    let running = db::get_running_job_for(&pool, "p1", "o1").await.unwrap();
    assert!(running.is_some());

    let updated = db::update_job(
        &pool,
        "j1",
        None,
        Some("stopped"),
        Some("2024-01-01T00:01:00Z"),
        Some(0),
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(updated.status, "stopped");
    assert_eq!(updated.exit_code, Some(0));

    let no_running = db::get_running_job_for(&pool, "p1", "o1").await.unwrap();
    assert!(no_running.is_none());
}

#[tokio::test]
async fn job_upsert_on_conflict() {
    let pool = test_pool().await;
    db::create_pipeline(&pool, "p1", "P", "key01", None, None)
        .await
        .unwrap();
    db::create_output(&pool, "o1", "p1", "Out", "rtmp://x", "stopped", "source")
        .await
        .unwrap();

    db::create_job(
        &pool,
        "j1",
        "p1",
        "o1",
        Some(100),
        "running",
        "2024-01-01T00:00:00Z",
    )
    .await
    .unwrap();
    let replaced = db::create_job(
        &pool,
        "j2",
        "p1",
        "o1",
        Some(200),
        "running",
        "2024-01-01T01:00:00Z",
    )
    .await
    .unwrap();
    assert_eq!(replaced.id, "j2");
    assert_eq!(replaced.pid, Some(200));

    let all = db::list_jobs(&pool).await.unwrap();
    assert_eq!(all.len(), 1);
}

#[tokio::test]
async fn job_logs() {
    let pool = test_pool().await;
    db::create_pipeline(&pool, "p1", "P", "key01", None, None)
        .await
        .unwrap();
    db::create_output(&pool, "o1", "p1", "Out", "rtmp://x", "stopped", "source")
        .await
        .unwrap();
    db::create_job(
        &pool,
        "j1",
        "p1",
        "o1",
        None,
        "running",
        "2024-01-01T00:00:00Z",
    )
    .await
    .unwrap();

    db::append_job_log(
        &pool,
        Some("j1"),
        Some("p1"),
        Some("o1"),
        "lifecycle.start",
        None,
        "2024-01-01T00:00:00Z",
        "Started",
    )
    .await
    .unwrap();
    db::append_job_log(
        &pool,
        Some("j1"),
        Some("p1"),
        Some("o1"),
        "lifecycle.stop",
        None,
        "2024-01-01T00:01:00Z",
        "Stopped",
    )
    .await
    .unwrap();

    let logs = db::list_job_logs(&pool, "j1").await.unwrap();
    assert_eq!(logs.len(), 2);
    assert_eq!(logs[0].message, "Started");

    let by_output = db::list_job_logs_by_output(&pool, "p1", "o1")
        .await
        .unwrap();
    assert_eq!(by_output.len(), 2);
}

#[tokio::test]
async fn ingest_crud() {
    let pool = test_pool().await;

    let i = db::create_ingest(&pool, "i1", "video.mp4", "key01", true, "00:00:05")
        .await
        .unwrap();
    assert_eq!(i.filename, "video.mp4");
    assert!(i.loop_flag);

    let all = db::list_ingests(&pool).await.unwrap();
    assert_eq!(all.len(), 1);

    let updated = db::update_ingest(&pool, "i1", "other.mp4", "key02", false, "")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.filename, "other.mp4");
    assert!(!updated.loop_flag);

    assert!(db::delete_ingest(&pool, "i1").await.unwrap());
    assert!(db::list_ingests(&pool).await.unwrap().is_empty());
}

#[tokio::test]
async fn meta_operations() {
    let pool = test_pool().await;

    assert!(db::get_meta(&pool, "foo").await.unwrap().is_none());

    db::set_meta(&pool, "foo", "bar").await.unwrap();
    assert_eq!(db::get_meta(&pool, "foo").await.unwrap().unwrap(), "bar");

    db::set_meta(&pool, "foo", "baz").await.unwrap();
    assert_eq!(db::get_meta(&pool, "foo").await.unwrap().unwrap(), "baz");
}

#[tokio::test]
async fn session_operations() {
    let pool = test_pool().await;

    db::create_session(&pool, "tok1", 1000).await.unwrap();
    db::create_session(&pool, "tok2", 2000).await.unwrap();

    let sessions = db::list_sessions(&pool).await.unwrap();
    assert_eq!(sessions.len(), 2);

    db::delete_session(&pool, "tok1").await.unwrap();
    let sessions = db::list_sessions(&pool).await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0], "tok2");
}

#[tokio::test]
async fn reset_running_jobs() {
    let pool = test_pool().await;
    db::create_pipeline(&pool, "p1", "P", "key01", None, None)
        .await
        .unwrap();
    db::create_output(&pool, "o1", "p1", "Out", "rtmp://x", "stopped", "source")
        .await
        .unwrap();
    db::create_job(
        &pool,
        "j1",
        "p1",
        "o1",
        Some(999),
        "running",
        "2024-01-01T00:00:00Z",
    )
    .await
    .unwrap();

    db::reset_running_jobs(&pool, "2024-01-01T00:05:00Z")
        .await
        .unwrap();

    let job = db::get_job(&pool, "j1").await.unwrap().unwrap();
    assert_eq!(job.status, "stopped");
    assert_eq!(job.exit_signal.as_deref(), Some("SIGKILL"));
}

#[tokio::test]
async fn filtered_job_logs() {
    let pool = test_pool().await;
    db::create_pipeline(&pool, "p1", "P", "key01", None, None)
        .await
        .unwrap();
    db::create_output(&pool, "o1", "p1", "Out", "rtmp://x", "stopped", "source")
        .await
        .unwrap();

    db::append_job_log(
        &pool,
        Some("j1"),
        Some("p1"),
        Some("o1"),
        "lifecycle.start",
        None,
        "2024-01-01T00:00:00Z",
        "[lifecycle] started",
    )
    .await
    .unwrap();
    db::append_job_log(
        &pool,
        Some("j1"),
        Some("p1"),
        Some("o1"),
        "output",
        None,
        "2024-01-01T00:00:01Z",
        "frame=100",
    )
    .await
    .unwrap();
    db::append_job_log(
        &pool,
        Some("j1"),
        Some("p1"),
        Some("o1"),
        "lifecycle.stop",
        None,
        "2024-01-01T00:00:02Z",
        "[lifecycle] stopped",
    )
    .await
    .unwrap();

    use restream::types::HistoryFilters;
    let filters = HistoryFilters {
        since: None,
        until: None,
        limit: Some(2),
        order: Some("asc".to_string()),
        prefixes: Some(vec!["[lifecycle]".to_string()]),
    };
    let logs = db::list_job_logs_by_output_filtered(&pool, "p1", "o1", &filters)
        .await
        .unwrap();
    assert_eq!(logs.len(), 2);
    assert!(logs[0].message.contains("[lifecycle]"));

    let lifecycle_logs = db::list_lifecycle_logs_by_output(&pool, "p1", "o1")
        .await
        .unwrap();
    assert_eq!(lifecycle_logs.len(), 2);
}
