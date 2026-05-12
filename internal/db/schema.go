package db

import "database/sql"

func setupDatabaseSchema(sqlDB *sql.DB) error {
	stmts := []string{
		`PRAGMA foreign_keys = ON`,
		`PRAGMA journal_mode = WAL`,
		`PRAGMA busy_timeout = 5000`,

		`CREATE TABLE IF NOT EXISTS pipelines (
			id TEXT PRIMARY KEY,
			name TEXT NOT NULL,
			stream_key TEXT NOT NULL,
			encoding TEXT,
			input_ever_seen_live INTEGER NOT NULL DEFAULT 0
		)`,

		// desired_state stores operator intent ("should be running") separately from
		// jobs.status ("what happened last"), letting recovery act on transient exits.
		`CREATE TABLE IF NOT EXISTS outputs (
			id TEXT PRIMARY KEY,
			pipeline_id TEXT NOT NULL,
			name TEXT NOT NULL,
			url TEXT NOT NULL,
			desired_state TEXT NOT NULL DEFAULT 'running',
			encoding TEXT,
			FOREIGN KEY(pipeline_id) REFERENCES pipelines(id) ON DELETE CASCADE
		)`,
		`CREATE INDEX IF NOT EXISTS idx_outputs_pipeline ON outputs(pipeline_id)`,

		`CREATE TABLE IF NOT EXISTS jobs (
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
		)`,
		// Recovery and health logic read jobs as the current terminal/running row per output.
		`CREATE UNIQUE INDEX IF NOT EXISTS idx_jobs_pipeline_output_unique ON jobs(pipeline_id, output_id)`,

		`CREATE TABLE IF NOT EXISTS job_logs (
			id INTEGER PRIMARY KEY AUTOINCREMENT,
			job_id TEXT,
			pipeline_id TEXT,
			output_id TEXT,
			event_type TEXT,
			event_data TEXT,
			ts TEXT,
			message TEXT
		)`,
		`CREATE INDEX IF NOT EXISTS idx_job_logs_output ON job_logs(pipeline_id, output_id, ts)`,

		`CREATE TABLE IF NOT EXISTS meta (
			key TEXT PRIMARY KEY,
			value TEXT
		)`,
	}

	for _, s := range stmts {
		if _, err := sqlDB.Exec(s); err != nil {
			return err
		}
	}
	return nil
}
