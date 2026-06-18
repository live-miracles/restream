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
            tcpRetransmits: 12,
            tcpUnacked: 20,
            tcpDeliveryRateMbps: 2.1,
        },
    };

    const alerts = getPublisherQualityAlerts(publisher);
    assert.deepEqual(
        alerts.map((alert) => alert.code),
        ['tcp_rtt', 'tcp_retransmits', 'tcp_unacked'],
    );
});

test('publisher quality metrics keep non-alert TCP fields visible in the modal', () => {
    const publisher = {
        protocol: 'rtmp',
        quality: {
            tcpCwnd: 14,
            tcpPacingRateMbps: 6.4,
            tcpSendRateMbps: 3.2,
        },
    };

    const metrics = getPublisherQualityMetrics(publisher);
    assert.deepEqual(
        metrics.map((metric) => metric.code),
        ['tcp_cwnd', 'tcp_pacing_rate', 'tcp_send_rate'],
    );
    assert.equal(
        metrics.every((metric) => metric.isAlert === false),
        true,
    );
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
