// Publisher connection quality helpers.
// Derives display labels, numeric metrics (bitrate, FPS, dropped frames), and alert
// thresholds from the raw MediaMTX publisher record for the active input stream.
/**
 * Maps a raw MediaMTX publisher protocol key to a short uppercase display label.
 * @param {string} protocol - e.g. `'rtsp'`, `'rtmp'`, `'srt'`.
 * @returns {string}
 */
function normalizePublisherProtocolLabel(protocol) {
    const map = { rtsp: 'RTSP', rtmp: 'RTMP', srt: 'SRT' };
    return map[protocol] || String(protocol || '').toUpperCase();
}

/**
 * Derives a flat array of numeric quality metrics from a MediaMTX publisher object.
 * Each element describes one signal (packet loss, jitter, RTT, etc.) and whether
 * its current value exceeds an alert threshold.
 * @param {object|null} publisher - MediaMTX publisher record from the health API.
 * @returns {Array<{code: string, label: string, value: number, displayValue: string, isAlert: boolean}>}
 */
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

    return metrics;
}

/**
 * Returns only the metrics that are currently above their alert threshold,
 * formatted as `{code, label}` pairs suitable for banner display.
 * @param {object|null} publisher - MediaMTX publisher record.
 * @returns {Array<{code: string, label: string}>}
 */
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