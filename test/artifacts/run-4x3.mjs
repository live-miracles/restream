#!/usr/bin/env node
import { spawn } from 'node:child_process';
import { closeSync, openSync } from 'node:fs';
import { access, mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath, pathToFileURL } from 'node:url';

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
    rtmpOutputBase: '',
    hlsOutputBase: '',
    inputProtocols: 'rtmp,rtsp,srt',
    maxRetries: 30,
    retryDelaySec: 1,
    timeoutSec: 180,
    pollSec: 2,
    outDir: 'test/artifacts/runs',
};

const LOOPBACK_TIMEOUT_SEC = 30;

const config = {
    apiUrl: process.env.API_URL || defaults.apiUrl,
    rtmpStatUrl: process.env.RTMP_STAT_URL || defaults.rtmpStatUrl,
    manifestPath: resolvePath(process.env.MANIFEST_PATH || defaults.manifestPath),
    logDir: resolvePath(process.env.LOG_DIR || defaults.logDir),
    appLogPath: resolvePath(process.env.APP_LOG_PATH || defaults.appLogPath),
    verifyAppRetries: Number(process.env.VERIFY_APP_RETRIES || defaults.verifyAppRetries),
    inputFile: resolvePath(process.env.INPUT_FILE || defaults.inputFile),
    rtmpOutputBase: process.env.RTMP_OUTPUT_BASE || defaults.rtmpOutputBase,
    hlsOutputBase: process.env.HLS_OUTPUT_BASE || defaults.hlsOutputBase,
    inputProtocols: process.env.INPUT_PROTOCOLS || defaults.inputProtocols,
    maxRetries: Number(process.env.MAX_RETRIES || defaults.maxRetries),
    retryDelaySec: Number(process.env.RETRY_DELAY_SEC || defaults.retryDelaySec),
    timeoutSec: Number(process.env.TIMEOUT_SEC || defaults.timeoutSec),
    pollSec: Number(process.env.POLL_SEC || defaults.pollSec),
    outDir: resolvePath(process.env.OUT_DIR || defaults.outDir),
    keepRunning: readBooleanEnv('KEEP_RUNNING', false),
};

const outputEncodingDefaults = ['source', 'vertical-crop', 'vertical-rotate', '720p', '1080p'];
const supportedOutputEncodings = new Set(outputEncodingDefaults);
const nonSourceOutputEncodings = outputEncodingDefaults.filter((encoding) => encoding !== 'source');
const outputEncodingUsage = new Map(outputEncodingDefaults.map((encoding) => [encoding, 0]));

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

    console.log('== Verify local 4x3 prerequisites ==');
    await ensureRunnerPrerequisites();

    console.log('== Verify app is running (run "make run-host" or start the host deployment first) ==');
    await ensureApiReachable();

    console.log('== Step 1: Ensure 4x3 manifest resources ==');
    const resolved = await ensureResources(manifest, cleanupTargets);

    const newlyCreatedOutputs = resolved.outputs.filter((target) => target.wasCreated);
    if (newlyCreatedOutputs.length > 0) {
        console.log('== Step 1b: Verify newly created outputs default to desiredState=stopped ==');
        await verifyDesiredStateForOutputs(newlyCreatedOutputs, 'stopped');
    } else {
        console.log('== Step 1b: No new outputs were created; skipping default desiredState check ==');
    }

    console.log('== Step 2: Start mixed-protocol input publishers (RTMP/RTSP/SRT) ==');
    const inputPublishers = await startInputPublishers(resolved);

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

    console.log(
        '== Step 12: Verify RTSP output loopback activates target pipeline input via MediaMTX ==',
    );
    await verifyRtspOutputLoopbackToPipelineInput(resolved, inputPublishers);

    console.log('== 4x3 run complete ==');
}

function resolvePath(targetPath) {
    return path.isAbsolute(targetPath) ? targetPath : path.resolve(rootDir, targetPath);
}

function relativePath(targetPath) {
    return path.relative(rootDir, targetPath) || '.';
}

function createCleanupTargets() {
    return {
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
    if (value == null || value === '') {
        return defaultValue;
    }
    return !['0', 'false', 'no', 'off'].includes(String(value).toLowerCase());
}

function printHelp() {
    console.log(
        `Usage: node test/artifacts/run-4x3.mjs\n\nEnvironment flags:\n  KEEP_RUNNING=1    Leave input publishers and manifest-scoped test resources in place after the run\n  MANIFEST_PATH     Path to the tracked 4x3 manifest\n  API_URL           Backend base URL (default: ${defaults.apiUrl})\n  RTMP_STAT_URL     nginx-rtmp stat URL (default: ${defaults.rtmpStatUrl})\n  RTMP_OUTPUT_BASE  Base URL used to normalize RTMP output URLs (if set)\n  HLS_OUTPUT_BASE   Base URL used to normalize HLS playlist output URLs (if set)`,
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
    if (shutdownPromise) {
        return shutdownPromise;
    }

    shutdownPromise = (async () => {
        if (leaveRunning) {
            console.log('== KEEP_RUNNING=1: leaving input publishers and manifest-scoped test resources in place; app stack was not started by this runner ==');
            return;
        }

        for (const proc of ownedProcesses.reverse()) {
            await terminateProcess(proc);
        }

        await cleanupTestResources(cleanupTargets);
    })();

    return shutdownPromise;
}

async function waitForApiHealth() {
    for (let attempt = 1; attempt <= config.verifyAppRetries; attempt += 1) {
        try {
            const response = await fetch(`${config.apiUrl}/healthz`, {
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
        `API did not become ready at ${config.apiUrl}/healthz\nRecent app log:\n${tailText(appLog, 120)}`,
    );
}

async function ensureApiReachable() {
    try {
        const response = await fetch(`${config.apiUrl}/healthz`, {
            signal: AbortSignal.timeout(5000),
        });
        if (!response.ok) {
            throw new Error(`HTTP ${response.status}`);
        }
    } catch (error) {
        throw new Error(
            `API readiness is not reachable at ${config.apiUrl}/healthz. Start app first (for example: make run-host or your host deployment). ${String(error)}`,
        );
    }
}

async function ensureRunnerPrerequisites() {
    await assertCommandAvailable('ffmpeg', ['-version'], 'Install ffmpeg before running the 4x3 suite.');
    await assertCommandAvailable(
        'docker',
        ['compose', 'version'],
        'Install Docker with the compose plugin before running the 4x3 suite.',
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
        let pipeline = state.pipelines.find((item) => item.name === pipelineDef.name);
        let pipelineWasCreated = false;
        if (!pipeline) {
            const result = await requestJson('/pipelines', {
                method: 'POST',
                body: { name: pipelineDef.name },
                okStatuses: [201],
            });
            pipeline = result.json.pipeline;
            console.log(
                `Created pipeline ${pipelineDef.name}: ${pipeline.id} streamKey=${pipeline.streamKey}`,
            );
            state = await fetchConfigState();
            pipeline = state.pipelines.find((item) => item.id === pipeline.id) || pipeline;
            pipelineWasCreated = true;
        } else {
            console.log(
                `Pipeline exists ${pipelineDef.name}: ${pipeline.id} streamKey=${pipeline.streamKey}`,
            );
        }

        trackUnique(
            pipelineTargets,
            {
                name: pipelineDef.name,
                streamKey: pipeline.streamKey,
                pipelineId: pipeline.id,
                wasCreated: pipelineWasCreated,
            },
            (item) => item.pipelineId,
        );

        for (const [outputIndex, outputDef] of pipelineDef.outputs.entries()) {
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
                const outputWithSameName = state.outputs.find(
                    (item) => item.pipelineId === pipeline.id && item.name === outputDef.name,
                );

                if (outputWithSameName) {
                    throw new Error(
                        `Refusing to mutate existing output ${pipelineDef.name}/${outputDef.name}. Expected url=${outputUrl} encoding=${encoding}, found url=${outputWithSameName.url} encoding=${normalizeOutputEncodingValue(outputWithSameName.encoding)}. Remove the conflicting output or run against a clean test stack.`,
                    );
                } else {
                    const result = await requestJson(`/pipelines/${pipeline.id}/outputs`, {
                        method: 'POST',
                        body: { name: outputDef.name, url: outputUrl, encoding },
                        okStatuses: [201],
                    });
                    output = result.json.output;
                    wasCreated = true;
                    console.log(`  Created output ${outputDef.name}: ${output.id}`);
                    state = await fetchConfigState();
                }
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

    console.log(`Manifest used (not modified): ${relativePath(config.manifestPath)}`);
    console.log(`Pipelines in manifest: ${pipelineTargets.length}`);
    console.log(`Outputs in manifest: ${outputTargets.length}`);

    return trackedTargets;
}

async function cleanupTestResources(targets) {
    const pipelineTargets = Array.isArray(targets?.pipelines)
        ? targets.pipelines.filter((target) => target?.wasCreated)
        : [];
    const outputTargets = Array.isArray(targets?.outputs)
        ? targets.outputs.filter((target) => target?.wasCreated)
        : [];

    if (
        pipelineTargets.length === 0 &&
        outputTargets.length === 0
    ) {
        return;
    }

    console.log('== Cleanup test resources (outputs, pipelines) ==');

    const cleanupErrors = [];
    let state = await fetchConfigState();

    for (const target of [...outputTargets].reverse()) {
        const output = state.outputs.find(
            (item) => item.pipelineId === target.pipelineId && item.id === target.outputId,
        );
        if (!output) {
            continue;
        }

        try {
            await requestJson(`/pipelines/${target.pipelineId}/outputs/${target.outputId}`, {
                method: 'DELETE',
                okStatuses: [200],
            });
            console.log(
                `Deleted test output ${target.pipelineName}/${target.outputName} ${target.pipelineId}/${target.outputId}`,
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
        if (!pipeline) {
            continue;
        }

        const remainingOutputs = state.outputs.filter((item) => item.pipelineId === target.pipelineId);
        const manifestOutputIds = manifestOutputIdsByPipeline.get(target.pipelineId) || new Set();
        const nonManifestOutputs = remainingOutputs.filter((item) => !manifestOutputIds.has(item.id));

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
            console.log(`Deleted test pipeline ${target.name} ${target.pipelineId}`);
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
        nonSourceOutputEncodings.find((encoding) => (outputEncodingUsage.get(encoding) || 0) < 1) ||
        'source';
    outputEncodingUsage.set(selectedFallback, (outputEncodingUsage.get(selectedFallback) || 0) + 1);
    return selectedFallback;
}

function normalizeOutputEncodingValue(encodingValue) {
    return String(encodingValue || '')
        .trim()
        .toLowerCase();
}

async function fetchConfigState() {
    const configResult = await requestJson('/config');
    const pipelines = Array.isArray(configResult.json?.pipelines) ? configResult.json.pipelines : [];

    return {
        pipelines,
        outputs: Array.isArray(configResult.json?.outputs) ? configResult.json.outputs : [],
        jobs: Array.isArray(configResult.json?.jobs) ? configResult.json.jobs : [],
    };
}

async function startInputPublishers(resolved) {
    await access(config.inputFile);
    await mkdir(config.logDir, { recursive: true });

    const state = await fetchConfigState();
    const pipelineStateById = new Map((state.pipelines || []).map((pipeline) => [pipeline.id, pipeline]));

    const protocols = config.inputProtocols
        .split(',')
        .map((value) => value.trim().toLowerCase())
        .filter(Boolean);

    if (protocols.length === 0) {
        throw new Error('No input protocols configured');
    }

    const pipelineTargets = resolved?.pipelines || [];
    if (pipelineTargets.length === 0) {
        throw new Error(`No pipelines found in manifest: ${relativePath(config.manifestPath)}`);
    }

    const publisherTargets = [];

    for (const [index, pipelineTarget] of pipelineTargets.entries()) {
        const ordinal = index + 1;
        const protocol = protocols[index % protocols.length];
        const pipelineState = pipelineStateById.get(pipelineTarget.pipelineId) || null;
        const streamKey = String(
            pipelineState?.streamKey || pipelineTarget.streamKey || '',
        ).trim();
        const targetUrl = selectIngestUrl(pipelineState, protocol);
        const logPath = path.join(config.logDir, `input-${ordinal}-${protocol}.log`);
        const publisherTarget = {
            ordinal,
            pipelineName: pipelineTarget.name || `Pipeline ${ordinal}`,
            streamKey,
            protocol,
            targetUrl,
            logPath,
            pid: null,
        };
        const pid = spawnInputPublisher(publisherTarget);

        console.log(
            `[${ordinal}/${pipelineTargets.length}] protocol=${protocol} streamKey=${streamKey} target=${targetUrl}`,
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

function getIngestTargetMarkers(streamKey, matchedManagedPublishers) {
    const markers = new Set();
    const normalizedStreamKey = String(streamKey || '').trim();

    if (normalizedStreamKey) {
        const normalizedRtmpBase = String(config.rtmpOutputBase).replace(/\/+$/, '');
        markers.add(`${normalizedRtmpBase}/${normalizedStreamKey}`);
        markers.add(`/live/${normalizedStreamKey}`);
        markers.add(`streamid=publish:live/${normalizedStreamKey}`);
        markers.add(`streamid=${encodeURIComponent(`publish:live/${normalizedStreamKey}`)}`);
    }

    for (const target of matchedManagedPublishers || []) {
        const targetUrl = String(target?.targetUrl || '').trim();
        if (!targetUrl) continue;
        markers.add(targetUrl);
    }

    return [...markers].filter(Boolean);
}

async function stopAllInputPublishersForStreamKey(streamKey, inputPublishers) {
    const matchedManagedPublishers = (inputPublishers || []).filter(
        (target) => target?.streamKey === streamKey,
    );

    for (const target of matchedManagedPublishers) {
        await stopInputPublisher(target);
    }

    const inputFileMarker = relativePath(config.inputFile);
    const ingestTargetMarkers = getIngestTargetMarkers(streamKey, matchedManagedPublishers);
    const processes = await listFfmpegProcesses();
    const stalePublishers = processes.filter(
        (proc) => {
            const hasIngestTargetMarker = ingestTargetMarkers.some((marker) =>
                proc.command.includes(marker),
            );

            return (
                proc.command.includes('ffmpeg') &&
                proc.command.includes(inputFileMarker) &&
                hasIngestTargetMarker
            );
        },
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

function selectIngestUrl(pipelineRecord, protocol) {
    const ingestUrls = pipelineRecord?.ingestUrls || {};

    if (protocol === 'rtmp' && ingestUrls.rtmp) return ingestUrls.rtmp;
    if (protocol === 'rtsp' && ingestUrls.rtsp) return ingestUrls.rtsp;
    if (protocol === 'srt' && ingestUrls.srt) return ingestUrls.srt;

    throw new Error(
        `Missing ingest URL for protocol=${protocol} pipeline=${pipelineRecord?.id || 'unknown'} streamKey=${pipelineRecord?.streamKey || 'unknown'}`,
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

function isHlsPlaylistUrl(parsedUrl) {
    if (!(parsedUrl instanceof URL)) {
        return false;
    }

    const protocol = String(parsedUrl.protocol || '').toLowerCase();
    if (protocol !== 'http:' && protocol !== 'https:') {
        return false;
    }

    if (/\.m3u8$/i.test(parsedUrl.pathname || '')) {
        return true;
    }

    for (const value of parsedUrl.searchParams.values()) {
        if (/\.m3u8$/i.test(String(value || '').trim())) {
            return true;
        }
    }

    return false;
}

function getOutputProtocol(outputUrl) {
    if (!outputUrl || typeof outputUrl !== 'string') {
        return null;
    }

    try {
        const parsed = new URL(outputUrl);
        if (parsed.protocol === 'rtmp:' || parsed.protocol === 'rtmps:') {
            return 'rtmp';
        }
        if (parsed.protocol === 'rtsp:' || parsed.protocol === 'rtsps:') {
            return 'rtsp';
        }
        if (parsed.protocol === 'srt:') {
            return 'srt';
        }
        if (isHlsPlaylistUrl(parsed)) {
            return 'hls';
        }
        return null;
    } catch {
        return null;
    }
}

function extractHlsPlaylistName(outputUrl) {
    try {
        const parsedOutputUrl =
            outputUrl instanceof URL ? outputUrl : new URL(String(outputUrl || ''));
        if (!isHlsPlaylistUrl(parsedOutputUrl)) {
            return null;
        }

        // Some upload endpoints put the real playlist target in a query param instead of the path.
        // Example: https://.../http_upload_hls?cid=abc&file=out.m3u8 should resolve to out.m3u8,
        // not to the pathname stem http_upload_hls.
        const pathname = String(parsedOutputUrl.pathname || '');
        if (/\.m3u8$/i.test(pathname)) {
            const playlistName = path.posix.basename(pathname);
            if (playlistName) {
                return playlistName;
            }
        }

        for (const value of parsedOutputUrl.searchParams.values()) {
            const normalizedValue = String(value || '').trim();
            if (!/\.m3u8$/i.test(normalizedValue)) {
                continue;
            }

            const playlistName = path.posix.basename(normalizedValue);
            if (playlistName) {
                return playlistName;
            }
        }

        return null;
    } catch {
        return null;
    }
}

function normalizeHlsOutputUrl(outputUrl, hlsOutputBase = config.hlsOutputBase) {
    if (!outputUrl || typeof outputUrl !== 'string' || !hlsOutputBase) {
        return outputUrl;
    }

    try {
        const parsedOutputUrl = new URL(outputUrl);
        if (!isHlsPlaylistUrl(parsedOutputUrl)) {
            return outputUrl;
        }

        const parsedBaseUrl = new URL(hlsOutputBase);
        const normalizedOutputUrl = new URL(parsedBaseUrl.toString());
        const playlistName = extractHlsPlaylistName(parsedOutputUrl) || 'out.m3u8';
        const normalizedBasePath = String(parsedBaseUrl.pathname || '').replace(/\/+$/, '');

        // Keep only the playlist filename from the original URL and graft it onto the override base.
        // Example: HLS_OUTPUT_BASE=http://nginx-rtmp/hls-upload plus file=out.m3u8 becomes
        // http://nginx-rtmp/hls-upload/out.m3u8 regardless of the original host or query string.
        normalizedOutputUrl.pathname = `${normalizedBasePath}/${playlistName}`.replace(/\/+/g, '/');
        normalizedOutputUrl.search = parsedBaseUrl.search;
        normalizedOutputUrl.hash = '';
        return normalizedOutputUrl.toString();
    } catch {
        return outputUrl;
    }
}

function normalizeOutputUrl(outputUrl) {
    if (!outputUrl || typeof outputUrl !== 'string') {
        return outputUrl;
    }

    if (getOutputProtocol(outputUrl) === 'hls') {
        return normalizeHlsOutputUrl(outputUrl);
    }

    if (!config.rtmpOutputBase) {
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
        throw new Error('Loopback stage requires at least two pipelines in the 4x3 manifest');
    }
    return pipelines[1];
}

function selectLoopbackSourceOutput(resolved, targetPipelineId) {
    const outputs = resolved?.outputs || [];
    const candidates = outputs.filter((output) => output.pipelineId !== targetPipelineId);
    if (candidates.length === 0) {
        throw new Error('No source output available outside the selected loopback target pipeline');
    }

    return candidates[0];
}

async function fetchConfigPipelineById(pipelineId) {
    const snapshot = (await requestJson('/config')).json || {};
    const pipelines = Array.isArray(snapshot.pipelines) ? snapshot.pipelines : [];
    return pipelines.find((pipeline) => pipeline.id === pipelineId) || null;
}

function resolveLoopbackUrlFromPayload(ingestUrl, streamKey, protocol) {
    const normalizedProtocol = String(protocol || '').trim().toLowerCase();
    if (!['srt', 'rtsp'].includes(normalizedProtocol)) {
        throw new Error(`Unsupported loopback protocol: ${normalizedProtocol || 'unknown'}`);
    }

    let parsed;
    try {
        parsed = new URL(ingestUrl);
    } catch {
        throw new Error(
            `Target ${normalizedProtocol.toUpperCase()} ingest URL is invalid: ${String(ingestUrl || '')}`,
        );
    }

    if (normalizedProtocol === 'srt') {
        if (parsed.protocol !== 'srt:') {
            throw new Error(`Target ingest URL is not SRT: ${ingestUrl}`);
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
        return String(ingestUrl);
    }

    if (parsed.protocol !== 'rtsp:' && parsed.protocol !== 'rtsps:') {
        throw new Error(`Target ingest URL is not RTSP/RTSPS: ${ingestUrl}`);
    }

    const pathSegments = String(parsed.pathname || '')
        .split('/')
        .filter(Boolean);
    const lastPathSegment =
        pathSegments.length > 0
            ? decodeURIComponent(pathSegments[pathSegments.length - 1])
            : '';
    if (lastPathSegment !== streamKey) {
        throw new Error(
            `Target RTSP ingest URL path mismatch: expected terminal path segment ${streamKey}, got ${lastPathSegment || 'missing'}`,
        );
    }

    // Preserve payload formatting exactly to avoid introducing URI re-encoding differences.
    return String(ingestUrl);
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

async function waitForPipelineInputNotOn(pipelineTarget, timeoutMs, label, logPrefix = 'loopback') {
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
        `[${logPrefix}] target input transitioned off on-state: status=${lastStatus} online=${lastOnline} ready=${lastReady} readers=${lastReaders}`,
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

async function verifyOutputLoopbackToPipelineInput(resolved, inputPublishers, protocol) {
    const normalizedProtocol = String(protocol || '').trim().toLowerCase();
    if (!['srt', 'rtsp'].includes(normalizedProtocol)) {
        throw new Error(`Unsupported loopback protocol: ${normalizedProtocol || 'unknown'}`);
    }

    const protocolLabel = normalizedProtocol.toUpperCase();
    const logPrefix = `${normalizedProtocol}-loopback`;
    const targetPipeline = selectLoopbackTargetPipeline(resolved);
    const sourceOutput = selectLoopbackSourceOutput(resolved, targetPipeline.pipelineId);
    if (sourceOutput.pipelineId === targetPipeline.pipelineId) {
        throw new Error(
            `${protocolLabel} loopback source output pipeline must differ from target pipeline: ${sourceOutput.pipelineId}`,
        );
    }
    const sourceOriginalUrl = sourceOutput.outputUrl;

    const targetConfigPipeline = await fetchConfigPipelineById(targetPipeline.pipelineId);
    const targetIngestUrl = targetConfigPipeline?.ingestUrls?.[normalizedProtocol];
    if (!targetIngestUrl) {
        throw new Error(
            `Selected target pipeline is missing ${protocolLabel} ingest URL: ${targetPipeline.pipelineId}`,
        );
    }

    const loopbackUrl = resolveLoopbackUrlFromPayload(
        targetIngestUrl,
        targetPipeline.streamKey,
        normalizedProtocol,
    );

    console.log(
        `[${logPrefix}] selection ${JSON.stringify({
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
                ingestUrl: loopbackUrl,
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
            `[${logPrefix}] 1/5 stop target external publisher and verify input leaves on-state`,
        );
        const stopSummary = await stopAllInputPublishersForStreamKey(
            targetPipeline.streamKey,
            inputPublishers,
        );
        targetPublisherStopped = true;
        console.log(
            `[${logPrefix}] stopped publishers for streamKey=${targetPipeline.streamKey} managed=${stopSummary.managedStopped} stale=${stopSummary.staleStopped}`,
        );

        await waitForPipelineInputNotOn(
            targetPipeline,
            LOOPBACK_TIMEOUT_SEC * 1000,
            `${targetPipeline.name || targetPipeline.pipelineId} input to leave on-state for ${normalizedProtocol} loopback publish`,
            logPrefix,
        );

        console.log(`[${logPrefix}] 2/5 stop source output and verify output=off`);
        await stopOutputForMutation(sourceOutput);
        await waitForOutputStatus(
            sourceOutput,
            'off',
            LOOPBACK_TIMEOUT_SEC * 1000,
            `${sourceOutput.pipelineName}/${sourceOutput.outputName} to stop before URL mutation`,
        );

        console.log(
            `[${logPrefix}] 3/5 repoint source output to target ${protocolLabel} ingest and start`,
        );
        await updateOutputUrl(sourceOutput, loopbackUrl);
        await startOutputWithRetry(sourceOutput);
        sourceMutated = true;

        console.log(`[${logPrefix}] 4/5 verify target pipeline input=on`);
        const timeoutMs = LOOPBACK_TIMEOUT_SEC * 1000;
        try {
            await waitForPipelineInputStatus(
                targetPipeline,
                'on',
                timeoutMs,
                `${targetPipeline.name || targetPipeline.pipelineId} input to become on via ${normalizedProtocol} loopback`,
            );
        } catch (_error) {
            const health = (await requestJson('/health')).json;
            const summary = getHealthLoopbackSummary(health || {}, sourceOutput, targetPipeline);
            throw new Error(
                `Timed out waiting for ${protocolLabel} loopback activation (${LOOPBACK_TIMEOUT_SEC}s): ${JSON.stringify(summary)}`,
            );
        }

        console.log(
            `[${logPrefix}] activation passed sourceOutput=${sourceOutput.pipelineId}/${sourceOutput.outputId} targetPipeline=${targetPipeline.pipelineId}`,
        );
        return;
    } finally {
        if (sourceMutated) {
            console.log(
                `[${logPrefix}] 5/5 restore source output URL and restart target publisher`,
            );
            try {
                await stopOutputForMutation(sourceOutput);
                await waitForOutputStatus(
                    sourceOutput,
                    'off',
                    LOOPBACK_TIMEOUT_SEC * 1000,
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
                    LOOPBACK_TIMEOUT_SEC * 1000,
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
                    LOOPBACK_TIMEOUT_SEC * 1000,
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
            throw new Error(
                `${protocolLabel} loopback cleanup failed: ${cleanupErrors.join(' | ')}`,
            );
        }
    }
}

async function verifySrtOutputLoopbackToPipelineInput(resolved, inputPublishers) {
    await verifyOutputLoopbackToPipelineInput(resolved, inputPublishers, 'srt');
}

async function verifyRtspOutputLoopbackToPipelineInput(resolved, inputPublishers) {
    await verifyOutputLoopbackToPipelineInput(resolved, inputPublishers, 'rtsp');
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

    const expected = resolved.outputs
        .filter((output) => getOutputProtocol(output.outputUrl) === 'rtmp')
        .map((output) => ({
            streamName: extractStreamName(output.outputUrl),
            pipelineId: output.pipelineId,
            outputId: output.outputId,
        }));

    if (expected.length === 0) {
        console.log('No RTMP outputs in manifest; skipping nginx /stat correlation');
        return;
    }

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
                `stream blocks in /stat (${streamBlocks}) are less than expected RTMP outputs (${expected.length})`,
            ];
            console.log(
                `nginx /stat correlation (attempt ${attempt}/10): expected_rtmp_streams=${expected.length} stream_blocks=${streamBlocks} issues=1`,
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
            `nginx /stat correlation (attempt ${attempt}/10): expected_rtmp_streams=${expected.length} stream_blocks=${streamBlocks} issues=${issues.length}`,
        );

        if (issues.length === 0) {
            await writeNginxStatSummary(summary);
            console.log(
                'nginx /stat correlation passed: expected RTMP stream blocks present and each RTMP output has video+audio meta',
            );
            return;
        }

        await sleep(1000);
    }

    console.log(
        `nginx /stat correlation: expected_rtmp_streams=${expected.length} stream_blocks=${lastStreamBlocks}`,
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

function isDirectExecution() {
    if (!process.argv[1]) {
        return false;
    }

    return pathToFileURL(path.resolve(process.argv[1])).href === import.meta.url;
}

export { extractHlsPlaylistName, normalizeHlsOutputUrl };
