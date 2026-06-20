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
        metrics.push({
            code,
            label,
            description,
            value: Math.round(num),
            displayValue: formatter ? formatter(num) : String(Math.round(num)),
            isAlert: alertCheck(num),
        });
    };

    if (publisher.protocol === 'srt') {
        if (q.srtBonded) {
            addNumericMetric({
                code: 'srt_bond_members',
                label: 'Bond member links',
                description:
                    'Number of network paths currently attached to this libsrt socket group.',
                rawValue: q.srtGroupMemberCount,
                alertCheck: (v) => v < 2,
                alwaysShow: true,
            });
            addNumericMetric({
                code: 'srt_bond_active',
                label: 'Bond active links',
                description:
                    'Member links currently carrying data for this bonded SRT publisher.',
                rawValue: q.srtGroupActiveMembers,
                alertCheck: (v) => v < 1,
                alwaysShow: true,
            });
            addNumericMetric({
                code: 'srt_bond_broken',
                label: 'Bond broken links',
                description:
                    'Member links that libsrt reports as broken. Any broken path reduces redundancy.',
                rawValue: q.srtGroupBrokenMembers,
                alertCheck: (v) => v > 0,
                alwaysShow: true,
            });
        }
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
            description: 'Cumulative RTP packets received with corrupt headers or invalid sequences.',
            rawValue: q.inboundRTPPacketsInError,
            alertCheck: (v) => v >= 20,
        });
        addNumericMetric({
            code: 'jitter',
            label: 'Jitter (inbound RTP)',
            description:
                'Variation in packet arrival time. High jitter can cause choppy playback even without loss.',
            rawValue: q.inboundRTPPacketsJitter,
            alertCheck: (v) => v >= 30,
        });
        addNumericMetric({
            code: 'rtt',
            label: 'RTT (ms)',
            description:
                'SRT round-trip time between publisher and receiver. Alert threshold: 200 ms.',
            rawValue: q.msRTT,
            alertCheck: (v) => v >= 200,
        });
        addNumericMetric({
            code: 'srt_recv_rate',
            label: 'Receive rate (Mbps)',
            description: 'Current inbound bitrate reported by the SRT receiver.',
            rawValue: q.mbpsReceiveRate,
            alertCheck: () => false,
            formatter: (v) => v.toFixed(2),
        });
        addNumericMetric({
            code: 'srt_negotiated_latency_buffer',
            label: 'Negotiated latency buffer (ms)',
            description:
                'Agreed timestamp-based delivery delay. Larger values tolerate jitter but add latency.',
            rawValue: q.msReceiveTsbPdDelay,
            alertCheck: () => false,
        });
        addNumericMetric({
            code: 'srt_current_latency_buffer',
            label: 'Current latency buffer (ms)',
            description:
                'Current receive-buffer timespan. Approaching the negotiated buffer increases late-drop risk.',
            rawValue: q.msReceiveBuf,
            alertCheck: () => false,
        });
        addNumericMetric({
            code: 'srt_link_capacity',
            label: 'Estimated network capacity (Mbps)',
            description: 'SRT estimate of available bandwidth between publisher and receiver.',
            rawValue: q.mbpsLinkCapacity,
            alertCheck: () => false,
            formatter: (v) => v.toFixed(2),
        });
        addNumericMetric({
            code: 'srt_naks_sent',
            label: 'NAKs sent',
            description: 'Negative acknowledgements requesting retransmission of lost packets.',
            rawValue: q.packetsSentNAK,
            alertCheck: () => false,
        });
        addNumericMetric({
            code: 'srt_loss_rate',
            label: 'Packets lost (SRT)',
            description:
                'Current loss rate. Alert threshold: 5/s. Total is cumulative for this connection.',
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
                'Packets arriving too late for the latency buffer. Alert threshold: 1/s.',
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
                'Rate of retransmitted packets received. Alert threshold: 10/s.',
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
                'Packets that could not be decrypted. Any non-zero rate indicates an encryption mismatch.',
            rawValue: q.packetsReceivedUndecryptPerSec,
            alertCheck: (v) => v > 0,
            alwaysShow: true,
            formatter: (v) =>
                `${v.toFixed(1)}/s (${Math.round(Number(q.packetsReceivedUndecrypt || 0))} total)`,
        });
    }

    if (publisher.protocol === 'rtmp') {
        addNumericMetric({
            code: 'tcp_recv_rate',
            label: 'Receive rate (Mbps)',
            description:
                'Inbound throughput computed from the receiver TCP bytes_received delta.',
            rawValue: q.tcpReceiveRateMbps,
            alertCheck: () => false,
            alwaysShow: true,
            formatter: (v) => v.toFixed(2),
        });
        addNumericMetric({
            code: 'tcp_rtt',
            label: 'TCP RTT (ms)',
            description: 'Smoothed TCP round-trip time. Alert threshold: 200 ms.',
            rawValue: q.tcpRttMs,
            alertCheck: (v) => v >= 200,
            formatter: (v) => v.toFixed(1),
        });
        addNumericMetric({
            code: 'tcp_rtt_var',
            label: 'TCP RTT variance (ms)',
            description: 'Variation in TCP RTT measurements; higher values indicate an unstable path.',
            rawValue: q.tcpRttVarMs,
            alertCheck: () => false,
            formatter: (v) => v.toFixed(1),
        });
        addNumericMetric({
            code: 'tcp_rcv_rtt',
            label: 'TCP receive RTT (ms)',
            description:
                'Receiver-side RTT estimate used for delayed ACK scheduling.',
            rawValue: q.tcpRcvRttMs,
            alertCheck: () => false,
            alwaysShow: true,
            formatter: (v) => v.toFixed(1),
        });
        addNumericMetric({
            code: 'tcp_lastrcv',
            label: 'Time since last recv (ms)',
            description:
                'Milliseconds since the receiver saw data. Alert threshold: 5000 ms.',
            rawValue: q.tcpLastRcvMs,
            alertCheck: (v) => v >= 5000,
            alwaysShow: true,
        });
        addNumericMetric({
            code: 'tcp_rcv_ooopack',
            label: 'Out-of-order packets (HOL)',
            description:
                'Out-of-order packets can cause TCP head-of-line blocking. Alert threshold: 50.',
            rawValue: q.tcpRcvOoopack,
            alertCheck: (v) => v >= 50,
            alwaysShow: true,
        });
        addNumericMetric({
            code: 'tcp_rcv_space',
            label: 'Receive window',
            description:
                'TCP receive window advertised to the sender. A shrinking window means the receiver is falling behind.',
            rawValue: q.tcpRcvSpace,
            alertCheck: () => false,
            formatter: formatBytes,
        });
        addNumericMetric({
            code: 'tcp_skmem_rmem',
            label: 'Recv buffer (used / max)',
            description:
                'Kernel socket receive-buffer occupancy. Alert when more than 80% full.',
            rawValue: q.tcpSkmemRmemAlloc,
            alertCheck: () =>
                q.tcpSkmemRmemAlloc != null &&
                q.tcpSkmemRmemMax != null &&
                q.tcpSkmemRmemMax > 0 &&
                q.tcpSkmemRmemAlloc / q.tcpSkmemRmemMax > 0.8,
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
        if (reason === 'collection_failed') {
            return 'RTMP receiver-side TCP socket metrics could not be read from the active connection.';
        }
        return 'RTMP receiver-side TCP socket metrics are not available from this host.';
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
