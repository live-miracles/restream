const { errMsg, maskToken, validateName, validateStreamKey } = require('../utils/app');
const { buildIngestUrls, getPermanentStreamKeys } = require('../utils/mediamtx');

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
            `[config] stream_key changed from ${maskToken(previousPipeline.streamKey)} to ${maskToken(nextPipeline.streamKey)}`,
            'pipeline.config.stream_key_changed',
            {
                fromMasked: maskToken(previousPipeline.streamKey),
                toMasked: maskToken(nextPipeline.streamKey),
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

function chooseAutomaticStreamKey(permanentStreamKeys, pipelines, excludedPipelineId = null) {
    const availableKeys = (permanentStreamKeys || []).map((item) => item?.key).filter(Boolean);
    if (availableKeys.length === 0) return null;

    const usedKeys = new Set(
        (pipelines || [])
            .filter((pipeline) => pipeline?.id !== excludedPipelineId)
            .map((pipeline) => pipeline?.streamKey)
            .filter(Boolean),
    );

    return availableKeys.find((key) => !usedKeys.has(key)) || availableKeys[0];
}

async function resolvePipelineStreamKey({ requestedStreamKey, db, excludedPipelineId = null }) {
    const permanentStreamKeys = await getPermanentStreamKeys();

    if (requestedStreamKey !== null) {
        const streamKeyError = validateStreamKey(requestedStreamKey);
        if (streamKeyError) {
            return { error: streamKeyError };
        }

        if (!permanentStreamKeys.some((item) => item.key === requestedStreamKey)) {
            return { error: 'Stream key must match one of the permanent MediaMTX paths' };
        }

        return { streamKey: requestedStreamKey };
    }

    const streamKey = chooseAutomaticStreamKey(
        permanentStreamKeys,
        db.listPipelines(),
        excludedPipelineId,
    );
    if (!streamKey) {
        return { error: 'No permanent MediaMTX stream paths are configured' };
    }

    return { streamKey };
}

function registerPipelineApi({
    app,
    db,
    healthMonitor,
    resetOutputFailureCount,
    clearOutputRestartState,
    stopRunningJobAndWait,
    stopRunningJob,
    recomputeConfigEtag,
    recomputeEtag,
}) {
    app.get('/stream-keys', async (req, res) => {
        try {
            const streamKeys = await Promise.all(
                (await getPermanentStreamKeys()).map(async (streamKey) => ({
                    ...streamKey,
                    ingestUrls: await buildIngestUrls(streamKey.key),
                })),
            );
            return res.json(streamKeys);
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });

    app.post('/pipelines', async (req, res) => {
        try {
            const name = req.body?.name;
            const requestedStreamKey = normalizePipelineStreamKey(req.body?.streamKey);
            const encoding = req.body?.encoding ?? null;
            const nameError = validateName(name, 'Pipeline name');
            if (nameError) {
                return res.status(400).json({ error: nameError });
            }

            const resolvedStreamKey = await resolvePipelineStreamKey({
                requestedStreamKey,
                db,
            });
            if (resolvedStreamKey.error) {
                return res.status(400).json({ error: resolvedStreamKey.error });
            }
            const streamKey = resolvedStreamKey.streamKey;

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
                `[config] created name="${pipelineWithState.name}" stream_key=${maskToken(pipelineWithState.streamKey)} encoding=${pipelineWithState.encoding || 'null'}`,
                'pipeline.config.created',
                {
                    name: pipelineWithState.name,
                    streamKeyMasked: maskToken(pipelineWithState.streamKey),
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
            const hasStreamKeyUpdate = Object.prototype.hasOwnProperty.call(
                req.body || {},
                'streamKey',
            );
            const requestedStreamKey = hasStreamKeyUpdate
                ? normalizePipelineStreamKey(req.body?.streamKey)
                : existing.streamKey;
            const encoding = req.body?.encoding ?? existing.encoding;
            const nameError = validateName(name, 'Pipeline name');
            if (nameError) {
                return res.status(400).json({ error: nameError });
            }

            const resolvedStreamKey = await resolvePipelineStreamKey({
                requestedStreamKey,
                db,
                excludedPipelineId: id,
            });
            if (resolvedStreamKey.error) {
                return res.status(400).json({ error: resolvedStreamKey.error });
            }
            const streamKey = resolvedStreamKey.streamKey;

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
