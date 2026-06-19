//! SQLite persistence layer — raw `sqlx` prepared statements against `data.db`.
//! Schema is created via `CREATE TABLE IF NOT EXISTS` at startup (no migrations).
//! WAL mode is enabled for concurrent reader/writer access.

use crate::types::*;
use sqlx::{Row, SqlitePool};
use std::time::SystemTime;

pub async fn setup_database_schema(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    // Enable WAL (Write-Ahead Logging) mode for concurrent reader/writer access
    sqlx::query("PRAGMA journal_mode = WAL;")
        .execute(pool)
        .await?;
    sqlx::query("PRAGMA foreign_keys = ON;")
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
            input_source TEXT
        );",
    )
    .execute(pool)
    .await?;

    // Outputs
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS outputs (
            id TEXT PRIMARY KEY,
            pipeline_id TEXT NOT NULL,
            name TEXT NOT NULL,
            url TEXT NOT NULL,
            desired_state TEXT NOT NULL DEFAULT 'running',
            encoding TEXT,
            FOREIGN KEY(pipeline_id) REFERENCES pipelines(id) ON DELETE CASCADE
        );",
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_outputs_pipeline ON outputs(pipeline_id);")
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

    // Job Logs
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS job_logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            job_id TEXT,
            pipeline_id TEXT,
            output_id TEXT,
            event_type TEXT,
            event_data TEXT,
            ts TEXT,
            message TEXT
        );",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_job_logs_output ON job_logs(pipeline_id, output_id, ts);",
    )
    .execute(pool)
    .await?;

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
) -> Result<Pipeline, sqlx::Error> {
    sqlx::query(
        "INSERT INTO pipelines (id, name, stream_key, input_source, encoding) VALUES (?, ?, ?, ?, ?)"
    )
    .bind(id)
    .bind(name)
    .bind(stream_key)
    .bind(input_source)
    .bind(encoding)
    .execute(pool)
    .await?;

    get_pipeline(pool, id)
        .await?
        .ok_or_else(|| sqlx::Error::RowNotFound)
}

pub async fn get_pipeline(pool: &SqlitePool, id: &str) -> Result<Option<Pipeline>, sqlx::Error> {
    sqlx::query_as::<_, Pipeline>(
        "SELECT id, name, stream_key, input_source, encoding FROM pipelines WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn list_pipelines(pool: &SqlitePool) -> Result<Vec<Pipeline>, sqlx::Error> {
    sqlx::query_as::<_, Pipeline>(
        "SELECT id, name, stream_key, input_source, encoding FROM pipelines",
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
) -> Result<Option<Pipeline>, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE pipelines SET name = ?, stream_key = ?, input_source = ?, encoding = ? WHERE id = ?"
    )
    .bind(name)
    .bind(stream_key)
    .bind(input_source)
    .bind(encoding)
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
    desired_state: &str,
    encoding: &str,
) -> Result<Output, sqlx::Error> {
    sqlx::query(
        "INSERT INTO outputs (id, pipeline_id, name, url, desired_state, encoding) VALUES (?, ?, ?, ?, ?, ?)"
    )
    .bind(id)
    .bind(pipeline_id)
    .bind(name)
    .bind(url)
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
        "SELECT id, pipeline_id, name, url, desired_state, encoding FROM outputs WHERE id = ? AND pipeline_id = ?"
    )
    .bind(id)
    .bind(pipeline_id)
    .fetch_optional(pool)
    .await
}

pub async fn list_outputs(pool: &SqlitePool) -> Result<Vec<Output>, sqlx::Error> {
    sqlx::query_as::<_, Output>(
        "SELECT id, pipeline_id, name, url, desired_state, encoding FROM outputs",
    )
    .fetch_all(pool)
    .await
}

pub async fn list_outputs_for_pipeline(
    pool: &SqlitePool,
    pipeline_id: &str,
) -> Result<Vec<Output>, sqlx::Error> {
    sqlx::query_as::<_, Output>(
        "SELECT id, pipeline_id, name, url, desired_state, encoding FROM outputs WHERE pipeline_id = ? ORDER BY rowid ASC"
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
    encoding: &str,
) -> Result<Option<Output>, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE outputs SET name = ?, url = ?, encoding = ? WHERE id = ? AND pipeline_id = ?",
    )
    .bind(name)
    .bind(url)
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

/* JobLog Operations */

pub async fn append_job_log(
    pool: &SqlitePool,
    job_id: Option<&str>,
    pipeline_id: Option<&str>,
    output_id: Option<&str>,
    event_type: &str,
    event_data: Option<&str>,
    ts: &str,
    message: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO job_logs (job_id, pipeline_id, output_id, event_type, event_data, ts, message)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(job_id)
    .bind(pipeline_id)
    .bind(output_id)
    .bind(event_type)
    .bind(event_data)
    .bind(ts)
    .bind(message)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_job_logs(pool: &SqlitePool, job_id: &str) -> Result<Vec<JobLog>, sqlx::Error> {
    sqlx::query_as::<_, JobLog>(
        "SELECT ts, message, event_type, event_data FROM job_logs WHERE job_id = ? ORDER BY id ASC",
    )
    .bind(job_id)
    .fetch_all(pool)
    .await
}

pub async fn list_job_logs_by_output(
    pool: &SqlitePool,
    pipeline_id: &str,
    output_id: &str,
) -> Result<Vec<JobLog>, sqlx::Error> {
    sqlx::query_as::<_, JobLog>(
        "SELECT ts, message, event_type, event_data FROM job_logs
         WHERE pipeline_id = ? AND output_id = ? ORDER BY ts DESC",
    )
    .bind(pipeline_id)
    .bind(output_id)
    .fetch_all(pool)
    .await
}

pub async fn list_job_logs_by_output_filtered(
    pool: &SqlitePool,
    pipeline_id: &str,
    output_id: &str,
    filters: &HistoryFilters,
) -> Result<Vec<JobLog>, sqlx::Error> {
    let mut clauses = vec!["pipeline_id = ?".to_string(), "output_id = ?".to_string()];
    let mut query_str = "SELECT ts, message, event_type, event_data FROM job_logs".to_string();

    if filters.since.is_some() {
        clauses.push("ts >= ?".to_string());
    }
    if filters.until.is_some() {
        clauses.push("ts < ?".to_string());
    }

    if let Some(ref prefixes) = filters.prefixes {
        if !prefixes.is_empty() {
            let mut prefix_clauses = vec![];
            for _ in prefixes {
                prefix_clauses.push("message LIKE ?".to_string());
            }
            clauses.push(format!("({})", prefix_clauses.join(" OR ")));
        }
    }

    query_str.push_str(" WHERE ");
    query_str.push_str(&clauses.join(" AND "));

    let order = if filters.order.as_deref() == Some("asc") {
        "ASC"
    } else {
        "DESC"
    };
    query_str.push_str(&format!(" ORDER BY ts {}", order));

    if filters.limit.is_some() {
        query_str.push_str(" LIMIT ?");
    }

    let mut query = sqlx::query_as::<_, JobLog>(&query_str)
        .bind(pipeline_id)
        .bind(output_id);

    if let Some(ref since) = filters.since {
        query = query.bind(since);
    }
    if let Some(ref until) = filters.until {
        query = query.bind(until);
    }

    if let Some(ref prefixes) = filters.prefixes {
        for prefix in prefixes {
            query = query.bind(format!("{}%", prefix));
        }
    }

    if let Some(limit) = filters.limit {
        query = query.bind(limit);
    }

    query.fetch_all(pool).await
}

pub async fn list_lifecycle_logs_by_output(
    pool: &SqlitePool,
    pipeline_id: &str,
    output_id: &str,
) -> Result<Vec<JobLog>, sqlx::Error> {
    sqlx::query_as::<_, JobLog>(
        "SELECT ts, message, event_type, event_data FROM job_logs
         WHERE pipeline_id = ? AND output_id = ? AND (event_type LIKE 'lifecycle.%' OR message LIKE '[lifecycle]%')
         ORDER BY ts ASC"
    )
    .bind(pipeline_id)
    .bind(output_id)
    .fetch_all(pool)
    .await
}

pub async fn list_job_logs_by_pipeline(
    pool: &SqlitePool,
    pipeline_id: &str,
) -> Result<Vec<JobLog>, sqlx::Error> {
    sqlx::query_as::<_, JobLog>(
        "SELECT ts, message, event_type, event_data FROM job_logs
         WHERE pipeline_id = ? AND output_id IS NULL ORDER BY ts DESC",
    )
    .bind(pipeline_id)
    .fetch_all(pool)
    .await
}

pub async fn delete_job_logs_older_than(pool: &SqlitePool, days: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM job_logs WHERE ts < datetime('now', ?)")
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
