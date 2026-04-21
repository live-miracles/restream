'use strict';

// Output retry timing: calculates the delay before the next restart attempt and the
// grace window for correlating a clean exit with a recent input-loss event.
// Both functions read config/env directly so they don't need to be threaded through DI.

const { getConfig } = require('../config');

function getOutputRecoveryConfig() {
    return getConfig().outputRecovery || {};
}

function getRetryDelayMs(failureCount) {
    // Retry policy is split into fixed-delay attempts first, then capped exponential backoff.
    const cfg = getOutputRecoveryConfig();
    const immediateRetries = Number(cfg.immediateRetries || 0);
    const immediateDelayMs = Number(cfg.immediateDelayMs || 1000);
    const backoffRetries = Number(cfg.backoffRetries || 0);
    const backoffBaseDelayMs = Number(cfg.backoffBaseDelayMs || 2000);
    const backoffMaxDelayMs = Number(cfg.backoffMaxDelayMs || backoffBaseDelayMs);
    const totalRetries = immediateRetries + backoffRetries;

    if (failureCount <= 0 || failureCount > totalRetries) {
        return null;
    }

    if (failureCount <= immediateRetries) {
        return immediateDelayMs;
    }

    const backoffAttempt = failureCount - immediateRetries;
    const multiplier = Math.pow(2, Math.max(0, backoffAttempt - 1));
    const delay = backoffBaseDelayMs * multiplier;
    return Math.min(delay, backoffMaxDelayMs);
}

function getInputUnavailableExitGraceMs() {
    // Health snapshots are periodic, so exit-vs-input-loss correlation needs a tolerance window
    // rather than exact timestamp equality. Grace = 3 × snapshot interval, floored at 15 s.
    const healthSnapshotIntervalMs = Number(process.env.HEALTH_SNAPSHOT_INTERVAL_MS || 2000);
    return Math.max(healthSnapshotIntervalMs * 3, 15000);
}

module.exports = {
    getRetryDelayMs,
    getInputUnavailableExitGraceMs,
};
