// Output history page controller.
// Fetches and renders per-output and per-pipeline job history, classifies log events
// for display, and drives the auto-refresh poll loop for the history view.

import { getOutputHistory, getPipelineHistory, createAdaptivePollLoop } from './client.js';
import { sanitizeLogMessage } from './utils.js';
import { registerDashboardVisibilitySync } from './features/dashboard-actions.js';
import {
    classifyHistoryEvent,
    classifyPipelineHistoryEvent,
    formatHistoryTime,
    getFilteredRawOutputLogs,
    getMatchingRawOutputLogs,
    getOrderedOutputLogs,
    getOutputHistoryContextKey,
    getPipelineTimelineLogs,
    getRawHistorySearchValue,
    getTimelineContextLogs,
    getTimelineContextRange,
} from './history/classify.mjs';

// Modal state is kept outside render functions so polling, search, and lazy-loaded context can
// update incrementally without rebuilding the whole feature around DOM state.
const outputHistoryState = {
    pipelineId: null,
    outputId: null,
    outputName: '',
    mode: 'timeline',
    order: 'desc',
    lifecycleLogs: [],
    rawLogs: [],
    rawQuery: '',
    rawMatchIndex: 0,
    expandedContextKeys: new Set(),
    contextLogsByKey: new Map(),
    contextLoadingKeys: new Set(),
    redacted: true,
    playing: false,
};

const pipelineHistoryState = {
    pipelineId: null,
    pipelineName: '',
    logs: [],
    playing: false,
};

// Hidden tabs back off polling to reduce network churn while still keeping history views fresh
// when the user returns.
const historyConstants = {
    OUTPUT_HISTORY_POLL_INTERVAL_MS: 5000,
    OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS: 30000,
    OUTPUT_HISTORY_RAW_LIMIT: 1000,
    OUTPUT_HISTORY_CONTEXT_WINDOW_MS: 5 * 60 * 1000,
    OUTPUT_HISTORY_CONTEXT_LIMIT: 50,
};

function renderHighlightedLogMessage(container, text, query) {
    container.replaceChildren();
    if (!query) {
        container.textContent = text;
        return;
    }

    const source = String(text || '');
    const lowerSource = source.toLowerCase();
    const needle = String(query || '').toLowerCase();
    if (!needle) {
        container.textContent = source;
        return;
    }

    let cursor = 0;
    while (cursor < source.length) {
        const idx = lowerSource.indexOf(needle, cursor);
        if (idx < 0) {
            container.appendChild(document.createTextNode(source.slice(cursor)));
            break;
        }

        if (idx > cursor) {
            container.appendChild(document.createTextNode(source.slice(cursor, idx)));
        }

        const mark = document.createElement('mark');
        mark.className = 'rounded bg-amber-400 px-0.5 text-gray-900';
        mark.textContent = source.slice(idx, idx + needle.length);
        container.appendChild(mark);

        cursor = idx + needle.length;
    }
}

function focusOutputHistoryRawMatch(state) {
    const list = document.getElementById('output-history-list');
    if (!list) return;
    const target = list.querySelector(`[data-raw-match-index="${state.rawMatchIndex}"]`);
    if (!target) return;
    target.scrollIntoView({ block: 'nearest' });
}

function renderOutputHistoryView(
    state,
    constants,
    { scrollToTop = false, anchorContextKey = null, onToggleContext = null } = {},
) {
    const list = document.getElementById('output-history-list');
    const empty = document.getElementById('output-history-empty');
    const searchWrap = document.getElementById('output-history-search-wrap');
    const searchInput = document.getElementById('output-history-search');
    const searchStatus = document.getElementById('output-history-search-status');
    const searchPrevBtn = document.getElementById('output-history-search-prev');
    const searchNextBtn = document.getElementById('output-history-search-next');
    const timelineBtn = document.getElementById('output-history-mode-timeline');
    const rawBtn = document.getElementById('output-history-mode-raw');
    const newestBtn = document.getElementById('output-history-order-newest');
    const oldestBtn = document.getElementById('output-history-order-oldest');

    if (
        !list ||
        !empty ||
        !timelineBtn ||
        !rawBtn ||
        !newestBtn ||
        !oldestBtn ||
        !searchWrap ||
        !searchInput ||
        !searchStatus ||
        !searchPrevBtn ||
        !searchNextBtn
    )
        return;

    const mode = state.mode;
    timelineBtn.classList.toggle('btn-accent', mode === 'timeline');
    timelineBtn.classList.toggle('btn-outline', mode !== 'timeline');
    rawBtn.classList.toggle('btn-accent', mode === 'raw');
    rawBtn.classList.toggle('btn-outline', mode !== 'raw');

    const newestFirst = state.order === 'desc';
    newestBtn.classList.toggle('btn-accent', newestFirst);
    newestBtn.classList.toggle('btn-outline', !newestFirst);
    oldestBtn.classList.toggle('btn-accent', !newestFirst);
    oldestBtn.classList.toggle('btn-outline', newestFirst);

    searchWrap.classList.toggle('hidden', mode !== 'raw');
    if (searchInput.value !== state.rawQuery) {
        searchInput.value = state.rawQuery;
    }

    const rawMatchingLogs = mode === 'raw' ? getMatchingRawOutputLogs(state) : [];
    const hasSearchQuery = getRawHistorySearchValue(state).length > 0;
    if (mode === 'raw' && hasSearchQuery && rawMatchingLogs.length > 0) {
        if (state.rawMatchIndex < 0 || state.rawMatchIndex >= rawMatchingLogs.length) {
            state.rawMatchIndex = 0;
        }
        searchStatus.textContent = `${state.rawMatchIndex + 1}/${rawMatchingLogs.length}`;
    } else if (mode === 'raw' && hasSearchQuery) {
        searchStatus.textContent = '0/0';
    } else {
        searchStatus.textContent = '';
    }

    const canNavigateMatches = mode === 'raw' && hasSearchQuery && rawMatchingLogs.length > 0;
    searchPrevBtn.disabled = !canNavigateMatches;
    searchNextBtn.disabled = !canNavigateMatches;

    list.replaceChildren();

    const hasLogs =
        mode === 'raw'
            ? Array.isArray(state.rawLogs) && state.rawLogs.length > 0
            : Array.isArray(state.lifecycleLogs) && state.lifecycleLogs.length > 0;

    if (!hasLogs) {
        empty.classList.remove('hidden');
        return;
    }

    empty.classList.add('hidden');

    if (mode === 'raw') {
        const rawLogs = getFilteredRawOutputLogs(state);
        empty.textContent = 'No history available yet.';
        const hasQuery = hasSearchQuery;
        let matchCounter = 0;
        for (let i = 0; i < rawLogs.length; i += 1) {
            const log = rawLogs[i];
            const haystack = `${log?.ts || ''}\n${log?.message || ''}`.toLowerCase();
            const isMatch = hasQuery && haystack.includes(getRawHistorySearchValue(state));
            const matchIndex = isMatch ? matchCounter++ : -1;
            const row = document.createElement('div');
            row.className = 'rounded border border-transparent bg-base-100 p-2';
            if (isMatch) {
                row.dataset.rawMatchIndex = String(matchIndex);
                if (matchIndex === state.rawMatchIndex) {
                    row.classList.remove('border-transparent');
                    row.classList.add('border-success');
                }
            }

            const header = document.createElement('div');
            header.className = 'flex items-center justify-between gap-2';

            const label = document.createElement('span');
            label.className = 'badge badge-sm badge-ghost';
            label.textContent = 'Log';

            const ts = document.createElement('span');
            ts.className = 'text-xs opacity-70';
            ts.textContent = formatHistoryTime(log.ts);

            header.appendChild(label);
            header.appendChild(ts);

            const msg = document.createElement('pre');
            msg.className = 'mt-1 text-xs whitespace-pre-wrap break-words';
            const safeMessage = sanitizeLogMessage(log.message || '', state.redacted);
            renderHighlightedLogMessage(
                msg,
                safeMessage,
                hasQuery ? getRawHistorySearchValue(state) : '',
            );

            row.appendChild(header);
            row.appendChild(msg);
            list.appendChild(row);
        }
        if (scrollToTop) list.scrollTop = 0;
        return;
    }

    empty.textContent = 'No history available yet.';

    const timelineLogs = getOrderedOutputLogs(state.lifecycleLogs, state.order);
    timelineLogs.forEach((log, index) => {
        const event = classifyHistoryEvent(log, timelineLogs, index);
        const contextLogs = getTimelineContextLogs(state, log);
        const contextKey = getOutputHistoryContextKey(log);
        const expanded = state.expandedContextKeys.has(contextKey);
        const contextLoading = state.contextLoadingKeys.has(contextKey);

        const row = document.createElement('div');
        row.className = 'rounded bg-base-100 p-2';
        if (contextKey) row.dataset.contextKey = contextKey;

        const header = document.createElement('div');
        header.className = 'flex items-center justify-between gap-2';

        const left = document.createElement('div');
        left.className = 'flex items-center gap-2';

        const badge = document.createElement('span');
        badge.className = `badge badge-sm ${event.badgeClass}`;
        badge.textContent = event.label;

        const toggle = document.createElement('button');
        toggle.type = 'button';
        toggle.className = 'btn btn-ghost btn-xs btn-square text-lg leading-none';
        if (contextLoading) {
            toggle.textContent = '…';
            toggle.disabled = true;
        } else {
            toggle.textContent = expanded ? '▾' : '▸';
        }
        toggle.title = expanded ? 'Hide context' : 'Show context';
        toggle.setAttribute('aria-label', expanded ? 'Hide context' : 'Show context');
        toggle.onclick = () => onToggleContext?.(log);
        left.appendChild(toggle);

        left.appendChild(badge);

        const ts = document.createElement('span');
        ts.className = 'text-xs opacity-70';
        ts.textContent = formatHistoryTime(log.ts);

        header.appendChild(left);
        header.appendChild(ts);

        const details = document.createElement('pre');
        details.className = 'mt-1 text-xs whitespace-pre-wrap break-words';
        details.textContent = sanitizeLogMessage(log.message || '', state.redacted);

        row.appendChild(header);
        row.appendChild(details);

        if (expanded) {
            const contextBox = document.createElement('div');
            contextBox.className = 'mt-2 rounded border border-base-300 bg-base-200 p-2';

            const contextTitle = document.createElement('div');
            contextTitle.className = 'mb-2 text-xs font-medium opacity-70';
            contextTitle.textContent = `stderr / exit / control before event (${contextLogs.length})`;
            contextBox.appendChild(contextTitle);

            if (contextLoading) {
                const loadingRow = document.createElement('div');
                loadingRow.className = 'text-xs opacity-70';
                loadingRow.textContent = 'Loading context...';
                contextBox.appendChild(loadingRow);
            } else if (contextLogs.length === 0) {
                const emptyRow = document.createElement('div');
                emptyRow.className = 'text-xs opacity-70';
                emptyRow.textContent =
                    'No stderr, exit, or control logs in the bounded window before this event.';
                contextBox.appendChild(emptyRow);
            } else {
                const orderedContextLogs = getOrderedOutputLogs(contextLogs, state.order);
                for (const contextLog of orderedContextLogs) {
                    const contextRow = document.createElement('div');
                    contextRow.className = 'mb-2 last:mb-0';

                    const contextTs = document.createElement('div');
                    contextTs.className = 'text-[11px] opacity-60';
                    contextTs.textContent = formatHistoryTime(contextLog.ts);

                    const contextMsg = document.createElement('pre');
                    contextMsg.className = 'mt-1 text-xs whitespace-pre-wrap break-words';
                    contextMsg.textContent = sanitizeLogMessage(
                        contextLog.message || '',
                        state.redacted,
                    );

                    contextRow.appendChild(contextTs);
                    contextRow.appendChild(contextMsg);
                    contextBox.appendChild(contextRow);
                }
            }

            row.appendChild(contextBox);
        }

        list.appendChild(row);
    });

    if (anchorContextKey) {
        const target = list.querySelector(
            `[data-context-key="${CSS.escape(anchorContextKey)}"]`,
        );
        if (target) target.scrollIntoView({ block: 'nearest' });
    } else if (scrollToTop) {
        list.scrollTop = 0;
    }
}

function renderPipelineHistoryView(state, { scrollToTop = false } = {}) {
    const list = document.getElementById('pipeline-history-list');
    const empty = document.getElementById('pipeline-history-empty');

    if (!list || !empty) return;

    list.replaceChildren();

    if (!Array.isArray(state.logs) || state.logs.length === 0) {
        empty.classList.remove('hidden');
        return;
    }

    empty.classList.add('hidden');

    const logs = getPipelineTimelineLogs(state.logs);
    for (const log of logs) {
        const event = classifyPipelineHistoryEvent(log);

        const row = document.createElement('div');
        row.className = 'rounded bg-base-100 p-2';

        const header = document.createElement('div');
        header.className = 'flex items-center justify-between gap-2';

        const badge = document.createElement('span');
        badge.className = `badge badge-sm ${event.badgeClass}`;
        badge.textContent = event.label;

        const ts = document.createElement('span');
        ts.className = 'text-xs opacity-70';
        ts.textContent = formatHistoryTime(log.ts);

        header.appendChild(badge);
        header.appendChild(ts);

        const details = document.createElement('pre');
        details.className = 'mt-1 text-xs whitespace-pre-wrap break-words';
        details.textContent = String(log.message || '');

        row.appendChild(header);
        row.appendChild(details);
        list.appendChild(row);
    }

    if (scrollToTop) list.scrollTop = 0;
}

function createOutputHistoryContextController({
    outputHistoryState: stateArg,
    historyConstants: constantsArg,
    getOutputHistory: getOutputHistoryFn,
    renderOutputHistory: renderOutputHistoryFn,
    getOutputHistoryContextKey: getOutputHistoryContextKeyFn,
    getTimelineContextRange: getTimelineContextRangeFn,
}) {
    async function ensureOutputHistoryContext(log) {
        const contextKey = getOutputHistoryContextKeyFn(log);
        if (
            !contextKey ||
            stateArg.contextLoadingKeys.has(contextKey) ||
            stateArg.contextLogsByKey.has(contextKey)
        ) {
            return;
        }

        const range = getTimelineContextRangeFn(stateArg, constantsArg, log);
        if (!range) {
            stateArg.contextLogsByKey.set(contextKey, []);
            return;
        }

        stateArg.contextLoadingKeys.add(contextKey);
        renderOutputHistoryFn(false);

        const response = await getOutputHistoryFn(stateArg.pipelineId, stateArg.outputId, {
            since: range.since,
            until: range.until,
            order: 'asc',
            limit: constantsArg.OUTPUT_HISTORY_CONTEXT_LIMIT,
            prefixes: ['stderr', 'exit', 'control'],
        });

        stateArg.contextLoadingKeys.delete(contextKey);
        stateArg.contextLogsByKey.set(
            contextKey,
            Array.isArray(response?.logs) ? response.logs : [],
        );
        renderOutputHistoryFn(false);
    }

    function toggleOutputHistoryContext(log) {
        const contextKey = getOutputHistoryContextKeyFn(log);
        if (!contextKey) return;

        if (stateArg.expandedContextKeys.has(contextKey)) {
            stateArg.expandedContextKeys.delete(contextKey);
        } else {
            stateArg.expandedContextKeys.add(contextKey);
            void ensureOutputHistoryContext(log);
        }

        renderOutputHistoryFn(false, contextKey);
    }

    return { ensureOutputHistoryContext, toggleOutputHistoryContext };
}

function createHistoryPollingController({
    outputHistoryState: outState,
    pipelineHistoryState: pipeState,
    historyConstants: constants,
    getOutputHistory: getOutputHistoryFn,
    getPipelineHistory: getPipelineHistoryFn,
    renderOutputHistory: renderOutputHistoryFn,
    renderPipelineHistory: renderPipelineHistoryFn,
}) {
    const {
        OUTPUT_HISTORY_POLL_INTERVAL_MS,
        OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS,
        OUTPUT_HISTORY_RAW_LIMIT,
    } = constants;

    function updateHistoryPlayPauseBtn() {
        const button = document.getElementById('output-history-playpause');
        if (!button) return;
        button.textContent = outState.playing ? '⏸ Pause' : '▶ Live';
        button.classList.toggle('btn-accent', outState.playing);
        button.classList.toggle('btn-outline', !outState.playing);
    }

    async function pollHistoryOnce(scrollToTop = false) {
        const { pipelineId, outputId, mode } = outState;
        if (!pipelineId || !outputId) return;

        if (mode === 'timeline') {
            const lifecycleResponse = await getOutputHistoryFn(pipelineId, outputId, {
                filter: 'lifecycle',
            });
            if (lifecycleResponse === null) return;
            outState.lifecycleLogs = Array.isArray(lifecycleResponse.logs)
                ? lifecycleResponse.logs
                : [];
        } else {
            const rawResponse = await getOutputHistoryFn(pipelineId, outputId, {
                limit: OUTPUT_HISTORY_RAW_LIMIT,
            });
            if (rawResponse === null) return;
            outState.rawLogs = Array.isArray(rawResponse.logs) ? rawResponse.logs : [];
        }

        renderOutputHistoryFn(scrollToTop);
    }

    const outputHistoryPollLoop = createAdaptivePollLoop({
        run: () => pollHistoryOnce(),
        getVisibleInterval: () => OUTPUT_HISTORY_POLL_INTERVAL_MS,
        getHiddenInterval: () => OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS,
        isEnabled: () => outState.playing,
    });

    function stopHistoryPoll() {
        outputHistoryPollLoop.stop();
        outState.playing = false;
        updateHistoryPlayPauseBtn();
    }

    function toggleHistoryPlayPause() {
        if (outState.playing) {
            stopHistoryPoll();
            return;
        }

        outState.playing = true;
        updateHistoryPlayPauseBtn();
        outputHistoryPollLoop.start();
        void pollHistoryOnce();
    }

    function updatePipelineHistoryPlayPauseBtn() {
        const button = document.getElementById('pipeline-history-playpause');
        if (!button) return;
        button.textContent = pipeState.playing ? '⏸ Pause' : '▶ Live';
        button.classList.toggle('btn-accent', pipeState.playing);
        button.classList.toggle('btn-outline', !pipeState.playing);
    }

    async function pollPipelineHistoryOnce(scrollToTop = false) {
        const { pipelineId } = pipeState;
        if (!pipelineId) return;

        const response = await getPipelineHistoryFn(pipelineId, 200);
        if (response === null) return;

        pipeState.logs = Array.isArray(response.logs) ? response.logs : [];
        renderPipelineHistoryFn(scrollToTop);
    }

    const pipelineHistoryPollLoop = createAdaptivePollLoop({
        run: () => pollPipelineHistoryOnce(),
        getVisibleInterval: () => OUTPUT_HISTORY_POLL_INTERVAL_MS,
        getHiddenInterval: () => OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS,
        isEnabled: () => pipeState.playing,
    });

    function stopPipelineHistoryPoll() {
        pipelineHistoryPollLoop.stop();
        pipeState.playing = false;
        updatePipelineHistoryPlayPauseBtn();
    }

    function togglePipelineHistoryPlayPause() {
        if (pipeState.playing) {
            stopPipelineHistoryPoll();
            return;
        }

        pipeState.playing = true;
        updatePipelineHistoryPlayPauseBtn();
        pipelineHistoryPollLoop.start();
        void pollPipelineHistoryOnce();
    }

    async function syncHistoryPollingWithVisibility() {
        await Promise.all([
            outputHistoryPollLoop.syncWithVisibility({
                pollImmediatelyOnVisible: !document.hidden,
            }),
            pipelineHistoryPollLoop.syncWithVisibility({
                pollImmediatelyOnVisible: !document.hidden,
            }),
        ]);
    }

    return {
        pollHistoryOnce,
        pollPipelineHistoryOnce,
        stopHistoryPoll,
        stopPipelineHistoryPoll,
        syncHistoryPollingWithVisibility,
        toggleHistoryPlayPause,
        togglePipelineHistoryPlayPause,
        updateHistoryPlayPauseBtn,
        updatePipelineHistoryPlayPauseBtn,
    };
}

function createOutputHistorySearchController({
    outputHistoryState: stateArg,
    renderOutputHistory: renderOutputHistoryFn,
    focusOutputHistoryRawMatch: focusOutputHistoryRawMatchFn,
    getMatchingRawOutputLogs: getMatchingRawOutputLogsFn,
}) {
    function setOutputHistorySearch(query) {
        stateArg.rawQuery = String(query || '');
        stateArg.rawMatchIndex = 0;
        renderOutputHistoryFn(true);
    }

    function navigateOutputHistorySearch(direction) {
        if (stateArg.mode !== 'raw') return;

        const matchingLogs = getMatchingRawOutputLogsFn(stateArg);
        if (matchingLogs.length === 0) return;

        const count = matchingLogs.length;
        const currentIndex = Number.isInteger(stateArg.rawMatchIndex)
            ? stateArg.rawMatchIndex
            : 0;
        stateArg.rawMatchIndex = (currentIndex + direction + count) % count;
        renderOutputHistoryFn(false);
        focusOutputHistoryRawMatchFn(stateArg);
    }

    function onOutputHistorySearchKeydown(event) {
        if (!event || event.key !== 'Enter') return;
        event.preventDefault();
        navigateOutputHistorySearch(event.shiftKey ? -1 : 1);
    }

    function setOutputHistoryOrder(order) {
        const nextOrder = order === 'asc' ? 'asc' : 'desc';
        if (stateArg.order === nextOrder) return;
        stateArg.order = nextOrder;
        renderOutputHistoryFn(true);
    }

    function setOutputHistoryMode(mode, pollHistoryOnceFn) {
        const nextMode = mode === 'raw' ? 'raw' : 'timeline';
        if (stateArg.mode === nextMode) return;
        stateArg.mode = nextMode;
        void pollHistoryOnceFn(true);
    }

    function toggleHistoryRedaction() {
        stateArg.redacted = !stateArg.redacted;
        const button = document.getElementById('output-history-redact');
        if (button) {
            const label = stateArg.redacted ? 'Show URLs' : 'Hide URLs';
            button.title = label;
            button.setAttribute('aria-label', label);
            button.classList.toggle('btn-outline', stateArg.redacted);
            button.classList.toggle('btn-warning', !stateArg.redacted);
        }
        renderOutputHistoryFn(false);
    }

    return {
        navigateOutputHistorySearch,
        onOutputHistorySearchKeydown,
        setOutputHistoryMode,
        setOutputHistoryOrder,
        setOutputHistorySearch,
        toggleHistoryRedaction,
    };
}

// History controller owns modal lifecycle, initial fetches, and the wiring between the focused
// search/context helpers and the renderers for both output and pipeline history.
let toggleOutputHistoryContext = null;

function renderOutputHistory(scrollToTop = false, anchorContextKey = null) {
    renderOutputHistoryView(outputHistoryState, historyConstants, {
        scrollToTop,
        anchorContextKey,
        onToggleContext: toggleOutputHistoryContext,
    });
}

({ toggleOutputHistoryContext } = createOutputHistoryContextController({
    outputHistoryState,
    historyConstants,
    getOutputHistory,
    renderOutputHistory,
    getOutputHistoryContextKey,
    getTimelineContextRange,
}));

const {
    pollHistoryOnce,
    pollPipelineHistoryOnce,
    stopHistoryPoll,
    stopPipelineHistoryPoll,
    syncHistoryPollingWithVisibility,
    toggleHistoryPlayPause,
    togglePipelineHistoryPlayPause,
    updateHistoryPlayPauseBtn,
    updatePipelineHistoryPlayPauseBtn,
} = createHistoryPollingController({
    outputHistoryState,
    pipelineHistoryState,
    historyConstants,
    getOutputHistory,
    getPipelineHistory,
    renderOutputHistory,
    renderPipelineHistory,
});

const {
    navigateOutputHistorySearch,
    onOutputHistorySearchKeydown,
    setOutputHistoryOrder,
    setOutputHistorySearch,
    toggleHistoryRedaction,
    setOutputHistoryMode: setOutputHistoryModeInternal,
} = createOutputHistorySearchController({
    outputHistoryState,
    renderOutputHistory,
    focusOutputHistoryRawMatch,
    getMatchingRawOutputLogs,
});

function setOutputHistoryMode(mode) {
    setOutputHistoryModeInternal(mode, pollHistoryOnce);
}

registerDashboardVisibilitySync(syncHistoryPollingWithVisibility);

function renderPipelineHistory(scrollToTop = false) {
    renderPipelineHistoryView(pipelineHistoryState, { scrollToTop });
}

async function openOutputHistoryModal(pipeId, outId, outName = '') {
    // Reset modal-local state on every open so search indices, context expansions, and redaction
    // choices never bleed across outputs.
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

    await pollHistoryOnce(true);
    loading.classList.add('hidden');
    modal.addEventListener('close', stopHistoryPoll, { once: true });
}

async function openPipelineHistoryModal(pipeId, pipeName = '') {
    // Pipeline history is simpler than output history, but it still resets state before each open
    // so polling and close handlers attach to the current pipeline only.
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

    await pollPipelineHistoryOnce(true);
    loading.classList.add('hidden');
    modal.addEventListener('close', stopPipelineHistoryPoll, { once: true });
}

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
    // re-exported for tests
    outputHistoryState,
    pipelineHistoryState,
    historyConstants,
};
