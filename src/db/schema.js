function setupDatabaseSchema(db) {
    db.pragma('foreign_keys = ON');

    /* pipelines table */
    db.prepare(
        `
  CREATE TABLE IF NOT EXISTS pipelines (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    stream_key TEXT NOT NULL,
    encoding TEXT,
    input_ever_seen_live INTEGER NOT NULL DEFAULT 0
  )
`,
    ).run();

    /* outputs table */
    // desired_state stores operator intent (“should be running”) separately from jobs.status
    // (“what happened last”), which lets recovery logic act on transient exits without losing intent.
    db.prepare(
        `
  CREATE TABLE IF NOT EXISTS outputs (
    id TEXT PRIMARY KEY,
    pipeline_id TEXT NOT NULL,
    name TEXT NOT NULL,
    url TEXT NOT NULL,
    desired_state TEXT NOT NULL DEFAULT 'running',
    encoding TEXT,
    FOREIGN KEY(pipeline_id) REFERENCES pipelines(id) ON DELETE CASCADE
  )
`,
    ).run();

    db.prepare(`CREATE INDEX IF NOT EXISTS idx_outputs_pipeline ON outputs(pipeline_id)`).run();

    /* jobs table */
    db.prepare(
        `
  CREATE TABLE IF NOT EXISTS jobs (
    id TEXT PRIMARY KEY,
    pipeline_id TEXT NOT NULL,
    output_id TEXT NOT NULL,
    pid INTEGER,
    status TEXT NOT NULL, -- running | stopped | failed
    started_at TEXT,
    ended_at TEXT,
    exit_code INTEGER,
    exit_signal TEXT,
    FOREIGN KEY(pipeline_id) REFERENCES pipelines(id) ON DELETE CASCADE,
    FOREIGN KEY(output_id) REFERENCES outputs(id) ON DELETE CASCADE
  )
`,
    ).run();

    // Recovery and health logic read jobs as the current terminal/running row for an output.
    db.prepare(
        `
    CREATE UNIQUE INDEX IF NOT EXISTS idx_jobs_pipeline_output_unique
    ON jobs(pipeline_id, output_id)
`,
    ).run();

    /* job_logs table */
    db.prepare(
        `
    CREATE TABLE IF NOT EXISTS job_logs (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        job_id TEXT,
        pipeline_id TEXT,
        output_id TEXT,
        event_type TEXT,
        event_data TEXT,
        ts TEXT,
        message TEXT
    )
`,
    ).run();

    // Create index for fast lookups of logs by output
    db.prepare(
        `
    CREATE INDEX IF NOT EXISTS idx_job_logs_output ON job_logs(pipeline_id, output_id, ts)
`,
    ).run();

    /* meta table */
    db.prepare(
        `
  CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT
  )
`,
    ).run();

    db.prepare(
        `
  CREATE TABLE IF NOT EXISTS encodings (
    id TEXT PRIMARY KEY,
    key TEXT NOT NULL UNIQUE,
    ffmpeg_args TEXT NOT NULL
  )
`,
    ).run();
}

module.exports = { setupDatabaseSchema };
