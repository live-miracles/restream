import { getOutputHistory, getPipelineHistory } from '../core/api.js';
import {
    historyConstants,
    outputHistoryState,
    pipelineHistoryState,
} from './state.js';
import {
    focusOutputHistoryRawMatch,
    getMatchingRawOutputLogs,
    getOutputHistoryContextKey,
    getTimelineContextRange,
    renderOutputHistory as renderOutputHistoryView,
    renderPipelineHistory as renderPipelineHistoryView,
    setHistoryRenderCallbacks,
} from './render.js';

const {
    OUTPUT_HISTORY_POLL_INTERVAL_MS,
    OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS,
    OUTPUT_HISTORY_RAW_LIMIT,
    OUTPUT_HISTORY_CONTEXT_LIMIT,
} = historyConstants;

    async function ensureOutputHistoryContext(log) {
        // Timeline rows only fetch nearby stderr/exit/control logs on demand so the main history
        // request can stay small while still letting users drill into a suspicious event.
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

        const res = await getOutputHistory(
            outputHistoryState.pipelineId,
            outputHistoryState.outputId,
            {
                since: range.since,
                until: range.until,
                order: 'asc',
                limit: OUTPUT_HISTORY_CONTEXT_LIMIT,
                prefixes: ['stderr', 'exit', 'control'],
            },
        );

        outputHistoryState.contextLoadingKeys.delete(contextKey);
        outputHistoryState.contextLogsByKey.set(
            contextKey,
            Array.isArray(res?.logs) ? res.logs : [],
        );
        renderOutputHistory(false);
    }

    function toggleOutputHistoryContext(log) {
        const contextKey = getOutputHistoryContextKey(log);
        if (!contextKey) return;
        if (outputHistoryState.expandedContextKeys.has(contextKey)) {
            outputHistoryState.expandedContextKeys.delete(contextKey);
        } else {
            outputHistoryState.expandedContextKeys.add(contextKey);
            ensureOutputHistoryContext(log);
        }
        renderOutputHistory(false, contextKey);
    }

    function setOutputHistorySearch(query) {
        outputHistoryState.rawQuery = String(query || '');
        outputHistoryState.rawMatchIndex = 0;
        renderOutputHistory(true);
    }

    function onOutputHistorySearchKeydown(event) {
        if (!event || event.key !== 'Enter') return;
        event.preventDefault();
        navigateOutputHistorySearch(event.shiftKey ? -1 : 1);
    }

    function navigateOutputHistorySearch(direction) {
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

    function setOutputHistoryOrder(order) {
        const nextOrder = order === 'asc' ? 'asc' : 'desc';
        if (outputHistoryState.order === nextOrder) return;
        outputHistoryState.order = nextOrder;
        renderOutputHistory(true);
    }

    function renderOutputHistory(scrollToTop = false, anchorContextKey = null) {
        renderOutputHistoryView(outputHistoryState, historyConstants, {
            scrollToTop,
            anchorContextKey,
        });
    }

    function setOutputHistoryMode(mode) {
        const newMode = mode === 'raw' ? 'raw' : 'timeline';
        if (outputHistoryState.mode === newMode) return;
        outputHistoryState.mode = newMode;
        pollHistoryOnce(true);
    }

    function toggleHistoryRedaction() {
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

    function stopHistoryPoll() {
        if (outputHistoryState.pollTimer) {
            clearTimeout(outputHistoryState.pollTimer);
            outputHistoryState.pollTimer = null;
        }
        outputHistoryState.pollEveryMs = null;
        outputHistoryState.playing = false;
        updateHistoryPlayPauseBtn();
    }

    function startHistoryPolling(intervalMs) {
        if (outputHistoryState.pollTimer && outputHistoryState.pollEveryMs === intervalMs) return;
        if (outputHistoryState.pollTimer) clearTimeout(outputHistoryState.pollTimer);
        outputHistoryState.pollEveryMs = intervalMs;
        outputHistoryState.isPolling = false;
        const pollWithGuard = async () => {
            // setTimeout plus isPolling avoids overlapping requests when a fetch takes longer than
            // the nominal interval, which is safer here than stacking setInterval callbacks.
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

    function updateHistoryPlayPauseBtn() {
        const btn = document.getElementById('output-history-playpause');
        if (!btn) return;
        btn.textContent = outputHistoryState.playing ? '⏸ Pause' : '▶ Live';
        btn.classList.toggle('btn-accent', outputHistoryState.playing);
        btn.classList.toggle('btn-outline', !outputHistoryState.playing);
    }

    async function pollHistoryOnce(scrollToTop = false) {
        const { pipelineId, outputId, mode } = outputHistoryState;
        if (!pipelineId || !outputId) return;
        if (mode === 'timeline') {
            const lifecycleRes = await getOutputHistory(pipelineId, outputId, {
                filter: 'lifecycle',
            });
            if (lifecycleRes === null) return;
            outputHistoryState.lifecycleLogs = Array.isArray(lifecycleRes.logs)
                ? lifecycleRes.logs
                : [];
        } else {
            const rawRes = await getOutputHistory(pipelineId, outputId, {
                limit: OUTPUT_HISTORY_RAW_LIMIT,
            });
            if (rawRes === null) return;
            outputHistoryState.rawLogs = Array.isArray(rawRes.logs) ? rawRes.logs : [];
        }
        renderOutputHistory(scrollToTop);
    }

    function toggleHistoryPlayPause() {
        // “Live” mode is just polling plus an immediate fetch; pausing stops both the timer and
        // the button state so the modal can stay open without consuming background requests.
        if (outputHistoryState.playing) {
            stopHistoryPoll();
        } else {
            outputHistoryState.playing = true;
            updateHistoryPlayPauseBtn();
            pollHistoryOnce();
            startHistoryPolling(
                document.hidden
                    ? OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS
                    : OUTPUT_HISTORY_POLL_INTERVAL_MS,
            );
        }
    }

    async function openOutputHistoryModal(pipeId, outId, outName = '') {
        const modal = document.getElementById('output-history-modal');
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
            ? lifecycleRes.logs
            : [];
        renderOutputHistory(true);
        modal.addEventListener('close', stopHistoryPoll, { once: true });
    }

    function renderPipelineHistory(scrollToTop = false) {
        renderPipelineHistoryView(pipelineHistoryState, { scrollToTop });
    }

    function stopPipelineHistoryPoll() {
        if (pipelineHistoryState.pollTimer) {
            clearTimeout(pipelineHistoryState.pollTimer);
            pipelineHistoryState.pollTimer = null;
        }
        pipelineHistoryState.pollEveryMs = null;
        pipelineHistoryState.isPolling = false;
        pipelineHistoryState.playing = false;
        updatePipelineHistoryPlayPauseBtn();
    }

    function startPipelineHistoryPolling(intervalMs) {
        if (pipelineHistoryState.pollTimer && pipelineHistoryState.pollEveryMs === intervalMs)
            return;
        if (pipelineHistoryState.pollTimer) clearTimeout(pipelineHistoryState.pollTimer);
        pipelineHistoryState.pollEveryMs = intervalMs;

        const scheduleNextPoll = () => {
            if (!pipelineHistoryState.playing || pipelineHistoryState.pollEveryMs !== intervalMs) {
                return;
            }
            pipelineHistoryState.pollTimer = setTimeout(pollWithGuard, intervalMs);
        };

        const pollWithGuard = async () => {
            if (!pipelineHistoryState.playing || pipelineHistoryState.pollEveryMs !== intervalMs) {
                return;
            }

            await pollPipelineHistoryOnce();
            scheduleNextPoll();
        };

        scheduleNextPoll();
    }

    function updatePipelineHistoryPlayPauseBtn() {
        const btn = document.getElementById('pipeline-history-playpause');
        if (!btn) return;
        btn.textContent = pipelineHistoryState.playing ? '⏸ Pause' : '▶ Live';
        btn.classList.toggle('btn-accent', pipelineHistoryState.playing);
        btn.classList.toggle('btn-outline', !pipelineHistoryState.playing);
    }

    async function pollPipelineHistoryOnce() {
        const { pipelineId } = pipelineHistoryState;
        if (!pipelineId || pipelineHistoryState.isPolling) return;

        pipelineHistoryState.isPolling = true;
        try {
            const res = await getPipelineHistory(pipelineId, 200);
            if (res === null) return;
            pipelineHistoryState.logs = Array.isArray(res.logs) ? res.logs : [];
            renderPipelineHistory(false);
        } finally {
            pipelineHistoryState.isPolling = false;
        }
    }

    function togglePipelineHistoryPlayPause() {
        if (pipelineHistoryState.playing) {
            stopPipelineHistoryPoll();
        } else {
            pipelineHistoryState.playing = true;
            updatePipelineHistoryPlayPauseBtn();
            pollPipelineHistoryOnce();
            startPipelineHistoryPolling(
                document.hidden
                    ? OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS
                    : OUTPUT_HISTORY_POLL_INTERVAL_MS,
            );
        }
    }

    async function openPipelineHistoryModal(pipeId, pipeName = '') {
        const modal = document.getElementById('pipeline-history-modal');
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

        pipelineHistoryState.logs = Array.isArray(res.logs) ? res.logs : [];
        renderPipelineHistory(true);
        modal.addEventListener('close', stopPipelineHistoryPoll, { once: true });
    }

    async function syncHistoryPollingWithVisibility() {
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

export {
    openOutputHistoryModal,
    openPipelineHistoryModal,
    syncHistoryPollingWithVisibility,
    toggleHistoryPlayPause,
    toggleHistoryRedaction,
    setOutputHistoryMode,
    setOutputHistoryOrder,
    setOutputHistorySearch,
    onOutputHistorySearchKeydown,
    navigateOutputHistorySearch,
    togglePipelineHistoryPlayPause,
};
