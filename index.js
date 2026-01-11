const express = require('express');
const { spawn } = require('child_process');
const path = require('path');

const app = express();
app.use(express.json());

let jobs = {}; // keep FFmpeg processes

// we should have the stream key management api here first.

// Endpoint: create a stream key
const crypto = require('crypto');
let streamKeys = {};

// todo storage for keys as we need to have descriptive names.
// create a stream key
app.post('/stream-keys', async (req, res) => {
    try {
        const { streamKey, label } = req.body || {};
        const key = streamKey || crypto.randomBytes(12).toString('hex');

        if (streamKeys[key]) {
            return res.status(409).json({ error: 'Stream key already exists' });
        }

        const url = `http://localhost:9997/v3/config/paths/add/${encodeURIComponent(key)}`;
        const resp = await fetch(url, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ name: key }),
        });

        let data = null;
        try {
            data = await resp.json();
        } catch (e) {
            /* no JSON body */
            console.log('No JSON body in MediaMTX response');
        }

        if (!resp.ok || data?.error) {
            return res
                .status(500)
                .json({ error: data?.error || `MediaMTX returned ${resp.status}` });
        }

        streamKeys[key] = { key, label: label || null, createdAt: new Date().toISOString() };
        return res.status(201).json({ message: 'Stream key created', streamKey: key });
    } catch (err) {
        console.error('Error creating stream key', err);
        return res.status(500).json({ error: err.toString() });
    }
});

// update a stream key's label
app.post('/stream-keys/:key', (req, res) => {
    try {
        const { key } = req.params;
        const { label } = req.body || {};

        if (!streamKeys[key]) {
            return res.status(404).json({ error: 'Stream key not found' });
        }

        streamKeys[key].label = label || null;
        return res.json({ message: 'Stream key updated', streamKey: streamKeys[key] });
    } catch (err) {
        console.error('Error updating stream key', err);
        return res.status(500).json({ error: err.toString() });
    }
});


// Endpoint: delete a stream key
app.delete('/stream-keys/:key', async (req, res) => {
    try {
        const { key } = req.params;

        if (!streamKeys[key]) {
            return res.status(404).json({ error: 'Stream key not found' });
        }

        const url = `http://localhost:9997/v3/config/paths/remove/${encodeURIComponent(key)}`;
        const resp = await fetch(url, { method: 'POST' });

        let data = null;
        try {
            data = await resp.json();
        } catch (e) {
            /* no JSON body */
            console.log('No JSON body in MediaMTX response');
        }

        if (!resp.ok || data?.error) {
            return res
                .status(500)
                .json({ error: data?.error || `MediaMTX returned ${resp.status}` });
        }

        delete streamKeys[key];
        return res.json({ message: 'Stream key deleted' });
    } catch (err) {
        console.error('Error deleting stream key', err);
        return res.status(500).json({ error: err.toString() });
    }
});

// Endpoint: list all stream keys
app.get('/stream-keys', (req, res) => {
    return res.json(Object.values(streamKeys));
});

// and then, the pipeline mgmt APIs go here

// Endpoint: create a new pipeline
app.post('/pipelines', (req, res) => {
    // parse the request first
    const { name } = req.body || {};
    if (!name) {
        return res.status(400).json({ error: 'Pipeline name is required' });
    }
    // placeholder implementation
    // return the pipeline object
    const pipeline = {
        id: Date.now().toString(),
        name,
        createdAt: new Date().toISOString(),
    };
    return res.status(201).json({ message: 'Pipeline created', pipeline });
});

// Endpoint: update an existing pipeline
// we are now defining the signature of the API here. the body of implementation could be a placeholder for now.
app.post('/pipelines/:id', (req, res) => {
    const { id } = req.params;
    const { name } = req.body || {};
    if (!name) {
        return res.status(400).json({ error: 'Pipeline name is required' });
    }
    // placeholder implementation
    const pipeline = {
        id,
        name,
        updatedAt: new Date().toISOString(),
    };
    return res.json({ message: 'Pipeline updated', pipeline });
});


// Endpoint: delete a pipeline
app.delete('/pipelines/:id', (req, res) => {
    const { id } = req.params;
    // placeholder implementation
    return res.json({ message: `Pipeline ${id} deleted` });
});



// Endpoint: list all the pipelines
app.get('/pipelines', (req, res) => {
    // placeholder implementation
    const pipelines = [
        { id: '1', name: 'Pipeline 1', createdAt: new Date().toISOString(), updatedAt: new Date().toISOString()
        },
        { id: '2', name: 'Pipeline 2', createdAt: new Date().toISOString(), updatedAt: new Date().toISOString()
        },
    ];
    return res.json(pipelines);
});

// and then, the output mgmt APIs go here

// list all outputs from a pipeline
app.get('/pipelines/:id/outputs', (req, res) => {
    const { id } = req.params;
    // placeholder implementation
    const outputs = [
        {
            id: 'out1',
            name: 'Output 1', // descriptive name
            url: 'rtmp://example.com/live/stream1', // we dont care if its a FB YT or sth.
        },
        {
            id: 'out2',
            name: 'Output 2', // descriptive name
            url: 'rtmp://example.com/live/stream1', // we dont care if its a FB YT or sth.
        },
    ];
    return res.json(outputs);
});


// Endpoint: create an output in a pipeline
app.post('/pipelines/:pipelineId/outputs', (req, res) => {
    const { pipelineId } = req.params;
    const { type, url } = req.body || {};
    if (!type || !url) {
        return res.status(400).json({ error: 'Output type and URL are required' });
    }
    // placeholder implementation
    const output = {
        id: Date.now().toString(),
        type,
        url,
    };
    return res.status(201).json({ message: 'Output created', output });
});


// Endpoint: update an output in a pipeline
app.post('/pipelines/:pipelineId/outputs/:outputId', (req, res) => {
    const { pipelineId, outputId } = req.params;
    const { type, url } = req.body || {};
    if (!type || !url) {
        return res.status(400).json({ error: 'Output type and URL are required' });
    }
    // placeholder implementation
    const output = {
        id: outputId,
        type,
        url,
    };
    return res.json({ message: 'Output updated', output });
});


// Endpoint: delete an output from a pipeline
app.delete('/pipelines/:pipelineId/outputs/:outputId', (req, res) => {
    const { pipelineId, outputId } = req.params;
    // placeholder implementation
    return res.json({ message: `Output ${outputId} from pipeline ${pipelineId} deleted` });
});


// Endpoint: get details of an output in a pipeline
app.get('/pipelines/:pipelineId/outputs/:outputId', (req, res) => {
    const { pipelineId, outputId } = req.params;
    // placeholder implementation
    const output = {
        id: outputId,
        name: 'rtmp',
        url: 'rtmp://example.com/live/stream1',
    };
    return res.json(output);
});




// Endpoint: start an output
app.post('/pipelines/:pipelineId/outputs/:outputId/start', (req, res) => {
    const { pipelineId, outputId } = req.params;
    // placeholder implementation
    return res.json({ message: `Output ${outputId} from pipeline ${pipelineId} started`, success: true });
});

// Endpoint: stop an output
app.post('/pipelines/:pipelineId/outputs/:outputId/stop', (req, res) => {
    const { pipelineId, outputId } = req.params;
    // placeholder implementation
    return res.json({ message: `Output ${outputId} from pipeline ${pipelineId} stopped`, success: true });
});

// Metrics APIs go here
// Endpoint: Get Metrics
// status: OFF, ACTIVE, WARNING, ERROR


// Metrics: List active inputs (ask MediaMTX)
app.get('/inputs', async (req, res) => {
    try {
        const resp = await fetch('http://localhost:9997/v3/paths/list');
        const data = await resp.json();
        res.json(data.items);
    } catch (err) {
        console.log('Error fetching /inputs', err);
        res.status(500).json({ error: err.toString() });
    }
});

// todo: more metrics endpoints as needed, e.g., pipelines, RTMP outputs. thru MediaMTX API.


app.use('/dashboard', express.static(path.join(__dirname, 'ui')));

app.listen(3030, () => console.log('Controller running on 3030'));
