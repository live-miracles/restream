const throughputState = {
    inputBytes: new Map(),
};

function computeKbps(stateMap, key, totalBytes, nowMs) {
    // Bitrate is inferred from monotonically increasing byte counters, so the first sample only
    // establishes a baseline and later samples compute delta-bytes over delta-time.
    if (!key) return null;
    const safeBytes = Number(totalBytes || 0);
    const prev = stateMap.get(key);
    stateMap.set(key, { ts: nowMs, bytes: safeBytes });

    if (!prev) return null;
    const dtMs = nowMs - prev.ts;
    if (dtMs <= 0) return null;

    const deltaBytes = Math.max(0, safeBytes - prev.bytes);
    return Number(((deltaBytes * 8) / (dtMs / 1000) / 1000).toFixed(1));
}

function resolveIngestUrls(pipeline, config) {
    const ingestUrls = pipeline?.ingestUrls;
    if (!ingestUrls) {
        return { rtmp: null, rtsp: null, srt: null };
    }

    const ingestHost = config?.ingestHost;
    if (ingestHost && ingestHost !== 'localhost') {
        return ingestUrls;
    }

    const currentHost =
        typeof window !== 'undefined' && window.location?.hostname
            ? window.location.hostname
            : null;
    if (!currentHost || currentHost === 'localhost') {
        return ingestUrls;
    }

    const rewriteHost = (url) => {
        if (!url) return null;
        try {
            const parsed = new URL(url);
            if (parsed.hostname !== 'localhost') return url;
            parsed.hostname = currentHost;
            return parsed.toString();
        } catch (_) {
            return url;
        }
    };

    return {
        rtmp: rewriteHost(ingestUrls.rtmp),
        rtsp: rewriteHost(ingestUrls.rtsp),
        srt: rewriteHost(ingestUrls.srt),
    };
}

function parsePipelinesInfo(config, health) {
    // The dashboard consumes one merged model that combines persisted config, current health, and
    // latest job state; this keeps renderers simple even though the source data lives in 3 APIs.
    const newPipelines = [];
    const latestJobsByOutput = new Map();
    const healthByPipeline = health?.pipelines || {};
    const nowMs = Date.now();

    (config?.jobs || []).forEach((job) => {
        const key = `${job.pipelineId}:${job.outputId}`;
        const previous = latestJobsByOutput.get(key);
        if (!previous) {
            latestJobsByOutput.set(key, job);
            return;
        }

        const previousTime = new Date(previous.startedAt || previous.endedAt || 0).getTime();
        const currentTime = new Date(job.startedAt || job.endedAt || 0).getTime();
        if (currentTime >= previousTime) latestJobsByOutput.set(key, job);
    });

    config?.pipelines.forEach((p) => {
        const inputBytesReceived = healthByPipeline[p.id]?.input?.bytesReceived || 0;
        const inputPublisher = healthByPipeline[p.id]?.input?.publisher || null;
        const unexpectedReadersCount = Number(
            healthByPipeline[p.id]?.input?.unexpectedReaders?.count || 0,
        );
        const inputVideo = healthByPipeline[p.id]?.input?.video
            ? { ...healthByPipeline[p.id].input.video }
            : null;
        const inputKbps = computeKbps(throughputState.inputBytes, p.id, inputBytesReceived, nowMs);

        if (inputVideo) inputVideo.bw = inputKbps;

        const inputStatus = healthByPipeline[p.id]?.input?.status || 'off';
        const publishStartedAt = healthByPipeline[p.id]?.input?.publishStartedAt || null;
        const publishStartedTs = publishStartedAt ? new Date(publishStartedAt).getTime() : NaN;

        let inputTime = null;
        if (inputStatus === 'on' && Number.isFinite(publishStartedTs) && publishStartedTs > 0) {
            inputTime = Math.max(0, nowMs - publishStartedTs);
        }

        newPipelines.push({
            id: p.id,
            name: p.name,
            key: p.streamKey,
            ingestUrls: resolveIngestUrls(p, config),
            input: {
                status: inputStatus,
                time: inputTime,
                video: inputVideo,
                audio: healthByPipeline[p.id]?.input?.audio || null,
                bytesReceived: inputBytesReceived,
                bytesSent: healthByPipeline[p.id]?.input?.bytesSent || 0,
                readers: healthByPipeline[p.id]?.input?.readers || 0,
                bitrateKbps: inputKbps,
                publisher: inputPublisher,
                unexpectedReadersCount,
            },
            outs: [],
            stats: {
                inputBitrateKbps: inputKbps,
                outputBitrateKbps: null,
                readerCount: healthByPipeline[p.id]?.input?.readers || 0,
                outputCount: 0,
                readerMismatch: false,
                unexpectedReadersCount,
            },
        });
    });

    config?.outputs.forEach((out) => {
        let pipe = newPipelines.find((p) => p.id === out.pipelineId);
        const latestJob = latestJobsByOutput.get(`${out.pipelineId}:${out.id}`);
        const outHealth = healthByPipeline[out.pipelineId]?.outputs?.[out.id] || null;
        const status = outHealth?.status || 'off';

        if (!pipe) {
            console.error('Not found pipeline for output: ', out);
            pipe = {
                id: out.pipelineId,
                name: 'Undefined',
                key: null,
                input: {
                    status: 'off',
                    time: null,
                    video: null,
                    audio: null,
                    bitrateKbps: null,
                    readers: 0,
                    publisher: null,
                    unexpectedReadersCount: 0,
                },
                ingestUrls: { rtmp: null, rtsp: null, srt: null },
                outs: [],
                stats: {
                    inputBitrateKbps: null,
                    outputBitrateKbps: null,
                    readerCount: 0,
                    outputCount: 0,
                    readerMismatch: false,
                    unexpectedReadersCount: 0,
                },
            };
            newPipelines.push(pipe);
        }

        const outputTotalSize = outHealth?.totalSize || null;
        const outputBitrateKbps = outHealth?.bitrateKbps ?? null;

        const encoding = out.encoding || 'source';
        const outVideo = outHealth?.media?.video ?? null;
        const outAudio = outHealth?.media?.audio ?? null;
        const mediaSource = outHealth?.mediaSource || 'unknown';

        let outTime = null;
        if (status === 'on' && latestJob?.startedAt) {
            outTime = Math.max(0, nowMs - new Date(latestJob.startedAt).getTime());
        }

        pipe.outs.push({
            id: out.id,
            pipe: pipe.name,
            name: out.name,
            desiredState: out.desiredState || 'stopped',
            encoding,
            url: out.url,
            status,
            time: outTime,
            video: outVideo,
            audio: outAudio,
            mediaSource,
            job: latestJob || null,
            totalSize: outputTotalSize,
            bitrateKbps: outputBitrateKbps,
        });
    });

    newPipelines.forEach((pipe) => {
        const outputCount = pipe.outs.length;
        const readerCount = pipe.input.readers || 0;
        const activeOutputBitratesKbps = pipe.outs
            .map((out) => out.bitrateKbps)
            .filter((value) => Number.isFinite(value));
        const outputBitrateKbps =
            activeOutputBitratesKbps.length > 0
                ? Number(activeOutputBitratesKbps.reduce((sum, value) => sum + value, 0).toFixed(1))
                : null;

        pipe.stats = {
            inputBitrateKbps: pipe.input.bitrateKbps,
            outputBitrateKbps,
            readerCount,
            outputCount,
            readerMismatch: readerCount !== outputCount,
            unexpectedReadersCount: Number(pipe.input.unexpectedReadersCount || 0),
        };
    });

    return newPipelines;
}

export { parsePipelinesInfo };
