'use strict';

// Express routes for output CRUD, start/stop control, job history, and per-output logs.
// Output mutations validate the URL and encoding before persisting. Start/stop delegate
// to the lifecycle service. History and log endpoints query SQLite directly.

const {
    errMsg,
    validateName,
    normalizeOutputEncoding,
    validateOutputUrl,
    INVALID_OUTPUT_URL_ERROR,
} = require('./utils');

const { respondError, respondErrorFromErr, respondJson } = require('./http');

// Output API

const INVALID_OUTPUT_ENCODING_ERROR =
    'Encoding must be one of: source, vertical-crop, vertical-rotate, 720p, 1080p';
const OUTPUT_MUTATION_WHILE_RUNNING_ERROR =
    'Cannot change output URL or encoding while output is running. Stop output first.';
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
const HISTORY_PREFIX_ERROR =
    'prefix must be a comma-separated list of lifecycle, stderr, exit, control, config, input_state';

function normalizeOutputPayload(body, existing = null) {
    const existingEncoding = existing ? normalizeOutputEncoding(existing.encoding) || 'source' : null;
    const name = existing ? body?.name ?? existing.name : body?.name;
    const url = existing ? body?.url ?? existing.url : body?.url;
    const encoding = existing
        ? body?.encoding === undefined
            ? existingEncoding
            : normalizeOutputEncoding(body?.encoding)
        : normalizeOutputEncoding(body?.encoding ?? 'source');

    return {
        name,
        url,
        encoding,
        urlChanged: existing ? url !== existing.url : false,
        encodingChanged: existing ? encoding !== existingEncoding : false,
    };
}

function getOutputValidationError({
    name,
    url,
    encoding,
    running = null,
    urlChanged = false,
    encodingChanged = false,
}) {
    const nameError = validateName(name, 'Output name');
    if (nameError) {
        return { status: 400, error: nameError };
    }

    if (!encoding) {
        return { status: 400, error: INVALID_OUTPUT_ENCODING_ERROR };
    }

    if (running && (urlChanged || encodingChanged)) {
        return { status: 409, error: OUTPUT_MUTATION_WHILE_RUNNING_ERROR };
    }

    if (!validateOutputUrl(url)) {
        return { status: 400, error: INVALID_OUTPUT_URL_ERROR };
    }

    return null;
}

function logOutputConfigChanges(db, pipelineId, outputId, previousOutput, nextOutput) {
    if (!pipelineId || !outputId || !previousOutput || !nextOutput) return;

    const changes = [];
    if (previousOutput.name !== nextOutput.name) {
        changes.push({ field: 'name', from: previousOutput.name, to: nextOutput.name });
    }
    if (previousOutput.url !== nextOutput.url) {
        changes.push({ field: 'url', from: previousOutput.url, to: nextOutput.url });
    }
    if (previousOutput.encoding !== nextOutput.encoding) {
        changes.push({
            field: 'encoding',
            from: previousOutput.encoding || null,
            to: nextOutput.encoding || null,
        });
    }

    if (changes.length === 0) return;

    const summary = changes
        .map((change) => `${change.field}=${change.from ?? 'null'} -> ${change.to ?? 'null'}`)
        .join(' | ');

    db.appendJobLog(
        null,
        `[lifecycle] config_changed ${summary}`,
        pipelineId,
        outputId,
        'lifecycle.config_changed',
        { changes },
    );
}

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

function getHistoryWindowError({ since, until, prefixes }) {
    if (!since || !until) return null;

    const rangeMs = Date.parse(until) - Date.parse(since);
    if (rangeMs > HISTORY_MAX_RANGE_MS) {
        return 'Requested history window is too large';
    }

    if (
        prefixes.some((prefix) => HISTORY_HIGH_VOLUME_PREFIXES.has(prefix)) &&
        rangeMs > HISTORY_MAX_HIGH_VOLUME_RANGE_MS
    ) {
        return 'Requested stderr/exit/control history window is too large';
    }

    return null;
}

function parseHistoryFilters(query, options = {}) {
    const {
        defaultLimit = 200,
        defaultOrder = 'desc',
        defaultPrefixes = [],
        lifecycleMode = false,
    } = options;

    const since = parseHistoryTimestamp(query?.since);
    if (since === undefined) {
        return { error: 'Invalid since timestamp' };
    }

    const until = parseHistoryTimestamp(query?.until);
    if (until === undefined) {
        return { error: 'Invalid until timestamp' };
    }

    if (since && until && since >= until) {
        return { error: 'since must be earlier than until' };
    }

    const order = parseHistoryOrder(query?.order, defaultOrder);
    if (!order) {
        return { error: 'order must be asc or desc' };
    }

    const prefixes = lifecycleMode
        ? ['[lifecycle]']
        : query?.prefix === undefined
          ? [...defaultPrefixes]
          : parseHistoryPrefixes(query?.prefix);
    if (prefixes === null) {
        return { error: HISTORY_PREFIX_ERROR };
    }

    const historyWindowError = getHistoryWindowError({ since, until, prefixes });
    if (historyWindowError) {
        return { error: historyWindowError };
    }

    const limit = parseHistoryLimit(query?.limit, defaultLimit);
    if (limit === null && (!lifecycleMode || query?.limit !== undefined)) {
        return { error: 'limit must be an integer between 1 and 1000' };
    }

    return {
        since,
        until,
        limit,
        order,
        prefixes,
    };
}

function registerOutputApi({
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
}) {
    function refreshConfigAndHealthEtags() {
        recomputeConfigEtag();
        recomputeEtag();
    }

    function getPipelineOrRespond(res, pipelineId) {
        const pipeline = db.getPipeline(pipelineId);
        if (!pipeline) {
            respondError(res, 404, 'Pipeline not found');
            return null;
        }

        return pipeline;
    }

    function getOutputOrRespond(res, pipelineId, outputId) {
        if (!getPipelineOrRespond(res, pipelineId)) return null;

        const output = db.getOutput(pipelineId, outputId);
        if (!output) {
            respondError(res, 404, 'Output not found');
            return null;
        }

        return output;
    }

    async function applyOutputStateChange(pid, oid, options) {
        // Start/stop routes differ in response payload, but both share the same state-change,
        // recovery-reset, and reconcile sequence.
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
            if (!getPipelineOrRespond(res, pid)) return;

            const runtimeConfig = getConfig();
            const outLimit = Number(runtimeConfig.outLimit);
            const currentOutCount = db.listOutputsForPipeline(pid).length;
            if (Number.isFinite(outLimit) && currentOutCount >= outLimit) {
                return respondError(res, 400, `Output limit reached for pipeline: ${outLimit}`);
            }

            const { name, url, encoding } = normalizeOutputPayload(req.body);
            const validationError = getOutputValidationError({ name, url, encoding });
            if (validationError) {
                return respondError(res, validationError.status, validationError.error);
            }

            const output = db.createOutput({ pipelineId: pid, name, url, encoding });

            db.appendJobLog(
                null,
                `[lifecycle] config_created name=${output.name} url=${output.url} encoding=${output.encoding || 'null'}`,
                pid,
                output.id,
                'lifecycle.config_created',
                {
                    name: output.name,
                    url: output.url,
                    encoding: output.encoding || null,
                },
            );

            refreshConfigAndHealthEtags();
            return respondJson(res, { message: 'Output created', output }, 201);
        } catch (err) {
            return respondError(res, 400, err.message || errMsg(err));
        }
    });

    app.post('/pipelines/:pipelineId/outputs/:outputId', (req, res) => {
        try {
            const pid = req.params.pipelineId;
            const oid = req.params.outputId;
            const existing = getOutputOrRespond(res, pid, oid);
            if (!existing) return;

            const running = db.getRunningJobFor(pid, oid);
            const { name, url, encoding, urlChanged, encodingChanged } = normalizeOutputPayload(
                req.body,
                existing,
            );
            const validationError = getOutputValidationError({
                name,
                url,
                encoding,
                running,
                urlChanged,
                encodingChanged,
            });
            if (validationError) {
                return respondError(res, validationError.status, validationError.error);
            }

            const updated = db.updateOutput(pid, oid, { name, url, encoding });
            if (!updated) return respondError(res, 500, 'Failed to update output');

            logOutputConfigChanges(db, pid, oid, existing, updated);

            refreshConfigAndHealthEtags();
            return respondJson(res, { message: 'Output updated', output: updated });
        } catch (err) {
            return respondError(res, 400, err.message || errMsg(err));
        }
    });

    app.delete('/pipelines/:pipelineId/outputs/:outputId', async (req, res) => {
        try {
            const pid = req.params.pipelineId;
            const oid = req.params.outputId;
            const existing = getOutputOrRespond(res, pid, oid);
            if (!existing) return;

            const running = db.getRunningJobFor(pid, oid);
            if (running) {
                const stopResult = await stopRunningJobAndWait(running);
                if (!stopResult.stopped || !stopResult.completed) {
                    return respondError(res, 409, 'Failed to stop output before delete', {
                        detail: stopResult.waitReason || stopResult.reason,
                        result: stopResult,
                    });
                }
            }

            const ok = db.deleteOutput(pid, oid);
            if (!ok) return respondError(res, 500, 'Failed to delete output');

            clearOutputRestartState(pid, oid);
            refreshConfigAndHealthEtags();
            return respondJson(res, { message: `Output ${oid} from pipeline ${pid} deleted` });
        } catch (err) {
            return respondError(res, 500, errMsg(err));
        }
    });

    app.post('/pipelines/:pipelineId/outputs/:outputId/start', async (req, res) => {
        const pid = req.params.pipelineId;
        const oid = req.params.outputId;

        try {
            if (!getOutputOrRespond(res, pid, oid)) return;

            const { reconciliation } = await applyOutputStateChange(pid, oid, {
                desiredState: 'running',
                stateReason: 'manual_start',
                resetReason: 'manual_start',
                trigger: 'manual',
                reconcileReason: 'manual_request',
            });

            if (reconciliation.action === 'started') {
                return respondJson(res, {
                    message: 'Output started',
                    desiredState: 'running',
                    job: reconciliation.job,
                }, 201);
            }

            if (reconciliation.action === 'already_running') {
                return respondJson(res, {
                    message: 'Output already running',
                    desiredState: 'running',
                    job: reconciliation.job,
                });
            }

            if (reconciliation.action === 'waiting_for_input') {
                return respondError(res, 409, 'Pipeline input is not available yet', {
                    message: 'Output desired state set to running; waiting for input',
                    desiredState: 'running',
                    detail: reconciliation.detail,
                });
            }

            if (reconciliation.action === 'start_in_progress') {
                return respondError(res, 409, 'Start already in progress for this output');
            }

            return respondJson(res, {
                message: 'Output desired state set to running',
                desiredState: 'running',
            });
        } catch (err) {
            return respondErrorFromErr(res, err);
        }
    });

    app.post('/pipelines/:pipelineId/outputs/:outputId/stop', async (req, res) => {
        try {
            const pid = req.params.pipelineId;
            const oid = req.params.outputId;

            const output = getOutputOrRespond(res, pid, oid);
            if (!output) return;

            const { desiredStateChange, reconciliation } = await applyOutputStateChange(pid, oid, {
                desiredState: 'stopped',
                stateReason: 'manual_stop',
                resetReason: 'manual_stop',
                trigger: 'manual-stop',
                reconcileReason: 'desired_stopped',
            });

            if (reconciliation.action === 'stop_requested') {
                return respondJson(res, {
                    message: 'Output desired state set to stopped',
                    desiredState: 'stopped',
                    previousState: desiredStateChange?.previousState || 'running',
                    jobId: reconciliation.job?.id || null,
                    result: reconciliation.result,
                });
            }

            return respondJson(res, {
                message: 'Output desired state set to stopped',
                desiredState: 'stopped',
                previousState: desiredStateChange?.previousState || getOutputDesiredState(output),
                jobId: null,
                result: { stopped: false, reason: 'already_stopped' },
            });
        } catch (err) {
            return respondError(res, 500, errMsg(err));
        }
    });

    app.get('/pipelines/:pipelineId/outputs/:outputId/history', (req, res) => {
        try {
            const pid = req.params.pipelineId;
            const oid = req.params.outputId;

            if (!getOutputOrRespond(res, pid, oid)) return;

            const filterLifecycle = req.query.filter === 'lifecycle';
            const historyFilters = parseHistoryFilters(req.query, {
                defaultLimit: filterLifecycle ? null : 200,
                defaultOrder: filterLifecycle ? 'asc' : 'desc',
                lifecycleMode: filterLifecycle,
            });
            if (historyFilters.error) {
                return respondError(res, 400, historyFilters.error);
            }

            const logs = db.listJobLogsByOutputFiltered(pid, oid, {
                since: historyFilters.since,
                until: historyFilters.until,
                limit: historyFilters.limit,
                order: historyFilters.order,
                prefixes: historyFilters.prefixes,
            });

            return respondJson(res, {
                pipelineId: pid,
                outputId: oid,
                logs,
            });
        } catch (err) {
            return respondError(res, 500, errMsg(err));
        }
    });

    app.get('/pipelines/:pipelineId/history', (req, res) => {
        try {
            const pid = req.params.pipelineId;
            if (!getPipelineOrRespond(res, pid)) return;

            const historyFilters = parseHistoryFilters(req.query, {
                defaultLimit: 200,
                defaultOrder: 'desc',
            });
            if (historyFilters.error) {
                return respondError(res, 400, historyFilters.error);
            }

            const logs =
                typeof db.listJobLogsByPipelineFiltered === 'function'
                    ? db.listJobLogsByPipelineFiltered(pid, historyFilters)
                    : db.listJobLogsByPipeline(pid).slice(0, historyFilters.limit || 200);
            return respondJson(res, { pipelineId: pid, logs });
        } catch (err) {
            return respondError(res, 500, errMsg(err));
        }
    });
}

module.exports = {
    registerOutputApi,
};
