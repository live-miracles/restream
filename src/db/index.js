const fs = require('fs');
const path = require('path');
const Database = require('better-sqlite3');
const crypto = require('crypto');
const { setupDatabaseSchema } = require('./schema');
const projectRoot = path.join(__dirname, '..', '..');
const dataDir = path.join(projectRoot, 'data');
const dbPath = path.join(dataDir, 'data.db');

fs.mkdirSync(dataDir, { recursive: true });

const db = new Database(dbPath);
setupDatabaseSchema(db);

function normalizeOutputEncodingValue(encoding) {
    const normalized = String(encoding ?? 'source')
        .trim()
        .toLowerCase();
    if (!normalized) return 'source';
    return normalized;
}

/* StreamKey statements */
const insertStreamKey = db.prepare(
    'INSERT INTO stream_keys (key, label, created_at) VALUES (@key, @label, @created_at)',
);
const getStreamKeyStmt = db.prepare(
    'SELECT key, label, created_at AS createdAt FROM stream_keys WHERE key = ?',
);
const listStreamKeysStmt = db.prepare(
    'SELECT key, label, created_at AS createdAt FROM stream_keys ORDER BY created_at DESC',
);
const updateStreamKeyStmt = db.prepare('UPDATE stream_keys SET label = @label WHERE key = @key');
const deleteStreamKeyStmt = db.prepare('DELETE FROM stream_keys WHERE key = ?');

/* Pipeline statements */
const insertPipeline = db.prepare(
    'INSERT INTO pipelines (id, name, stream_key, encoding, input_ever_seen_live, created_at, updated_at) VALUES (@id, @name, @stream_key, @encoding, @input_ever_seen_live, @created_at, @updated_at)',
);
const getPipelineStmt = db.prepare(
    'SELECT id, name, stream_key AS streamKey, encoding, input_ever_seen_live AS inputEverSeenLive, created_at AS createdAt, updated_at AS updatedAt FROM pipelines WHERE id = ?',
);
const listPipelinesStmt = db.prepare(
    'SELECT id, name, stream_key AS streamKey, encoding, input_ever_seen_live AS inputEverSeenLive, created_at AS createdAt, updated_at AS updatedAt FROM pipelines',
);
const updatePipelineStmt = db.prepare(
    'UPDATE pipelines SET name = @name, stream_key = @stream_key, encoding = @encoding, input_ever_seen_live = @input_ever_seen_live, updated_at = @updated_at WHERE id = @id',
);
const markPipelineInputSeenLiveStmt = db.prepare(
    'UPDATE pipelines SET input_ever_seen_live = 1 WHERE id = @id',
);
const deletePipelineStmt = db.prepare('DELETE FROM pipelines WHERE id = ?');

/* Output statements */
const insertOutput = db.prepare(
    'INSERT INTO outputs (id, pipeline_id, name, url, desired_state, encoding, created_at) VALUES (@id, @pipeline_id, @name, @url, @desired_state, @encoding, @created_at)',
);
const getOutputStmt = db.prepare(
    'SELECT id, pipeline_id AS pipelineId, name, url, desired_state AS desiredState, encoding, created_at AS createdAt FROM outputs WHERE id = ? AND pipeline_id = ?',
);
const listOutputsStmt = db.prepare(
    'SELECT id, pipeline_id AS pipelineId, name, url, desired_state AS desiredState, encoding, created_at AS createdAt FROM outputs',
);
const listOutputsForPipelineStmt = db.prepare(
    'SELECT id, pipeline_id AS pipelineId, name, url, desired_state AS desiredState, encoding, created_at AS createdAt FROM outputs WHERE pipeline_id = ? ORDER BY created_at ASC, id ASC',
);
const updateOutputStmt = db.prepare(
    'UPDATE outputs SET name = @name, url = @url, encoding = @encoding WHERE id = @id AND pipeline_id = @pipeline_id',
);
const setOutputDesiredStateStmt = db.prepare(
    'UPDATE outputs SET desired_state = @desired_state WHERE id = @id AND pipeline_id = @pipeline_id',
);
const deleteOutputStmt = db.prepare('DELETE FROM outputs WHERE id = ? AND pipeline_id = ?');

/* Job statements */
const insertJob = db.prepare(`
    INSERT INTO jobs (id, pipeline_id, output_id, pid, status, started_at, ended_at, exit_code, exit_signal)
    VALUES (@id, @pipeline_id, @output_id, @pid, @status, @started_at, NULL, NULL, NULL)
    ON CONFLICT(pipeline_id, output_id) DO UPDATE SET
        id = excluded.id,
        pid = excluded.pid,
        status = excluded.status,
        started_at = excluded.started_at,
        ended_at = NULL,
        exit_code = NULL,
        exit_signal = NULL
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
const listJobsStmt = db.prepare(`
    SELECT id, pipeline_id AS pipelineId, output_id AS outputId, pid, status, started_at AS startedAt, ended_at AS endedAt, exit_code AS exitCode, exit_signal AS exitSignal
    FROM jobs ORDER BY started_at DESC, id DESC
`);

/* JobLog statements */
const insertJobLog = db.prepare(`
    INSERT INTO job_logs (job_id, pipeline_id, output_id, event_type, event_data, ts, message)
    VALUES (@job_id, @pipeline_id, @output_id, @event_type, @event_data, @ts, @message)
`);
const listJobLogs = db.prepare(`
    SELECT ts, message, event_type AS eventType, event_data AS eventData FROM job_logs WHERE job_id = ? ORDER BY id ASC
`);
const listJobLogsByOutput = db.prepare(`
    SELECT ts, message, event_type AS eventType, event_data AS eventData FROM job_logs
    WHERE pipeline_id = ? AND output_id = ?
    ORDER BY ts DESC
`);
const listLifecycleLogsByOutput = db.prepare(`
    SELECT ts, message, event_type AS eventType, event_data AS eventData FROM job_logs
    WHERE pipeline_id = ? AND output_id = ? AND (event_type LIKE 'lifecycle.%' OR message LIKE '[lifecycle]%')
    ORDER BY ts ASC
`);
function listJobLogsByOutputFiltered(
    pipelineId,
    outputId,
    { since = null, until = null, limit = null, order = 'desc', prefixes = [] } = {},
) {
    const clauses = ['pipeline_id = ?', 'output_id = ?'];
    const params = [pipelineId, outputId];

    if (since) {
        clauses.push('ts >= ?');
        params.push(since);
    }
    if (until) {
        clauses.push('ts < ?');
        params.push(until);
    }

    if (Array.isArray(prefixes) && prefixes.length > 0) {
        const prefixClauses = [];
        for (const prefix of prefixes) {
            prefixClauses.push('message LIKE ?');
            params.push(`${prefix}%`);
        }
        clauses.push(`(${prefixClauses.join(' OR ')})`);
    }

    const normalizedOrder = order === 'asc' ? 'ASC' : 'DESC';
    let sql = `
        SELECT ts, message, event_type AS eventType, event_data AS eventData FROM job_logs
        WHERE ${clauses.join(' AND ')}
        ORDER BY ts ${normalizedOrder}
    `;

    if (Number.isFinite(limit) && limit > 0) {
        sql += '\nLIMIT ?';
        params.push(limit);
    }

    return db.prepare(sql).all(...params);
}
const listJobLogsByPipeline = db.prepare(`
    SELECT ts, message, event_type AS eventType, event_data AS eventData FROM job_logs
    WHERE pipeline_id = ? AND output_id IS NULL
    ORDER BY ts DESC
`);
const deleteOldJobLogs = db.prepare(`
    DELETE FROM job_logs WHERE ts < datetime('now', ?)
`);
const deleteOldJobs = db.prepare(`
    DELETE FROM jobs
    WHERE (status IN ('stopped','failed') AND ended_at IS NOT NULL AND datetime(ended_at) < datetime('now', '-7 days'))
       OR datetime(COALESCE(ended_at, started_at)) < datetime('now', '-30 days')
`);

/* Meta statements */
const getMetaStmt = db.prepare(`SELECT value FROM meta WHERE key = ?`);
const setMetaStmt = db.prepare(`
  INSERT INTO meta (key, value) VALUES (@key, @value)
  ON CONFLICT(key) DO UPDATE SET value = excluded.value
`);

/* Exported DB helpers */

function serializeEventData(eventData) {
    if (eventData === null || eventData === undefined) return null;
    try {
        return JSON.stringify(eventData);
    } catch {
        return null;
    }
}

function parseLogRow(row) {
    if (!row) return row;

    let eventData = null;
    if (typeof row.eventData === 'string' && row.eventData.length > 0) {
        try {
            eventData = JSON.parse(row.eventData);
        } catch {
            eventData = null;
        }
    }

    return {
        ...row,
        eventData,
    };
}

function parseLogRows(rows) {
    return rows.map(parseLogRow);
}

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
    createPipeline({ id, name, streamKey = null, encoding = null, createdAt }) {
        if (!name || typeof name !== 'string') throw new Error('Pipeline.name is required');
        const pid = id || crypto.randomBytes(8).toString('hex');
        const now = createdAt || new Date().toISOString();
        insertPipeline.run({
            id: pid,
            name,
            stream_key: streamKey,
            encoding: encoding,
            input_ever_seen_live: 0,
            created_at: now,
            updated_at: null,
        });
        return getPipelineStmt.get(pid);
    },
    getPipeline(id) {
        return getPipelineStmt.get(id);
    },
    listPipelines() {
        return listPipelinesStmt.all();
    },
    updatePipeline(id, { name, streamKey, encoding = null, inputEverSeenLive = 0, updatedAt }) {
        const now = updatedAt || new Date().toISOString();
        const info = updatePipelineStmt.run({
            id,
            name,
            stream_key: streamKey,
            encoding,
            input_ever_seen_live: inputEverSeenLive,
            updated_at: now,
        });
        return info.changes > 0 ? getPipelineStmt.get(id) : null;
    },
    markPipelineInputSeenLive(id) {
        markPipelineInputSeenLiveStmt.run({ id });
        return getPipelineStmt.get(id);
    },
    deletePipeline(id) {
        const info = deletePipelineStmt.run(id);
        return info.changes > 0;
    },

    /* output helpers */
    createOutput({
        id,
        pipelineId,
        name,
        url,
        desiredState = 'stopped',
        encoding = 'source',
        createdAt,
    }) {
        if (!pipelineId) throw new Error('pipelineId is required');
        if (!name || !url) throw new Error('Output.name and Output.url are required');
        const oid = id || crypto.randomBytes(8).toString('hex');
        const now = createdAt || new Date().toISOString();
        insertOutput.run({
            id: oid,
            pipeline_id: pipelineId,
            name,
            url,
            desired_state: desiredState === 'running' ? 'running' : 'stopped',
            encoding: normalizeOutputEncodingValue(encoding),
            created_at: now,
        });
        return getOutputStmt.get(oid, pipelineId);
    },
    getOutput(pipelineId, id) {
        return getOutputStmt.get(id, pipelineId);
    },
    listOutputs() {
        return listOutputsStmt.all();
    },
    listOutputsForPipeline(pipelineId) {
        return listOutputsForPipelineStmt.all(pipelineId);
    },
    updateOutput(pipelineId, id, { name, url, encoding = 'source' }) {
        const info = updateOutputStmt.run({
            id,
            pipeline_id: pipelineId,
            name,
            url,
            encoding: normalizeOutputEncodingValue(encoding),
        });
        return info.changes > 0 ? getOutputStmt.get(id, pipelineId) : null;
    },
    setOutputDesiredState(pipelineId, id, desiredState) {
        const info = setOutputDesiredStateStmt.run({
            id,
            pipeline_id: pipelineId,
            desired_state: desiredState === 'running' ? 'running' : 'stopped',
        });
        return info.changes > 0
            ? getOutputStmt.get(id, pipelineId)
            : getOutputStmt.get(id, pipelineId);
    },
    deleteOutput(pipelineId, id) {
        const info = deleteOutputStmt.run(id, pipelineId);
        return info.changes > 0;
    },

    /* job helpers */
    createJob({ id, pipelineId, outputId, pid = null, status = 'running', startedAt }) {
        const jid = id || crypto.randomBytes(8).toString('hex');
        const now = startedAt || new Date().toISOString();
        insertJob.run({
            id: jid,
            pipeline_id: pipelineId,
            output_id: outputId,
            pid,
            status,
            started_at: now,
        });
        return getJobStmt.get(jid);
    },
    getJob(id) {
        return getJobStmt.get(id);
    },
    getRunningJobFor(pipelineId, outputId) {
        return getRunningJobByPipelineOutputStmt.get(pipelineId, outputId);
    },
    updateJob(
        id,
        { pid = null, status = null, endedAt = null, exitCode = null, exitSignal = null } = {},
    ) {
        updateJobStmt.run({
            id,
            pid,
            status,
            ended_at: endedAt,
            exit_code: exitCode,
            exit_signal: exitSignal,
        });
        return getJobStmt.get(id);
    },
    listJobsForOutput(pipelineId, outputId) {
        return listJobsForOutputStmt.all(pipelineId, outputId);
    },
    listJobs() {
        return listJobsStmt.all();
    },

    /* job log helpers */
    appendJobLog(
        jobId,
        message,
        pipelineId = null,
        outputId = null,
        eventType = 'output.log',
        eventData = null,
    ) {
        try {
            insertJobLog.run({
                job_id: jobId,
                pipeline_id: pipelineId,
                output_id: outputId,
                event_type: eventType,
                event_data: serializeEventData(eventData),
                ts: new Date().toISOString(),
                message,
            });
        } catch (e) {
            /* ignore logging failures */
        }
    },
    appendPipelineEvent(
        pipelineId,
        message,
        eventType = 'pipeline.event',
        eventData = null,
    ) {
        try {
            insertJobLog.run({
                job_id: null,
                pipeline_id: pipelineId,
                output_id: null,
                event_type: eventType,
                event_data: serializeEventData(eventData),
                ts: new Date().toISOString(),
                message,
            });
        } catch (e) {
            /* ignore logging failures */
        }
    },
    listJobLogs(jobId) {
        return parseLogRows(listJobLogs.all(jobId));
    },
    listJobLogsByOutput(pipelineId, outputId) {
        return parseLogRows(listJobLogsByOutput.all(pipelineId, outputId));
    },
    listJobLogsByOutputFiltered(pipelineId, outputId, filters = {}) {
        return parseLogRows(listJobLogsByOutputFiltered(pipelineId, outputId, filters));
    },
    listLifecycleLogsByOutput(pipelineId, outputId) {
        return parseLogRows(listLifecycleLogsByOutput.all(pipelineId, outputId));
    },
    listJobLogsByPipeline(pipelineId) {
        return parseLogRows(listJobLogsByPipeline.all(pipelineId));
    },
    deleteJobLogsOlderThan(days = 7) {
        deleteOldJobLogs.run(`-${days} days`);
    },
    cleanupOldJobs() {
        const tx = db.transaction(() => {
            const jobResult = deleteOldJobs.run();
            return { deletedJobs: jobResult.changes, deletedLogs: 0 };
        });
        return tx();
    },

    /* meta helpers */
    getMeta(key) {
        const r = getMetaStmt.get(key);
        return r ? r.value : null;
    },

    setMeta(key, value) {
        setMetaStmt.run({ key, value });
        return value;
    },

    getEtag() {
        return module.exports.getMeta('etag');
    },

    setEtag(v) {
        return module.exports.setMeta('etag', v);
    },

    getConfigEtag() {
        return module.exports.getMeta('config_etag');
    },

    setConfigEtag(v) {
        return module.exports.setMeta('config_etag', v);
    },
};
