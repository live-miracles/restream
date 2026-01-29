
const path = require('path');
const Database = require('better-sqlite3');
const crypto = require('crypto');
const db = new Database(path.join(__dirname, 'data.db'));
db.pragma('foreign_keys = ON');

/* stream_keys table */
db.prepare(`
  CREATE TABLE IF NOT EXISTS stream_keys (
    key TEXT PRIMARY KEY,
    label TEXT,
    created_at TEXT
  )
`).run();

/* pipelines table */
db.prepare(`
  CREATE TABLE IF NOT EXISTS pipelines (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    stream_key TEXT,
    created_at TEXT,
    updated_at TEXT,
    FOREIGN KEY(stream_key) REFERENCES stream_keys(key) ON DELETE SET NULL
  )
`).run();

/* outputs table */
db.prepare(`
  CREATE TABLE IF NOT EXISTS outputs (
    id TEXT PRIMARY KEY,
    pipeline_id TEXT NOT NULL,
    type TEXT NOT NULL,
    url TEXT NOT NULL,
    created_at TEXT,
    FOREIGN KEY(pipeline_id) REFERENCES pipelines(id) ON DELETE CASCADE
  )
`).run();

db.prepare(`CREATE INDEX IF NOT EXISTS idx_outputs_pipeline ON outputs(pipeline_id)`).run();


/* jobs table */
db.prepare(`
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
`).run();

db.prepare(`
  CREATE INDEX IF NOT EXISTS idx_jobs_pipeline_output ON jobs(pipeline_id, output_id)
`).run();

/* job_logs table */
db.prepare(`
  CREATE TABLE IF NOT EXISTS job_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id TEXT NOT NULL,
    ts TEXT,
    message TEXT,
    FOREIGN KEY(job_id) REFERENCES jobs(id) ON DELETE CASCADE
  )
`).run();




/* StreamKey statements */
const insertStreamKey = db.prepare('INSERT INTO stream_keys (key, label, created_at) VALUES (@key, @label, @created_at)');
const getStreamKeyStmt = db.prepare('SELECT key, label, created_at AS createdAt FROM stream_keys WHERE key = ?');
const listStreamKeysStmt = db.prepare('SELECT key, label, created_at AS createdAt FROM stream_keys ORDER BY created_at DESC');
const updateStreamKeyStmt = db.prepare('UPDATE stream_keys SET label = @label WHERE key = @key');
const deleteStreamKeyStmt = db.prepare('DELETE FROM stream_keys WHERE key = ?');

/* Pipeline statements */
const insertPipeline = db.prepare('INSERT INTO pipelines (id, name, stream_key, created_at, updated_at) VALUES (@id, @name, @stream_key, @created_at, @updated_at)');
const getPipelineStmt = db.prepare('SELECT id, name, stream_key AS streamKey, created_at AS createdAt, updated_at AS updatedAt FROM pipelines WHERE id = ?');
const listPipelinesStmt = db.prepare('SELECT id, name, stream_key AS streamKey, created_at AS createdAt, updated_at AS updatedAt FROM pipelines ORDER BY created_at DESC');
const updatePipelineStmt = db.prepare('UPDATE pipelines SET name = @name, stream_key = @stream_key, updated_at = @updated_at WHERE id = @id');
const deletePipelineStmt = db.prepare('DELETE FROM pipelines WHERE id = ?');

/* Output statements */
const insertOutput = db.prepare('INSERT INTO outputs (id, pipeline_id, type, url, created_at) VALUES (@id, @pipeline_id, @type, @url, @created_at)');
const getOutputStmt = db.prepare('SELECT id, pipeline_id AS pipelineId, type, url, created_at AS createdAt FROM outputs WHERE id = ? AND pipeline_id = ?');
const listOutputsStmt = db.prepare('SELECT id, pipeline_id AS pipelineId, type, url, created_at AS createdAt FROM outputs WHERE pipeline_id = ? ORDER BY created_at DESC');
const updateOutputStmt = db.prepare('UPDATE outputs SET type = @type, url = @url WHERE id = @id AND pipeline_id = @pipeline_id');
const deleteOutputStmt = db.prepare('DELETE FROM outputs WHERE id = ? AND pipeline_id = ?');


/* Job statements */
const insertJob = db.prepare(`
  INSERT INTO jobs (id, pipeline_id, output_id, pid, status, started_at) 
  VALUES (@id, @pipeline_id, @output_id, @pid, @status, @started_at)
`);
const getJobStmt = db.prepare(`
  SELECT id, pipeline_id AS pipelineId, output_id AS outputId, pid, status, started_at AS startedAt, ended_at AS endedAt, exit_code AS exitCode, exit_signal AS exitSignal
  FROM jobs WHERE id = ?
`);
const getRunningJobByPipelineOutputStmt = db.prepare(`
  SELECT * FROM jobs WHERE pipeline_id = ? AND output_id = ? AND status = 'running' LIMIT 1
`);
const updateJobStmt = db.prepare(`
  UPDATE jobs SET pid = @pid, status = @status, ended_at = @ended_at, exit_code = @exit_code, exit_signal = @exit_signal WHERE id = @id
`);
const listJobsForOutputStmt = db.prepare(`
  SELECT id, pipeline_id AS pipelineId, output_id AS outputId, pid, status, started_at AS startedAt, ended_at AS endedAt, exit_code AS exitCode, exit_signal AS exitSignal
  FROM jobs WHERE pipeline_id = ? AND output_id = ? ORDER BY started_at DESC
`);

/* JobLog statements */
const insertJobLog = db.prepare(`
  INSERT INTO job_logs (job_id, ts, message) VALUES (@job_id, @ts, @message)
`);
const listJobLogs = db.prepare(`
  SELECT ts, message FROM job_logs WHERE job_id = ? ORDER BY id ASC
`);



/* Exported DB helpers */

module.exports = {
    /* stream key helpers */
    createStreamKey({ key, label, createdAt }) {
        insertStreamKey.run({ key, label, created_at: createdAt });
        return getStreamKeyStmt.get(key);
    },
    getStreamKey(key) {
        return getStreamKeyStmt.get(key);
    },
    listStreamKeys() {
        return listStreamKeysStmt.all();
    },
    updateStreamKey(key, label) {
        const info = updateStreamKeyStmt.run({ key, label });
        return info.changes > 0 ? getStreamKeyStmt.get(key) : null;
    },
    deleteStreamKey(key) {
        const info = deleteStreamKeyStmt.run(key);
        return info.changes > 0;
    },

    /* pipeline helpers */
    createPipeline({ id, name, streamKey = null, createdAt }) {
        if (!name || typeof name !== 'string') throw new Error('Pipeline.name is required');
        const pid = id || crypto.randomBytes(8).toString('hex');
        const now = createdAt || new Date().toISOString();
        insertPipeline.run({ id: pid, name, stream_key: streamKey, created_at: now, updated_at: null });
        return getPipelineStmt.get(pid);
    },
    getPipeline(id) {
        return getPipelineStmt.get(id);
    },
    listPipelines() {
        return listPipelinesStmt.all();
    },
    updatePipeline(id, { name, streamKey, updatedAt }) {
        const now = updatedAt || new Date().toISOString();
        const info = updatePipelineStmt.run({ id, name, stream_key: streamKey, updated_at: now });
        return info.changes > 0 ? getPipelineStmt.get(id) : null;
    },
    deletePipeline(id) {
        const info = deletePipelineStmt.run(id);
        return info.changes > 0;
    },

    /* output helpers */
    createOutput({ id, pipelineId, type, url, createdAt }) {
        if (!pipelineId) throw new Error('pipelineId is required');
        if (!type || !url) throw new Error('Output.type and Output.url are required');
        const oid = id || crypto.randomBytes(8).toString('hex');
        const now = createdAt || new Date().toISOString();
        insertOutput.run({ id: oid, pipeline_id: pipelineId, type, url, created_at: now });
        return getOutputStmt.get(oid, pipelineId);
    },
    getOutput(pipelineId, id) {
        return getOutputStmt.get(id, pipelineId);
    },
    listOutputs(pipelineId) {
        return listOutputsStmt.all(pipelineId);
    },
    updateOutput(pipelineId, id, { type, url }) {
        const info = updateOutputStmt.run({ id, pipeline_id: pipelineId, type, url });
        return info.changes > 0 ? getOutputStmt.get(id, pipelineId) : null;
    },
    deleteOutput(pipelineId, id) {
        const info = deleteOutputStmt.run(id, pipelineId);
        return info.changes > 0;
    },

    /* job helpers */
    createJob({ id, pipelineId, outputId, pid = null, status = 'running', startedAt }) {
        const jid = id || crypto.randomBytes(8).toString('hex');
        const now = startedAt || new Date().toISOString();
        insertJob.run({ id: jid, pipeline_id: pipelineId, output_id: outputId, pid, status, started_at: now });
        return getJobStmt.get(jid);
    },
    getJob(id) {
        return getJobStmt.get(id);
    },
    getRunningJobFor(pipelineId, outputId) {
        return getRunningJobByPipelineOutputStmt.get(pipelineId, outputId);
    },
    updateJob(id, { pid = null, status = null, endedAt = null, exitCode = null, exitSignal = null } = {}) {
        updateJobStmt.run({ id, pid, status, ended_at: endedAt, exit_code: exitCode, exit_signal: exitSignal });
        return getJobStmt.get(id);
    },
    listJobsForOutput(pipelineId, outputId) {
        return listJobsForOutputStmt.all(pipelineId, outputId);
    },

    /* job log helpers */
    appendJobLog(jobId, message) {
        try {
            insertJobLog.run({ job_id: jobId, ts: new Date().toISOString(), message });
        } catch (e) { /* ignore logging failures */ }
    },
    listJobLogs(jobId) {
        return listJobLogs.all(jobId);
    }
};

