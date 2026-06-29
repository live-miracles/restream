use restream::{
    db,
    types::{AppLogEntry, AppLogFilters},
};

async fn test_pool() -> sqlx::SqlitePool {
    let pool = db::create_pool("sqlite::memory:").await.unwrap();
    db::setup_database_schema(&pool).await.unwrap();
    pool
}

fn app_log_entry(
    ts: &str,
    pipeline_id: Option<&str>,
    output_id: Option<&str>,
    event_type: Option<&str>,
    event_class: Option<&str>,
    message: &str,
    fields: Option<&str>,
) -> AppLogEntry {
    AppLogEntry {
        ts: ts.to_string(),
        level: "INFO".to_string(),
        target: "restream::tests".to_string(),
        message: message.to_string(),
        fields: fields.map(str::to_string),
        pipeline_id: pipeline_id.map(str::to_string),
        output_id: output_id.map(str::to_string),
        event_type: event_type.map(str::to_string),
        event_class: event_class.map(str::to_string),
    }
}

#[tokio::test]
async fn pipeline_crud() {
    let pool = test_pool().await;

    let p = db::create_pipeline(&pool, "p1", "Test Pipeline", "key01", None, None, None)
        .await
        .unwrap();
    assert_eq!(p.id, "p1");
    assert_eq!(p.name, "Test Pipeline");
    assert_eq!(p.stream_key, "key01");
    assert!(p.input_source.is_none());

    let fetched = db::get_pipeline(&pool, "p1").await.unwrap().unwrap();
    assert_eq!(fetched.name, "Test Pipeline");
    let by_stream_key = db::get_pipeline_by_stream_key(&pool, "key01")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_stream_key.id, "p1");

    let all = db::list_pipelines(&pool).await.unwrap();
    assert_eq!(all.len(), 1);

    let updated = db::update_pipeline(
        &pool,
        "p1",
        "Renamed",
        "key02",
        Some("file.mp4"),
        None,
        None,
    )
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
    let result = db::update_pipeline(&pool, "nope", "x", "k", None, None, None)
        .await
        .unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn output_crud() {
    let pool = test_pool().await;
    db::create_pipeline(&pool, "p1", "P", "key01", None, None, None)
        .await
        .unwrap();

    let o = db::create_output(
        &pool,
        "o1",
        "p1",
        "YouTube",
        "rtmp://yt/live",
        None,
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

    let updated = db::update_output(&pool, "p1", "o1", "Twitch", "rtmp://tw/live", None, "720p")
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
    db::create_pipeline(&pool, "p1", "P", "key01", None, None, None)
        .await
        .unwrap();
    db::create_output(
        &pool, "o1", "p1", "Out", "rtmp://x", None, "stopped", "source",
    )
    .await
    .unwrap();

    db::delete_pipeline(&pool, "p1").await.unwrap();
    let outputs = db::list_outputs(&pool).await.unwrap();
    assert!(outputs.is_empty());
}

#[tokio::test]
async fn job_lifecycle() {
    let pool = test_pool().await;
    db::create_pipeline(&pool, "p1", "P", "key01", None, None, None)
        .await
        .unwrap();
    db::create_output(
        &pool, "o1", "p1", "Out", "rtmp://x", None, "stopped", "source",
    )
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
    db::create_pipeline(&pool, "p1", "P", "key01", None, None, None)
        .await
        .unwrap();
    db::create_output(
        &pool, "o1", "p1", "Out", "rtmp://x", None, "stopped", "source",
    )
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
async fn stale_job_update_cannot_clobber_replacement_attempt() {
    let pool = test_pool().await;
    db::create_pipeline(&pool, "p1", "P", "key01", None, None, None)
        .await
        .unwrap();
    db::create_output(
        &pool, "o1", "p1", "Out", "rtmp://x", None, "stopped", "source",
    )
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

    let replacement = db::create_job(
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
    assert_eq!(replacement.id, "j2");

    let stale_cleanup = db::update_job(
        &pool,
        "j1",
        None,
        Some("failed"),
        Some("2024-01-01T00:05:00Z"),
        Some(1),
        None,
    )
    .await
    .unwrap();
    assert!(stale_cleanup.is_none());

    let running = db::get_running_job_for(&pool, "p1", "o1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(running.id, "j2");
    assert_eq!(running.status, "running");
    assert_eq!(running.pid, Some(200));
    assert!(running.ended_at.is_none());
    assert!(running.exit_code.is_none());
}

#[tokio::test]
async fn app_logs_can_be_queried_by_output_scope() {
    let pool = test_pool().await;
    db::create_pipeline(&pool, "p1", "P", "key01", None, None, None)
        .await
        .unwrap();
    db::create_output(
        &pool, "o1", "p1", "Out", "rtmp://x", None, "stopped", "source",
    )
    .await
    .unwrap();

    db::append_app_log_batch(
        &pool,
        &[
            app_log_entry(
                "2024-01-01T00:00:00Z",
                Some("p1"),
                Some("o1"),
                Some("lifecycle.start"),
                Some("lifecycle"),
                "Started",
                Some(r#"{"jobId":"j1"}"#),
            ),
            app_log_entry(
                "2024-01-01T00:01:00Z",
                Some("p1"),
                Some("o1"),
                Some("lifecycle.stop"),
                Some("lifecycle"),
                "Stopped",
                Some(r#"{"jobId":"j1"}"#),
            ),
        ],
    )
    .await
    .unwrap();

    let logs = db::list_app_logs(
        &pool,
        &AppLogFilters {
            level: Some("info".to_string()),
            since: None,
            until: None,
            target: None,
            scope: None,
            pipeline_id: Some("p1".to_string()),
            output_id: Some("o1".to_string()),
            event_class: None,
            prefix: None,
            limit: Some(10),
            order: Some("asc".to_string()),
        },
    )
    .await
    .unwrap();
    assert_eq!(logs.len(), 2);
    assert_eq!(logs[0].message, "Started");

    let lifecycle_only = db::list_app_logs(
        &pool,
        &AppLogFilters {
            level: Some("info".to_string()),
            since: None,
            until: None,
            target: None,
            scope: None,
            pipeline_id: Some("p1".to_string()),
            output_id: Some("o1".to_string()),
            event_class: Some("lifecycle".to_string()),
            prefix: None,
            limit: Some(10),
            order: Some("asc".to_string()),
        },
    )
    .await
    .unwrap();
    assert_eq!(lifecycle_only.len(), 2);
    assert_eq!(
        lifecycle_only[1].event_type.as_deref(),
        Some("lifecycle.stop")
    );
}

#[tokio::test]
async fn filtered_app_logs_honor_prefix_and_event_class_filters() {
    let pool = test_pool().await;
    db::create_pipeline(&pool, "p1", "P", "key01", None, None, None)
        .await
        .unwrap();
    db::create_output(
        &pool, "o1", "p1", "Out", "rtmp://x", None, "stopped", "source",
    )
    .await
    .unwrap();

    db::append_app_log_batch(
        &pool,
        &[
            app_log_entry(
                "2024-01-01T00:00:00Z",
                Some("p1"),
                Some("o1"),
                Some("lifecycle.start"),
                Some("lifecycle"),
                "[lifecycle] started",
                None,
            ),
            app_log_entry(
                "2024-01-01T00:00:01Z",
                Some("p1"),
                Some("o1"),
                Some("output"),
                Some("status"),
                "frame=100",
                None,
            ),
            app_log_entry(
                "2024-01-01T00:00:02Z",
                Some("p1"),
                Some("o1"),
                Some("lifecycle.stop"),
                Some("lifecycle"),
                "[lifecycle] stopped",
                None,
            ),
        ],
    )
    .await
    .unwrap();

    let logs = db::list_app_logs(
        &pool,
        &AppLogFilters {
            level: Some("info".to_string()),
            since: None,
            until: None,
            target: None,
            scope: None,
            pipeline_id: Some("p1".to_string()),
            output_id: Some("o1".to_string()),
            event_class: None,
            prefix: Some("[lifecycle]".to_string()),
            limit: Some(2),
            order: Some("asc".to_string()),
        },
    )
    .await
    .unwrap();
    assert_eq!(logs.len(), 2);
    assert!(logs[0].message.contains("[lifecycle]"));

    let lifecycle_logs = db::list_app_logs(
        &pool,
        &AppLogFilters {
            level: Some("info".to_string()),
            since: None,
            until: None,
            target: None,
            scope: None,
            pipeline_id: Some("p1".to_string()),
            output_id: Some("o1".to_string()),
            event_class: Some("lifecycle".to_string()),
            prefix: None,
            limit: Some(10),
            order: Some("asc".to_string()),
        },
    )
    .await
    .unwrap();
    assert_eq!(lifecycle_logs.len(), 2);
}

#[tokio::test]
async fn scoped_app_logs_can_be_limited_to_restream_only_entries() {
    let pool = test_pool().await;

    db::append_app_log_batch(
        &pool,
        &[
            app_log_entry(
                "2024-01-01T00:00:00Z",
                None,
                None,
                Some("restream.http.ready"),
                Some("lifecycle"),
                "dashboard API server listening",
                None,
            ),
            app_log_entry(
                "2024-01-01T00:00:01Z",
                Some("p1"),
                None,
                Some("ingest.connected"),
                Some("lifecycle"),
                "publisher connected",
                None,
            ),
            app_log_entry(
                "2024-01-01T00:00:02Z",
                Some("p1"),
                Some("o1"),
                Some("egress.failed"),
                Some("lifecycle"),
                "output failed",
                None,
            ),
        ],
    )
    .await
    .unwrap();

    let restream_logs = db::list_app_logs(
        &pool,
        &AppLogFilters {
            level: Some("info".to_string()),
            since: None,
            until: None,
            target: None,
            scope: Some("restream".to_string()),
            pipeline_id: None,
            output_id: None,
            event_class: None,
            prefix: None,
            limit: Some(10),
            order: Some("asc".to_string()),
        },
    )
    .await
    .unwrap();
    assert_eq!(restream_logs.len(), 1);
    assert_eq!(
        restream_logs[0].event_type.as_deref(),
        Some("restream.http.ready")
    );
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
    db::create_pipeline(&pool, "p1", "P", "key01", None, None, None)
        .await
        .unwrap();
    db::create_output(
        &pool, "o1", "p1", "Out", "rtmp://x", None, "stopped", "source",
    )
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

// ── Regression tests for Round 10 audit fixes ────────────────────────────────

// M1: list_sessions must return Err (not Ok([])) when the DB fails. The
// reconciler's session-prune logic skips retain() on Err — if this returned
// Ok([]) instead, every active session would be wiped from memory.
#[tokio::test]
async fn list_sessions_returns_err_not_empty_on_db_failure() {
    let pool = db::create_pool("sqlite::memory:").await.unwrap();
    db::setup_database_schema(&pool).await.unwrap();

    // Insert a live session so Ok([]) vs Err is distinguishable.
    let _ = sqlx::query("INSERT INTO sessions (token, created_at) VALUES ('tok1', 0)")
        .execute(&pool)
        .await
        .unwrap();

    // Close the pool to simulate a DB failure.
    pool.close().await;

    let result = db::list_sessions(&pool).await;
    assert!(
        result.is_err(),
        "closed pool must return Err, not Ok([]) — \
         Ok([]) would wipe all active sessions from memory"
    );
}

// M4: Per-connection PRAGMAs — every pooled connection must have busy_timeout
// set so SQLITE_BUSY retries rather than failing immediately. Verify via the
// PRAGMA value read back from the pool (not just the setup connection).
#[tokio::test]
async fn pool_connections_have_busy_timeout_set() {
    let pool = db::create_pool("sqlite::memory:").await.unwrap();
    db::setup_database_schema(&pool).await.unwrap();

    // Acquire two distinct connections and check both have busy_timeout.
    let conn1 = sqlx::query_scalar::<_, i64>("PRAGMA busy_timeout")
        .fetch_one(&pool)
        .await
        .unwrap();
    let conn2 = sqlx::query_scalar::<_, i64>("PRAGMA busy_timeout")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(
        conn1, 5000,
        "busy_timeout must be 5000ms on every connection"
    );
    assert_eq!(
        conn2, 5000,
        "busy_timeout must be 5000ms on every connection"
    );
}

// M5: NULL encoding in DB must not cause a decode failure. A row with
// encoding=NULL must be returned as an empty string via COALESCE.
#[tokio::test]
async fn output_with_null_encoding_decodes_as_empty_string() {
    let pool = db::create_pool("sqlite::memory:").await.unwrap();
    db::setup_database_schema(&pool).await.unwrap();

    db::create_pipeline(&pool, "p1", "P", "key-null-enc", None, None, None)
        .await
        .unwrap();

    // Insert a row with NULL encoding directly — bypasses the Rust layer to
    // simulate a legacy row that predates the encoding column.
    sqlx::query(
        "INSERT INTO outputs (id, pipeline_id, name, url, desired_state, encoding) \
         VALUES ('o-null', 'p1', 'Legacy', 'rtmp://x', 'stopped', NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let outputs = db::list_outputs(&pool).await.unwrap();
    assert_eq!(outputs.len(), 1);
    assert_eq!(
        outputs[0].encoding, "",
        "NULL encoding must decode to empty string via COALESCE"
    );

    let fetched = db::get_output(&pool, "p1", "o-null")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched.encoding, "");

    let by_pipeline = db::list_outputs_for_pipeline(&pool, "p1").await.unwrap();
    assert_eq!(by_pipeline[0].encoding, "");
}
