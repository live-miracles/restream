import { escapeHtml, getUrlParam, sanitizeLogMessage, setUrlParam } from '../core/utils.js';
import { state } from '../core/state.js';
import { openDiagnosticsModal } from './diagnostics.js';
import { fetchProcessingGraph, renderGraphInto } from './graph.js';
import { isOutputIntentStopped, isOutputRunning, isOutputUnexpectedlyDown, selectPipeline } from './render.js';
import type { OutputView, PipelineView } from '../types.js';

type DashboardMode = 'overview' | 'pipeline' | 'inspect' | 'admin';

const validModes = new Set(['overview', 'pipeline', 'inspect', 'admin']);
let currentMode: DashboardMode | null = null;
let inspectPipelineId: string | null = null;
let inspectGraphPipelineId: string | null = null;
let inspectGraphInFlight: Promise<void> | null = null;
let inspectGraphRequestSeq = 0;
let inspectGraphAutoRefresh = true;
let inspectGraphTimer: ReturnType<typeof setInterval> | null = null;
const INSPECT_GRAPH_REFRESH_MS = 5000;
type StatusTone = 'success' | 'warning' | 'error' | 'neutral' | 'info';

function normalizeMode(mode: string | null): DashboardMode {
    if (mode && validModes.has(mode)) return mode as DashboardMode;
    return getUrlParam('p') ? 'pipeline' : 'overview';
}

function formatBitrate(kbps: number | null | undefined): string {
    if (!Number.isFinite(kbps as number) || (kbps as number) < 0) return '--';
    const value = kbps as number;
    return value >= 1000 ? `${(value / 1000).toFixed(1)} Mb/s` : `${value.toFixed(0)} Kb/s`;
}

function formatBytes(bytes: number | null | undefined): string {
    if (!Number.isFinite(bytes as number) || (bytes as number) <= 0) return '--';
    const value = bytes as number;
    if (value < 1024) return `${value} B`;
    if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KiB`;
    if (value < 1024 * 1024 * 1024) return `${(value / (1024 * 1024)).toFixed(1)} MiB`;
    return `${(value / (1024 * 1024 * 1024)).toFixed(1)} GiB`;
}

function badgeClassForTone(tone: StatusTone): string {
    if (tone === 'success') return 'badge-success';
    if (tone === 'warning') return 'badge-warning';
    if (tone === 'error') return 'badge-error';
    if (tone === 'info') return 'badge-info';
    return 'badge-neutral';
}

function statusPill(label: string, tone: StatusTone, detail?: string): string {
    const toneClass =
        tone === 'success'
            ? 'border-success/30 bg-success/10 text-success'
            : tone === 'warning'
              ? 'border-warning/35 bg-warning/10 text-warning'
              : tone === 'error'
                ? 'border-error/35 bg-error/10 text-error'
                : tone === 'info'
                  ? 'border-info/30 bg-info/10 text-info'
                  : 'border-base-content/10 bg-base-100/80 text-base-content/75';
    return `<span class="${toneClass} inline-flex min-h-8 max-w-full items-center gap-2 rounded-lg border px-2.5 py-1 text-xs font-semibold leading-tight">
        <span class="truncate">${escapeHtml(label)}</span>
        ${detail ? `<span class="text-base-content/55 font-normal">${escapeHtml(detail)}</span>` : ''}
    </span>`;
}

function pipelineHealthLabel(pipe: PipelineView): { label: string; cls: string; tone: StatusTone; detail?: string } {
    if (pipe.input.status === 'error') {
        return { label: 'Input error', cls: badgeClassForTone('error'), tone: 'error', detail: 'publisher fault' };
    }
    if (pipe.input.status === 'warning') {
        return { label: 'Input warning', cls: badgeClassForTone('warning'), tone: 'warning', detail: 'check ingest' };
    }
    if (pipe.input.status !== 'on') {
        if (pipe.outs.some(isOutputUnexpectedlyDown)) {
            return { label: 'Input down', cls: badgeClassForTone('error'), tone: 'error', detail: 'outputs blocked' };
        }
        return { label: 'Idle', cls: badgeClassForTone('neutral'), tone: 'neutral', detail: 'waiting for input' };
    }
    if (pipe.outs.some(isOutputUnexpectedlyDown)) {
        return { label: 'Output down', cls: badgeClassForTone('error'), tone: 'error', detail: 'input live' };
    }
    if (pipe.outs.some((out) => out.status === 'warning')) {
        return { label: 'Output warning', cls: badgeClassForTone('warning'), tone: 'warning', detail: 'input live' };
    }
    return { label: 'Live', cls: badgeClassForTone('success'), tone: 'success', detail: 'healthy' };
}

function outputStateLabel(out: OutputView): { label: string; cls: string } {
    if (isOutputIntentStopped(out)) return { label: 'Stopped', cls: 'badge-neutral' };
    if (isOutputRunning(out)) return { label: 'Running', cls: 'badge-success' };
    if (out.status === 'warning') return { label: 'Warning', cls: 'badge-warning' };
    return { label: 'Down', cls: 'badge-error' };
}

function summaryCounts() {
    const outputs = state.pipelines.flatMap((pipe) => pipe.outs);
    return {
        pipelines: state.pipelines.length,
        liveInputs: state.pipelines.filter((pipe) => pipe.input.status === 'on').length,
        warningInputs: state.pipelines.filter((pipe) => pipe.input.status === 'warning').length,
        runningOutputs: outputs.filter(isOutputRunning).length,
        stoppedOutputs: outputs.filter(isOutputIntentStopped).length,
        downOutputs: outputs.filter(isOutputUnexpectedlyDown).length,
        recording: state.pipelines.filter((pipe) => pipe.recording.active).length,
        inputKbps: state.pipelines.reduce((sum, pipe) => sum + (pipe.stats.inputBitrateKbps || 0), 0),
        outputKbps: state.pipelines.reduce((sum, pipe) => sum + (pipe.stats.outputBitrateKbps || 0), 0),
    };
}

function inputOverviewPill(pipe: PipelineView): string {
    const protocol = pipe.input.publisher?.protocol?.toUpperCase();
    const rate = formatBitrate(pipe.stats.inputBitrateKbps);
    if (pipe.input.status === 'on') {
        return statusPill('Live input', 'success', [protocol, rate !== '--' ? rate : null].filter(Boolean).join(' / '));
    }
    if (pipe.input.status === 'warning') {
        return statusPill('Input warning', 'warning', protocol || 'publisher attached');
    }
    if (pipe.input.status === 'error') {
        return statusPill('Input error', 'error', protocol || 'publisher fault');
    }
    return statusPill('No input', 'neutral', pipe.inputSource ? 'file/source idle' : 'waiting');
}

function outputsOverviewPill(pipe: PipelineView): string {
    const total = pipe.outs.length;
    const running = pipe.outs.filter(isOutputRunning).length;
    const stopped = pipe.outs.filter(isOutputIntentStopped).length;
    const down = pipe.outs.filter(isOutputUnexpectedlyDown).length;
    if (!total) return statusPill('No outputs', 'neutral', 'not configured');
    if (pipe.input.status !== 'on' && down > 0) {
        return statusPill(`${running}/${total} running`, 'neutral', 'blocked by input');
    }
    if (down > 0) return statusPill(`${down} down`, 'error', `${running}/${total} running`);
    if (stopped === total) return statusPill('Stopped', 'neutral', `${total} configured`);
    if (running === total) return statusPill(`${running}/${total} running`, 'success');
    return statusPill(`${running}/${total} running`, 'warning', `${stopped} stopped`);
}

function recordingOverviewPill(pipe: PipelineView): string {
    if (pipe.recording.active) return statusPill('Recording', 'error', 'active');
    if (pipe.recording.enabled) return statusPill('Armed', 'warning', 'ready');
    return statusPill('Off', 'neutral');
}

function rateOverviewPill(kbps: number | null | undefined): string {
    const value = formatBitrate(kbps);
    return statusPill(value, value === '--' ? 'neutral' : 'info');
}

function renderOverview(): void {
    const container = document.getElementById('overview-mode-content');
    if (!container) return;

    const counts = summaryCounts();
    const pipelineRows = [...state.pipelines]
        .sort((a, b) => a.name.localeCompare(b.name))
        .map((pipe) => {
            const health = pipelineHealthLabel(pipe);
            return `<tr class="border-base-content/5 hover:bg-base-100/60 border-t">
                <td class="min-w-56 py-3">
                    <button type="button" class="group flex max-w-xs text-left js-open-pipeline" data-pipeline-id="${escapeHtml(pipe.id)}">
                        <span class="group-hover:text-accent truncate font-semibold">${escapeHtml(pipe.name)}</span>
                    </button>
                </td>
                <td>${statusPill(health.label, health.tone, health.detail)}</td>
                <td>${inputOverviewPill(pipe)}</td>
                <td>${outputsOverviewPill(pipe)}</td>
                <td>${rateOverviewPill(pipe.stats.inputBitrateKbps)}</td>
                <td>${rateOverviewPill(pipe.stats.outputBitrateKbps)}</td>
                <td>${recordingOverviewPill(pipe)}</td>
            </tr>`;
        })
        .join('');

    container.innerHTML = `
        <div class="mb-4 grid gap-3 md:grid-cols-2 xl:grid-cols-4">
            ${overviewMetric('Inputs Live', `${counts.liveInputs}/${counts.pipelines}`, counts.warningInputs ? `${counts.warningInputs} warning` : 'All quiet')}
            ${overviewMetric('Outputs Running', `${counts.runningOutputs}`, `${counts.downOutputs} down / ${counts.stoppedOutputs} stopped`)}
            ${overviewMetric('Throughput In', formatBitrate(counts.inputKbps), 'Across active publishers')}
            ${overviewMetric('Throughput Out', formatBitrate(counts.outputKbps), `${counts.recording} active recording${counts.recording === 1 ? '' : 's'}`)}
        </div>
        <section class="border-base-content/10 bg-base-200/80 rounded-lg border">
            <div class="border-base-content/10 flex flex-wrap items-center justify-between gap-2 border-b px-4 py-3">
                <div>
                    <h1 class="text-lg font-semibold">Operator Overview</h1>
                    <p class="text-base-content/60 text-sm">Primary state follows the upstream cause before downstream symptoms.</p>
                </div>
                <button type="button" class="btn btn-sm btn-outline" id="overview-add-pipeline-btn">Add Pipeline</button>
            </div>
            <div class="overflow-x-auto">
                <table class="table table-sm">
                    <thead class="text-base-content/55 bg-base-100/50 text-xs uppercase">
                        <tr>
                            <th>Pipeline</th>
                            <th>State</th>
                            <th>Input</th>
                            <th>Outputs</th>
                            <th>Input Rate</th>
                            <th>Output Rate</th>
                            <th>Recording</th>
                        </tr>
                    </thead>
                    <tbody>${pipelineRows || '<tr><td colspan="7" class="text-base-content/60 px-4 py-6">No pipelines configured.</td></tr>'}</tbody>
                </table>
            </div>
        </section>`;

    container.querySelectorAll<HTMLElement>('.js-open-pipeline').forEach((button) => {
        button.onclick = () => {
            if (!button.dataset.pipelineId) return;
            selectPipeline(button.dataset.pipelineId);
            setDashboardMode('pipeline');
        };
    });
    const addBtn = document.getElementById('overview-add-pipeline-btn');
    if (addBtn) addBtn.onclick = () => void window.addPipeBtn();
}

function overviewMetric(label: string, value: string, note: string): string {
    return `<section class="border-base-content/10 bg-base-200 rounded-lg border p-4">
        <div class="text-base-content/60 text-xs font-semibold uppercase">${escapeHtml(label)}</div>
        <div class="mt-2 text-2xl font-semibold tabular-nums">${escapeHtml(value)}</div>
        <div class="text-base-content/60 mt-1 text-sm">${escapeHtml(note)}</div>
    </section>`;
}

function getInspectPipeline(): PipelineView | null {
    const selectedFromUrl = getUrlParam('p');
    if (inspectPipelineId && state.pipelines.some((pipe) => pipe.id === inspectPipelineId)) {
        return state.pipelines.find((pipe) => pipe.id === inspectPipelineId) || null;
    }
    if (selectedFromUrl && state.pipelines.some((pipe) => pipe.id === selectedFromUrl)) {
        inspectPipelineId = selectedFromUrl;
        return state.pipelines.find((pipe) => pipe.id === selectedFromUrl) || null;
    }
    inspectPipelineId = state.pipelines[0]?.id || null;
    return state.pipelines[0] || null;
}

function renderInspect(): void {
    const pipe = getInspectPipeline();
    const select = document.getElementById('inspect-pipeline-select') as HTMLSelectElement | null;
    if (select) {
        select.innerHTML = state.pipelines
            .map((p) => `<option value="${escapeHtml(p.id)}">${escapeHtml(p.name)}</option>`)
            .join('');
        select.value = pipe?.id || '';
        select.onchange = () => {
            inspectPipelineId = select.value || null;
            resetInspectGraphForSelection(inspectPipelineId);
            renderInspect();
            void refreshInspectGraph();
        };
    }
    if (!pipe && inspectGraphPipelineId !== null) {
        resetInspectGraphForSelection(null);
    }

    const openBtn = document.getElementById('inspect-open-pipeline-btn') as HTMLButtonElement | null;
    if (openBtn) {
        openBtn.disabled = !pipe;
        openBtn.onclick = () => {
            if (pipe) {
                selectPipeline(pipe.id);
                setDashboardMode('pipeline');
            }
        };
    }

    renderInspectSummary(pipe);
    renderInspectDiagnostics(pipe);

    const refreshBtn = document.getElementById('inspect-refresh-graph-btn') as HTMLButtonElement | null;
    if (refreshBtn) {
        refreshBtn.textContent = inspectGraphAutoRefresh ? 'Stop Refresh' : 'Auto Refresh';
        refreshBtn.classList.toggle('btn-accent', inspectGraphAutoRefresh);
        refreshBtn.classList.toggle('btn-outline', !inspectGraphAutoRefresh);
        refreshBtn.setAttribute('aria-pressed', inspectGraphAutoRefresh ? 'true' : 'false');
        refreshBtn.onclick = () => {
            inspectGraphAutoRefresh = !inspectGraphAutoRefresh;
            syncInspectGraphAutoRefresh();
            renderInspect();
            if (inspectGraphAutoRefresh) void refreshInspectGraph();
        };
    }
    const diagBtn = document.getElementById('inspect-open-diagnostics-btn') as HTMLButtonElement | null;
    if (diagBtn) {
        diagBtn.disabled = !pipe || pipe.input.status !== 'on';
        diagBtn.onclick = () => {
            if (pipe) openDiagnosticsModal(pipe.id);
        };
    }

    if (pipe && inspectGraphPipelineId !== pipe.id) {
        void refreshInspectGraph();
    }
    syncInspectGraphAutoRefresh();
}

function resetInspectGraphForSelection(pipeId: string | null): void {
    inspectGraphRequestSeq++;
    inspectGraphPipelineId = pipeId;
    const status = document.getElementById('inspect-graph-status');
    const container = document.getElementById('inspect-graph-container');
    if (status) status.textContent = pipeId ? 'Loading graph...' : 'Select a pipeline.';
    if (container) {
        container.innerHTML = `<div class="text-base-content/60 flex h-full min-h-72 items-center justify-center text-sm">
            ${pipeId ? 'Loading graph...' : 'Select a pipeline to inspect its graph.'}
        </div>`;
    }
}

function renderInspectSummary(pipe: PipelineView | null): void {
    const container = document.getElementById('inspect-pipeline-summary');
    if (!container) return;
    if (!pipe) {
        container.innerHTML = '<div class="text-base-content/60 text-sm">No pipeline selected.</div>';
        return;
    }

    const health = pipelineHealthLabel(pipe);
    const outputs = pipe.outs
        .map((out) => {
            const stateLabel = outputStateLabel(out);
            return `<div class="flex items-center justify-between gap-2 border-base-content/10 border-t py-2">
                <div class="min-w-0">
                    <div class="truncate text-sm font-medium">${escapeHtml(out.name)}</div>
                    <div class="text-base-content/60 truncate text-xs">${escapeHtml(out.encoding)} / ${sanitizeLogMessage(out.url, true)}</div>
                </div>
                <span class="badge ${stateLabel.cls} shrink-0">${stateLabel.label}</span>
            </div>`;
        })
        .join('');

    container.innerHTML = `<section class="border-base-content/10 bg-base-200 rounded-lg border p-3">
        <div class="mb-2 flex items-center justify-between gap-2">
            <h2 class="font-semibold">${escapeHtml(pipe.name)}</h2>
            <span class="badge ${health.cls}">${health.label}</span>
        </div>
        <dl class="grid grid-cols-2 gap-2 text-sm">
            <div><dt class="text-base-content/60">Input</dt><dd>${escapeHtml(pipe.input.status)}</dd></div>
            <div><dt class="text-base-content/60">Publisher</dt><dd>${escapeHtml(pipe.input.publisher?.protocol || '--')}</dd></div>
            <div><dt class="text-base-content/60">Input Rate</dt><dd>${formatBitrate(pipe.stats.inputBitrateKbps)}</dd></div>
            <div><dt class="text-base-content/60">Output Rate</dt><dd>${formatBitrate(pipe.stats.outputBitrateKbps)}</dd></div>
            <div><dt class="text-base-content/60">Received</dt><dd>${formatBytes(pipe.input.bytesReceived)}</dd></div>
            <div><dt class="text-base-content/60">Sent</dt><dd>${formatBytes(pipe.input.bytesSent)}</dd></div>
        </dl>
        <div class="mt-3">${outputs || '<div class="text-base-content/60 text-sm">No outputs configured.</div>'}</div>
    </section>`;
}

function renderInspectDiagnostics(pipe: PipelineView | null): void {
    const container = document.getElementById('inspect-diagnostics-summary');
    if (!container) return;
    if (!pipe) {
        container.innerHTML = '<div class="text-base-content/60 text-sm">Select a pipeline to inspect diagnostics.</div>';
        return;
    }

    const blockers: string[] = [];
    if (pipe.input.status !== 'on') blockers.push('Input must be online for active probes.');
    if (!pipe.input.publisher?.protocol) blockers.push('Publisher protocol is not known yet.');
    const downOutputs = pipe.outs.filter(isOutputUnexpectedlyDown);

    container.innerHTML = `<div class="grid gap-3 md:grid-cols-3">
        <div class="bg-base-100 rounded-lg p-3">
            <div class="text-base-content/60 text-xs font-semibold uppercase">Probe Readiness</div>
            <div class="mt-2 text-sm">${blockers.length ? blockers.map(escapeHtml).join('<br>') : 'Ready for active diagnostics.'}</div>
        </div>
        <div class="bg-base-100 rounded-lg p-3">
            <div class="text-base-content/60 text-xs font-semibold uppercase">Fault Candidates</div>
            <div class="mt-2 text-sm">${downOutputs.length ? downOutputs.map((out) => escapeHtml(out.name)).join('<br>') : 'No unexpected output failures.'}</div>
        </div>
        <div class="bg-base-100 rounded-lg p-3">
            <div class="text-base-content/60 text-xs font-semibold uppercase">Suggested Next Step</div>
            <div class="mt-2 text-sm">${pipe.input.status === 'on' ? 'Run diagnostics, then inspect graph edges with zero packet output.' : 'Start or reconnect the publisher before probing.'}</div>
        </div>
    </div>`;
}

async function refreshInspectGraph(): Promise<void> {
    const pipe = getInspectPipeline();
    const status = document.getElementById('inspect-graph-status');
    const container = document.getElementById('inspect-graph-container');
    if (!pipe || !container) return;
    const requestPipeId = pipe.id;
    const requestSeq = ++inspectGraphRequestSeq;
    inspectGraphPipelineId = requestPipeId;
    if (status) status.textContent = 'Loading graph...';
    container.innerHTML = `<div class="text-base-content/60 flex h-full min-h-72 items-center justify-center text-sm">
        Loading graph...
    </div>`;
    inspectGraphInFlight = (async () => {
        const graph = await fetchProcessingGraph(requestPipeId);
        if (requestSeq !== inspectGraphRequestSeq || getInspectPipeline()?.id !== requestPipeId) {
            return;
        }
        inspectGraphPipelineId = requestPipeId;
        if (!graph || !container || graph.pipelineId !== requestPipeId) {
            if (status) status.textContent = 'Graph unavailable.';
            container.innerHTML =
                '<div class="text-base-content/60 flex h-full min-h-72 items-center justify-center text-sm">Graph unavailable.</div>';
            return;
        }
        renderGraphInto(container, graph as Parameters<typeof renderGraphInto>[1]);
        if (status) {
            const nodeCount = (graph as { nodes?: unknown[] }).nodes?.length || 0;
            const inputState = pipe.input.status === 'on' ? 'live' : pipe.input.status;
            status.textContent = `${pipe.name} / ${nodeCount} nodes / input ${inputState}`;
        }
    })();
    try {
        await inspectGraphInFlight;
    } finally {
        if (requestSeq === inspectGraphRequestSeq) {
            inspectGraphInFlight = null;
        }
    }
}

function syncInspectGraphAutoRefresh(): void {
    if (inspectGraphTimer) {
        clearInterval(inspectGraphTimer);
        inspectGraphTimer = null;
    }
    if (!inspectGraphAutoRefresh || currentMode !== 'inspect' || !getInspectPipeline()) return;
    inspectGraphTimer = setInterval(() => {
        if (!document.hidden) void refreshInspectGraph();
    }, INSPECT_GRAPH_REFRESH_MS);
}

function renderAdmin(): void {
    const container = document.getElementById('admin-mode-content');
    if (!container) return;
    const security = state.config.ingestSecurity;
    const profileCount = Object.keys(state.config.transcodeProfiles || {}).length;
    container.innerHTML = `<div class="grid gap-4 lg:grid-cols-[minmax(0,1fr)_minmax(18rem,24rem)]">
        <section class="border-base-content/10 bg-base-200 rounded-lg border p-4">
            <h1 class="text-lg font-semibold">Admin</h1>
            <div class="mt-4 grid gap-3 md:grid-cols-3">
                ${adminLink('Settings', 'settings.html', 'Server name, ingest host, security, media ingest, passwords.')}
                ${adminLink('Status', 'status.html', 'Runtime build, native libraries, and system health.')}
                ${adminLink('GitHub', 'https://github.com/live-miracles/restream', 'Source repository and release history.')}
            </div>
        </section>
        <aside class="border-base-content/10 bg-base-200 rounded-lg border p-4">
            <h2 class="font-semibold">Configuration Snapshot</h2>
            <dl class="mt-3 grid gap-2 text-sm">
                <div class="flex justify-between gap-3"><dt class="text-base-content/60">Server</dt><dd>${escapeHtml(state.config.serverName || 'Restream')}</dd></div>
                <div class="flex justify-between gap-3"><dt class="text-base-content/60">Ingest Host</dt><dd>${escapeHtml(state.config.ingestHost || 'localhost')}</dd></div>
                <div class="flex justify-between gap-3"><dt class="text-base-content/60">Pipelines</dt><dd>${state.pipelines.length}</dd></div>
                <div class="flex justify-between gap-3"><dt class="text-base-content/60">Profiles</dt><dd>${profileCount}</dd></div>
                <div class="flex justify-between gap-3"><dt class="text-base-content/60">Failure Limit</dt><dd>${security?.failureLimit ?? '--'}</dd></div>
                <div class="flex justify-between gap-3"><dt class="text-base-content/60">Tracked IP Limit</dt><dd>${security?.trackedIpLimit ?? '--'}</dd></div>
            </dl>
        </aside>
    </div>`;
}

function adminLink(label: string, href: string, description: string): string {
    return `<a class="border-base-content/10 bg-base-100 hover:bg-base-300 rounded-lg border p-3" href="${escapeHtml(href)}">
        <div class="font-semibold">${escapeHtml(label)}</div>
        <div class="text-base-content/60 mt-1 text-sm">${escapeHtml(description)}</div>
    </a>`;
}

function refreshActiveMode(): void {
    renderDashboardModes();
}

function applyMode(mode: DashboardMode): void {
    currentMode = mode;
    const panels: Record<DashboardMode, HTMLElement | null> = {
        overview: document.getElementById('overview-mode-panel'),
        pipeline: document.getElementById('dashboard-grid'),
        inspect: document.getElementById('inspect-mode-panel'),
        admin: document.getElementById('admin-mode-panel'),
    };
    for (const [name, panel] of Object.entries(panels)) {
        panel?.classList.toggle('hidden', name !== mode);
    }

    document.querySelectorAll<HTMLButtonElement>('[data-dashboard-mode]').forEach((button) => {
        const active = button.dataset.dashboardMode === mode;
        button.classList.toggle('btn-accent', active);
        button.classList.toggle('btn-outline', !active);
        button.setAttribute('aria-pressed', active ? 'true' : 'false');
    });

    const summary = document.getElementById('workspace-mode-summary');
    if (summary) {
        const counts = summaryCounts();
        summary.textContent =
            mode === 'overview'
                ? `${counts.liveInputs} live inputs / ${counts.runningOutputs} running outputs`
                : mode === 'pipeline'
                  ? 'Pipeline workflow'
                  : mode === 'inspect'
                    ? 'Graph and diagnostics'
                    : 'Configuration and runtime';
    }
    syncInspectGraphAutoRefresh();
}

export function setDashboardMode(mode: string): void {
    const nextMode = normalizeMode(mode);
    setUrlParam('mode', nextMode);
    if (currentMode === nextMode) {
        applyMode(nextMode);
        return;
    }
    refreshActiveMode();
}

export function openInspectGraph(pipeId: string): void {
    inspectPipelineId = pipeId;
    resetInspectGraphForSelection(pipeId);
    setUrlParam('p', pipeId);
    setDashboardMode('inspect');
    void refreshInspectGraph();
}

export function renderDashboardModes(): void {
    renderOverview();
    renderInspect();
    renderAdmin();
    applyMode(normalizeMode(getUrlParam('mode')));
}

export function initDashboardModes(): void {
    document.querySelectorAll<HTMLButtonElement>('[data-dashboard-mode]').forEach((button) => {
        button.onclick = () => setDashboardMode(button.dataset.dashboardMode || 'overview');
    });
    window.addEventListener('popstate', refreshActiveMode);
    window.setDashboardMode = setDashboardMode;
    refreshActiveMode();
}
