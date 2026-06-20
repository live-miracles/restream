import { showLoading, hideLoading, showErrorAlert } from './utils.js';
import { withBasePath } from './base-path.js';
import type {
    ConfigData,
    HealthData,
    IngestSecurityConfig,
    SystemMetrics,
    StreamKey,
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
        response = await fetch(withBasePath(url), options);
    } catch (e) {
        showErrorAlert('Network request failed: ' + e);
        return null;
    } finally {
        if (showMutationLoading) endMutationRequest();
    }

    if (response.status === 204) {
        return null;
    }

    if (response.status === 401) {
        window.location.href = '/login';
        return null;
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

async function getConfig(): Promise<ConfigData | null> {
    return apiRequest<ConfigData>('/config');
}

async function getHealth(): Promise<HealthData | null> {
    return apiRequest<HealthData>('/health');
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
    inputSource?: string | null;
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

async function patchConfig(body: {
    serverName?: string;
    ingestHost?: string;
    ingestSecurity?: Partial<IngestSecurityConfig>;
}): Promise<{
    serverName: string;
    ingestHost: string;
    ingestSecurity: IngestSecurityConfig;
} | null> {
    return apiRequest<{
        serverName: string;
        ingestHost: string;
        ingestSecurity: IngestSecurityConfig;
    }>('/config', { method: 'PATCH', body });
}

async function getCustomEncoding(): Promise<{ ffmpegArgs: string | null } | null> {
    return apiRequest<{ ffmpegArgs: string | null }>('/encodings/custom');
}

async function updateCustomEncoding(ffmpegArgs: string): Promise<{ ffmpegArgs: string } | null> {
    return apiRequest<{ ffmpegArgs: string }>('/encodings/custom', {
        method: 'PUT',
        body: { ffmpegArgs },
    });
}

async function startRecording(
    pipeId: string,
): Promise<{ enabled: boolean; active: boolean } | null> {
    return apiRequest<{ enabled: boolean; active: boolean }>(
        `/pipelines/${encodeURIComponent(pipeId)}/recording/start`,
        { method: 'POST' },
    );
}

async function stopRecording(
    pipeId: string,
): Promise<{ enabled: boolean; active: boolean } | null> {
    return apiRequest<{ enabled: boolean; active: boolean }>(
        `/pipelines/${encodeURIComponent(pipeId)}/recording/stop`,
        { method: 'POST' },
    );
}

export interface MediaFile {
    name: string;
    size: number;
    modifiedAt: string;
    ingestCount?: number;
}

export interface IngestConfig {
    id: string;
    filename: string;
    streamKey: string;
    loop: boolean;
    startTime: string;
    running: boolean;
}

async function listMediaFiles(): Promise<{ files: MediaFile[] } | null> {
    return apiRequest<{ files: MediaFile[] }>('/api/media');
}

async function deleteMediaFile(filename: string): Promise<{ deleted: boolean } | null> {
    return apiRequest<{ deleted: boolean }>(`/api/media/${encodeURIComponent(filename)}`, {
        method: 'DELETE',
    });
}

async function listIngests(): Promise<IngestConfig[] | null> {
    return apiRequest<IngestConfig[]>('/api/ingests');
}

async function createIngest(data: {
    filename: string;
    streamKey: string;
    loop: boolean;
    startTime: string;
}): Promise<IngestConfig | null> {
    return apiRequest<IngestConfig>('/api/ingests', { method: 'POST', body: data });
}

async function updateIngest(
    id: string,
    data: { filename: string; streamKey: string; loop: boolean; startTime: string },
): Promise<IngestConfig | null> {
    return apiRequest<IngestConfig>(`/api/ingests/${encodeURIComponent(id)}`, {
        method: 'PUT',
        body: data,
    });
}

async function deleteIngest(id: string): Promise<{ deleted: boolean } | null> {
    return apiRequest<{ deleted: boolean }>(`/api/ingests/${encodeURIComponent(id)}`, {
        method: 'DELETE',
    });
}

async function startIngest(id: string): Promise<IngestConfig | null> {
    return apiRequest<IngestConfig>(`/api/ingests/${encodeURIComponent(id)}/start`, {
        method: 'POST',
    });
}

async function stopIngest(id: string): Promise<IngestConfig | null> {
    return apiRequest<IngestConfig>(`/api/ingests/${encodeURIComponent(id)}/stop`, {
        method: 'POST',
    });
}

async function logout(): Promise<{ ok: boolean } | null> {
    return apiRequest<{ ok: boolean }>('/api/auth/logout', { method: 'POST' });
}

async function changePassword(
    currentPassword: string,
    newPassword: string,
): Promise<{ ok: boolean } | null> {
    return apiRequest<{ ok: boolean }>('/api/auth/change-password', {
        method: 'POST',
        body: { currentPassword, newPassword },
    });
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
    patchConfig,
    getCustomEncoding,
    updateCustomEncoding,
    startRecording,
    stopRecording,
    listMediaFiles,
    deleteMediaFile,
    listIngests,
    createIngest,
    updateIngest,
    deleteIngest,
    startIngest,
    stopIngest,
    logout,
    changePassword,
};
