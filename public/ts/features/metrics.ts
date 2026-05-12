import { state } from '../core/state.js';

const HEALTH_RECOVERY_BANNER_MS = 6000;
let previousHealthStatus: string | null = null;
let recoveryBannerVisible = false;
let recoveryBannerTimer: ReturnType<typeof setTimeout> | null = null;
let activeHealthBannerState: 'hidden' | 'degraded' | 'recovered' = 'hidden';
let dismissedHealthBannerState: string | null = null;

function dismissHealthBanner(): void {
    const banner = document.getElementById('health-banner');
    if (!banner || activeHealthBannerState === 'hidden') return;

    dismissedHealthBannerState = activeHealthBannerState;
    recoveryBannerVisible = false;
    if (recoveryBannerTimer) {
        clearTimeout(recoveryBannerTimer);
        recoveryBannerTimer = null;
    }
    banner.classList.add('hidden');
}

function getHealthBannerState(currentStatus: string | null): 'hidden' | 'degraded' | 'recovered' {
    if (currentStatus === 'degraded') return 'degraded';
    if (previousHealthStatus === 'degraded') return 'recovered';
    return 'hidden';
}

function renderServerMetrics(): void {
    const setText = (id: string, text: string): void => {
        const el = document.getElementById(id);
        if (el) el.textContent = text;
    };

    if (!state.metrics || Object.keys(state.metrics).length === 0) {
        setText('navbar-cpu-value', 'CPU ...');
        setText('navbar-ram-value', 'RAM ...');
        setText('navbar-disk-value', 'Disk ...');
        setText('navbar-down-value', '↓ ...');
        setText('navbar-up-value', '↑ ...');
        return;
    }

    const toGiB = (bytes: number | null | undefined): string =>
        (Number(bytes || 0) / (1024 * 1024 * 1024)).toFixed(1);

    const pct = (v: number | null | undefined): string =>
        v != null ? `${Math.round(Number(v))}%` : '--';

    const toMbps = (kbps: number | null | undefined): string => {
        const v = Number(kbps);
        return Number.isFinite(v) && v >= 0 ? `${(v / 1000).toFixed(2)} Mb/s` : '--';
    };

    const cores = state.metrics?.cpu?.cores;
    const cpuStr =
        cores != null
            ? `${cores}c CPU: ${pct(state.metrics?.cpu?.usagePercent)}`
            : `CPU: ${pct(state.metrics?.cpu?.usagePercent)}`;

    const totalRamGiB =
        state.metrics?.memory?.totalBytes != null ? toGiB(state.metrics.memory.totalBytes) : null;
    const ramStr =
        totalRamGiB != null
            ? `${totalRamGiB}G RAM: ${pct(state.metrics?.memory?.usedPercent)}`
            : `RAM: ${pct(state.metrics?.memory?.usedPercent)}`;

    const totalDiskGiB =
        state.metrics?.disk?.totalBytes != null ? toGiB(state.metrics.disk.totalBytes) : null;
    const diskStr =
        totalDiskGiB != null
            ? `${totalDiskGiB}G Disk: ${pct(state.metrics?.disk?.usedPercent)}`
            : `Disk: ${pct(state.metrics?.disk?.usedPercent)}`;

    setText('navbar-cpu-value', cpuStr);
    setText('navbar-ram-value', ramStr);
    setText('navbar-disk-value', diskStr);
    setText('navbar-down-value', `↓ ${toMbps(state.metrics?.network?.downloadKbps)}`);
    setText('navbar-up-value', `↑ ${toMbps(state.metrics?.network?.uploadKbps)}`);
}

function renderHealthBanner(): void {
    const banner = document.getElementById('health-banner');
    const text = document.getElementById('health-banner-text');
    if (!banner || !text) return;

    const currentStatus = state.health?.status || null;
    const bannerState = getHealthBannerState(currentStatus);

    if (bannerState !== activeHealthBannerState) {
        activeHealthBannerState = bannerState;
        if (dismissedHealthBannerState && dismissedHealthBannerState !== bannerState) {
            dismissedHealthBannerState = null;
        }
    }

    if (bannerState === 'degraded') {
        recoveryBannerVisible = false;
        if (recoveryBannerTimer) {
            clearTimeout(recoveryBannerTimer);
            recoveryBannerTimer = null;
        }

        banner.classList.remove('alert-success');
        banner.classList.add('alert-warning');
        text.innerText = 'Service is degraded: runtime telemetry is temporarily unavailable.';
        if (dismissedHealthBannerState === bannerState) {
            banner.classList.add('hidden');
        } else {
            banner.classList.remove('hidden');
        }
        previousHealthStatus = currentStatus;
        return;
    }

    if (bannerState === 'recovered') {
        banner.classList.remove('alert-warning');
        banner.classList.add('alert-success');
        text.innerText = 'Service recovered: runtime telemetry is available again.';

        if (dismissedHealthBannerState === bannerState) {
            recoveryBannerVisible = false;
            banner.classList.add('hidden');
        } else {
            banner.classList.remove('hidden');
            recoveryBannerVisible = true;
            if (recoveryBannerTimer) clearTimeout(recoveryBannerTimer);
            recoveryBannerTimer = setTimeout(() => {
                recoveryBannerVisible = false;
                banner.classList.add('hidden');
            }, HEALTH_RECOVERY_BANNER_MS);
        }

        previousHealthStatus = currentStatus;
        return;
    }

    if (recoveryBannerTimer) {
        clearTimeout(recoveryBannerTimer);
        recoveryBannerTimer = null;
    }
    recoveryBannerVisible = false;
    banner.classList.add('hidden');
    previousHealthStatus = currentStatus;
}

document
    .getElementById('dismiss-health-banner-btn')
    ?.addEventListener('click', dismissHealthBanner);

export { dismissHealthBanner, renderHealthBanner, renderServerMetrics };
