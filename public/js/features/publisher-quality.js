function normalizePublisherProtocolLabel(protocol) {
    const map = { rtsp: 'RTSP', rtmp: 'RTMP', srt: 'SRT', webrtc: 'WebRTC' };
    return map[protocol] || String(protocol || '').toUpperCase();
}

function getPublisherQualityMetrics(publisher) {
    if (!publisher) return [];

    const q = publisher.quality || {};
    const metrics = [];

    const addNumericMetric = ({ code, label, rawValue, alertCheck, formatter }) => {
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

    if (publisher.protocol === 'webrtc' && q.peerConnectionEstablished !== undefined) {
        metrics.push({
            code: 'webrtc_peer',
            label: 'Peer connection established',
            value: q.peerConnectionEstablished ? 1 : 0,
            displayValue: q.peerConnectionEstablished ? 'Yes' : 'No',
            isAlert: false,
        });
    }

    return metrics;
}

function getPublisherQualityAlerts(publisher) {
    return getPublisherQualityMetrics(publisher)
        .filter((metric) => metric.isAlert)
        .map((metric) => ({
            code: metric.code,
            label: `${metric.label}: ${metric.displayValue}`,
        }));
}

export {
    normalizePublisherProtocolLabel,
    getPublisherQualityMetrics,
    getPublisherQualityAlerts,
};