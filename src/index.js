/* top requires */
const express = require('express');
const fetch = global.fetch || require('node-fetch'); // keep compatibility
const db = require('./db');
const { getConfig } = require('./config');
const fs = require('fs');
const os = require('os');

const app = express();
app.use(express.json());

const { spawn } = require('child_process');
const path = require('path');
const crypto = require('crypto');
const { createHash } = require('crypto');

const processes = new Map(); // runtime only: jobId -> ChildProcess
const ffmpegCmd = process.env.FFMPEG_PATH || 'ffmpeg';
const ffprobeCmd = process.env.FFPROBE_PATH || 'ffprobe';
const appPort = Number(process.env.PORT || 3030);
const appHost = getConfig().host;
const logLevel = (process.env.LOG_LEVEL || 'info').toLowerCase();
const probeCacheTtlMs = Number(process.env.PROBE_CACHE_TTL_MS || 30000);
const streamProbeCache = new Map(); // streamKey -> { ts, info }
// Runtime-only progress state from ffmpeg "-progress pipe:3" (never persisted to DB).
// NOTE: This is intentionally internal for now; a future API/WS endpoint can expose it.
const ffmpegProgressByJobId = new Map(); // jobId -> latest ffmpeg progress block

let systemMetricsSample = {
    ts: Date.now(),
    cpu: getCpuTotals(),
    net: getNetworkTotals(),
};

const levelOrder = { error: 0, warn: 1, info: 2, debug: 3 };
const mediamtxReadiness = {
    ready: false,
    checkedAt: null,
    readyAt: null,
    error: null,
};
let mediamtxReadinessTimer = null;

function shouldLog(level) {
    const current = levelOrder[logLevel] ?? levelOrder.info;
    const target = levelOrder[level] ?? levelOrder.info;
    return target <= current;
}

function log(level, message, fields = {}) {
    if (!shouldLog(level)) return;
    const payload = {
        ts: new Date().toISOString(),
        level,
        message,
        ...fields,
    };
    // Keep logs single-line JSON to simplify grep and diff across runs.
    console.log(JSON.stringify(payload));
}

function shellQuote(arg) {
    const s = String(arg ?? '');
    if (/^[A-Za-z0-9_./:-]+$/.test(s)) return s;
    return `'${s.replace(/'/g, `'\\''`)}'`;
}

function buildCommandPreview(cmd, args) {
    return [cmd, ...(args || []).map(shellQuote)].join(' ');
}

function maskToken(value) {
    const s = String(value ?? '');
    if (!s) return '';
    if (s.length <= 4) {
        if (s.length === 1) return s;
        return `${s[0]}...${s[s.length - 1]}`;
    }
    return `${s.slice(0, 2)}...${s.slice(-2)}`;
}

function redactSensitiveUrl(rawUrl) {
    if (!rawUrl || typeof rawUrl !== 'string') return rawUrl;

    let parsed;
    try {
        parsed = new URL(rawUrl);
    } catch {
        return maskToken(rawUrl);
    }

    if (parsed.username) parsed.username = '[REDACTED]';
    if (parsed.password) parsed.password = '[REDACTED]';

    const sensitiveParams = /key|streamkey|stream_key|token|secret|pass|passphrase|signature|sig|auth|streamid/i;
    for (const [paramKey] of parsed.searchParams.entries()) {
        if (sensitiveParams.test(paramKey)) {
            parsed.searchParams.set(paramKey, '[REDACTED]');
        }
    }

    const protocol = String(parsed.protocol || '').toLowerCase();
    if (['rtmp:', 'rtmps:', 'rtsp:', 'rtsps:', 'srt:'].includes(protocol)) {
        const segments = parsed.pathname.split('/');
        const lastIdx = segments.length - 1;
        if (lastIdx >= 1 && segments[lastIdx]) {
            segments[lastIdx] = maskToken(segments[lastIdx]);
            parsed.pathname = segments.join('/');
        }
    }

    parsed.hash = '';
    return parsed.toString();
}

function redactFfmpegArgs(args) {
    return (args || []).map((arg) => {
        const s = String(arg ?? '');
        return s.includes('://') ? redactSensitiveUrl(s) : s;
    });
}

function getMediamtxApiBaseUrl() {
    // MediaMTX internal API is always available on localhost:9997
    return 'http://localhost:9997';
}

async function checkMediamtxReadiness() {
    const checkedAt = new Date().toISOString();
    const wasReady = mediamtxReadiness.ready;
    const previousError = mediamtxReadiness.error;
    try {
        const response = await fetch(`${getMediamtxApiBaseUrl()}/v3/config/global/get`, {
            signal: AbortSignal.timeout(5000),
        });

        if (!response.ok) {
            throw new Error(`HTTP ${response.status}`);
        }

        mediamtxReadiness.ready = true;
        mediamtxReadiness.checkedAt = checkedAt;
        mediamtxReadiness.readyAt = mediamtxReadiness.readyAt || checkedAt;
        mediamtxReadiness.error = null;
        if (!wasReady) {
            log('info', 'MediaMTX readiness check recovered', {
                checkedAt,
                readyAt: mediamtxReadiness.readyAt,
            });
        }
    } catch (err) {
        const errorMessage = String(err);
        mediamtxReadiness.ready = false;
        mediamtxReadiness.checkedAt = checkedAt;
        mediamtxReadiness.error = errorMessage;
        if (wasReady || previousError !== errorMessage) {
            log('warn', 'MediaMTX readiness check failed', {
                checkedAt,
                error: errorMessage,
            });
        }
    }
}

function startMediamtxReadinessChecks() {
    void checkMediamtxReadiness();
    if (mediamtxReadinessTimer) return;
    mediamtxReadinessTimer = setInterval(() => {
        void checkMediamtxReadiness();
    }, 5000);
    mediamtxReadinessTimer.unref?.();
}

function getMediamtxRtspBaseUrl() {
    // MediaMTX RTSP input is always available on localhost:8554
    return 'rtsp://localhost:8554';
}

function getCpuTotals() {
    const totals = os.cpus().reduce(
        (acc, cpu) => {
            const times = cpu.times || {};
            const total =
                Number(times.user || 0) +
                Number(times.nice || 0) +
                Number(times.sys || 0) +
                Number(times.idle || 0) +
                Number(times.irq || 0);
            acc.total += total;
            acc.idle += Number(times.idle || 0);
            return acc;
        },
        { total: 0, idle: 0 },
    );
    return totals;
}

function getNetworkTotals() {
    try {
        const content = fs.readFileSync('/proc/net/dev', 'utf8');
        const lines = content.split('\n').slice(2).filter(Boolean);
        let rx = 0;
        let tx = 0;

        for (const line of lines) {
            const [ifaceRaw, rest] = line.split(':');
            if (!ifaceRaw || !rest) continue;
            const iface = ifaceRaw.trim();
            if (!iface || iface === 'lo') continue;

            const fields = rest.trim().split(/\s+/);
            if (fields.length < 16) continue;

            rx += Number(fields[0] || 0);
            tx += Number(fields[8] || 0);
        }

        return { rx, tx };
    } catch (err) {
        return { rx: 0, tx: 0 };
    }
}

function getDiskUsage(pathname = '/') {
    try {
        const stats = fs.statfsSync(pathname);
        const blockSize = Number(stats.bsize || 0);
        const totalBlocks = Number(stats.blocks || 0);
        const availBlocks = Number(stats.bavail || stats.bfree || 0);

        const totalBytes = blockSize * totalBlocks;
        const freeBytes = blockSize * availBlocks;
        const usedBytes = Math.max(0, totalBytes - freeBytes);
        const usedPercent = totalBytes > 0 ? (usedBytes / totalBytes) * 100 : null;

        return { totalBytes, usedBytes, freeBytes, usedPercent };
    } catch (err) {
        return {
            totalBytes: null,
            usedBytes: null,
            freeBytes: null,
            usedPercent: null,
        };
    }
}

function parseFrameRate(rateValue) {
    if (!rateValue || typeof rateValue !== 'string') return null;
    if (rateValue.includes('/')) {
        const [numRaw, denRaw] = rateValue.split('/');
        const num = Number(numRaw);
        const den = Number(denRaw);
        if (Number.isFinite(num) && Number.isFinite(den) && den !== 0) {
            return Number((num / den).toFixed(2));
        }
    }
    const asNumber = Number(rateValue);
    return Number.isFinite(asNumber) ? asNumber : null;
}

function parseFfmpegBitrateToKbps(rateValue) {
    if (rateValue === null || rateValue === undefined) return null;
    const raw = String(rateValue).trim();
    if (!raw || raw.toUpperCase() === 'N/A') return null;

    const match = raw.match(/^([0-9]+(?:\.[0-9]+)?)\s*([kKmMgG]?)\s*(?:bits\/s)?$/);
    if (!match) return null;

    const value = Number(match[1]);
    if (!Number.isFinite(value) || value < 0) return null;

    const unit = (match[2] || '').toLowerCase();
    let bps = value;
    if (unit === 'k') bps = value * 1000;
    else if (unit === 'm') bps = value * 1000 * 1000;
    else if (unit === 'g') bps = value * 1000 * 1000 * 1000;

    return Number((bps / 1000).toFixed(1));
}

function extractProbeMediaInfo(stdout) {
    if (!stdout) return null;
    let parsed = null;
    try {
        parsed = JSON.parse(stdout);
    } catch (err) {
        return null;
    }

    const streams = Array.isArray(parsed?.streams) ? parsed.streams : [];
    const video = streams.find((stream) => stream?.codec_type === 'video') || null;
    const audio = streams.find((stream) => stream?.codec_type === 'audio') || null;

    return {
        video: video
            ? {
                  fps: parseFrameRate(video.avg_frame_rate) || parseFrameRate(video.r_frame_rate),
              }
            : null,
        audio: audio
            ? {
                  codec: audio.codec_name || null,
                  channels: audio.channels || null,
                  sampleRate: audio.sample_rate ? Number(audio.sample_rate) : null,
                  profile: audio.profile || null,
              }
            : null,
    };
}

async function getCachedRtspProbeInfo(streamKey, inputUrl) {
    if (!streamKey || !inputUrl) return null;
    const now = Date.now();
    const cached = streamProbeCache.get(streamKey);
    if (cached && now - cached.ts < probeCacheTtlMs) return cached.info;

    const probe = await probeRtspInput(inputUrl);
    if (!probe.ok || !probe.info) {
        if (cached) return cached.info;
        return null;
    }

    streamProbeCache.set(streamKey, { ts: now, info: probe.info });
    return probe.info;
}

function getPipelineRtspUrl(streamKey) {
    return `${getMediamtxRtspBaseUrl()}/${streamKey}`;
}

function generateReaderTag(pipelineId, outputId) {
    return `reader_${pipelineId}_${outputId}`.replace(/[^a-zA-Z0-9_-]/g, '_');
}

function getPipelineTaggedRtspUrl(streamKey, pipelineId, outputId) {
    const readerTag = generateReaderTag(pipelineId, outputId);
    return `${getMediamtxRtspBaseUrl()}/${streamKey}?reader_id=${encodeURIComponent(readerTag)}`;
}

function getExpectedReaderTag(pipelineId, outputId) {
    return generateReaderTag(pipelineId, outputId);
}

function getReaderIdFromQuery(query) {
    if (!query || typeof query !== 'string') return null;
    const normalized = query.startsWith('?') ? query.slice(1) : query;
    if (!normalized) return null;
    try {
        const params = new URLSearchParams(normalized);
        const readerId = params.get('reader_id');
        return readerId || null;
    } catch (err) {
        return null;
    }
}

async function fetchMediamtxJson(endpoint) {
    const url = `${getMediamtxApiBaseUrl()}${endpoint}`;
    const resp = await fetch(url, {
        signal: AbortSignal.timeout(5000),
    });
    let data = null;
    try {
        data = await resp.json();
    } catch (err) {
        throw new Error(`Invalid JSON from MediaMTX endpoint ${endpoint}: ${String(err)}`);
    }
    if (!resp.ok) {
        throw new Error(`MediaMTX ${endpoint} failed with status ${resp.status}`);
    }
    return data;
}

function stopRunningJob(job, signal = 'SIGTERM') {
    if (!job) return { stopped: false, reason: 'missing-job' };

    const proc = processes.get(job.id);
    if (proc && !proc.killed) {
        try {
            proc.kill(signal);
            db.appendJobLog(job.id, `[control] requested ${signal}`, job.pipelineId, job.outputId);
            return { stopped: true, reason: 'signal-sent' };
        } catch (err) {
            db.appendJobLog(job.id, `[control] failed to send ${signal}: ${String(err)}`, job.pipelineId, job.outputId);
            return { stopped: false, reason: 'signal-failed' };
        }
    }

    db.updateJob(job.id, {
        status: 'stopped',
        endedAt: new Date().toISOString(),
        exitCode: null,
        exitSignal: null,
    });
    db.appendJobLog(job.id, '[control] process not found in memory; marked stopped', job.pipelineId, job.outputId);
    return { stopped: true, reason: 'marked-stopped' };
}

async function probeRtspInput(inputUrl) {
    return new Promise((resolve) => {
        const args = [
            '-v',
            'error',
            '-rtsp_transport',
            'tcp',
            '-show_entries',
            'stream=codec_type,codec_name,profile,avg_frame_rate,r_frame_rate,channels,sample_rate',
            '-of',
            'json',
            inputUrl,
        ];

        let stderr = '';
        let stdout = '';
        let settled = false;
        let child;

        try {
            child = spawn(ffprobeCmd, args, {
                stdio: ['ignore', 'pipe', 'pipe'],
                env: process.env,
            });
        } catch (err) {
            resolve({ ok: false, error: `Failed to spawn ffprobe: ${String(err)}` });
            return;
        }

        const timeout = setTimeout(() => {
            if (settled) return;
            settled = true;
            try {
                child.kill('SIGKILL');
            } catch (e) {
                /* ignore */
            }
            resolve({ ok: false, error: 'Timed out waiting for RTSP input to become readable' });
        }, 8000);

        child.stdout.on('data', (chunk) => {
            stdout += chunk.toString();
        });
        child.stderr.on('data', (chunk) => {
            stderr += chunk.toString();
        });
        child.on('error', (err) => {
            if (settled) return;
            settled = true;
            clearTimeout(timeout);
            resolve({ ok: false, error: String(err) });
        });
        child.on('exit', (code) => {
            if (settled) return;
            settled = true;
            clearTimeout(timeout);
            if (code === 0) {
                resolve({ ok: true, stdout, info: extractProbeMediaInfo(stdout) });
                return;
            }
            resolve({ ok: false, error: stderr || `ffprobe exited with ${code}` });
        });
    });
}

/* ======================
 * Stream Key APIs
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
        const url = `${getMediamtxApiBaseUrl()}/v3/config/paths/add/${encodeURIComponent(key)}`;
        const resp = await fetch(url, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ name: key }),
        });

        let data = null;
        try {
            data = await resp.json();
        } catch (e) {
            /* ignore parse errors */
        }

        if (!resp.ok || data?.error) {
            return res.status(500).json({
                error: data?.error || `MediaMTX returned ${resp.status}`,
            });
        }

        const sk = db.createStreamKey({ key, label, createdAt: new Date().toISOString() });
        recomputeConfigEtag();
        recomputeEtag();
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
        recomputeConfigEtag();
        recomputeEtag();
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

        const url = `${getMediamtxApiBaseUrl()}/v3/config/paths/delete/${encodeURIComponent(key)}`;

        const resp = await fetch(url, {
            method: 'DELETE',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ name: key }),
        });

        let data = null;
        try {
            data = await resp.json();
        } catch (e) {
            /* ignore parse errors */
        }

        if (!resp.ok || data?.error) {
            return res.status(500).json({
                error: data?.error || `MediaMTX returned ${resp.status}`,
            });
        }

        const deleted = db.deleteStreamKey(key);
        if (!deleted) {
            return res.status(500).json({ error: 'Failed to remove stream key from DB' });
        }

        recomputeConfigEtag();
        recomputeEtag();
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
        const runtimeConfig = getConfig();
        const pipelineLimit = Number(runtimeConfig.pipelinesLimit);
        if (Number.isFinite(pipelineLimit) && db.listPipelines().length >= pipelineLimit) {
            return res.status(400).json({ error: `Pipeline limit reached: ${pipelineLimit}` });
        }

        const name = req.body?.name;
        const streamKey = req.body?.streamKey ?? null;
        const encoding = req.body?.encoding ?? null;

        const pipeline = db.createPipeline({ name, streamKey, encoding });
        // recompute global etag if available
        recomputeConfigEtag();
        recomputeEtag();
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
        const encoding = req.body?.encoding ?? existing.encoding;

        const updated = db.updatePipeline(id, { name, streamKey, encoding });
        if (!updated) return res.status(500).json({ error: 'Failed to update pipeline' });

        recomputeConfigEtag();
        recomputeEtag();
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

        const outputs = db.listOutputs().filter((output) => output.pipelineId === id);
        for (const output of outputs) {
            const running = db.getRunningJobFor(id, output.id);
            if (running) stopRunningJob(running);
        }

        const ok = db.deletePipeline(id);
        if (!ok) return res.status(500).json({ error: 'Failed to delete pipeline' });

        recomputeConfigEtag();
        recomputeEtag();
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

function validateOutputUrl(url) {
    if (!url || typeof url !== 'string') return false;
    let parsed;
    try {
        parsed = new URL(url);
    } catch {
        return false;
    }
    return parsed.protocol === 'rtmp:' || parsed.protocol === 'rtmps:';
}

// create output
app.post('/pipelines/:pipelineId/outputs', (req, res) => {
    try {
        const pid = req.params.pipelineId;
        const pipeline = db.getPipeline(pid);
        if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });

        const runtimeConfig = getConfig();
        const outLimit = Number(runtimeConfig.outLimit);
        const currentOutCount = db.listOutputs().filter((output) => output.pipelineId === pid).length;
        if (Number.isFinite(outLimit) && currentOutCount >= outLimit) {
            return res.status(400).json({ error: `Output limit reached for pipeline: ${outLimit}` });
        }

        const name = req.body?.name;
        const url = req.body?.url;
        const encoding = req.body?.encoding ?? 'source';

        if (!validateOutputUrl(url)) {
            return res.status(400).json({ error: 'Output URL must be a valid rtmp:// or rtmps:// URL' });
        }

        const output = db.createOutput({ pipelineId: pid, name, url, encoding });
        recomputeConfigEtag();
        recomputeEtag();

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

        const name = req.body?.name ?? existing.name;
        const url = req.body?.url ?? existing.url;
        const encoding = req.body?.encoding ?? existing.encoding ?? 'source';

        if (!validateOutputUrl(url)) {
            return res.status(400).json({ error: 'Output URL must be a valid rtmp:// or rtmps:// URL' });
        }

        const updated = db.updateOutput(pid, oid, { name, url, encoding });
        if (!updated) return res.status(500).json({ error: 'Failed to update output' });

        recomputeConfigEtag();
        recomputeEtag();
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

        const running = db.getRunningJobFor(pid, oid);
        if (running) stopRunningJob(running);

        const ok = db.deleteOutput(pid, oid);
        if (!ok) return res.status(500).json({ error: 'Failed to delete output' });

        recomputeConfigEtag();
        recomputeEtag();
        return res.json({ message: `Output ${oid} from pipeline ${pid} deleted` });
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
        if (existingRunning)
            return res
                .status(409)
                .json({ error: 'Output already has a running job', job: existingRunning });

        if (!pipeline.streamKey)
            return res.status(400).json({ error: 'Pipeline has no stream key assigned' });

        const probeInputUrl = getPipelineRtspUrl(pipeline.streamKey);

        const probe = await probeRtspInput(probeInputUrl);
        if (!probe.ok) {
            return res.status(409).json({
                error: 'Pipeline input is not available yet',
                detail: probe.error,
                inputUrl: probeInputUrl,
            });
        }
        if (probe.info) {
            streamProbeCache.set(pipeline.streamKey, { ts: Date.now(), info: probe.info });
        }

        const inputUrl = getPipelineTaggedRtspUrl(pipeline.streamKey, pid, oid);
        const expectedReaderTag = getExpectedReaderTag(pid, oid);

        const outputUrl = output.url;
        if (!outputUrl) return res.status(400).json({ error: 'Output URL is empty' });

        const ffArgs = [
            '-nostdin',
            '-hide_banner',
            '-loglevel',
            'info',
            // Disable legacy stderr progress lines; progress is emitted as key=value on fd3.
            '-nostats',
            '-stats_period',
            '1',
            '-progress',
            'pipe:3',
            '-rtsp_transport',
            'tcp',
            '-i',
            inputUrl,
            '-c:v',
            'copy',
            '-c:a',
            'copy',
            '-flvflags',
            'no_duration_filesize',
            '-rtmp_live',
            'live',
            '-f',
            'flv',
            outputUrl,
        ];

        const redactedFfArgs = redactFfmpegArgs(ffArgs);
        log('debug', 'Crafted ffmpeg output command', {
            pipelineId: pid,
            outputId: oid,
            probeInputUrl: redactSensitiveUrl(probeInputUrl),
            inputUrl: redactSensitiveUrl(inputUrl),
            expectedReaderTag,
            outputUrl: redactSensitiveUrl(outputUrl),
            ffmpegCmd,
            ffmpegArgs: redactedFfArgs,
            ffmpegCommandPreview: buildCommandPreview(ffmpegCmd, redactedFfArgs),
        });

        let child;
        try {
            child = spawn(ffmpegCmd, ffArgs, {
                // fd3 is dedicated ffmpeg progress output (pipe:3), stderr remains persistent logs.
                stdio: ['ignore', 'ignore', 'pipe', 'pipe'],
                env: process.env,
            });
        } catch (err) {
            return res.status(500).json({ error: 'Failed to spawn ffmpeg', detail: String(err) });
        }

        log('info', 'Spawned ffmpeg output process', {
            pipelineId: pid,
            outputId: oid,
            childPid: child.pid || null,
        });

        // persist job row
        const job = db.createJob({
            id: undefined,
            pipelineId: pid,
            outputId: oid,
            pid: child.pid || null,
            status: 'running',
            startedAt: new Date().toISOString(),
        });
        recomputeEtag();

        // keep only process ref in-memory
        processes.set(job.id, child);
        ffmpegProgressByJobId.set(job.id, {});

        const pushLog = (msg) => {
            db.appendJobLog(job.id, msg, pid, oid);
        };

        child.on('error', (err) => {
            db.appendJobLog(job.id, `[error] ${String(err)}`, pid, oid);
            log('error', 'ffmpeg child process error', {
                pipelineId: pid,
                outputId: oid,
                jobId: job.id,
                childPid: child.pid || null,
                error: String(err),
            });
            // mark failed
            db.updateJob(job.id, {
                status: 'failed',
                endedAt: new Date().toISOString(),
                exitCode: null,
                exitSignal: null,
            });
            recomputeEtag();
            processes.delete(job.id);
            ffmpegProgressByJobId.delete(job.id);
        });

        const progressStream = child.stdio[3];
        let progressBuffer = '';
        if (progressStream)
            progressStream.on('data', (d) => {
                progressBuffer += d.toString();
                // A data chunk may end mid-line, so keep the trailing fragment for next chunk.
                const lines = progressBuffer.split('\n');
                progressBuffer = lines.pop() || '';

                const latest = ffmpegProgressByJobId.get(job.id) || {};
                for (const rawLine of lines) {
                    const line = rawLine.trim();
                    if (!line) continue;
                    const idx = line.indexOf('=');
                    if (idx <= 0) continue;
                    const key = line.slice(0, idx).trim();
                    const value = line.slice(idx + 1).trim();
                    latest[key] = value;
                }
                ffmpegProgressByJobId.set(job.id, latest);
            });

        // Persist stderr/error/exit for diagnostics; skip progress stream to avoid DB bloat.
        if (child.stderr)
            child.stderr.on('data', (d) => {
                const s = d.toString();
                pushLog(`[stderr] ${s}`);
            });

        child.on('exit', (code, signal) => {
            const st = code === 0 ? 'stopped' : 'failed';
            log('info', 'ffmpeg child process exit', {
                pipelineId: pid,
                outputId: oid,
                jobId: job.id,
                childPid: child.pid || null,
                code,
                signal: signal || null,
                finalStatus: st,
            });
            db.updateJob(job.id, {
                status: st,
                endedAt: new Date().toISOString(),
                exitCode: code,
                exitSignal: signal || null,
            });
            pushLog(`[exit] code=${code} signal=${signal}`);
            recomputeEtag();
            processes.delete(job.id);
            ffmpegProgressByJobId.delete(job.id);
        });

        // short delay to detect immediate exit/err
        await new Promise((r) => setTimeout(r, 250));
        const fresh = db.getJob(job.id);
        if (fresh.status !== 'running') {
            // return logs if failed immediately
            const logs = db
                .listJobLogs(job.id)
                .map((r) => `${r.ts} ${r.message}`)
                .slice(-100);
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
        const result = stopRunningJob(running);
        if (proc && !proc.killed) {
            const killTimeout = setTimeout(() => {
                try {
                    if (!proc.killed) proc.kill('SIGKILL');
                } catch (e) {
                    /* ignore */
                }
            }, 5000);
            proc.once('exit', () => clearTimeout(killTimeout));
        }
        recomputeEtag();
        return res.json({ message: 'Stopping job', jobId, result });
    } catch (err) {
        return res.status(500).json({ error: String(err) });
    }
});

/* ======================
 * Metrics
 * ====================== */

app.get('/metrics/system', (req, res) => {
    try {
        const now = Date.now();
        const dtSec = Math.max((now - systemMetricsSample.ts) / 1000, 0.001);

        const currentCpu = getCpuTotals();
        const currentNet = getNetworkTotals();
        const memTotal = os.totalmem();
        const memFree = os.freemem();
        const memUsed = Math.max(0, memTotal - memFree);
        const memUsedPercent = memTotal > 0 ? (memUsed / memTotal) * 100 : null;
        const disk = getDiskUsage('/');

        const cpuTotalDiff = currentCpu.total - systemMetricsSample.cpu.total;
        const cpuIdleDiff = currentCpu.idle - systemMetricsSample.cpu.idle;
        let cpuUsagePercent = 0;
        if (cpuTotalDiff > 0) {
            cpuUsagePercent = Math.max(0, Math.min(100, ((cpuTotalDiff - cpuIdleDiff) / cpuTotalDiff) * 100));
        }

        const rxDiff = Math.max(0, currentNet.rx - systemMetricsSample.net.rx);
        const txDiff = Math.max(0, currentNet.tx - systemMetricsSample.net.tx);
        const downloadBytesPerSec = rxDiff / dtSec;
        const uploadBytesPerSec = txDiff / dtSec;

        systemMetricsSample = {
            ts: now,
            cpu: currentCpu,
            net: currentNet,
        };

        return res.json({
            generatedAt: new Date(now).toISOString(),
            cpu: {
                usagePercent: Number(cpuUsagePercent.toFixed(2)),
                cores: os.cpus().length,
                load1: Number(os.loadavg()[0].toFixed(2)),
            },
            memory: {
                totalBytes: memTotal,
                usedBytes: memUsed,
                freeBytes: memFree,
                usedPercent: memUsedPercent !== null ? Number(memUsedPercent.toFixed(2)) : null,
            },
            disk,
            network: {
                downloadBytesPerSec: Number(downloadBytesPerSec.toFixed(2)),
                uploadBytesPerSec: Number(uploadBytesPerSec.toFixed(2)),
                downloadKbps: Number(((downloadBytesPerSec * 8) / 1000).toFixed(2)),
                uploadKbps: Number(((uploadBytesPerSec * 8) / 1000).toFixed(2)),
            },
        });
    } catch (err) {
        return res.status(500).json({ error: String(err) });
    }
});

app.get('/health', async (req, res) => {
    if (!mediamtxReadiness.ready) {
        return res.json({
            generatedAt: new Date().toISOString(),
            status: 'degraded',
            pipelines: {},
        });
    }

    try {
        const [paths, rtspConns, rtspSessions] = await Promise.all([
            fetchMediamtxJson('/v3/paths/list'),
            fetchMediamtxJson('/v3/rtspconns/list'),
            fetchMediamtxJson('/v3/rtspsessions/list'),
        ]);

        log('debug', 'Fetched MediaMTX health sources', {
            pathCount: paths.itemCount || 0,
            rtspConnCount: rtspConns.itemCount || 0,
            rtspSessionCount: rtspSessions.itemCount || 0,
            rtspConnSummaries: (rtspConns.items || []).slice(0, 20).map((conn) => ({
                id: conn?.id || null,
                state: conn?.state || null,
                path: conn?.path || null,
                useragent: conn?.useragent || null,
                userAgent: conn?.userAgent || null,
                remoteAddr: conn?.remoteAddr || null,
                bytesReceived: conn?.bytesReceived || 0,
                bytesSent: conn?.bytesSent || 0,
            })),
        });

        const pathByName = new Map((paths.items || []).map((item) => [item.name, item]));
        const rtspSessionById = new Map((rtspSessions.items || []).map((s) => [s.id, s]));
        const rtspConnectionRecords = (rtspConns.items || []).map((conn) => {
            const session = conn?.session ? rtspSessionById.get(conn.session) : null;

            return {
                id: conn?.id || null,
                sessionId: conn?.session || session?.id || null,
                path: conn?.path || session?.path || null,
                query: conn?.query || session?.query || null,
                remoteAddr: conn?.remoteAddr || session?.remoteAddr || null,
                bytesReceived: conn?.bytesReceived || session?.bytesReceived || 0,
                bytesSent: conn?.bytesSent || session?.bytesSent || 0,
            };
        });

        const rtspByReaderTag = new Map();
        for (const conn of rtspConnectionRecords) {
            const readerTag = getReaderIdFromQuery(conn.query);
            if (!readerTag) continue;
            const list = rtspByReaderTag.get(readerTag) || [];
            list.push(conn);
            rtspByReaderTag.set(readerTag, list);
        }

        if ((rtspConns.items || []).length > 0 && rtspByReaderTag.size === 0) {
            log('warn', 'MediaMTX RTSP payload has no reader_id query for active readers', {
                rtspConnCount: rtspConns.itemCount || 0,
                rtspSessionCount: rtspSessions.itemCount || 0,
                sampleRtspConnKeys: Object.keys((rtspConns.items || [])[0] || {}),
                sampleRtspSessionKeys: Object.keys((rtspSessions.items || [])[0] || {}),
            });
        }

        const pipelines = db.listPipelines();
        const outputs = db.listOutputs();
        const jobs = db.listJobs();

        // With upsert, each output has exactly 1 job row, so no reduction needed
        const jobByOutputId = new Map();
        for (const job of jobs) {
            jobByOutputId.set(job.outputId, job);
        }

        const health = { pipelines: {} };

        for (const pipeline of pipelines) {
            const key = pipeline.streamKey || '';
            const pathInfo = key ? pathByName.get(key) : null;
            const readers = pathInfo?.readers || [];
            // MediaMTX marks `ready` as deprecated; prefer `available` and fall back to `ready` for older versions.
            // `available` (stream != nil): stream is ready and readable by consumers — the signal we care about.
            // `online` (source != nil): a publisher is attached but stream may not be initialised yet.
            const pathAvailable = !!(pathInfo?.available || pathInfo?.ready);
            const pathOnline = !!pathInfo?.online;
            let inputStatus = 'off';
            if (key && pathAvailable) inputStatus = 'on';
            else if (key && pathOnline) inputStatus = 'warning'; // publisher connecting, stream not yet ready

            const probeInfo =
                key && pathAvailable ? await getCachedRtspProbeInfo(key, getPipelineRtspUrl(key)) : null;

            const firstVideoTrack = (pathInfo?.tracks2 || []).find((track) =>
                String(track.codec || '').toLowerCase().includes('264'),
            );
            const firstAudioTrack = (pathInfo?.tracks2 || []).find((track) => {
                const codec = String(track.codec || '').toLowerCase();
                if (!codec) return false;
                return !codec.includes('264') && !codec.includes('265') && !codec.includes('vp8') && !codec.includes('vp9') && !codec.includes('av1');
            });

            const pipelineHealth = {
                input: {
                    status: inputStatus,
                    publishStartedAt: pathInfo?.availableTime || pathInfo?.readyTime || null,
                    streamKey: key || null,
                    readers: readers.length,
                    bytesReceived: pathInfo?.bytesReceived || 0,
                    bytesSent: pathInfo?.bytesSent || 0,
                    video: firstVideoTrack
                        ? {
                              codec: firstVideoTrack.codec || null,
                              width: firstVideoTrack.codecProps?.width || null,
                              height: firstVideoTrack.codecProps?.height || null,
                              profile: firstVideoTrack.codecProps?.profile || null,
                              level: firstVideoTrack.codecProps?.level || null,
                              fps: probeInfo?.video?.fps || null,
                              bw: null,
                          }
                        : null,
                    audio: firstAudioTrack || probeInfo?.audio
                        ? {
                              codec: probeInfo?.audio?.codec || firstAudioTrack?.codec || null,
                              channels:
                                  probeInfo?.audio?.channels ||
                                  firstAudioTrack?.codecProps?.channelCount ||
                                  null,
                              sample_rate: probeInfo?.audio?.sampleRate || firstAudioTrack?.codecProps?.sampleRate || null,
                              profile: probeInfo?.audio?.profile || firstAudioTrack?.codecProps?.profile || null,
                              bw: null,
                          }
                        : null,
                },
                outputs: {},
            };

            const pipelineOutputs = outputs.filter((output) => output.pipelineId === pipeline.id);

            for (const output of pipelineOutputs) {
                const latest = jobByOutputId.get(output.id) || null;
                let readerConn = null;
                let status = 'off';
                const ffmpegProgress = latest?.id ? ffmpegProgressByJobId.get(latest.id) || null : null;

                if (latest?.status === 'failed') status = 'error';
                if (latest?.status === 'running') {
                    const expectedReaderTag = getExpectedReaderTag(pipeline.id, output.id);
                    const matches = rtspByReaderTag.get(expectedReaderTag) || [];
                    readerConn = matches[0] || null;
                    status = readerConn ? 'on' : 'warning';

                    log('debug', 'Output health match result', {
                        pipelineId: pipeline.id,
                        outputId: output.id,
                        jobId: latest?.id || null,
                        jobPid: Number.isFinite(Number(latest.pid)) ? Number(latest.pid) : null,
                        jobStatus: latest?.status || null,
                        expectedReaderTag,
                        hasReaderTagMatch: !!readerConn,
                        matchedReaderCount: matches.length,
                        knownReaderTagCount: rtspByReaderTag.size,
                        finalStatus: status,
                    });
                }

                pipelineHealth.outputs[output.id] = {
                    status,
                    jobId: latest?.id || null,
                    totalSize: ffmpegProgress?.total_size || null,
                    bitrate: ffmpegProgress?.bitrate || null,
                    bitrateKbps: parseFfmpegBitrateToKbps(ffmpegProgress?.bitrate),
                };
            }

            health.pipelines[pipeline.id] = pipelineHealth;
        }

        return res.json({
            generatedAt: new Date().toISOString(),
            status: 'ready',
            mediamtx: {
                pathCount: paths.itemCount || 0,
                rtspConnCount: rtspConns.itemCount || 0,
                ready: mediamtxReadiness.ready,
            },
            ...health,
        });
    } catch (err) {
        log('error', 'Failed to build health response', {
            error: String(err),
        });
        return res.json({
            generatedAt: new Date().toISOString(),
            status: 'degraded',
            pipelines: {},
        });
    }
});

app.get('/healthz', (req, res) => {
    if (!mediamtxReadiness.ready) {
        return res.status(503).json({ status: 'not_ready' });
    }
    return res.json({ status: 'ok' });
});

/* ======================
 * Static UI & Server
 * ====================== */

app.use('/', express.static(path.join(__dirname, '..', 'public')));

app.listen(appPort, appHost, () => {
    startMediamtxReadinessChecks();
    console.log(`Controller running on ${appHost}:${appPort}`);
    
    // Start periodic cleanup of old job logs (7-day retention)
    setInterval(() => {
        try {
            db.deleteJobLogsOlderThan(7);
        } catch (err) {
            console.error('Error cleaning up old job logs:', err);
        }
    }, 60 * 60 * 1000); // Run every hour
});

// Etag-related, for the FE to check the last modified time of the entire config.

// normalize quoted etag helper
function normalizeEtag(s) {
    if (!s) return null;
    return s.replace(/^"(.*)"$/, '$1');
}

function buildConfigSnapshot() {
    const streamKeys = db
        .listStreamKeys()
        .map((sk) => ({ key: sk.key, label: sk.label, createdAt: sk.createdAt }));
    const pipelines = db.listPipelines().map((p) => ({
        id: p.id,
        name: p.name,
        streamKey: p.streamKey,
        encoding: p.encoding,
        createdAt: p.createdAt,
        updatedAt: p.updatedAt,
    }));

    const outputsByPipeline = db.listOutputs().reduce((acc, output) => {
        const pipelineId = output.pipelineId;
        if (!acc[pipelineId]) acc[pipelineId] = [];
        acc[pipelineId].push(output);
        return acc;
    }, {});

    for (const pipeline of pipelines) {
        const outs = (outputsByPipeline[pipeline.id] || []).map((output) => ({
            id: output.id,
            name: output.name,
            url: output.url,
            encoding: output.encoding,
            createdAt: output.createdAt,
        }));
        outs.sort((a, b) => a.id.localeCompare(b.id));
        pipeline.outputs = outs;
    }

    streamKeys.sort((a, b) => (a.key || '').localeCompare(b.key || ''));
    pipelines.sort((a, b) => (a.id || '').localeCompare(b.id || ''));

    return { streamKeys, pipelines };
}

function buildJobsSnapshot() {
    const jobs = db.listJobs().map((job) => ({
        id: job.id,
        pipelineId: job.pipelineId,
        outputId: job.outputId,
        status: job.status,
        startedAt: job.startedAt,
        endedAt: job.endedAt,
        exitCode: job.exitCode,
        exitSignal: job.exitSignal,
    }));

    jobs.sort((a, b) => (b.startedAt || '').localeCompare(a.startedAt || ''));
    return jobs;
}

function hashSnapshot(snapshot) {
    return createHash('sha256').update(JSON.stringify(snapshot)).digest('hex');
}

function recomputeConfigEtag() {
    try {
        const etag = hashSnapshot(buildConfigSnapshot());
        db.setConfigEtag(etag);
        return etag;
    } catch (err) {
        console.error('recomputeConfigEtag error:', err);
        return null;
    }
}

// recomputeEtag: deterministic snapshot -> sha256 hex -> persist via db.setEtag
async function recomputeEtag() {
    try {
        const etag = hashSnapshot({
            ...buildConfigSnapshot(),
            jobs: buildJobsSnapshot(),
        });

        db.setEtag(etag);
        return etag;
    } catch (err) {
        console.error('recomputeEtag error:', err);
        return null;
    }
}

// Initialize etag at startup (best-effort)
(async () => {
    try {
        if (!db.getConfigEtag()) recomputeConfigEtag();
        if (!db.getEtag()) await recomputeEtag();
    } catch (e) {
        /* ignore */
    }
})();

// endpoint: GET /config  (returns full config + ETag, respect If-None-Match)
app.get('/config', async (req, res) => {
    try {
        // ensure etag is up-to-date
        let etag = db.getEtag();
        let configEtag = db.getConfigEtag();
        if (!configEtag) configEtag = recomputeConfigEtag();
        if (!etag) etag = await recomputeEtag();

        const ifNoneMatch = normalizeEtag(req.get('If-None-Match'));
        if (ifNoneMatch && etag && ifNoneMatch === etag) {
            // Not modified
            res.set('ETag', `"${etag}"`);
            if (configEtag) res.set('X-Config-ETag', `"${configEtag}"`);
            return res.status(304).end();
        }

        // build snapshot same as recomputeEtag logic
        const streamKeys = db.listStreamKeys();
        const pipelines = db.listPipelines();
        const outputs = db.listOutputs();
        const jobs = db.listJobs();
        const runtimeConfig = getConfig();

        const snapshot = {
            ...runtimeConfig,
            streamKeys,
            pipelines,
            outputs,
            jobs,
        };

        // send ETag header (quoted per spec)
        if (etag) res.set('ETag', `"${etag}"`);
        if (configEtag) res.set('X-Config-ETag', `"${configEtag}"`);
        return res.json(snapshot);
    } catch (err) {
        return res.status(500).json({ error: String(err) });
    }
});

app.head('/config/version', (req, res) => {
    try {
        let configEtag = db.getConfigEtag();
        if (!configEtag) configEtag = recomputeConfigEtag();

        const ifNoneMatch = normalizeEtag(req.get('If-None-Match'));
        if (ifNoneMatch && configEtag && ifNoneMatch === configEtag) {
            res.set('ETag', `"${configEtag}"`);
            return res.status(304).end();
        }

        if (configEtag) res.set('ETag', `"${configEtag}"`);
        return res.status(200).end();
    } catch (err) {
        return res.status(500).end();
    }
});

// optional: HEAD /config to check ETag only
app.head('/config', (req, res) => {
    try {
        const etag = db.getEtag();
        const configEtag = db.getConfigEtag();
        if (etag) res.set('ETag', `"${etag}"`);
        if (configEtag) res.set('X-Config-ETag', `"${configEtag}"`);
        return res.status(200).end();
    } catch (err) {
        return res.status(500).end();
    }
});
