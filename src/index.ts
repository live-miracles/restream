import express from 'express';
import compression from 'compression';
import path from 'path';
import { spawn } from 'child_process';
import type { ChildProcess } from 'child_process';
import * as db from './db';
import { mkdirSync } from 'fs';
import { registerConfigApi } from './api/config';
import { registerGrafanaProxyRoutes } from './api/grafana';
import { registerPreviewProxyRoutes } from './api/preview';
import { registerOutputApi } from './api/outputs';
import { registerEncodingsApi } from './api/encodings';
import { registerPipelineApi } from './api/pipelines';
import { registerRecordingApi } from './api/recording';
import { registerIngestApi } from './api/ingest';
import { registerSecurityApi } from './api/security';
import { registerDiagnosticsApi } from './api/diagnostics';
import { createIngestService } from './services/ingest';
import { createHealthMonitorService } from './services/health';
import { createOutputLifecycleService } from './services/outputs';
import { createRecordingService } from './services/recording';
import { createIngestSecurityService } from './services/security';
import { startServer } from './services/bootstrap';
import { registerSystemMetricsApi } from './api/metrics';
import { errMsg, log } from './utils/app';
import { buildMediamtxPath, getMediamtxHlsBaseUrl } from './utils/mediamtx';

const app = express();

// Register before body parsers so Grafana API requests can stream through unchanged.
registerGrafanaProxyRoutes({ app, log });

const REVALIDATE_STATIC_CACHE_CONTROL = 'public, max-age=0, must-revalidate';
app.use(express.json());
app.use(
    compression({
        threshold: 1024,
        brotli: { enabled: true } as Record<string, unknown>,
        filter: (req, res) => {
            if (req.headers['x-no-compression']) return false;
            const contentType = res.getHeader('Content-Type');
            if (typeof contentType === 'string' && contentType.includes('text/event-stream')) {
                return false;
            }
            return compression.filter(req, res);
        },
    }),
);

const appPort = 3030;
const mediaDir = path.join(__dirname, '..', 'media');
mkdirSync(mediaDir, { recursive: true });

// Runtime-only progress state from ffmpeg "-progress pipe:3" (never persisted to DB).
const ffmpegProgressByJobId = new Map<string, Record<string, string>>();

// ── Shared child-process handle registry ─────────────
const processes = new Map<string, ChildProcess>();

// ── Config API ────────────────────────────────────────
registerConfigApi({ app, db });

// ── Health monitor ────────────────────────────────────
const healthMonitor = createHealthMonitorService({
    db,
    fetch,
    ffmpegProgressByJobId,
});

// ── Output lifecycle (FFmpeg process management) ──────
const outputLifecycle = createOutputLifecycleService({
    db,
    spawn,
    processes,
    ffmpegProgressByJobId,
    isInputOn: healthMonitor.isInputOn,
    getInputPullProtocol: healthMonitor.getInputPullProtocol,
});

// ── Ingest service ────────────────────────────────────
const ingestService = createIngestService({ db, mediaDir });

// ── Recording service ─────────────────────────────────
const recordingService = createRecordingService({
    db,
    mediaDir,
    isInputOn: healthMonitor.isInputOn,
    getInputPullProtocol: healthMonitor.getInputPullProtocol,
});

// Resolve circular dependency: register the output recovery callback now that both services exist.
healthMonitor.registerInputRecoveryHandler((pipelineId) => {
    outputLifecycle.restartPipelineOutputsOnInputRecovery(pipelineId);
    recordingService.onInputRecovered(pipelineId);
});
healthMonitor.registerInputLostHandler((pipelineId) => {
    recordingService.onInputLost(pipelineId);
});
healthMonitor.registerRecordingStateProvider((pipelineId) => recordingService.getState(pipelineId));

const {
    clearOutputRestartState,
    getOutputDesiredState,
    reconcileOutput,
    resetOutputFailureCount,
    setOutputDesiredState,
    stopRunningJobAndWait,
    stopRunningJob,
} = outputLifecycle;

// ── API routes ────────────────────────────────────────
registerPipelineApi({
    app,
    db,
    healthMonitor,
    resetOutputFailureCount,
    clearOutputRestartState,
    stopRunningJobAndWait,
    stopRunningJob,
});

registerOutputApi({
    app,
    db,
    clearOutputRestartState,
    getOutputDesiredState,
    reconcileOutput,
    resetOutputFailureCount,
    setOutputDesiredState,
    stopRunningJobAndWait,
    stopRunningJob,
});

healthMonitor.registerRoutes(app);
registerEncodingsApi({ app, db });
registerSystemMetricsApi({ app });
registerRecordingApi({ app, db, recording: recordingService, mediaDir });
registerIngestApi({ app, db, ingestService });
registerDiagnosticsApi({ app, db });
const ingestSecurityService = createIngestSecurityService({
    getConfig: db.getIngestSecurityConfig,
    log,
});
registerSecurityApi({
    app,
    ingestSecurity: ingestSecurityService,
    log,
});
registerPreviewProxyRoutes({
    app,
    fetch,
    log,
    getMediamtxHlsBaseUrl,
    buildMediamtxPath,
});

// ── Static media files ────────────────────────────────
app.use('/media', express.static(mediaDir, { maxAge: 0, etag: true }));

// ── Static UI ─────────────────────────────────────────
const hlsVendorDir = path.join(__dirname, '..', 'node_modules', 'hls.js', 'dist');
app.use(
    '/vendor',
    express.static(hlsVendorDir, {
        maxAge: '1h',
        etag: true,
        lastModified: true,
        setHeaders: (res, filePath) => {
            const ext = path.extname(filePath).toLowerCase();
            if (ext === '.js') {
                res.setHeader('Cache-Control', REVALIDATE_STATIC_CACHE_CONTROL);
            }
        },
    }),
);

const publicDir = path.join(__dirname, '..', 'public');
app.use(
    '/',
    express.static(publicDir, {
        maxAge: '1h',
        etag: true,
        lastModified: true,
        setHeaders: (res, filePath) => {
            const ext = path.extname(filePath).toLowerCase();

            // Prevent HTML document caching so clients always fetch the latest module graph.
            if (ext === '.html') {
                res.setHeader('Cache-Control', 'no-store');
                return;
            }

            // Revalidate JS/CSS on reload while still allowing browser/proxy caching.
            if (ext === '.js' || ext === '.css') {
                res.setHeader('Cache-Control', REVALIDATE_STATIC_CACHE_CONTROL);
            }
        },
    }),
);

async function main(): Promise<void> {
    try {
        await ingestSecurityService.refreshStreamKeys();
    } catch (err) {
        log('error', 'ingest_security_stream_key_prewarm_failed', { error: errMsg(err) });
    }

    await startServer({
        app,
        healthMonitor,
        db,
        log,
        appPort,
        afterHealthStart: () => recordingService.init(),
    });
}

main().catch((err) => {
    console.error('Fatal startup error:', err);
    process.exit(1);
});
