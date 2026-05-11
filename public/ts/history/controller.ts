import { getOutputHistory, getPipelineHistory } from '../core/api.js';
import { historyConstants, outputHistoryState, pipelineHistoryState } from './state.js';
import {
    focusOutputHistoryRawMatch,
    getMatchingRawOutputLogs,
    getOutputHistoryContextKey,
    getTimelineContextRange,
    renderOutputHistory as renderOutputHistoryView,
    renderPipelineHistory as renderPipelineHistoryView,
    setHistoryRenderCallbacks,
} from './render.js';
import type { HistoryLog } from '../types.js';

const {
    OUTPUT_HISTORY_POLL_INTERVAL_MS,
    OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS,
    OUTPUT_HISTORY_RAW_LIMIT,
    OUTPUT_HISTORY_CONTEXT_LIMIT,
} = historyConstants;

async function ensureOutputHistoryContext(log: HistoryLog): Promise<void> {
    const contextKey = getOutputHistoryContextKey(log);
    if (
        !contextKey ||
        outputHistoryState.contextLoadingKeys.has(contextKey) ||
        outputHistoryState.contextLogsByKey.has(contextKey)
    ) {
        return;
    }

    const range = getTimelineContextRange(outputHistoryState, historyConstants, log);
    if (!range) {
        outputHistoryState.contextLogsByKey.set(contextKey, []);
        return;
    }

    outputHistoryState.contextLoadingKeys.add(contextKey);
    renderOutputHistory(false);

    const res = await getOutputHistory(outputHistoryState.pipelineId!, outputHistoryState.outputId!, {
        since: range.since,
        until: range.until,
        order: 'asc',
        limit: OUTPUT_HISTORY_CONTEXT_LIMIT,
        prefixes: ['stderr', 'exit', 'control'],
    });

    outputHistoryState.contextLoadingKeys.delete(contextKey);
    outputHistoryState.contextLogsByKey.set(
        contextKey,
        Array.isArray(res?.logs) ? (res.logs as HistoryLog[]) : [],
    );
    renderOutputHistory(false);
}

function toggleOutputHistoryContext(log: HistoryLog): void {
    const contextKey = getOutputHistoryContextKey(log);
    if (!contextKey) return;
    if (outputHistoryState.expandedContextKeys.has(contextKey)) {
        outputHistoryState.expandedContextKeys.delete(contextKey);
    } else {
        outputHistoryState.expandedContextKeys.add(contextKey);
        void ensureOutputHistoryContext(log);
    }
    renderOutputHistory(false, contextKey);
}

export function setOutputHistorySearch(query: string): void {
    outputHistoryState.rawQuery = String(query || '');
    outputHistoryState.rawMatchIndex = 0;
    renderOutputHistory(true);
}

export function onOutputHistorySearchKeydown(event: KeyboardEvent): void {
    if (!event || event.key !== 'Enter') return;
    event.preventDefault();
    navigateOutputHistorySearch(event.shiftKey ? -1 : 1);
}

export function navigateOutputHistorySearch(direction: number): void {
    if (outputHistoryState.mode !== 'raw') return;
    const matchingLogs = getMatchingRawOutputLogs(outputHistoryState);
    if (matchingLogs.length === 0) return;

    const count = matchingLogs.length;
    const current = Number.isInteger(outputHistoryState.rawMatchIndex)
        ? outputHistoryState.rawMatchIndex
        : 0;
    const next = (current + direction + count) % count;
    outputHistoryState.rawMatchIndex = next;
    renderOutputHistory(false);
    focusOutputHistoryRawMatch(outputHistoryState);
}

export function setOutputHistoryOrder(order: string): void {
    const nextOrder = order === 'asc' ? 'asc' : ('desc' as const);
    if (outputHistoryState.order === nextOrder) return;
    outputHistoryState.order = nextOrder;
    renderOutputHistory(true);
}

function renderOutputHistory(scrollToTop = false, anchorContextKey: string | null = null): void {
    renderOutputHistoryView(outputHistoryState, historyConstants, {
        scrollToTop,
        anchorContextKey,
    });
}

export function setOutputHistoryMode(mode: string): void {
    const newMode = mode === 'raw' ? 'raw' : ('timeline' as const);
    if (outputHistoryState.mode === newMode) return;
    outputHistoryState.mode = newMode;
    void pollHistoryOnce(true);
}

export function toggleHistoryRedaction(): void {
    outputHistoryState.redacted = !outputHistoryState.redacted;
    const btn = document.getElementById('output-history-redact');
    if (btn) {
        const label = outputHistoryState.redacted ? 'Show URLs' : 'Hide URLs';
        btn.title = label;
        btn.setAttribute('aria-label', label);
        btn.classList.toggle('btn-outline', outputHistoryState.redacted);
        btn.classList.toggle('btn-warning', !outputHistoryState.redacted);
    }
    renderOutputHistory(false);
}

function stopHistoryPoll(): void {
    if (outputHistoryState.pollTimer) {
        clearTimeout(outputHistoryState.pollTimer);
        outputHistoryState.pollTimer = null;
    }
    outputHistoryState.pollEveryMs = null;
    outputHistoryState.playing = false;
    updateHistoryPlayPauseBtn();
}

function startHistoryPolling(intervalMs: number): void {
    if (outputHistoryState.pollTimer && outputHistoryState.pollEveryMs === intervalMs) return;
    if (outputHistoryState.pollTimer) clearTimeout(outputHistoryState.pollTimer);
    outputHistoryState.pollEveryMs = intervalMs;
    outputHistoryState.isPolling = false;
    const pollWithGuard = async (): Promise<void> => {
        if (outputHistoryState.isPolling) return;
        outputHistoryState.isPolling = true;
        try {
            await pollHistoryOnce();
        } finally {
            outputHistoryState.isPolling = false;
        }
        outputHistoryState.pollTimer = setTimeout(pollWithGuard, intervalMs);
    };
    outputHistoryState.pollTimer = setTimeout(pollWithGuard, intervalMs);
}

function updateHistoryPlayPauseBtn(): void {
    const btn = document.getElementById('output-history-playpause');
    if (!btn) return;
    btn.textContent = outputHistoryState.playing ? '⏸ Pause' : '▶ Live';
    btn.classList.toggle('btn-accent', outputHistoryState.playing);
    btn.classList.toggle('btn-outline', !outputHistoryState.playing);
}

async function pollHistoryOnce(scrollToTop = false): Promise<void> {
    const { pipelineId, outputId, mode } = outputHistoryState;
    if (!pipelineId || !outputId) return;
    if (mode === 'timeline') {
        const lifecycleRes = await getOutputHistory(pipelineId, outputId, {
            filter: 'lifecycle',
        });
        if (lifecycleRes === null) return;
        outputHistoryState.lifecycleLogs = Array.isArray(lifecycleRes.logs)
            ? (lifecycleRes.logs as HistoryLog[])
            : [];
    } else {
        const rawRes = await getOutputHistory(pipelineId, outputId, {
            limit: OUTPUT_HISTORY_RAW_LIMIT,
        });
        if (rawRes === null) return;
        outputHistoryState.rawLogs = Array.isArray(rawRes.logs)
            ? (rawRes.logs as HistoryLog[])
            : [];
    }
    renderOutputHistory(scrollToTop);
}

export function toggleHistoryPlayPause(): void {
    if (outputHistoryState.playing) {
        stopHistoryPoll();
    } else {
        outputHistoryState.playing = true;
        updateHistoryPlayPauseBtn();
        void pollHistoryOnce();
        startHistoryPolling(
            document.hidden
                ? OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS
                : OUTPUT_HISTORY_POLL_INTERVAL_MS,
        );
    }
}

export async function openOutputHistoryModal(
    pipeId: string,
    outId: string,
    outName = '',
): Promise<void> {
    const modal = document.getElementById('output-history-modal') as HTMLDialogElement | null;
    const title = document.getElementById('output-history-title');
    const loading = document.getElementById('output-history-loading');

    if (!modal || !title || !loading) return;

    stopHistoryPoll();

    outputHistoryState.pipelineId = pipeId;
    outputHistoryState.outputId = outId;
    outputHistoryState.outputName = outName || outId;
    outputHistoryState.mode = 'timeline';
    outputHistoryState.order = 'desc';
    outputHistoryState.lifecycleLogs = [];
    outputHistoryState.rawLogs = [];
    outputHistoryState.rawQuery = '';
    outputHistoryState.rawMatchIndex = 0;
    outputHistoryState.expandedContextKeys = new Set();
    outputHistoryState.contextLogsByKey = new Map();
    outputHistoryState.contextLoadingKeys = new Set();
    outputHistoryState.redacted = true;

    title.textContent = `History: ${outputHistoryState.outputName}`;
    updateHistoryPlayPauseBtn();
    const redactBtn = document.getElementById('output-history-redact');
    if (redactBtn) {
        redactBtn.title = 'Show URLs';
        redactBtn.classList.add('btn-outline');
        redactBtn.classList.remove('btn-warning');
    }
    loading.classList.remove('hidden');
    renderOutputHistory();
    modal.showModal();

    const lifecycleRes = await getOutputHistory(pipeId, outId, { filter: 'lifecycle' });
    loading.classList.add('hidden');
    if (lifecycleRes === null) return;

    outputHistoryState.lifecycleLogs = Array.isArray(lifecycleRes.logs)
        ? (lifecycleRes.logs as HistoryLog[])
        : [];
    renderOutputHistory(true);
    modal.addEventListener('close', stopHistoryPoll, { once: true });
}

function renderPipelineHistory(scrollToTop = false): void {
    renderPipelineHistoryView(pipelineHistoryState, { scrollToTop });
}

function stopPipelineHistoryPoll(): void {
    if (pipelineHistoryState.pollTimer) {
        clearTimeout(pipelineHistoryState.pollTimer);
        pipelineHistoryState.pollTimer = null;
    }
    pipelineHistoryState.pollEveryMs = null;
    pipelineHistoryState.isPolling = false;
    pipelineHistoryState.playing = false;
    updatePipelineHistoryPlayPauseBtn();
}

function startPipelineHistoryPolling(intervalMs: number): void {
    if (pipelineHistoryState.pollTimer && pipelineHistoryState.pollEveryMs === intervalMs) return;
    if (pipelineHistoryState.pollTimer) clearTimeout(pipelineHistoryState.pollTimer);
    pipelineHistoryState.pollEveryMs = intervalMs;

    const scheduleNextPoll = (): void => {
        if (!pipelineHistoryState.playing || pipelineHistoryState.pollEveryMs !== intervalMs) {
            return;
        }
        pipelineHistoryState.pollTimer = setTimeout(pollWithGuard, intervalMs);
    };

    const pollWithGuard = async (): Promise<void> => {
        if (!pipelineHistoryState.playing || pipelineHistoryState.pollEveryMs !== intervalMs) {
            return;
        }

        await pollPipelineHistoryOnce();
        scheduleNextPoll();
    };

    scheduleNextPoll();
}

function updatePipelineHistoryPlayPauseBtn(): void {
    const btn = document.getElementById('pipeline-history-playpause');
    if (!btn) return;
    btn.textContent = pipelineHistoryState.playing ? '⏸ Pause' : '▶ Live';
    btn.classList.toggle('btn-accent', pipelineHistoryState.playing);
    btn.classList.toggle('btn-outline', !pipelineHistoryState.playing);
}

async function pollPipelineHistoryOnce(): Promise<void> {
    const { pipelineId } = pipelineHistoryState;
    if (!pipelineId || pipelineHistoryState.isPolling) return;

    pipelineHistoryState.isPolling = true;
    try {
        const res = await getPipelineHistory(pipelineId, 200);
        if (res === null) return;
        pipelineHistoryState.logs = Array.isArray(res.logs) ? (res.logs as HistoryLog[]) : [];
        renderPipelineHistory(false);
    } finally {
        pipelineHistoryState.isPolling = false;
    }
}

export function togglePipelineHistoryPlayPause(): void {
    if (pipelineHistoryState.playing) {
        stopPipelineHistoryPoll();
    } else {
        pipelineHistoryState.playing = true;
        updatePipelineHistoryPlayPauseBtn();
        void pollPipelineHistoryOnce();
        startPipelineHistoryPolling(
            document.hidden
                ? OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS
                : OUTPUT_HISTORY_POLL_INTERVAL_MS,
        );
    }
}

export async function openPipelineHistoryModal(
    pipeId: string,
    pipeName = '',
): Promise<void> {
    const modal = document.getElementById('pipeline-history-modal') as HTMLDialogElement | null;
    const title = document.getElementById('pipeline-history-title');
    const loading = document.getElementById('pipeline-history-loading');

    if (!modal || !title || !loading) return;

    stopPipelineHistoryPoll();

    pipelineHistoryState.pipelineId = pipeId;
    pipelineHistoryState.pipelineName = pipeName || pipeId;
    pipelineHistoryState.logs = [];

    title.textContent = `Pipeline History: ${pipelineHistoryState.pipelineName}`;
    updatePipelineHistoryPlayPauseBtn();
    loading.classList.remove('hidden');
    renderPipelineHistory();
    modal.showModal();

    const res = await getPipelineHistory(pipeId, 200);
    loading.classList.add('hidden');
    if (res === null) return;

    pipelineHistoryState.logs = Array.isArray(res.logs) ? (res.logs as HistoryLog[]) : [];
    renderPipelineHistory(true);
    modal.addEventListener('close', stopPipelineHistoryPoll, { once: true });
}

export async function syncHistoryPollingWithVisibility(): Promise<void> {
    if (document.hidden) {
        if (outputHistoryState.playing) {
            startHistoryPolling(OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS);
        }
        if (pipelineHistoryState.playing) {
            startPipelineHistoryPolling(OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS);
        }
        return;
    }

    if (outputHistoryState.playing) {
        startHistoryPolling(OUTPUT_HISTORY_POLL_INTERVAL_MS);
        await pollHistoryOnce();
    }
    if (pipelineHistoryState.playing) {
        startPipelineHistoryPolling(OUTPUT_HISTORY_POLL_INTERVAL_MS);
        await pollPipelineHistoryOnce();
    }
}

setHistoryRenderCallbacks({
    toggleOutputHistoryContext,
});

window.toggleHistoryPlayPause = toggleHistoryPlayPause;
window.toggleHistoryRedaction = toggleHistoryRedaction;
window.setOutputHistoryMode = setOutputHistoryMode;
window.setOutputHistoryOrder = setOutputHistoryOrder;
window.setOutputHistorySearch = setOutputHistorySearch;
window.onOutputHistorySearchKeydown = onOutputHistorySearchKeydown;
window.navigateOutputHistorySearch = navigateOutputHistorySearch;
window.togglePipelineHistoryPlayPause = togglePipelineHistoryPlayPause;
