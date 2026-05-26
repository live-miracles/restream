import type { OutputView, PipelineView } from '../types.js';

const GRAFANA_OVERVIEW_DASHBOARD_PATH = '/grafana/d/restream-mediamtx-overview/mediamtx-overview';
const GRAFANA_SRT_CONNECTION_HEALTH_DASHBOARD_PATH =
    '/grafana/d/restream-srt-connection-health/srt-connection-health';

function setCommonDashboardParams(url: URL, pipe: PipelineView): void {
    url.searchParams.set('orgId', '1');
    url.searchParams.set('from', 'now-30m');
    url.searchParams.set('to', 'now');

    const mediamtxPath = pipe.key ? `live/${pipe.key}` : '';
    if (mediamtxPath) url.searchParams.set('var-path', mediamtxPath);
}

function buildGrafanaDashboardUrl(pipe: PipelineView, output?: OutputView | null): string {
    const url = new URL(GRAFANA_OVERVIEW_DASHBOARD_PATH, window.location.origin);

    setCommonDashboardParams(url, pipe);
    if (output?.id) url.searchParams.set('var-output', `${output.name || output.id}`);

    return `${url.pathname}${url.search}`;
}

function buildSrtConnectionHealthDashboardUrl(pipe: PipelineView): string {
    const url = new URL(GRAFANA_SRT_CONNECTION_HEALTH_DASHBOARD_PATH, window.location.origin);

    setCommonDashboardParams(url, pipe);

    return `${url.pathname}${url.search}`;
}

function openGrafanaDashboard(pipe: PipelineView, output?: OutputView | null): void {
    window.open(buildGrafanaDashboardUrl(pipe, output), '_blank', 'noopener,noreferrer');
}

function openSrtConnectionHealthDashboard(pipe: PipelineView): void {
    window.open(buildSrtConnectionHealthDashboardUrl(pipe), '_blank', 'noopener,noreferrer');
}

export {
    buildGrafanaDashboardUrl,
    buildSrtConnectionHealthDashboardUrl,
    openGrafanaDashboard,
    openSrtConnectionHealthDashboard,
};
