// javascript
const crypto = require('crypto');

class StreamKey {
    constructor({ key, label = null, createdAt } = {}) {
        this.key = key || crypto.randomBytes(12).toString('hex');
        this.label = label ?? null;
        this.createdAt = createdAt || new Date().toISOString();
    }

    static validate(obj = {}) {
        if (obj.key && typeof obj.key !== 'string')
            return { ok: false, error: 'key must be a string' };
        return { ok: true };
    }

    static from(obj = {}) {
        return new StreamKey(obj);
    }

    toJSON() {
        return { key: this.key, label: this.label, createdAt: this.createdAt };
    }
}

module.exports = StreamKey;

// javascript
class Pipeline {
    constructor({ id, name, description = null, createdAt, updatedAt } = {}) {
        if (!name || typeof name !== 'string') throw new Error('Pipeline.name is required');
        this.id = id || Date.now().toString();
        this.name = name;
        this.description = description ?? null;
        this.createdAt = createdAt || new Date().toISOString();
        this.updatedAt = updatedAt || null;
    }

    static validate(obj = {}) {
        if (!obj.name || typeof obj.name !== 'string')
            return { ok: false, error: 'name is required' };
        return { ok: true };
    }

    static from(obj = {}) {
        return new Pipeline(obj);
    }

    touch() {
        this.updatedAt = new Date().toISOString();
    }

    toJSON() {
        return {
            id: this.id,
            name: this.name,
            description: this.description,
            createdAt: this.createdAt,
            updatedAt: this.updatedAt,
        };
    }
}

module.exports = Pipeline;

// javascript
class Output {
    constructor({
        id,
        pipelineId,
        type,
        url,
        label = null,
        inputPath = null,
        status = 'stopped',
        jobId = null,
        createdAt,
        updatedAt,
        lastExit = null,
    } = {}) {
        if (!pipelineId) throw new Error('Output.pipelineId is required');
        if (!type) throw new Error('Output.type is required');
        if (!url) throw new Error('Output.url is required');

        this.id = id || Date.now().toString();
        this.pipelineId = pipelineId;
        this.type = type;
        this.url = url;
        this.label = label ?? null;
        this.inputPath = inputPath ?? null;
        this.status = status;
        this.jobId = jobId;
        this.createdAt = createdAt || new Date().toISOString();
        this.updatedAt = updatedAt || null;
        this.lastExit = lastExit;
    }

    static validate(obj = {}) {
        if (!obj.pipelineId) return { ok: false, error: 'pipelineId is required' };
        if (!obj.type) return { ok: false, error: 'type is required' };
        if (!obj.url) return { ok: false, error: 'url is required' };
        return { ok: true };
    }

    static from(obj = {}) {
        return new Output(obj);
    }

    markRunning(jobId) {
        this.status = 'running';
        this.jobId = jobId;
        this.touch();
    }

    markStopped(exitInfo = null) {
        this.status = 'stopped';
        this.jobId = null;
        this.lastExit = exitInfo;
        this.touch();
    }

    touch() {
        this.updatedAt = new Date().toISOString();
    }

    toJSON() {
        return {
            id: this.id,
            pipelineId: this.pipelineId,
            type: this.type,
            url: this.url,
            label: this.label,
            inputPath: this.inputPath,
            status: this.status,
            jobId: this.jobId,
            createdAt: this.createdAt,
            updatedAt: this.updatedAt,
            lastExit: this.lastExit,
        };
    }
}

module.exports = Output;

// javascript
class Job {
    constructor({
        id,
        outputId,
        pid = null,
        status = 'pending',
        startedAt = null,
        stoppedAt = null,
        exitCode = null,
    } = {}) {
        this.id = id || Date.now().toString();
        this.outputId = outputId || null;
        this.pid = pid;
        this.status = status;
        this.startedAt = startedAt;
        this.stoppedAt = stoppedAt;
        this.exitCode = exitCode;
    }

    static from(obj = {}) {
        return new Job(obj);
    }

    markStarted(pid) {
        this.pid = pid;
        this.status = 'running';
        this.startedAt = new Date().toISOString();
    }

    markStopped(code = null) {
        this.exitCode = code;
        this.status = 'stopped';
        this.stoppedAt = new Date().toISOString();
    }

    toJSON() {
        return {
            id: this.id,
            outputId: this.outputId,
            pid: this.pid,
            status: this.status,
            startedAt: this.startedAt,
            stoppedAt: this.stoppedAt,
            exitCode: this.exitCode,
        };
    }
}

module.exports = Job;
