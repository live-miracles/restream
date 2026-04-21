function setupDatabaseSchema(db) {
    db.pragma('foreign_keys = ON');

    /* stream_keys table */
    db.prepare(
        `
  CREATE TABLE IF NOT EXISTS stream_keys (
    key TEXT PRIMARY KEY,
    label TEXT,
    created_at TEXT
  )
`,
    ).run();

    /* pipelines table */
    db.prepare(
        `
  CREATE TABLE IF NOT EXISTS pipelines (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    stream_key TEXT,
    encoding TEXT,
    input_ever_seen_live INTEGER NOT NULL DEFAULT 0,
    created_at TEXT,
    updated_at TEXT,
    FOREIGN KEY(stream_key) REFERENCES stream_keys(key) ON DELETE SET NULL
  )
`,
    ).run();

    const pipelineColumns = db.prepare(`PRAGMA table_info(pipelines)`).all();
    if (!pipelineColumns.some((column) => column.name === 'input_ever_seen_live')) {
        db.prepare(
            `ALTER TABLE pipelines ADD COLUMN input_ever_seen_live INTEGER NOT NULL DEFAULT 0`,
        ).run();
    }

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
    created_at TEXT,
    FOREIGN KEY(pipeline_id) REFERENCES pipelines(id) ON DELETE CASCADE
  )
`,
    ).run();

    const outputColumns = db.prepare(`PRAGMA table_info(outputs)`).all();
    if (!outputColumns.some((column) => column.name === 'desired_state')) {
        db.prepare(
            `ALTER TABLE outputs ADD COLUMN desired_state TEXT NOT NULL DEFAULT 'running'`,
        ).run();
    }
    if (!outputColumns.some((column) => column.name === 'encoding')) {
        db.prepare(`ALTER TABLE outputs ADD COLUMN encoding TEXT`).run();
    }

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

    // Older schemas allowed multiple historical jobs per output; keep the newest plausible row
    // before enforcing the one-current-job-per-output invariant used by the recovery layer.
    const duplicateJobPairs = db
        .prepare(
            `
    SELECT pipeline_id AS pipelineId, output_id AS outputId, COUNT(*) AS count
    FROM jobs
    GROUP BY pipeline_id, output_id
    HAVING COUNT(*) > 1
`,
        )
        .all();

    if (duplicateJobPairs.length > 0) {
        const selectJobToKeep = db.prepare(
            `
      SELECT id
      FROM jobs
      WHERE pipeline_id = ? AND output_id = ?
      ORDER BY
        CASE WHEN started_at IS NULL THEN 1 ELSE 0 END,
        started_at DESC,
        CASE WHEN ended_at IS NULL THEN 1 ELSE 0 END,
        ended_at DESC,
        rowid DESC
      LIMIT 1
  `,
        );
        const deleteDuplicateJobs = db.prepare(
            `
      DELETE FROM jobs
      WHERE pipeline_id = ? AND output_id = ? AND id != ?
  `,
        );
        const dedupeJobs = db.transaction((pairs) => {
            for (const pair of pairs) {
                const kept = selectJobToKeep.get(pair.pipelineId, pair.outputId);
                if (!kept?.id) continue;
                deleteDuplicateJobs.run(pair.pipelineId, pair.outputId, kept.id);
            }
        });
        dedupeJobs(duplicateJobPairs);
    }

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

    // Add decoupled columns for old schemas
    const jobLogsColumns = db.prepare(`PRAGMA table_info(job_logs)`).all();
    if (!jobLogsColumns.some((column) => column.name === 'pipeline_id')) {
        db.prepare(`ALTER TABLE job_logs ADD COLUMN pipeline_id TEXT`).run();
    }
    if (!jobLogsColumns.some((column) => column.name === 'output_id')) {
        db.prepare(`ALTER TABLE job_logs ADD COLUMN output_id TEXT`).run();
    }
    if (!jobLogsColumns.some((column) => column.name === 'event_type')) {
        db.prepare(`ALTER TABLE job_logs ADD COLUMN event_type TEXT`).run();
    }
    if (!jobLogsColumns.some((column) => column.name === 'event_data')) {
        db.prepare(`ALTER TABLE job_logs ADD COLUMN event_data TEXT`).run();
    }

    // Migrate old FK-based job_logs table to decoupled schema.
    // Upsert updates jobs.id, which conflicts with FK(job_logs.job_id -> jobs.id).
    const jobLogsForeignKeys = db.prepare(`PRAGMA foreign_key_list(job_logs)`).all();
    if (jobLogsForeignKeys.some((fk) => fk.table === 'jobs' && fk.from === 'job_id')) {
        db.exec(`PRAGMA foreign_keys = OFF`);
        db.exec(`BEGIN TRANSACTION`);
        try {
            db.exec(`
      CREATE TABLE job_logs_new (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        job_id TEXT,
        pipeline_id TEXT,
        output_id TEXT,
        event_type TEXT,
                event_data TEXT,
        ts TEXT,
        message TEXT
      )
    `);
            db.exec(`
            INSERT INTO job_logs_new (id, job_id, pipeline_id, output_id, event_type, event_data, ts, message)
            SELECT id, job_id, pipeline_id, output_id, event_type, event_data, ts, message
      FROM job_logs
    `);
            db.exec(`DROP TABLE job_logs`);
            db.exec(`ALTER TABLE job_logs_new RENAME TO job_logs`);
            db.exec(`COMMIT`);
        } catch (err) {
            db.exec(`ROLLBACK`);
            db.exec(`PRAGMA foreign_keys = ON`);
            throw err;
        }
        db.exec(`PRAGMA foreign_keys = ON`);
    }

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
}

module.exports = { setupDatabaseSchema };
