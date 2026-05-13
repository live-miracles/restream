import path from 'path';
import Database from 'better-sqlite3';
import crypto from 'crypto';
import { setupDatabaseSchema } from './schema';
import type { Pipeline, Output, Job, JobLog, HistoryFilters } from '../types';

const projectRoot = path.join(__dirname, '..', '..');
const dbPath = path.join(projectRoot, 'data.db');

const db = new Database(dbPath);
setupDatabaseSchema(db);

function normalizeOutputEncodingValue(encoding: unknown): string {
    const normalized = String(encoding ?? 'source')
        .trim()
        .toLowerCase();
    if (!normalized) return 'source';
    return normalized;
}

/* Pipeline statements */
const insertPipeline = db.prepare(
    'INSERT INTO pipelines (id, name, stream_key, encoding) VALUES (@id, @name, @stream_key, @encoding)',
);
const getPipelineStmt = db.prepare(
    'SELECT id, name, stream_key AS streamKey, encoding FROM pipelines WHERE id = ?',
);
const listPipelinesStmt = db.prepare(
    'SELECT id, name, stream_key AS streamKey, encoding FROM pipelines',
);
const updatePipelineStmt = db.prepare(
    'UPDATE pipelines SET name = @name, stream_key = @stream_key, encoding = @encoding WHERE id = @id',
);
const deletePipelineStmt = db.prepare('DELETE FROM pipelines WHERE id = ?');

/* Output statements */
const insertOutput = db.prepare(
    'INSERT INTO outputs (id, pipeline_id, name, url, desired_state, encoding) VALUES (@id, @pipeline_id, @name, @url, @desired_state, @encoding)',
);
const getOutputStmt = db.prepare(
    'SELECT id, pipeline_id AS pipelineId, name, url, desired_state AS desiredState, encoding FROM outputs WHERE id = ? AND pipeline_id = ?',
);
const listOutputsStmt = db.prepare(
    'SELECT id, pipeline_id AS pipelineId, name, url, desired_state AS desiredState, encoding FROM outputs',
);
const listOutputsForPipelineStmt = db.prepare(
    'SELECT id, pipeline_id AS pipelineId, name, url, desired_state AS desiredState, encoding FROM outputs WHERE pipeline_id = ? ORDER BY rowid ASC',
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
const listJobLogsStmt = db.prepare(`
    SELECT ts, message, event_type AS eventType, event_data AS eventData FROM job_logs WHERE job_id = ? ORDER BY id ASC
`);
const listJobLogsByOutputStmt = db.prepare(`
    SELECT ts, message, event_type AS eventType, event_data AS eventData FROM job_logs
    WHERE pipeline_id = ? AND output_id = ?
    ORDER BY ts DESC
`);
const listLifecycleLogsByOutputStmt = db.prepare(`
    SELECT ts, message, event_type AS eventType, event_data AS eventData FROM job_logs
    WHERE pipeline_id = ? AND output_id = ? AND (event_type LIKE 'lifecycle.%' OR message LIKE '[lifecycle]%')
    ORDER BY ts ASC
`);
const listJobLogsByPipelineStmt = db.prepare(`
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

/* Helpers */

function serializeEventData(eventData: unknown): string | null {
    if (eventData === null || eventData === undefined) return null;
    try {
        return JSON.stringify(eventData);
    } catch {
        return null;
    }
}

interface RawLogRow {
    ts: string;
    message: string;
    eventType: string;
    eventData: string | null;
}

function parseLogRow(row: RawLogRow | undefined): JobLog | undefined {
    if (!row) return row;

    let eventData: unknown = null;
    if (typeof row.eventData === 'string' && row.eventData.length > 0) {
        try {
            eventData = JSON.parse(row.eventData);
        } catch {
            eventData = null;
        }
    }

    return { ...row, eventData };
}

function parseLogRows(rows: RawLogRow[]): JobLog[] {
    return rows.map((r) => parseLogRow(r) as JobLog);
}

function listJobLogsByOutputFiltered(
    pipelineId: string,
    outputId: string,
    {
        since = null,
        until = null,
        limit = null,
        order = 'desc',
        prefixes = [],
    }: HistoryFilters = {},
): JobLog[] {
    const clauses = ['pipeline_id = ?', 'output_id = ?'];
    const params: unknown[] = [pipelineId, outputId];

    if (since) {
        clauses.push('ts >= ?');
        params.push(since);
    }
    if (until) {
        clauses.push('ts < ?');
        params.push(until);
    }

    if (Array.isArray(prefixes) && prefixes.length > 0) {
        const prefixClauses: string[] = [];
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

    if (Number.isFinite(limit) && limit !== null && limit > 0) {
        sql += '\nLIMIT ?';
        params.push(limit);
    }

    return parseLogRows(db.prepare(sql).all(...params) as RawLogRow[]);
}

/* Exported DB helpers */

export function createPipeline({
    id,
    name,
    streamKey,
    encoding = null,
}: {
    id?: string;
    name: string;
    streamKey: string;
    encoding?: string | null;
}): Pipeline {
    if (!name || typeof name !== 'string') throw new Error('Pipeline.name is required');
    if (!streamKey || typeof streamKey !== 'string') {
        throw new Error('Pipeline.streamKey is required');
    }
    const pid = id || crypto.randomBytes(8).toString('hex');
    insertPipeline.run({ id: pid, name, stream_key: streamKey, encoding });
    return getPipelineStmt.get(pid) as Pipeline;
}

export function getPipeline(id: string): Pipeline | undefined {
    return getPipelineStmt.get(id) as Pipeline | undefined;
}

export function listPipelines(): Pipeline[] {
    return listPipelinesStmt.all() as Pipeline[];
}

export function updatePipeline(
    id: string,
    {
        name,
        streamKey,
        encoding = null,
    }: { name: string; streamKey: string; encoding?: string | null },
): Pipeline | null {
    const info = updatePipelineStmt.run({ id, name, stream_key: streamKey, encoding });
    return (info.changes > 0 ? getPipelineStmt.get(id) : null) as Pipeline | null;
}

export function deletePipeline(id: string): boolean {
    const info = deletePipelineStmt.run(id);
    return info.changes > 0;
}

export function createOutput({
    id,
    pipelineId,
    name,
    url,
    desiredState = 'stopped',
    encoding = 'source',
}: {
    id?: string;
    pipelineId: string;
    name: string;
    url: string;
    desiredState?: string;
    encoding?: string;
}): Output {
    if (!pipelineId) throw new Error('pipelineId is required');
    if (!name || !url) throw new Error('Output.name and Output.url are required');
    const oid = id || crypto.randomBytes(8).toString('hex');
    insertOutput.run({
        id: oid,
        pipeline_id: pipelineId,
        name,
        url,
        desired_state: desiredState === 'running' ? 'running' : 'stopped',
        encoding: normalizeOutputEncodingValue(encoding),
    });
    return getOutputStmt.get(oid, pipelineId) as Output;
}

export function getOutput(pipelineId: string, id: string): Output | undefined {
    return getOutputStmt.get(id, pipelineId) as Output | undefined;
}

export function listOutputs(): Output[] {
    return listOutputsStmt.all() as Output[];
}

export function listOutputsForPipeline(pipelineId: string): Output[] {
    return listOutputsForPipelineStmt.all(pipelineId) as Output[];
}

export function updateOutput(
    pipelineId: string,
    id: string,
    { name, url, encoding = 'source' }: { name: string; url: string; encoding?: string },
): Output | null {
    const info = updateOutputStmt.run({
        id,
        pipeline_id: pipelineId,
        name,
        url,
        encoding: normalizeOutputEncodingValue(encoding),
    });
    return (info.changes > 0 ? getOutputStmt.get(id, pipelineId) : null) as Output | null;
}

export function setOutputDesiredState(
    pipelineId: string,
    id: string,
    desiredState: string,
): Output {
    setOutputDesiredStateStmt.run({
        id,
        pipeline_id: pipelineId,
        desired_state: desiredState === 'running' ? 'running' : 'stopped',
    });
    return getOutputStmt.get(id, pipelineId) as Output;
}

export function deleteOutput(pipelineId: string, id: string): boolean {
    const info = deleteOutputStmt.run(id, pipelineId);
    return info.changes > 0;
}

export function createJob({
    id,
    pipelineId,
    outputId,
    pid = null,
    status = 'running',
    startedAt,
}: {
    id?: string;
    pipelineId: string;
    outputId: string;
    pid?: number | null;
    status?: string;
    startedAt?: string;
}): Job {
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
    return getJobStmt.get(jid) as Job;
}

export function getJob(id: string): Job | undefined {
    return getJobStmt.get(id) as Job | undefined;
}

export function getRunningJobFor(pipelineId: string, outputId: string): Job | undefined {
    return getRunningJobByPipelineOutputStmt.get(pipelineId, outputId) as Job | undefined;
}

export function updateJob(
    id: string,
    {
        pid = null,
        status = null,
        endedAt = null,
        exitCode = null,
        exitSignal = null,
    }: {
        pid?: number | null;
        status?: string | null;
        endedAt?: string | null;
        exitCode?: number | null;
        exitSignal?: string | null;
    } = {},
): Job | undefined {
    updateJobStmt.run({
        id,
        pid,
        status,
        ended_at: endedAt,
        exit_code: exitCode,
        exit_signal: exitSignal,
    });
    return getJobStmt.get(id) as Job | undefined;
}

export function listJobsForOutput(pipelineId: string, outputId: string): Job[] {
    return listJobsForOutputStmt.all(pipelineId, outputId) as Job[];
}

export function listJobs(): Job[] {
    return listJobsStmt.all() as Job[];
}

export function appendJobLog(
    jobId: string | null,
    message: string,
    pipelineId: string | null = null,
    outputId: string | null = null,
    eventType = 'output.log',
    eventData: unknown = null,
): void {
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
    } catch {
        /* ignore logging failures */
    }
}

export function appendPipelineEvent(
    pipelineId: string,
    message: string,
    eventType = 'pipeline.event',
    eventData: unknown = null,
): void {
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
    } catch {
        /* ignore logging failures */
    }
}

export function listJobLogs(jobId: string): JobLog[] {
    return parseLogRows(listJobLogsStmt.all(jobId) as RawLogRow[]);
}

export function listJobLogsByOutput(pipelineId: string, outputId: string): JobLog[] {
    return parseLogRows(listJobLogsByOutputStmt.all(pipelineId, outputId) as RawLogRow[]);
}

export { listJobLogsByOutputFiltered };

export function listLifecycleLogsByOutput(pipelineId: string, outputId: string): JobLog[] {
    return parseLogRows(listLifecycleLogsByOutputStmt.all(pipelineId, outputId) as RawLogRow[]);
}

export function listJobLogsByPipeline(pipelineId: string): JobLog[] {
    return parseLogRows(listJobLogsByPipelineStmt.all(pipelineId) as RawLogRow[]);
}

export function deleteJobLogsOlderThan(days = 7): void {
    deleteOldJobLogs.run(`-${days} days`);
}

export function cleanupOldJobs(): { deletedJobs: number; deletedLogs: number } {
    const tx = db.transaction(() => {
        const jobResult = deleteOldJobs.run();
        return { deletedJobs: jobResult.changes, deletedLogs: 0 };
    });
    return tx();
}

export function getMeta(key: string): string | null {
    const r = getMetaStmt.get(key) as { value: string } | undefined;
    return r ? r.value : null;
}

export function setMeta(key: string, value: string): string {
    setMetaStmt.run({ key, value });
    return value;
}

export function getCustomEncoding(): string | null {
    return getMeta('custom_encoding') || null;
}

export function setCustomEncoding(ffmpegArgs: string): string {
    return setMeta('custom_encoding', ffmpegArgs || '');
}

export function getServerName(): string {
    return getMeta('server_name') || 'Name';
}

export function setServerName(name: string): string {
    const trimmed = typeof name === 'string' ? name.trim() : '';
    return setMeta('server_name', trimmed || 'Name');
}
