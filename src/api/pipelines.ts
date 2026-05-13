import type { Express } from 'express';
import { errMsg, maskToken, validateName, validateStreamKey } from '../utils/app';
import { buildIngestUrls, getPermanentStreamKeys } from '../utils/mediamtx';
import type { Db, Pipeline } from '../types';
import type { HealthMonitor } from '../services/health';
import type { OutputLifecycle } from '../services/outputs';

function logPipelineConfigChanges(
    db: Db,
    pipelineId: string,
    previousPipeline: Pipeline,
    nextPipeline: Pipeline,
) {
    if (!pipelineId || !previousPipeline || !nextPipeline) return;

    if (previousPipeline.name !== nextPipeline.name) {
        db.appendPipelineEvent(
            pipelineId,
            `[config] name changed from "${previousPipeline.name}" to "${nextPipeline.name}"`,
            'pipeline.config.name_changed',
            { from: previousPipeline.name, to: nextPipeline.name },
        );
    }

    if (previousPipeline.encoding !== nextPipeline.encoding) {
        db.appendPipelineEvent(
            pipelineId,
            `[config] encoding changed from ${previousPipeline.encoding || 'null'} to ${nextPipeline.encoding || 'null'}`,
            'pipeline.config.encoding_changed',
            { from: previousPipeline.encoding || null, to: nextPipeline.encoding || null },
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

function normalizePipelineStreamKey(value: unknown): string | null {
    if (typeof value !== 'string') return null;
    return value.trim() || null;
}

function chooseAutomaticStreamKey(
    permanentStreamKeys: { key: string }[],
    pipelines: Pipeline[],
    excludedPipelineId: string | null = null,
): string | null {
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

async function resolvePipelineStreamKey({
    requestedStreamKey,
    db,
    excludedPipelineId = null,
}: {
    requestedStreamKey: string | null;
    db: Db;
    excludedPipelineId?: string | null;
}): Promise<{ streamKey?: string; error?: string }> {
    const permanentStreamKeys = await getPermanentStreamKeys();

    if (requestedStreamKey !== null) {
        const streamKeyError = validateStreamKey(requestedStreamKey);
        if (streamKeyError) return { error: streamKeyError };

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

export function registerPipelineApi({
    app,
    db,
    healthMonitor,
    resetOutputFailureCount,
    clearOutputRestartState,
    stopRunningJobAndWait,
    stopRunningJob,
}: {
    app: Express;
    db: Db;
    healthMonitor: HealthMonitor;
    resetOutputFailureCount: OutputLifecycle['resetOutputFailureCount'];
    clearOutputRestartState: OutputLifecycle['clearOutputRestartState'];
    stopRunningJobAndWait: OutputLifecycle['stopRunningJobAndWait'];
    stopRunningJob: OutputLifecycle['stopRunningJob'];
}): void {
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
            const body = (req.body || {}) as Record<string, unknown>;
            const name = body?.name;
            const requestedStreamKey = normalizePipelineStreamKey(body?.streamKey);
            const encoding = (body?.encoding as string | null | undefined) ?? null;
            const nameError = validateName(name, 'Pipeline name');
            if (nameError) return res.status(400).json({ error: nameError });

            const resolvedStreamKey = await resolvePipelineStreamKey({ requestedStreamKey, db });
            if (resolvedStreamKey.error) {
                return res.status(400).json({ error: resolvedStreamKey.error });
            }
            const streamKey = resolvedStreamKey.streamKey!;

            const runtimeState = await healthMonitor.resolveRuntimeInputState(streamKey, 0);
            const pipeline = db.createPipeline({ name: name as string, streamKey, encoding });
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

            return res
                .status(201)
                .json({ message: 'Pipeline created', pipeline: pipelineWithState });
        } catch (err) {
            return res.status(400).json({ error: errMsg(err) });
        }
    });

    app.post('/pipelines/:id', async (req, res) => {
        try {
            const id = req.params.id;
            const existing = db.getPipeline(id);
            if (!existing) return res.status(404).json({ error: 'Pipeline not found' });

            const body = (req.body || {}) as Record<string, unknown>;
            const name = (body?.name ?? existing.name) as string;
            const hasStreamKeyUpdate = Object.prototype.hasOwnProperty.call(body, 'streamKey');
            const requestedStreamKey = hasStreamKeyUpdate
                ? normalizePipelineStreamKey(body?.streamKey)
                : existing.streamKey;
            const encoding = (body?.encoding ?? existing.encoding) as string | null;
            const nameError = validateName(name, 'Pipeline name');
            if (nameError) return res.status(400).json({ error: nameError });

            const resolvedStreamKey = await resolvePipelineStreamKey({
                requestedStreamKey,
                db,
                excludedPipelineId: id,
            });
            if (resolvedStreamKey.error) {
                return res.status(400).json({ error: resolvedStreamKey.error });
            }
            const streamKey = resolvedStreamKey.streamKey!;

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
            let initialInputStatus: string | null = null;

            if (streamKeyChanging) {
                const runtimeState = await healthMonitor.resolveRuntimeInputState(streamKey, 0);
                inputEverSeenLive = runtimeState.inputEverSeenLive;
                initialInputStatus = runtimeState.status;
            }

            const updated = db.updatePipeline(id, { name, streamKey, encoding, inputEverSeenLive });
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
                    resetOutputFailureCount(id, output.id);
                }
            }

            logPipelineConfigChanges(db, id, existing, updated);
            return res.json({ message: 'Pipeline updated', pipeline: updated });
        } catch (err) {
            return res.status(400).json({ error: errMsg(err) });
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
                .filter((j): j is NonNullable<typeof j> => j !== undefined && j !== null);

            if (runningJobs.length > 0) {
                const stopResults = await Promise.all(
                    runningJobs.map((job) => stopRunningJobAndWait(job)),
                );
                const failedStops = stopResults.filter((r) => !r.stopped || !r.completed);

                if (failedStops.length > 0) {
                    return res.status(409).json({
                        error: 'Failed to stop all outputs before deleting pipeline',
                        outputs: failedStops.map((result) => ({
                            jobId: result.jobId,
                            detail: result.reason,
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
