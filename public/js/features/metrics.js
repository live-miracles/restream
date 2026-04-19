import { setMetricsBitrateWithSubtleUnit, setMetricsValueWithSubtleUnit } from './metric-format.js';
import { state } from '../core/state.js';

const HEALTH_RECOVERY_BANNER_MS = 6000;
    let previousHealthStatus = null;
    let recoveryBannerVisible = false;
    let recoveryBannerTimer = null;
    let activeHealthBannerState = 'hidden';
    let dismissedHealthBannerState = null;

    function dismissHealthBanner() {
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

    function getHealthBannerState(currentStatus) {
        if (currentStatus === 'degraded') return 'degraded';
        if (previousHealthStatus === 'degraded') return 'recovered';
        return 'hidden';
    }

    function renderServerMetrics() {
        const setAll = (selector, value) =>
            document.querySelectorAll(selector).forEach((elem) => {
                elem.innerText = value;
            });

        if (!state.metrics || Object.keys(state.metrics).length === 0) {
            setAll('.cpu-metric', '...');
            setAll('.ram-metric', '...');
            setAll('.disk-metric', '...');
            setAll('.downlink-metric', '...');
            setAll('.uplink-metric', '...');
            return;
        }

        const toGiB = (bytes) => (Number(bytes || 0) / (1024 * 1024 * 1024)).toFixed(1);

        const cpuParts =
            state.metrics?.cpu?.usagePercent !== null && state.metrics?.cpu?.usagePercent !== undefined
                ? { valueText: state.metrics.cpu.usagePercent.toFixed(1), unitText: '%' }
                : null;
        const ramParts =
            state.metrics?.memory?.usedBytes !== null && state.metrics?.memory?.totalBytes !== null
                ? {
                      valueText: `${toGiB(state.metrics.memory.usedBytes)}/${toGiB(state.metrics.memory.totalBytes)}`,
                      unitText: 'G',
                  }
                : null;
        const diskParts =
            state.metrics?.disk?.usedPercent !== null && state.metrics?.disk?.usedPercent !== undefined
                ? { valueText: state.metrics.disk.usedPercent.toFixed(1), unitText: '%' }
                : null;
        const downKbps = state.metrics?.network?.downloadKbps;
        const upKbps = state.metrics?.network?.uploadKbps;

        setMetricsValueWithSubtleUnit('.cpu-metric', cpuParts);
        setMetricsValueWithSubtleUnit('.ram-metric', ramParts);
        setMetricsValueWithSubtleUnit('.disk-metric', diskParts);
        setMetricsBitrateWithSubtleUnit('.downlink-metric', downKbps);
        setMetricsBitrateWithSubtleUnit('.uplink-metric', upKbps);
    }

    function renderHealthBanner() {
        // The banner behaves like a small state machine: degraded stays visible, recovered shows
        // briefly, and hidden means either healthy steady-state or user dismissal.
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
