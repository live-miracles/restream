const SUPPORTED_OUTPUT_ENCODINGS = new Set([
    'source',
    'vertical-crop',
    'vertical-rotate',
    '720p',
    '1080p',
]);

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

function normalizeOutputEncoding(value) {
    const normalized = String(value ?? 'source')
        .trim()
        .toLowerCase();
    if (!normalized) return 'source';
    if (normalized === 'vertical') return 'vertical-crop';
    if (!SUPPORTED_OUTPUT_ENCODINGS.has(normalized)) return null;
    return normalized;
}

function buildFfmpegOutputArgs({ inputUrl, outputUrl, encoding = 'source' }) {
    const normalizedEncoding = normalizeOutputEncoding(encoding) || 'source';
    const args = [
        '-nostdin',
        '-hide_banner',
        '-loglevel',
        'info',
        '-nostats',
        '-stats_period',
        '1',
        '-progress',
        'pipe:3',
        '-rtsp_transport',
        'tcp',
        '-i',
        inputUrl,
    ];

    if (normalizedEncoding === 'source') {
        args.push('-c:v', 'copy', '-c:a', 'copy');
    } else {
        const profileByEncoding = {
            'vertical-crop': {
                vf: 'scale=720:1280:force_original_aspect_ratio=increase,crop=720:1280',
                videoBitrate: '2500k',
                maxrate: '2800k',
                bufsize: '4200k',
            },
            'vertical-rotate': {
                vf: 'transpose=1,scale=720:1280:force_original_aspect_ratio=increase,crop=720:1280',
                videoBitrate: '2500k',
                maxrate: '2800k',
                bufsize: '4200k',
            },
            '720p': {
                vf: 'scale=-2:720',
                videoBitrate: '3000k',
                maxrate: '3500k',
                bufsize: '5000k',
            },
            '1080p': {
                vf: 'scale=-2:1080',
                videoBitrate: '5000k',
                maxrate: '5800k',
                bufsize: '8000k',
            },
        };

        const profile = profileByEncoding[normalizedEncoding] || profileByEncoding['720p'];
        args.push(
            '-vf',
            profile.vf,
            '-c:v',
            'libx264',
            '-preset',
            'veryfast',
            '-pix_fmt',
            'yuv420p',
            '-profile:v',
            'high',
            '-level:v',
            '4.1',
            '-g',
            '60',
            '-keyint_min',
            '60',
            '-sc_threshold',
            '0',
            '-b:v',
            profile.videoBitrate,
            '-maxrate',
            profile.maxrate,
            '-bufsize',
            profile.bufsize,
            '-c:a',
            'aac',
            '-b:a',
            '128k',
            '-ar',
            '48000',
            '-ac',
            '2',
        );
    }

    args.push('-flvflags', 'no_duration_filesize', '-rtmp_live', 'live', '-f', 'flv', outputUrl);
    return args;
}

function createHttpError(status, error, detail, extra = {}) {
    const err = new Error(error);
    err.status = status;
    err.publicError = error;
    err.detail = detail;
    Object.assign(err, extra);
    return err;
}

const HISTORY_MESSAGE_PREFIXES = {
    lifecycle: '[lifecycle]',
    stderr: '[stderr]',
    exit: '[exit]',
    control: '[control]',
    config: '[config]',
    input_state: '[input_state]',
};
const HISTORY_MAX_LIMIT = 1000;
const HISTORY_MAX_RANGE_MS = 24 * 60 * 60 * 1000;
const HISTORY_MAX_HIGH_VOLUME_RANGE_MS = 10 * 60 * 1000;
const HISTORY_HIGH_VOLUME_PREFIXES = new Set(['[stderr]', '[exit]', '[control]']);

function parseHistoryTimestamp(value) {
    if (value === undefined || value === null || value === '') return null;
    const parsed = new Date(String(value));
    if (Number.isNaN(parsed.getTime())) return undefined;
    return parsed.toISOString();
}

function parseHistoryOrder(value, defaultValue = 'desc') {
    if (value === undefined || value === null || value === '') return defaultValue;
    const normalized = String(value).trim().toLowerCase();
    if (normalized === 'asc' || normalized === 'desc') return normalized;
    return null;
}

function parseHistoryLimit(value, defaultValue = 200) {
    if (value === undefined || value === null || value === '') return defaultValue;
    const parsed = Number.parseInt(String(value), 10);
    if (!Number.isFinite(parsed)) return null;
    return Math.max(1, Math.min(parsed, HISTORY_MAX_LIMIT));
}

function parseHistoryPrefixes(value) {
    if (value === undefined || value === null || value === '') return [];

    const rawValues = Array.isArray(value) ? value : [value];
    const tokens = rawValues
        .flatMap((entry) => String(entry).split(','))
        .map((entry) => entry.trim().toLowerCase())
        .filter(Boolean);

    const prefixes = [];
    for (const token of tokens) {
        const mappedPrefix = HISTORY_MESSAGE_PREFIXES[token];
        if (!mappedPrefix) return null;
        if (!prefixes.includes(mappedPrefix)) prefixes.push(mappedPrefix);
    }

    return prefixes;
}

function registerOutputApi({
    app,
    db,
    getConfig,
    errMsg,
    recomputeConfigEtag,
    recomputeEtag,
    clearOutputRestartState,
    getOutputDesiredState,
    reconcileOutput,
    resetOutputFailureCount,
    setOutputDesiredState,
    stopRunningJob,
    validateName,
}) {
    async function applyOutputStateChange(pid, oid, options) {
        const {
            desiredState,
            stateReason,
            resetReason,
            trigger,
            reconcileReason,
        } = options;

        const desiredStateChange = setOutputDesiredState(pid, oid, desiredState, {
            source: 'api',
            reason: stateReason,
        });
        recomputeConfigEtag();

        resetOutputFailureCount(pid, oid, resetReason);

        const reconciliation = await reconcileOutput(pid, oid, {
            trigger,
            reason: reconcileReason,
            source: 'api',
        });
        recomputeEtag();

        return { desiredStateChange, reconciliation };
    }

    app.post('/pipelines/:pipelineId/outputs', (req, res) => {
        try {
            const pid = req.params.pipelineId;
            const pipeline = db.getPipeline(pid);
            if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });

            const runtimeConfig = getConfig();
            const outLimit = Number(runtimeConfig.outLimit);
            const currentOutCount = db.listOutputsForPipeline(pid).length;
            if (Number.isFinite(outLimit) && currentOutCount >= outLimit) {
                return res
                    .status(400)
                    .json({ error: `Output limit reached for pipeline: ${outLimit}` });
            }

            const name = req.body?.name;
            const url = req.body?.url;
            const encoding = normalizeOutputEncoding(req.body?.encoding ?? 'source');
            const nameError = validateName(name, 'Output name');

            if (nameError) {
                return res.status(400).json({ error: nameError });
            }

            if (!encoding) {
                return res.status(400).json({
                    error: 'Encoding must be one of: source, vertical-crop, vertical-rotate, 720p, 1080p',
                });
            }

            if (!validateOutputUrl(url)) {
                return res
                    .status(400)
                    .json({ error: 'Output URL must be a valid rtmp:// or rtmps:// URL' });
            }

            const output = db.createOutput({ pipelineId: pid, name, url, encoding });
            recomputeConfigEtag();
            recomputeEtag();
            return res.status(201).json({ message: 'Output created', output });
        } catch (err) {
            return res.status(400).json({ error: err.message || errMsg(err) });
        }
    });

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
            const existingEncoding = normalizeOutputEncoding(existing.encoding) || 'source';
            const encoding =
                req.body?.encoding === undefined
                    ? existingEncoding
                    : normalizeOutputEncoding(req.body?.encoding);
            const nameError = validateName(name, 'Output name');
            const running = db.getRunningJobFor(pid, oid);
            const urlChanged = url !== existing.url;
            const encodingChanged = encoding !== existingEncoding;

            if (nameError) {
                return res.status(400).json({ error: nameError });
            }

            if (!encoding) {
                return res.status(400).json({
                    error: 'Encoding must be one of: source, vertical-crop, vertical-rotate, 720p, 1080p',
                });
            }

            if (running && (urlChanged || encodingChanged)) {
                return res.status(409).json({
                    error: 'Cannot change output URL or encoding while output is running. Stop output first.',
                });
            }

            if (!validateOutputUrl(url)) {
                return res
                    .status(400)
                    .json({ error: 'Output URL must be a valid rtmp:// or rtmps:// URL' });
            }

            const updated = db.updateOutput(pid, oid, { name, url, encoding });
            if (!updated) return res.status(500).json({ error: 'Failed to update output' });

            recomputeConfigEtag();
            recomputeEtag();
            return res.json({ message: 'Output updated', output: updated });
        } catch (err) {
            return res.status(400).json({ error: err.message || errMsg(err) });
        }
    });

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

            clearOutputRestartState(pid, oid);
            recomputeConfigEtag();
            recomputeEtag();
            return res.json({ message: `Output ${oid} from pipeline ${pid} deleted` });
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });

    app.post('/pipelines/:pipelineId/outputs/:outputId/start', async (req, res) => {
        const pid = req.params.pipelineId;
        const oid = req.params.outputId;

        try {
            const pipeline = db.getPipeline(pid);
            if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });

            const output = db.getOutput(pid, oid);
            if (!output) return res.status(404).json({ error: 'Output not found' });

            const { reconciliation } = await applyOutputStateChange(pid, oid, {
                desiredState: 'running',
                stateReason: 'manual_start',
                resetReason: 'manual_start',
                trigger: 'manual',
                reconcileReason: 'manual_request',
            });

            if (reconciliation.action === 'started') {
                return res.status(201).json({
                    message: 'Output started',
                    desiredState: 'running',
                    job: reconciliation.job,
                });
            }

            if (reconciliation.action === 'already_running') {
                return res.status(200).json({
                    message: 'Output already running',
                    desiredState: 'running',
                    job: reconciliation.job,
                });
            }

            if (reconciliation.action === 'waiting_for_input') {
                return res.status(409).json({
                    error: 'Pipeline input is not available yet',
                    message: 'Output desired state set to running; waiting for input',
                    desiredState: 'running',
                    detail: reconciliation.detail,
                });
            }

            if (reconciliation.action === 'start_in_progress') {
                return res.status(409).json({ error: 'Start already in progress for this output' });
            }

            return res
                .status(200)
                .json({ message: 'Output desired state set to running', desiredState: 'running' });
        } catch (err) {
            const status = Number(err?.status || 500);
            const payload = { error: err?.publicError || errMsg(err) };
            if (err?.detail) payload.detail = err.detail;
            if (err?.job) payload.job = err.job;
            if (err?.logs) payload.logs = err.logs;
            return res.status(status).json(payload);
        }
    });

    app.post('/pipelines/:pipelineId/outputs/:outputId/stop', async (req, res) => {
        try {
            const pid = req.params.pipelineId;
            const oid = req.params.outputId;

            const pipeline = db.getPipeline(pid);
            if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });

            const output = db.getOutput(pid, oid);
            if (!output) return res.status(404).json({ error: 'Output not found' });

            const { desiredStateChange, reconciliation } = await applyOutputStateChange(pid, oid, {
                desiredState: 'stopped',
                stateReason: 'manual_stop',
                resetReason: 'manual_stop',
                trigger: 'manual-stop',
                reconcileReason: 'desired_stopped',
            });

            if (reconciliation.action === 'stop_requested') {
                return res.json({
                    message: 'Output desired state set to stopped',
                    desiredState: 'stopped',
                    previousState: desiredStateChange?.previousState || 'running',
                    jobId: reconciliation.job?.id || null,
                    result: reconciliation.result,
                });
            }

            return res.json({
                message: 'Output desired state set to stopped',
                desiredState: 'stopped',
                previousState: desiredStateChange?.previousState || getOutputDesiredState(output),
                jobId: null,
                result: { stopped: false, reason: 'already_stopped' },
            });
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });

    app.get('/pipelines/:pipelineId/outputs/:outputId/history', (req, res) => {
        try {
            const pid = req.params.pipelineId;
            const oid = req.params.outputId;

            const pipeline = db.getPipeline(pid);
            if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });

            const output = db.getOutput(pid, oid);
            if (!output) return res.status(404).json({ error: 'Output not found' });

            const filterLifecycle = req.query.filter === 'lifecycle';
            const since = parseHistoryTimestamp(req.query.since);
            if (since === undefined)
                return res.status(400).json({ error: 'Invalid since timestamp' });

            const until = parseHistoryTimestamp(req.query.until);
            if (until === undefined)
                return res.status(400).json({ error: 'Invalid until timestamp' });
            if (since && until && since >= until) {
                return res.status(400).json({ error: 'since must be earlier than until' });
            }

            const order = parseHistoryOrder(req.query.order, filterLifecycle ? 'asc' : 'desc');
            if (!order) return res.status(400).json({ error: 'order must be asc or desc' });

            const prefixes = filterLifecycle
                ? ['[lifecycle]']
                : parseHistoryPrefixes(req.query.prefix);
            if (prefixes === null) {
                return res.status(400).json({
                    error: 'prefix must be a comma-separated list of lifecycle, stderr, exit, control, config, input_state',
                });
            }

            const sinceMs = since ? Date.parse(since) : null;
            const untilMs = until ? Date.parse(until) : null;
            const rangeMs = sinceMs !== null && untilMs !== null ? untilMs - sinceMs : null;
            if (rangeMs !== null) {
                if (rangeMs > HISTORY_MAX_RANGE_MS) {
                    return res.status(400).json({ error: 'Requested history window is too large' });
                }
                const requestsHighVolumePrefixes = prefixes.some((prefix) =>
                    HISTORY_HIGH_VOLUME_PREFIXES.has(prefix),
                );
                if (requestsHighVolumePrefixes && rangeMs > HISTORY_MAX_HIGH_VOLUME_RANGE_MS) {
                    return res.status(400).json({
                        error: 'Requested stderr/exit/control history window is too large',
                    });
                }
            }

            let logs;
            if (filterLifecycle) {
                const requestedLimit = parseHistoryLimit(req.query.limit, null);
                if (requestedLimit === null && req.query.limit !== undefined) {
                    return res
                        .status(400)
                        .json({ error: 'limit must be an integer between 1 and 1000' });
                }
                logs = db.listJobLogsByOutputFiltered(pid, oid, {
                    since,
                    until,
                    limit: requestedLimit,
                    order,
                    prefixes,
                });
            } else {
                const limit = parseHistoryLimit(req.query.limit, 200);
                if (limit === null) {
                    return res
                        .status(400)
                        .json({ error: 'limit must be an integer between 1 and 1000' });
                }
                logs = db.listJobLogsByOutputFiltered(pid, oid, {
                    since,
                    until,
                    limit,
                    order,
                    prefixes,
                });
            }

            return res.json({
                pipelineId: pid,
                outputId: oid,
                logs,
            });
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });

    app.get('/pipelines/:pipelineId/history', (req, res) => {
        try {
            const pid = req.params.pipelineId;
            const pipeline = db.getPipeline(pid);
            if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });

            const requestedLimit = Number.parseInt(String(req.query.limit || '200'), 10);
            const limit = Number.isFinite(requestedLimit)
                ? Math.max(1, Math.min(requestedLimit, 1000))
                : 200;

            const logs = db.listJobLogsByPipeline(pid).slice(0, limit);
            return res.json({ pipelineId: pid, logs });
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });
}

module.exports = {
    buildFfmpegOutputArgs,
    createHttpError,
    normalizeOutputEncoding,
    registerOutputApi,
    validateOutputUrl,
};
