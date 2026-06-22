const test = require('node:test');
const assert = require('node:assert/strict');

const {
    getPublisherQualityEmptyMessage,
    getPublisherQualityAlerts,
    getPublisherQualityMetrics,
} = require('../../public/ts/features/publisher-quality');

test('RTMP TCP socket metrics contribute alerts for unhealthy publishers', () => {
    const publisher = {
        protocol: 'rtmp',
        quality: {
            tcpRttMs: 240.5,
            tcpLastRcvMs: 6000,
            tcpRcvOoopack: 60,
        },
    };

    const alerts = getPublisherQualityAlerts(publisher);
    assert.deepEqual(
        alerts.map((alert) => alert.code),
        ['tcp_rtt', 'tcp_lastrcv', 'tcp_rcv_ooopack'],
    );
});

test('publisher quality metrics keep non-alert receiver TCP fields visible in the modal', () => {
    const publisher = {
        protocol: 'rtmp',
        quality: {
            tcpRcvRttMs: 12.5,
            tcpRcvSpace: 65536,
            tcpSkmemRmemAlloc: 4096,
            tcpSkmemRmemMax: 2097152,
        },
    };

    const metrics = getPublisherQualityMetrics(publisher);
    const present = metrics.map((metric) => metric.code);
    for (const expected of ['tcp_rcv_rtt', 'tcp_rcv_space', 'tcp_skmem_rmem']) {
        assert.ok(present.includes(expected), `${expected} should be present`);
    }
    assert.equal(
        metrics.every((metric) => metric.isAlert === false),
        true,
    );
});

test('SRT rate-based metrics alert on current rates, not cumulative totals', () => {
    const publisher = {
        protocol: 'srt',
        quality: {
            packetsReceivedLoss: 5000,
            packetsReceivedDrop: 500,
            packetsReceivedRetrans: 10000,
            packetsReceivedLossPerSec: 0,
            packetsReceivedDropPerSec: 0,
            packetsReceivedRetransPerSec: 0,
            packetsReceivedUndecryptPerSec: 0,
        },
    };

    const alerts = getPublisherQualityAlerts(publisher);
    assert.equal(alerts.length, 0, 'No alerts when rates are zero despite high cumulative totals');
});

test('SRT rate-based metrics trigger alerts when rates exceed thresholds', () => {
    const publisher = {
        protocol: 'srt',
        quality: {
            packetsReceivedLossPerSec: 10,
            packetsReceivedDropPerSec: 3,
        },
    };

    const alerts = getPublisherQualityAlerts(publisher);
    assert.deepEqual(
        alerts.map((a) => a.code),
        ['srt_loss_rate', 'srt_drop_rate'],
    );
});

test('SRT rate metrics display rate and cumulative total inline', () => {
    const publisher = {
        protocol: 'srt',
        quality: {
            packetsReceivedLoss: 1234,
            packetsReceivedLossPerSec: 2.5,
        },
    };

    const metrics = getPublisherQualityMetrics(publisher);
    const loss = metrics.find((m) => m.code === 'srt_loss_rate');
    assert.ok(loss, 'srt_loss_rate metric should be present');
    assert.equal(loss.displayValue, '2.5/s (1234 total)');
    assert.equal(loss.isAlert, false, 'rate 2.5 is below threshold 5');
});

test('publisher quality empty message explains RTMP TCP stats availability', () => {
    assert.equal(
        getPublisherQualityEmptyMessage({
            protocol: 'rtmp',
            quality: { tcpStatsUnavailableReason: 'not_linux' },
        }),
        'RTMP TCP socket metrics are only available when Restream runs on Linux.',
    );
    assert.equal(
        getPublisherQualityEmptyMessage({
            protocol: 'rtmp',
            quality: { tcpStatsUnavailableReason: 'ss_missing' },
        }),
        'RTMP TCP socket metrics require the Linux ss tool. Install the iproute2 package on this host.',
    );
});
