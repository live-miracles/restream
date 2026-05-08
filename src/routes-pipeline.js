'use strict';

// Express routes for the config snapshot and pipeline/stream-key CRUD API.
// GET /config serves a point-in-time dashboard snapshot.
// Pipeline routes create, update, and delete pipelines and manage their stream-key assignments.
// Stream-key routes handle creation, label updates, and deletion.

const {
    errMsg,
    buildIngestUrls,
    maskToken,
    validateName,
    validateStreamKey,
    getMediamtxApiBaseUrl,
    buildMediamtxPath,
} = require('./utils');
const {
    buildConfigApiSnapshot,
    buildConfigSnapshot,
    buildJobsSnapshot,
    hashSnapshot,
} = require('./config');

const { respondEmpty, respondError, respondJson } = require('./http');

// Config API

const DASHBOARD_SSE_INTERVAL_MS = Number(process.env.DASHBOARD_SSE_INTERVAL_MS || 2000);

function registerConfigApi({
    app,
    db,
    getConfig,
    toPublicConfig,
    buildIngestUrlsImpl = buildIngestUrls,
    getHealthSnapshot = null,
    getSystemMetricsSnapshot = null,
}) {
    let healthSnapshotProvider = getHealthSnapshot;
    let systemMetricsProvider = getSystemMetricsSnapshot;

    function recomputeConfigSnapshotVersion() {
        try {
            const snapshotVersion = db.getSnapshotVersion() || recomputeSnapshotVersion();
            db.setConfigSnapshotVersion(snapshotVersion);
            return snapshotVersion;
        } catch (err) {
            console.error('recomputeConfigSnapshotVersion error:', err);
            return null;
        }
    }

    function recomputeSnapshotVersion() {
        try {
            // Full snapshot version includes current job rows because the dashboard uses /config as
            // its durable control-plane view, not just static configuration.
            const snapshotVersion = hashSnapshot({
                ...buildConfigSnapshot({ db }),
                jobs: buildJobsSnapshot({ db }),
            });

            db.setSnapshotVersion(snapshotVersion);
            return snapshotVersion;
        } catch (err) {
            console.error('recomputeSnapshotVersion error:', err);
            return null;
        }
    }

    // Seed both snapshot versions on startup so the first SSE connect can diff immediately.
    (async () => {
        try {
            if (!db.getSnapshotVersion()) recomputeSnapshotVersion();
            if (!db.getConfigSnapshotVersion()) recomputeConfigSnapshotVersion();
        } catch (e) {
            /* ignore */
        }
    })();

    function ensureSnapshotVersions() {
        let snapshotVersion = db.getSnapshotVersion();
        if (!snapshotVersion) snapshotVersion = recomputeSnapshotVersion();

        let configSnapshotVersion = db.getConfigSnapshotVersion();
        if (!configSnapshotVersion) {
            configSnapshotVersion = snapshotVersion;
            db.setConfigSnapshotVersion(configSnapshotVersion);
        }

        return { snapshotVersion, configSnapshotVersion };
    }

    async function buildConfigSnapshotPayload() {
        return buildConfigApiSnapshot({
            db,
            getConfig,
            toPublicConfig,
            buildIngestUrls: buildIngestUrlsImpl,
        });
    }

    async function buildDashboardConfigPayload() {
        const { configSnapshotVersion } = ensureSnapshotVersions();
        return {
            snapshotVersion: configSnapshotVersion || null,
            data: await buildConfigSnapshotPayload(),
        };
    }

    async function buildDashboardTelemetryPayload() {
        const healthSnapshot =
            typeof healthSnapshotProvider === 'function'
                ? await healthSnapshotProvider({ refreshIfStale: true })
                : null;
        const systemMetrics =
            typeof systemMetricsProvider === 'function'
                ? systemMetricsProvider()
                : null;

        const snapshotVersion = [
            healthSnapshot?.snapshotVersion || 'health-none',
            systemMetrics?.generatedAt || 'metrics-none',
        ].join(':');

        return {
            snapshotVersion,
            health: healthSnapshot,
            metrics: systemMetrics,
        };
    }

    app.get('/config', async (req, res) => {
        try {
            const snapshot = await buildConfigSnapshotPayload();
            return respondJson(res, snapshot);
        } catch (err) {
            return respondError(res, 500, errMsg(err));
        }
    });

    app.get('/dashboard/events', (req, res) => {
        res.setHeader('Content-Type', 'text/event-stream');
        res.setHeader('Cache-Control', 'no-cache');
        res.setHeader('Connection', 'keep-alive');
        res.setHeader('X-Accel-Buffering', 'no');
        res.flushHeaders?.();

        let lastSentConfigVersion = null;

        const writeConfigEvent = async ({ force = false } = {}) => {
            try {
                const { configSnapshotVersion } = ensureSnapshotVersions();
                const configVersion = configSnapshotVersion || null;
                if (!force && configVersion === lastSentConfigVersion) {
                    return;
                }

                const payload = await buildDashboardConfigPayload();
                const eventId = String(payload.snapshotVersion || Date.now());
                res.write(`id: config:${eventId}\n`);
                res.write('event: dashboard.config\n');
                res.write(`data: ${JSON.stringify(payload)}\n\n`);
                lastSentConfigVersion = payload.snapshotVersion || configVersion;
            } catch (err) {
                res.write('event: error\n');
                res.write(`data: ${JSON.stringify({ error: errMsg(err) })}\n\n`);
            }
        };

        const writeTelemetryEvent = async () => {
            try {
                const payload = await buildDashboardTelemetryPayload();
                const eventId = String(payload.snapshotVersion || Date.now());
                res.write(`id: telemetry:${eventId}\n`);
                res.write('event: dashboard.telemetry\n');
                res.write(`data: ${JSON.stringify(payload)}\n\n`);
            } catch (err) {
                res.write('event: error\n');
                res.write(`data: ${JSON.stringify({ error: errMsg(err) })}\n\n`);
            }
        };

        void writeConfigEvent({ force: true });
        void writeTelemetryEvent();

        const timer = setInterval(() => {
            void writeConfigEvent();
            void writeTelemetryEvent();
        }, DASHBOARD_SSE_INTERVAL_MS);
        timer.unref?.();

        req.on('close', () => {
            clearInterval(timer);
            res.end();
        });
    });

    return {
        recomputeConfigSnapshotVersion,
        recomputeSnapshotVersion,
        setHealthSnapshotProvider(provider) {
            healthSnapshotProvider = typeof provider === 'function' ? provider : null;
        },
        setSystemMetricsProvider(provider) {
            systemMetricsProvider = typeof provider === 'function' ? provider : null;
        },
    };
}

// Pipeline API

function logPipelineConfigChanges(db, pipelineId, previousPipeline, nextPipeline) {
    if (!pipelineId || !previousPipeline || !nextPipeline) return;

    if (previousPipeline.name !== nextPipeline.name) {
        db.appendPipelineEvent(
            pipelineId,
            `[config] name changed from "${previousPipeline.name}" to "${nextPipeline.name}"`,
            'pipeline.config.name_changed',
            {
                from: previousPipeline.name,
                to: nextPipeline.name,
            },
        );
    }

    if (previousPipeline.encoding !== nextPipeline.encoding) {
        db.appendPipelineEvent(
            pipelineId,
            `[config] encoding changed from ${previousPipeline.encoding || 'null'} to ${nextPipeline.encoding || 'null'}`,
            'pipeline.config.encoding_changed',
            {
                from: previousPipeline.encoding || null,
                to: nextPipeline.encoding || null,
            },
        );
    }
}

function registerPipelineApi({
    app,
    db,
    getConfig,
    fetch,
    crypto,
    pipelineRuntimeState,
    clearOutputRestartState,
    stopRunningJobAndWait,
    stopRunningJob,
    recomputeConfigSnapshotVersion,
    recomputeSnapshotVersion,
}) {
    const {
        clearPipelineState,
        resolveInputState,
        seedPipelineState,
    } = pipelineRuntimeState;

    function refreshConfigAndHealthSnapshotVersions() {
        recomputeConfigSnapshotVersion();
        recomputeSnapshotVersion();
    }

    function getRecordOrRespond(res, record, notFoundMessage) {
        if (record) return record;
        respondError(res, 404, notFoundMessage);
        return null;
    }

    function parsePipelineMutation(body, existing = null) {
        const name = body?.name ?? existing?.name;

        return {
            name,
            encoding: body?.encoding ?? existing?.encoding ?? null,
            validationError: validateName(name, 'Pipeline name'),
        };
    }

    function createAutoPipelineStreamKey() {
        let key;
        do {
            key = crypto.randomBytes(12).toString('hex');
        } while (db.getStreamKey(key));
        return key;
    }

    function normalizeStreamKeyLabel(value) {
        if (value === undefined || value === null) return null;
        const normalized = String(value).trim();
        return normalized.length > 0 ? normalized : null;
    }

    function parseStreamKeyCreateRequest(body) {
        const hasCustomStreamKey = Object.prototype.hasOwnProperty.call(body || {}, 'streamKey');
        const requestedStreamKey = hasCustomStreamKey ? body?.streamKey : null;
        const key = hasCustomStreamKey
            ? (typeof requestedStreamKey === 'string' ? requestedStreamKey.trim() : requestedStreamKey)
            : crypto.randomBytes(12).toString('hex');

        return {
            hasCustomStreamKey,
            key,
            label: normalizeStreamKeyLabel(body?.label),
            validationError: hasCustomStreamKey ? validateStreamKey(key) : null,
        };
    }

    async function buildStreamKeyResponse(streamKey) {
        if (!streamKey) return streamKey;
        return {
            ...streamKey,
            ingestUrls: await buildIngestUrls(streamKey.key, getConfig),
        };
    }

    async function listStreamKeysWithIngestUrls() {
        return Promise.all(
            db.listStreamKeys().map((streamKey) => buildStreamKeyResponse(streamKey)),
        );
    }

    function initializePipelineInputState(pipelineId, inputStatus, { reason = null } = {}) {
        const status = inputStatus || 'off';
        seedPipelineState(pipelineId, status);

        if (reason) {
            db.appendPipelineEvent(
                pipelineId,
                `[input_state] reset reason=${reason}`,
                'pipeline.input_state.reset',
                { reason },
            );
        }

        db.appendPipelineEvent(
            pipelineId,
            `[input_state] initial_state=${status}`,
            'pipeline.input_state.initialized',
            { state: status },
        );

        return status;
    }

    async function applyMediamtxPathMutation(key, action) {
        // Stream-key creation/deletion must stay in sync with MediaMTX path config, so the route
        // handlers share one request/parse/error path instead of duplicating control-plane logic.
        const methodByAction = {
            add: 'POST',
            delete: 'DELETE',
        };

        const method = methodByAction[action];
        if (!method) {
            throw new Error(`Unsupported MediaMTX path action: ${action}`);
        }

        const effectivePath = buildMediamtxPath(key);
        const url = `${getMediamtxApiBaseUrl()}/v3/config/paths/${action}/${encodeURIComponent(effectivePath)}`;
        const resp = await fetch(url, {
            method,
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ name: effectivePath }),
        });

        let data = null;
        try {
            data = await resp.json();
        } catch (e) {
            /* ignore parse errors */
        }

        if (!resp.ok || data?.error) {
            throw new Error(data?.error || `MediaMTX returned ${resp.status}`);
        }

        return data;
    }

    async function applyMediamtxPathMutationWithRollback(key, action, applyDbChange) {
        await applyMediamtxPathMutation(key, action);

        try {
            return await applyDbChange();
        } catch (dbError) {
            const rollbackAction = action === 'add' ? 'delete' : 'add';
            try {
                await applyMediamtxPathMutation(key, rollbackAction);
            } catch (rollbackError) {
                throw new Error(
                    `${errMsg(dbError)}; MediaMTX rollback (${rollbackAction}) failed: ${errMsg(rollbackError)}`,
                );
            }

            throw new Error(
                `${errMsg(dbError)}; MediaMTX change was rolled back`,
            );
        }
    }

    app.post('/stream-keys', async (req, res) => {
        try {
            const {
                hasCustomStreamKey,
                key,
                label,
                validationError,
            } = parseStreamKeyCreateRequest(req.body || {});

            if (hasCustomStreamKey) {
                if (validationError) {
                    return respondError(res, 400, validationError);
                }
            }

            if (db.getStreamKey(key)) {
                return respondError(res, 409, 'Stream key already exists');
            }

            const streamKey = await applyMediamtxPathMutationWithRollback(
                key,
                'add',
                () => db.createStreamKey({
                    key,
                    label,
                    createdAt: new Date().toISOString(),
                }),
            );
            refreshConfigAndHealthSnapshotVersions();
            return respondJson(res, {
                message: 'Stream key created',
                streamKey: await buildStreamKeyResponse(streamKey),
            }, 201);
        } catch (err) {
            return respondError(res, 500, errMsg(err));
        }
    });

    app.post('/stream-keys/:key', (req, res) => {
        try {
            const { key } = req.params;
            const label = normalizeStreamKeyLabel(req.body?.label);

            if (!getRecordOrRespond(res, db.getStreamKey(key), 'Stream key not found')) return;

            const streamKey = db.updateStreamKey(key, label ?? null);
            refreshConfigAndHealthSnapshotVersions();
            return respondJson(res, { message: 'Stream key updated', streamKey });
        } catch (err) {
            return respondError(res, 500, errMsg(err));
        }
    });

    app.delete('/stream-keys/:key', async (req, res) => {
        try {
            const { key } = req.params;
            if (!getRecordOrRespond(res, db.getStreamKey(key), 'Stream key not found')) return;

            await applyMediamtxPathMutationWithRollback(key, 'delete', () => {
                const deleted = db.deleteStreamKey(key);
                if (!deleted) {
                    throw new Error('Failed to remove stream key from DB');
                }
                return deleted;
            });

            refreshConfigAndHealthSnapshotVersions();
            return respondJson(res, { message: 'Stream key deleted' });
        } catch (err) {
            return respondError(res, 500, errMsg(err));
        }
    });

    app.get('/stream-keys', async (req, res) => {
        try {
            const streamKeys = await listStreamKeysWithIngestUrls();
            return respondJson(res, streamKeys);
        } catch (err) {
            return respondError(res, 500, errMsg(err));
        }
    });

    app.post('/pipelines', async (req, res) => {
        try {
            const runtimeConfig = getConfig();
            const pipelineLimit = Number(runtimeConfig.pipelinesLimit);
            if (Number.isFinite(pipelineLimit) && db.listPipelines().length >= pipelineLimit) {
                return respondError(res, 400, `Pipeline limit reached: ${pipelineLimit}`);
            }

            const { name, encoding, validationError } = parsePipelineMutation(req.body);
            if (validationError) return respondError(res, 400, validationError);

            const streamKey = createAutoPipelineStreamKey();

            await applyMediamtxPathMutationWithRollback(
                streamKey,
                'add',
                () => db.createStreamKey({
                    key: streamKey,
                    label: null,
                    createdAt: new Date().toISOString(),
                }),
            );

            let runtimeState;
            let pipeline;
            try {
                runtimeState = await resolveInputState(streamKey, 0);
                pipeline = db.createPipeline({ name, streamKey, encoding });
            } catch (pipelineError) {
                try {
                    await applyMediamtxPathMutation(streamKey, 'delete');
                } catch {
                    // ignore cleanup failures; original error is returned
                }
                db.deleteStreamKey(streamKey);
                throw pipelineError;
            }

            const pipelineWithState =
                db.updatePipeline(pipeline.id, {
                    name: pipeline.name,
                    streamKey: pipeline.streamKey,
                    encoding: pipeline.encoding,
                    inputEverSeenLive: runtimeState.inputEverSeenLive,
                }) || pipeline;

            db.appendPipelineEvent(
                pipelineWithState.id,
                `[config] created name="${pipelineWithState.name}" stream_key=${pipelineWithState.streamKey ? maskToken(pipelineWithState.streamKey) : 'unassigned'} encoding=${pipelineWithState.encoding || 'null'}`,
                'pipeline.config.created',
                {
                    name: pipelineWithState.name,
                    streamKeyMasked: pipelineWithState.streamKey
                        ? maskToken(pipelineWithState.streamKey)
                        : 'unassigned',
                    encoding: pipelineWithState.encoding || null,
                },
            );
            initializePipelineInputState(pipelineWithState.id, runtimeState.status);

            refreshConfigAndHealthSnapshotVersions();
            return respondJson(res, { message: 'Pipeline created', pipeline: pipelineWithState }, 201);
        } catch (err) {
            return respondError(res, 400, err.message);
        }
    });

    app.post('/pipelines/:id', async (req, res) => {
        try {
            const id = req.params.id;
            const existing = getRecordOrRespond(res, db.getPipeline(id), 'Pipeline not found');
            if (!existing) return;

            const { name, encoding, validationError } = parsePipelineMutation(
                req.body,
                existing,
            );
            if (validationError) return respondError(res, 400, validationError);

            const updated = db.updatePipeline(id, {
                name,
                streamKey: existing.streamKey,
                encoding,
                inputEverSeenLive: Number(existing.inputEverSeenLive || 0),
            });
            if (!updated) return respondError(res, 500, 'Failed to update pipeline');

            logPipelineConfigChanges(db, id, existing, updated);
            refreshConfigAndHealthSnapshotVersions();
            return respondJson(res, { message: 'Pipeline updated', pipeline: updated });
        } catch (err) {
            return respondError(res, 400, err.message);
        }
    });

    app.delete('/pipelines/:id', async (req, res) => {
        try {
            const id = req.params.id;
            const pipeline = getRecordOrRespond(res, db.getPipeline(id), 'Pipeline not found');
            if (!pipeline) return;
            const streamKey = pipeline.streamKey || null;

            const outputs = db.listOutputsForPipeline(id);
            const runningJobs = outputs
                .map((output) => db.getRunningJobFor(id, output.id))
                .filter(Boolean);

            if (runningJobs.length > 0) {
                const stopResults = await Promise.all(
                    runningJobs.map((job) => stopRunningJobAndWait(job)),
                );
                const failedStops = stopResults.filter(
                    (result) => !result.stopped || !result.completed,
                );

                if (failedStops.length > 0) {
                    return respondError(res, 409, 'Failed to stop all outputs before deleting pipeline', {
                        outputs: failedStops.map((result) => ({
                            outputId: result.outputId,
                            jobId: result.jobId,
                            detail: result.waitReason || result.reason,
                        })),
                    });
                }
            }

            const ok = db.deletePipeline(id);
            if (!ok) return respondError(res, 500, 'Failed to delete pipeline');

            clearPipelineState(id);
            for (const output of outputs) {
                clearOutputRestartState(id, output.id);
            }

            if (streamKey) {
                const stillReferenced = db
                    .listPipelines()
                    .some((candidate) => candidate.id !== id && candidate.streamKey === streamKey);

                if (!stillReferenced) {
                    try {
                        await applyMediamtxPathMutation(streamKey, 'delete');
                    } catch {
                        // ignore cleanup failures; pipeline deletion already succeeded
                    }
                    db.deleteStreamKey(streamKey);
                }
            }

            refreshConfigAndHealthSnapshotVersions();
            return respondJson(res, { message: `Pipeline ${id} deleted` });
        } catch (err) {
            return respondError(res, 500, errMsg(err));
        }
    });

    app.get('/pipelines', (req, res) => {
        try {
            return respondJson(res, db.listPipelines());
        } catch (err) {
            return respondError(res, 500, errMsg(err));
        }
    });
}

module.exports = {
    registerConfigApi,
    registerPipelineApi,
};
