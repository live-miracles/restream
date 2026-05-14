#!/usr/bin/env node
import { spawn } from 'node:child_process';
import { closeSync, openSync } from 'node:fs';
import { access, mkdir, readFile } from 'node:fs/promises';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath, pathToFileURL } from 'node:url';

const rootDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
process.chdir(rootDir);

const defaults = {
    apiUrl: 'http://localhost:3030',
    manifestPath: 'test/artifacts/session-2x3-manifest.json',
    logDir: 'test/artifacts/logs',
    appLogPath: 'test/artifacts/logs/app-under-test.log',
    verifyAppRetries: 30,
    inputFile: 'media/colorbar-timer.mp4',
    rtmpOutputBase: '',
    inputProtocols: 'rtmp,srt',
    maxRetries: 30,
    retryDelaySec: 1,
    timeoutSec: 120,
    pollSec: 2,
};

const config = {
    apiUrl: process.env.API_URL || defaults.apiUrl,
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
    keepRunning: readBooleanEnv('KEEP_RUNNING', false),
};

const outputEncodingDefaults = ['source', 'vertical-crop', 'vertical-rotate', '720p', '1080p'];
const supportedOutputEncodings = new Set(outputEncodingDefaults);
const nonSourceOutputEncodings = outputEncodingDefaults.filter((e) => e !== 'source');
const outputEncodingUsage = new Map(outputEncodingDefaults.map((e) => [e, 0]));

const ownedProcesses = [];
let shutdownPromise = null;
let cleanupTargets = createCleanupTargets();

if (isDirectExecution()) {
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
        try {
            await shutdown(config.keepRunning);
        } catch (error) {
            console.error(error instanceof Error ? error.message : String(error));
            process.exitCode = 1;
        }
    }
}

async function main() {
    cleanupTargets = createCleanupTargets();
    const manifest = await loadManifest(config.manifestPath);

    console.log('== Verify 2x3 prerequisites ==');
    await ensureRunnerPrerequisites();

    console.log('== Verify app is running (run "make run-host" or "make run-docker" first) ==');
    await ensureApiReachable();

    console.log('== Step 1: Ensure 2x3 manifest resources ==');
    const resolved = await ensureResources(manifest, cleanupTargets);

    const newlyCreatedOutputs = resolved.outputs.filter((target) => target.wasCreated);
    if (newlyCreatedOutputs.length > 0) {
        console.log('== Step 1b: Verify newly created outputs default to desiredState=stopped ==');
        await verifyDesiredStateForOutputs(newlyCreatedOutputs, 'stopped');
    }

    console.log('== Step 2: Start RTMP/SRT input publishers ==');
    await startInputPublishers(manifest);

    console.log('== Step 3: Start all outputs ==');
    await startOutputs(resolved.outputs);

    console.log('== Step 3b: Verify started outputs persist desiredState=running ==');
    await verifyDesiredStateForOutputs(resolved.outputs, 'running');

    console.log('== Step 4: Wait for all inputs/outputs active ==');
    await waitForActive(resolved);

    console.log('== Step 5: Stop all outputs ==');
    await stopOutputs(resolved.outputs);

    console.log('== 2x3 run complete ==');
}

function resolvePath(targetPath) {
    return path.isAbsolute(targetPath) ? targetPath : path.resolve(rootDir, targetPath);
}

function relativePath(targetPath) {
    return path.relative(rootDir, targetPath) || '.';
}

function createCleanupTargets() {
    return {
        streamKeys: [],
        pipelines: [],
        outputs: [],
    };
}

function trackUnique(targets, item, keyFn) {
    const key = keyFn(item);
    const index = targets.findIndex((existing) => keyFn(existing) === key);
    if (index >= 0) {
        targets[index] = { ...targets[index], ...item };
        return targets[index];
    }
    targets.push(item);
    return item;
}

function readBooleanEnv(name, defaultValue) {
    const value = process.env[name];
    if (value == null || value === '') return defaultValue;
    return !['0', 'false', 'no', 'off'].includes(String(value).toLowerCase());
}

function printHelp() {
    console.log(
        `Usage: node test/artifacts/run-2x3.mjs\n\nEnvironment flags:\n  KEEP_RUNNING=1    Leave input publishers and manifest-scoped test resources in place after the run\n  MANIFEST_PATH     Path to the tracked 2x3 manifest\n  API_URL           Backend base URL (default: ${defaults.apiUrl})\n  RTMP_OUTPUT_BASE  Base URL used to normalize RTMP output URLs (if set)\n  INPUT_PROTOCOLS   Comma-separated input protocols to use (default: ${defaults.inputProtocols})`,
    );
}

function registerSignalHandlers() {
    for (const signal of ['SIGINT', 'SIGTERM']) {
        process.on(signal, () => {
            void shutdown(config.keepRunning)
                .catch((error) => {
                    console.error(error instanceof Error ? error.message : String(error));
                    process.exitCode = 1;
                })
                .finally(() => {
                    process.exit(signal === 'SIGINT' ? 130 : 143);
                });
        });
    }
}

async function shutdown(leaveRunning) {
    if (shutdownPromise) return shutdownPromise;

    shutdownPromise = (async () => {
        if (leaveRunning) {
            console.log(
                '== KEEP_RUNNING=1: leaving input publishers and manifest-scoped test resources in place ==',
            );
            return;
        }

        for (const proc of ownedProcesses.reverse()) {
            await terminateProcess(proc);
        }

        await cleanupTestResources(cleanupTargets);
    })();

    return shutdownPromise;
}

async function ensureApiReachable() {
    try {
        const response = await fetch(`${config.apiUrl}/healthz`, {
            signal: AbortSignal.timeout(5000),
        });
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
    } catch (error) {
        throw new Error(
            `API not reachable at ${config.apiUrl}/healthz. Start the app first (make run-host or make run-docker). ${String(error)}`,
        );
    }
}

async function ensureRunnerPrerequisites() {
    await assertCommandAvailable(
        'ffmpeg',
        ['-version'],
        'Install ffmpeg before running the 2x3 suite.',
    );
}

async function assertCommandAvailable(command, args, helpText) {
    try {
        await runCommand(command, args, { stdio: 'ignore' });
    } catch (error) {
        throw new Error(
            `${helpText} Failed prerequisite check: ${command} ${args.join(' ')}. ${String(error?.message || error)}`,
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

async function ensureResources(manifest, trackedTargets = createCleanupTargets()) {
    const pipelineTargets = trackedTargets.pipelines;
    const outputTargets = trackedTargets.outputs;
    let state = await fetchConfigState();

    for (const pipelineDef of manifest.pipelines) {
        let pipeline = state.pipelines.find(
            (item) => item.name === pipelineDef.name && item.streamKey === pipelineDef.streamKey,
        );
        let pipelineWasCreated = false;

        if (!pipeline) {
            const result = await requestJson('/pipelines', {
                method: 'POST',
                body: { name: pipelineDef.name, streamKey: pipelineDef.streamKey },
                okStatuses: [201],
            });
            pipeline = result.json.pipeline;
            console.log(`Created pipeline ${pipelineDef.name}: ${pipeline.id}`);
            state = await fetchConfigState();
            pipelineWasCreated = true;
        } else {
            console.log(`Pipeline exists ${pipelineDef.name}: ${pipeline.id}`);
        }

        trackUnique(
            pipelineTargets,
            {
                name: pipelineDef.name,
                streamKey: pipelineDef.streamKey,
                pipelineId: pipeline.id,
                wasCreated: pipelineWasCreated,
            },
            (item) => item.pipelineId,
        );

        for (const outputDef of pipelineDef.outputs) {
            const outputUrl = normalizeOutputUrl(outputDef.url);
            const encoding = resolveOutputEncoding(outputDef.encoding);
            let wasCreated = false;
            let output = state.outputs.find(
                (item) =>
                    item.pipelineId === pipeline.id &&
                    item.name === outputDef.name &&
                    item.url === outputUrl &&
                    normalizeOutputEncodingValue(item.encoding) === encoding,
            );

            if (!output) {
                const conflict = state.outputs.find(
                    (item) => item.pipelineId === pipeline.id && item.name === outputDef.name,
                );
                if (conflict) {
                    throw new Error(
                        `Refusing to mutate existing output ${pipelineDef.name}/${outputDef.name}. Expected url=${outputUrl} encoding=${encoding}, found url=${conflict.url} encoding=${normalizeOutputEncodingValue(conflict.encoding)}. Remove the conflicting output or run against a clean test stack.`,
                    );
                }

                const result = await requestJson(`/pipelines/${pipeline.id}/outputs`, {
                    method: 'POST',
                    body: { name: outputDef.name, url: outputUrl, encoding },
                    okStatuses: [201],
                });
                output = result.json.output;
                wasCreated = true;
                console.log(`  Created output ${outputDef.name}: ${output.id}`);
                state = await fetchConfigState();
            } else {
                console.log(`  Output exists ${outputDef.name}: ${output.id}`);
            }

            trackUnique(
                outputTargets,
                {
                    pipelineId: pipeline.id,
                    pipelineName: pipelineDef.name,
                    outputId: output.id,
                    outputName: outputDef.name,
                    outputUrl,
                    wasCreated,
                },
                (item) => `${item.pipelineId}:${item.outputId}`,
            );
        }
    }

    console.log(`Manifest used: ${relativePath(config.manifestPath)}`);
    console.log(`Pipelines: ${pipelineTargets.length}`);
    console.log(`Outputs: ${outputTargets.length}`);

    return trackedTargets;
}

async function cleanupTestResources(targets) {
    const pipelineTargets = Array.isArray(targets?.pipelines)
        ? targets.pipelines.filter((t) => t?.wasCreated)
        : [];
    const outputTargets = Array.isArray(targets?.outputs)
        ? targets.outputs.filter((t) => t?.wasCreated)
        : [];

    if (pipelineTargets.length === 0 && outputTargets.length === 0) return;

    console.log('== Cleanup test resources ==');

    const cleanupErrors = [];
    let state = await fetchConfigState();

    for (const target of [...outputTargets].reverse()) {
        const output = state.outputs.find(
            (item) => item.pipelineId === target.pipelineId && item.id === target.outputId,
        );
        if (!output) continue;

        try {
            await requestJson(`/pipelines/${target.pipelineId}/outputs/${target.outputId}`, {
                method: 'DELETE',
                okStatuses: [200],
            });
            console.log(
                `Deleted output ${target.pipelineName}/${target.outputName} ${target.outputId}`,
            );
            state = await fetchConfigState();
        } catch (error) {
            cleanupErrors.push(
                `delete output ${target.pipelineId}/${target.outputId}: ${String(error?.message || error)}`,
            );
        }
    }

    const manifestOutputIdsByPipeline = new Map();
    for (const target of outputTargets) {
        const ids = manifestOutputIdsByPipeline.get(target.pipelineId) || new Set();
        ids.add(target.outputId);
        manifestOutputIdsByPipeline.set(target.pipelineId, ids);
    }

    for (const target of [...pipelineTargets].reverse()) {
        const pipeline = state.pipelines.find((item) => item.id === target.pipelineId);
        if (!pipeline) continue;

        const remainingOutputs = state.outputs.filter(
            (item) => item.pipelineId === target.pipelineId,
        );
        const manifestIds = manifestOutputIdsByPipeline.get(target.pipelineId) || new Set();
        const nonManifestOutputs = remainingOutputs.filter((item) => !manifestIds.has(item.id));

        if (nonManifestOutputs.length > 0) {
            console.log(
                `Skipping pipeline cleanup for ${target.name} ${target.pipelineId}; non-test outputs remain: ${nonManifestOutputs.map((item) => item.id).join(', ')}`,
            );
            continue;
        }

        try {
            await requestJson(`/pipelines/${target.pipelineId}`, {
                method: 'DELETE',
                okStatuses: [200],
            });
            console.log(`Deleted pipeline ${target.name} ${target.pipelineId}`);
            state = await fetchConfigState();
        } catch (error) {
            cleanupErrors.push(
                `delete pipeline ${target.pipelineId}: ${String(error?.message || error)}`,
            );
        }
    }

    cleanupTargets = createCleanupTargets();

    if (cleanupErrors.length > 0) {
        throw new Error(`Test resource cleanup failed: ${cleanupErrors.join(' | ')}`);
    }
}

function resolveOutputEncoding(encodingValue) {
    const normalized = normalizeOutputEncodingValue(encodingValue);
    if (normalized) {
        if (!supportedOutputEncodings.has(normalized)) {
            throw new Error(`Unsupported output encoding in manifest: ${encodingValue}`);
        }
        outputEncodingUsage.set(normalized, (outputEncodingUsage.get(normalized) || 0) + 1);
        return normalized;
    }

    const selectedFallback =
        nonSourceOutputEncodings.find((e) => (outputEncodingUsage.get(e) || 0) < 1) || 'source';
    outputEncodingUsage.set(selectedFallback, (outputEncodingUsage.get(selectedFallback) || 0) + 1);
    return selectedFallback;
}

function normalizeOutputEncodingValue(encodingValue) {
    return String(encodingValue || '')
        .trim()
        .toLowerCase();
}

async function fetchConfigState() {
    const result = await requestJson('/config');
    const pipelines = Array.isArray(result.json?.pipelines) ? result.json.pipelines : [];
    const streamKeys = pipelines.map((p) => ({
        key: p.streamKey,
        label: p.name,
        ingestUrls: p.ingestUrls,
    }));
    return {
        streamKeys,
        pipelines,
        outputs: Array.isArray(result.json?.outputs) ? result.json.outputs : [],
        jobs: Array.isArray(result.json?.jobs) ? result.json.jobs : [],
    };
}

async function startInputPublishers(manifest) {
    await access(config.inputFile);
    await mkdir(config.logDir, { recursive: true });

    const state = await fetchConfigState();
    const streamKeysByKey = new Map((state.streamKeys || []).map((sk) => [sk.key, sk]));

    const protocols = config.inputProtocols
        .split(',')
        .map((v) => v.trim().toLowerCase())
        .filter(Boolean);

    if (protocols.length === 0) throw new Error('No input protocols configured');

    const streamKeys = manifest.pipelines.map((p) => p.streamKey);
    if (streamKeys.length === 0) {
        throw new Error(`No stream keys found in manifest: ${relativePath(config.manifestPath)}`);
    }

    for (const [index, streamKey] of streamKeys.entries()) {
        const ordinal = index + 1;
        const protocol = protocols[index % protocols.length];
        const streamKeyRecord = streamKeysByKey.get(streamKey) || null;
        const targetUrl = selectIngestUrl(streamKeyRecord, protocol);
        const logPath = path.join(config.logDir, `input-${ordinal}-${protocol}.log`);
        const target = {
            ordinal,
            pipelineName: manifest.pipelines[index]?.name || `Pipeline ${ordinal}`,
            streamKey,
            protocol,
            targetUrl,
            logPath,
            pid: null,
        };
        const pid = spawnInputPublisher(target);
        console.log(
            `[${ordinal}/${streamKeys.length}] protocol=${protocol} streamKey=${streamKey} target=${targetUrl} pid=${pid} log=${relativePath(logPath)}`,
        );
    }
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

function selectIngestUrl(streamKeyRecord, protocol) {
    const ingestUrls = streamKeyRecord?.ingestUrls || {};
    if (protocol === 'rtmp' && ingestUrls.rtmp) return ingestUrls.rtmp;
    if (protocol === 'srt' && ingestUrls.srt) return ingestUrls.srt;
    throw new Error(
        `Missing ingest URL for protocol=${protocol} streamKey=${streamKeyRecord?.key || 'unknown'}`,
    );
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
    if (protocol === 'rtmp') return [...baseArgs, '-f', 'flv', targetUrl];
    if (protocol === 'srt') return [...baseArgs, '-f', 'mpegts', targetUrl];
    throw new Error(`Unsupported input protocol: ${protocol}`);
}

function isHlsPlaylistUrl(parsedUrl) {
    if (!(parsedUrl instanceof URL)) return false;
    const protocol = String(parsedUrl.protocol || '').toLowerCase();
    if (protocol !== 'http:' && protocol !== 'https:') return false;
    if (/\.m3u8$/i.test(parsedUrl.pathname || '')) return true;
    for (const value of parsedUrl.searchParams.values()) {
        if (/\.m3u8$/i.test(String(value || '').trim())) return true;
    }
    return false;
}

function getOutputProtocol(outputUrl) {
    if (!outputUrl || typeof outputUrl !== 'string') return null;
    try {
        const parsed = new URL(outputUrl);
        if (parsed.protocol === 'rtmp:' || parsed.protocol === 'rtmps:') return 'rtmp';
        if (parsed.protocol === 'srt:') return 'srt';
        if (isHlsPlaylistUrl(parsed)) return 'hls';
        return null;
    } catch {
        return null;
    }
}

function extractHlsPlaylistName(outputUrl) {
    try {
        const parsedOutputUrl =
            outputUrl instanceof URL ? outputUrl : new URL(String(outputUrl || ''));
        if (!isHlsPlaylistUrl(parsedOutputUrl)) return null;
        const pathname = String(parsedOutputUrl.pathname || '');
        if (/\.m3u8$/i.test(pathname)) {
            const name = path.posix.basename(pathname);
            if (name) return name;
        }
        for (const value of parsedOutputUrl.searchParams.values()) {
            const normalized = String(value || '').trim();
            if (/\.m3u8$/i.test(normalized)) {
                const name = path.posix.basename(normalized);
                if (name) return name;
            }
        }
        return null;
    } catch {
        return null;
    }
}

function normalizeHlsOutputUrl(outputUrl, hlsOutputBase = '') {
    if (!outputUrl || typeof outputUrl !== 'string' || !hlsOutputBase) return outputUrl;
    try {
        const parsedOutputUrl = new URL(outputUrl);
        if (!isHlsPlaylistUrl(parsedOutputUrl)) return outputUrl;
        const parsedBaseUrl = new URL(hlsOutputBase);
        const normalized = new URL(parsedBaseUrl.toString());
        const playlistName = extractHlsPlaylistName(parsedOutputUrl) || 'out.m3u8';
        const basePath = String(parsedBaseUrl.pathname || '').replace(/\/+$/, '');
        normalized.pathname = `${basePath}/${playlistName}`.replace(/\/+/g, '/');
        normalized.search = parsedBaseUrl.search;
        normalized.hash = '';
        return normalized.toString();
    } catch {
        return outputUrl;
    }
}

function extractStreamName(outputUrl) {
    if (!outputUrl || typeof outputUrl !== 'string') return null;
    try {
        const parsed = new URL(outputUrl);
        const segments = parsed.pathname.split('/').filter(Boolean);
        return segments.length > 0 ? segments[segments.length - 1] : null;
    } catch {
        const parts = outputUrl.split('/').filter(Boolean);
        return parts.length > 0 ? parts[parts.length - 1] : null;
    }
}

function normalizeOutputUrl(outputUrl) {
    if (!outputUrl || typeof outputUrl !== 'string') return outputUrl;
    if (getOutputProtocol(outputUrl) === 'hls') return normalizeHlsOutputUrl(outputUrl);
    if (!config.rtmpOutputBase) return outputUrl;
    try {
        const parsed = new URL(outputUrl);
        if (parsed.protocol !== 'rtmp:' && parsed.protocol !== 'rtmps:') return outputUrl;
    } catch {
        return outputUrl;
    }
    const streamName = extractStreamName(outputUrl);
    if (!streamName) return outputUrl;
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
                { method: 'POST', okStatuses: [200, 201, 409] },
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
            if (errorMessage) console.log(errorMessage);
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

async function stopOutputs(outputs) {
    for (const target of outputs) {
        const result = await requestJson(
            `/pipelines/${target.pipelineId}/outputs/${target.outputId}/stop`,
            { method: 'POST', okStatuses: [200] },
        );
        console.log(
            `Stopped ${target.pipelineName}/${target.outputName}: desiredState=${result.json?.desiredState}`,
        );
    }

    await pollUntil(
        async () => {
            const state = await fetchConfigState();
            return outputs.every((target) => {
                const job = state.jobs.find(
                    (j) => j.pipelineId === target.pipelineId && j.outputId === target.outputId,
                );
                return !job || job.status === 'stopped';
            });
        },
        60000,
        1000,
        'all outputs to reach stopped status',
    );

    console.log('All outputs stopped');
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
            (t) => health.pipelines?.[t.pipelineId]?.input?.status === 'on',
        ).length;
        const inputWarning = resolved.pipelines.filter(
            (t) => health.pipelines?.[t.pipelineId]?.input?.status === 'warning',
        ).length;
        const outputOn = resolved.outputs.filter(
            (t) => health.pipelines?.[t.pipelineId]?.outputs?.[t.outputId]?.status === 'on',
        ).length;
        const outputWarning = resolved.outputs.filter(
            (t) => health.pipelines?.[t.pipelineId]?.outputs?.[t.outputId]?.status === 'warning',
        ).length;

        console.log(
            `Status: inputs on=${inputOn}/${expectedInputs} warning=${inputWarning} | outputs on=${outputOn}/${expectedOutputs} warning=${outputWarning}`,
        );

        if (inputOn === expectedInputs && outputOn === expectedOutputs) {
            console.log('All expected inputs and outputs are green (on)');
            return;
        }

        await sleep(config.pollSec * 1000);
    }

    const health = (await requestJson('/health')).json;
    console.log('Timed out waiting for all manifest streams to become green');
    console.log('---- Input status ----');
    for (const target of resolved.pipelines) {
        const input = health.pipelines?.[target.pipelineId]?.input;
        console.log(
            `${target.pipelineId} input=${input?.status || 'missing'} online=${input?.online ?? 'null'} ready=${input?.ready ?? 'null'}`,
        );
    }
    console.log('---- Output status (non-on only) ----');
    for (const target of resolved.outputs) {
        const output = health.pipelines?.[target.pipelineId]?.outputs?.[target.outputId];
        if (output?.status === 'on') continue;
        console.log(
            `${target.pipelineId}/${target.outputId} status=${output?.status || 'missing'} jobStatus=${output?.jobStatus || 'null'}`,
        );
    }
    throw new Error('Timed out waiting for all manifest streams to become green');
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
    if (!proc?.pid) return;
    try {
        process.kill(proc.pid, 'SIGTERM');
    } catch {
        return;
    }
    for (let attempt = 0; attempt < 10; attempt += 1) {
        if (!isProcessAlive(proc.pid)) return;
        await sleep(200);
    }
    try {
        process.kill(proc.pid, 'SIGKILL');
    } catch {
        // Already exited.
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

function sleep(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
}

function isDirectExecution() {
    if (!process.argv[1]) return false;
    return pathToFileURL(path.resolve(process.argv[1])).href === import.meta.url;
}

export { extractHlsPlaylistName, normalizeHlsOutputUrl };
