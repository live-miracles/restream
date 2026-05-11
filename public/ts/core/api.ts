import { showLoading, hideLoading, showErrorAlert, normalizeEtag } from './utils.js';
import type {
    ConfigData,
    HealthData,
    SystemMetrics,
    StreamKey,
    GetConfigResult,
    GetHealthResult,
} from '../types.js';

let activeMutationRequestCount = 0;

function isMutationMethod(method: string): boolean {
    const normalizedMethod = String(method || 'GET').toUpperCase();
    return (
        normalizedMethod !== 'GET' && normalizedMethod !== 'HEAD' && normalizedMethod !== 'OPTIONS'
    );
}

function beginMutationRequest(): void {
    activeMutationRequestCount += 1;
    if (activeMutationRequestCount === 1) {
        showLoading();
    }
}

function endMutationRequest(): void {
    if (activeMutationRequestCount <= 0) {
        activeMutationRequestCount = 0;
        return;
    }

    activeMutationRequestCount -= 1;
    if (activeMutationRequestCount === 0) {
        hideLoading();
    }
}

interface FetchWithEtagOptions {
    etag?: string | null;
    method?: string;
    networkErrorMessage?: string | null;
}

async function fetchWithEtag(
    url: string,
    { etag = null, method = 'GET', networkErrorMessage = null }: FetchWithEtagOptions = {},
): Promise<Response | null> {
    const headers: Record<string, string> = {};
    if (etag) headers['If-None-Match'] = `"${etag}"`;
    const options: RequestInit = { method, headers, cache: 'no-store' };

    if (!networkErrorMessage) {
        return fetch(url, options);
    }

    try {
        return await fetch(url, options);
    } catch (e) {
        showErrorAlert(networkErrorMessage + e);
        return null;
    }
}

async function parseJsonResponse<T>(response: Response): Promise<T | null> {
    try {
        return (await response.json()) as T;
    } catch (e) {
        showErrorAlert('Invalid JSON response: ' + e);
        return null;
    }
}

async function apiRequest<T = unknown>(
    url: string,
    { method = 'GET', body = null }: { method?: string; body?: unknown } = {},
): Promise<T | null> {
    const normalizedMethod = String(method || 'GET').toUpperCase();
    const options: RequestInit = { method: normalizedMethod };

    if (body !== null) {
        options.headers = { 'Content-Type': 'application/json' };
        options.body = JSON.stringify(body);
    }

    const showMutationLoading = isMutationMethod(normalizedMethod);
    let response: Response | null = null;
    if (showMutationLoading) beginMutationRequest();
    try {
        response = await fetch(url, options);
    } catch (e) {
        showErrorAlert('Network request failed: ' + e);
        return null;
    } finally {
        if (showMutationLoading) endMutationRequest();
    }

    let data: T | null = null;
    try {
        data = (await response.json()) as T;
    } catch (e) {
        showErrorAlert('Invalid JSON response: ' + e);
        return null;
    }

    if (!response.ok) {
        const errData = data as Record<string, unknown> | null;
        showErrorAlert(errData?.error || `Request failed with ${response.status}`);
        return null;
    }

    return data;
}

async function getConfig(etag: string | null = null): Promise<GetConfigResult | null> {
    const response = await fetchWithEtag('/config', { etag });
    if (!response) return null;

    if (response.status === 304) {
        return { notModified: true, etag, data: null };
    }

    const data = await parseJsonResponse<ConfigData & { error?: string }>(response);
    if (data === null) return null;

    if (!response.ok) {
        showErrorAlert(data?.error || `Request failed with ${response.status}`);
        return null;
    }

    return {
        notModified: false,
        etag: normalizeEtag(response.headers.get('ETag')),
        data: data as ConfigData,
    };
}

async function getHealth(etag: string | null = null): Promise<GetHealthResult | null> {
    const response = await fetchWithEtag('/health', {
        etag,
        networkErrorMessage: 'Network request failed: ',
    });
    if (!response) return null;

    if (response.status === 304) {
        return { notModified: true, etag, data: null };
    }

    const data = await parseJsonResponse<HealthData & { error?: string }>(response);
    if (data === null) return null;

    if (!response.ok) {
        showErrorAlert(data?.error || `Request failed with ${response.status}`);
        return null;
    }

    return {
        notModified: false,
        etag: normalizeEtag(response.headers.get('ETag')),
        data: data as HealthData,
    };
}

async function getSystemMetrics(): Promise<SystemMetrics | null> {
    return apiRequest<SystemMetrics>('/metrics/system');
}

async function getStreamKeys(): Promise<StreamKey[] | null> {
    return apiRequest<StreamKey[]>('/stream-keys');
}

interface CreatePipelineArgs {
    name: string;
    streamKey: string;
    encoding?: string | null;
}

async function createPipeline(args: CreatePipelineArgs): Promise<unknown | null> {
    if (!args.name) {
        showErrorAlert('Invalid pipeline name');
        return;
    }

    return apiRequest('/pipelines', {
        method: 'POST',
        body: args,
    });
}

async function updatePipeline(pipeId: string, data: unknown): Promise<unknown | null> {
    if (!pipeId) {
        showErrorAlert('Pipeline id is required');
        return null;
    }

    return apiRequest(`/pipelines/${encodeURIComponent(pipeId)}`, {
        method: 'POST',
        body: data,
    });
}

async function deletePipeline(pipeId: string): Promise<unknown | null> {
    if (!pipeId) {
        showErrorAlert('Pipeline id is required');
        return null;
    }

    return apiRequest(`/pipelines/${encodeURIComponent(pipeId)}`, { method: 'DELETE' });
}

async function createOutput(pipeId: string, data: unknown): Promise<unknown | null> {
    if (!pipeId) {
        showErrorAlert('Pipeline id is required');
        return null;
    }

    return apiRequest(`/pipelines/${encodeURIComponent(pipeId)}/outputs`, {
        method: 'POST',
        body: data,
    });
}

async function updateOutput(pipeId: string, outId: string, data: unknown): Promise<unknown | null> {
    if (!pipeId || !outId) {
        showErrorAlert('Pipeline id and output id are required');
        return null;
    }

    return apiRequest(
        `/pipelines/${encodeURIComponent(pipeId)}/outputs/${encodeURIComponent(outId)}`,
        { method: 'POST', body: data },
    );
}

async function deleteOutput(pipeId: string, outId: string): Promise<unknown | null> {
    if (!pipeId || !outId) {
        showErrorAlert('Pipeline id and output id are required');
        return null;
    }

    return apiRequest(
        `/pipelines/${encodeURIComponent(pipeId)}/outputs/${encodeURIComponent(outId)}`,
        { method: 'DELETE' },
    );
}

async function startOut(pipeId: string, outId: string): Promise<unknown | null> {
    if (!pipeId || !outId) {
        showErrorAlert('Pipeline id and output id are required');
        return null;
    }

    return apiRequest(
        `/pipelines/${encodeURIComponent(pipeId)}/outputs/${encodeURIComponent(outId)}/start`,
        { method: 'POST' },
    );
}

async function stopOut(pipeId: string, outId: string): Promise<unknown | null> {
    if (!pipeId || !outId) {
        showErrorAlert('Pipeline id and output id are required');
        return null;
    }

    return apiRequest(
        `/pipelines/${encodeURIComponent(pipeId)}/outputs/${encodeURIComponent(outId)}/stop`,
        { method: 'POST' },
    );
}

interface GetOutputHistoryOptions {
    limit?: number;
    filter?: string | null;
    since?: string | null;
    until?: string | null;
    order?: string | null;
    prefixes?: string[] | null;
}

async function getOutputHistory(
    pipeId: string,
    outId: string,
    options: GetOutputHistoryOptions = {},
): Promise<{ logs: unknown[] } | null> {
    if (!pipeId || !outId) {
        showErrorAlert('Pipeline id and output id are required');
        return null;
    }

    const query = new URLSearchParams();
    const {
        limit = 200,
        filter = null,
        since = null,
        until = null,
        order = null,
        prefixes = null,
    } = options;

    if (filter === 'lifecycle') {
        query.set('filter', 'lifecycle');
    } else {
        const safeLimit = Number.isFinite(Number(limit)) ? Number(limit) : 200;
        query.set('limit', String(safeLimit));
    }

    if (since) query.set('since', String(since));
    if (until) query.set('until', String(until));
    if (order) query.set('order', String(order));
    if (Array.isArray(prefixes) && prefixes.length > 0) {
        query.set('prefix', prefixes.join(','));
    }

    return apiRequest(
        `/pipelines/${encodeURIComponent(pipeId)}/outputs/${encodeURIComponent(outId)}/history?${query.toString()}`,
    );
}

async function getPipelineHistory(
    pipeId: string,
    limit = 200,
): Promise<{ logs: unknown[] } | null> {
    if (!pipeId) {
        showErrorAlert('Pipeline id is required');
        return null;
    }

    const safeLimit = Number.isFinite(Number(limit)) ? Number(limit) : 200;
    return apiRequest(
        `/pipelines/${encodeURIComponent(pipeId)}/history?limit=${encodeURIComponent(safeLimit)}`,
    );
}

export {
    apiRequest,
    getConfig,
    getHealth,
    getSystemMetrics,
    getStreamKeys,
    createPipeline,
    updatePipeline,
    deletePipeline,
    createOutput,
    updateOutput,
    deleteOutput,
    startOut,
    stopOut,
    getOutputHistory,
    getPipelineHistory,
};
