// History event classification.
// Classifies raw FFmpeg/control log lines into typed history events (start, stop, error,
// warning, info) with a human-readable summary, infers whether a stop was intentional,
// and formats timestamps for the history view.
/**
 * Formats an ISO timestamp for display in the history view.
 * @param {string|null|undefined} ts - ISO 8601 timestamp string.
 * @returns {string} Locale-formatted date/time string, or `'--'` for falsy input.
 */
function formatHistoryTime(ts) {
    if (!ts) return '--';
    const date = new Date(ts);
    if (Number.isNaN(date.getTime())) return ts;
    return date.toLocaleString();
}

/** @param {object} log @returns {string} */
function getNormalizedEventType(log) {
    return String(log?.eventType || '').trim().toLowerCase();
}

/** @param {object} log @returns {object|null} */
function getEventData(log) {
    return log?.eventData && typeof log.eventData === 'object' ? log.eventData : null;
}

/**
 * Infers whether the output exit at `index` in `logs` was caused by a deliberate stop
 * request rather than an unexpected failure. Scans a ±window of surrounding lifecycle
 * and control events when the exit record itself is ambiguous.
 * @param {object[]} logs - Full ordered log array for the output.
 * @param {number} index - Index of the exit entry being classified.
 * @returns {boolean}
 */
function inferIntentionalStop(logs, index) {
    // Exit logs alone are ambiguous, so scan nearby lifecycle and control events before deciding
    // whether an FFmpeg exit came from a deliberate stop or an unexpected failure.
    const entries = Array.isArray(logs) ? logs : [];
    const target = entries[index];
    if (!target) return false;

    const targetEventType = getNormalizedEventType(target);
    const targetEventData = getEventData(target);
    if (targetEventType === 'lifecycle.exited' && targetEventData?.requestedStop === true) {
        return true;
    }

    const targetMessage = String(target.message || '');
    if (/requestedStop=true/.test(targetMessage)) return true;

    const windowStart = Math.max(0, index - 4);
    const windowEnd = Math.min(entries.length - 1, index + 6);
    for (let cursor = windowStart; cursor <= windowEnd; cursor += 1) {
        if (cursor === index) continue;

        const eventType = getNormalizedEventType(entries[cursor]);
        if (eventType === 'lifecycle.stop_requested' || eventType === 'control.signal_requested') {
            return true;
        }

        const message = String(entries[cursor]?.message || '');
        if (
            message.startsWith('[lifecycle] stop_requested') ||
            message.startsWith('[control] requested SIGTERM') ||
            /received signal 15/i.test(message)
        ) {
            return true;
        }
    }

    return false;
}

/**
 * Classifies a single output lifecycle log entry into a display descriptor.
 * Pass `logs` and `index` when classifying an `exited` event so `inferIntentionalStop`
 * can scan the surrounding window to resolve ambiguous exits.
 * @param {object} log - The log entry to classify.
 * @param {object[]} [logs] - Full log array (required for accurate exit classification).
 * @param {number} [index] - Position of `log` within `logs`.
 * @returns {{type: string, label: string, badgeClass: string}}
 */
function classifyHistoryEvent(log, logs = [], index = -1) {
    const eventType = getNormalizedEventType(log);
    const eventData = getEventData(log);

    if (eventType === 'lifecycle.desired_state_changed') {
        const desiredRunning = eventData?.state === 'running';
        return {
            type: 'desired_state',
            label: desiredRunning ? 'Start requested' : 'Stop requested',
            badgeClass: desiredRunning ? 'badge-info' : 'badge-warning',
        };
    }
    if (eventType === 'lifecycle.started') {
        return { type: 'started', label: 'Started', badgeClass: 'badge-success' };
    }
    if (eventType === 'lifecycle.stop_requested') {
        return { type: 'stopping', label: 'Stop requested', badgeClass: 'badge-warning' };
    }
    if (eventType === 'lifecycle.auto_start_suppressed') {
        return { type: 'suppressed', label: 'Auto-start skipped', badgeClass: 'badge-info' };
    }
    if (eventType === 'lifecycle.failed_on_error') {
        return { type: 'failed', label: 'Failed', badgeClass: 'badge-error' };
    }
    if (eventType === 'lifecycle.retry_decision') {
        if (eventData?.scheduled === false && eventData?.reason === 'desired_state_stopped') {
            return { type: 'retry_suppressed', label: 'Retry skipped', badgeClass: 'badge-info' };
        }
        if (eventData?.scheduled === false) {
            return { type: 'retry_update', label: 'Retry not scheduled', badgeClass: 'badge-ghost' };
        }
        return { type: 'retry_update', label: 'Retry queued', badgeClass: 'badge-warning' };
    }
    if (eventType === 'lifecycle.retry_suppressed') {
        return { type: 'retry_suppressed', label: 'Retry skipped', badgeClass: 'badge-info' };
    }
    if (eventType === 'lifecycle.retry_exhausted') {
        return { type: 'retry_exhausted', label: 'Retry exhausted', badgeClass: 'badge-error' };
    }
    if (eventType === 'lifecycle.marked_stopped_no_process') {
        return { type: 'stopped', label: 'Stopped', badgeClass: 'badge-stopped' };
    }
    if (eventType === 'lifecycle.config_created') {
        return { type: 'config', label: 'Config Created', badgeClass: 'badge-secondary' };
    }
    if (eventType === 'lifecycle.config_changed' || eventType.startsWith('lifecycle.config_')) {
        return { type: 'config', label: 'Config Updated', badgeClass: 'badge-secondary' };
    }
    if (eventType === 'lifecycle.exited') {
        const failed = eventData?.status === 'failed';
        const requestedStop =
            typeof eventData?.requestedStop === 'boolean'
                ? eventData.requestedStop
                : inferIntentionalStop(logs, index);
        return {
            type: failed && !requestedStop ? 'failed' : 'stopped',
            label: failed && requestedStop ? 'Stopped' : failed ? 'Exited (failed)' : 'Exited',
            badgeClass: failed && !requestedStop ? 'badge-error' : 'badge-stopped',
        };
    }
    if (eventType === 'output.exit') {
        return { type: 'log', label: 'Log', badgeClass: 'badge-ghost' };
    }

    const message = String(log?.message || '');
    if (message.startsWith('[lifecycle] desired_state')) {
        const desiredRunning = /state=running/.test(message);
        return {
            type: 'desired_state',
            label: desiredRunning ? 'Start requested' : 'Stop requested',
            badgeClass: desiredRunning ? 'badge-info' : 'badge-warning',
        };
    }
    if (message.startsWith('[lifecycle] started')) {
        return { type: 'started', label: 'Started', badgeClass: 'badge-success' };
    }
    if (message.startsWith('[lifecycle] stop_requested')) {
        return { type: 'stopping', label: 'Stop requested', badgeClass: 'badge-warning' };
    }
    if (message.startsWith('[lifecycle] auto_start_suppressed')) {
        return { type: 'suppressed', label: 'Auto-start skipped', badgeClass: 'badge-info' };
    }
    if (message.startsWith('[lifecycle] failed_on_error')) {
        return { type: 'failed', label: 'Failed', badgeClass: 'badge-error' };
    }
    if (message.startsWith('[lifecycle] retry_decision')) {
        if (/scheduled=false/.test(message) && /reason=desired_state_stopped/.test(message)) {
            return { type: 'retry_suppressed', label: 'Retry skipped', badgeClass: 'badge-info' };
        }
        if (/scheduled=false/.test(message)) {
            return { type: 'retry_update', label: 'Retry not scheduled', badgeClass: 'badge-ghost' };
        }
        return { type: 'retry_update', label: 'Retry queued', badgeClass: 'badge-warning' };
    }
    if (message.startsWith('[lifecycle] retry_exhausted')) {
        return { type: 'retry_exhausted', label: 'Retry exhausted', badgeClass: 'badge-error' };
    }
    if (message.startsWith('[lifecycle] marked_stopped_no_process')) {
        return { type: 'stopped', label: 'Stopped', badgeClass: 'badge-stopped' };
    }
    if (message.startsWith('[lifecycle] config_created')) {
        return { type: 'config', label: 'Config Created', badgeClass: 'badge-secondary' };
    }
    if (message.startsWith('[lifecycle] config_changed') || message.startsWith('[lifecycle] config_')) {
        return { type: 'config', label: 'Config Updated', badgeClass: 'badge-secondary' };
    }
    if (message.startsWith('[lifecycle] exited')) {
        const failed = /status=failed/.test(message);
        const requestedStop = inferIntentionalStop(logs, index);
        return {
            type: failed && !requestedStop ? 'failed' : 'stopped',
            label: failed && requestedStop ? 'Stopped' : failed ? 'Exited (failed)' : 'Exited',
            badgeClass: failed && !requestedStop ? 'badge-error' : 'badge-stopped',
        };
    }

    return { type: 'log', label: 'Log', badgeClass: 'badge-ghost' };
}

/**
 * Classifies a pipeline-level history log entry (config changes and input-state
 * transitions) into a display descriptor.
 * @param {object} log - The pipeline log entry to classify.
 * @returns {{type: string, label: string, badgeClass: string}}
 */
function classifyPipelineHistoryEvent(log) {
    const eventType = getNormalizedEventType(log);
    const eventData = getEventData(log);

    if (eventType === 'pipeline.config.created') {
        return { type: 'config', label: 'Config Created', badgeClass: 'badge-secondary' };
    }
    if (eventType.startsWith('pipeline.config.')) {
        return { type: 'config', label: 'Config Updated', badgeClass: 'badge-secondary' };
    }
    if (eventType === 'pipeline.input_state.initialized') {
        const finalState = String(eventData?.state || '').toLowerCase();
        if (finalState === 'on') return { type: 'on', label: 'Input On', badgeClass: 'badge-success' };
        if (finalState === 'warning') return { type: 'warning', label: 'Input Warning', badgeClass: 'badge-warning' };
        if (finalState === 'error') return { type: 'error', label: 'Input Error', badgeClass: 'badge-error' };
        if (finalState === 'off') return { type: 'off', label: 'Input Off', badgeClass: 'badge-stopped' };
    }
    if (eventType === 'pipeline.input_state.transitioned') {
        const finalState = String(eventData?.to || '').toLowerCase();
        if (finalState === 'on') return { type: 'on', label: 'Input On', badgeClass: 'badge-success' };
        if (finalState === 'warning') return { type: 'warning', label: 'Input Warning', badgeClass: 'badge-warning' };
        if (finalState === 'error') return { type: 'error', label: 'Input Error', badgeClass: 'badge-error' };
        if (finalState === 'off') return { type: 'off', label: 'Input Off', badgeClass: 'badge-stopped' };
    }
    if (eventType === 'pipeline.input_state.reset') {
        return { type: 'reset', label: 'Input Reset', badgeClass: 'badge-info' };
    }

    const message = String(log?.message || '');
    if (message.startsWith('[config] created')) {
        return { type: 'config', label: 'Config Created', badgeClass: 'badge-secondary' };
    }
    if (message.startsWith('[config]')) {
        return { type: 'config', label: 'Config Updated', badgeClass: 'badge-secondary' };
    }
    if (message.startsWith('[input_state]')) {
        let finalState = '';
        if (message.includes('->')) {
            finalState = message.split('->').pop().trim().toLowerCase();
        } else {
            const match = message.match(/initial_state\s*=\s*([a-z_]+)/i);
            finalState = (match && match[1] ? match[1] : '').toLowerCase();
        }

        if (finalState === 'on') return { type: 'on', label: 'Input On', badgeClass: 'badge-success' };
        if (finalState === 'warning') return { type: 'warning', label: 'Input Warning', badgeClass: 'badge-warning' };
        if (finalState === 'error') return { type: 'error', label: 'Input Error', badgeClass: 'badge-error' };
        if (finalState === 'off') return { type: 'off', label: 'Input Off', badgeClass: 'badge-stopped' };
    }

    return { type: 'log', label: 'Event', badgeClass: 'badge-ghost' };
}

/**
 * Filters a log array to entries relevant to the pipeline timeline — config
 * changes and input-state transitions only.
 * @param {object[]} logs
 * @returns {object[]}
 */
function getPipelineTimelineLogs(logs) {
    return (Array.isArray(logs) ? logs : []).filter((log) => {
        const eventType = getNormalizedEventType(log);
        if (eventType.startsWith('pipeline.config.') || eventType.startsWith('pipeline.input_state.')) {
            return true;
        }
        const message = String(log?.message || '');
        return message.startsWith('[config]') || message.startsWith('[input_state]');
    });
}

/**
 * Returns a stable-sorted copy of `logs` ordered by timestamp.
 * @param {object[]} logs
 * @param {'asc'|'desc'} order
 * @returns {object[]}
 */
function getOrderedOutputLogs(logs, order) {
    const items = Array.isArray(logs) ? [...logs] : [];
    items.sort((left, right) => {
        const leftMs = Date.parse(left?.ts || '');
        const rightMs = Date.parse(right?.ts || '');
        return (Number.isNaN(leftMs) ? 0 : leftMs) - (Number.isNaN(rightMs) ? 0 : rightMs);
    });
    return order === 'asc' ? items : items.reverse();
}

/**
 * Parses an ISO timestamp string to milliseconds-since-epoch.
 * @param {string} ts
 * @returns {number|null} `null` when the string cannot be parsed.
 */
function parseHistoryTimeMs(ts) {
    const parsed = Date.parse(ts || '');
    return Number.isNaN(parsed) ? null : parsed;
}

/**
 * Returns a stable string key for a log entry used to look up pre-computed
 * context entries in the history page state map.
 * @param {object} log
 * @returns {string} `"<ts>::<message>"`
 */
function getOutputHistoryContextKey(log) {
    return `${log?.ts || ''}::${log?.message || ''}`;
}

/**
 * Extracts and normalises the raw-log search query from the history page state.
 * @param {object} state - History page state; reads `state.rawQuery`.
 * @returns {string} Trimmed, lower-cased query string.
 */
function getRawHistorySearchValue(state) {
    return String(state.rawQuery || '').trim().toLowerCase();
}

/**
 * Returns the raw output logs sorted according to `state.order`.
 * @param {object} state - History page state; reads `state.rawLogs` and `state.order`.
 * @returns {object[]}
 */
function getFilteredRawOutputLogs(state) {
    return getOrderedOutputLogs(state.rawLogs, state.order);
}

/**
 * Returns raw output logs that contain the current search query.
 * @param {object} state - History page state; reads `state.rawQuery`, `state.rawLogs`, `state.order`.
 * @returns {object[]}
 */
function getMatchingRawOutputLogs(state) {
    const query = getRawHistorySearchValue(state);
    if (!query) return [];

    return getFilteredRawOutputLogs(state).filter((log) => {
        const haystack = `${log?.ts || ''}\n${log?.message || ''}`.toLowerCase();
        return haystack.includes(query);
    });
}

/**
 * Returns the pre-computed context logs stored for a lifecycle timeline entry.
 * @param {object} state - History page state; reads `state.contextLogsByKey` (Map).
 * @param {object} log - The timeline entry whose context is requested.
 * @returns {object[]}
 */
function getTimelineContextLogs(state, log) {
    return state.contextLogsByKey.get(getOutputHistoryContextKey(log)) || [];
}

/**
 * Computes the `{since, until}` ISO timestamp window used when fetching
 * context raw logs for a lifecycle timeline entry. The lower bound is clamped
 * to the previous lifecycle event so context does not bleed across jobs.
 * @param {object} state - History page state; reads `state.lifecycleLogs`.
 * @param {object} constants - Must provide `OUTPUT_HISTORY_CONTEXT_WINDOW_MS`.
 * @param {object} log - The lifecycle entry whose context window is needed.
 * @returns {{since: string, until: string}|null} `null` when `log.ts` cannot be parsed.
 */
function getTimelineContextRange(state, constants, log) {
    const targetMs = parseHistoryTimeMs(log?.ts);
    if (targetMs === null) return null;

    const lifecycleLogsAsc = getOrderedOutputLogs(state.lifecycleLogs, 'asc');
    const targetIndex = lifecycleLogsAsc.findIndex(
        (entry) => entry?.ts === log?.ts && String(entry?.message || '') === String(log?.message || ''),
    );
    const previousLifecycle = targetIndex > 0 ? lifecycleLogsAsc[targetIndex - 1] : null;
    const previousLifecycleMs = parseHistoryTimeMs(previousLifecycle?.ts);
    const lowerBoundMs = Math.max(
        previousLifecycleMs === null ? Number.NEGATIVE_INFINITY : previousLifecycleMs,
        targetMs - constants.OUTPUT_HISTORY_CONTEXT_WINDOW_MS,
    );
    const sinceMs = Number.isFinite(lowerBoundMs)
        ? lowerBoundMs
        : targetMs - constants.OUTPUT_HISTORY_CONTEXT_WINDOW_MS;

    return {
        since: new Date(sinceMs).toISOString(),
        until: new Date(targetMs).toISOString(),
    };
}

export {
    classifyHistoryEvent,
    classifyPipelineHistoryEvent,
    focusOutputHistoryRawMatch,
    formatHistoryTime,
    getFilteredRawOutputLogs,
    getMatchingRawOutputLogs,
    getOrderedOutputLogs,
    getOutputHistoryContextKey,
    getPipelineTimelineLogs,
    getRawHistorySearchValue,
    getTimelineContextLogs,
    getTimelineContextRange,
    inferIntentionalStop,
};

/**
 * Scrolls the raw-log list to the entry currently focused by `state.rawMatchIndex`.
 * No-ops when the list element or the target item cannot be found in the DOM.
 * @param {object} state - History page state; reads `state.rawMatchIndex`.
 */
function focusOutputHistoryRawMatch(state) {
    const list = document.getElementById('output-history-list');
    if (!list) return;
    const target = list.querySelector(`[data-raw-match-index="${state.rawMatchIndex}"]`);
    if (!target) return;
    target.scrollIntoView({ block: 'nearest' });
}