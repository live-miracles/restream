#!/usr/bin/env node
import { spawn } from 'node:child_process';
import { closeSync, openSync } from 'node:fs';
import { access, mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

const rootDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
process.chdir(rootDir);

const defaults = {
    apiUrl: 'http://localhost:3030',
    rtmpStatUrl: 'http://localhost:8081/stat',
    manifestPath: 'test/artifacts/session-4x3-manifest.json',
    logDir: 'test/artifacts/logs',
    appLogPath: 'test/artifacts/logs/app-under-test.log',
    verifyAppRetries: 30,
    inputFile: 'test/colorbar-timer.mp4',
    rtmpOutputBase: 'rtmp://localhost:1936/live',
    inputProtocols: 'rtmp,rtsp,srt',
    maxRetries: 30,
    retryDelaySec: 1,
    timeoutSec: 180,
    pollSec: 2,
    outDir: 'test/artifacts/runs',
};

const SRT_LOOPBACK_TIMEOUT_SEC = 10;

const config = {
    apiUrl: process.env.API_URL || defaults.apiUrl,
    rtmpStatUrl: process.env.RTMP_STAT_URL || defaults.rtmpStatUrl,
    manifestPath: resolvePath(process.env.MANIFEST_PATH || defaults.manifestPath),
    logDir: resolvePath(process.env.LOG_DIR || defaults.logDir),
    appLogPath: resolvePath(process.env.APP_LOG_PATH || defaults.appLogPath),
    verifyAppRetries: Number(process.env.VERIFY_APP_RETRIES || defaults.verifyAppRetries),
    inputFile: resolvePath(process.env.INPUT_FILE || defaults.inputFile),
    rtmpOutputBase: process.env.RTMP_OUTPUT_BASE || defaults.rtmpOutputBase,
    inputProtocols: process.env.INPUT_PROTOCOLS || defaults.inputProtocols,
    maxRetries: Number(process.env.MAX_RETRIES || defaults.maxRetries),
    retryDelaySec: Number(process.env.RETRY_DELAY_SEC || defaults.retryDelaySec),
    timeoutSec: Number(process.env.TIMEOUT_SEC || defaults.timeoutSec),
    pollSec: Number(process.env.POLL_SEC || defaults.pollSec),
    outDir: resolvePath(process.env.OUT_DIR || defaults.outDir),
    cleanStart: readBooleanEnv('CLEAN_START', true),
    keepRunning: readBooleanEnv('KEEP_RUNNING', false),
};

const outputEncodingDefaults = ['source', 'vertical-crop', 'vertical-rotate', '720p', '1080p'];
const supportedOutputEncodings = new Set(outputEncodingDefaults);
const nonSourceOutputEncodings = outputEncodingDefaults.filter((encoding) => encoding !== 'source');
const outputEncodingUsage = new Map(outputEncodingDefaults.map((encoding) => [encoding, 0]));

const ownedProcesses = [];
let shutdownPromise = null;

if (process.argv.includes('--help') || process.argv.includes('-h')) {
    printHelp();
    process.exit(0);
}

registerSignalHandlers();

try {
    await main();
} catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
} finally {
    await shutdown(config.keepRunning);
}

async function main() {
    const manifest = await loadManifest(config.manifestPath);

    if (config.cleanStart) {
        await cleanStart();
    } else {
        await ensureApiReachable();
    }

    console.log('== Step 1: Ensure 4x3 manifest resources ==');
    const resolved = await ensureResources(manifest);

    console.log('== Step 1b: Verify new outputs default to desiredState=stopped ==');
    await verifyDesiredStateForOutputs(resolved.outputs, 'stopped');

    console.log('== Step 2: Start mixed-protocol input publishers (RTMP/RTSP/SRT) ==');
    const inputPublishers = await startInputPublishers(manifest);

    console.log('== Step 3: Start outputs from manifest ==');
    await startOutputs(resolved.outputs);

    console.log('== Step 3b: Verify started outputs persist desiredState=running ==');
    await verifyDesiredStateForOutputs(resolved.outputs, 'running');

    console.log('== Step 4: Wait for all manifest inputs/outputs active ==');
    await waitForActive(resolved);

    console.log('== Step 5: Capture health + stat snapshots ==');
    await captureHealthSnapshot(resolved);

    console.log('== Step 6: Correlate nginx-rtmp /stat with expected outputs (final check) ==');
    await verifyRtmpStat(resolved);

    console.log('== Step 7: Verify stop is idempotent and start restores desiredState ==');
    await verifyIdempotentOutputStopStart(resolved.outputs[0]);

    console.log('== Step 8: Verify output retry recovers after RTMP sink outage ==');
    await verifyOutputRetryOnSinkRecovery(resolved.outputs[0], resolved);

    console.log('== Step 9: Verify output retry recovers after unexpected SIGKILL ==');
    await verifyOutputRetryOnUnexpectedCleanExit(resolved.outputs[0], resolved);

    console.log('== Step 10: Verify input recovery restarts outputs with desiredState=running ==');
    await verifyInputRecoveryRestart(
        resolved.pipelines[0],
        resolved.outputs.filter((target) => target.pipelineId === resolved.pipelines[0]?.pipelineId),
        inputPublishers,
    );

    console.log(
        '== Step 11: Verify SRT output loopback activates target pipeline input via MediaMTX ==',
    );
    await verifySrtOutputLoopbackToPipelineInput(resolved, inputPublishers);

    console.log('== 4x3 run complete ==');
}

function resolvePath(targetPath) {
    return path.isAbsolute(targetPath) ? targetPath : path.resolve(rootDir, targetPath);
}

function relativePath(targetPath) {
    return path.relative(rootDir, targetPath) || '.';
}

function readBooleanEnv(name, defaultValue) {
    const value = process.env[name];
    if (value == null || value === '') {
        return defaultValue;
    }
    return !['0', 'false', 'no', 'off'].includes(String(value).toLowerCase());
}

function printHelp() {
    console.log(
        `Usage: node test/artifacts/run-4x3.mjs\n\nEnvironment flags:\n  CLEAN_START=1    Tear down stale state and launch a fresh stack (default)\n  KEEP_RUNNING=1   Leave backend and publishers running after the run\n  MANIFEST_PATH    Path to the tracked 4x3 manifest\n  API_URL          Backend base URL (default: ${defaults.apiUrl})\n  RTMP_STAT_URL    nginx-rtmp stat URL (default: ${defaults.rtmpStatUrl})\n  RTMP_OUTPUT_BASE Base URL used for RTMP outputs (default: ${defaults.rtmpOutputBase})`,
    );
}

function registerSignalHandlers() {
    for (const signal of ['SIGINT', 'SIGTERM']) {
        process.on(signal, () => {
            void shutdown(config.keepRunning).finally(() => {
                process.exit(signal === 'SIGINT' ? 130 : 143);
            });
        });
    }
}

async function shutdown(leaveRunning) {
    if (shutdownPromise) {
        return shutdownPromise;
    }

    shutdownPromise = (async () => {
        if (leaveRunning) {
            console.log('== KEEP_RUNNING=1: leaving input publishers and app running ==');
            return;
        }

        for (const proc of ownedProcesses.reverse()) {
            await terminateProcess(proc);
        }
    })();

    return shutdownPromise;
}

async function cleanStart() {
    console.log('== Clean start: tear down stale processes and state ==');
    await runCommand('bash', ['scripts/down.sh'], { allowFailure: true, stdio: 'inherit' });

    await runCommand('docker', ['compose', 'up', '-d', 'mediamtx', 'nginx-rtmp'], {
        stdio: 'inherit',
    });

    await mkdir(config.logDir, { recursive: true });
    await writeFile(config.appLogPath, '', 'utf8');

    console.log('== Clean start: launch backend and wait for health ==');
    const appPid = spawnDetachedProcess({
        name: 'backend',
        command: process.execPath,
        args: ['src/index.js'],
        logPath: config.appLogPath,
    });
    console.log(`Started backend pid=${appPid} log=${relativePath(config.appLogPath)}`);

    await waitForApiHealth();
}

async function waitForApiHealth() {
    for (let attempt = 1; attempt <= config.verifyAppRetries; attempt += 1) {
        try {
            const response = await fetch(`${config.apiUrl}/health`, {
                signal: AbortSignal.timeout(5000),
            });
            if (response.ok) {
                return;
            }
        } catch {
            // Retry.
        }
        await sleep(1000);
    }

    const appLog = await safeReadFile(config.appLogPath);
    throw new Error(
        `API did not become healthy at ${config.apiUrl}/health\nRecent app log:\n${tailText(appLog, 120)}`,
    );
}

async function ensureApiReachable() {
    try {
        const response = await fetch(`${config.apiUrl}/health`, {
            signal: AbortSignal.timeout(5000),
        });
        if (!response.ok) {
            throw new Error(`HTTP ${response.status}`);
        }
    } catch (error) {
        throw new Error(
            `API is not reachable at ${config.apiUrl}. Start app first (for example: make run-host). ${String(error)}`,
        );
    }
}

async function loadManifest(manifestPath) {
    const raw = await readFile(manifestPath, 'utf8');
    const manifest = JSON.parse(raw);
    if (!Array.isArray(manifest.pipelines) || manifest.pipelines.length === 0) {
        throw new Error(`Manifest is empty or invalid: ${relativePath(manifestPath)}`);
    }
    return manifest;
}

async function ensureResources(manifest) {
    const pipelineTargets = [];
    const outputTargets = [];
    let state = await fetchConfigState();

    for (const [pipelineIndex, pipelineDef] of manifest.pipelines.entries()) {
        let streamKey = state.streamKeys.find((item) => item.key === pipelineDef.streamKey);
        if (!streamKey) {
            await requestJson('/stream-keys', {
                method: 'POST',
                body: { streamKey: pipelineDef.streamKey, label: pipelineDef.name },
                okStatuses: [201],
            });
            console.log(`Created stream key for ${pipelineDef.name}: ${pipelineDef.streamKey}`);
            state = await fetchConfigState();
            streamKey = state.streamKeys.find((item) => item.key === pipelineDef.streamKey);
        } else {
            console.log(`Stream key exists for ${pipelineDef.name}: ${pipelineDef.streamKey}`);
        }

        let pipeline = state.pipelines.find(
            (item) => item.name === pipelineDef.name && item.streamKey === pipelineDef.streamKey,
        );
        if (!pipeline) {
            const result = await requestJson('/pipelines', {
                method: 'POST',
                body: { name: pipelineDef.name, streamKey: pipelineDef.streamKey },
                okStatuses: [201],
            });
            pipeline = result.json.pipeline;
            console.log(`Created pipeline ${pipelineDef.name}: ${pipeline.id}`);
            state = await fetchConfigState();
        } else {
            console.log(`Pipeline exists ${pipelineDef.name}: ${pipeline.id}`);
        }

        pipelineTargets.push({
            name: pipelineDef.name,
            streamKey: pipelineDef.streamKey,
            pipelineId: pipeline.id,
        });

        for (const [outputIndex, outputDef] of pipelineDef.outputs.entries()) {
            const outputUrl = normalizeOutputUrl(outputDef.url);
            const encoding = resolveOutputEncoding(outputDef.encoding);
            let output = state.outputs.find(
                (item) =>
                    item.pipelineId === pipeline.id &&
                    item.name === outputDef.name &&
                    item.url === outputUrl,
            );

            if (!output) {
                const outputWithSameName = state.outputs.find(
                    (item) => item.pipelineId === pipeline.id && item.name === outputDef.name,
                );

                if (outputWithSameName) {
                    const result = await requestJson(
                        `/pipelines/${pipeline.id}/outputs/${outputWithSameName.id}`,
                        {
                            method: 'POST',
                            body: { name: outputDef.name, url: outputUrl, encoding },
                            okStatuses: [200],
                        },
                    );
                    output = result.json.output;
                    console.log(`  Updated output ${outputDef.name}: ${output.id} -> ${outputUrl}`);
                    state = await fetchConfigState();
                } else {
                    const result = await requestJson(`/pipelines/${pipeline.id}/outputs`, {
                        method: 'POST',
                        body: { name: outputDef.name, url: outputUrl, encoding },
                        okStatuses: [201],
                    });
                    output = result.json.output;
                    console.log(`  Created output ${outputDef.name}: ${output.id}`);
                    state = await fetchConfigState();
                }
            } else {
                console.log(`  Output exists ${outputDef.name}: ${output.id}`);
            }

            outputTargets.push({
                pipelineId: pipeline.id,
                pipelineName: pipelineDef.name,
                outputId: output.id,
                outputName: outputDef.name,
                outputUrl,
            });
        }
    }

    console.log(`Manifest used (not modified): ${relativePath(config.manifestPath)}`);
    console.log(`Pipelines in manifest: ${pipelineTargets.length}`);
    console.log(`Outputs in manifest: ${outputTargets.length}`);

    return { pipelines: pipelineTargets, outputs: outputTargets };
}

function resolveOutputEncoding(encodingValue) {
    const normalized = String(encodingValue || '')
        .trim()
        .toLowerCase();
    if (normalized) {
        if (!supportedOutputEncodings.has(normalized)) {
            throw new Error(`Unsupported output encoding in manifest: ${encodingValue}`);
        }
        outputEncodingUsage.set(normalized, (outputEncodingUsage.get(normalized) || 0) + 1);
        return normalized;
    }

    const selectedFallback =
        nonSourceOutputEncodings.find((encoding) => (outputEncodingUsage.get(encoding) || 0) < 1) ||
        'source';
    outputEncodingUsage.set(selectedFallback, (outputEncodingUsage.get(selectedFallback) || 0) + 1);
    return selectedFallback;
}

async function fetchConfigState() {
    const [streamKeysResult, pipelinesResult, configResult] = await Promise.all([
        requestJson('/stream-keys'),
        requestJson('/pipelines'),
        requestJson('/config'),
    ]);

    return {
        streamKeys: Array.isArray(streamKeysResult.json) ? streamKeysResult.json : [],
        pipelines: Array.isArray(pipelinesResult.json) ? pipelinesResult.json : [],
        outputs: Array.isArray(configResult.json?.outputs) ? configResult.json.outputs : [],
        jobs: Array.isArray(configResult.json?.jobs) ? configResult.json.jobs : [],
    };
}

async function startInputPublishers(manifest) {
    await access(config.inputFile);
    await mkdir(config.logDir, { recursive: true });

    const state = await fetchConfigState();
    const streamKeysByKey = new Map(
        (state.streamKeys || []).map((streamKey) => [streamKey.key, streamKey]),
    );

    const protocols = config.inputProtocols
        .split(',')
        .map((value) => value.trim().toLowerCase())
        .filter(Boolean);

    if (protocols.length === 0) {
        throw new Error('No input protocols configured');
    }

    const streamKeys = manifest.pipelines.map((pipeline) => pipeline.streamKey);
    if (streamKeys.length === 0) {
        throw new Error(`No stream keys found in manifest: ${relativePath(config.manifestPath)}`);
    }

    const publisherTargets = [];

    for (const [index, streamKey] of streamKeys.entries()) {
        const ordinal = index + 1;
        const protocol = protocols[index % protocols.length];
        const streamKeyRecord = streamKeysByKey.get(streamKey) || null;
        const targetUrl = selectIngestUrl(streamKeyRecord, protocol);
        const logPath = path.join(config.logDir, `input-${ordinal}-${protocol}.log`);
        const publisherTarget = {
            ordinal,
            pipelineName: manifest.pipelines[index]?.name || `Pipeline ${ordinal}`,
            streamKey,
            protocol,
            targetUrl,
            logPath,
            pid: null,
        };
        const pid = spawnInputPublisher(publisherTarget);

        console.log(
            `[${ordinal}/${streamKeys.length}] protocol=${protocol} streamKey=${streamKey} target=${targetUrl}`,
        );
        console.log(`  pid=${pid} log=${relativePath(logPath)}`);
        publisherTargets.push(publisherTarget);
    }

    return publisherTargets;
}

function spawnInputPublisher(target) {
    const pid = spawnDetachedProcess({
        name: `input-${target.ordinal}`,
        command: 'ffmpeg',
        args: buildFfmpegArgs(target.protocol, target.targetUrl),
        logPath: target.logPath,
    });
    target.pid = pid;
    return pid;
}

async function stopInputPublisher(target) {
    if (!target?.pid) return;
    await terminateProcess(target);
    target.pid = null;
}

async function stopAllInputPublishersForStreamKey(streamKey, inputPublishers) {
    const matchedManagedPublishers = (inputPublishers || []).filter(
        (target) => target?.streamKey === streamKey,
    );

    for (const target of matchedManagedPublishers) {
        await stopInputPublisher(target);
    }

    const inputFileMarker = relativePath(config.inputFile);
    const processes = await listFfmpegProcesses();
    const stalePublishers = processes.filter(
        (proc) =>
            proc.command.includes('ffmpeg') &&
            proc.command.includes(inputFileMarker) &&
            proc.command.includes(streamKey),
    );

    for (const proc of stalePublishers) {
        await terminateProcess({ pid: proc.pid, name: `stale-input-${streamKey}` });
    }

    return {
        managedStopped: matchedManagedPublishers.length,
        staleStopped: stalePublishers.length,
    };
}

async function restartInputPublisher(target) {
    if (!target) {
        throw new Error('No publisher target available for restart');
    }
    if (target.pid && isProcessAlive(target.pid)) {
        await stopInputPublisher(target);
    }
    const pid = spawnInputPublisher(target);
    console.log(
        `Restarted input publisher ${target.pipelineName} protocol=${target.protocol} streamKey=${target.streamKey} pid=${pid}`,
    );
    return pid;
}

function selectIngestUrl(streamKeyRecord, protocol) {
    const ingestUrls = streamKeyRecord?.ingestUrls || {};

    if (protocol === 'rtmp' && ingestUrls.rtmp) return ingestUrls.rtmp;
    if (protocol === 'rtsp' && ingestUrls.rtsp) return ingestUrls.rtsp;
    if (protocol === 'srt' && ingestUrls.srt) return ingestUrls.srt;

    throw new Error(`Missing ingest URL for protocol=${protocol} streamKey=${streamKeyRecord?.key || 'unknown'}`);
}

function buildFfmpegArgs(protocol, targetUrl) {
    const baseArgs = [
        '-nostdin',
        '-re',
        '-stream_loop',
        '-1',
        '-i',
        relativePath(config.inputFile),
        '-map',
        '0',
        '-c',
        'copy',
    ];
    if (protocol === 'rtmp') {
        return [...baseArgs, '-f', 'flv', targetUrl];
    }
    if (protocol === 'rtsp') {
        return [...baseArgs, '-f', 'rtsp', '-rtsp_transport', 'tcp', targetUrl];
    }
    if (protocol === 'srt') {
        return [...baseArgs, '-f', 'mpegts', targetUrl];
    }
    throw new Error(`Unsupported input protocol: ${protocol}`);
}

function normalizeOutputUrl(outputUrl) {
    if (!outputUrl || typeof outputUrl !== 'string') {
        return outputUrl;
    }

    try {
        const parsed = new URL(outputUrl);
        if (parsed.protocol !== 'rtmp:' && parsed.protocol !== 'rtmps:') {
            return outputUrl;
        }
    } catch {
        return outputUrl;
    }

    const streamName = extractStreamName(outputUrl);
    if (!streamName) {
        return outputUrl;
    }

    return `${String(config.rtmpOutputBase).replace(/\/+$/, '')}/${streamName}`;
}

async function startOutputs(outputs) {
    let count = 0;
    let ok = 0;

    for (const target of outputs) {
        count += 1;
        let started = false;

        for (let attempt = 1; attempt <= config.maxRetries; attempt += 1) {
            const result = await requestJson(
                `/pipelines/${target.pipelineId}/outputs/${target.outputId}/start`,
                {
                    method: 'POST',
                    okStatuses: [200, 201, 409],
                },
            );
            const errorMessage = result.json?.error || result.text || '';
            const label = `${target.pipelineName}/${target.outputName}`;

            if (result.status === 200 || result.status === 201) {
                ok += 1;
                started = true;
                console.log(
                    `[${count}] ${label} ${target.pipelineId}/${target.outputId} -> ${result.status} (attempt ${attempt})`,
                );
                break;
            }

            if (result.status === 409 && errorMessage.includes('already has a running job')) {
                ok += 1;
                started = true;
                console.log(
                    `[${count}] ${label} ${target.pipelineId}/${target.outputId} -> 409 already running (attempt ${attempt})`,
                );
                break;
            }

            if (result.status === 409 && errorMessage.includes('input is not available yet')) {
                await sleep(config.retryDelaySec * 1000);
                continue;
            }

            console.log(
                `[${count}] ${label} ${target.pipelineId}/${target.outputId} -> ${result.status} (attempt ${attempt})`,
            );
            if (errorMessage) {
                console.log(errorMessage);
            }
            break;
        }

        if (!started) {
            console.log(
                `[${count}] ${target.pipelineName}/${target.outputName} ${target.pipelineId}/${target.outputId} failed to start after ${config.maxRetries} attempts`,
            );
        }
    }

    console.log(`Started/Already-running outputs: ${ok}/${count}`);
}

async function verifyDesiredStateForOutputs(outputs, expectedDesiredState) {
    const state = await fetchConfigState();

    for (const target of outputs) {
        const output = state.outputs.find(
            (item) => item.pipelineId === target.pipelineId && item.id === target.outputId,
        );
        if (!output) {
            throw new Error(
                `Missing output in /config snapshot: ${target.pipelineId}/${target.outputId}`,
            );
        }
        if (output.desiredState !== expectedDesiredState) {
            throw new Error(
                `Expected desiredState=${expectedDesiredState} for ${target.pipelineName}/${target.outputName} ${target.pipelineId}/${target.outputId}, got ${output.desiredState}`,
            );
        }
    }
}

async function verifyIdempotentOutputStopStart(target) {
    if (!target) {
        throw new Error('No output target available for idempotent stop/start verification');
    }

    const label = `${target.pipelineName}/${target.outputName}`;
    const firstStop = await requestJson(
        `/pipelines/${target.pipelineId}/outputs/${target.outputId}/stop`,
        {
            method: 'POST',
            okStatuses: [200],
        },
    );
    if (firstStop.json?.desiredState !== 'stopped') {
        throw new Error(`Expected first stop desiredState=stopped for ${label}`);
    }

    await pollUntil(
        async () => {
            const state = await fetchConfigState();
            const job = state.jobs.find(
                (item) =>
                    item.pipelineId === target.pipelineId && item.outputId === target.outputId,
            );
            const output = state.outputs.find(
                (item) => item.pipelineId === target.pipelineId && item.id === target.outputId,
            );
            return output?.desiredState === 'stopped' && job?.status === 'stopped';
        },
        30000,
        500,
        `${label} stopped after first stop`,
    );

    const secondStop = await requestJson(
        `/pipelines/${target.pipelineId}/outputs/${target.outputId}/stop`,
        {
            method: 'POST',
            okStatuses: [200],
        },
    );
    if (secondStop.json?.result?.reason !== 'already_stopped') {
        throw new Error(
            `Expected second stop to be idempotent for ${label}, got ${secondStop.json?.result?.reason || 'unknown'}`,
        );
    }

    const restart = await requestJson(
        `/pipelines/${target.pipelineId}/outputs/${target.outputId}/start`,
        {
            method: 'POST',
            okStatuses: [200, 201, 409],
        },
    );
    if (![200, 201, 409].includes(restart.status)) {
        throw new Error(`Unexpected restart status for ${label}: ${restart.status}`);
    }

    await pollUntil(
        async () => {
            const state = await fetchConfigState();
            const output = state.outputs.find(
                (item) => item.pipelineId === target.pipelineId && item.id === target.outputId,
            );
            return output?.desiredState === 'running';
        },
        30000,
        500,
        `${label} desiredState restored to running`,
    );

    await waitForActive({
        pipelines: [{ pipelineId: target.pipelineId }],
        outputs: [target],
    });
}

function findLatestJobForOutput(state, pipelineId, outputId) {
    return (
        state.jobs.find(
            (item) => item.pipelineId === pipelineId && item.outputId === outputId,
        ) || null
    );
}

async function waitForRunningJobStability(target, minAgeMs = 5000) {
    const label = `${target.pipelineName}/${target.outputName}`;

    await pollUntil(
        async () => {
            const state = await fetchConfigState();
            const output = state.outputs.find(
                (item) => item.pipelineId === target.pipelineId && item.id === target.outputId,
            );
            const latestJob = findLatestJobForOutput(state, target.pipelineId, target.outputId);
            if (output?.desiredState !== 'running' || latestJob?.status !== 'running') {
                return false;
            }
            const startedAtMs = Date.parse(latestJob.startedAt || '');
            if (!Number.isFinite(startedAtMs)) {
                return false;
            }
            return Date.now() - startedAtMs >= minAgeMs;
        },
        minAgeMs + 30000,
        500,
        `${label} running job stable for ${minAgeMs}ms`,
    );
}

async function fetchOutputHistory(target, options = {}) {
    const query = new URLSearchParams();
    const {
        since = null,
        until = null,
        order = 'asc',
        limit = 200,
        filter = null,
    } = options;

    if (filter) query.set('filter', filter);
    if (since) query.set('since', since);
    if (until) query.set('until', until);
    if (order) query.set('order', order);
    if (limit) query.set('limit', String(limit));

    const result = await requestJson(
        `/pipelines/${target.pipelineId}/outputs/${target.outputId}/history?${query.toString()}`,
    );
    return Array.isArray(result.json?.logs) ? result.json.logs : [];
}

async function verifyOutputRetryOnSinkRecovery(target, resolved) {
    if (!target) {
        throw new Error('No output target available for output retry verification');
    }

    const label = `${target.pipelineName}/${target.outputName}`;
    const failureSince = new Date().toISOString();

    console.log('Stopping nginx-rtmp container to force output delivery failures');
    await runCommand('docker', ['compose', 'stop', 'nginx-rtmp'], { stdio: 'inherit' });

    await pollUntil(
        async () => {
            const state = await fetchConfigState();
            const output = state.outputs.find(
                (item) => item.pipelineId === target.pipelineId && item.id === target.outputId,
            );
            const latestJob = findLatestJobForOutput(state, target.pipelineId, target.outputId);
            return output?.desiredState === 'running' && latestJob?.status === 'failed';
        },
        30000,
        500,
        `${label} to fail while desiredState stays running`,
    );

    await pollUntil(
        async () => {
            const logs = await fetchOutputHistory(target, {
                since: failureSince,
                filter: 'lifecycle',
                order: 'asc',
                limit: 200,
            });
            return logs.some(
                (log) =>
                    String(log.message || '').startsWith('[lifecycle] retry_decision') &&
                    /scheduled=true/.test(String(log.message || '')),
            );
        },
        30000,
        500,
        `${label} auto-retry decision after sink outage`,
    );

    console.log('Restarting nginx-rtmp container to allow auto-retry recovery');
    await runCommand('docker', ['compose', 'up', '-d', 'nginx-rtmp'], { stdio: 'inherit' });

    await pollUntil(
        async () => {
            const logs = await fetchOutputHistory(target, {
                since: failureSince,
                filter: 'lifecycle',
                order: 'asc',
                limit: 200,
            });
            return logs.some(
                (log) =>
                    String(log.message || '').startsWith('[lifecycle] started') &&
                    /trigger=auto-retry/.test(String(log.message || '')),
            );
        },
        90000,
        1000,
        `${label} auto-retry restart after sink recovery`,
    );

    await waitForActive(resolved);
}

function getExpectedReaderTag(pipelineId, outputId) {
    return `reader_${pipelineId}_${outputId}`.replace(/[^a-zA-Z0-9_-]/g, '_');
}

async function listFfmpegProcesses() {
    return await new Promise((resolve, reject) => {
        const child = spawn('ps', ['-eo', 'pid=,args='], {
            cwd: rootDir,
            env: process.env,
            stdio: ['ignore', 'pipe', 'pipe'],
        });

        let stdout = '';
        let stderr = '';

        child.stdout.on('data', (chunk) => {
            stdout += chunk.toString();
        });

        child.stderr.on('data', (chunk) => {
            stderr += chunk.toString();
        });

        child.on('error', reject);
        child.on('close', (code) => {
            if (code !== 0) {
                reject(new Error(`ps -eo pid=,args= failed${stderr ? `: ${stderr.trim()}` : ''}`));
                return;
            }

            const processes = stdout
                .split('\n')
                .map((line) => line.trim())
                .filter(Boolean)
                .map((line) => {
                    const match = line.match(/^(\d+)\s+(.*)$/);
                    if (!match) {
                        return null;
                    }
                    return {
                        pid: Number(match[1]),
                        command: match[2],
                    };
                })
                .filter(Boolean);

            resolve(processes);
        });
    });
}

async function findOutputFfmpegProcess(target) {
    const readerTag = getExpectedReaderTag(target.pipelineId, target.outputId);
    const processes = await listFfmpegProcesses();
    const matches = processes.filter(
        (proc) => proc.command.includes('ffmpeg') && proc.command.includes(readerTag),
    );

    if (matches.length === 0) {
        throw new Error(
            `No running FFmpeg process found for ${target.pipelineName}/${target.outputName} using reader tag ${readerTag}`,
        );
    }

    if (matches.length > 1) {
        throw new Error(
            `Multiple FFmpeg processes matched ${target.pipelineName}/${target.outputName} using reader tag ${readerTag}: ${matches.map((proc) => proc.pid).join(', ')}`,
        );
    }

    return {
        pid: matches[0].pid,
        readerTag,
        command: matches[0].command,
    };
}

async function requestUnexpectedSigkillExit(target) {
    const match = await findOutputFfmpegProcess(target);
    const pid = Number(match.pid);

    if (!Number.isInteger(pid) || pid <= 0) {
        throw new Error(
            `No running FFmpeg pid available for ${target.pipelineName}/${target.outputName}`,
        );
    }

    process.kill(pid, 'SIGKILL');
    return match;
}

function isUnexpectedSigkillExitLog(message) {
    const text = String(message || '');
    if (!text.startsWith('[lifecycle] exited')) {
        return false;
    }

    if (!/status=failed/.test(text) || !/requestedStop=false/.test(text)) {
        return false;
    }

    return /exitSignal=SIGKILL/.test(text) || /exitCode=null/.test(text);
}

async function verifyOutputRetryOnUnexpectedCleanExit(target, resolved) {
    if (!target) {
        throw new Error('No output target available for clean-exit retry verification');
    }

    const label = `${target.pipelineName}/${target.outputName}`;

    await waitForRunningJobStability(target, 5000);

    const failureSince = new Date().toISOString();

    const sigkillTarget = await requestUnexpectedSigkillExit(target);
    console.log(
        `Sent SIGKILL to ${target.pipelineName}/${target.outputName} ${target.pipelineId}/${target.outputId}`,
    );
    console.log(
        `  ffmpeg pid=${sigkillTarget.pid} readerTag=${sigkillTarget.readerTag} to verify auto-retry after external termination`,
    );

    await pollUntil(
        async () => {
            const logs = await fetchOutputHistory(target, {
                since: failureSince,
                filter: 'lifecycle',
                order: 'asc',
                limit: 200,
            });
            return logs.some((log) => isUnexpectedSigkillExitLog(log.message));
        },
        30000,
        500,
        `${label} unexpected SIGKILL exit`,
    );

    await pollUntil(
        async () => {
            const logs = await fetchOutputHistory(target, {
                since: failureSince,
                filter: 'lifecycle',
                order: 'asc',
                limit: 200,
            });
            return logs.some(
                (log) =>
                    String(log.message || '').startsWith('[lifecycle] retry_decision') &&
                    /scheduled=true/.test(String(log.message || '')),
            );
        },
        15000,
        500,
        `${label} sigkill auto-retry decision`,
    );

    await pollUntil(
        async () => {
            const logs = await fetchOutputHistory(target, {
                since: failureSince,
                filter: 'lifecycle',
                order: 'asc',
                limit: 200,
            });
            return logs.some(
                (log) =>
                    String(log.message || '').startsWith('[lifecycle] started') &&
                    /trigger=auto-retry/.test(String(log.message || '')),
            );
        },
        45000,
        1000,
        `${label} sigkill auto-retry restart`,
    );

    await waitForActive(resolved);
}

async function verifyInputRecoveryRestart(pipelineTarget, pipelineOutputs, inputPublishers) {
    if (!pipelineTarget) {
        throw new Error('No pipeline target available for input recovery verification');
    }
    if (!Array.isArray(pipelineOutputs) || pipelineOutputs.length === 0) {
        throw new Error('No output targets available for input recovery verification');
    }

    const publisherTarget = (inputPublishers || []).find(
        (item) => item.streamKey === pipelineTarget.streamKey,
    );
    if (!publisherTarget) {
        throw new Error(
            `No input publisher target found for pipeline ${pipelineTarget.name || pipelineTarget.pipelineId}`,
        );
    }

    const targetOutput = pipelineOutputs[0];
    const label = `${targetOutput.pipelineName}/${targetOutput.outputName}`;
    const failureSince = new Date().toISOString();

    console.log(
        `Stopping input publisher for ${pipelineTarget.name || pipelineTarget.pipelineId} to verify input-recovery restart`,
    );
    await stopInputPublisher(publisherTarget);

    await pollUntil(
        async () => {
            const health = (await requestJson('/health')).json;
            return health.pipelines?.[pipelineTarget.pipelineId]?.input?.status !== 'on';
        },
        30000,
        500,
        `${pipelineTarget.name || pipelineTarget.pipelineId} input to leave on-state`,
    );

    await pollUntil(
        async () => {
            const state = await fetchConfigState();
            const output = state.outputs.find(
                (item) =>
                    item.pipelineId === targetOutput.pipelineId && item.id === targetOutput.outputId,
            );
            const latestJob = findLatestJobForOutput(
                state,
                targetOutput.pipelineId,
                targetOutput.outputId,
            );
            return output?.desiredState === 'running' && latestJob && latestJob.status !== 'running';
        },
        45000,
        500,
        `${label} to stop while preserving desiredState=running`,
    );

    await restartInputPublisher(publisherTarget);

    await pollUntil(
        async () => {
            const logs = await fetchOutputHistory(targetOutput, {
                since: failureSince,
                filter: 'lifecycle',
                order: 'asc',
                limit: 200,
            });
            return logs.some(
                (log) =>
                    String(log.message || '').startsWith('[lifecycle] started') &&
                    /trigger=input-recovery/.test(String(log.message || '')),
            );
        },
        90000,
        1000,
        `${label} input-recovery restart`,
    );

    await waitForActive({
        pipelines: [pipelineTarget],
        outputs: pipelineOutputs,
    });
}

function selectLoopbackTargetPipeline(resolved) {
    const pipelines = resolved?.pipelines || [];
    if (pipelines.length < 2) {
        throw new Error('SRT loopback stage requires at least two pipelines in the 4x3 manifest');
    }
    return pipelines[1];
}

function selectLoopbackSourceOutput(resolved, targetPipelineId) {
    const outputs = resolved?.outputs || [];
    const candidates = outputs.filter((output) => output.pipelineId !== targetPipelineId);
    if (candidates.length === 0) {
        throw new Error(
            'No source output available outside the selected SRT loopback target pipeline',
        );
    }

    return candidates[0];
}

async function fetchConfigPipelineById(pipelineId) {
    const snapshot = (await requestJson('/config')).json || {};
    const pipelines = Array.isArray(snapshot.pipelines) ? snapshot.pipelines : [];
    return pipelines.find((pipeline) => pipeline.id === pipelineId) || null;
}

function resolveLoopbackSrtUrlFromPayload(ingestSrtUrl, streamKey) {
    let parsed;
    try {
        parsed = new URL(ingestSrtUrl);
    } catch {
        throw new Error(`Target SRT ingest URL is invalid: ${String(ingestSrtUrl || '')}`);
    }

    if (parsed.protocol !== 'srt:') {
        throw new Error(`Target ingest URL is not SRT: ${ingestSrtUrl}`);
    }

    const streamIdRaw = parsed.searchParams.get('streamid') || '';
    const streamIdDecoded = decodeURIComponent(streamIdRaw);
    const expectedStreamId = `publish:live/${streamKey}`;
    if (streamIdDecoded !== expectedStreamId) {
        throw new Error(
            `Target SRT ingest URL streamid mismatch: expected ${expectedStreamId}, got ${streamIdDecoded || 'missing'}`,
        );
    }

    // Preserve payload formatting exactly to avoid introducing URI re-encoding differences.
    return String(ingestSrtUrl);
}

async function stopOutputForMutation(target) {
    await requestJson(`/pipelines/${target.pipelineId}/outputs/${target.outputId}/stop`, {
        method: 'POST',
        okStatuses: [200],
    });
}

async function updateOutputUrl(target, outputUrl) {
    await requestJson(`/pipelines/${target.pipelineId}/outputs/${target.outputId}`, {
        method: 'POST',
        body: { url: outputUrl },
        okStatuses: [200],
    });
    target.outputUrl = outputUrl;
}

async function startOutputWithRetry(target) {
    for (let attempt = 1; attempt <= config.maxRetries; attempt += 1) {
        const result = await requestJson(
            `/pipelines/${target.pipelineId}/outputs/${target.outputId}/start`,
            {
                method: 'POST',
                okStatuses: [200, 201, 409],
            },
        );
        const message = result.json?.error || result.text || '';

        if (result.status === 200 || result.status === 201) {
            return;
        }

        if (result.status === 409 && message.includes('already has a running job')) {
            return;
        }

        if (result.status === 409 && message.includes('input is not available yet')) {
            if (attempt < config.maxRetries) {
                await sleep(config.retryDelaySec * 1000);
                continue;
            }
            throw new Error(
                `Timed out waiting to start ${target.pipelineName}/${target.outputName}: ${message}`,
            );
        }

        throw new Error(
            `Failed to start ${target.pipelineName}/${target.outputName}: ${message || `HTTP ${result.status}`}`,
        );
    }
}

function getHealthLoopbackSummary(health, sourceOutput, targetPipeline) {
    const sourceHealth = health?.pipelines?.[sourceOutput.pipelineId]?.outputs?.[sourceOutput.outputId] || null;
    const targetInput = health?.pipelines?.[targetPipeline.pipelineId]?.input || null;
    return {
        sourceOutput: {
            status: sourceHealth?.status || 'missing',
            jobStatus: sourceHealth?.jobStatus || null,
            remoteAddr: sourceHealth?.remoteAddr || null,
        },
        targetInput: {
            status: targetInput?.status || 'missing',
            online: targetInput?.online ?? null,
            ready: targetInput?.ready ?? null,
            readers: targetInput?.readers ?? null,
        },
    };
}

async function waitForPipelineInputStatus(pipelineTarget, expectedStatus, timeoutMs, label) {
    await pollUntil(
        async () => {
            const health = (await requestJson('/health')).json;
            return health?.pipelines?.[pipelineTarget.pipelineId]?.input?.status === expectedStatus;
        },
        timeoutMs,
        500,
        label || `${pipelineTarget.name || pipelineTarget.pipelineId} input status=${expectedStatus}`,
    );
}

async function waitForPipelineInputNotOn(pipelineTarget, timeoutMs, label) {
    let lastStatus = 'missing';
    let lastOnline = null;
    let lastReady = null;
    let lastReaders = null;

    await pollUntil(
        async () => {
            const health = (await requestJson('/health')).json;
            const input = health?.pipelines?.[pipelineTarget.pipelineId]?.input || null;
            lastStatus = input?.status || 'missing';
            lastOnline = input?.online ?? null;
            lastReady = input?.ready ?? null;
            lastReaders = input?.readers ?? null;
            return lastStatus !== 'on';
        },
        timeoutMs,
        500,
        label || `${pipelineTarget.name || pipelineTarget.pipelineId} input to leave on-state`,
    );

    console.log(
        `[srt-loopback] target input transitioned off on-state: status=${lastStatus} online=${lastOnline} ready=${lastReady} readers=${lastReaders}`,
    );
}

async function waitForOutputStatus(outputTarget, expectedStatus, timeoutMs, label) {
    await pollUntil(
        async () => {
            const health = (await requestJson('/health')).json;
            return (
                health?.pipelines?.[outputTarget.pipelineId]?.outputs?.[outputTarget.outputId]?.status ===
                expectedStatus
            );
        },
        timeoutMs,
        500,
        label ||
            `${outputTarget.pipelineName}/${outputTarget.outputName} status=${expectedStatus}`,
    );
}

async function waitForOutputUrl(outputTarget, expectedUrl, timeoutMs, label) {
    await pollUntil(
        async () => {
            const state = await fetchConfigState();
            const output = state.outputs.find(
                (item) =>
                    item.pipelineId === outputTarget.pipelineId && item.id === outputTarget.outputId,
            );
            return output?.url === expectedUrl;
        },
        timeoutMs,
        500,
        label ||
            `${outputTarget.pipelineName}/${outputTarget.outputName} URL to match ${expectedUrl}`,
    );
}

async function verifySrtOutputLoopbackToPipelineInput(resolved, inputPublishers) {
    const targetPipeline = selectLoopbackTargetPipeline(resolved);
    const sourceOutput = selectLoopbackSourceOutput(resolved, targetPipeline.pipelineId);
    if (sourceOutput.pipelineId === targetPipeline.pipelineId) {
        throw new Error(
            `SRT loopback source output pipeline must differ from target pipeline: ${sourceOutput.pipelineId}`,
        );
    }
    const sourceOriginalUrl = sourceOutput.outputUrl;

    const targetConfigPipeline = await fetchConfigPipelineById(targetPipeline.pipelineId);
    const targetIngestSrtUrl = targetConfigPipeline?.ingestUrls?.srt;
    if (!targetIngestSrtUrl) {
        throw new Error(
            `Selected target pipeline is missing SRT ingest URL: ${targetPipeline.pipelineId}`,
        );
    }

    const loopbackSrtUrl = resolveLoopbackSrtUrlFromPayload(
        targetIngestSrtUrl,
        targetPipeline.streamKey,
    );

    console.log(
        `[srt-loopback] selection ${JSON.stringify({
            sourceOutput: {
                pipelineId: sourceOutput.pipelineId,
                outputId: sourceOutput.outputId,
                outputName: sourceOutput.outputName,
                originalUrl: sourceOriginalUrl,
            },
            targetPipeline: {
                pipelineId: targetPipeline.pipelineId,
                pipelineName: targetPipeline.name,
                streamKey: targetPipeline.streamKey,
                ingestSrtUrl: loopbackSrtUrl,
            },
        })}`,
    );

    const targetPublisher = (inputPublishers || []).find(
        (publisher) => publisher.streamKey === targetPipeline.streamKey,
    );
    if (!targetPublisher) {
        throw new Error(
            `No managed input publisher found for target pipeline ${targetPipeline.pipelineId}`,
        );
    }

    let targetPublisherStopped = false;
    let sourceMutated = false;
    const cleanupErrors = [];

    try {
        console.log(
            '[srt-loopback] 1/5 stop target external publisher and verify input leaves on-state',
        );
        const stopSummary = await stopAllInputPublishersForStreamKey(
            targetPipeline.streamKey,
            inputPublishers,
        );
        targetPublisherStopped = true;
        console.log(
            `[srt-loopback] stopped publishers for streamKey=${targetPipeline.streamKey} managed=${stopSummary.managedStopped} stale=${stopSummary.staleStopped}`,
        );

        await waitForPipelineInputNotOn(
            targetPipeline,
            SRT_LOOPBACK_TIMEOUT_SEC * 1000,
            `${targetPipeline.name || targetPipeline.pipelineId} input to leave on-state for loopback publish`,
        );

        console.log('[srt-loopback] 2/5 stop source output and verify output=off');
        await stopOutputForMutation(sourceOutput);
        await waitForOutputStatus(
            sourceOutput,
            'off',
            SRT_LOOPBACK_TIMEOUT_SEC * 1000,
            `${sourceOutput.pipelineName}/${sourceOutput.outputName} to stop before URL mutation`,
        );

        console.log('[srt-loopback] 3/5 repoint source output to target SRT ingest and start');
        await updateOutputUrl(sourceOutput, loopbackSrtUrl);
        await startOutputWithRetry(sourceOutput);
        sourceMutated = true;

        console.log('[srt-loopback] 4/5 verify target pipeline input=on');
        const timeoutMs = SRT_LOOPBACK_TIMEOUT_SEC * 1000;
        try {
            await waitForPipelineInputStatus(
                targetPipeline,
                'on',
                timeoutMs,
                `${targetPipeline.name || targetPipeline.pipelineId} input to become on via loopback`,
            );
        } catch (_error) {
            const health = (await requestJson('/health')).json;
            const summary = getHealthLoopbackSummary(health || {}, sourceOutput, targetPipeline);
            throw new Error(
                `Timed out waiting for SRT loopback activation (${SRT_LOOPBACK_TIMEOUT_SEC}s): ${JSON.stringify(summary)}`,
            );
        }

        console.log(
            `[srt-loopback] activation passed sourceOutput=${sourceOutput.pipelineId}/${sourceOutput.outputId} targetPipeline=${targetPipeline.pipelineId}`,
        );
        return;
    } finally {
        if (sourceMutated) {
            console.log('[srt-loopback] 5/5 restore source output URL and restart target publisher');
            try {
                await stopOutputForMutation(sourceOutput);
                await waitForOutputStatus(
                    sourceOutput,
                    'off',
                    SRT_LOOPBACK_TIMEOUT_SEC * 1000,
                    `${sourceOutput.pipelineName}/${sourceOutput.outputName} to stop before restoring URL`,
                );
            } catch (error) {
                cleanupErrors.push(`stop source output failed: ${String(error?.message || error)}`);
            }

            try {
                await updateOutputUrl(sourceOutput, sourceOriginalUrl);
                await waitForOutputUrl(
                    sourceOutput,
                    sourceOriginalUrl,
                    SRT_LOOPBACK_TIMEOUT_SEC * 1000,
                    `${sourceOutput.pipelineName}/${sourceOutput.outputName} URL to restore`,
                );
            } catch (error) {
                cleanupErrors.push(
                    `restore source output URL failed: ${String(error?.message || error)}`,
                );
            }

            try {
                await startOutputWithRetry(sourceOutput);
                await waitForOutputStatus(
                    sourceOutput,
                    'on',
                    SRT_LOOPBACK_TIMEOUT_SEC * 1000,
                    `${sourceOutput.pipelineName}/${sourceOutput.outputName} to return on-state after restore`,
                );
            } catch (error) {
                cleanupErrors.push(
                    `restart source output on original URL failed: ${String(error?.message || error)}`,
                );
            }
        }

        if (targetPublisherStopped && sourceMutated) {
            try {
                await restartInputPublisher(targetPublisher);
            } catch (error) {
                cleanupErrors.push(
                    `restart target input publisher failed: ${String(error?.message || error)}`,
                );
            }
        }

        if (sourceMutated) {
            try {
                await waitForActive(resolved);
            } catch (error) {
                cleanupErrors.push(`post-loopback stabilization failed: ${String(error?.message || error)}`);
            }
        }

        if (cleanupErrors.length > 0) {
            throw new Error(`SRT loopback cleanup failed: ${cleanupErrors.join(' | ')}`);
        }
    }
}

async function waitForActive(resolved) {
    const expectedInputs = resolved.pipelines.length;
    const expectedOutputs = resolved.outputs.length;
    const deadline = Date.now() + config.timeoutSec * 1000;

    console.log(
        `Waiting for all streams green (inputs=${expectedInputs} outputs=${expectedOutputs})`,
    );

    while (Date.now() <= deadline) {
        let health;
        try {
            health = (await requestJson('/health')).json;
        } catch {
            await sleep(config.pollSec * 1000);
            continue;
        }

        const inputOn = resolved.pipelines.filter(
            (target) => health.pipelines?.[target.pipelineId]?.input?.status === 'on',
        ).length;
        const inputWarning = resolved.pipelines.filter(
            (target) => health.pipelines?.[target.pipelineId]?.input?.status === 'warning',
        ).length;
        const outputOn = resolved.outputs.filter(
            (target) =>
                health.pipelines?.[target.pipelineId]?.outputs?.[target.outputId]?.status === 'on',
        ).length;
        const outputWarning = resolved.outputs.filter(
            (target) =>
                health.pipelines?.[target.pipelineId]?.outputs?.[target.outputId]?.status ===
                'warning',
        ).length;
        const outputActive = outputOn + outputWarning;

        console.log(
            `Status now: inputs on=${inputOn}/${expectedInputs} warning=${inputWarning} | outputs on=${outputOn}/${expectedOutputs} warning=${outputWarning} active=${outputActive}/${expectedOutputs}`,
        );

        if (inputOn === expectedInputs && outputOn === expectedOutputs) {
            console.log('All expected inputs and outputs are green (on)');
            return;
        }

        await sleep(config.pollSec * 1000);
    }

    const health = (await requestJson('/health')).json;
    console.log('Timed out waiting for all manifest streams to become green');
    console.log('---- Input status summary ----');
    for (const target of resolved.pipelines) {
        const input = health.pipelines?.[target.pipelineId]?.input;
        console.log(
            `${target.pipelineId} input=${input?.status || 'missing'} online=${input?.online ?? 'null'} ready=${input?.ready ?? 'null'} readers=${input?.readers ?? 'null'}`,
        );
    }
    console.log('---- Output mismatch details (non-on only) ----');
    for (const target of resolved.outputs) {
        const output = health.pipelines?.[target.pipelineId]?.outputs?.[target.outputId];
        if (output?.status === 'on') {
            continue;
        }
        console.log(
            `${target.pipelineId}/${target.outputId} status=${output?.status || 'missing'} jobStatus=${output?.jobStatus || 'null'} jobId=${output?.jobId || 'null'} bytesIn=${output?.bytesReceived || 0} bytesOut=${output?.bytesSent || 0} remote=${output?.remoteAddr || 'null'}`,
        );
    }
    throw new Error('Timed out waiting for all manifest streams to become green');
}

async function verifyRtmpStat(resolved) {
    const fetchRtmpStatXml = async () => {
        try {
            return await requestText(config.rtmpStatUrl, { timeoutMs: 8000 });
        } catch {
            return '';
        }
    };

    const summarizeRtmpStatXml = (xml, expectedOutputs) => ({
        expectedOutputs,
        streamBlocks: (xml.match(/<stream>/g) || []).length,
        streamMetaVideo: (xml.match(/<meta>[\s\S]*?<video>/g) || []).length,
        streamMetaAudio: (xml.match(/<meta>[\s\S]*?<audio>/g) || []).length,
    });

    const escapeRegExp = (value) => String(value).replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

    const findStreamBlock = (xml, streamName) => {
        const escaped = escapeRegExp(streamName);
        const re = new RegExp(
            `<stream>[\\s\\S]*?<name>${escaped}<\\/name>[\\s\\S]*?<\\/stream>`,
            'i',
        );
        const match = xml.match(re);
        return match ? match[0] : null;
    };

    const hasMetaTrack = (streamBlock, track) => {
        const escapedTrack = escapeRegExp(track);
        const re = new RegExp(
            `<meta>[\\s\\S]*?<${escapedTrack}>[\\s\\S]*?<\\/${escapedTrack}>[\\s\\S]*?<\\/meta>`,
        );
        return re.test(streamBlock);
    };

    const writeNginxStatSummary = async (statSummary) => {
        await mkdir(config.outDir, { recursive: true });
        const statOutFile = path.join(config.outDir, `nginx-stat-summary-${timestampUtc()}.json`);
        await writeFile(statOutFile, `${JSON.stringify(statSummary, null, 2)}\n`, 'utf8');
        console.log(`Saved nginx stat summary: ${relativePath(statOutFile)}`);
    };

    const expected = resolved.outputs.map((output) => ({
        streamName: extractStreamName(output.outputUrl),
        pipelineId: output.pipelineId,
        outputId: output.outputId,
    }));

    let lastIssues = [];
    let lastStreamBlocks = 0;
    let bestStatSummary = {
        expectedOutputs: expected.length,
        streamBlocks: 0,
        streamMetaVideo: 0,
        streamMetaAudio: 0,
    };

    for (let attempt = 1; attempt <= 10; attempt += 1) {
        const xml = await fetchRtmpStatXml();

        if (!xml.includes('<rtmp>')) {
            await sleep(1000);
            continue;
        }

        const issues = [];
        const summary = summarizeRtmpStatXml(xml, expected.length);
        const { streamBlocks } = summary;

        bestStatSummary = {
            expectedOutputs: expected.length,
            streamBlocks: Math.max(bestStatSummary.streamBlocks, summary.streamBlocks),
            streamMetaVideo: Math.max(bestStatSummary.streamMetaVideo, summary.streamMetaVideo),
            streamMetaAudio: Math.max(bestStatSummary.streamMetaAudio, summary.streamMetaAudio),
        };

        if (streamBlocks < expected.length) {
            lastStreamBlocks = streamBlocks;
            lastIssues = [
                `stream blocks in /stat (${streamBlocks}) are less than expected outputs (${expected.length})`,
            ];
            console.log(
                `nginx /stat correlation (attempt ${attempt}/10): expected_streams=${expected.length} stream_blocks=${streamBlocks} issues=1`,
            );
            await sleep(1000);
            continue;
        }

        for (const target of expected) {
            if (!target.streamName) {
                issues.push(
                    `${target.pipelineId}/${target.outputId}: could not derive stream name from output URL`,
                );
                continue;
            }

            const block = findStreamBlock(xml, target.streamName);
            if (!block) {
                issues.push(
                    `${target.pipelineId}/${target.outputId} (${target.streamName}): missing from nginx /stat`,
                );
                continue;
            }

            const hasVideoMeta = hasMetaTrack(block, 'video');
            const hasAudioMeta = hasMetaTrack(block, 'audio');

            if (!hasVideoMeta) {
                issues.push(
                    `${target.pipelineId}/${target.outputId} (${target.streamName}): missing video meta in /stat`,
                );
            }
            if (!hasAudioMeta) {
                issues.push(
                    `${target.pipelineId}/${target.outputId} (${target.streamName}): missing audio meta in /stat`,
                );
            }
        }

        lastIssues = issues;
        lastStreamBlocks = streamBlocks;

        console.log(
            `nginx /stat correlation (attempt ${attempt}/10): expected_streams=${expected.length} stream_blocks=${streamBlocks} issues=${issues.length}`,
        );

        if (issues.length === 0) {
            await writeNginxStatSummary(summary);
            console.log(
                'nginx /stat correlation passed: expected stream blocks present and each output has video+audio meta',
            );
            return;
        }

        await sleep(1000);
    }

    console.log(
        `nginx /stat correlation: expected_streams=${expected.length} stream_blocks=${lastStreamBlocks}`,
    );
    console.log('---- nginx /stat issues ----');
    for (const issue of lastIssues) {
        console.log(issue);
    }
    await writeNginxStatSummary(bestStatSummary);
    throw new Error(`nginx /stat correlation failed with ${lastIssues.length} issue(s)`);
}

async function captureHealthSnapshot(resolved) {
    await mkdir(config.outDir, { recursive: true });
    let health = null;

    for (let attempt = 1; attempt <= 10; attempt += 1) {
        try {
            health = (await requestJson('/health')).json;
            break;
        } catch {
            await sleep(1000);
        }
    }

    if (!health) {
        throw new Error(`Failed to fetch ${config.apiUrl}/health after retries`);
    }

    const outFile = path.join(config.outDir, `health-${timestampUtc()}.json`);
    await writeFile(outFile, `${JSON.stringify(health, null, 2)}\n`, 'utf8');

    const inputOn = Object.values(health.pipelines || {}).filter(
        (pipeline) => pipeline.input?.status === 'on',
    ).length;
    const outputOn = Object.values(health.pipelines || {})
        .flatMap((pipeline) => Object.values(pipeline.outputs || {}))
        .filter((output) => output.status === 'on').length;

    console.log(`Saved health snapshot: ${relativePath(outFile)}`);
    console.log(`Input ON count: ${inputOn}`);
    console.log(`Output ON count: ${outputOn}`);
}

async function requestJson(route, options = {}) {
    const url = route.startsWith('http') ? route : `${config.apiUrl}${route}`;
    const method = options.method || 'GET';
    const headers = { ...(options.headers || {}) };
    let body = options.body;

    if (body != null && typeof body !== 'string') {
        body = JSON.stringify(body);
        headers['Content-Type'] = 'application/json';
    }

    const response = await fetch(url, {
        method,
        headers,
        body,
        signal: AbortSignal.timeout(options.timeoutMs || 8000),
    });

    const text = await response.text();
    let json = null;
    if (text) {
        try {
            json = JSON.parse(text);
        } catch {
            json = null;
        }
    }

    const okStatuses = options.okStatuses || [200];
    if (!okStatuses.includes(response.status)) {
        const details = json?.error || text || `HTTP ${response.status}`;
        throw new Error(`${method} ${url} failed: ${details}`);
    }

    return { status: response.status, text, json };
}

async function pollUntil(checkFn, timeoutMs, intervalMs, label) {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() <= deadline) {
        if (await checkFn()) return;
        await sleep(intervalMs);
    }
    throw new Error(`Timed out waiting for ${label}`);
}

async function requestText(url, options = {}) {
    const response = await fetch(url, {
        method: 'GET',
        signal: AbortSignal.timeout(options.timeoutMs || 8000),
    });

    if (!response.ok) {
        throw new Error(`GET ${url} failed: HTTP ${response.status}`);
    }

    return response.text();
}

function extractStreamName(outputUrl) {
    if (!outputUrl || typeof outputUrl !== 'string') {
        return null;
    }

    try {
        const parsed = new URL(outputUrl);
        const segments = parsed.pathname.split('/').filter(Boolean);
        return segments.length > 0 ? segments[segments.length - 1] : null;
    } catch {
        const parts = outputUrl.split('/').filter(Boolean);
        return parts.length > 0 ? parts[parts.length - 1] : null;
    }
}

function spawnDetachedProcess({ name, command, args, logPath }) {
    const logFd = openSync(logPath, 'a');
    const child = spawn(command, args, {
        cwd: rootDir,
        env: process.env,
        detached: true,
        stdio: ['ignore', logFd, logFd],
    });

    closeSync(logFd);

    child.unref();
    ownedProcesses.push({ name, pid: child.pid });
    return child.pid;
}

async function terminateProcess(proc) {
    if (!proc?.pid) {
        return;
    }

    try {
        process.kill(proc.pid, 'SIGTERM');
    } catch {
        return;
    }

    for (let attempt = 0; attempt < 10; attempt += 1) {
        if (!isProcessAlive(proc.pid)) {
            return;
        }
        await sleep(200);
    }

    try {
        process.kill(proc.pid, 'SIGKILL');
    } catch {
        // Process already exited.
    }
}

function isProcessAlive(pid) {
    try {
        process.kill(pid, 0);
        return true;
    } catch {
        return false;
    }
}

async function runCommand(command, args, options = {}) {
    await new Promise((resolve, reject) => {
        const child = spawn(command, args, {
            cwd: rootDir,
            env: process.env,
            stdio: options.stdio || 'pipe',
        });

        let stderr = '';
        if (child.stderr) {
            child.stderr.on('data', (chunk) => {
                stderr += chunk.toString();
            });
        }

        child.on('error', reject);
        child.on('close', (code) => {
            if (code === 0 || options.allowFailure) {
                resolve();
                return;
            }
            reject(
                new Error(
                    `${command} ${args.join(' ')} failed${stderr ? `: ${stderr.trim()}` : ''}`,
                ),
            );
        });
    });
}

async function safeReadFile(filePath) {
    try {
        return await readFile(filePath, 'utf8');
    } catch {
        return '';
    }
}

function tailText(text, lineCount) {
    return text.split('\n').slice(-lineCount).join('\n');
}

function timestampUtc() {
    return new Date()
        .toISOString()
        .replace(/[-:]/g, '')
        .replace(/\.\d{3}Z$/, 'Z');
}

function sleep(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
}
