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
    pollTimer: null,
    pollEveryMs: null,
    isPolling: false,
};

const pipelineHistoryState = {
    pipelineId: null,
    pipelineName: '',
    logs: [],
    playing: false,
    pollTimer: null,
    pollEveryMs: null,
    isPolling: false,
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

export { outputHistoryState, pipelineHistoryState, historyConstants };
