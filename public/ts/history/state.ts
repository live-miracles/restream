import type { HistoryLog } from '../types.js';

export interface OutputHistoryState {
    pipelineId: string | null;
    outputId: string | null;
    outputName: string;
    mode: 'timeline' | 'raw';
    order: 'asc' | 'desc';
    lifecycleLogs: HistoryLog[];
    rawLogs: HistoryLog[];
    rawQuery: string;
    rawMatchIndex: number;
    expandedContextKeys: Set<string>;
    contextLogsByKey: Map<string, HistoryLog[]>;
    contextLoadingKeys: Set<string>;
    redacted: boolean;
    playing: boolean;
    pollTimer: ReturnType<typeof setTimeout> | null;
    pollEveryMs: number | null;
    isPolling: boolean;
}

export interface PipelineHistoryState {
    pipelineId: string | null;
    pipelineName: string;
    logs: HistoryLog[];
    playing: boolean;
    pollTimer: ReturnType<typeof setTimeout> | null;
    pollEveryMs: number | null;
    isPolling: boolean;
}

export interface HistoryConstants {
    OUTPUT_HISTORY_POLL_INTERVAL_MS: number;
    OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS: number;
    OUTPUT_HISTORY_RAW_LIMIT: number;
    OUTPUT_HISTORY_CONTEXT_WINDOW_MS: number;
    OUTPUT_HISTORY_CONTEXT_LIMIT: number;
}

export const outputHistoryState: OutputHistoryState = {
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

export const pipelineHistoryState: PipelineHistoryState = {
    pipelineId: null,
    pipelineName: '',
    logs: [],
    playing: false,
    pollTimer: null,
    pollEveryMs: null,
    isPolling: false,
};

export const historyConstants: HistoryConstants = {
    OUTPUT_HISTORY_POLL_INTERVAL_MS: 5000,
    OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS: 30000,
    OUTPUT_HISTORY_RAW_LIMIT: 1000,
    OUTPUT_HISTORY_CONTEXT_WINDOW_MS: 5 * 60 * 1000,
    OUTPUT_HISTORY_CONTEXT_LIMIT: 50,
};
