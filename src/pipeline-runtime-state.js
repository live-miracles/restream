'use strict';

// Shared pipeline runtime-state coordinator.
// Owns the in-memory input transition history that routes, health collection, and output
// recovery all share so those modules do not need to call into each other directly.

const {
    errMsg,
    log,
    fetchMediamtxJson,
    buildMediamtxPath,
    getInputUnavailableExitGraceMs,
} = require('./utils');
const { computeInputStatus } = require('./health-compute');

function createPipelineRuntimeStateService({ db, getNow = Date.now }) {
    const pipelineInputStatusHistory = new Map();
    const pipelineLastInputUnavailableAtMs = new Map();
    let inputRecoveryHandler = null;

    function getInputPublisherMetadata(publisher) {
        const protocol = String(publisher?.protocol || '')
            .trim()
            .toLowerCase();
        const remoteAddr = String(publisher?.remoteAddr || '').trim();

        return {
            protocol: protocol || null,
            remoteAddr: remoteAddr || null,
        };
    }

    function setInputRecoveryHandler(fn) {
        inputRecoveryHandler = typeof fn === 'function' ? fn : null;
    }

    function isLatestJobLikelyInputUnavailableStop(pipelineId, latestJob) {
        // A clean stop close to an input-off transition is treated as input loss, not an output
        // failure, so retry logic can suppress noisy restarts during upstream outages.
        if (!latestJob || latestJob.status === 'running') {
            return { matched: false, reason: 'no_terminal_job' };
        }

        if (latestJob.status !== 'stopped') {
            return { matched: false, reason: 'job_not_stopped' };
        }

        const lastInputUnavailableAtMs = pipelineLastInputUnavailableAtMs.get(pipelineId);
        if (!Number.isFinite(lastInputUnavailableAtMs)) {
            return { matched: false, reason: 'no_input_unavailable_transition' };
        }

        const endedAtMs = Date.parse(latestJob.endedAt || '');
        if (!Number.isFinite(endedAtMs)) {
            return { matched: false, reason: 'missing_job_end_time' };
        }

        const graceMs = getInputUnavailableExitGraceMs();
        const deltaMs = Math.abs(endedAtMs - lastInputUnavailableAtMs);
        if (deltaMs > graceMs) {
            return { matched: false, reason: 'outside_grace_window', deltaMs, graceMs };
        }

        return {
            matched: true,
            reason: 'near_input_unavailable_transition',
            deltaMs,
            graceMs,
            exitStatus: latestJob.status,
            exitCode: latestJob.exitCode ?? null,
            exitSignal: latestJob.exitSignal || null,
        };
    }

    async function resolveInputState(streamKey, existingEverSeenLive = 0) {
        // inputEverSeenLive lets the UI and recovery logic distinguish "never published" from
        // "was live before, but is currently missing".
        const hasKey = !!streamKey;
        if (!hasKey) {
            return {
                status: 'off',
                inputEverSeenLive: 0,
            };
        }

        let pathInfo = null;
        try {
            const paths = await fetchMediamtxJson('/v3/paths/list');
            const effectivePath = buildMediamtxPath(streamKey);
            pathInfo =
                (paths.items || []).find((pathItem) => pathItem?.name === effectivePath) || null;
        } catch (_err) {
            return {
                status: computeInputStatus({
                    hasKey: true,
                    pathAvailable: false,
                    pathOnline: false,
                    hasEverSeenLive: Number(existingEverSeenLive || 0) === 1,
                }),
                inputEverSeenLive: Number(existingEverSeenLive || 0),
            };
        }

        const pathAvailable = !!(pathInfo?.available || pathInfo?.ready);
        const pathOnline = !!pathInfo?.online;
        const nextEverSeenLive = pathAvailable ? 1 : Number(existingEverSeenLive || 0);

        return {
            status: computeInputStatus({
                hasKey: true,
                pathAvailable,
                pathOnline,
                hasEverSeenLive: nextEverSeenLive === 1,
            }),
            inputEverSeenLive: nextEverSeenLive,
        };
    }

    async function bootstrap() {
        // Recovery decisions rely on in-memory transition history, so startup seeds that history
        // from current MediaMTX state before timers and routes begin using it.
        const pipelines = db.listPipelines();
        const pathByName = new Map();

        try {
            const paths = await fetchMediamtxJson('/v3/paths/list');
            for (const item of paths.items || []) {
                if (item?.name) pathByName.set(item.name, item);
            }
        } catch (err) {
            log('warn', 'Failed to fetch MediaMTX paths during startup bootstrap', {
                error: errMsg(err),
                pipelineCount: pipelines.length,
            });
        }

        for (const pipeline of pipelines) {
            const key = pipeline.streamKey || '';
            const hasKey = !!key;
            const effectivePath = hasKey ? buildMediamtxPath(key) : '';
            const pathInfo = hasKey ? pathByName.get(effectivePath) : null;
            const pathAvailable = !!(pathInfo?.available || pathInfo?.ready);
            const pathOnline = !!pathInfo?.online;
            const hasEverSeenLive = Number(pipeline.inputEverSeenLive || 0) === 1 || pathAvailable;
            const status = computeInputStatus({
                hasKey,
                pathAvailable,
                pathOnline,
                hasEverSeenLive,
            });

            pipelineInputStatusHistory.set(pipeline.id, status);

            if (hasKey && pathAvailable && Number(pipeline.inputEverSeenLive || 0) !== 1) {
                db.markPipelineInputSeenLive(pipeline.id);
            }
        }

        log('info', 'Pipeline input state bootstrap complete', {
            pipelineCount: pipelines.length,
            seededCount: pipelineInputStatusHistory.size,
        });
    }

    function recordPipelineInputStatus(pipelineId, inputStatus, options = {}) {
        // Recovery logic needs both the latest state and the last off-transition timestamp. The
        // history logs here also power the pipeline-history modal in the dashboard.
        const previousInputStatus = pipelineInputStatusHistory.get(pipelineId);
        const publisherMeta = getInputPublisherMetadata(options.publisher);
        const inputBecameOn = inputStatus === 'on';
        const transitionDetails = inputBecameOn
            ? ` protocol=${publisherMeta.protocol || 'unknown'} remote=${publisherMeta.remoteAddr || 'unknown'}`
            : '';

        if (previousInputStatus === undefined) {
            db.appendPipelineEvent(
                pipelineId,
                `[input_state] initial_state=${inputStatus}${transitionDetails}`,
                'pipeline.input_state.initialized',
                {
                    state: inputStatus,
                    protocol: inputBecameOn ? publisherMeta.protocol : null,
                    remoteAddr: inputBecameOn ? publisherMeta.remoteAddr : null,
                },
            );
        } else if (previousInputStatus !== inputStatus) {
            db.appendPipelineEvent(
                pipelineId,
                `[input_state] ${previousInputStatus} -> ${inputStatus}${transitionDetails}`,
                'pipeline.input_state.transitioned',
                {
                    from: previousInputStatus,
                    to: inputStatus,
                    protocol: inputBecameOn ? publisherMeta.protocol : null,
                    remoteAddr: inputBecameOn ? publisherMeta.remoteAddr : null,
                },
            );
        }

        if (
            previousInputStatus !== undefined &&
            previousInputStatus === 'on' &&
            inputStatus !== 'on'
        ) {
            pipelineLastInputUnavailableAtMs.set(pipelineId, getNow());
        }

        pipelineInputStatusHistory.set(pipelineId, inputStatus);

        if (
            previousInputStatus !== undefined &&
            previousInputStatus !== 'on' &&
            inputStatus === 'on'
        ) {
            inputRecoveryHandler?.(pipelineId);
        }

        return {
            previous: previousInputStatus,
            current: inputStatus,
            changed: previousInputStatus !== inputStatus,
        };
    }

    function seedPipelineState(pipelineId, status) {
        pipelineInputStatusHistory.set(pipelineId, status || 'off');
        pipelineLastInputUnavailableAtMs.delete(pipelineId);
    }

    function clearPipelineState(pipelineId) {
        pipelineInputStatusHistory.delete(pipelineId);
        pipelineLastInputUnavailableAtMs.delete(pipelineId);
    }

    return {
        bootstrap,
        clearPipelineState,
        isLatestJobLikelyInputUnavailableStop,
        recordPipelineInputStatus,
        resolveInputState,
        seedPipelineState,
        setInputRecoveryHandler,
    };
}

module.exports = {
    createPipelineRuntimeStateService,
};