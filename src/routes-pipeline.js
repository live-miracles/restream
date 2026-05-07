'use strict';

// Express routes for the config snapshot and pipeline/stream-key CRUD API.
// GET /config serves the dashboard polling endpoint with ETag support.
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

// /config is the durable dashboard snapshot. It exposes both a full-state ETag and a config-only
// ETag so the frontend can detect meaningful changes without always refetching the entire payload.
function normalizeEtag(value) {
    if (!value) return null;
    return value.replace(/^"(.*)"$/, '$1');
}

// Keep cache-related headers together so GET and HEAD stay aligned.
function setSnapshotHeaders(res, etag, configEtag) {
    if (etag) res.set('ETag', `"${etag}"`);
    if (configEtag) res.set('X-Config-ETag', `"${configEtag}"`);
    if (etag) res.set('X-Snapshot-Version', `"${etag}"`);
}

function registerConfigApi({ app, db, getConfig, toPublicConfig, buildIngestUrlsImpl = buildIngestUrls }) {
    function recomputeConfigEtag() {
        try {
            // Config ETag ignores runtime job rows and changes only when the operator-facing
            // configuration shape changes.
            const etag = hashSnapshot(buildConfigSnapshot({ db }));
            db.setConfigEtag(etag);
            return etag;
        } catch (err) {
            console.error('recomputeConfigEtag error:', err);
            return null;
        }
    }

    function recomputeEtag() {
        try {
            // Full snapshot ETag includes current job rows because the dashboard uses /config as
            // its durable control-plane view, not just static configuration.
            const etag = hashSnapshot({
                ...buildConfigSnapshot({ db }),
                jobs: buildJobsSnapshot({ db }),
            });

            db.setEtag(etag);
            return etag;
        } catch (err) {
            console.error('recomputeEtag error:', err);
            return null;
        }
    }

    // Seed both ETags on startup so the first poll can use normal cache headers immediately.
    (async () => {
        try {
            if (!db.getConfigEtag()) recomputeConfigEtag();
            if (!db.getEtag()) recomputeEtag();
        } catch (e) {
            /* ignore */
        }
    })();

    app.get('/config', async (req, res) => {
        try {
            let etag = db.getEtag();
            let configEtag = db.getConfigEtag();
            if (!configEtag) configEtag = recomputeConfigEtag();
            if (!etag) etag = recomputeEtag();

            const ifNoneMatch = normalizeEtag(req.get('If-None-Match'));
            if (ifNoneMatch && etag && ifNoneMatch === etag) {
                setSnapshotHeaders(res, etag, configEtag);
                return respondEmpty(res, 304);
            }

            const snapshot = await buildConfigApiSnapshot({
                db,
                getConfig,
                toPublicConfig,
                buildIngestUrls: buildIngestUrlsImpl,
            });

            setSnapshotHeaders(res, etag, configEtag);
            return respondJson(res, snapshot);
        } catch (err) {
            return respondError(res, 500, errMsg(err));
        }
    });

    app.head('/config/version', (req, res) => {
        try {
            let configEtag = db.getConfigEtag();
            if (!configEtag) configEtag = recomputeConfigEtag();

            const ifNoneMatch = normalizeEtag(req.get('If-None-Match'));
            if (ifNoneMatch && configEtag && ifNoneMatch === configEtag) {
                return respondEmpty(res, 304, { ETag: `"${configEtag}"` });
            }

            return respondEmpty(res, 200, configEtag ? { ETag: `"${configEtag}"` } : null);
        } catch (err) {
            return respondEmpty(res, 500);
        }
    });

    app.head('/config', (req, res) => {
        try {
            const etag = db.getEtag();
            const configEtag = db.getConfigEtag();
            setSnapshotHeaders(res, etag, configEtag);
            return respondEmpty(res, 200);
        } catch (err) {
            return respondEmpty(res, 500);
        }
    });

    return {
        normalizeEtag,
        recomputeConfigEtag,
        recomputeEtag,
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

    if (previousPipeline.streamKey !== nextPipeline.streamKey) {
        db.appendPipelineEvent(
            pipelineId,
            `[config] stream_key changed from ${previousPipeline.streamKey ? maskToken(previousPipeline.streamKey) : 'unassigned'} to ${nextPipeline.streamKey ? maskToken(nextPipeline.streamKey) : 'unassigned'}`,
            'pipeline.config.stream_key_changed',
            {
                fromMasked: previousPipeline.streamKey
                    ? maskToken(previousPipeline.streamKey)
                    : 'unassigned',
                toMasked: nextPipeline.streamKey
                    ? maskToken(nextPipeline.streamKey)
                    : 'unassigned',
            },
        );
    }
}

function normalizePipelineStreamKey(value) {
    if (value === null || value === undefined) return null;
    if (typeof value !== 'string') return value;

    const normalized = value.trim();
    return normalized || null;
}

function registerPipelineApi({
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
}) {
    const {
        clearPipelineState,
        resolveInputState,
        seedPipelineState,
    } = pipelineRuntimeState;

    function refreshConfigAndHealthEtags() {
        recomputeConfigEtag();
        recomputeEtag();
    }

    function getRecordOrRespond(res, record, notFoundMessage) {
        if (record) return record;
        respondError(res, 404, notFoundMessage);
        return null;
    }

    function parsePipelineMutation(body, existing = null) {
        const hasStreamKeyUpdate = !existing || Object.prototype.hasOwnProperty.call(body || {}, 'streamKey');
        const name = body?.name ?? existing?.name;
        const streamKey = hasStreamKeyUpdate
            ? normalizePipelineStreamKey(body?.streamKey)
            : existing.streamKey;

        return {
            name,
            streamKey,
            encoding: body?.encoding ?? existing?.encoding ?? null,
            hasStreamKeyUpdate,
            validationError:
                validateName(name, 'Pipeline name') ||
                (hasStreamKeyUpdate && streamKey !== null ? validateStreamKey(streamKey) : null),
        };
    }

    async function mutateMediamtxPathWithRollback(key, action, applyDbChange) {
        await mutateMediamtxPath(key, action);

        try {
            return await applyDbChange();
        } catch (dbError) {
            const rollbackAction = action === 'add' ? 'delete' : 'add';
            try {
                await mutateMediamtxPath(key, rollbackAction);
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

    async function mutateMediamtxPath(key, action) {
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

    app.post('/stream-keys', async (req, res) => {
        try {
            const hasCustomStreamKey = Object.prototype.hasOwnProperty.call(req.body || {}, 'streamKey');
            const key = hasCustomStreamKey
                ? (typeof req.body?.streamKey === 'string' ? req.body.streamKey.trim() : req.body?.streamKey)
                : crypto.randomBytes(12).toString('hex');
            const label = req.body?.label ?? null;

            if (hasCustomStreamKey) {
                const streamKeyError = validateStreamKey(key);
                if (streamKeyError) {
                    return respondError(res, 400, streamKeyError);
                }
            }

            if (db.getStreamKey(key)) {
                return respondError(res, 409, 'Stream key already exists');
            }

            const streamKey = await mutateMediamtxPathWithRollback(
                key,
                'add',
                () => db.createStreamKey({
                    key,
                    label,
                    createdAt: new Date().toISOString(),
                }),
            );
            refreshConfigAndHealthEtags();
            return respondJson(res, {
                message: 'Stream key created',
                streamKey: {
                    ...streamKey,
                    ingestUrls: await buildIngestUrls(streamKey.key, getConfig),
                },
            }, 201);
        } catch (err) {
            return respondError(res, 500, errMsg(err));
        }
    });

    app.post('/stream-keys/:key', (req, res) => {
        try {
            const { key } = req.params;
            const { label } = req.body || {};

            if (!getRecordOrRespond(res, db.getStreamKey(key), 'Stream key not found')) return;

            const streamKey = db.updateStreamKey(key, label ?? null);
            refreshConfigAndHealthEtags();
            return respondJson(res, { message: 'Stream key updated', streamKey });
        } catch (err) {
            return respondError(res, 500, errMsg(err));
        }
    });

    app.delete('/stream-keys/:key', async (req, res) => {
        try {
            const { key } = req.params;
            if (!getRecordOrRespond(res, db.getStreamKey(key), 'Stream key not found')) return;

            await mutateMediamtxPathWithRollback(key, 'delete', () => {
                const deleted = db.deleteStreamKey(key);
                if (!deleted) {
                    throw new Error('Failed to remove stream key from DB');
                }
                return deleted;
            });

            refreshConfigAndHealthEtags();
            return respondJson(res, { message: 'Stream key deleted' });
        } catch (err) {
            return respondError(res, 500, errMsg(err));
        }
    });

    app.get('/stream-keys', async (req, res) => {
        try {
            const streamKeys = await Promise.all(
                db.listStreamKeys().map(async (streamKey) => ({
                    ...streamKey,
                    ingestUrls: await buildIngestUrls(streamKey.key, getConfig),
                })),
            );
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

            const { name, streamKey, encoding, validationError } = parsePipelineMutation(req.body);
            if (validationError) return respondError(res, 400, validationError);

            const runtimeState = await resolveInputState(streamKey, 0);
            const pipeline = db.createPipeline({ name, streamKey, encoding });
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
            seedPipelineState(pipelineWithState.id, runtimeState.status);
            db.appendPipelineEvent(
                pipelineWithState.id,
                `[input_state] initial_state=${runtimeState.status}`,
                'pipeline.input_state.initialized',
                { state: runtimeState.status },
            );

            refreshConfigAndHealthEtags();
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

            const { name, streamKey, encoding, hasStreamKeyUpdate, validationError } = parsePipelineMutation(
                req.body,
                existing,
            );
            if (validationError) return respondError(res, 400, validationError);

            const streamKeyChanging = streamKey !== existing.streamKey;
            if (streamKeyChanging) {
                const pipelineOutputs = db.listOutputsForPipeline(id);
                const hasRunningJob = pipelineOutputs.some(
                    (output) => !!db.getRunningJobFor(id, output.id),
                );
                if (hasRunningJob) {
                    return respondError(
                        res,
                        409,
                        'Cannot change stream key while outputs are running. Stop all outputs first.',
                    );
                }
            }

            let inputEverSeenLive = Number(existing.inputEverSeenLive || 0);
            let initialInputStatus = null;

            if (streamKeyChanging) {
                const runtimeState = await resolveInputState(streamKey, 0);
                inputEverSeenLive = runtimeState.inputEverSeenLive;
                initialInputStatus = runtimeState.status;
            }

            const updated = db.updatePipeline(id, {
                name,
                streamKey,
                encoding,
                inputEverSeenLive,
            });
            if (!updated) return respondError(res, 500, 'Failed to update pipeline');

            if (streamKeyChanging) {
                seedPipelineState(id, initialInputStatus || 'off');
                db.appendPipelineEvent(
                    id,
                    '[input_state] reset reason=stream_key_change',
                    'pipeline.input_state.reset',
                    { reason: 'stream_key_change' },
                );
                db.appendPipelineEvent(
                    id,
                    `[input_state] initial_state=${initialInputStatus || 'off'}`,
                    'pipeline.input_state.initialized',
                    { state: initialInputStatus || 'off' },
                );

                const outputs = db.listOutputsForPipeline(id);
                for (const output of outputs) {
                    resetOutputFailureCount(id, output.id, 'stream_key_change');
                }
            }

            logPipelineConfigChanges(db, id, existing, updated);
            refreshConfigAndHealthEtags();
            return respondJson(res, { message: 'Pipeline updated', pipeline: updated });
        } catch (err) {
            return respondError(res, 400, err.message);
        }
    });

    app.delete('/pipelines/:id', async (req, res) => {
        try {
            const id = req.params.id;
            if (!getRecordOrRespond(res, db.getPipeline(id), 'Pipeline not found')) return;

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

            refreshConfigAndHealthEtags();
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
