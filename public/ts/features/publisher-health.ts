import { state } from '../core/state.js';
import type { PipelineView } from '../types.js';
import {
    getPublisherQualityEmptyMessage,
    getPublisherQualityMetrics,
    normalizePublisherProtocolLabel,
} from './publisher-quality.js';
import {
    buildGrafanaDashboardUrl,
    buildSrtConnectionHealthDashboardUrl,
} from './grafana.js';

let publisherHealthModalPipeId: string | null = null;

function getModal(): HTMLDialogElement | null {
    return document.getElementById('publisher-health-modal') as HTMLDialogElement | null;
}

function getPipeline(): PipelineView | undefined {
    return (state.pipelines || []).find((p) => p.id === publisherHealthModalPipeId);
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
        return;
    }

    const proto = normalizePublisherProtocolLabel(publisher.protocol);
    subtitle.textContent = `${proto} | ${publisher.remoteAddr || 'unknown remote'}`;

    const rows = getPublisherQualityMetrics(publisher);
    tbody.innerHTML = rows
        .map(
            (row) => `<tr>
                <td>${row.label}</td>
                <td class="text-right font-mono">${row.displayValue}</td>
                <td class="text-right"><span class="badge badge-xs ${row.isAlert ? 'badge-warning' : 'badge-success'}">${row.isAlert ? 'Alert' : 'OK'}</span></td>
            </tr>`,
        )
        .join('');

    empty.textContent = getPublisherQualityEmptyMessage(publisher);
    empty.classList.toggle('hidden', rows.length > 0);

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
    if (!modal.open) modal.showModal();
    renderPublisherHealthModal();
}
