import type { Publisher } from '../types.js';

function normalizePublisherProtocolLabel(protocol: string): string {
    const map: Record<string, string> = { rtmp: 'RTMP', srt: 'SRT' };
    return map[protocol] || String(protocol || '').toUpperCase();
}

interface QualityMetric {
    code: string;
    label: string;
    description: string;
    value: number;
    displayValue: string;
    isAlert: boolean;
}

interface NumericMetricArgs {
    code: string;
    label: string;
    description: string;
    rawValue: number | null | undefined;
    alertCheck: (v: number) => boolean;
    formatter?: (v: number) => string;
    alwaysShow?: boolean;
}

function formatBytes(bytes: number): string {
    if (bytes >= 1_000_000) return `${(bytes / 1_000_000).toFixed(1)} MB`;
    if (bytes >= 1_000) return `${(bytes / 1_000).toFixed(0)} KB`;
    return `${bytes} B`;
}

function getPublisherQualityMetrics(publisher: Publisher | null): QualityMetric[] {
    if (!publisher) return [];

    const q = publisher.quality || {};
    const metrics: QualityMetric[] = [];
    const proto = publisher.protocol;

    const addNumericMetric = ({
        code,
        label,
        description,
        rawValue,
        alertCheck,
        formatter,
        alwaysShow,
    }: NumericMetricArgs): void => {
        if (rawValue === null || rawValue === undefined) {
            if (alwaysShow) {
                metrics.push({
                    code,
                    label,
                    description,
                    value: 0,
                    displayValue: '—',
                    isAlert: false,
                });
            }
            return;
        }
        const num = Number(rawValue) || 0;
        const rounded = Math.round(num);
        metrics.push({
            code,
            label,
            description,
            value: rounded,
            displayValue: formatter ? formatter(num) : String(rounded),
            isAlert: !!alertCheck(rounded),
        });
    };

    // --- SRT-only metrics ---
    if (proto === 'srt') {
        addNumericMetric({
            code: 'rtp_loss',
            label: 'Packets lost (inbound RTP)',
            description:
                'Cumulative packets lost on the inbound RTP stream. High values indicate network packet loss.',
            rawValue: q.inboundRTPPacketsLost,
            alertCheck: (v) => v >= 100,
        });
        addNumericMetric({
            code: 'rtp_err',
            label: 'Packets in error (inbound RTP)',
            description:
                'Cumulative RTP packets received with errors (corrupt headers, invalid sequences).',
            rawValue: q.inboundRTPPacketsInError,
            alertCheck: (v) => v >= 20,
        });
        addNumericMetric({
            code: 'jitter',
            label: 'Jitter (inbound RTP)',
            description:
                'Variation in packet arrival time. High jitter causes choppy playback even without loss.',
            rawValue: q.inboundRTPPacketsJitter,
            alertCheck: (v) => v >= 30,
        });
        addNumericMetric({
            code: 'rtt',
            label: 'RTT (ms)',
            description:
                'SRT round-trip time between publisher and receiver. Affects retransmission recovery speed.',
            rawValue: q.msRTT,
            alertCheck: (v) => v >= 200,
        });
        addNumericMetric({
            code: 'srt_recv_rate',
            label: 'Receive rate (Mbps)',
            description: 'Current inbound bitrate as reported by the SRT protocol.',
            rawValue: q.mbpsReceiveRate,
            alertCheck: () => false,
            formatter: (v) => v.toFixed(2),
        });
        addNumericMetric({
            code: 'srt_negotiated_latency_buffer',
            label: 'Negotiated Latency Buffer (ms)',
            description:
                'Agreed-upon latency buffer (TsbPd) between publisher and receiver. Larger values tolerate more jitter but add delay.',
            rawValue: q.msReceiveTsbPdDelay,
            alertCheck: () => false,
        });
        addNumericMetric({
            code: 'srt_current_latency_buffer',
            label: 'Current Latency Buffer (ms)',
            description:
                'Current receive buffer fill level. If it approaches the negotiated buffer, packets may be dropped.',
            rawValue: q.msReceiveBuf,
            alertCheck: () => false,
        });
        addNumericMetric({
            code: 'srt_link_capacity',
            label: 'Estimated Network Capacity (Mbps)',
            description:
                'SRT estimate of the available network bandwidth between publisher and receiver.',
            rawValue: q.mbpsLinkCapacity,
            alertCheck: () => false,
            formatter: (v) => v.toFixed(2),
        });
        addNumericMetric({
            code: 'srt_naks_sent',
            label: 'NAKs Sent',
            description:
                'Negative acknowledgements sent, requesting retransmission of lost packets.',
            rawValue: q.packetsSentNAK,
            alertCheck: () => false,
        });
        addNumericMetric({
            code: 'srt_loss_rate',
            label: 'Packets lost (SRT)',
            description:
                'Rate of packets detected as lost by SRT. Alert threshold: 5/s. Total is cumulative since connection start.',
            rawValue: q.packetsReceivedLossPerSec,
            alertCheck: (v) => v >= 5,
            alwaysShow: true,
            formatter: (v) =>
                `${v.toFixed(1)}/s (${Math.round(Number(q.packetsReceivedLoss || 0))} total)`,
        });
        addNumericMetric({
            code: 'srt_drop_rate',
            label: 'Packets dropped (SRT)',
            description:
                'Rate of packets dropped (arrived too late for the latency buffer). Alert threshold: 1/s. Drops cause visible glitches.',
            rawValue: q.packetsReceivedDropPerSec,
            alertCheck: (v) => v >= 1,
            alwaysShow: true,
            formatter: (v) =>
                `${v.toFixed(1)}/s (${Math.round(Number(q.packetsReceivedDrop || 0))} total)`,
        });
        addNumericMetric({
            code: 'srt_retrans_rate',
            label: 'Retransmissions (SRT)',
            description:
                'Rate of retransmitted packets received. High retransmissions indicate network loss being recovered by SRT.',
            rawValue: q.packetsReceivedRetransPerSec,
            alertCheck: (v) => v >= 10,
            alwaysShow: true,
            formatter: (v) =>
                `${v.toFixed(1)}/s (${Math.round(Number(q.packetsReceivedRetrans || 0))} total)`,
        });
        addNumericMetric({
            code: 'srt_undecrypt_rate',
            label: 'Undecrypted (SRT)',
            description:
                'Rate of packets that could not be decrypted. Non-zero means an encryption key mismatch.',
            rawValue: q.packetsReceivedUndecryptPerSec,
            alertCheck: (v) => v > 0,
            alwaysShow: true,
            formatter: (v) =>
                `${v.toFixed(1)}/s (${Math.round(Number(q.packetsReceivedUndecrypt || 0))} total)`,
        });
    }

    // --- RTMP-only metrics (receiver-side TCP from ss) ---
    if (proto === 'rtmp') {
        addNumericMetric({
            code: 'tcp_recv_rate',
            label: 'Receive rate (Mbps)',
            description:
                'Inbound data rate computed from TCP bytes_received delta. Shows actual throughput on the receiver side.',
            rawValue: q.tcpReceiveRateMbps,
            alertCheck: () => false,
            alwaysShow: true,
            formatter: (v) => v.toFixed(2),
        });
        addNumericMetric({
            code: 'tcp_rtt',
            label: 'TCP RTT (ms)',
            description:
                'Smoothed round-trip time estimated by the TCP stack. High RTT slows loss recovery.',
            rawValue: q.tcpRttMs,
            alertCheck: (v) => v >= 200,
            formatter: (v) => v.toFixed(1),
        });
        addNumericMetric({
            code: 'tcp_rtt_var',
            label: 'TCP RTT variance (ms)',
            description:
                'Variation in RTT measurements. High variance indicates an unstable network path.',
            rawValue: q.tcpRttVarMs,
            alertCheck: () => false,
            formatter: (v) => v.toFixed(1),
        });
        addNumericMetric({
            code: 'tcp_rcv_rtt',
            label: 'TCP receive RTT (ms)',
            description:
                'Receiver-side RTT estimate used for delayed ACK scheduling. Spikes indicate the receiver is stalling.',
            rawValue: q.tcpRcvRttMs,
            alertCheck: () => false,
            alwaysShow: true,
            formatter: (v) => v.toFixed(1),
        });
        addNumericMetric({
            code: 'tcp_lastrcv',
            label: 'Time since last recv (ms)',
            description:
                'Milliseconds since the last data packet was received. Alert at 5000 ms — the publisher may have stalled.',
            rawValue: q.tcpLastRcvMs,
            alertCheck: (v) => v >= 5000,
            alwaysShow: true,
        });
        addNumericMetric({
            code: 'tcp_rcv_ooopack',
            label: 'Out-of-order packets (HOL)',
            description:
                'Packets received out of order. High values indicate head-of-line blocking — TCP holds data in the reorder queue, stalling the application.',
            rawValue: q.tcpRcvOoopack,
            alertCheck: (v) => v >= 50,
            alwaysShow: true,
        });
        addNumericMetric({
            code: 'tcp_rcv_space',
            label: 'Receive window',
            description:
                'TCP receive window advertised to the sender. Shrinking window means the receiver is falling behind.',
            rawValue: q.tcpRcvSpace,
            alertCheck: () => false,
            formatter: (v) => formatBytes(v),
        });
        addNumericMetric({
            code: 'tcp_skmem_rmem',
            label: 'Recv buffer (used / max)',
            description:
                'Kernel socket receive buffer usage. Alert when >80% full — the application cannot drain data fast enough, risking drops.',
            rawValue: q.tcpSkmemRmemAlloc,
            alertCheck: () => {
                if (
                    q.tcpSkmemRmemAlloc != null &&
                    q.tcpSkmemRmemMax != null &&
                    q.tcpSkmemRmemMax > 0
                ) {
                    return q.tcpSkmemRmemAlloc / q.tcpSkmemRmemMax > 0.8;
                }
                return false;
            },
            formatter: (v) =>
                q.tcpSkmemRmemMax != null
                    ? `${formatBytes(v)} / ${formatBytes(q.tcpSkmemRmemMax)}`
                    : formatBytes(v),
        });
    }

    return metrics;
}

function getPublisherQualityEmptyMessage(publisher: Publisher | null): string {
    if (!publisher) return 'Start a publisher to inspect transport health.';

    if (publisher.protocol === 'rtmp') {
        const reason = publisher.quality?.tcpStatsUnavailableReason;
        if (reason === 'not_linux') {
            return 'RTMP TCP socket metrics are only available when Restream runs on Linux.';
        }
        if (reason === 'ss_missing') {
            return 'RTMP TCP socket metrics require the Linux ss tool. Install the iproute2 package on this host.';
        }
        if (reason === 'collection_failed') {
            return 'RTMP TCP socket metrics could not be collected from ss on this host.';
        }
        if (reason === 'no_matching_socket') {
            return 'RTMP is publishing, but no matching TCP socket stats were found yet.';
        }
        return 'RTMP TCP socket metrics are not available from this host.';
    }

    return 'No protocol-specific transport metrics available for this publisher.';
}

interface QualityAlert {
    code: string;
    label: string;
}

function getPublisherQualityAlerts(publisher: Publisher | null): QualityAlert[] {
    return getPublisherQualityMetrics(publisher)
        .filter((metric) => metric.isAlert)
        .map((metric) => ({
            code: metric.code,
            label: `${metric.label}: ${metric.displayValue}`,
        }));
}

export {
    normalizePublisherProtocolLabel,
    getPublisherQualityAlerts,
    getPublisherQualityMetrics,
    getPublisherQualityEmptyMessage,
};
