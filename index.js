const express = require("express");
const {spawn} = require("child_process");
const path = require("path");

const app = express();
app.use(express.json());

let jobs = {}; // keep FFmpeg processes


// we should have the stream key management api here first.

// Endpoint: create a stream key
const crypto = require("crypto");
let streamKeys = {};

app.post("/stream-keys", async (req, res) => {
    try {
        const {streamKey, label} = req.body || {};
        const key = streamKey || crypto.randomBytes(12).toString("hex");

        if (streamKeys[key]) {
            return res.status(409).json({error: "Stream key already exists"});
        }

        const url = `http://localhost:9997/v3/config/paths/add/${encodeURIComponent(key)}`;
        const resp = await fetch(url, {
            method: "POST",
            headers: {"Content-Type": "application/json"},
            body: JSON.stringify({name: key}),
        });

        let data = null;
        try {
            data = await resp.json();
        } catch (e) { /* no JSON body */
            console.log("No JSON body in MediaMTX response");
        }

        if (!resp.ok || data?.error) {
            return res.status(500).json({error: data?.error || `MediaMTX returned ${resp.status}`});
        }

        streamKeys[key] = {key, label: label || null, createdAt: new Date().toISOString()};
        return res.status(201).json({message: "Stream key created", streamKey: key});
    } catch (err) {
        console.error("Error creating stream key", err);
        return res.status(500).json({error: err.toString()});
    }
});

// Endpoint: delete a stream key

// Endpoint: list all stream keys


// and then, the pipeline mgmt APIs go here

// Endpoint: create a new pipeline

// Endpoint: delete a pipeline

// Endpoint: list all the pipelines


// and then, the output mgmt APIs go here

// Endpoint: create an output

// Endpoint: delete an output

// Endpoint: start an output

// Endpoint: stop an output

// Endpoint: list all the outputs

// and then, the Metrics APIs go here


// Endpoint: Get Metrics

//

// List active inputs (ask MediaMTX)
app.get("/inputs", async (req, res) => {
    try {
        const resp = await fetch("http://localhost:9997/v3/paths/list");
        const data = await resp.json();
        res.json(data.items);
    } catch (err) {
        console.log("Error fetching /inputs", err);
        res.status(500).json({error: err.toString()});
    }
});

// Add output
app.post("/outputs/add", (req, res) => {
    const {inputPath, outputUrl} = req.body;

    const inputUrl = `rtmp://mediamtx:9997/${inputPath}`;

    const cmd = spawn("ffmpeg", [
        "-i",
        inputUrl,
        "-c",
        "copy",
        "-f",
        "flv",
        outputUrl,
    ]);

    const jobId = Date.now().toString();
    jobs[jobId] = cmd;

    cmd.stderr.on("data", (d) => console.log(`[${jobId}] ${d}`));
    cmd.on("exit", () => delete jobs[jobId]);

    res.json({jobId});
});

app.use("/dashboard", express.static(path.join(__dirname, "ui")));

app.listen(3030, () => console.log("Controller running on 3030"));
