import { copyText, showCopiedNotification } from '../core/utils.js';
import { state } from '../core/state.js';
import type { PipelineView } from '../types.js';

interface DiagnosticResult {
    index: number;
    name: string;
    description: string;
    command: string;
    stdout: string;
    stderr: string;
    exitCode: number | null;
    durationMs: number;
    help?: string;
    issues?: string[];
}

interface RunningEvent {
    index: number;
    name: string;
    description: string;
}

let diagnosticsPipeId: string | null = null;
let eventSource: EventSource | null = null;
let results: DiagnosticResult[] = [];
let commandMeta: { name: string; description: string }[] = [];
let expandedSet: Set<number> = new Set();
let runningSet: Set<number> = new Set();
let totalCommands = 0;
let done = false;
let activeProbeProtocol: string = 'rtmp';
let publisherProtocol: string | null = null;
let probeRawStdout: string = '';
let probeRawStderr: string = '';

function getModal(): HTMLDialogElement | null {
    return document.getElementById('diagnostics-modal') as HTMLDialogElement | null;
}

function getPipeline(): PipelineView | undefined {
    return (state.pipelines || []).find((p) => p.id === diagnosticsPipeId);
}

export function openDiagnosticsModal(pipeId: string): void {
    const modal = getModal();
    if (!modal) return;

    diagnosticsPipeId = pipeId;
    const pipe = getPipeline();
    publisherProtocol = pipe?.input.publisher?.protocol || null;
    activeProbeProtocol = publisherProtocol || 'rtmp';

    resetState();

    const title = document.getElementById('diagnostics-title');
    if (title) title.textContent = `Diagnostics — ${pipe?.name || pipeId}`;

    renderControls();
    renderList();
    modal.showModal();
    runDiagnostics();

    modal.addEventListener('close', cleanup, { once: true });
}

function resetState(): void {
    results = [];
    commandMeta = [];
    expandedSet = new Set();
    runningSet = new Set();
    totalCommands = 0;
    done = false;
    probeRawStdout = '';
    probeRawStderr = '';

    const totalTime = document.getElementById('diagnostics-total-time');
    if (totalTime) totalTime.textContent = '';

    for (const id of [
        'diagnostics-copy-all-btn',
        'diagnostics-download-btn',
        'diagnostics-ask-ai-btn',
    ]) {
        const btn = document.getElementById(id) as HTMLButtonElement | null;
        if (btn) {
            btn.classList.add('btn-disabled');
            btn.disabled = true;
        }
    }
}

function renderControls(): void {
    const container = document.getElementById('diagnostics-probe-toggle');
    if (!container) return;

    const isRtmp = activeProbeProtocol === 'rtmp';
    container.innerHTML =
        `<div class="join">` +
        `<button class="join-item btn btn-xs ${isRtmp ? 'btn-accent' : 'btn-ghost'}" data-probe="rtmp">RTMP</button>` +
        `<button class="join-item btn btn-xs ${!isRtmp ? 'btn-accent' : 'btn-ghost'}" data-probe="srt">SRT</button>` +
        `</div>`;

    container.querySelectorAll<HTMLButtonElement>('[data-probe]').forEach((btn) => {
        btn.addEventListener('click', () => {
            const proto = btn.dataset.probe!;
            if (proto === activeProbeProtocol) return;
            activeProbeProtocol = proto;
            cleanup();
            resetState();
            renderControls();
            renderList();
            runDiagnostics();
        });
    });
}

function cleanup(): void {
    if (eventSource) {
        eventSource.close();
        eventSource = null;
    }
    document.getElementById('ai-toast')?.remove();
}

function getPublishStartedAt(): string | null {
    const pipe = getPipeline();
    if (!pipe) return null;
    const health = state.health as Record<string, unknown>;
    const pipelines = health?.pipelines as
        | Record<string, { input?: { publishStartedAt?: string } }>
        | undefined;
    if (!pipelines) return null;
    return pipelines[pipe.id]?.input?.publishStartedAt || null;
}

function runDiagnostics(): void {
    if (!diagnosticsPipeId) return;

    const params = new URLSearchParams();
    params.set('probe', activeProbeProtocol);
    if (publisherProtocol) params.set('publisher', publisherProtocol);
    const since = getPublishStartedAt();
    if (since) params.set('since', since);
    const url = `/pipelines/${encodeURIComponent(diagnosticsPipeId)}/diagnostics?${params}`;

    eventSource = new EventSource(url);

    eventSource.addEventListener('running', (e: Event) => {
        const data = JSON.parse((e as MessageEvent).data) as RunningEvent;
        runningSet.add(data.index);
        commandMeta[data.index] = { name: data.name, description: data.description };
        if (data.index + 1 > totalCommands) totalCommands = data.index + 1;
        renderList();
    });

    eventSource.addEventListener('result', (e: Event) => {
        const data = JSON.parse((e as MessageEvent).data) as DiagnosticResult;
        results[data.index] = data;
        runningSet.delete(data.index);
        if (data.index + 1 > totalCommands) totalCommands = data.index + 1;
        renderList();
    });

    eventSource.addEventListener('probe-raw', (e: Event) => {
        const data = JSON.parse((e as MessageEvent).data) as {
            stdout: string;
            stderr: string;
        };
        probeRawStdout = data.stdout;
        probeRawStderr = data.stderr;
    });

    eventSource.addEventListener('done', (e: Event) => {
        const data = JSON.parse((e as MessageEvent).data);
        done = true;
        runningSet.clear();
        eventSource?.close();
        eventSource = null;

        const totalTime = document.getElementById('diagnostics-total-time');
        if (totalTime) totalTime.textContent = formatDuration(data.totalDurationMs);

        const actions: [string, (() => void) | (() => Promise<void>)][] = [
            ['diagnostics-copy-all-btn', copyAll],
            ['diagnostics-download-btn', downloadLog],
            ['diagnostics-ask-ai-btn', askAi],
        ];
        for (const [id, handler] of actions) {
            const btn = document.getElementById(id) as HTMLButtonElement | null;
            if (btn) {
                btn.classList.remove('btn-disabled');
                btn.disabled = false;
                btn.onclick = handler;
            }
        }

        renderList();
    });

    eventSource.onerror = () => {
        if (!done) {
            done = true;
            runningSet.clear();
            eventSource?.close();
            eventSource = null;
            renderList();
        }
    };
}

function formatDuration(ms: number): string {
    if (ms < 1000) return `${ms}ms`;
    return `${(ms / 1000).toFixed(1)}s`;
}

function escapeHtml(str: string): string {
    return str.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

function colorizeJson(str: string): string {
    return escapeHtml(str).replace(
        /("(?:[^"\\]|\\.)*")(\s*:)?|(\b(?:true|false|null)\b)|(-?\b\d+(?:\.\d+)?(?:[eE][+-]?\d+)?\b)/g,
        (match, strVal, colon, boolNull, num) => {
            if (strVal && colon) return `<span class="text-info">${strVal}</span>${colon}`;
            if (strVal) return `<span class="text-success">${strVal}</span>`;
            if (boolNull) return `<span class="text-warning">${boolNull}</span>`;
            if (num) return `<span class="text-accent">${num}</span>`;
            return match;
        },
    );
}

function formatOutput(str: string): string {
    const trimmed = str.trim();
    if (!trimmed) return '';
    try {
        JSON.parse(trimmed);
        return `<pre class="whitespace-pre-wrap break-words font-mono select-all">${colorizeJson(trimmed)}</pre>`;
    } catch {
        return formatPlainDiagnosticOutput(trimmed);
    }
}

function formatDiagnosticOutput(result: DiagnosticResult): string {
    const trimmed = result.stdout.trim();
    if (!trimmed) return '';
    if (result.name === 'GOP Analysis') return formatGopAnalysisOutput(trimmed);
    if (result.name === 'Ring Buffer Health') return formatRingBufferOutput(trimmed);
    if (result.name === 'Active Outputs') return formatActiveOutputsOutput(trimmed);
    return formatOutput(trimmed);
}

function cleanDiagnosticValue(value: string): string {
    const trimmed = value.trim();
    if (!trimmed || trimmed === 'None') return '--';
    return trimmed
        .replace(/\bNone\b/g, '--')
        .replace(/Some\(([^()]*)\)/g, '$1');
}

function formatDiagnosticMetric(label: string, value: string): string {
    return `<div class="border-base-content/10 bg-base-100/70 rounded-lg border px-3 py-2">
        <div class="text-base-content/55 text-[11px] font-semibold uppercase">${escapeHtml(label)}</div>
        <div class="mt-1 break-words font-mono text-xs">${escapeHtml(cleanDiagnosticValue(value))}</div>
    </div>`;
}

function formatGopAnalysisOutput(str: string): string {
    const metrics: string[] = [];
    const notes: string[] = [];
    for (const rawLine of str.split(/\r?\n/)) {
        const line = rawLine.trim();
        if (!line) continue;
        const minMax = line.match(/^Min:\s*(.*?)\s+Max:\s*(.*)$/);
        if (minMax) {
            metrics.push(formatDiagnosticMetric('Min', minMax[1]));
            metrics.push(formatDiagnosticMetric('Max', minMax[2]));
            continue;
        }
        const kv = line.match(/^([^:]+):\s*(.*)$/);
        if (kv) {
            metrics.push(formatDiagnosticMetric(kv[1], kv[2]));
        } else {
            notes.push(line);
        }
    }
    return `<div class="space-y-2">
        <div class="grid gap-2 sm:grid-cols-2 lg:grid-cols-3">${metrics.join('')}</div>
        ${notes.map((note) => `<div class="text-base-content/70 rounded-lg bg-base-100/70 px-3 py-2 text-xs">${escapeHtml(note)}</div>`).join('')}
    </div>`;
}

function formatRingBufferOutput(str: string): string {
    const metrics: string[] = [];
    const readers: string[] = [];
    const notes: string[] = [];
    let inReaders = false;

    for (const rawLine of str.split(/\r?\n/)) {
        const line = rawLine.trim();
        if (!line) continue;
        if (line === 'Active readers:') {
            inReaders = true;
            continue;
        }
        if (line === 'Active readers: none') {
            notes.push('No active readers are attached.');
            continue;
        }
        const reader = line.match(/^-\s*(.+):\s*lag=(\d+)\s+slots,\s*overflows=(\d+),\s*packet_age=(.+)ms$/);
        if (inReaders && reader) {
            readers.push(`<div class="border-base-content/10 bg-base-100/70 rounded-lg border p-3">
                <div class="truncate text-xs font-semibold" title="${escapeHtml(reader[1])}">${escapeHtml(reader[1])}</div>
                <div class="mt-2 grid grid-cols-3 gap-2">
                    ${formatDiagnosticMetric('Lag', `${reader[2]} slots`)}
                    ${formatDiagnosticMetric('Overflows', reader[3])}
                    ${formatDiagnosticMetric('Packet Age', `${cleanDiagnosticValue(reader[4])} ms`)}
                </div>
            </div>`);
            continue;
        }
        const kv = line.match(/^([^:]+):\s*(.*)$/);
        if (!inReaders && kv) {
            metrics.push(formatDiagnosticMetric(kv[1], kv[2]));
            continue;
        }
        notes.push(line.replace('Buffer is empty — no media has been received recently.', 'Buffer is empty — active readers are caught up with the producer.'));
    }

    return `<div class="space-y-3">
        ${metrics.length ? `<div class="grid gap-2 sm:grid-cols-2 lg:grid-cols-4">${metrics.join('')}</div>` : ''}
        ${readers.length ? `<div><div class="text-base-content/60 mb-2 text-[11px] font-semibold uppercase">Active Readers</div><div class="space-y-2">${readers.join('')}</div></div>` : ''}
        ${notes.map((note) => `<div class="text-base-content/70 rounded-lg bg-base-100/70 px-3 py-2 text-xs">${escapeHtml(note)}</div>`).join('')}
    </div>`;
}

interface OutputDiagnosticBlock {
    id: string;
    fields: Record<string, string>;
    quality: Record<string, string>;
    qualityName: string | null;
}

function parseActiveOutputBlocks(str: string): OutputDiagnosticBlock[] {
    const blocks: OutputDiagnosticBlock[] = [];
    let current: OutputDiagnosticBlock | null = null;
    let section: 'fields' | 'quality' = 'fields';

    for (const rawLine of str.split(/\r?\n/)) {
        const line = rawLine.trim();
        if (!line) continue;
        const output = line.match(/^Output\s+(.+)$/);
        if (output) {
            current = { id: output[1], fields: {}, quality: {}, qualityName: null };
            blocks.push(current);
            section = 'fields';
            continue;
        }
        if (!current) continue;
        const heading = line.match(/^([A-Za-z0-9_]+):$/);
        if (heading) {
            current.qualityName = heading[1];
            section = 'quality';
            continue;
        }
        const kv = line.match(/^([A-Za-z0-9_]+):\s*(.*)$/);
        if (kv) {
            current[section][kv[1]] = kv[2];
        }
    }

    return blocks;
}

function formatActiveOutputsOutput(str: string): string {
    const blocks = parseActiveOutputBlocks(str);
    if (!blocks.length) return formatOutput(str);
    return `<div class="space-y-3">${blocks
        .map((block) => {
            const protocol = cleanDiagnosticValue(block.fields.protocol || '--').toUpperCase();
            const status = cleanDiagnosticValue(block.fields.status || '--');
            const target = cleanDiagnosticValue(block.fields.target || '--');
            const fieldEntries = Object.entries(block.fields).filter(
                ([key]) => !['protocol', 'status', 'target'].includes(key),
            );
            const qualityEntries = Object.entries(block.quality);
            return `<section class="border-base-content/10 bg-base-100/70 rounded-lg border p-3">
                <div class="flex flex-wrap items-center justify-between gap-2">
                    <div class="min-w-0">
                        <div class="text-base-content/55 text-[11px] font-semibold uppercase">Output</div>
                        <div class="truncate font-mono text-xs" title="${escapeHtml(block.id)}">${escapeHtml(block.id)}</div>
                    </div>
                    <div class="flex flex-wrap items-center gap-2">
                        <span class="badge badge-outline">${escapeHtml(protocol)}</span>
                        <span class="badge badge-success">${escapeHtml(status)}</span>
                    </div>
                </div>
                <div class="border-base-content/10 bg-base-200/70 mt-3 rounded-lg border px-3 py-2">
                    <div class="text-base-content/55 text-[11px] font-semibold uppercase">Target</div>
                    <div class="mt-1 break-all font-mono text-xs">${escapeHtml(target)}</div>
                </div>
                ${fieldEntries.length ? `<div class="mt-3 grid gap-2 sm:grid-cols-2 lg:grid-cols-3">${fieldEntries.map(([key, value]) => formatDiagnosticMetric(key, value)).join('')}</div>` : ''}
                ${qualityEntries.length ? `<div class="mt-3">
                    <div class="text-base-content/60 mb-2 text-[11px] font-semibold uppercase">${escapeHtml(block.qualityName || 'quality')}</div>
                    <div class="grid gap-2 sm:grid-cols-2 lg:grid-cols-4">${qualityEntries.map(([key, value]) => formatDiagnosticMetric(key, value)).join('')}</div>
                </div>` : ''}
            </section>`;
        })
        .join('')}</div>`;
}

function formatPlainDiagnosticOutput(str: string): string {
    const lines = str.split(/\r?\n/);
    let html = '<div class="space-y-1">';

    for (const rawLine of lines) {
        const line = rawLine.trimEnd();
        const trimmed = line.trim();
        if (!trimmed) continue;

        const headingMatch = trimmed.endsWith(':') && !trimmed.includes('://');
        if (headingMatch) {
            html += `<div class="text-base-content/70 pt-1 text-[11px] font-semibold uppercase">${escapeHtml(trimmed.slice(0, -1))}</div>`;
            continue;
        }

        const kv = trimmed.match(/^([^:]+):\s*(.*)$/);
        if (kv && !trimmed.includes('://')) {
            html += `<div class="grid grid-cols-[minmax(7rem,14rem)_minmax(0,1fr)] gap-3 rounded bg-base-100/60 px-2 py-1">`;
            html += `<span class="text-base-content/55">${escapeHtml(kv[1])}</span>`;
            html += `<span class="font-mono break-words">${escapeHtml(kv[2] || '--')}</span>`;
            html += `</div>`;
            continue;
        }

        html += `<div class="font-mono break-words rounded bg-base-100/60 px-2 py-1">${escapeHtml(trimmed)}</div>`;
    }

    html += '</div>';
    return html;
}

function renderList(): void {
    const container = document.getElementById('diagnostics-list');
    if (!container) return;

    let html = '';
    const count = Math.max(totalCommands, results.length);

    for (let i = 0; i < count; i++) {
        const result = results[i];
        const isRunning = runningSet.has(i) && !result;
        const expanded = expandedSet.has(i);

        let statusIcon: string;
        let statusClass: string;
        if (result && result.exitCode === 0) {
            if (result.issues && result.issues.length > 0) {
                statusIcon = '&#9888;';
                statusClass = 'text-warning';
            } else {
                statusIcon = '&#10003;';
                statusClass = 'text-success';
            }
        } else if (result && result.exitCode !== 0) {
            statusIcon = '&#10007;';
            statusClass = 'text-error';
        } else if (isRunning) {
            statusIcon = '<span class="loading loading-spinner loading-xs"></span>';
            statusClass = 'text-accent';
        } else {
            statusIcon = '&middot;';
            statusClass = 'opacity-40';
        }

        const meta = commandMeta[i];
        const name = result?.name || meta?.name || `Command ${i + 1}`;
        const description = result?.description || meta?.description || '';
        const timing = result
            ? `<span class="badge badge-sm badge-ghost">${formatDuration(result.durationMs)}</span>`
            : '';

        let issueBadge = '';
        if (result && result.issues && result.issues.length > 0) {
            const cCount = result.issues.length;
            const label = cCount === 1 ? '1 issue' : `${cCount} issues`;
            issueBadge = `<span class="inline-flex items-center justify-center px-1.5 py-0.5 text-[10px] font-bold rounded-full border border-amber-500/30 bg-amber-500/15 text-amber-600 dark:text-amber-400 leading-none h-4.5 shrink-0">${label}</span>`;
        }

        html += `<div class="rounded-lg border border-base-300 bg-base-100 p-3">`;
        html += `<div class="flex items-center justify-between gap-2">`;
        html += `<div class="flex items-center gap-2 flex-1 min-w-0">`;

        if (result) {
            html += `<button class="btn btn-ghost btn-xs btn-square text-lg leading-none" data-toggle="${i}" title="${expanded ? 'Hide output' : 'Show output'}">`;
            html += expanded ? '&#9662;' : '&#9656;';
            html += `</button>`;
        } else {
            html += `<span class="${statusClass} text-base font-bold w-5 text-center">${statusIcon}</span>`;
        }

        html += `<div class="min-w-0 flex-1">`;
        html += `<div class="flex items-center gap-2 flex-wrap">`;
        if (result) html += `<span class="${statusClass} font-bold">${statusIcon}</span>`;
        html += `<span class="font-medium text-sm">${escapeHtml(name)}</span>`;
        html += timing;
        if (issueBadge) html += issueBadge;
        html += `</div>`;
        if (description) {
            html += `<div class="text-xs opacity-50 mt-0.5">${escapeHtml(description)}</div>`;
        }
        html += `</div>`;
        html += `</div>`;

        if (result) {
            html += `<button class="btn btn-xs btn-ghost" data-copy-index="${i}">Copy</button>`;
        }

        html += `</div>`;

        if (isRunning) {
            html += `<div class="mt-2 ml-7 text-sm opacity-60">Running...</div>`;
        }

        if (result && expanded) {
            html += `<div class="mt-2 ml-7">`;

            // Help text for operators
            if (result.help) {
                html += `<div class="mb-2 text-xs opacity-60 italic">${escapeHtml(result.help)}</div>`;
            }

            // RTMP Wall-clock Timing Notice Banner (Step 5)
            if (i === 5 && activeProbeProtocol === 'rtmp') {
                html += `
                <div class="mb-3 border border-blue-500/25 bg-blue-500/10 py-2.5 px-3 shadow-sm text-xs rounded-lg flex items-start gap-2 text-blue-800 dark:text-blue-200">
                    <span class="text-base leading-none shrink-0">💡</span>
                    <div>
                        <span class="font-bold text-blue-600 dark:text-blue-400">Timing Analysis Note:</span> This RTMP diagnostic uses microsecond-level socket arrival timestamps (wall-clock) to actively measure physical TCP Head-of-Line blocking stalls and packet delivery bursts under network stress.
                    </div>
                </div>`;
            }

            // Beautiful alert box for issues
            if (result.issues && result.issues.length > 0) {
                const count = result.issues.length;
                html += `<div class="mb-3 border border-amber-500/25 bg-amber-500/10 py-2.5 px-3 shadow-sm text-xs rounded-lg flex flex-col items-start gap-1">`;
                html += `<div class="font-bold flex items-center gap-1 text-amber-600 dark:text-amber-400">⚠️ Detected Issues (${count}):</div>`;
                html += `<ul class="list-disc pl-5 space-y-1 text-amber-800 dark:text-amber-200">`;
                for (const issue of result.issues) {
                    html += `<li>${escapeHtml(issue)}</li>`;
                }
                html += `</ul>`;
                html += `</div>`;
            }

            html += `<div class="bg-base-200 rounded p-2 text-xs overflow-x-auto max-h-80 overflow-y-auto">`;
            html += `<div class="opacity-50 mb-1 select-all font-mono">$ ${escapeHtml(result.command)}</div>`;

            if (result.stdout) html += formatDiagnosticOutput(result);
            if (result.stderr) {
                html += `<pre class="whitespace-pre-wrap break-words font-mono text-warning select-all">${escapeHtml(result.stderr)}</pre>`;
            }
            if (!result.stdout && !result.stderr) {
                html += `<span class="opacity-40">(no output)</span>`;
            }

            html += `</div></div>`;
        }

        html += `</div>`;
    }

    container.innerHTML = html;

    container.querySelectorAll<HTMLButtonElement>('[data-toggle]').forEach((btn) => {
        btn.addEventListener('click', () => {
            const idx = Number(btn.dataset.toggle);
            if (expandedSet.has(idx)) expandedSet.delete(idx);
            else expandedSet.add(idx);
            renderList();
        });
    });

    container.querySelectorAll<HTMLButtonElement>('[data-copy-index]').forEach((btn) => {
        btn.addEventListener('click', () => {
            const idx = Number(btn.dataset.copyIndex);
            copySingle(idx);
        });
    });

    // Match header right padding to scrollbar width so buttons align with list content
    const header = document.getElementById('diagnostics-header');
    if (header) {
        const scrollbarW = container.offsetWidth - container.clientWidth;
        header.style.paddingRight = scrollbarW > 0 ? `${scrollbarW}px` : '';
    }
}

function formatResultForCopy(r: DiagnosticResult, index: number, total: number): string {
    let text = `--- [${index + 1}/${total}] ${r.name} (${formatDuration(r.durationMs)}) ---\n`;
    text += `${r.description}\n`;
    text += `$ ${r.command}\n`;
    if (r.exitCode !== 0 && r.exitCode !== null) {
        text += `Exit code: ${r.exitCode}\n`;
    }
    if (r.exitCode === null) {
        text += `Exit code: timed out\n`;
    }
    if (r.issues && r.issues.length > 0) {
        text += `⚠️ DETECTED ISSUES:\n`;
        for (const issue of r.issues) {
            text += `  - ${issue}\n`;
        }
        text += `\n`;
    }
    if (r.stdout) text += `${r.stdout}\n`;
    if (r.stderr) text += `[stderr] ${r.stderr}\n`;
    return text;
}

async function copySingle(index: number): Promise<void> {
    const r = results[index];
    if (!r) return;
    const text = formatResultForCopy(r, index, totalCommands);
    if (await copyText(text)) showCopiedNotification();
}

function buildFullReport(): string {
    const pipe = getPipeline();
    return JSON.stringify(
        {
            schema: 'restream.diagnostics.v1',
            generatedAt: new Date().toISOString(),
            pipeline: {
                id: diagnosticsPipeId,
                name: pipe?.name || diagnosticsPipeId,
            },
            probe: activeProbeProtocol,
            totalCommands,
            results: results.filter(Boolean),
            rawProbe: {
                stdout: probeRawStdout || null,
                stderr: probeRawStderr || null,
            },
        },
        null,
        2,
    );
}

async function copyAll(): Promise<void> {
    if (await copyText(buildFullReport())) showCopiedNotification();
}

function downloadLog(): void {
    const pipe = getPipeline();
    const name = (pipe?.name || 'pipeline').replace(/[^a-zA-Z0-9_-]/g, '_');
    const id = (diagnosticsPipeId || 'unknown').replace(/[^a-zA-Z0-9_-]/g, '_');
    const now = new Date().toISOString().replace(/[:.]/g, '-').replace('T', '_').slice(0, 19);
    const filename = `diagnostic-${name}-${id}-${now}.json`;

    const blob = new Blob([buildFullReport()], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = filename;
    a.click();
    URL.revokeObjectURL(url);
}

const AI_SYSTEM_PROMPT = `You are an expert live-streaming and broadcast engineer. I have uploaded a diagnostic report from my RTMP/SRT streaming pipeline (MediaMTX media server → FFmpeg restreaming outputs).

The report includes MediaMTX server connection stats, server logs, and — most importantly — the full raw ffprobe output: every packet (with DTS/PTS/size/flags), every decoded frame, stream metadata, and format info.

Perform your own independent analysis directly from the raw data. Specifically:
- Walk the raw packet sequence: compute interleaving quality, find runs of consecutive same-type packets, measure gaps between audio and video DTS.
- Compute GOP structure from keyframe flags: measure each keyframe interval, find inconsistencies, count B-frames between references.
- Check A/V clock drift: compare audio and video DTS progression over the capture window.
- Measure startup sync: how far apart are the first audio and first video packets/frames.
- Note any DTS violations (non-monotonic timestamps), discontinuities, or ffprobe warnings from stderr.
- Review the MediaMTX server logs for connection errors, resets, or protocol warnings.
- Check publisher transport stats for stalls (flat byte counters) or SRT packet drops.

Output:
1. Stream health verdict: healthy / minor issues / degraded / broken.
2. For each issue found, cite the exact raw data (packet numbers, timestamps, counter values) and explain the viewer impact.
3. Concrete, copy-pasteable encoder fixes (OBS, vMix, FFmpeg CLI, hardware encoders like AJA Bridge Live and TVU) with specific settings.
4. If healthy, confirm and suggest any minor optimizations.`;

async function askAi(): Promise<void> {
    downloadLog();

    const success = await copyText(AI_SYSTEM_PROMPT);
    if (success) {
        showAiToast();
        setTimeout(() => {
            window.open('https://chatgpt.com/', '_blank');
        }, 3000);
    }
}

function showAiToast(): void {
    const existing = document.getElementById('ai-toast');
    if (existing) existing.remove();

    const modal = getModal();
    const container = modal?.querySelector('.modal-box');
    const list = document.getElementById('diagnostics-list');
    if (!container || !list) return;

    const toast = document.createElement('div');
    toast.id = 'ai-toast';
    toast.className =
        'alert alert-success shadow-lg text-xs mb-2 shrink-0 transition-opacity duration-300';
    toast.innerHTML = `
        <div>
            <div class="font-bold text-sm">Prompt copied and report downloaded</div>
            <div>Opening ChatGPT. Upload the JSON report and paste (Ctrl+V) the prompt.</div>
        </div>
    `;

    container.insertBefore(toast, list);

    setTimeout(() => {
        toast.classList.add('opacity-0');
        setTimeout(() => toast.remove(), 300);
    }, 10000);
}
