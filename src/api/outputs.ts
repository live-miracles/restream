import type { Express } from 'express';
import { errMsg, validateName } from '../utils/app';
import {
    normalizeOutputEncoding,
    validateOutputUrl,
    INVALID_OUTPUT_URL_ERROR,
    isValidOutputEncoding,
} from '../utils/ffmpeg';
import type { Db, Output } from '../types';
import type { OutputLifecycle } from '../services/outputs';

const HISTORY_MESSAGE_PREFIXES: Record<string, string> = {
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
const INVALID_OUTPUT_ENCODING_ERROR = 'Encoding must be a valid encoding key';
const OUTPUT_MUTATION_WHILE_RUNNING_ERROR =
    'Cannot change output URL or encoding while output is running. Stop output first.';

function parseHistoryTimestamp(value: unknown): string | null | undefined {
    if (value === undefined || value === null || value === '') return null;
    const parsed = new Date(String(value));
    if (Number.isNaN(parsed.getTime())) return undefined;
    return parsed.toISOString();
}

function parseHistoryOrder(value: unknown, defaultValue = 'desc'): string | null {
    if (value === undefined || value === null || value === '') return defaultValue;
    const normalized = String(value).trim().toLowerCase();
    if (normalized === 'asc' || normalized === 'desc') return normalized;
    return null;
}

function parseHistoryLimit(value: unknown, defaultValue: number | null = 200): number | null {
    if (value === undefined || value === null || value === '') return defaultValue;
    const parsed = Number.parseInt(String(value), 10);
    if (!Number.isFinite(parsed)) return null;
    return Math.max(1, Math.min(parsed, HISTORY_MAX_LIMIT));
}

function parseHistoryPrefixes(value: unknown): string[] | null {
    if (value === undefined || value === null || value === '') return [];

    const rawValues = Array.isArray(value) ? value : [value];
    const tokens = rawValues
        .flatMap((entry) => String(entry).split(','))
        .map((entry) => entry.trim().toLowerCase())
        .filter(Boolean);

    const prefixes: string[] = [];
    for (const token of tokens) {
        const mappedPrefix = HISTORY_MESSAGE_PREFIXES[token];
        if (!mappedPrefix) return null;
        if (!prefixes.includes(mappedPrefix)) prefixes.push(mappedPrefix);
    }

    return prefixes;
}

export function registerOutputApi({
    app,
    db,
    clearOutputRestartState,
    getOutputDesiredState,
    reconcileOutput,
    resetOutputFailureCount,
    setOutputDesiredState,
    stopRunningJobAndWait,
    stopRunningJob,
}: {
    app: Express;
    db: Db;
    clearOutputRestartState: OutputLifecycle['clearOutputRestartState'];
    getOutputDesiredState: OutputLifecycle['getOutputDesiredState'];
    reconcileOutput: OutputLifecycle['reconcileOutput'];
    resetOutputFailureCount: OutputLifecycle['resetOutputFailureCount'];
    setOutputDesiredState: OutputLifecycle['setOutputDesiredState'];
    stopRunningJobAndWait: OutputLifecycle['stopRunningJobAndWait'];
    stopRunningJob: OutputLifecycle['stopRunningJob'];
}): void {
    function logOutputConfigChanges(
        pipelineId: string,
        outputId: string,
        previousOutput: Output,
        nextOutput: Output,
    ) {
        if (!pipelineId || !outputId || !previousOutput || !nextOutput) return;

        const changes: { field: string; from: string | null; to: string | null }[] = [];
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
            .map((c) => `${c.field}=${c.from ?? 'null'} -> ${c.to ?? 'null'}`)
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

    async function applyOutputStateChange(
        pid: string,
        oid: string,
        options: {
            desiredState: string;
            stateReason: string;
            resetReason: string;
            trigger: string;
            reconcileReason: string;
        },
    ) {
        const { desiredState, stateReason, resetReason, trigger, reconcileReason } = options;

        const desiredStateChange = setOutputDesiredState(pid, oid, desiredState, {
            source: 'api',
            reason: stateReason,
        });

        resetOutputFailureCount(pid, oid);

        const reconciliation = await reconcileOutput(pid, oid, {
            trigger,
            reason: reconcileReason,
            source: 'api',
        });

        return { desiredStateChange, reconciliation };
    }

    function normalizeOutputPayload(
        body: Record<string, unknown> | null | undefined,
        existing: Output | null = null,
    ) {
        const existingEncoding = existing
            ? normalizeOutputEncoding(existing.encoding) || 'source'
            : null;
        const name = existing ? (body?.name ?? existing.name) : body?.name;
        const url = existing ? (body?.url ?? existing.url) : body?.url;
        const encoding = existing
            ? body?.encoding === undefined
                ? existingEncoding
                : normalizeOutputEncoding(body?.encoding)
            : normalizeOutputEncoding(body?.encoding ?? 'source');

        return {
            name: name as string | undefined,
            url: url as string | undefined,
            encoding: encoding as string,
            existingEncoding,
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
    }: {
        name: unknown;
        url: unknown;
        encoding: string;
        running?: unknown;
        urlChanged?: boolean;
        encodingChanged?: boolean;
    }): { status: number; error: string } | null {
        const nameError = validateName(name, 'Output name');
        if (nameError) return { status: 400, error: nameError };

        if (!encoding || !isValidOutputEncoding(encoding)) {
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

    app.post('/pipelines/:pipelineId/outputs', (req, res) => {
        try {
            const pid = req.params.pipelineId;
            const pipeline = db.getPipeline(pid);
            if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });

            const { name, url, encoding } = normalizeOutputPayload(
                req.body as Record<string, unknown>,
            );
            const validationError = getOutputValidationError({ name, url, encoding });
            if (validationError) {
                return res.status(validationError.status).json({ error: validationError.error });
            }

            const output = db.createOutput({
                pipelineId: pid,
                name: name!,
                url: url!,
                encoding,
            });

            db.appendJobLog(
                null,
                `[lifecycle] config_created name=${output.name} url=${output.url} encoding=${output.encoding || 'null'}`,
                pid,
                output.id,
                'lifecycle.config_created',
                { name: output.name, url: output.url, encoding: output.encoding || null },
            );

            return res.status(201).json({ message: 'Output created', output });
        } catch (err) {
            return res.status(400).json({ error: errMsg(err) });
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

            const running = db.getRunningJobFor(pid, oid);
            const { name, url, encoding, urlChanged, encodingChanged } = normalizeOutputPayload(
                req.body as Record<string, unknown>,
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
                return res.status(validationError.status).json({ error: validationError.error });
            }

            const updated = db.updateOutput(pid, oid, { name: name!, url: url!, encoding });
            if (!updated) return res.status(500).json({ error: 'Failed to update output' });

            logOutputConfigChanges(pid, oid, existing, updated);

            return res.json({ message: 'Output updated', output: updated });
        } catch (err) {
            return res.status(400).json({ error: errMsg(err) });
        }
    });

    app.delete('/pipelines/:pipelineId/outputs/:outputId', async (req, res) => {
        try {
            const pid = req.params.pipelineId;
            const oid = req.params.outputId;
            const pipeline = db.getPipeline(pid);
            if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });

            const existing = db.getOutput(pid, oid);
            if (!existing) return res.status(404).json({ error: 'Output not found' });

            const running = db.getRunningJobFor(pid, oid);
            if (running) {
                const stopResult = await stopRunningJobAndWait(running);
                if (!stopResult.stopped || !stopResult.completed) {
                    return res.status(409).json({
                        error: 'Failed to stop output before delete',
                        result: stopResult,
                    });
                }
            }

            const ok = db.deleteOutput(pid, oid);
            if (!ok) return res.status(500).json({ error: 'Failed to delete output' });

            clearOutputRestartState(pid, oid);
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
                });
            }

            if (reconciliation.action === 'start_in_progress') {
                return res.status(409).json({ error: 'Start already in progress for this output' });
            }

            return res
                .status(200)
                .json({ message: 'Output desired state set to running', desiredState: 'running' });
        } catch (err) {
            const e = err as { status?: number; publicError?: string; detail?: string };
            const status = Number(e?.status || 500);
            return res.status(status).json({
                error: e?.publicError || errMsg(err),
                ...(e?.detail && { detail: e.detail }),
            });
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
                    limit: requestedLimit ?? null,
                    order: order as 'asc' | 'desc',
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
                    order: order as 'asc' | 'desc',
                    prefixes,
                });
            }

            return res.json({ pipelineId: pid, outputId: oid, logs });
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
