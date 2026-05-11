const { getSessionBytesIn, getSessionBytesOut } = require('./health-media');

function indexPublishersByPath(rtmpConns, srtConns) {
    const publisherByPath = new Map();

    const setPublisher = (pathName, publisher) => {
        if (!pathName || publisherByPath.has(pathName)) return;
        publisherByPath.set(pathName, publisher);
    };

    for (const conn of rtmpConns.items || []) {
        if (conn?.state !== 'publish') continue;
        setPublisher(conn?.path, {
            id: conn?.id || null,
            protocol: 'rtmp',
            state: conn?.state || null,
            remoteAddr: conn?.remoteAddr || null,
            bytesReceived: getSessionBytesIn(conn),
            bytesSent: getSessionBytesOut(conn),
            quality: {},
        });
    }

    for (const conn of srtConns.items || []) {
        if (conn?.state !== 'publish') continue;
        setPublisher(conn?.path, {
            id: conn?.id || null,
            protocol: 'srt',
            state: conn?.state || null,
            remoteAddr: conn?.remoteAddr || null,
            bytesReceived: getSessionBytesIn(conn),
            bytesSent: getSessionBytesOut(conn),
            quality: {
                msRTT: conn?.msRTT || 0,
                packetsReceivedLoss: conn?.packetsReceivedLoss || 0,
                packetsReceivedRetrans: conn?.packetsReceivedRetrans || 0,
                packetsReceivedUndecrypt: conn?.packetsReceivedUndecrypt || 0,
                packetsReceivedDrop: conn?.packetsReceivedDrop || 0,
                mbpsReceiveRate: conn?.mbpsReceiveRate ?? null,
            },
        });
    }

    return publisherByPath;
}

// Managed reader types are those spawned by the app (FFmpeg outputs pulling RTMP/SRT)
// plus the one internal HLS muxer that MediaMTX adds per ready path.
const MANAGED_READER_TYPES = new Set(['rtmpconn', 'srtconn', 'hlsmuxer']);

function buildUnexpectedReaders({ pathInfo, generateProbeReaderTag, streamKey }) {
    const readers = pathInfo?.readers || [];
    const probeTag = streamKey ? generateProbeReaderTag(streamKey) : null;
    const unexpectedReaders = [];

    for (const reader of readers) {
        const readerType = String(reader?.type || 'unknown');
        const normalizedReaderType = readerType.toLowerCase();

        if (MANAGED_READER_TYPES.has(normalizedReaderType)) continue;

        // Ignore the probe ffprobe session — it shows up as a generic rtsp or webrtc reader
        // depending on MediaMTX version; the probe tag check handles that case.
        if (probeTag && String(reader?.query || '').includes(probeTag)) continue;

        unexpectedReaders.push({
            id: reader?.id || null,
            type: readerType,
            reason: 'non_managed_reader_type',
        });
    }

    return {
        count: unexpectedReaders.length,
        readers: unexpectedReaders,
    };
}

module.exports = {
    buildUnexpectedReaders,
    indexPublishersByPath,
};
