function registerPipelineApi({
    app,
    db,
    getConfig,
    fetch,
    crypto,
    errMsg,
    getMediamtxApiBaseUrl,
    healthMonitor,
    maskToken,
    logPipelineConfigChanges,
    resetOutputFailureCount,
    clearOutputRestartState,
    stopRunningJob,
    recomputeConfigEtag,
    recomputeEtag,
    validateName,
}) {
    async function mutateMediamtxPath(key, action) {
        const methodByAction = {
            add: 'POST',
            delete: 'DELETE',
        };

        const method = methodByAction[action];
        if (!method) {
            throw new Error(`Unsupported MediaMTX path action: ${action}`);
        }

        const url = `${getMediamtxApiBaseUrl()}/v3/config/paths/${action}/${encodeURIComponent(key)}`;
        const resp = await fetch(url, {
            method,
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

            await mutateMediamtxPath(key, 'add');

            const streamKey = db.createStreamKey({
                key,
                label,
                createdAt: new Date().toISOString(),
            });
            recomputeConfigEtag();
            recomputeEtag();
            return res.status(201).json({ message: 'Stream key created', streamKey });
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

            await mutateMediamtxPath(key, 'delete');

            const deleted = db.deleteStreamKey(key);
            if (!deleted) {
                return res.status(500).json({ error: 'Failed to remove stream key from DB' });
            }

            recomputeConfigEtag();
            recomputeEtag();
            return res.json({ message: 'Stream key deleted' });
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });

    app.get('/stream-keys', (req, res) => {
        try {
            return res.json(db.listStreamKeys());
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
                'pipeline_config',
            );
            healthMonitor.seedPipelineRuntimeState(pipelineWithState.id, runtimeState.status);
            db.appendPipelineEvent(
                pipelineWithState.id,
                `[input_state] initial_state=${runtimeState.status}`,
                'pipeline_state',
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
                    'pipeline_state',
                );
                db.appendPipelineEvent(
                    id,
                    `[input_state] initial_state=${initialInputStatus || 'off'}`,
                    'pipeline_state',
                );

                const outputs = db.listOutputsForPipeline(id);
                for (const output of outputs) {
                    resetOutputFailureCount(id, output.id, 'stream_key_change');
                }
            }

            logPipelineConfigChanges(id, existing, updated);
            recomputeConfigEtag();
            recomputeEtag();
            return res.json({ message: 'Pipeline updated', pipeline: updated });
        } catch (err) {
            return res.status(400).json({ error: err.message });
        }
    });

    app.delete('/pipelines/:id', (req, res) => {
        try {
            const id = req.params.id;
            const existing = db.getPipeline(id);
            if (!existing) return res.status(404).json({ error: 'Pipeline not found' });

            const outputs = db.listOutputsForPipeline(id);
            for (const output of outputs) {
                const running = db.getRunningJobFor(id, output.id);
                if (running) stopRunningJob(running);
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
