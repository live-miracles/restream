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
    const setTitle = (id: string, text: string): void => {
        const el = document.getElementById(id);
        if (el) el.title = text;
    };
    const setMeter = (name: 'cpu' | 'ram' | 'disk', value: number | null | undefined): void => {
        const card = document.getElementById(`navbar-${name}-card`);
        const bar = document.getElementById(`navbar-${name}-bar`) as HTMLElement | null;
        const pctValue = Number(value);
        const safePct = Number.isFinite(pctValue) ? Math.max(0, Math.min(100, pctValue)) : 0;
        if (bar) {
            bar.style.width = `${safePct}%`;
            bar.classList.remove('bg-success', 'bg-warning', 'bg-error');
            bar.classList.add(safePct >= 95 ? 'bg-error' : safePct >= 85 ? 'bg-warning' : 'bg-success');
        }
        if (card) {
            card.classList.remove('border-warning/50', 'bg-warning/10', 'border-error/60', 'bg-error/10');
            if (safePct >= 95) card.classList.add('border-error/60', 'bg-error/10');
            else if (safePct >= 85) card.classList.add('border-warning/50', 'bg-warning/10');
            card.title =
                safePct >= 95
                    ? `${name.toUpperCase()} is at ${Math.round(safePct)}% of provisioned capacity`
                    : safePct >= 85
                      ? `${name.toUpperCase()} is nearing provisioned capacity`
                      : '';
        }
    };

    if (!state.metrics || Object.keys(state.metrics).length === 0) {
        setText('navbar-cpu-value', '...');
        setText('navbar-ram-value', '...');
        setText('navbar-disk-value', '...');
        setText('navbar-down-value', '↓ ...');
        setText('navbar-up-value', '↑ ...');
        setMeter('cpu', null);
        setMeter('ram', null);
        setMeter('disk', null);
        return;
    }

    const pct = (v: number | null | undefined): string =>
        v != null ? `${Math.round(Number(v))}%` : '--';

    const toMbps = (kbps: number | null | undefined): string => {
        const v = Number(kbps);
        return Number.isFinite(v) && v >= 0 ? `${(v / 1000).toFixed(2)} Mb/s` : '--';
    };

    const cpuPct = state.metrics?.cpu?.usagePercent;
    const ramPct = state.metrics?.memory?.usedPercent;
    const diskPct = state.metrics?.disk?.usedPercent;
    const ifaceNames = state.metrics?.network?.interfaces?.map((iface) => iface.name).join(', ');
    const ignored = state.metrics?.network?.ignoredInterfaces?.join(', ');

    setText('navbar-cpu-value', pct(cpuPct));
    setText('navbar-ram-value', pct(ramPct));
    setText('navbar-disk-value', pct(diskPct));
    setText('navbar-down-value', `↓ ${toMbps(state.metrics?.network?.downloadKbps)}`);
    setText('navbar-up-value', `↑ ${toMbps(state.metrics?.network?.uploadKbps)}`);
    setMeter('cpu', cpuPct);
    setMeter('ram', ramPct);
    setMeter('disk', diskPct);
    setTitle(
        'navbar-net-card',
        [
            'Network bitrate from external interfaces only.',
            ifaceNames ? `Included: ${ifaceNames}.` : 'No active external interfaces in the sample.',
            ignored ? `Ignored local/virtual: ${ignored}.` : '',
        ]
            .filter(Boolean)
            .join(' '),
    );
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
