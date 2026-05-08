// Pipeline data helpers.
// Merges config and health API responses into the unified pipeline model used by dashboard
// rendering, computes per-output bitrate from progress samples, resolves ingest URLs for
// display, and builds the latest-jobs index the dashboard renders per output.

/**
 * Rewrites the `localhost` hostname in an ingest URL to `currentHost` so that
 * URLs generated on the server remain clickable from a remote browser.
 * Returns `url` unchanged when it is already a non-localhost address.
 * @param {string|null} url
 * @param {string|null} currentHost - `window.location.hostname` of the client.
 * @returns {string|null}
 */
function rewriteIngestUrlHost(url, currentHost) {
    if (!url) return null;

    try {
        const parsed = new URL(url);
        if (parsed.hostname !== 'localhost' || !currentHost || currentHost === 'localhost') {
            return url;
        }

        parsed.hostname = currentHost;
        return parsed.toString();
    } catch {
        return url;
    }
}

/**
 * Returns the `{rtmp, rtsp, srt}` ingest URL map for a pipeline, rewriting any
 * `localhost` addresses to `currentHost` when the server's `ingestHost` is also
 * localhost (i.e. the server does not know its own public address).
 * @param {object} pipeline - Pipeline config record.
 * @param {object} config - Full config API response (reads `config.ingestHost`).
 * @param {string|null} [currentHost]
 * @returns {{rtmp: string|null, rtsp: string|null, srt: string|null}}
 */
function resolveIngestUrls(pipeline, config, currentHost = null) {
    const ingestUrls = pipeline?.ingestUrls;
    if (!ingestUrls) {
        return { rtmp: null, rtsp: null, srt: null };
    }

    const ingestHost = config?.ingestHost;
    if (ingestHost && ingestHost !== 'localhost') {
        return ingestUrls;
    }

    return {
        rtmp: rewriteIngestUrlHost(ingestUrls.rtmp, currentHost),
        rtsp: rewriteIngestUrlHost(ingestUrls.rtsp, currentHost),
        srt: rewriteIngestUrlHost(ingestUrls.srt, currentHost),
    };
}

/**
 * Computes the current bitrate in kbps for a byte-counter metric by diffing
 * the latest sample against the previous one stored in `stateMap`.
 * Returns `null` on the first call for a given key (no baseline yet) and when
 * the time delta is non-positive.
 * @param {Map<string, {ts: number, bytes: number}>} stateMap - Mutable per-pipeline accumulator.
 * @param {string|null} key - Unique metric identifier (e.g. pipeline ID).
 * @param {number} totalBytes - Latest cumulative byte count from the API.
 * @param {number} nowMs - Current timestamp in milliseconds.
 * @returns {number|null}
 */
function computeKbps(stateMap, key, totalBytes, nowMs) {
    // Bitrate is inferred from cumulative byte counters, so the first sample only seeds the
    // baseline and later samples convert byte deltas into kbps.
    if (!key) return null;

    const safeBytes = Number(totalBytes || 0);
    const previous = stateMap.get(key);
    stateMap.set(key, { ts: nowMs, bytes: safeBytes });

    if (!previous) return null;

    const dtMs = nowMs - previous.ts;
    if (dtMs <= 0) return null;

    const deltaBytes = Math.max(0, safeBytes - previous.bytes);
    return Number(((deltaBytes * 8) / (dtMs / 1000) / 1000).toFixed(1));
}

/**
 * Builds an index of the most-recent job record per `"pipelineId:outputId"` key.
 * Later jobs (by `startedAt` or `endedAt`) replace earlier ones.
 * @param {object[]} [jobs] - Job array from the config API response.
 * @returns {Map<string, object>}
 */
function buildLatestJobsByOutput(jobs = []) {
    const latestJobsByOutput = new Map();

    jobs.forEach((job) => {
        const key = `${job.pipelineId}:${job.outputId}`;
        const previous = latestJobsByOutput.get(key);
        if (!previous) {
            latestJobsByOutput.set(key, job);
            return;
        }

        const previousTime = new Date(previous.startedAt || previous.endedAt || 0).getTime();
        const currentTime = new Date(job.startedAt || job.endedAt || 0).getTime();
        if (currentTime >= previousTime) {
            latestJobsByOutput.set(key, job);
        }
    });

    return latestJobsByOutput;
}

function createMissingPipeline(output) {
    return {
        id: output.pipelineId,
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
}

/**
 * Merges pipeline config, live health data, and job history into the unified
 * pipeline model consumed by dashboard renderers. Handles orphaned outputs
 * (outputs whose pipeline has been deleted) by synthesising a placeholder pipeline.
 * @param {{config: object, health: object, nowMs: number, currentHost?: string|null, computeInputKbps: Function}} opts
 * @returns {object[]} Array of merged pipeline objects.
 */
function mergePipelineInfo({ config, health, nowMs, currentHost = null, computeInputKbps }) {
    // Merge persisted config with live health slices and latest jobs so renderers can stay focused
    // on presentation instead of recomputing cross-API joins every frame.
    const pipelines = [];
    const latestJobsByOutput = buildLatestJobsByOutput(config?.jobs || []);
    const healthByPipeline = health?.pipelines || {};

    (config?.pipelines || []).forEach((pipeline) => {
        const inputHealth = healthByPipeline[pipeline.id]?.input || {};
        const inputBytesReceived = inputHealth.bytesReceived || 0;
        const inputVideo = inputHealth.video ? { ...inputHealth.video } : null;
        const inputKbps = computeInputKbps(pipeline.id, inputBytesReceived, nowMs);

        if (inputVideo) inputVideo.bw = inputKbps;

        const publishStartedAt = inputHealth.publishStartedAt || null;
        const publishStartedTs = publishStartedAt ? new Date(publishStartedAt).getTime() : NaN;
        const inputStatus = inputHealth.status || 'off';
        const inputTime =
            inputStatus === 'on' && Number.isFinite(publishStartedTs) && publishStartedTs > 0
                ? Math.max(0, nowMs - publishStartedTs)
                : null;
        const unexpectedReadersCount = Number(inputHealth.unexpectedReaders?.count || 0);

        pipelines.push({
            id: pipeline.id,
            name: pipeline.name,
            key: pipeline.streamKey,
            ingestUrls: resolveIngestUrls(pipeline, config, currentHost),
            input: {
                status: inputStatus,
                time: inputTime,
                video: inputVideo,
                audio: inputHealth.audio || null,
                bytesReceived: inputBytesReceived,
                bytesSent: inputHealth.bytesSent || 0,
                readers: inputHealth.readers || 0,
                bitrateKbps: inputKbps,
                publisher: inputHealth.publisher || null,
                unexpectedReadersCount,
            },
            outs: [],
            stats: {
                inputBitrateKbps: inputKbps,
                outputBitrateKbps: null,
                readerCount: inputHealth.readers || 0,
                outputCount: 0,
                readerMismatch: false,
                unexpectedReadersCount,
            },
        });
    });

    (config?.outputs || []).forEach((output) => {
        let pipeline = pipelines.find((candidate) => candidate.id === output.pipelineId);
        if (!pipeline) {
            console.error('Not found pipeline for output: ', output);
            pipeline = createMissingPipeline(output);
            pipelines.push(pipeline);
        }

        const latestJob = latestJobsByOutput.get(`${output.pipelineId}:${output.id}`) || null;
        const outputHealth = healthByPipeline[output.pipelineId]?.outputs?.[output.id] || null;
        const status = outputHealth?.status || 'off';
        const startedAtMs = latestJob?.startedAt ? new Date(latestJob.startedAt).getTime() : NaN;
        const outputTime =
            status === 'on' && Number.isFinite(startedAtMs) ? Math.max(0, nowMs - startedAtMs) : null;

        pipeline.outs.push({
            id: output.id,
            pipe: pipeline.name,
            name: output.name,
            desiredState: output.desiredState || 'stopped',
            encoding: output.encoding || 'source',
            url: output.url,
            status,
            time: outputTime,
            video: outputHealth?.media?.video ?? null,
            audio: outputHealth?.media?.audio ?? null,
            mediaSource: outputHealth?.mediaSource || 'unknown',
            job: latestJob,
            totalSize: outputHealth?.totalSize || null,
            bitrateKbps: outputHealth?.bitrateKbps ?? null,
            progressFrame: outputHealth?.progressFrame ?? null,
            progressFps: outputHealth?.progressFps ?? null,
            process: outputHealth?.process || null,
            processCpuPercent: outputHealth?.process?.cpuPercent ?? null,
            processMemoryBytes: outputHealth?.process?.memoryBytes ?? null,
        });
    });

    pipelines.forEach((pipeline) => {
        const readerCount = pipeline.input.readers || 0;
        const outputCount = pipeline.outs.length;
        const outputBitrates = pipeline.outs
            .map((output) => output.bitrateKbps)
            .filter((value) => Number.isFinite(value));
        const outputProcessCpuValues = pipeline.outs
            .map((output) => output.processCpuPercent)
            .filter((value) => Number.isFinite(value));
        const outputProcessMemoryValues = pipeline.outs
            .map((output) => output.processMemoryBytes)
            .filter((value) => Number.isFinite(value) && value >= 0);

        pipeline.stats = {
            inputBitrateKbps: pipeline.input.bitrateKbps,
            outputBitrateKbps:
                outputBitrates.length > 0
                    ? Number(outputBitrates.reduce((sum, value) => sum + value, 0).toFixed(1))
                    : null,
            processCpuPercent:
                outputProcessCpuValues.length > 0
                    ? Number(
                          outputProcessCpuValues
                              .reduce((sum, value) => sum + value, 0)
                              .toFixed(2),
                      )
                    : null,
            processMemoryBytes:
                outputProcessMemoryValues.length > 0
                    ? outputProcessMemoryValues.reduce((sum, value) => sum + value, 0)
                    : null,
            readerCount,
            outputCount,
            readerMismatch: readerCount !== outputCount,
            unexpectedReadersCount: Number(pipeline.input.unexpectedReadersCount || 0),
        };
    });

    return pipelines;
}

// Small facade that turns /config + /health into the dashboard view model. The only state kept
// here is throughput history, because kbps calculations need the previous byte sample.
const throughputState = {
    inputBytes: new Map(),
};

/**
 * Thin wrapper that calls `mergePipelineInfo` using `nowMs = Date.now()` and a
 * module-scoped kbps accumulator map. Suitable for one-shot polling calls that
 * do not need to share accumulator state across invocations.
 * @param {object} config - Config API response.
 * @param {object} health - Health API response.
 * @returns {object[]}
 */
function parsePipelinesInfo(config, health) {
    return mergePipelineInfo({
        config,
        health,
        nowMs: Date.now(),
        currentHost:
            typeof window !== 'undefined' && window.location?.hostname
                ? window.location.hostname
                : null,
        computeInputKbps: (key, totalBytes, nowMs) =>
            computeKbps(throughputState.inputBytes, key, totalBytes, nowMs),
    });
}

export { buildLatestJobsByOutput, mergePipelineInfo, parsePipelinesInfo, computeKbps, resolveIngestUrls };
