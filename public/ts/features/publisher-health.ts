import { getHealth } from '../core/api.js';
import { parsePipelinesInfo } from '../core/pipeline.js';
import { state } from '../core/state.js';
import type { PipelineView } from '../types.js';
import {
    getPublisherQualityEmptyMessage,
    getPublisherQualityMetrics,
    normalizePublisherProtocolLabel,
} from './publisher-quality.js';
import { buildGrafanaDashboardUrl, buildSrtConnectionHealthDashboardUrl } from './grafana.js';

let publisherHealthModalPipeId: string | null = null;
let modalRefreshTimer: ReturnType<typeof setInterval> | null = null;
let modalRefreshInFlight = false;

/** Matches the backend HEALTH_SNAPSHOT_INTERVAL_MS (2 s). */
const MODAL_REFRESH_INTERVAL_MS = 2000;

function getModal(): HTMLDialogElement | null {
    return document.getElementById('publisher-health-modal') as HTMLDialogElement | null;
}

function getPipeline(): PipelineView | undefined {
    return (state.pipelines || []).find((p) => p.id === publisherHealthModalPipeId);
}

function startModalRefreshTimer(): void {
    if (modalRefreshTimer) return;
    modalRefreshTimer = setInterval(() => {
        if (modalRefreshInFlight) return;
        modalRefreshInFlight = true;
        getHealth()
            .then((healthResult) => {
                if (healthResult) {
                    state.health = healthResult;
                    state.pipelines = parsePipelinesInfo(state.config, state.health);
                }
            })
            .catch(() => {
                // Silently ignore — the dashboard's own poll will retry.
            })
            .finally(() => {
                modalRefreshInFlight = false;
                renderPublisherHealthModal();
            });
    }, MODAL_REFRESH_INTERVAL_MS);
}

function stopModalRefreshTimer(): void {
    if (modalRefreshTimer) {
        clearInterval(modalRefreshTimer);
        modalRefreshTimer = null;
    }
}

export function renderPublisherHealthModal(): void {
    const modal = getModal();
    if (!modal || !modal.open) return;

    const pipe = getPipeline();
    const publisher = pipe?.input?.publisher || null;
    const title = document.getElementById('publisher-health-title');
    const subtitle = document.getElementById('publisher-health-subtitle');
    const tbody = document.getElementById('publisher-health-rows');
    const empty = document.getElementById('publisher-health-empty');
    const grafanaBtn = document.getElementById(
        'publisher-health-grafana-btn',
    ) as HTMLButtonElement | null;
    const copyBtn = document.getElementById(
        'publisher-health-copy-btn',
    ) as HTMLButtonElement | null;
    if (!title || !subtitle || !tbody || !empty || !grafanaBtn) return;

    title.textContent = `Publisher Health${pipe?.name ? ` - ${pipe.name}` : ''}`;

    if (!publisher || !pipe) {
        subtitle.textContent = 'No active publisher.';
        tbody.innerHTML = '';
        empty.textContent = 'Start a publisher to inspect transport health.';
        empty.classList.remove('hidden');
        grafanaBtn.disabled = true;
        grafanaBtn.classList.add('btn-disabled');
        grafanaBtn.onclick = null;
        if (copyBtn) {
            copyBtn.disabled = true;
            copyBtn.onclick = null;
        }
        return;
    }

    const proto = normalizePublisherProtocolLabel(publisher.protocol);
    subtitle.textContent = `${proto} | ${publisher.remoteAddr || 'unknown remote'}`;

    const rows = getPublisherQualityMetrics(publisher);
    tbody.innerHTML = rows
        .map(
            (row) => `<tr>
                <td title="${row.description}">${row.label} <span class="text-xs opacity-40">&#9432;</span></td>
                <td class="text-right font-mono">${row.displayValue}</td>
                <td class="text-right"><span class="badge badge-xs ${row.isAlert ? 'badge-warning' : 'badge-success'}">${row.isAlert ? 'Alert' : 'OK'}</span></td>
            </tr>`,
        )
        .join('');

    empty.textContent = getPublisherQualityEmptyMessage(publisher);
    empty.classList.toggle('hidden', rows.length > 0);

    if (copyBtn) {
        copyBtn.disabled = rows.length === 0;
        copyBtn.onclick = () => {
            const header = `${title.textContent}\n${subtitle.textContent}`;
            const lines = rows.map(
                (row) => `${row.label}: ${row.displayValue} [${row.isAlert ? 'Alert' : 'OK'}]`,
            );
            navigator.clipboard.writeText([header, '', ...lines].join('\n')).then(() => {
                copyBtn.textContent = 'Copied!';
                setTimeout(() => {
                    copyBtn.textContent = 'Copy';
                }, 1500);
            });
        };
    }

    grafanaBtn.disabled = !pipe.key;
    grafanaBtn.classList.toggle('btn-disabled', !pipe.key);
    grafanaBtn.onclick = () => {
        if (!pipe.key) return;
        const url =
            publisher.protocol === 'srt'
                ? buildSrtConnectionHealthDashboardUrl(pipe)
                : buildGrafanaDashboardUrl(pipe);
        window.open(url, '_blank', 'noopener,noreferrer');
    };
}

export function openPublisherHealthModal(pipeId: string): void {
    publisherHealthModalPipeId = pipeId;
    const modal = getModal();
    if (!modal) return;

    // Stop any previous timer before (re-)opening.
    stopModalRefreshTimer();

    if (!modal.open) {
        modal.showModal();
    }

    renderPublisherHealthModal();
    startModalRefreshTimer();
}

// When the modal is closed (via backdrop click, Close button, or Escape),
// stop the dedicated refresh timer so we don't poll unnecessarily.
function initModalCloseListener(): void {
    const modal = getModal();
    if (!modal) return;
    modal.addEventListener('close', () => {
        stopModalRefreshTimer();
    });
    // Also handle cancel (Escape key) for <dialog>.
    modal.addEventListener('cancel', () => {
        stopModalRefreshTimer();
    });
}

initModalCloseListener();
