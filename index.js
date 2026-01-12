const express = require('express');
const { spawn } = require('child_process');
const path = require('path');
const crypto = require('crypto');

const app = express();
app.use(express.json());

/* ======================
 * In-memory storage
 * ====================== */
let jobs = {};
let streamKeys = {};

/* ======================
 * Models
 * ====================== */

// 1. StreamKey model
class StreamKey {
    constructor({ key, label = null, createdAt } = {}) {
        this.key = key || crypto.randomBytes(12).toString('hex');
        this.label = label ?? null;
        this.createdAt = createdAt || new Date().toISOString();
    }
}

// 2. Pipeline model
class Pipeline {
    constructor({ id, name, streamKey = null, createdAt, updatedAt } = {}) {
        if (!name || typeof name !== 'string') {
            throw new Error('Pipeline.name is required');
        }
        this.id = id || Date.now().toString();
        this.name = name;
        this.streamKey = streamKey;
        this.createdAt = createdAt || new Date().toISOString();
        this.updatedAt = updatedAt || null;
    }
}

// 3. Output model
class Output {
    constructor({ id, type, url } = {}) {
        if (!type || typeof type !== 'string') {
            throw new Error('Output.type is required');
        }
        if (!url || typeof url !== 'string') {
            throw new Error('Output.url is required');
        }
        this.id = id || Date.now().toString();
        this.type = type;
        this.url = url;
    }
}

/* ======================
 * Stream Key APIs
 * ====================== */

// create stream key
app.post('/stream-keys', async (req, res) => {
    try {
        const streamKeyObj = new StreamKey({
            key: req.body?.streamKey,
            label: req.body?.label,
        });

        if (streamKeys[streamKeyObj.key]) {
            return res.status(409).json({ error: 'Stream key already exists' });
        }

        const url = `http://localhost:9997/v3/config/paths/add/${encodeURIComponent(streamKeyObj.key)}`;
        const resp = await fetch(url, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ name: streamKeyObj.key }),
        });

        const data = await resp.json().catch(() => null);
        if (!resp.ok || data?.error) {
            return res.status(500).json({
                error: data?.error || `MediaMTX returned ${resp.status}`,
            });
        }

        streamKeys[streamKeyObj.key] = streamKeyObj;
        return res.status(201).json({
            message: 'Stream key created',
            streamKey: streamKeyObj,
        });
    } catch (err) {
        return res.status(500).json({ error: err.toString() });
    }
});

// update stream key label
app.post('/stream-keys/:key', (req, res) => {
    try {
        const { key } = req.params;
        const { label } = req.body || {};

        const sk = streamKeys[key];
        if (!sk) {
            return res.status(404).json({ error: 'Stream key not found' });
        }

        sk.label = label ?? null;
        return res.json({ message: 'Stream key updated', streamKey: sk });
    } catch (err) {
        return res.status(500).json({ error: err.toString() });
    }
});

// delete stream key
app.delete('/stream-keys/:key', async (req, res) => {
    try {
        const { key } = req.params;

        if (!streamKeys[key]) {
            return res.status(404).json({ error: 'Stream key not found' });
        }

        const url = `http://localhost:9997/v3/config/paths/remove/${encodeURIComponent(key)}`;
        const resp = await fetch(url, { method: 'POST' });

        const data = await resp.json().catch(() => null);
        if (!resp.ok || data?.error) {
            return res.status(500).json({
                error: data?.error || `MediaMTX returned ${resp.status}`,
            });
        }

        delete streamKeys[key];
        return res.json({ message: 'Stream key deleted' });
    } catch (err) {
        return res.status(500).json({ error: err.toString() });
    }
});

// list stream keys
app.get('/stream-keys', (req, res) => {
    return res.json(Object.values(streamKeys));
});

/* ======================
 * Pipeline APIs
 * ====================== */

// create pipeline
app.post('/pipelines', (req, res) => {
    try {
        const pipeline = new Pipeline({
            name: req.body?.name,
            streamKey: 'sample-stream-key',
        });

        return res.status(201).json({
            message: 'Pipeline created',
            pipeline,
        });
    } catch (err) {
        return res.status(400).json({ error: err.message });
    }
});

// update pipeline
app.post('/pipelines/:id', (req, res) => {
    try {
        const pipeline = new Pipeline({
            id: req.params.id,
            name: req.body?.name,
            updatedAt: new Date().toISOString(),
        });

        return res.json({
            message: 'Pipeline updated',
            pipeline,
        });
    } catch (err) {
        return res.status(400).json({ error: err.message });
    }
});

// delete pipeline
app.delete('/pipelines/:id', (req, res) => {
    return res.json({ message: `Pipeline ${req.params.id} deleted` });
});

// list pipelines
app.get('/pipelines', (req, res) => {
    const pipelines = [
        new Pipeline({ id: '1', name: 'Pipeline 1' }),
        new Pipeline({ id: '2', name: 'Pipeline 2' }),
    ];
    return res.json(pipelines);
});

/* ======================
 * Output APIs
 * ====================== */

// list outputs
app.get('/pipelines/:id/outputs', (req, res) => {
    const outputs = [
        new Output({
            id: 'out1',
            type: 'rtmp',
            url: 'rtmp://example.com/live/stream1',
        }),
        new Output({
            id: 'out2',
            type: 'rtmp',
            url: 'rtmp://example.com/live/stream2',
        }),
    ];
    return res.json(outputs);
});

// create output
app.post('/pipelines/:pipelineId/outputs', (req, res) => {
    try {
        const output = new Output(req.body);
        return res.status(201).json({
            message: 'Output created',
            output,
        });
    } catch (err) {
        return res.status(400).json({ error: err.message });
    }
});

// update output
app.post('/pipelines/:pipelineId/outputs/:outputId', (req, res) => {
    try {
        const output = new Output({
            id: req.params.outputId,
            type: req.body?.type,
            url: req.body?.url,
        });

        return res.json({
            message: 'Output updated',
            output,
        });
    } catch (err) {
        return res.status(400).json({ error: err.message });
    }
});

// delete output
app.delete('/pipelines/:pipelineId/outputs/:outputId', (req, res) => {
    return res.json({
        message: `Output ${req.params.outputId} from pipeline ${req.params.pipelineId} deleted`,
    });
});

// get output detail
app.get('/pipelines/:pipelineId/outputs/:outputId', (req, res) => {
    const output = new Output({
        id: req.params.outputId,
        type: 'rtmp',
        url: 'rtmp://example.com/live/stream1',
    });
    return res.json(output);
});

// start output
app.post('/pipelines/:pipelineId/outputs/:outputId/start', (req, res) => {
    return res.json({
        message: `Output ${req.params.outputId} from pipeline ${req.params.pipelineId} started`,
        success: true,
    });
});

// stop output
app.post('/pipelines/:pipelineId/outputs/:outputId/stop', (req, res) => {
    return res.json({
        message: `Output ${req.params.outputId} from pipeline ${req.params.pipelineId} stopped`,
        success: true,
    });
});

/* ======================
 * Metrics
 * ====================== */

app.get('/inputs', async (req, res) => {
    try {
        const resp = await fetch('http://localhost:9997/v3/paths/list');
        const data = await resp.json();
        res.json(data.items);
    } catch (err) {
        res.status(500).json({ error: err.toString() });
    }
});

/* ======================
 * Static UI & Server
 * ====================== */

app.use('/', express.static(path.join(__dirname, 'ui')));

app.listen(3030, () => console.log('Controller running on 3030'));
