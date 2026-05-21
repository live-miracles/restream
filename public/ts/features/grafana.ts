import type { OutputView, PipelineView } from '../types.js';

const GRAFANA_OVERVIEW_DASHBOARD_PATH = '/grafana/d/restream-mediamtx-overview/mediamtx-overview';

function buildGrafanaDashboardUrl(pipe: PipelineView, output?: OutputView | null): string {
    const url = new URL(GRAFANA_OVERVIEW_DASHBOARD_PATH, window.location.origin);
    const mediamtxPath = pipe.key ? `live/${pipe.key}` : '';

    url.searchParams.set('orgId', '1');
    url.searchParams.set('from', 'now-30m');
    url.searchParams.set('to', 'now');
    if (mediamtxPath) url.searchParams.set('var-path', mediamtxPath);
    if (output?.id) url.searchParams.set('var-output', `${output.name || output.id}`);

    return `${url.pathname}${url.search}`;
}

function openGrafanaDashboard(pipe: PipelineView, output?: OutputView | null): void {
    window.open(buildGrafanaDashboardUrl(pipe, output), '_blank', 'noopener,noreferrer');
}

export { buildGrafanaDashboardUrl, openGrafanaDashboard };
