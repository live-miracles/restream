const { errMsg, maskToken, validateName } = require('../utils/app');
const {
    getMediamtxApiBaseUrl,
    buildMediamtxPath,
    buildIngestUrls,
} = require('../utils/mediamtx');

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

function registerPipelineApi({
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
}) {
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
            const key = req.body?.streamKey || crypto.randomBytes(12).toString('hex');
            const label = req.body?.label ?? null;

            if (db.getStreamKey(key)) {
                return res.status(409).json({ error: 'Stream key already exists' });
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
            recomputeConfigEtag();
            recomputeEtag();
            return res.status(201).json({
                message: 'Stream key created',
                streamKey: {
                    ...streamKey,
                    ingestUrls: await buildIngestUrls(streamKey.key, getConfig),
                },
            });
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });

    app.post('/stream-keys/:key', (req, res) => {
        try {
            const { key } = req.params;
            const { label } = req.body || {};

            const existing = db.getStreamKey(key);
            if (!existing) {
                return res.status(404).json({ error: 'Stream key not found' });
            }

            const streamKey = db.updateStreamKey(key, label ?? null);
            recomputeConfigEtag();
            recomputeEtag();
            return res.json({ message: 'Stream key updated', streamKey });
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });

    app.delete('/stream-keys/:key', async (req, res) => {
        try {
            const { key } = req.params;
            const existing = db.getStreamKey(key);
            if (!existing) {
                return res.status(404).json({ error: 'Stream key not found' });
            }

            await mutateMediamtxPathWithRollback(key, 'delete', () => {
                const deleted = db.deleteStreamKey(key);
                if (!deleted) {
                    throw new Error('Failed to remove stream key from DB');
                }
                return deleted;
            });

            recomputeConfigEtag();
            recomputeEtag();
            return res.json({ message: 'Stream key deleted' });
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
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
            return res.json(streamKeys);
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });

    app.post('/pipelines', async (req, res) => {
        try {
            const runtimeConfig = getConfig();
            const pipelineLimit = Number(runtimeConfig.pipelinesLimit);
            if (Number.isFinite(pipelineLimit) && db.listPipelines().length >= pipelineLimit) {
                return res.status(400).json({ error: `Pipeline limit reached: ${pipelineLimit}` });
            }

            const name = req.body?.name;
            const streamKey = req.body?.streamKey ?? null;
            const encoding = req.body?.encoding ?? null;
            const nameError = validateName(name, 'Pipeline name');
            if (nameError) {
                return res.status(400).json({ error: nameError });
            }

            const runtimeState = await healthMonitor.resolveRuntimeInputState(streamKey, 0);
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
            healthMonitor.seedPipelineRuntimeState(pipelineWithState.id, runtimeState.status);
            db.appendPipelineEvent(
                pipelineWithState.id,
                `[input_state] initial_state=${runtimeState.status}`,
                'pipeline.input_state.initialized',
                { state: runtimeState.status },
            );

            recomputeConfigEtag();
            recomputeEtag();
            return res
                .status(201)
                .json({ message: 'Pipeline created', pipeline: pipelineWithState });
        } catch (err) {
            return res.status(400).json({ error: err.message });
        }
    });

    app.post('/pipelines/:id', async (req, res) => {
        try {
            const id = req.params.id;
            const existing = db.getPipeline(id);
            if (!existing) return res.status(404).json({ error: 'Pipeline not found' });

            const name = req.body?.name ?? existing.name;
            const streamKey = req.body?.streamKey ?? existing.streamKey;
            const encoding = req.body?.encoding ?? existing.encoding;
            const nameError = validateName(name, 'Pipeline name');
            if (nameError) {
                return res.status(400).json({ error: nameError });
            }

            const streamKeyChanging = streamKey !== existing.streamKey;
            if (streamKeyChanging) {
                const pipelineOutputs = db.listOutputsForPipeline(id);
                const hasRunningJob = pipelineOutputs.some(
                    (output) => !!db.getRunningJobFor(id, output.id),
                );
                if (hasRunningJob) {
                    return res.status(409).json({
                        error: 'Cannot change stream key while outputs are running. Stop all outputs first.',
                    });
                }
            }

            let inputEverSeenLive = Number(existing.inputEverSeenLive || 0);
            let initialInputStatus = null;

            if (streamKeyChanging) {
                const runtimeState = await healthMonitor.resolveRuntimeInputState(streamKey, 0);
                inputEverSeenLive = runtimeState.inputEverSeenLive;
                initialInputStatus = runtimeState.status;
            }

            const updated = db.updatePipeline(id, {
                name,
                streamKey,
                encoding,
                inputEverSeenLive,
            });
            if (!updated) return res.status(500).json({ error: 'Failed to update pipeline' });

            if (streamKeyChanging) {
                healthMonitor.seedPipelineRuntimeState(id, initialInputStatus || 'off');
                db.appendPipelineEvent(
                    id,
                    '[input_state] reset due to stream_key change',
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
            recomputeConfigEtag();
            recomputeEtag();
            return res.json({ message: 'Pipeline updated', pipeline: updated });
        } catch (err) {
            return res.status(400).json({ error: err.message });
        }
    });

    app.delete('/pipelines/:id', async (req, res) => {
        try {
            const id = req.params.id;
            const existing = db.getPipeline(id);
            if (!existing) return res.status(404).json({ error: 'Pipeline not found' });

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
                    return res.status(409).json({
                        error: 'Failed to stop all outputs before deleting pipeline',
                        outputs: failedStops.map((result) => ({
                            outputId: result.outputId,
                            jobId: result.jobId,
                            detail: result.waitReason || result.reason,
                        })),
                    });
                }
            }

            const ok = db.deletePipeline(id);
            if (!ok) return res.status(500).json({ error: 'Failed to delete pipeline' });

            healthMonitor.clearPipelineRuntimeState(id);
            for (const output of outputs) {
                clearOutputRestartState(id, output.id);
            }

            recomputeConfigEtag();
            recomputeEtag();
            return res.json({ message: `Pipeline ${id} deleted` });
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });

    app.get('/pipelines', (req, res) => {
        try {
            return res.json(db.listPipelines());
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });
}

module.exports = {
    registerPipelineApi,
};
