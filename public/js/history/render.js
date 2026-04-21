import { sanitizeLogMessage } from '../core/utils.js';

const historyRenderCallbacks = {
    toggleOutputHistoryContext: null,
};

function setHistoryRenderCallbacks(callbacks) {
    Object.assign(historyRenderCallbacks, callbacks || {});
}

function formatHistoryTime(ts) {
        if (!ts) return '--';
        const d = new Date(ts);
        if (Number.isNaN(d.getTime())) return ts;
        return d.toLocaleString();
    }

    function getNormalizedEventType(log) {
        return String(log?.eventType || '').trim().toLowerCase();
    }

    function getEventData(log) {
        return log?.eventData && typeof log.eventData === 'object' ? log.eventData : null;
    }

    function inferIntentionalStop(logs, index) {
        // Exit logs alone are ambiguous, so we scan nearby lifecycle/control messages to decide
        // whether a terminal ffmpeg exit came from a user stop or from an unexpected failure.
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
        for (let i = windowStart; i <= windowEnd; i += 1) {
            if (i === index) continue;
            const eventType = getNormalizedEventType(entries[i]);
            if (
                eventType === 'lifecycle.stop_requested' ||
                eventType === 'control.signal_requested'
            ) {
                return true;
            }
            const msg = String(entries[i]?.message || '');
            if (
                msg.startsWith('[lifecycle] stop_requested') ||
                msg.startsWith('[control] requested SIGTERM') ||
                /received signal 15/i.test(msg)
            ) {
                return true;
            }
        }

        return false;
    }

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
            if (
                eventData?.scheduled === false &&
                eventData?.reason === 'desired_state_stopped'
            ) {
                return {
                    type: 'retry_suppressed',
                    label: 'Retry skipped',
                    badgeClass: 'badge-info',
                };
            }
            if (eventData?.scheduled === false) {
                return {
                    type: 'retry_update',
                    label: 'Retry not scheduled',
                    badgeClass: 'badge-ghost',
                };
            }
            return { type: 'retry_update', label: 'Retry queued', badgeClass: 'badge-warning' };
        }
        if (eventType === 'lifecycle.retry_suppressed') {
            return {
                type: 'retry_suppressed',
                label: 'Retry skipped',
                badgeClass: 'badge-info',
            };
        }
        if (eventType === 'lifecycle.retry_exhausted') {
            return { type: 'retry_exhausted', label: 'Retry exhausted', badgeClass: 'badge-error' };
        }
        if (eventType === 'lifecycle.marked_stopped_no_process') {
            return { type: 'stopped', label: 'Stopped', badgeClass: 'badge-stopped' };
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
                return {
                    type: 'retry_suppressed',
                    label: 'Retry skipped',
                    badgeClass: 'badge-info',
                };
            }
            if (/scheduled=false/.test(message)) {
                return {
                    type: 'retry_update',
                    label: 'Retry not scheduled',
                    badgeClass: 'badge-ghost',
                };
            }
            return { type: 'retry_update', label: 'Retry queued', badgeClass: 'badge-warning' };
        }
        if (message.startsWith('[lifecycle] retry_exhausted')) {
            return { type: 'retry_exhausted', label: 'Retry exhausted', badgeClass: 'badge-error' };
        }
        if (message.startsWith('[lifecycle] marked_stopped_no_process')) {
            return { type: 'stopped', label: 'Stopped', badgeClass: 'badge-stopped' };
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
        if (message.startsWith('[exit]')) {
            return { type: 'log', label: 'Log', badgeClass: 'badge-ghost' };
        }

        return { type: 'log', label: 'Log', badgeClass: 'badge-ghost' };
    }

    function classifyPipelineHistoryEvent(log) {
        const eventType = getNormalizedEventType(log);
        const eventData = getEventData(log);

        if (eventType.startsWith('pipeline.config.')) {
            return { type: 'config', label: 'Config', badgeClass: 'badge-secondary' };
        }
        if (eventType === 'pipeline.input_state.initialized') {
            const finalState = String(eventData?.state || '').toLowerCase();
            if (finalState === 'on')
                return { type: 'on', label: 'Input On', badgeClass: 'badge-success' };
            if (finalState === 'warning')
                return { type: 'warning', label: 'Input Warning', badgeClass: 'badge-warning' };
            if (finalState === 'error')
                return { type: 'error', label: 'Input Error', badgeClass: 'badge-error' };
            if (finalState === 'off')
                return { type: 'off', label: 'Input Off', badgeClass: 'badge-stopped' };
        }
        if (eventType === 'pipeline.input_state.transitioned') {
            const finalState = String(eventData?.to || '').toLowerCase();
            if (finalState === 'on')
                return { type: 'on', label: 'Input On', badgeClass: 'badge-success' };
            if (finalState === 'warning')
                return { type: 'warning', label: 'Input Warning', badgeClass: 'badge-warning' };
            if (finalState === 'error')
                return { type: 'error', label: 'Input Error', badgeClass: 'badge-error' };
            if (finalState === 'off')
                return { type: 'off', label: 'Input Off', badgeClass: 'badge-stopped' };
        }
        if (eventType === 'pipeline.input_state.reset') {
            return { type: 'reset', label: 'Input Reset', badgeClass: 'badge-info' };
        }

        const message = String(log?.message || '');

        if (message.startsWith('[config]')) {
            return { type: 'config', label: 'Config', badgeClass: 'badge-secondary' };
        }
        if (message.startsWith('[input_state]')) {
            let finalState = '';
            if (message.includes('->')) {
                finalState = message.split('->').pop().trim().toLowerCase();
            } else {
                const match = message.match(/initial_state\s*=\s*([a-z_]+)/i);
                finalState = (match && match[1] ? match[1] : '').toLowerCase();
            }

            if (finalState === 'on')
                return { type: 'on', label: 'Input On', badgeClass: 'badge-success' };
            if (finalState === 'warning')
                return { type: 'warning', label: 'Input Warning', badgeClass: 'badge-warning' };
            if (finalState === 'error')
                return { type: 'error', label: 'Input Error', badgeClass: 'badge-error' };
            if (finalState === 'off')
                return { type: 'off', label: 'Input Off', badgeClass: 'badge-stopped' };
        }

        return { type: 'log', label: 'Event', badgeClass: 'badge-ghost' };
    }

    function getPipelineTimelineLogs(logs) {
        const items = Array.isArray(logs) ? logs : [];
        return items.filter((log) => {
            const eventType = getNormalizedEventType(log);
            if (
                eventType.startsWith('pipeline.config.') ||
                eventType.startsWith('pipeline.input_state.')
            ) {
                return true;
            }
            const message = String(log?.message || '');
            return message.startsWith('[config]') || message.startsWith('[input_state]');
        });
    }

    function getOrderedOutputLogs(logs, order) {
        const items = Array.isArray(logs) ? [...logs] : [];
        items.sort((a, b) => {
            const ta = Date.parse(a?.ts || '');
            const tb = Date.parse(b?.ts || '');
            const aMs = Number.isNaN(ta) ? 0 : ta;
            const bMs = Number.isNaN(tb) ? 0 : tb;
            return aMs - bMs;
        });
        return order === 'asc' ? items : items.reverse();
    }

    function parseHistoryTimeMs(ts) {
        const value = Date.parse(ts || '');
        return Number.isNaN(value) ? null : value;
    }

    function getOutputHistoryContextKey(log) {
        return `${log?.ts || ''}::${log?.message || ''}`;
    }

    function getRawHistorySearchValue(state) {
        return String(state.rawQuery || '')
            .trim()
            .toLowerCase();
    }

    function getFilteredRawOutputLogs(state) {
        return getOrderedOutputLogs(state.rawLogs, state.order);
    }

    function getMatchingRawOutputLogs(state) {
        const query = getRawHistorySearchValue(state);
        if (!query) return [];
        return getFilteredRawOutputLogs(state).filter((log) => {
            const haystack = `${log?.ts || ''}\n${log?.message || ''}`.toLowerCase();
            return haystack.includes(query);
        });
    }

    function getTimelineContextLogs(state, log) {
        return state.contextLogsByKey.get(getOutputHistoryContextKey(log)) || [];
    }

    function getTimelineContextRange(state, constants, log) {
        const targetMs = parseHistoryTimeMs(log?.ts);
        if (targetMs === null) return null;

        const lifecycleLogsAsc = getOrderedOutputLogs(state.lifecycleLogs, 'asc');
        const targetIndex = lifecycleLogsAsc.findIndex(
            (entry) =>
                entry?.ts === log?.ts &&
                String(entry?.message || '') === String(log?.message || ''),
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

    function renderOutputHistory(
        state,
        constants,
        { scrollToTop = false, anchorContextKey = null } = {},
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
            toggle.onclick = () => historyRenderCallbacks.toggleOutputHistoryContext?.(log);
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

    function renderPipelineHistory(state, { scrollToTop = false } = {}) {
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

export {
    focusOutputHistoryRawMatch,
    getMatchingRawOutputLogs,
    getOutputHistoryContextKey,
    getTimelineContextRange,
    renderOutputHistory,
    renderPipelineHistory,
    setHistoryRenderCallbacks,
};
