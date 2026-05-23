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
        return colorizeJson(trimmed);
    } catch {
        return escapeHtml(trimmed);
    }
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

            if (result.stdout) {
                html += `<pre class="whitespace-pre-wrap break-all font-mono select-all">${formatOutput(result.stdout)}</pre>`;
            }
            if (result.stderr) {
                html += `<pre class="whitespace-pre-wrap break-all font-mono text-warning select-all">${escapeHtml(result.stderr)}</pre>`;
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
    const lines: string[] = [];
    lines.push('=== Restream Diagnostics ===');
    lines.push(
        `Pipeline: ${pipe?.name || diagnosticsPipeId} | Probe: ${activeProbeProtocol.toUpperCase()} | ${new Date().toISOString()}`,
    );
    lines.push('');

    for (let i = 0; i < results.length; i++) {
        const r = results[i];
        if (!r) continue;
        lines.push(formatResultForCopy(r, i, totalCommands));
    }

    // Append raw ffprobe output for post-mortem analysis
    if (probeRawStdout) {
        lines.push('');
        lines.push('=== RAW FFPROBE OUTPUT (packets + frames + streams + format) ===');
        lines.push(probeRawStdout);
    }
    if (probeRawStderr) {
        lines.push('');
        lines.push('=== RAW FFPROBE STDERR (warnings) ===');
        lines.push(probeRawStderr);
    }

    return lines.join('\n');
}

async function copyAll(): Promise<void> {
    if (await copyText(buildFullReport())) showCopiedNotification();
}

function downloadLog(): void {
    const pipe = getPipeline();
    const name = (pipe?.name || 'pipeline').replace(/[^a-zA-Z0-9_-]/g, '_');
    const id = (diagnosticsPipeId || 'unknown').replace(/[^a-zA-Z0-9_-]/g, '_');
    const now = new Date().toISOString().replace(/[:.]/g, '-').replace('T', '_').slice(0, 19);
    const filename = `diagnostic-${name}-${id}-${now}.log`;

    const blob = new Blob([buildFullReport()], { type: 'text/plain' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = filename;
    a.click();
    URL.revokeObjectURL(url);
}

