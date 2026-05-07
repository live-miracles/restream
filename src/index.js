// Composition root. Creates long-lived singletons (DB, config, processes, health monitor),
// wires the shared callbacks between services, mounts all Express routes, and starts the server.
// No business logic lives here — delegate to the relevant service or route module.
const express = require('express');
const compression = require('compression');
const path = require('path');
const fetch = global.fetch || require('node-fetch'); // keep compatibility
const crypto = require('crypto');
const { spawn } = require('child_process');
const db = require('./db');
const { getConfig, toPublicConfig } = require('./config');
const { registerConfigApi, registerPipelineApi } = require('./routes-pipeline');
const { registerOutputApi } = require('./routes-output');
const { registerPreviewProxyRoutes } = require('./preview');
const { createHealthMonitorService, registerSystemMetricsApi } = require('./health');
const { createOutputLifecycleService } = require('./outputs');
const { createPipelineRuntimeStateService } = require('./pipeline-runtime-state');
const { startServer, createRuntimeRegistries } = require('./bootstrap');
const { log, buildMediamtxPath, getMediamtxHlsBaseUrl } = require('./utils');

const REVALIDATE_STATIC_CACHE_CONTROL = 'public, max-age=0, must-revalidate';

const app = express();
app.use(express.json());
app.use(
    compression({
        threshold: 1024,
        brotli: { enabled: true },
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

const appPort = Number(process.env.PORT || 3030);
const appHost = getConfig().host;

// These Maps mirror live child-process state and are intentionally not persisted.
const { ffmpegProgressByJobId, ffmpegOutputMediaByJobId, processes } =
    createRuntimeRegistries();
const pipelineRuntimeState = createPipelineRuntimeStateService({ db });

// ── Config API (provides normalizeEtag + recomputeEtag) ──────────
const { normalizeEtag, recomputeConfigEtag, recomputeEtag } = registerConfigApi({
    app,
    db,
    getConfig,
    toPublicConfig,
});

// ── Health monitor ────────────────────────────────────
const healthMonitor = createHealthMonitorService({
    db,
    fetch,
    createHash: crypto.createHash.bind(crypto),
    normalizeEtag,
    ffmpegProgressByJobId,
    ffmpegOutputMediaByJobId,
    pipelineRuntimeState,
    spawn,
});

// ── Output lifecycle (FFmpeg process management) ──────
const outputLifecycle = createOutputLifecycleService({
    db,
    getConfig,
    spawn,
    processes,
    ffmpegProgressByJobId,
    ffmpegOutputMediaByJobId,
    recomputeEtag,
    isLatestJobLikelyInputUnavailableStop:
        pipelineRuntimeState.isLatestJobLikelyInputUnavailableStop,
});

// Shared pipeline runtime state owns the recovery callback seam so health and output lifecycle do
// not need to reference each other directly.
pipelineRuntimeState.setInputRecoveryHandler(
    outputLifecycle.restartPipelineOutputsOnInputRecovery,
);

const {
    clearOutputRestartState,
    getOutputDesiredState,
    reconcileOutput,
    resetOutputFailureCount,
    setOutputDesiredState,
    shutdown: shutdownOutputs,
    stopRunningJobAndWait,
    stopRunningJob,
} = outputLifecycle;

// ── API routes ────────────────────────────────────────
registerPipelineApi({
    app,
    db,
    getConfig,
    fetch,
    crypto,
    pipelineRuntimeState,
    resetOutputFailureCount,
    clearOutputRestartState,
    stopRunningJobAndWait,
    stopRunningJob,
    recomputeConfigEtag,
    recomputeEtag,
});

registerOutputApi({
    app,
    db,
    getConfig,
    recomputeConfigEtag,
    recomputeEtag,
    clearOutputRestartState,
    getOutputDesiredState,
    reconcileOutput,
    resetOutputFailureCount,
    setOutputDesiredState,
    stopRunningJobAndWait,
    stopRunningJob,
});

healthMonitor.registerRoutes(app);
registerSystemMetricsApi({ app });
registerPreviewProxyRoutes({
    app,
    fetch,
    log,
    getMediamtxHlsBaseUrl,
    buildMediamtxPath,
});

const hlsVendorDir = path.join(__dirname, '..', 'node_modules', 'hls.js', 'dist');
app.use(
    '/vendor',
    express.static(hlsVendorDir, {
        maxAge: '1h',
        etag: true,
        lastModified: true,
        setHeaders: (res, filePath) => {
            if (path.extname(filePath).toLowerCase() === '.js') {
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

            if (ext === '.html') {
                res.setHeader('Cache-Control', 'no-store');
                return;
            }

            if (ext === '.js' || ext === '.css') {
                res.setHeader('Cache-Control', REVALIDATE_STATIC_CACHE_CONTROL);
            }
        },
    }),
);

startServer({
    app,
    healthMonitor,
    db,
    log,
    appPort,
    appHost,
    onShutdown: shutdownOutputs,
}).catch((err) => {
    console.error('Fatal startup error:', err);
    process.exit(1);
});
