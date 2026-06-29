//! SQLite persistence layer — raw `sqlx` prepared statements against `data.db`.
//! Schema is created via `CREATE TABLE IF NOT EXISTS` at startup (no migrations).
//! WAL mode is enabled for concurrent reader/writer access.

use crate::types::*;
use sqlx::{AssertSqlSafe, Row, SqlitePool};
use std::time::SystemTime;

/// Create a connection pool with all per-connection PRAGMAs baked in via
/// `SqliteConnectOptions`. This ensures every pooled connection gets the same
/// tuning, not just the setup connection (M4 fix).
pub async fn create_pool(url: &str) -> Result<SqlitePool, sqlx::Error> {
    use sqlx::sqlite::SqliteConnectOptions;
    use std::str::FromStr;

    let opts = SqliteConnectOptions::from_str(url)?
        // Per-connection PRAGMAs — applied to every new connection in the pool.
        .pragma("foreign_keys", "ON")
        .pragma("synchronous", "NORMAL")
        .pragma("busy_timeout", "5000")
        .pragma("cache_size", "-16384") // KiB; 16 MB page cache
        .pragma("temp_store", "MEMORY")
        .pragma("mmap_size", "134217728"); // 128 MB mmap

    SqlitePool::connect_with(opts).await
}

pub async fn setup_database_schema(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    // WAL mode and journal_size_limit are database-level settings stored in
    // the database file — they only need to be set once (not per-connection).
    // Per-connection PRAGMAs (busy_timeout, synchronous, etc.) are now
    // applied via SqliteConnectOptions in create_pool().
    sqlx::query("PRAGMA journal_mode = WAL;")
        .execute(pool)
        .await?;
    // Cap the WAL file at 64 MB. Without this the WAL can grow to gigabytes
    // on a busy instance before the next checkpoint.
    sqlx::query("PRAGMA journal_size_limit = 67108864;")
        .execute(pool)
        .await?;

    // Pipelines
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS pipelines (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            stream_key TEXT NOT NULL,
            encoding TEXT,
            input_ever_seen_live INTEGER NOT NULL DEFAULT 0,
            input_source TEXT,
            srt_ingest_policy TEXT
        );",
    )
    .execute(pool)
    .await?;
    ensure_column_exists(pool, "pipelines", "srt_ingest_policy", "TEXT").await?;

    // Outputs
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS outputs (
            id TEXT PRIMARY KEY,
            pipeline_id TEXT NOT NULL,
            name TEXT NOT NULL,
            url TEXT NOT NULL,
            monitoring_url TEXT,
            desired_state TEXT NOT NULL DEFAULT 'running',
            encoding TEXT,
            FOREIGN KEY(pipeline_id) REFERENCES pipelines(id) ON DELETE CASCADE
        );",
    )
    .execute(pool)
    .await?;
    ensure_column_exists(pool, "outputs", "monitoring_url", "TEXT").await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_outputs_pipeline ON outputs(pipeline_id);")
        .execute(pool)
        .await?;

    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_pipelines_stream_key_unique ON pipelines(stream_key);"
    )
    .execute(pool)
    .await?;

    // Jobs
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS jobs (
            id TEXT PRIMARY KEY,
            pipeline_id TEXT NOT NULL,
            output_id TEXT NOT NULL,
            pid INTEGER,
            status TEXT NOT NULL,
            started_at TEXT,
            ended_at TEXT,
            exit_code INTEGER,
            exit_signal TEXT,
            FOREIGN KEY(pipeline_id) REFERENCES pipelines(id) ON DELETE CASCADE,
            FOREIGN KEY(output_id) REFERENCES outputs(id) ON DELETE CASCADE
        );",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_jobs_pipeline_output_unique ON jobs(pipeline_id, output_id);"
    ).execute(pool).await?;

    // Ingests
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ingests (
            id TEXT PRIMARY KEY,
            filename TEXT NOT NULL,
            stream_key TEXT NOT NULL,
            loop INTEGER NOT NULL DEFAULT 0,
            start_time TEXT NOT NULL DEFAULT ''
        );",
    )
    .execute(pool)
    .await?;

    // Meta Config
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT
        );",
    )
    .execute(pool)
    .await?;

    // Sessions
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS sessions (
            token TEXT PRIMARY KEY,
            created_at INTEGER NOT NULL
        );",
    )
    .execute(pool)
    .await?;

    // Application process logs (tracing-based, multi-sink).
    // event_type and event_class are promoted from tracing fields so scoped log
    // queries do not need JSON extraction.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS app_logs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            ts          TEXT    NOT NULL,
            level       TEXT    NOT NULL,
            target      TEXT    NOT NULL,
            message     TEXT    NOT NULL,
            fields      TEXT,
            pipeline_id TEXT,
            output_id   TEXT,
            event_type  TEXT,
            event_class TEXT
        );",
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_app_logs_ts ON app_logs(ts DESC);")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_app_logs_level ON app_logs(level, ts DESC);")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_app_logs_target ON app_logs(target, ts DESC);")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_app_logs_pipeline ON app_logs(pipeline_id, ts DESC) WHERE pipeline_id IS NOT NULL;"
    ).execute(pool).await?;
    sqlx::query("DROP INDEX IF EXISTS idx_app_logs_history;")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_app_logs_scope ON app_logs(pipeline_id, output_id, event_class, ts) WHERE pipeline_id IS NOT NULL;"
    ).execute(pool).await?;

    Ok(())
}

async fn ensure_column_exists(
    pool: &SqlitePool,
    table: &str,
    column: &str,
    column_type: &str,
) -> Result<(), sqlx::Error> {
    let pragma = format!("PRAGMA table_info({table})");
    let rows = sqlx::query(AssertSqlSafe(pragma)).fetch_all(pool).await?;
    let exists = rows
        .iter()
        .any(|row| row.get::<String, _>("name") == column);
    if exists {
        return Ok(());
    }
    let alter = format!("ALTER TABLE {table} ADD COLUMN {column} {column_type}");
    sqlx::query(AssertSqlSafe(alter)).execute(pool).await?;
    Ok(())
}

/* Pipeline Operations */

pub async fn create_pipeline(
    pool: &SqlitePool,
    id: &str,
    name: &str,
    stream_key: &str,
    input_source: Option<&str>,
    encoding: Option<&str>,
    srt_ingest_policy: Option<&str>,
) -> Result<Pipeline, sqlx::Error> {
    let exists =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pipelines WHERE stream_key = ?")
            .bind(stream_key)
            .fetch_one(pool)
            .await?;
    if exists > 0 {
        return Err(sqlx::Error::Protocol("duplicate stream key".into()));
    }

    sqlx::query(
        "INSERT INTO pipelines (id, name, stream_key, input_source, encoding, srt_ingest_policy) VALUES (?, ?, ?, ?, ?, ?)"
    )
    .bind(id)
    .bind(name)
    .bind(stream_key)
    .bind(input_source)
    .bind(encoding)
    .bind(srt_ingest_policy)
    .execute(pool)
    .await?;

    get_pipeline(pool, id)
        .await?
        .ok_or_else(|| sqlx::Error::RowNotFound)
}

pub async fn get_pipeline(pool: &SqlitePool, id: &str) -> Result<Option<Pipeline>, sqlx::Error> {
    sqlx::query_as::<_, Pipeline>(
        "SELECT id, name, stream_key, input_source, encoding, srt_ingest_policy FROM pipelines WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn get_pipeline_by_stream_key(
    pool: &SqlitePool,
    stream_key: &str,
) -> Result<Option<Pipeline>, sqlx::Error> {
    sqlx::query_as::<_, Pipeline>(
        "SELECT id, name, stream_key, input_source, encoding, srt_ingest_policy FROM pipelines WHERE stream_key = ?",
    )
    .bind(stream_key)
    .fetch_optional(pool)
    .await
}

pub async fn list_pipelines(pool: &SqlitePool) -> Result<Vec<Pipeline>, sqlx::Error> {
    sqlx::query_as::<_, Pipeline>(
        "SELECT id, name, stream_key, input_source, encoding, srt_ingest_policy FROM pipelines",
    )
    .fetch_all(pool)
    .await
}

pub async fn update_pipeline(
    pool: &SqlitePool,
    id: &str,
    name: &str,
    stream_key: &str,
    input_source: Option<&str>,
    encoding: Option<&str>,
    srt_ingest_policy: Option<&str>,
) -> Result<Option<Pipeline>, sqlx::Error> {
    let duplicate = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM pipelines WHERE stream_key = ? AND id != ?",
    )
    .bind(stream_key)
    .bind(id)
    .fetch_one(pool)
    .await?;
    if duplicate > 0 {
        return Err(sqlx::Error::Protocol("duplicate stream key".into()));
    }

    let result = sqlx::query(
        "UPDATE pipelines SET name = ?, stream_key = ?, input_source = ?, encoding = ?, srt_ingest_policy = ? WHERE id = ?"
    )
    .bind(name)
    .bind(stream_key)
    .bind(input_source)
    .bind(encoding)
    .bind(srt_ingest_policy)
    .bind(id)
    .execute(pool)
    .await?;

    if result.rows_affected() > 0 {
        get_pipeline(pool, id).await
    } else {
        Ok(None)
    }
}

pub async fn delete_pipeline(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM pipelines WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/* Output Operations */

pub async fn create_output(
    pool: &SqlitePool,
    id: &str,
    pipeline_id: &str,
    name: &str,
    url: &str,
    monitoring_url: Option<&str>,
    desired_state: &str,
    encoding: &str,
) -> Result<Output, sqlx::Error> {
    sqlx::query(
        "INSERT INTO outputs (id, pipeline_id, name, url, monitoring_url, desired_state, encoding) VALUES (?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(id)
    .bind(pipeline_id)
    .bind(name)
    .bind(url)
    .bind(monitoring_url)
    .bind(desired_state)
    .bind(encoding)
    .execute(pool)
    .await?;

    get_output(pool, pipeline_id, id)
        .await?
        .ok_or_else(|| sqlx::Error::RowNotFound)
}

pub async fn get_output(
    pool: &SqlitePool,
    pipeline_id: &str,
    id: &str,
) -> Result<Option<Output>, sqlx::Error> {
    sqlx::query_as::<_, Output>(
        "SELECT id, pipeline_id, name, url, monitoring_url, desired_state, COALESCE(encoding, '') AS encoding \
         FROM outputs WHERE id = ? AND pipeline_id = ?",
    )
    .bind(id)
    .bind(pipeline_id)
    .fetch_optional(pool)
    .await
}

pub async fn list_outputs(pool: &SqlitePool) -> Result<Vec<Output>, sqlx::Error> {
    sqlx::query_as::<_, Output>(
        "SELECT id, pipeline_id, name, url, monitoring_url, desired_state, COALESCE(encoding, '') AS encoding \
         FROM outputs",
    )
    .fetch_all(pool)
    .await
}

pub async fn list_outputs_for_pipeline(
    pool: &SqlitePool,
    pipeline_id: &str,
) -> Result<Vec<Output>, sqlx::Error> {
    sqlx::query_as::<_, Output>(
        "SELECT id, pipeline_id, name, url, monitoring_url, desired_state, COALESCE(encoding, '') AS encoding \
         FROM outputs WHERE pipeline_id = ? ORDER BY rowid ASC",
    )
    .bind(pipeline_id)
    .fetch_all(pool)
    .await
}

pub async fn update_output(
    pool: &SqlitePool,
    pipeline_id: &str,
    id: &str,
    name: &str,
    url: &str,
    monitoring_url: Option<&str>,
    encoding: &str,
) -> Result<Option<Output>, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE outputs SET name = ?, url = ?, monitoring_url = ?, encoding = ? WHERE id = ? AND pipeline_id = ?",
    )
    .bind(name)
    .bind(url)
    .bind(monitoring_url)
    .bind(encoding)
    .bind(id)
    .bind(pipeline_id)
    .execute(pool)
    .await?;

    if result.rows_affected() > 0 {
        get_output(pool, pipeline_id, id).await
    } else {
        Ok(None)
    }
}

pub async fn set_output_desired_state(
    pool: &SqlitePool,
    pipeline_id: &str,
    id: &str,
    desired_state: &str,
) -> Result<Output, sqlx::Error> {
    sqlx::query("UPDATE outputs SET desired_state = ? WHERE id = ? AND pipeline_id = ?")
        .bind(desired_state)
        .bind(id)
        .bind(pipeline_id)
        .execute(pool)
        .await?;

    get_output(pool, pipeline_id, id)
        .await?
        .ok_or_else(|| sqlx::Error::RowNotFound)
}

pub async fn delete_output(
    pool: &SqlitePool,
    pipeline_id: &str,
    id: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM outputs WHERE id = ? AND pipeline_id = ?")
        .bind(id)
        .bind(pipeline_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/* Job Operations */

pub async fn create_job(
    pool: &SqlitePool,
    id: &str,
    pipeline_id: &str,
    output_id: &str,
    pid: Option<i64>,
    status: &str,
    started_at: &str,
) -> Result<Job, sqlx::Error> {
    sqlx::query(
        "INSERT INTO jobs (id, pipeline_id, output_id, pid, status, started_at, ended_at, exit_code, exit_signal)
         VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, NULL)
         ON CONFLICT(pipeline_id, output_id) DO UPDATE SET
             id = excluded.id,
             pid = excluded.pid,
             status = excluded.status,
             started_at = excluded.started_at,
             ended_at = NULL,
             exit_code = NULL,
             exit_signal = NULL"
    )
    .bind(id)
    .bind(pipeline_id)
    .bind(output_id)
    .bind(pid)
    .bind(status)
    .bind(started_at)
    .execute(pool)
    .await?;

    get_job(pool, id)
        .await?
        .ok_or_else(|| sqlx::Error::RowNotFound)
}

pub async fn get_job(pool: &SqlitePool, id: &str) -> Result<Option<Job>, sqlx::Error> {
    sqlx::query_as::<_, Job>(
        "SELECT id, pipeline_id, output_id, pid, status, started_at, ended_at, exit_code, exit_signal FROM jobs WHERE id = ?"
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn get_running_job_for(
    pool: &SqlitePool,
    pipeline_id: &str,
    output_id: &str,
) -> Result<Option<Job>, sqlx::Error> {
    sqlx::query_as::<_, Job>(
        "SELECT id, pipeline_id, output_id, pid, status, started_at, ended_at, exit_code, exit_signal
         FROM jobs WHERE pipeline_id = ? AND output_id = ? AND status = 'running' LIMIT 1"
    )
    .bind(pipeline_id)
    .bind(output_id)
    .fetch_optional(pool)
    .await
}

pub async fn update_job(
    pool: &SqlitePool,
    id: &str,
    pid: Option<i64>,
    status: Option<&str>,
    ended_at: Option<&str>,
    exit_code: Option<i64>,
    exit_signal: Option<&str>,
) -> Result<Option<Job>, sqlx::Error> {
    sqlx::query(
        "UPDATE jobs SET pid = COALESCE(?, pid), status = COALESCE(?, status), ended_at = COALESCE(?, ended_at),
                         exit_code = COALESCE(?, exit_code), exit_signal = COALESCE(?, exit_signal) WHERE id = ?"
    )
    .bind(pid)
    .bind(status)
    .bind(ended_at)
    .bind(exit_code)
    .bind(exit_signal)
    .bind(id)
    .execute(pool)
    .await?;

    get_job(pool, id).await
}

pub async fn list_jobs_for_output(
    pool: &SqlitePool,
    pipeline_id: &str,
    output_id: &str,
) -> Result<Vec<Job>, sqlx::Error> {
    sqlx::query_as::<_, Job>(
        "SELECT id, pipeline_id, output_id, pid, status, started_at, ended_at, exit_code, exit_signal
         FROM jobs WHERE pipeline_id = ? AND output_id = ? ORDER BY started_at DESC"
    )
    .bind(pipeline_id)
    .bind(output_id)
    .fetch_all(pool)
    .await
}

pub async fn list_jobs(pool: &SqlitePool) -> Result<Vec<Job>, sqlx::Error> {
    sqlx::query_as::<_, Job>(
        "SELECT id, pipeline_id, output_id, pid, status, started_at, ended_at, exit_code, exit_signal
         FROM jobs ORDER BY started_at DESC, id DESC"
    )
    .fetch_all(pool)
    .await
}

/* AppLog Operations — unified process log store backing /api/v1/logs and log timeline views */

/// Batch-insert log entries. Called by the DbLayer drain task every 100 ms.
pub async fn append_app_log_batch(
    pool: &SqlitePool,
    entries: &[crate::types::AppLogEntry],
) -> Result<(), sqlx::Error> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for e in entries {
        sqlx::query(
            "INSERT INTO app_logs (ts, level, target, message, fields, pipeline_id, output_id, event_type, event_class)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&e.ts)
        .bind(&e.level)
        .bind(&e.target)
        .bind(&e.message)
        .bind(&e.fields)
        .bind(&e.pipeline_id)
        .bind(&e.output_id)
        .bind(&e.event_type)
        .bind(&e.event_class)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// General query for `/api/v1/logs` — supports level, target, pipeline_id, event_class,
/// prefix (message LIKE), time range, limit, order.
pub async fn list_app_logs(
    pool: &SqlitePool,
    filters: &crate::types::AppLogFilters,
) -> Result<Vec<crate::types::AppLogRow>, sqlx::Error> {
    let mut clauses: Vec<String> = vec![];

    let levels: &[&str] = match filters.level.as_deref().unwrap_or("info") {
        "error" => &["ERROR"],
        "warn" => &["ERROR", "WARN"],
        "debug" => &["ERROR", "WARN", "INFO", "DEBUG"],
        _ => &["ERROR", "WARN", "INFO"],
    };
    let placeholders = levels.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    clauses.push(format!("level IN ({})", placeholders));

    if filters.target.is_some() {
        clauses.push("target LIKE ?".to_string());
    }
    if filters.pipeline_id.is_some() {
        clauses.push("pipeline_id = ?".to_string());
    }
    if filters.output_id.is_some() {
        clauses.push("output_id = ?".to_string());
    }
    if filters.event_class.is_some() {
        clauses.push("event_class = ?".to_string());
    }
    if filters.since.is_some() {
        clauses.push("ts >= ?".to_string());
    }
    if filters.until.is_some() {
        clauses.push("ts < ?".to_string());
    }

    // Comma-separated message prefix filter (e.g. "stderr,exit,control")
    if let Some(ref prefix) = filters.prefix {
        let parts: Vec<_> = prefix
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if !parts.is_empty() {
            let px: Vec<_> = parts.iter().map(|_| "message LIKE ?".to_string()).collect();
            clauses.push(format!("({})", px.join(" OR ")));
        }
    }

    let order = if filters.order.as_deref() == Some("asc") {
        "ASC"
    } else {
        "DESC"
    };
    let limit = filters.limit.unwrap_or(200).clamp(1, 1000);
    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    let sql = format!(
        "SELECT id, ts, level, target, message, fields, pipeline_id, output_id, event_type \
         FROM app_logs {} ORDER BY ts {}, id {} LIMIT {}",
        where_clause, order, order, limit
    );

    let mut q = sqlx::query_as::<_, crate::types::AppLogRow>(AssertSqlSafe(sql));
    for l in levels {
        q = q.bind(l);
    }
    if let Some(ref t) = filters.target {
        q = q.bind(format!("{}%", t));
    }
    if let Some(ref p) = filters.pipeline_id {
        q = q.bind(p);
    }
    if let Some(ref o) = filters.output_id {
        q = q.bind(o);
    }
    if let Some(ref ec) = filters.event_class {
        q = q.bind(ec);
    }
    if let Some(ref s) = filters.since {
        q = q.bind(s);
    }
    if let Some(ref u) = filters.until {
        q = q.bind(u);
    }
    if let Some(ref prefix) = filters.prefix {
        for p in prefix.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            q = q.bind(format!("{}%", p));
        }
    }

    q.fetch_all(pool).await
}

pub async fn delete_app_logs_older_than(pool: &SqlitePool, days: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM app_logs WHERE ts < datetime('now', ?)")
        .bind(format!("-{} days", days))
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn cleanup_old_jobs(pool: &SqlitePool) -> Result<(u64, u64), sqlx::Error> {
    let mut tx = pool.begin().await?;

    let result = sqlx::query(
        "DELETE FROM jobs
         WHERE (status IN ('stopped','failed') AND ended_at IS NOT NULL AND datetime(ended_at) < datetime('now', '-7 days'))
            OR datetime(COALESCE(ended_at, started_at)) < datetime('now', '-30 days')"
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok((result.rows_affected(), 0))
}

pub async fn reset_running_jobs(pool: &SqlitePool, now_ts: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE jobs SET status = 'stopped', ended_at = ?, exit_code = NULL, exit_signal = 'SIGKILL'
         WHERE status = 'running'"
    )
    .bind(now_ts)
    .execute(pool)
    .await?;
    Ok(())
}

/* Ingest Operations */

pub async fn create_ingest(
    pool: &SqlitePool,
    id: &str,
    filename: &str,
    stream_key: &str,
    loop_flag: bool,
    start_time: &str,
) -> Result<Ingest, sqlx::Error> {
    sqlx::query(
        "INSERT INTO ingests (id, filename, stream_key, loop, start_time) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(filename)
    .bind(stream_key)
    .bind(if loop_flag { 1 } else { 0 })
    .bind(start_time)
    .execute(pool)
    .await?;

    get_ingest(pool, id)
        .await?
        .ok_or_else(|| sqlx::Error::RowNotFound)
}

pub async fn get_ingest(pool: &SqlitePool, id: &str) -> Result<Option<Ingest>, sqlx::Error> {
    sqlx::query_as::<_, Ingest>(
        "SELECT id, filename, stream_key, loop, start_time FROM ingests WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn get_ingest_by_stream_key(
    pool: &SqlitePool,
    stream_key: &str,
) -> Result<Option<Ingest>, sqlx::Error> {
    sqlx::query_as::<_, Ingest>(
        "SELECT id, filename, stream_key, loop, start_time FROM ingests WHERE stream_key = ? ORDER BY rowid DESC LIMIT 1",
    )
    .bind(stream_key)
    .fetch_optional(pool)
    .await
}

pub async fn list_ingests_for_stream_key(
    pool: &SqlitePool,
    stream_key: &str,
) -> Result<Vec<Ingest>, sqlx::Error> {
    sqlx::query_as::<_, Ingest>(
        "SELECT id, filename, stream_key, loop, start_time FROM ingests WHERE stream_key = ? ORDER BY rowid ASC",
    )
    .bind(stream_key)
    .fetch_all(pool)
    .await
}

pub async fn list_ingests(pool: &SqlitePool) -> Result<Vec<Ingest>, sqlx::Error> {
    sqlx::query_as::<_, Ingest>(
        "SELECT id, filename, stream_key, loop, start_time FROM ingests ORDER BY rowid ASC",
    )
    .fetch_all(pool)
    .await
}

pub async fn list_ingests_for_filename(
    pool: &SqlitePool,
    filename: &str,
) -> Result<Vec<Ingest>, sqlx::Error> {
    sqlx::query_as::<_, Ingest>(
        "SELECT id, filename, stream_key, loop, start_time FROM ingests WHERE filename = ?",
    )
    .bind(filename)
    .fetch_all(pool)
    .await
}

pub async fn update_ingest(
    pool: &SqlitePool,
    id: &str,
    filename: &str,
    stream_key: &str,
    loop_flag: bool,
    start_time: &str,
) -> Result<Option<Ingest>, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE ingests SET filename = ?, stream_key = ?, loop = ?, start_time = ? WHERE id = ?",
    )
    .bind(filename)
    .bind(stream_key)
    .bind(if loop_flag { 1 } else { 0 })
    .bind(start_time)
    .bind(id)
    .execute(pool)
    .await?;

    if result.rows_affected() > 0 {
        get_ingest(pool, id).await
    } else {
        Ok(None)
    }
}

pub async fn delete_ingest(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM ingests WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/* Meta Operations */

pub async fn get_meta(pool: &SqlitePool, key: &str) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query("SELECT value FROM meta WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(|r| r.get::<String, _>(0)))
}

pub async fn set_meta(pool: &SqlitePool, key: &str, value: &str) -> Result<String, sqlx::Error> {
    sqlx::query(
        "INSERT INTO meta (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;

    Ok(value.to_string())
}

pub async fn get_ingest_host(pool: &SqlitePool) -> Result<Option<String>, sqlx::Error> {
    get_meta(pool, "ingest_host").await
}

pub async fn set_ingest_host(pool: &SqlitePool, host: &str) -> Result<String, sqlx::Error> {
    set_meta(pool, "ingest_host", host.trim()).await
}

/* Session Operations */

pub async fn create_session(pool: &SqlitePool, token: &str, ts: i64) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT OR REPLACE INTO sessions (token, created_at) VALUES (?, ?)")
        .bind(token)
        .bind(ts)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_session(pool: &SqlitePool, token: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM sessions WHERE token = ?")
        .bind(token)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_sessions(pool: &SqlitePool) -> Result<Vec<String>, sqlx::Error> {
    let rows = sqlx::query("SELECT token FROM sessions")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|r| r.get::<String, _>(0)).collect())
}

pub async fn prune_expired_sessions(pool: &SqlitePool, max_age_ms: i64) -> Result<(), sqlx::Error> {
    let expire_limit = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
        - max_age_ms;

    sqlx::query("DELETE FROM sessions WHERE created_at < ?")
        .bind(expire_limit)
        .execute(pool)
        .await?;
    Ok(())
}
