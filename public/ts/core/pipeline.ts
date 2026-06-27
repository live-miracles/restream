import type {
    AudioTrack,
    ConfigData,
    HealthData,
    IngestUrls,
    Job,
    PipelineView,
    VideoTrack,
} from '../types.js';

const throughputState = {
    outputBytes: new Map<string, { ts: number; bytes: number }>(),
};

function computeKbps(
    stateMap: Map<string, { ts: number; bytes: number }>,
    key: string | null | undefined,
    totalBytes: number,
    nowMs: number,
): number | null {
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

function resolveIngestUrls(pipeline: { ingestUrls?: IngestUrls }): IngestUrls {
    return pipeline?.ingestUrls || { rtmp: null, srt: null };
}

function parsePipelinesInfo(
    config: Partial<ConfigData>,
    health: Partial<HealthData>,
): PipelineView[] {
    const newPipelines: PipelineView[] = [];
    const latestJobsByOutput = new Map<string, Job>();
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

    (config?.pipelines || []).forEach((p) => {
        const inputBytesReceived = healthByPipeline[p.id]?.input?.bytesReceived || 0;
        const inputPublisher = healthByPipeline[p.id]?.input?.publisher || null;
        const unexpectedReadersCount = Number(
            healthByPipeline[p.id]?.input?.unexpectedReaders?.count || 0,
        );
        const rawInputVideo = healthByPipeline[p.id]?.input?.video;
        const inputVideo: VideoTrack | null = rawInputVideo ? { ...rawInputVideo } : null;
        const rawInputAudio = healthByPipeline[p.id]?.input?.audio || null;
        const rawInputAudioTracks = healthByPipeline[p.id]?.input?.audioTracks || [];
        const mapAudioTrack = (track: any): AudioTrack => ({
            index: track.index !== undefined ? track.index : track.trackIndex,
            codec: track.codec,
            channels: track.channels,
            sample_rate: track.sampleRate !== undefined ? track.sampleRate : track.sample_rate,
            profile: track.profile,
        });
        const inputAudioTracks: AudioTrack[] =
            rawInputAudioTracks.length > 0
                ? rawInputAudioTracks.map(mapAudioTrack)
                : rawInputAudio
                  ? [mapAudioTrack(rawInputAudio)]
                  : [];
        const rawInputKbps = healthByPipeline[p.id]?.input?.bitrateKbps;
        const inputKbps = Number.isFinite(rawInputKbps as number)
            ? Number((rawInputKbps as number).toFixed(1))
            : null;

        if (inputVideo) inputVideo.bw = inputKbps;

        const inputStatus = healthByPipeline[p.id]?.input?.status || 'off';
        const publishStartedAt = healthByPipeline[p.id]?.input?.publishStartedAt || null;
        const publishStartedTs = publishStartedAt ? new Date(publishStartedAt).getTime() : NaN;

        let inputTime: number | null = null;
        if (inputStatus === 'on' && Number.isFinite(publishStartedTs) && publishStartedTs > 0) {
            inputTime = Math.max(0, nowMs - publishStartedTs);
        }

        newPipelines.push({
            id: p.id,
            name: p.name,
            key: p.streamKey,
            inputSource: p.inputSource || null,
            ingestUrls: resolveIngestUrls(p),
            input: {
                status: inputStatus,
                time: inputTime,
                video: inputVideo,
                audio: inputAudioTracks[0] || null,
                audioTracks: inputAudioTracks,
                bytesReceived: inputBytesReceived,
                bytesSent: healthByPipeline[p.id]?.input?.bytesSent || 0,
                readers: healthByPipeline[p.id]?.input?.readers || 0,
                bitrateKbps: inputKbps,
                publisher: inputPublisher ?? null,
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
            recording: healthByPipeline[p.id]?.recording ?? { enabled: false, active: false },
        });
    });

    (config?.outputs || []).forEach((out) => {
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
                inputSource: null,
                input: {
                    status: 'off',
                    time: null,
                    video: null,
                    audio: null,
                    audioTracks: [],
                    bitrateKbps: null,
                    bytesReceived: 0,
                    bytesSent: 0,
                    readers: 0,
                    publisher: null,
                    unexpectedReadersCount: 0,
                },
                ingestUrls: { rtmp: null, srt: null },
                outs: [],
                stats: {
                    inputBitrateKbps: null,
                    outputBitrateKbps: null,
                    readerCount: 0,
                    outputCount: 0,
                    readerMismatch: false,
                    unexpectedReadersCount: 0,
                },
                recording: { enabled: false, active: false },
            };
            newPipelines.push(pipe);
        }

        const outputTotalSize = outHealth?.totalSize ?? null;
        // Prefer the direct bitrate reading from ffmpeg progress (reliable for all protocols
        // including HLS where total_size may report N/A). Fall back to computing from byte delta.
        const outBitrateKbps =
            outHealth?.bitrateKbps ??
            computeKbps(
                throughputState.outputBytes,
                `${out.pipelineId}:${out.id}`,
                outputTotalSize ?? 0,
                nowMs,
            );

        let outTime: number | null = null;
        if ((status === 'on' || status === 'running') && latestJob?.startedAt) {
            outTime = Math.max(0, nowMs - new Date(latestJob.startedAt).getTime());
        }

        pipe.outs.push({
            id: out.id,
            pipe: pipe.name,
            name: out.name,
            desiredState: out.desiredState || 'stopped',
            encoding: out.encoding || 'source',
            url: out.url,
            status,
            time: outTime,
            job: latestJob || null,
            totalSize: outputTotalSize,
            bitrateKbps: outBitrateKbps,
        });
    });

    newPipelines.forEach((pipe) => {
        const outputCount = pipe.outs.length;
        const readerCount = pipe.input.readers || 0;

        const activeOutputKbps = pipe.outs
            .filter((o) => o.status === 'on' || o.status === 'running' || o.status === 'warning')
            .map((o) => o.bitrateKbps)
            .filter((k): k is number => k !== null && k >= 0);
        const outputBitrateKbps =
            activeOutputKbps.length > 0
                ? Number(activeOutputKbps.reduce((a, b) => a + b, 0).toFixed(1))
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
