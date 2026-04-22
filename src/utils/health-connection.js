const { getSessionBytesIn, getSessionBytesOut } = require('./health-media');

function indexRtspConnectionsByReaderTag(rtspConns, rtspSessions, getReaderIdFromQuery) {
    const rtspSessionById = new Map(
        (rtspSessions.items || []).map((session) => [session.id, session]),
    );
    const rtspConnectionRecords = (rtspConns.items || []).map((conn) => {
        const session = conn?.session ? rtspSessionById.get(conn.session) : null;

        return {
            id: conn?.id || null,
            sessionId: conn?.session || session?.id || null,
            path: conn?.path || session?.path || null,
            query: conn?.query || session?.query || null,
            remoteAddr: conn?.remoteAddr || session?.remoteAddr || null,
            userAgent: conn?.userAgent || conn?.useragent || null,
            bytesReceived: conn?.bytesReceived || session?.bytesReceived || 0,
            bytesSent: conn?.bytesSent || session?.bytesSent || 0,
        };
    });

    const rtspByReaderTag = new Map();
    for (const conn of rtspConnectionRecords) {
        const readerTag = getReaderIdFromQuery(conn.query);
        if (!readerTag) continue;

        const existing = rtspByReaderTag.get(readerTag);
        if (existing) {
            existing.push(conn);
            continue;
        }
        rtspByReaderTag.set(readerTag, [conn]);
    }

    const rtspConnectionById = new Map(rtspConnectionRecords.map((conn) => [conn.id, conn]));
    const rtspSessionRecordById = new Map(
        (rtspSessions.items || []).map((session) => [
            session.id,
            {
                id: session?.id || null,
                sessionId: session?.id || null,
                path: session?.path || null,
                query: session?.query || null,
                remoteAddr: session?.remoteAddr || null,
                userAgent: session?.userAgent || session?.useragent || null,
                bytesReceived: session?.bytesReceived || 0,
                bytesSent: session?.bytesSent || 0,
            },
        ]),
    );

    return { rtspByReaderTag, rtspConnectionById, rtspSessionRecordById };
}

function indexPublishersByPath(rtspSessions, rtmpConns, srtConns) {
    const publisherByPath = new Map();

    const setPublisher = (pathName, publisher) => {
        if (!pathName || publisherByPath.has(pathName)) return;
        publisherByPath.set(pathName, publisher);
    };

    for (const session of rtspSessions.items || []) {
        if (session?.state !== 'publish') continue;
        setPublisher(session?.path, {
            id: session?.id || null,
            protocol: 'rtsp',
            state: session?.state || null,
            remoteAddr: session?.remoteAddr || null,
            bytesReceived: getSessionBytesIn(session),
            bytesSent: getSessionBytesOut(session),
            quality: {
                inboundRTPPacketsLost: session?.inboundRTPPacketsLost || 0,
                inboundRTPPacketsInError: session?.inboundRTPPacketsInError || 0,
                inboundRTPPacketsJitter: session?.inboundRTPPacketsJitter || 0,
            },
        });
    }

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

function buildUnexpectedReaders({
    pathInfo,
    pipelineOutputs,
    rtspConnectionById,
    streamKey,
    rtspSessionRecordById,
    getExpectedReaderTag,
    generateProbeReaderTag,
    getReaderIdFromQuery,
}) {
    const readers = pathInfo?.readers || [];
    const expectedReaderTags = new Set(
        (pipelineOutputs || []).map((output) => getExpectedReaderTag(output.pipelineId, output.id)),
    );
    if (streamKey) {
        expectedReaderTags.add(generateProbeReaderTag(streamKey));
    }
    const unexpectedReaders = [];
    let ignoredInternalHlsMuxer = false;

    for (const reader of readers) {
        const readerType = String(reader?.type || 'unknown');
        const readerId = reader?.id || null;
        const normalizedReaderType = readerType.toLowerCase();

        if (normalizedReaderType === 'hlsmuxer' && !ignoredInternalHlsMuxer) {
            // MediaMTX exposes one internal HLS muxer reader per ready path when HLS is enabled.
            // Ignore this single internal reader to avoid noisy dashboard warnings.
            ignoredInternalHlsMuxer = true;
            continue;
        }

        if (readerType !== 'rtspSession' && readerType !== 'rtspConn') {
            unexpectedReaders.push({
                id: readerId,
                type: readerType,
                reason: 'non_managed_reader_type',
            });
            continue;
        }

        const rtspConn = readerId
            ? readerType === 'rtspSession'
                ? rtspSessionRecordById?.get(readerId) || rtspConnectionById.get(readerId) || null
                : rtspConnectionById.get(readerId) || null
            : null;
        const readerTag = getReaderIdFromQuery(rtspConn?.query || null);
        const userAgent = String(rtspConn?.userAgent || '').toLowerCase();

        if (readerTag && expectedReaderTags.has(readerTag)) {
            continue;
        }

        if (!readerTag && userAgent.includes('ffprobe')) {
            continue;
        }

        unexpectedReaders.push({
            id: readerId,
            type: readerType,
            query: rtspConn?.query || null,
            remoteAddr: rtspConn?.remoteAddr || null,
            userAgent: rtspConn?.userAgent || null,
            reason: readerTag ? 'unknown_reader_tag' : 'missing_reader_tag',
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
    indexRtspConnectionsByReaderTag,
};
