import type { Publisher } from '../types.js';

function normalizePublisherProtocolLabel(protocol: string): string {
    const map: Record<string, string> = { rtmp: 'RTMP', srt: 'SRT' };
    return map[protocol] || String(protocol || '').toUpperCase();
}

interface QualityMetric {
    code: string;
    label: string;
    value: number;
    displayValue: string;
    isAlert: boolean;
}

interface NumericMetricArgs {
    code: string;
    label: string;
    rawValue: number | null | undefined;
    alertCheck: (v: number) => boolean;
    formatter?: (v: number) => string;
}

function getPublisherQualityMetrics(publisher: Publisher | null): QualityMetric[] {
    if (!publisher) return [];

    const q = publisher.quality || {};
    const metrics: QualityMetric[] = [];

    const addNumericMetric = ({
        code,
        label,
        rawValue,
        alertCheck,
        formatter,
    }: NumericMetricArgs): void => {
        if (rawValue === null || rawValue === undefined) return;
        const num = Number(rawValue) || 0;
        const rounded = Math.round(num);
        metrics.push({
            code,
            label,
            value: rounded,
            displayValue: formatter ? formatter(num) : String(rounded),
            isAlert: !!alertCheck(rounded),
        });
    };

    addNumericMetric({
        code: 'rtp_loss',
        label: 'Packets lost (inbound RTP)',
        rawValue: q.inboundRTPPacketsLost,
        alertCheck: (v) => v >= 100,
    });
    addNumericMetric({
        code: 'rtp_err',
        label: 'Packets in error (inbound RTP)',
        rawValue: q.inboundRTPPacketsInError,
        alertCheck: (v) => v >= 20,
    });
    addNumericMetric({
        code: 'jitter',
        label: 'Jitter (inbound RTP)',
        rawValue: q.inboundRTPPacketsJitter,
        alertCheck: (v) => v >= 30,
    });
    addNumericMetric({
        code: 'rtt',
        label: 'RTT (ms)',
        rawValue: q.msRTT,
        alertCheck: (v) => v >= 200,
    });
    addNumericMetric({
        code: 'srt_recv_rate',
        label: 'Receive rate (Mbps)',
        rawValue: q.mbpsReceiveRate,
        alertCheck: () => false,
        formatter: (v) => v.toFixed(2),
    });
    addNumericMetric({
        code: 'srt_negotiated_latency_buffer',
        label: 'Negotiated Latency Buffer (ms)',
        rawValue: q.msReceiveTsbPdDelay,
        alertCheck: () => false,
    });
    addNumericMetric({
        code: 'srt_current_latency_buffer',
        label: 'Current Latency Buffer (ms)',
        rawValue: q.msReceiveBuf,
        alertCheck: () => false,
    });
    addNumericMetric({
        code: 'srt_link_capacity',
        label: 'Estimated Network Capacity (Mbps)',
        rawValue: q.mbpsLinkCapacity,
        alertCheck: () => false,
        formatter: (v) => v.toFixed(2),
    });
    addNumericMetric({
        code: 'srt_naks_sent',
        label: 'NAKs Sent',
        rawValue: q.packetsSentNAK,
        alertCheck: () => false,
    });
    addNumericMetric({
        code: 'srt_loss',
        label: 'Packets lost (SRT received)',
        rawValue: q.packetsReceivedLoss,
        alertCheck: (v) => v >= 100,
    });
    addNumericMetric({
        code: 'srt_drop',
        label: 'Packets dropped (SRT received)',
        rawValue: q.packetsReceivedDrop,
        alertCheck: (v) => v >= 10,
    });
    addNumericMetric({
        code: 'srt_retrans',
        label: 'Packets retransmitted (SRT)',
        rawValue: q.packetsReceivedRetrans,
        alertCheck: (v) => v >= 200,
    });
    addNumericMetric({
        code: 'srt_undecrypt',
        label: 'Packets undecrypted (SRT)',
        rawValue: q.packetsReceivedUndecrypt,
        alertCheck: (v) => v > 0,
    });
    addNumericMetric({
        code: 'tcp_rtt',
        label: 'TCP RTT (ms)',
        rawValue: q.tcpRttMs,
        alertCheck: (v) => v >= 200,
        formatter: (v) => v.toFixed(1),
    });
    addNumericMetric({
        code: 'tcp_rtt_var',
        label: 'TCP RTT variance (ms)',
        rawValue: q.tcpRttVarMs,
        alertCheck: () => false,
        formatter: (v) => v.toFixed(1),
    });
    addNumericMetric({
        code: 'tcp_retransmits',
        label: 'TCP retransmissions',
        rawValue: q.tcpRetransmits,
        alertCheck: (v) => v >= 10,
    });
    addNumericMetric({
        code: 'tcp_unacked',
        label: 'TCP unacked segments',
        rawValue: q.tcpUnacked,
        alertCheck: (v) => v >= 16,
    });
    addNumericMetric({
        code: 'tcp_cwnd',
        label: 'TCP congestion window (segments)',
        rawValue: q.tcpCwnd,
        alertCheck: () => false,
    });
    addNumericMetric({
        code: 'tcp_delivery_rate',
        label: 'TCP delivery rate (Mbps)',
        rawValue: q.tcpDeliveryRateMbps,
        alertCheck: () => false,
        formatter: (v) => v.toFixed(3),
    });
    addNumericMetric({
        code: 'tcp_pacing_rate',
        label: 'TCP pacing rate (Mbps)',
        rawValue: q.tcpPacingRateMbps,
        alertCheck: () => false,
        formatter: (v) => v.toFixed(3),
    });
    addNumericMetric({
        code: 'tcp_send_rate',
        label: 'TCP send rate (Mbps)',
        rawValue: q.tcpSendRateMbps,
        alertCheck: () => false,
        formatter: (v) => v.toFixed(3),
    });

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
