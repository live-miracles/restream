

/* top requires */
const express = require('express');
const fetch = global.fetch || require('node-fetch'); // keep compatibility
const db = require('./db');

const app = express();
app.use(express.json());

const { spawn } = require('child_process');
const path = require('path');
const crypto = require('crypto');

app.use(express.json());

/* ======================
 * In-memory storage
 * ====================== */
let jobs = {};

const processes = new Map(); // runtime only: jobId -> ChildProcess


/* ======================
 * Models
 * ====================== */

// 1. StreamKey model
class StreamKey {
    constructor({ key, label = null, createdAt } = {}) {
        this.key = key || crypto.randomBytes(12).toString('hex');
        this.label = label ?? null;
        this.createdAt = createdAt || new Date().toISOString();
    }
}

// 2. Pipeline model
class Pipeline {
    constructor({ id, name, streamKey = null, createdAt, updatedAt } = {}) {
        if (!name || typeof name !== 'string') {
            throw new Error('Pipeline.name is required');
        }
        this.id = id || Date.now().toString();
        this.name = name;
        this.streamKey = streamKey;
        this.createdAt = createdAt || new Date().toISOString();
        this.updatedAt = updatedAt || null;
    }
}

// 3. Output model
class Output {
    constructor({ id, type, url } = {}) {
        if (!type || typeof type !== 'string') {
            throw new Error('Output.type is required');
        }
        if (!url || typeof url !== 'string') {
            throw new Error('Output.url is required');
        }
        this.id = id || Date.now().toString();
        this.type = type;
        this.url = url;
    }
}

// i think we should add the sqlite dependency now.
// and also, add the interactions with mediaMTX, like creating/removing paths

/* ======================
 * Stream Key APIs (updated to use SQLite)
 * ====================== */

// create stream key
app.post('/stream-keys', async (req, res) => {
    try {
        const key = req.body?.streamKey || crypto.randomBytes(12).toString('hex');
        const label = req.body?.label ?? null;

        if (db.getStreamKey(key)) {
            return res.status(409).json({ error: 'Stream key already exists' });
        }

        // call MediaMTX
        const url = `http://localhost:9997/v3/config/paths/add/${encodeURIComponent(key)}`;
        const resp = await fetch(url, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ name: key }),
        });

        let data = null;
        try { data = await resp.json(); } catch (e) { /* ignore parse errors */ }

        if (!resp.ok || data?.error) {
            return res.status(500).json({
                error: data?.error || `MediaMTX returned ${resp.status}`,
            });
        }

        const sk = db.createStreamKey({ key, label, createdAt: new Date().toISOString() });
        return res.status(201).json({
            message: 'Stream key created',
            streamKey: sk,
        });
    } catch (err) {
        return res.status(500).json({ error: err.toString() });
    }
});

// update stream key label
app.post('/stream-keys/:key', (req, res) => {
    try {
        const { key } = req.params;
        const { label } = req.body || {};

        const existing = db.getStreamKey(key);
        if (!existing) {
            return res.status(404).json({ error: 'Stream key not found' });
        }

        const updated = db.updateStreamKey(key, label ?? null);
        return res.json({ message: 'Stream key updated', streamKey: updated });
    } catch (err) {
        return res.status(500).json({ error: err.toString() });
    }
});

// delete stream key
app.delete('/stream-keys/:key', async (req, res) => {
    try {
        const { key } = req.params;

        const existing = db.getStreamKey(key);
        if (!existing) {
            return res.status(404).json({ error: 'Stream key not found' });
        }

        const url = `http://localhost:9997/v3/config/paths/remove/${encodeURIComponent(key)}`;
        const resp = await fetch(url, { method: 'POST' });

        let data = null;
        try { data = await resp.json(); } catch (e) { /* ignore parse errors */ }

        if (!resp.ok || data?.error) {
            return res.status(500).json({
                error: data?.error || `MediaMTX returned ${resp.status}`,
            });
        }

        const deleted = db.deleteStreamKey(key);
        if (!deleted) {
            return res.status(500).json({ error: 'Failed to remove stream key from DB' });
        }

        return res.json({ message: 'Stream key deleted' });
    } catch (err) {
        return res.status(500).json({ error: err.toString() });
    }
});

// list stream keys
app.get('/stream-keys', (req, res) => {
    try {
        const keys = db.listStreamKeys();
        return res.json(keys);
    } catch (err) {
        return res.status(500).json({ error: err.toString() });
    }
});


/* ======================
 * Pipeline APIs
 * ====================== */

// create pipeline
app.post('/pipelines', (req, res) => {
    try {
        const name = req.body?.name;
        const streamKey = req.body?.streamKey ?? null;

        const pipeline = db.createPipeline({ name, streamKey });
        // recompute global etag if available
        if (typeof recomputeEtag === 'function') recomputeEtag();
        return res.status(201).json({ message: 'Pipeline created', pipeline });
    } catch (err) {
        return res.status(400).json({ error: err.message });
    }
});

// update pipeline
app.post('/pipelines/:id', (req, res) => {
    try {
        const id = req.params.id;
        const existing = db.getPipeline(id);
        if (!existing) return res.status(404).json({ error: 'Pipeline not found' });

        const name = req.body?.name ?? existing.name;
        const streamKey = req.body?.streamKey ?? existing.streamKey;

        const updated = db.updatePipeline(id, { name, streamKey });
        if (!updated) return res.status(500).json({ error: 'Failed to update pipeline' });

        if (typeof recomputeEtag === 'function') recomputeEtag();
        return res.json({ message: 'Pipeline updated', pipeline: updated });
    } catch (err) {
        return res.status(400).json({ error: err.message });
    }
});

// delete pipeline
app.delete('/pipelines/:id', (req, res) => {
    try {
        const id = req.params.id;
        const existing = db.getPipeline(id);
        if (!existing) return res.status(404).json({ error: 'Pipeline not found' });

        const ok = db.deletePipeline(id);
        if (!ok) return res.status(500).json({ error: 'Failed to delete pipeline' });

        if (typeof recomputeEtag === 'function') recomputeEtag();
        return res.json({ message: `Pipeline ${id} deleted` });
    } catch (err) {
        return res.status(500).json({ error: err.toString() });
    }
});

// list pipelines
app.get('/pipelines', (req, res) => {
    try {
        const pipelines = db.listPipelines();
        return res.json(pipelines);
    } catch (err) {
        return res.status(500).json({ error: err.toString() });
    }
});

/* ======================
 * Output APIs
 * ====================== */

// list outputs for pipeline
app.get('/pipelines/:id/outputs', (req, res) => {
    try {
        const pid = req.params.id;
        const pipeline = db.getPipeline(pid);
        if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });

        const outputs = db.listOutputs(pid);
        return res.json(outputs);
    } catch (err) {
        return res.status(500).json({ error: err.toString() });
    }
});

// create output
app.post('/pipelines/:pipelineId/outputs', (req, res) => {
    try {
        const pid = req.params.pipelineId;
        const pipeline = db.getPipeline(pid);
        if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });

        const type = req.body?.type;
        const url = req.body?.url;

        const output = db.createOutput({ pipelineId: pid, type, url });
        if (typeof recomputeEtag === 'function') recomputeEtag();

        return res.status(201).json({ message: 'Output created', output });
    } catch (err) {
        return res.status(400).json({ error: err.message || err.toString() });
    }
});

// update output
app.post('/pipelines/:pipelineId/outputs/:outputId', (req, res) => {
    try {
        const pid = req.params.pipelineId;
        const oid = req.params.outputId;
        const pipeline = db.getPipeline(pid);
        if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });

        const existing = db.getOutput(pid, oid);
        if (!existing) return res.status(404).json({ error: 'Output not found' });

        const type = req.body?.type ?? existing.type;
        const url = req.body?.url ?? existing.url;

        const updated = db.updateOutput(pid, oid, { type, url });
        if (!updated) return res.status(500).json({ error: 'Failed to update output' });

        if (typeof recomputeEtag === 'function') recomputeEtag();
        return res.json({ message: 'Output updated', output: updated });
    } catch (err) {
        return res.status(400).json({ error: err.message || err.toString() });
    }
});

// delete output
app.delete('/pipelines/:pipelineId/outputs/:outputId', (req, res) => {
    try {
        const pid = req.params.pipelineId;
        const oid = req.params.outputId;
        const pipeline = db.getPipeline(pid);
        if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });

        const existing = db.getOutput(pid, oid);
        if (!existing) return res.status(404).json({ error: 'Output not found' });

        const ok = db.deleteOutput(pid, oid);
        if (!ok) return res.status(500).json({ error: 'Failed to delete output' });

        if (typeof recomputeEtag === 'function') recomputeEtag();
        return res.json({ message: `Output ${oid} from pipeline ${pid} deleted` });
    } catch (err) {
        return res.status(500).json({ error: err.toString() });
    }
});

// get output detail
app.get('/pipelines/:pipelineId/outputs/:outputId', (req, res) => {
    try {
        const pid = req.params.pipelineId;
        const oid = req.params.outputId;
        const pipeline = db.getPipeline(pid);
        if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });

        const output = db.getOutput(pid, oid);
        if (!output) return res.status(404).json({ error: 'Output not found' });

        return res.json(output);
    } catch (err) {
        return res.status(500).json({ error: err.toString() });
    }
});


/* ======================
 * Start/Stop Output APIs
 * ====================== */
// we should manage the FFMPEG processes here, and start/stop them accordingly.

// start output (spawn ffmpeg)
app.post('/pipelines/:pipelineId/outputs/:outputId/start', async (req, res) => {
    try {
        const pid = req.params.pipelineId;
        const oid = req.params.outputId;
        const pipeline = db.getPipeline(pid);
        if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });

        const output = db.getOutput(pid, oid);
        if (!output) return res.status(404).json({ error: 'Output not found' });

        // ensure no running job in DB for this pipeline+output
        const existingRunning = db.getRunningJobFor(pid, oid);
        if (existingRunning) return res.status(409).json({ error: 'Output already has a running job', job: existingRunning });

        const inputUrl = pipeline.streamKey ? `rtmp://localhost:1935/${pipeline.streamKey}` : req.body?.inputUrl;
        if (!inputUrl) return res.status(400).json({ error: 'No inputUrl available (pipeline.streamKey missing and no inputUrl provided)' });

        const outputUrl = output.url;
        if (!outputUrl) return res.status(400).json({ error: 'Output URL is empty' });

        const ffArgs = [
            '-re',
            '-i', inputUrl,
            '-f', 'lavfi', '-i', 'anullsrc=channel_layout=stereo:sample_rate=44100',
            '-map', '0:v:0', '-map', '1:a:0',
            '-c:v', 'copy',
            '-c:a', 'aac', '-b:a', '128k',
            '-flvflags', 'no_duration_filesize',
            '-rtmp_live', 'live',
            '-f', 'flv',
            outputUrl
        ];

        const ffmpegCmd = process.env.FFMPEG_PATH || 'ffmpeg';
        let child;
        try {
            child = spawn(ffmpegCmd, ffArgs, { stdio: ['ignore', 'pipe', 'pipe'], env: process.env });
        } catch (err) {
            return res.status(500).json({ error: 'Failed to spawn ffmpeg', detail: String(err) });
        }

        // persist job row
        const job = db.createJob({
            id: undefined,
            pipelineId: pid,
            outputId: oid,
            pid: child.pid || null,
            status: 'running',
            startedAt: new Date().toISOString()
        });

        // keep only process ref in-memory
        processes.set(job.id, child);

        const pushLog = (msg) => {
            db.appendJobLog(job.id, msg);
        };

        child.on('error', (err) => {
            db.appendJobLog(job.id, `[error] ${String(err)}`);
            // mark failed
            db.updateJob(job.id, { status: 'failed', endedAt: new Date().toISOString(), exitCode: null, exitSignal: null });
            processes.delete(job.id);
        });

        if (child.stdout) child.stdout.on('data', (d) => {
            const s = d.toString();
            pushLog(`[stdout] ${s}`);
        });
        if (child.stderr) child.stderr.on('data', (d) => {
            const s = d.toString();
            pushLog(`[stderr] ${s}`);
        });

        child.on('exit', (code, signal) => {
            const st = (code === 0) ? 'stopped' : 'failed';
            db.updateJob(job.id, { status: st, endedAt: new Date().toISOString(), exitCode: code, exitSignal: signal || null });
            pushLog(`[exit] code=${code} signal=${signal}`);
            processes.delete(job.id);
        });

        // short delay to detect immediate exit/err
        await new Promise(r => setTimeout(r, 250));
        const fresh = db.getJob(job.id);
        if (fresh.status !== 'running') {
            // return logs if failed immediately
            const logs = db.listJobLogs(job.id).map(r => `${r.ts} ${r.message}`).slice(-100);
            return res.status(500).json({ error: 'ffmpeg failed to start', job: fresh, logs });
        }

        return res.status(201).json({ message: 'Job started', job });
    } catch (err) {
        return res.status(500).json({ error: String(err) });
    }
});

// stop output (kill ffmpeg)
app.post('/pipelines/:pipelineId/outputs/:outputId/stop', (req, res) => {
    try {
        const pid = req.params.pipelineId;
        const oid = req.params.outputId;

        const running = db.getRunningJobFor(pid, oid);
        if (!running) return res.status(404).json({ error: 'No running job for this output' });

        const jobId = running.id;
        const proc = processes.get(jobId);

        if (proc && !proc.killed) {
            try { proc.kill('SIGTERM'); } catch (e) { /* ignore */ }

            const killTimeout = setTimeout(() => {
                try { if (!proc.killed) proc.kill('SIGKILL'); } catch (e) { /* ignore */ }
            }, 5000);

            proc.once('exit', () => clearTimeout(killTimeout));
            db.appendJobLog(jobId, '[control] requested SIGTERM');
            return res.json({ message: 'Stopping job', jobId });
        } else {
            // process not in memory — mark job as stopped in DB (best-effort)
            db.updateJob(jobId, { status: 'stopped', endedAt: new Date().toISOString(), exitCode: null, exitSignal: null });
            db.appendJobLog(jobId, '[control] stop requested but process not found in memory; marked stopped');
            return res.json({ message: 'Job marked stopped (process not found)', jobId });
        }
    } catch (err) {
        return res.status(500).json({ error: String(err) });
    }
});


/* ======================
 * Metrics
 * ====================== */

app.get('/inputs', async (req, res) => {
    try {
        const resp = await fetch('http://localhost:9997/v3/paths/list');
        const data = await resp.json();
        res.json(data.items);
    } catch (err) {
        res.status(500).json({ error: err.toString() });
    }
});

// to expose the MediaMTX API content of this endpoint to FE:
// GET /v3/rtmpconns/list
// we simply return everything as-is from MediaMTX.

app.get('/metrics/mediamtx/v3/rtmpconns/list', async (req, res) => {
    try {
        const resp = await fetch('http://localhost:9997/v3/rtmpconns/list');
        const data = await resp.json();
        res.json(data);
    } catch (err) {
        res.status(500).json({ error: err.toString() });
    }
});





/* ======================
 * Static UI & Server
 * ====================== */

app.use('/', express.static(path.join(__dirname, 'ui')));

app.listen(3030, () => console.log('Controller running on 3030'));

// todo: add an etag, for the FE to check the last modified time of the entire config.



