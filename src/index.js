/* top requires */
const express = require('express');
const compression = require('compression');
const fetch = global.fetch || require('node-fetch'); // keep compatibility
const path = require('path');
const crypto = require('crypto');
const { spawn } = require('child_process');
const db = require('./db');
const { getConfig, toPublicConfig } = require('./config');
const { registerConfigApi } = require('./api/config');
const { registerOutputApi } = require('./api/outputs');
const { registerPipelineApi } = require('./api/pipelines');
const { createHealthMonitorService } = require('./services/health');
const { createOutputLifecycleService } = require('./services/outputs');
const { startServer } = require('./services/bootstrap');
const { registerSystemMetricsApi } = require('./api/metrics');
const { log } = require('./utils/app');

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

// Runtime-only progress state from ffmpeg "-progress pipe:3" (never persisted to DB).
const ffmpegProgressByJobId = new Map(); // jobId -> latest ffmpeg progress block
// Parsed output media info from FFmpeg stderr "Output #0" section.
const ffmpegOutputMediaByJobId = new Map(); // jobId -> { video: {...}, audio: {...} }

// ── Shared child-process handle registry ─────────────
const processes = new Map(); // jobId -> ChildProcess

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
    isLatestJobLikelyInputUnavailableStop: healthMonitor.isLatestJobLikelyInputUnavailableStop,
});

// Resolve circular dependency without late-binding let-variable workaround:
// register the output recovery callback now that both services are created.
healthMonitor.registerInputRecoveryHandler(
    outputLifecycle.restartPipelineOutputsOnInputRecovery,
);

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
    getConfig,
    fetch,
    crypto,
    healthMonitor,
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

// ── Static UI ─────────────────────────────────────────
const publicDir = path.join(__dirname, '..', 'public');
const experienceRoutes = new Map([
    ['/', { mobile: '/mobile/dashboard.html', desktop: '/' }],
    ['/index.html', { mobile: '/mobile/dashboard.html', desktop: '/' }],
    ['/stream-keys.html', { mobile: '/mobile/keys.html', desktop: '/stream-keys.html' }],
    ['/mobile', { mobile: '/mobile/dashboard.html', desktop: '/' }],
    ['/mobile/', { mobile: '/mobile/dashboard.html', desktop: '/' }],
    ['/mobile/dashboard.html', { mobile: '/mobile/dashboard.html', desktop: '/' }],
    ['/mobile/keys.html', { mobile: '/mobile/keys.html', desktop: '/stream-keys.html' }],
    ['/mobile-dashboard.html', { mobile: '/mobile/dashboard.html', desktop: '/' }],
    ['/mobile-keys.html', { mobile: '/mobile/keys.html', desktop: '/stream-keys.html' }],
]);
const mobileUserAgentPattern = /Android|webOS|iPhone|iPad|iPod|BlackBerry|IEMobile|Opera Mini/i;

function getRequestedExperience(req) {
    const view = String(req.query.view || '').toLowerCase();
    return view === 'mobile' || view === 'desktop' ? view : null;
}

function isLikelyMobileRequest(req) {
    const uaMobile = req.get('sec-ch-ua-mobile');
    if (uaMobile === '?1') return true;
    if (uaMobile === '?0') return false;
    return mobileUserAgentPattern.test(req.get('user-agent') || '');
}

app.use((req, res, next) => {
    const variants = experienceRoutes.get(req.path);
    if (!variants) {
        next();
        return;
    }

    const requestedExperience = getRequestedExperience(req);
    const preferredExperience = requestedExperience || (isLikelyMobileRequest(req) ? 'mobile' : 'desktop');
    const targetPath = variants[preferredExperience];
    if (!targetPath || targetPath === req.path) {
        next();
        return;
    }

    const query = new URLSearchParams(req.query);
    const suffix = query.size > 0 ? `?${query.toString()}` : '';
    res.redirect(302, `${targetPath}${suffix}`);
});

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

            // Force revalidation for module-bearing assets to avoid stale mixed-version loads.
            if (ext === '.js' || ext === '.css') {
                res.setHeader('Cache-Control', 'public, max-age=0, must-revalidate');
            }
        },
    }),
);

startServer({ app, healthMonitor, db, log, appPort, appHost }).catch((err) => {
    console.error('Fatal startup error:', err);
    process.exit(1);
});
