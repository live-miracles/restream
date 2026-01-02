const express = require("express");
const { spawn } = require("child_process");
const path = require("path");

const app = express();
app.use(express.json());

let jobs = {}; // keep FFmpeg processes

// List active inputs (ask MediaMTX)
app.get("/inputs", async (req, res) => {
  try {
    const resp = await fetch("http://localhost:9997/v3/paths/list");
    const data = await resp.json();
    res.json(data.items);
  } catch (err) {
    console.log("Error fetching /inputs", err);
    res.status(500).json({ error: err.toString() });
  }
});

// Add output
app.post("/outputs/add", (req, res) => {
  const { inputPath, outputUrl } = req.body;

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

  res.json({ jobId });
});

app.use("/dashboard", express.static(path.join(__dirname, "ui")));

app.listen(3030, () => console.log("Controller running on 3030"));
