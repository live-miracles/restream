export interface Ingest {
    id: string;
    filename: string;
    streamKey: string;
    loop: boolean;
    startTime: string;
}

export interface IngestSecurityConfig {
    failureLimit: number;
    failureWindowMs: number;
    banMs: number;
    trackedIpLimit: number;
}

export interface Pipeline {
    id: string;
    name: string;
    streamKey: string;
    encoding: string | null;
}

export interface Output {
    id: string;
    pipelineId: string;
    name: string;
    url: string;
    desiredState: 'running' | 'stopped';
    encoding: string;
}

export interface Job {
    id: string;
    pipelineId: string;
    outputId: string;
    pid: number | null;
    status: 'running' | 'stopped' | 'failed';
    startedAt: string;
    endedAt: string | null;
    exitCode: number | null;
    exitSignal: string | null;
}

export interface JobLog {
    ts: string;
    message: string;
    eventType: string;
    eventData: unknown;
}

export interface HttpError extends Error {
    status: number;
    publicError: string;
    detail?: string;
    [key: string]: unknown;
}

export interface HistoryFilters {
    since?: string | null;
    until?: string | null;
    limit?: number | null;
    order?: 'asc' | 'desc';
    prefixes?: string[];
}

export interface Db {
    createPipeline(params: {
        id?: string;
        name: string;
        streamKey: string;
        encoding?: string | null;
    }): Pipeline;
    getPipeline(id: string): Pipeline | undefined;
    listPipelines(): Pipeline[];
    updatePipeline(
        id: string,
        params: { name: string; streamKey: string; encoding?: string | null },
    ): Pipeline | null;
    deletePipeline(id: string): boolean;

    createOutput(params: {
        id?: string;
        pipelineId: string;
        name: string;
        url: string;
        desiredState?: string;
        encoding?: string;
    }): Output;
    getOutput(pipelineId: string, id: string): Output | undefined;
    listOutputs(): Output[];
    listOutputsForPipeline(pipelineId: string): Output[];
    updateOutput(
        pipelineId: string,
        id: string,
        params: { name: string; url: string; encoding?: string },
    ): Output | null;
    setOutputDesiredState(pipelineId: string, id: string, desiredState: string): Output;
    deleteOutput(pipelineId: string, id: string): boolean;

    createJob(params: {
        id?: string;
        pipelineId: string;
        outputId: string;
        pid?: number | null;
        status?: string;
        startedAt?: string;
    }): Job;
    getJob(id: string): Job | undefined;
    getRunningJobFor(pipelineId: string, outputId: string): Job | undefined;
    updateJob(
        id: string,
        params: {
            pid?: number | null;
            status?: string | null;
            endedAt?: string | null;
            exitCode?: number | null;
            exitSignal?: string | null;
        },
    ): Job | undefined;
    listJobsForOutput(pipelineId: string, outputId: string): Job[];
    listJobs(): Job[];

    appendJobLog(
        jobId: string | null,
        message: string,
        pipelineId?: string | null,
        outputId?: string | null,
        eventType?: string,
        eventData?: unknown,
    ): void;
    appendPipelineEvent(
        pipelineId: string,
        message: string,
        eventType?: string,
        eventData?: unknown,
    ): void;
    listJobLogs(jobId: string): JobLog[];
    listJobLogsByOutput(pipelineId: string, outputId: string): JobLog[];
    listJobLogsByOutputFiltered(
        pipelineId: string,
        outputId: string,
        filters?: HistoryFilters,
    ): JobLog[];
    listLifecycleLogsByOutput(pipelineId: string, outputId: string): JobLog[];
    listJobLogsByPipeline(pipelineId: string): JobLog[];
    deleteJobLogsOlderThan(days?: number): void;
    cleanupOldJobs(): { deletedJobs: number; deletedLogs: number };

    createIngest(params: {
        id?: string;
        filename: string;
        streamKey: string;
        loop: boolean;
        startTime: string;
    }): Ingest;
    getIngest(id: string): Ingest | undefined;
    listIngests(): Ingest[];
    listIngestsForFilename(filename: string): Ingest[];
    updateIngest(
        id: string,
        params: { filename: string; streamKey: string; loop: boolean; startTime: string },
    ): Ingest | undefined;
    deleteIngest(id: string): boolean;

    getMeta(key: string): string | null;
    setMeta(key: string, value: string): string;
    getCustomEncoding(): string | null;
    setCustomEncoding(ffmpegArgs: string): string;
    getServerName(): string;
    setServerName(name: string): string;
    getIngestSecurityConfig(): Partial<IngestSecurityConfig>;
    setIngestSecurityConfig(config: IngestSecurityConfig): string;
}
