import { execFile } from 'child_process';
import type { Express, Response } from 'express';
import {
    buildMediamtxPath,
    getMediamtxApiBaseUrl,
    getMediamtxIngestPorts,
    fetchMediamtxJson,
} from '../utils/mediamtx';
import type { Db } from '../types';

const ffprobeCmd = process.env.FFPROBE_PATH || 'ffprobe';
const PROBE_DURATION_S = 20;
const PROBE_TIMEOUT_MS = 45_000;
const POLL_DURATION_MS = 10_000;
const POLL_INTERVAL_MS = 1_000;
const MAX_OUTPUT_BYTES = 2 * 1024 * 1024;
const JOURNAL_TIMEOUT_MS = 5_000;

// ── Probe URL builder ───────────────────────────────

function buildProbeUrl(
    streamKey: string,
    probeProtocol: string,
    ports: { rtmp: string | null; srt: string | null },
): string {
    const path = buildMediamtxPath(streamKey);
    if (probeProtocol === 'srt') {
        const port = ports.srt || '10080';
        return `srt://localhost:${port}?streamid=read:${path}`;
    }
    const port = ports.rtmp || '1935';
    return `rtmp://localhost:${port}/${path}`;
}

// ── Helpers ─────────────────────────────────────────

function truncate(str: string): string {
    if (Buffer.byteLength(str) <= MAX_OUTPUT_BYTES) return str;
    const truncated = Buffer.from(str).subarray(0, MAX_OUTPUT_BYTES).toString('utf8');
    return truncated + '\n... (output truncated)';
}

interface StepResult {
    stdout: string;
    stderr: string;
    exitCode: number | null;
    durationMs: number;
    command: string;
}

function sendSSE(res: Response, event: string, data: unknown): void {
    res.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
}

function sendRunning(res: Response, index: number, name: string, description: string): void {
    sendSSE(res, 'running', { index, name, description });
}

function sendResult(
    res: Response,
    index: number,
    name: string,
    description: string,
    result: StepResult,
    opts?: { issues?: string[]; status?: string },
): void {
    sendSSE(res, 'result', {
        index,
        name,
        description,
        command: result.command,
        stdout: result.stdout,
        stderr: result.stderr,
        exitCode: result.exitCode,
        durationMs: result.durationMs,
        issues: opts?.issues || [],
        status: opts?.status || (result.exitCode === 0 ? 'success' : 'failed'),
    });
}

// ── Runners ─────────────────────────────────────────

function runExec(program: string, args: string[], timeoutMs: number): Promise<StepResult> {
    const cmd = [program, ...args].map((a) => (a.includes(' ') ? `'${a}'` : a)).join(' ');
    const start = Date.now();
    return new Promise((resolve) => {
        execFile(
            program,
            args,
            { timeout: timeoutMs, maxBuffer: MAX_OUTPUT_BYTES * 2 },
            (err, stdout, stderr) => {
                const durationMs = Date.now() - start;
                if (err && 'killed' in err && err.killed) {
                    resolve({
                        stdout: truncate(stdout || ''),
                        stderr: 'Command timed out',
                        exitCode: null,
                        durationMs,
                        command: cmd,
                    });
                    return;
                }
                const exitCode = err && 'code' in err ? (err.code as number | null) : 0;
                resolve({
                    stdout: truncate(stdout || ''),
                    stderr: truncate(stderr || ''),
                    exitCode: typeof exitCode === 'number' ? exitCode : null,
                    durationMs,
                    command: cmd,
                });
            },
        );
    });
}

async function runPoll(
    durationMs: number,
    intervalMs: number,
    run: () => Promise<unknown>,
    abortCheck: () => boolean,
): Promise<{ samples: { t: number; data: unknown }[]; durationMs: number }> {
    const start = Date.now();
    const samples: { t: number; data: unknown }[] = [];
    const end = start + durationMs;

    while (Date.now() < end && !abortCheck()) {
        const sampleStart = Date.now();
        try {
            const data = await run();
            samples.push({ t: Date.now() - start, data });
        } catch (err) {
            samples.push({ t: Date.now() - start, data: { error: String(err) } });
        }
        const elapsed = Date.now() - sampleStart;
        const wait = Math.max(0, intervalMs - elapsed);
        if (Date.now() + wait >= end) break;
        await new Promise((r) => setTimeout(r, wait));
    }

    return { samples, durationMs: Date.now() - start };
}

// Filters mediamtx log lines to only those related to a specific path.
// Two-pass: first finds conn/session IDs from lines mentioning the path,
// then expands by following RTSP session→conn linkage ("created by" /
// "torn down by"), and finally keeps all lines matching any known ID.
function filterLogForPath(output: string, mediaPath: string): string {
    const lines = output.split('\n');
    const connIdPattern = /\[(conn [^\]]+|session [^\]]+)\]/g;
    const ids = new Set<string>();

    // Pass 1: extract conn/session IDs from lines that mention the path
    for (const line of lines) {
        if (!line.includes(mediaPath)) continue;
        for (const m of line.matchAll(connIdPattern)) ids.add(m[1]);
    }

    if (ids.size === 0) return '';

    // Pass 2: follow RTSP session↔conn links from matched lines.
    // "created by IP:port" and "torn down by IP:port" link a session to a conn.
    const addrPattern = /(?:created|torn down) by (\S+)/;
    let expanded = true;
    while (expanded) {
        expanded = false;
        for (const line of lines) {
            const hasKnownId = [...ids].some((id) => line.includes(id));
            if (!hasKnownId) continue;
            // Extract any new conn/session IDs from this line
            for (const m of line.matchAll(connIdPattern)) {
                if (!ids.has(m[1])) {
                    ids.add(m[1]);
                    expanded = true;
                }
            }
            // Link "created by"/"torn down by" address to its conn form
            const addrMatch = line.match(addrPattern);
            if (addrMatch) {
                const connId = `conn ${addrMatch[1]}`;
                if (!ids.has(connId)) {
                    ids.add(connId);
                    expanded = true;
                }
            }
        }
    }

    return lines
        .filter((line) => [...ids].some((id) => line.includes(id)) || line.includes(mediaPath))
        .join('\n');
}

function runJournalctl(sinceTimestamp: string | null): Promise<StepResult> {
    const args = ['-u', 'mediamtx', '--no-pager', '-o', 'short-iso'];
    if (sinceTimestamp) args.push('--since', sinceTimestamp);
    else args.push('-n', '10000');
    const cmd = `journalctl ${args.join(' ')}`;

    const start = Date.now();
    return new Promise((resolve) => {
        execFile('journalctl', args, { timeout: JOURNAL_TIMEOUT_MS }, (err, stdout, stderr) => {
            const durationMs = Date.now() - start;
            const output = (stdout || '').trim();
            if (err && 'code' in err && (err.code as number) === 127) {
                // journalctl not found
                resolve({ stdout: '', stderr: '', exitCode: 127, durationMs, command: cmd });
                return;
            }
            if (!output || output === '-- No entries --') {
                resolve({ stdout: '', stderr: '', exitCode: 0, durationMs, command: cmd });
                return;
            }
            const exitCode = err && 'code' in err ? (err.code as number | null) : 0;
            resolve({
                stdout: output,
                stderr: stderr || '',
                exitCode: typeof exitCode === 'number' ? exitCode : null,
                durationMs,
                command: cmd,
            });
        });
    });
}

// Discover mediamtx log file path: first check the MediaMTX config for logFile,
// then fall back to reading /proc/<pid>/fd/1 (stdout redirect).
async function findMediamtxLogFile(): Promise<string | null> {
    // Check if MediaMTX is configured to write to a file
    try {
        const cfg = (await fetchMediamtxJson('/v3/config/global/get')) as Record<string, unknown>;
        const destinations = cfg?.logDestinations;
        if (Array.isArray(destinations) && destinations.includes('file')) {
            const logFile = cfg?.logFile;
            if (typeof logFile === 'string' && logFile.trim()) {
                return logFile.trim();
            }
        }
    } catch {
        // Config unavailable — try proc fallback
    }

    // Fall back to discovering stdout redirect via /proc
    return new Promise((resolve) => {
        execFile('pgrep', ['-x', 'mediamtx'], { timeout: 2000 }, (err, stdout) => {
            if (err || !stdout.trim()) return resolve(null);
            const pid = stdout.trim().split('\n')[0];
            const fdPath = `/proc/${pid}/fd/1`;
            import('fs').then(({ readlinkSync }) => {
                try {
                    const target = readlinkSync(fdPath);
                    // Only use it if it points to a regular file (not a pipe/socket)
                    if (target.startsWith('/') && !target.startsWith('/dev/')) {
                        resolve(target);
                    } else {
                        resolve(null);
                    }
                } catch {
                    resolve(null);
                }
            });
        });
    });
}

function readLogFileTail(filePath: string, sinceTimestamp: string | null): Promise<StepResult> {
    // Read last 10000 lines to keep it bounded, then filter by timestamp
    const args = ['-n', '10000', filePath];
    const cmd = `tail -n 10000 ${filePath}`;

    const start = Date.now();
    return new Promise((resolve) => {
        execFile(
            'tail',
            args,
            { timeout: JOURNAL_TIMEOUT_MS, maxBuffer: MAX_OUTPUT_BYTES * 2 },
            (err, stdout) => {
                const durationMs = Date.now() - start;
                if (err) {
                    resolve({
                        stdout: '',
                        stderr: `Failed to read ${filePath}`,
                        exitCode: 1,
                        durationMs,
                        command: cmd,
                    });
                    return;
                }
                let output = (stdout || '').trim();

                // Filter lines to only those after the since timestamp.
                // MediaMTX log lines start with "YYYY/MM/DD HH:MM:SS".
                if (sinceTimestamp && output) {
                    const sinceDate = new Date(sinceTimestamp);
                    if (!isNaN(sinceDate.getTime())) {
                        const sinceMs = sinceDate.getTime();
                        const lines = output.split('\n');
                        const filtered: string[] = [];
                        const tsPattern = /^(\d{4}\/\d{2}\/\d{2}\s+\d{2}:\d{2}:\d{2})/;
                        for (const line of lines) {
                            const m = line.match(tsPattern);
                            if (m) {
                                // MediaMTX uses "YYYY/MM/DD HH:MM:SS" in local time
                                const lineDate = new Date(m[1].replace(/\//g, '-'));
                                if (!isNaN(lineDate.getTime()) && lineDate.getTime() < sinceMs)
                                    continue;
                            }
                            filtered.push(line);
                        }
                        output = filtered.join('\n');
                    }
                }

                resolve({ stdout: output, stderr: '', exitCode: 0, durationMs, command: cmd });
            },
        );
    });
}

async function runMediamtxLogs(
    sinceTimestamp: string | null,
    mediaPath: string,
): Promise<StepResult> {
    const start = Date.now();

    // 1. Try journalctl
    const journal = await runJournalctl(sinceTimestamp);
    if (journal.stdout) {
        const filtered = filterLogForPath(journal.stdout, mediaPath);
        return {
            stdout: truncate(filtered || '(no matching log lines)'),
            stderr: journal.stderr,
            exitCode: journal.exitCode,
            durationMs: Date.now() - start,
            command: journal.command + ` | filter for ${mediaPath}`,
        };
    }

    // 2. Try log file (config logFile or /proc stdout redirect)
    const logFile = await findMediamtxLogFile();
    if (logFile) {
        const fileResult = await readLogFileTail(logFile, sinceTimestamp);
        if (fileResult.stdout) {
            const filtered = filterLogForPath(fileResult.stdout, mediaPath);
            return {
                stdout: truncate(filtered || '(no matching log lines)'),
                stderr: '',
                exitCode: 0,
                durationMs: Date.now() - start,
                command: fileResult.command + ` | filter for ${mediaPath}`,
            };
        }
        return {
            stdout: '',
            stderr: `Log file ${logFile} contained no matching entries`,
            exitCode: 0,
            durationMs: Date.now() - start,
            command: fileResult.command,
        };
    }

    // 3. No log source available
    return {
        stdout: '',
        stderr: 'No log source: journalctl has no entries and no log file found',
        exitCode: 0,
        durationMs: Date.now() - start,
        command: 'journalctl / logFile / /proc fallback',
    };
}

// ── Analysis types ──────────────────────────────────

interface ProbeFrame {
    type?: string;
    media_type?: string;
    stream_index: number;
    key_frame?: number;
    pts_time?: string;
    pict_type?: string;
    pkt_size?: string;
}

interface ProbePacket {
    type?: string;
    stream_index: number;
    pts_time?: string;
    dts_time?: string;
    size?: string;
    flags?: string;
}

interface ProbeStream {
    index: number;
    codec_name?: string;
    codec_type?: string;
    width?: number;
    height?: number;
    r_frame_rate?: string;
    avg_frame_rate?: string;
    time_base?: string;
    start_time?: string;
    sample_rate?: string;
    channels?: number;
    channel_layout?: string;
    profile?: string;
    level?: number;
    pix_fmt?: string;
    sample_fmt?: string;
}

interface ProbeFormat {
    filename?: string;
    nb_streams?: number;
    format_name?: string;
    start_time?: string;
    probe_score?: number;
    bit_rate?: string;
}

interface ParsedProbe {
    streams: ProbeStream[];
    format: ProbeFormat;
    packets: ProbePacket[];
    frames: ProbeFrame[];
}

// ── Probe output parser ─────────────────────────────

// When both -show_packets and -show_frames are used, ffprobe emits a single
// interleaved "packets_and_frames" array (not separate "packets"/"frames").
// Each entry has a "type" field: "packet" or "frame".
function parseConsolidatedOutput(stdout: string): ParsedProbe {
    let raw: Record<string, unknown> = {};
    try {
        raw = JSON.parse(stdout) as Record<string, unknown>;
    } catch {
        return { streams: [], format: {}, packets: [], frames: [] };
    }

    const streams = (raw.streams || []) as ProbeStream[];
    const format = (raw.format || {}) as ProbeFormat;

    const combined = (raw.packets_and_frames || []) as (ProbePacket | ProbeFrame)[];
    const packets = combined.filter((e) => e.type === 'packet') as ProbePacket[];
    const frames = combined.filter((e) => e.type === 'frame') as ProbeFrame[];

    return { streams, format, packets, frames };
}

// ── Analysis algorithms ─────────────────────────────

// Measures whether audio and video clocks are drifting apart over the capture
// window. A healthy stream has near-zero drift; drift > 100ms/20s indicates
// the encoder's A/V clocks are diverging, which causes lip-sync issues.
//
// Method: compare the A/V timestamp delta at the start of the window to the
// delta at the end. The difference is the accumulated drift.
function analyzeClockDrift(frames: ProbeFrame[]) {
    const issues: string[] = [];
    const videoPTS = frames
        .filter((f) => f.media_type === 'video' && f.pts_time)
        .map((f) => parseFloat(f.pts_time!))
        .sort((a, b) => a - b);
    const audioPTS = frames
        .filter((f) => f.media_type === 'audio' && f.pts_time)
        .map((f) => parseFloat(f.pts_time!))
        .sort((a, b) => a - b);

    if (videoPTS.length < 2 || audioPTS.length < 2) {
        return { skipped: true, reason: 'Insufficient frames to compute drift', issues };
    }

    // Find the audio timestamp closest to a given video timestamp
    const closest = (target: number, arr: number[]) =>
        arr.reduce((prev, curr) =>
            Math.abs(curr - target) < Math.abs(prev - target) ? curr : prev,
        );

    // A/V delta at start and end of window
    const startDelta = videoPTS[0] - closest(videoPTS[0], audioPTS);
    const endDelta =
        videoPTS[videoPTS.length - 1] - closest(videoPTS[videoPTS.length - 1], audioPTS);

    // Drift = how much the delta changed over the window
    const drift = endDelta - startDelta;
    const severe = Math.abs(drift) > 0.1;

    if (severe) {
        issues.push(
            `Severe A/V clock drift of ${drift.toFixed(3)}s detected. This causes visible lip-sync delays over time.`,
        );
    }

    return {
        initialSyncGapSec: +startDelta.toFixed(3),
        finalSyncGapSec: +endDelta.toFixed(3),
        driftSec: +drift.toFixed(3),
        severe,
        issues,
    };
}

// Checks whether audio and video packets are properly interleaved in the mux.
// A well-muxed stream alternates A/V packets frequently; long runs of one type
// cause decoder buffer starvation (audio glitches or video freezes).
//
// ── DUAL-MODE INTERLEAVING & TRANSPORT ANALYZER DESIGN ───────────────────────
// We employ a protocol-specific, dual-mode timing strategy to dissect transport
// and container health accurately across RTMP and SRT:
//
// 1. RTMP (TCP Transport) & Wallclock Mode (isWallclock = true):
//    TCP guarantees ordered, lossless delivery, which hides network congestion,
//    packet loss, and buffer stalls from standard ffprobe analysis (since packets
//    are eventually delivered in sequence). To expose transport-level Head-of-Line
//    (HOL) blocking and socket congestion, we conditionally run ffprobe for RTMP with
//    '-use_wallclock_as_timestamps 1'. This overwrites media PTS/DTS with Unix epoch
//    microsecond timestamps of when the packets physically arrived at our socket.
//    - We audit: physical arrival stalls (packet gaps) and high-density bursts (TCP flushes).
//    - We skip: PTS/DTS timeline/container checks as the original timeline is overwritten.
//
// 2. SRT (UDP Transport) & Original Timeline Mode (isWallclock = false):
//    UDP does not suffer from HOL blocking. Crucially, SRT publishes real-time native
//    socket metrics (RTT, packet loss, drops, retransmissions) via the MediaMTX REST API,
//    which we poll directly in Step 1. Therefore, for SRT, we preserve the original
//    container timestamps. This is vital because it lets us audit media-layer muxing health:
//    - We audit: out-of-order DTS/PTS, PCR jitter, presentation drift, and codec sequence.
//    - If we had used wallclock timestamps here, it would destroy the stream's native
//      timeline, blindfolding our ability to detect encoder-side sync defects.
// ─────────────────────────────────────────────────────────────────────────────
//
// Skips the initial warmup period where only audio packets arrive (B-frame
// decode delay means video frames don't appear until the first GOP is complete).
function analyzeInterleaving(packets: ProbePacket[]) {
    const issues: string[] = [];
    const videoPackets = packets.filter((p) => p.stream_index === 0);
    const audioPackets = packets.filter((p) => p.stream_index === 1);

    if (videoPackets.length === 0) {
        issues.push('No video packets captured. The encoder is not sending a video track.');
    }
    if (audioPackets.length === 0) {
        issues.push('No audio packets captured. The encoder is not sending an audio track.');
    }

    // Find the index of the first video packet — everything before is warmup
    const firstVideoIdx = packets.findIndex((p) => p.stream_index === 0);
    // If no video packets at all, we can't measure interleaving
    if (firstVideoIdx < 0) {
        return {
            skipped: true,
            reason: 'No video packets captured',
            warmupPackets: packets.length,
            issues,
        };
    }

    // Only analyze packets after both streams are present
    const steady = packets.slice(firstVideoIdx);

    // Detect if we are in Wallclock Mode: ffprobe wallclock timestamps represent
    // the Unix epoch in seconds (typically > 1,000,000,000) whereas standard media
    // container PTS/DTS timelines start near 0.
    const isWallclock = packets.length > 0 && parseFloat(packets[0].pts_time || '0') > 1000000000;

    let maxConsecAudio = 0;
    let maxConsecVideo = 0;
    let run = 0;
    let lastSi = -1;

    for (const pkt of steady) {
        if (pkt.stream_index === lastSi) {
            run++;
        } else {
            if (lastSi === 0) maxConsecVideo = Math.max(maxConsecVideo, run);
            else if (lastSi === 1) maxConsecAudio = Math.max(maxConsecAudio, run);
            lastSi = pkt.stream_index;
            run = 1;
        }
    }
    // Flush final run
    if (lastSi === 0) maxConsecVideo = Math.max(maxConsecVideo, run);
    else if (lastSi === 1) maxConsecAudio = Math.max(maxConsecAudio, run);

    // Track the maximum PTS gap between the two streams — large gaps mean
    // one stream is starving while the other is buffered ahead
    let maxGap = 0;
    let lastVPts = -1;
    let lastAPts = -1;
    for (const pkt of steady) {
        const pts = pkt.pts_time ? parseFloat(pkt.pts_time) : NaN;
        if (isNaN(pts)) continue;
        if (pkt.stream_index === 0) {
            lastVPts = pts;
            if (lastAPts >= 0) maxGap = Math.max(maxGap, Math.abs(pts - lastAPts));
        } else if (pkt.stream_index === 1) {
            lastAPts = pts;
            if (lastVPts >= 0) maxGap = Math.max(maxGap, Math.abs(pts - lastVPts));
        }
    }

    // Transport arrival jitter analysis (when using wallclock timestamps)
    let maxArrivalStallMs = 0;
    let burstPacketCount = 0; // Global count of sub-millisecond packets
    let maxConsecutiveBursts = 0; // Maximum consecutive sub-millisecond packets in a single run
    let currentBurstRun = 0;
    let previousArrival = 0;
    const startSteadyTime =
        steady.length > 0 && steady[0].pts_time ? parseFloat(steady[0].pts_time) : 0;

    for (const pkt of steady) {
        const currentArrival = pkt.pts_time ? parseFloat(pkt.pts_time) : 0;
        if (previousArrival > 0 && currentArrival > 0) {
            const deltaMs = (currentArrival - previousArrival) * 1000;
            // Skip the first 2.0s of the steady stream to ignore initial socket/RTMP handshake and connect transients
            const isStartupTransient =
                startSteadyTime > 0 && currentArrival - startSteadyTime < 2.0;
            if (deltaMs > maxArrivalStallMs && !isStartupTransient) {
                maxArrivalStallMs = deltaMs;
            }
            if (deltaMs < 1) {
                burstPacketCount++;
                currentBurstRun++;
                if (currentBurstRun > maxConsecutiveBursts) {
                    maxConsecutiveBursts = currentBurstRun;
                }
            } else {
                currentBurstRun = 0;
            }
        }
        if (currentArrival > 0) {
            previousArrival = currentArrival;
        }
    }

    let backwardsDtsCount = 0;

    if (isWallclock) {
        // Physical transport stall threshold is increased to 250ms to ignore minor OS/virtualization scheduler jitter
        // under healthy local loopback testing, while reliably flagging real WAN packet delays.
        if (maxArrivalStallMs > 250) {
            issues.push(
                `Transport delivery stall detected! Maximum packet arrival delay was ${maxArrivalStallMs.toFixed(0)}ms. This indicates severe network congestion or Head-of-Line (HOL) blocking.`,
            );
        }
        // Gated by a physical stall (> 250ms) AND a high consecutive burst count (> 100):
        // Real HOL blocking recovery flushes multiple queued-up video frames (hundreds of packets)
        // simultaneously over TCP, creating an uninterrupted consecutive sub-millisecond run.
        // Under healthy conditions, standard RTMP video frame chunking resets the run to 0 every ~33-40ms.
        if (maxConsecutiveBursts > 100 && maxArrivalStallMs > 250) {
            issues.push(
                `Transport packet bursts detected: ${maxConsecutiveBursts} consecutive packets were delivered back-to-back in sub-millisecond flushes. This is a clear signature of TCP Head-of-Line blocking recovery.`,
            );
        }
        // Jitter is also gated by transport delivery stalls to prevent scheduling/buffering noise on loopback
        if (maxGap > 0.5 && maxArrivalStallMs > 250) {
            issues.push(
                `Transport delivery jitter: max inter-stream arrival gap is ${maxGap.toFixed(3)}s. Packets are arriving out of sync due to network jitter.`,
            );
        }
    } else {
        // PTS vs DTS timeline checks (only valid for original container timestamps)
        for (const pkt of packets) {
            const pts = pkt.pts_time ? parseFloat(pkt.pts_time) : NaN;
            const dts = pkt.dts_time ? parseFloat(pkt.dts_time) : NaN;
            if (!isNaN(pts) && !isNaN(dts) && pts < dts) {
                backwardsDtsCount++;
            }
        }

        if (backwardsDtsCount > 0) {
            issues.push(
                `Found ${backwardsDtsCount} packets where Presentation Time (PTS) is earlier than Decode Time (DTS). This invalid timestamp sequence breaks player decoders.`,
            );
        }
        if (maxConsecAudio > 50) {
            issues.push(
                `Poor muxing: max consecutive audio packets without video is ${maxConsecAudio} (expected < 50). This can cause video frame freezes.`,
            );
        }
        if (maxConsecVideo > 50) {
            issues.push(
                `Poor muxing: max consecutive video packets without audio is ${maxConsecVideo} (expected < 50). This can cause audio stutter.`,
            );
        }
        if (maxGap > 0.5) {
            issues.push(
                `Poor muxing: max interleaving gap is ${maxGap.toFixed(3)}s (expected < 0.5s). One stream is buffering too far ahead.`,
            );
        }
    }

    return {
        warmupPackets: firstVideoIdx,
        maxConsecutiveVideoPackets: maxConsecVideo,
        maxConsecutiveAudioPackets: maxConsecAudio,
        maxInterleavingGapSec: +maxGap.toFixed(3),
        ...(isWallclock && {
            maxArrivalStallMs: +maxArrivalStallMs.toFixed(0),
            burstPacketCount,
            maxConsecutiveBursts,
        }),
        defective: isWallclock
            ? false
            : maxConsecAudio > 50 || maxConsecVideo > 50 || maxGap > 0.5 || backwardsDtsCount > 0,
        issues,
    };
}

// Analyzes the Group of Pictures structure: keyframe intervals, B-frame usage,
// and GOP consistency. Irregular GOPs cause seeking problems and adaptive
// bitrate switching failures; missing B-frames may indicate encoder misconfiguration.
function analyzeGOP(frames: ProbeFrame[]) {
    const issues: string[] = [];
    const videoFrames = frames.filter((f) => f.media_type === 'video');
    if (videoFrames.length === 0) {
        return { skipped: true, reason: 'No video frames captured', issues };
    }

    // Extract sorted PTS of keyframes (I-frames) to compute intervals
    const keyframePTS = videoFrames
        .filter((f) => f.key_frame === 1 && f.pts_time)
        .map((f) => parseFloat(f.pts_time!))
        .sort((a, b) => a - b);

    if (keyframePTS.length === 0) {
        issues.push(
            'No keyframes (I-frames) detected in the window. The stream cannot be decoded or seeked without keyframes.',
        );
    }

    // GOP interval = time between consecutive keyframes
    const intervals: number[] = [];
    for (let i = 1; i < keyframePTS.length; i++) {
        intervals.push(+(keyframePTS[i] - keyframePTS[i - 1]).toFixed(2));
    }

    const avg = intervals.length > 0 ? intervals.reduce((a, b) => a + b, 0) / intervals.length : 0;

    const unstable = intervals.length > 0 && intervals.some((v) => Math.abs(v - avg) > 0.5);
    if (unstable) {
        issues.push(
            `Unstable keyframe interval (GOP jitter > 0.5s). Average is ${avg.toFixed(2)}s. This causes player buffering and adaptive bitrate switching failures.`,
        );
    }
    if (avg > 10) {
        issues.push(
            `Keyframe interval is very high (${avg.toFixed(2)}s). High keyframe intervals make seeking sluggish and increase stream latency.`,
        );
    }

    // Count B-frames — high B-frame density is normal (60-75%) for H.264 High profile
    const bCount = videoFrames.filter((f) => f.pict_type === 'B').length;
    const bFramePct = videoFrames.length > 0 ? (bCount / videoFrames.length) * 100 : 0;

    // UDP fragmentation check / Large frames
    let maxPktSize = 0;
    for (const f of videoFrames) {
        if (f.pkt_size) {
            const size = parseInt(f.pkt_size, 10);
            if (!isNaN(size) && size > maxPktSize) maxPktSize = size;
        }
    }
    if (maxPktSize > 1_000_000) {
        issues.push(
            `Extremely large video frame detected (${(maxPktSize / 1024 / 1024).toFixed(2)} MB). Massive keyframes cause severe UDP packet fragmentation and packet drops on lossy networks (SRT).`,
        );
    }

    // Duplicate timestamps check
    let duplicatePtsCount = 0;
    let lastPts = -1;
    for (const f of videoFrames) {
        if (f.pts_time) {
            const pts = parseFloat(f.pts_time);
            if (!isNaN(pts)) {
                if (pts === lastPts) duplicatePtsCount++;
                lastPts = pts;
            }
        }
    }
    if (duplicatePtsCount > 0) {
        issues.push(
            `Found ${duplicatePtsCount} duplicate video presentation timestamps (PTS). This suggests the encoder is overloading and duplicating frames.`,
        );
    }

    return {
        keyframeCount: keyframePTS.length,
        avgKeyframeIntervalSec: +avg.toFixed(2),
        gopIntervals: intervals,
        hasBFrames: bCount > 0,
        bFramePct: +bFramePct.toFixed(1),
        unstable,
        issues,
    };
}

// Measures the A/V start time offset from stream metadata headers. This
// reflects the publisher's actual encoding offset, not the probe connection.
// A gap > 1.5s causes noticeable buffering at playback start.
function analyzeStartupGap(streams: ProbeStream[]) {
    const issues: string[] = [];
    const videoStream = streams.find((s) => s.codec_type === 'video');
    const audioStream = streams.find((s) => s.codec_type === 'audio');

    const vStart = videoStream?.start_time ? parseFloat(videoStream.start_time) : null;
    const aStart = audioStream?.start_time ? parseFloat(audioStream.start_time) : null;

    if (vStart === null || aStart === null) {
        return { skipped: true, reason: 'Missing stream start_time metadata', issues };
    }

    const gap = Math.abs(vStart - aStart);
    const laggy = gap > 1.5;
    if (laggy) {
        issues.push(
            `Large A/V startup gap of ${gap.toFixed(3)}s detected. This causes initial buffering or freeze on players.`,
        );
    }

    return {
        videoStartSec: +vStart.toFixed(3),
        audioStartSec: +aStart.toFixed(3),
        offsetSec: +gap.toFixed(3),
        laggy,
        issues,
    };
}

// Classifies ffprobe stderr warnings into actionable categories.
// These warnings come from the actual codec decoder during the 20s probe
// and indicate real transport or encoding problems.
function analyzeWarnings(stderr: string) {
    const issues: string[] = [];
    const lines = stderr.split('\n');
    let discontinuities = 0;
    let missingRefs = 0;
    let invalidDTS = 0;
    const other: string[] = [];

    for (const line of lines) {
        const t = line.trim();
        if (!t) continue;
        if (/discontinuity/i.test(t)) discontinuities++;
        else if (/missing|reference/i.test(t)) missingRefs++;
        else if (/non-monotonically|invalid dts/i.test(t)) invalidDTS++;
        else if (other.length < 100) other.push(t);
    }

    if (discontinuities > 0) {
        issues.push(
            `Found ${discontinuities} discontinuities in stream. Indicates timestamp jumps from encoder restarts or network drops.`,
        );
    }
    if (missingRefs > 0) {
        issues.push(
            `Found ${missingRefs} missing reference frame warnings. P/B-frames are referencing missing data (packet loss).`,
        );
    }
    if (invalidDTS > 5) {
        issues.push(
            `Found ${invalidDTS} DTS timeline violations. Decode timestamps going backwards breaks decoders and causes frame freezing.`,
        );
    }

    return {
        discontinuities,
        missingRefs,
        dtsViolations: invalidDTS,
        faulty: discontinuities > 0 || invalidDTS > 5,
        otherWarnings: other,
        issues,
    };
}

// Evaluates the metadata returned from ffprobe for common encoding mistakes.
function analyzeCodecAndFormat(streams: ProbeStream[], format: ProbeFormat) {
    const issues: string[] = [];
    const video = streams.find((s) => s.codec_type === 'video');
    const audio = streams.find((s) => s.codec_type === 'audio');

    // 1. Frame dropping
    if (video) {
        const parseFps = (fpsStr: string | undefined): number | null => {
            if (!fpsStr) return null;
            const parts = fpsStr.split('/');
            if (parts.length === 1) return parseFloat(parts[0]);
            if (parts.length === 2) {
                const num = parseFloat(parts[0]);
                const den = parseFloat(parts[1]);
                if (den !== 0) return num / den;
            }
            return null;
        };

        const rFps = parseFps(video.r_frame_rate);
        const avgFps = parseFps(video.avg_frame_rate);

        if (rFps !== null && avgFps !== null && rFps > 0) {
            const diffPct = Math.abs(rFps - avgFps) / rFps;
            if (diffPct > 0.05 && Math.abs(rFps - avgFps) > 1.5) {
                issues.push(
                    `Actual frame rate (${avgFps.toFixed(2)} fps) differs significantly from configured frame rate (${rFps.toFixed(2)} fps). The encoder is overloaded and dropping frames internally.`,
                );
            }
        }

        // 2. Hardware Compatibility (Pixel Format)
        if (video.pix_fmt && video.pix_fmt !== 'yuv420p') {
            const fmt = video.pix_fmt;
            if (fmt === 'yuv422p' || fmt === 'yuv444p' || fmt.includes('10')) {
                issues.push(
                    `Pixel format ${fmt} detected. Chroma subsampling 4:2:2 or 4:4:4 is incompatible with many web browsers and hardware decoders (yuv420p is required for broad compatibility).`,
                );
            }
        }

        // 3. H.264 Baseline profile check
        if (video.codec_name === 'h264' && video.profile === 'Baseline') {
            issues.push(
                `H.264 Baseline profile detected. Baseline has poor compression efficiency compared to Main or High profiles. Upgrade encoder settings to High/Main.`,
            );
        }
    }

    // 4. Audio sample rate check
    if (audio && audio.sample_rate) {
        const rate = parseInt(audio.sample_rate, 10);
        if (!isNaN(rate) && rate !== 44100 && rate !== 48000) {
            issues.push(
                `Non-standard audio sample rate (${rate} Hz) detected. Web browsers and RTMP/HLS targets require 44100 Hz or 48000 Hz for stability.`,
            );
        }
    }

    // 5. Audio channels check
    if (audio && audio.channels !== undefined) {
        const chans = audio.channels;
        if (chans > 2) {
            issues.push(
                `Multi-channel audio (${chans} channels) detected. Standard web players and browser playback typically require stereo (2 channels).`,
            );
        } else if (chans === 0) {
            issues.push(
                `Audio stream reports 0 channels. This indicates invalid audio track configuration.`,
            );
        }
    }

    // 6. Container health
    if (format.probe_score !== undefined) {
        const minScore = format.format_name === 'mpegts' ? 50 : 100;
        if (format.probe_score < minScore) {
            issues.push(
                `Low ffprobe confidence score (${format.probe_score}/100). The container format is non-standard or missing headers, which may cause playback failures.`,
            );
        }
    }

    // 7. Excessive / Insufficient Video Bitrate
    if (format.bit_rate) {
        const rateBps = parseInt(format.bit_rate, 10);
        if (!isNaN(rateBps) && rateBps > 0) {
            const mbps = rateBps / 1_000_000;
            if (mbps > 15) {
                issues.push(
                    `Excessively high total bitrate (${mbps.toFixed(2)} Mbps). This will cause heavy viewer buffering and transport overhead.`,
                );
            } else if (video && video.width && video.width >= 1280 && rateBps < 500_000) {
                issues.push(
                    `Unusually low bitrate (${(rateBps / 1000).toFixed(0)} Kbps) for ${video.width}x${video.height} video. Expect severe image pixelation.`,
                );
            }
        }
    }

    return { issues };
}

// Deeply analyzes the 10 samples collected for the publisher.
function analyzePublisherPoll(samples: any[], protocol: string) {
    const issues: string[] = [];
    let overallStatus: 'HEALTHY' | 'STRAINED' | 'UNHEALTHY' = 'HEALTHY';

    const items = samples
        .map((s) => {
            const arr = s.data;
            return Array.isArray(arr) && arr.length > 0 ? arr[0] : null;
        })
        .filter((item) => item !== null) as any[];

    if (items.length === 0) {
        issues.push('No active publisher connection detected during the 10-second polling window.');
        return { issues, active: false };
    }

    const first = items[0];
    const last = items[items.length - 1];

    const getBytesIn = (item: any) => Number(item?.bytesReceived || item?.inboundBytes || 0);
    const firstBytes = getBytesIn(first);
    const lastBytes = getBytesIn(last);

    if (items.length > 1 && firstBytes === lastBytes) {
        issues.push(
            `Data flow has stalled completely. Bytes received remained flat at ${(firstBytes / 1024 / 1024).toFixed(2)} MB over 10s. The connection may be dead.`,
        );
        overallStatus = 'UNHEALTHY';
    }

    if (protocol === 'srt') {
        const avgRTT = items.reduce((sum, item) => sum + (item.msRTT || 0), 0) / items.length;
        const latency = last.msReceiveTsbPdDelay || last.msPeerTsbPdDelay || 120; // fallback to 120ms default if omitted

        // Gather deltas
        const firstDrop = first.packetsReceivedDrop || 0;
        const lastDrop = last.packetsReceivedDrop || 0;
        const deltaDrop = lastDrop - firstDrop;

        const firstLoss = first.packetsReceivedLoss || 0;
        const lastLoss = last.packetsReceivedLoss || 0;
        const deltaLoss = lastLoss - firstLoss;

        const firstPackets = first.packetsReceived || 0;
        const lastPackets = last.packetsReceived || 0;
        const deltaPackets = lastPackets - firstPackets;

        // Total packets sent includes those received and those lost
        const deltaSent = deltaPackets + deltaLoss;

        // Averages for buffer estimation
        const avgMbpsReceiveRate =
            items.reduce((sum, item) => sum + (item.mbpsReceiveRate || 0), 0) / items.length;
        const byteMSS = last.byteMSS || 1316;
        const avgPacketsReceiveBuf =
            items.reduce((sum, item) => sum + (item.packetsReceiveBuf || 0), 0) / items.length;

        // 1. ARQ Headroom (Physical Retransmission Limit)
        const nAttempts = Math.floor(latency / Math.max(1, avgRTT) - 0.5);
        const hHeadroom = nAttempts >= 3 ? 1.0 : nAttempts >= 2 ? 0.8 : nAttempts >= 1 ? 0.5 : 0.0;

        // 2. Network Delivery Health (Drops vs Recovered Loss)
        const dPct = deltaDrop / Math.max(1, deltaSent);
        const pPct = deltaLoss / Math.max(1, deltaSent);
        const hLoss = Math.max(0.0, 1.0 - (50 * dPct + 2 * pPct));

        // 3. Playback Buffer Stability (Dynamic Packet Expectation)
        const expectedBufferPkts =
            ((avgMbpsReceiveRate * 1000000) / 8 / byteMSS) * (latency / 1000);
        let hBuffer = 1.0;
        if (expectedBufferPkts > 1) {
            hBuffer = Math.min(1.0, avgPacketsReceiveBuf / (expectedBufferPkts * 0.8));
        }

        // Final Deterministic Health Score
        const finalScore = Math.round(100 * hHeadroom * hLoss * hBuffer);

        // Generate human-readable issues based on limits
        if (expectedBufferPkts > 1 && avgPacketsReceiveBuf < expectedBufferPkts * 0.8) {
            issues.push(
                `Low safety margin: Average buffer level is ${avgPacketsReceiveBuf.toFixed(0)} packets (expected > ${(expectedBufferPkts * 0.8).toFixed(0)} packets).`,
            );
        }

        if (nAttempts < 3) {
            issues.push(
                `SRT Latency (${latency}ms) to Round Trip Time (${avgRTT.toFixed(1)}ms) ratio allows only ${Math.max(0, nAttempts)} retransmission attempts (safe margin is >= 3).`,
            );
        }

        if (deltaDrop > 0) {
            issues.push(
                `${deltaDrop} unrecovered SRT packets dropped. Network jitter exceeded buffer capacity.`,
            );
        }

        const firstBelated = first.packetsReceivedBelated || 0;
        const lastBelated = last.packetsReceivedBelated || 0;
        const deltaBelated = lastBelated - firstBelated;
        if (deltaBelated > 0) {
            issues.push(`${deltaBelated} belated packets arrived too late for playback.`);
        }

        if (pPct > 0.01) {
            issues.push(
                `Elevated baseline network packet loss (avg ${(pPct * 100).toFixed(2)}%). SRT overhead is high.`,
            );
        }

        const lastRate = last.mbpsReceiveRate || last.mbpsSendRate || 0;
        const lastCap = last.mbpsLinkCapacity || 0;
        if (lastCap > 0 && lastRate > 0.8 * lastCap) {
            issues.push(
                `Stream bitrate (${lastRate.toFixed(2)} Mbps) approaches estimated link capacity (${lastCap.toFixed(2)} Mbps). Severe congestion risk.`,
            );
        }

        const firstNAK = first.packetsSentNAK || 0;
        const lastNAK = last.packetsSentNAK || 0;
        const deltaNAK = lastNAK - firstNAK;
        if (deltaNAK > 100) {
            issues.push(
                `High rate of NAKs sent (${deltaNAK} in 10s). The receiver is actively requesting many missing packets.`,
            );
        }

        const firstDecryptFail = first.packetsReceivedUndecrypt || 0;
        const lastDecryptFail = last.packetsReceivedUndecrypt || 0;
        const deltaDecryptFail = lastDecryptFail - firstDecryptFail;
        if (deltaDecryptFail > 0) {
            issues.push(
                `SRT decryption failed for ${deltaDecryptFail} packets. Check if passphrase configuration matches.`,
            );
        }

        // Determine overall status based on score thresholds
        if (finalScore < 60) {
            overallStatus = 'UNHEALTHY';
        } else if (finalScore < 90) {
            overallStatus = 'STRAINED';
        }

        // Prepend status summary line at the very top of issues if not healthy
        if (overallStatus !== 'HEALTHY') {
            const statusLine =
                overallStatus === 'STRAINED'
                    ? `🟡 SRT Connection Health is STRAINED (Score: ${finalScore}%). The stream is active but operating with unsafe margins.`
                    : `🔴 SRT Connection Health is UNHEALTHY (Score: ${finalScore}%). High risk of active playback degradation. Viewers are experiencing stutters.`;
            issues.unshift(statusLine);
        }
    } else if (protocol === 'rtmp') {
        const firstDiscard = first.outboundFramesDiscarded || 0;
        const lastDiscard = last.outboundFramesDiscarded || 0;
        const deltaDiscard = lastDiscard - firstDiscard;
        if (deltaDiscard > 0) {
            issues.push(
                `${deltaDiscard} RTMP frames discarded by publisher. Connection is unstable or encoder sent malformed data.`,
            );
            overallStatus = 'UNHEALTHY';
        }

        // Calculate second-by-second delivery jitter (CoCV) from existing polled connection samples
        const deltas: { deltaTimeSec: number; deltaBytes: number; rateKbps: number }[] = [];
        let prevBytes: number | null = null;
        let prevTime: number | null = null;
        let prevId: string | null = null;

        for (const sample of samples) {
            const t = sample.t;
            const pub =
                Array.isArray(sample.data) && sample.data.length > 0 ? sample.data[0] : null;
            if (!pub) continue;

            const id = pub.id || null;
            const bytes = Number(
                pub.bytesReceived || pub.inboundBytes || pub.bytesSent || pub.outboundBytes || 0,
            );
            if (
                prevBytes !== null &&
                prevTime !== null &&
                (prevId === null || id === null || prevId === id)
            ) {
                const deltaBytes = bytes - prevBytes;
                const deltaTimeSec = (t - prevTime) / 1000;
                if (deltaTimeSec > 0 && deltaBytes >= 0) {
                    const rateKbps = (deltaBytes * 8) / (deltaTimeSec * 1000);
                    deltas.push({ deltaTimeSec, deltaBytes, rateKbps });
                }
            }
            prevBytes = bytes;
            prevTime = t;
            prevId = id;
        }

        const rates = deltas.map((d) => d.rateKbps);
        if (rates.length >= 3) {
            const mean = rates.reduce((a, b) => a + b, 0) / rates.length;
            if (mean > 0) {
                const variance =
                    rates.reduce((a, b) => a + Math.pow(b - mean, 2), 0) / rates.length;
                const stdDev = Math.sqrt(variance);
                const cocv = stdDev / mean;

                const meanMbps = mean / 1000;
                const stdDevMbps = stdDev / 1000;

                if (cocv > 0.18) {
                    overallStatus = 'UNHEALTHY';
                    issues.push(
                        `Severe RTMP delivery jitter detected (CoCV: ${(cocv * 100).toFixed(1)}%, StdDev: ${stdDevMbps.toFixed(2)} Mbps, Mean: ${meanMbps.toFixed(2)} Mbps). The stream is experiencing heavy congestion or TCP Head-of-Line (HOL) blocking.`,
                    );
                } else if (cocv > 0.1) {
                    if (overallStatus === 'HEALTHY') {
                        overallStatus = 'STRAINED';
                    }
                    issues.push(
                        `Moderate RTMP delivery jitter detected (CoCV: ${(cocv * 100).toFixed(1)}%, StdDev: ${stdDevMbps.toFixed(2)} Mbps, Mean: ${meanMbps.toFixed(2)} Mbps). The connection operates with tight network safety margins.`,
                    );
                }
            }
        }

        // Prepend status summary line at the very top of issues if not healthy
        if (overallStatus !== 'HEALTHY') {
            const statusLine =
                overallStatus === 'STRAINED'
                    ? `🟡 RTMP Connection Health is STRAINED. The stream is active but experiencing delivery jitter.`
                    : `🔴 RTMP Connection Health is UNHEALTHY. High risk of frame drops, latency spikes, or stream disconnection.`;
            issues.unshift(statusLine);
        }
    }

    return { issues, active: true, status: overallStatus };
}

// Deeply analyzes the 10 samples collected for the readers.
function analyzeReaderPoll(samples: any[]) {
    const issues: string[] = [];

    let totalReadersCount = 0;
    const latestSample = samples.length > 0 ? samples[samples.length - 1] : null;
    if (latestSample && latestSample.data) {
        const data = latestSample.data as Record<string, any[]>;
        for (const items of Object.values(data)) {
            if (Array.isArray(items)) totalReadersCount += items.length;
        }
    }

    if (totalReadersCount === 0) {
        return { issues };
    }

    const firstSample = samples.length > 0 ? samples[0] : null;
    if (firstSample && latestSample && firstSample.data && latestSample.data) {
        const firstData = firstSample.data as Record<string, any[]>;
        const latestData = latestSample.data as Record<string, any[]>;

        let rStrainedCount = 0;
        let rUnhealthyCount = 0;

        for (const proto of ['rtmp', 'srt'] as const) {
            const firstConns = firstData[proto] || [];
            const latestConns = latestData[proto] || [];

            for (const lastC of latestConns) {
                const firstC = firstConns.find((c: any) => c.id === lastC.id);
                if (firstC) {
                    const shortId = lastC.id.substring(0, 8);
                    const firstBytes = Number(firstC.bytesSent || firstC.outboundBytes || 0);
                    const lastBytes = Number(lastC.bytesSent || lastC.outboundBytes || 0);

                    if (firstBytes === lastBytes && firstBytes > 0) {
                        rUnhealthyCount++;
                        issues.push(
                            `Reader connection stalled: [${proto.toUpperCase()} reader ${shortId}] did not pull any bytes over 10s.`,
                        );
                        continue;
                    }

                    if (proto === 'rtmp') {
                        const firstDiscard = firstC.outboundFramesDiscarded || 0;
                        const lastDiscard = lastC.outboundFramesDiscarded || 0;
                        const deltaDiscard = lastDiscard - firstDiscard;
                        if (deltaDiscard > 50) {
                            rUnhealthyCount++;
                            issues.push(
                                `Reader [RTMP reader ${shortId}] discarded ${deltaDiscard} frames in 10s, indicating severe network congestion.`,
                            );
                        } else if (deltaDiscard > 0) {
                            rStrainedCount++;
                            issues.push(
                                `Reader [RTMP reader ${shortId}] discarded ${deltaDiscard} frames in 10s, indicating mild network buffer pressure.`,
                            );
                        }
                    }

                    if (proto === 'srt') {
                        const connSamples = samples
                            .map((s) => {
                                const rList = (s.data || {})[proto] || [];
                                return rList.find((c: any) => c.id === lastC.id);
                            })
                            .filter((c) => c !== undefined && c !== null);

                        let redFlags = 0;
                        let yellowFlags = 0;

                        // 1. msSendBuf (Sender Buffer Congestion)
                        const avgSendBuf =
                            connSamples.reduce(
                                (sum, c) => sum + (c.msSendBuf || c.msSndBuf || 0),
                                0,
                            ) / (connSamples.length || 1);
                        if (avgSendBuf > 200) {
                            redFlags++;
                            issues.push(
                                `Reader [SRT reader ${shortId}] sender buffer is severely congested (avg ${avgSendBuf.toFixed(0)}ms). Downstream playback is likely frozen.`,
                            );
                        } else if (avgSendBuf > 50) {
                            yellowFlags++;
                            issues.push(
                                `Reader [SRT reader ${shortId}] sender buffer is congested (avg ${avgSendBuf.toFixed(0)}ms). Playback delays may occur.`,
                            );
                        }

                        // 2. RTT vs Latency Ratio
                        const avgRTT =
                            connSamples.reduce((sum, c) => sum + (c.msRTT || 0), 0) /
                            (connSamples.length || 1);
                        const latency = lastC.msReceiveTsbPdDelay || lastC.msPeerTsbPdDelay || 120;
                        const rttRatio = latency / (avgRTT || 1);
                        if (latency < 2.5 * avgRTT) {
                            yellowFlags++;
                            issues.push(
                                `Reader [SRT reader ${shortId}] peer latency (${latency}ms) is too tight for RTT (${avgRTT.toFixed(1)}ms). Ratio is ${rttRatio.toFixed(1)}x.`,
                            );
                        }

                        // 3. Dropped Packets
                        const firstDrop = firstC.packetsSentDrop || firstC.packetsDropped || 0;
                        const lastDrop = lastC.packetsSentDrop || lastC.packetsDropped || 0;
                        const deltaDrop = lastDrop - firstDrop;
                        if (deltaDrop > 5) {
                            redFlags++;
                            issues.push(
                                `Reader [SRT reader ${shortId}] dropped ${deltaDrop} packets in 10s due to send retransmission timeout.`,
                            );
                        } else if (deltaDrop > 0) {
                            yellowFlags++;
                            issues.push(
                                `Reader [SRT reader ${shortId}] dropped ${deltaDrop} packets in 10s.`,
                            );
                        }

                        // 4. Decryption Failures
                        const firstDecryptFail = firstC.packetsReceivedUndecrypt || 0;
                        const lastDecryptFail = lastC.packetsReceivedUndecrypt || 0;
                        const deltaDecryptFail = lastDecryptFail - firstDecryptFail;
                        if (deltaDecryptFail > 0) {
                            redFlags++;
                            issues.push(
                                `Reader [SRT reader ${shortId}] decryption failed for ${deltaDecryptFail} packets.`,
                            );
                        }

                        // 5. Raw Loss Rate
                        const firstLoss = firstC.packetsSentLoss || firstC.packetsLoss || 0;
                        const lastLoss = lastC.packetsSentLoss || lastC.packetsLoss || 0;
                        const deltaLoss = lastLoss - firstLoss;
                        const firstSent = firstC.packetsSent || 0;
                        const lastSent = lastC.packetsSent || 0;
                        const deltaSent = lastSent - firstSent;
                        if (deltaSent > 0) {
                            const lossRate = deltaLoss / deltaSent;
                            if (lossRate > 0.1) {
                                redFlags++;
                                issues.push(
                                    `Reader [SRT reader ${shortId}] experiencing severe packet loss rate (${(lossRate * 100).toFixed(2)}%) on downstream link.`,
                                );
                            } else if (lossRate > 0.01) {
                                yellowFlags++;
                                issues.push(
                                    `Reader [SRT reader ${shortId}] experiencing elevated packet loss rate (${(lossRate * 100).toFixed(2)}%) on downstream link.`,
                                );
                            }
                        }

                        if (redFlags > 0) rUnhealthyCount++;
                        else if (yellowFlags > 0) rStrainedCount++;
                    }
                }
            }
        }

        // Consolidated summary at the top of reader issues
        if (rUnhealthyCount > 0) {
            issues.unshift(
                `🔴 Downstream Readers: Unhealthy reader connections detected. Active packet loss or stalled playbacks in progress.`,
            );
        } else if (rStrainedCount > 0) {
            issues.unshift(
                `🟡 Downstream Readers: Strained reader connections detected. Operates with tight safety thresholds.`,
            );
        }
    }

    return { issues };
}

// Deeply analyzes the MediaMTX logs.
function analyzeMediamtxLogs(logs: string) {
    const issues: string[] = [];
    if (!logs) return issues;

    const lines = logs.split('\n');
    let tooManyReordered = 0;
    let maxRecordedExceeded = 0;

    for (const line of lines) {
        if (/too many reordered frames/i.test(line)) tooManyReordered++;
        if (/max recorded size exceeded/i.test(line)) maxRecordedExceeded++;
    }

    // Publishers exit gracefully with "closed: EOF" under SIGINT/SIGTERM.
    // Readers (SIGINT/SIGTERM/SIGKILL) abruptly close sockets, causing normal but noisy server-push "broken pipe" / "connection reset" warnings.
    // TODO: Explore clean output job termination before putting back the check.

    if (tooManyReordered > 0) {
        issues.push(
            `Detected ${tooManyReordered} 'too many reordered frames' warnings. The encoder is sending severely out-of-order data.`,
        );
    }
    if (maxRecordedExceeded > 0) {
        issues.push(
            `Found ${maxRecordedExceeded} 'max recorded size exceeded' warnings. The SRT pre-roll or record buffer overflowed.`,
        );
    }

    return issues;
}

// ── Enabled protocol detection ─────────────────────

interface EnabledProtocols {
    rtmp: boolean;
    srt: boolean;
    hls: boolean;
    webrtc: boolean;
}

async function getEnabledReaderProtocols(): Promise<EnabledProtocols> {
    try {
        const cfg = (await fetchMediamtxJson('/v3/config/global/get')) as Record<string, unknown>;
        const isEnabled = (key: string) => {
            const val = cfg?.[key];
            return typeof val === 'string' && val.trim() !== '';
        };
        return {
            rtmp: isEnabled('rtmpAddress'),
            srt: isEnabled('srtAddress'),
            hls: isEnabled('hlsAddress'),
            webrtc: isEnabled('webRTCAddress'),
        };
    } catch {
        // If config unavailable, assume all enabled
        return { rtmp: true, srt: true, hls: true, webrtc: true };
    }
}

// ── SSE endpoint ────────────────────────────────────

export function registerDiagnosticsApi({ app, db }: { app: Express; db: Db }): void {
    app.get('/pipelines/:pipelineId/diagnostics', async (req, res) => {
        const pipeline = db.getPipeline(req.params.pipelineId);
        if (!pipeline) {
            return res.status(404).json({ error: 'Pipeline not found' });
        }

        const publisherProtocol =
            typeof req.query.publisher === 'string' ? req.query.publisher : null;
        const probeProtocol =
            typeof req.query.probe === 'string' ? req.query.probe : publisherProtocol || 'rtmp';
        const publishStartedAt = typeof req.query.since === 'string' ? req.query.since : null;

        const ports = await getMediamtxIngestPorts();
        const probeUrl = buildProbeUrl(pipeline.streamKey, probeProtocol, ports);
        const mediaPath = buildMediamtxPath(pipeline.streamKey);
        const apiBase = getMediamtxApiBaseUrl();

        res.writeHead(200, {
            'Content-Type': 'text/event-stream',
            'Cache-Control': 'no-cache',
            Connection: 'keep-alive',
            'X-Accel-Buffering': 'no',
        });

        let aborted = false;
        req.on('close', () => {
            aborted = true;
        });

        const totalStart = Date.now();
        const protoLabel = probeProtocol.toUpperCase();

        // ── Step 0: MediaMTX Path Status (instant) ──
        const pathEndpoint = `/v3/paths/get/${mediaPath}`;
        sendRunning(
            res,
            0,
            'MediaMTX Path Status',
            'Checks if the stream path is registered, lists publisher and readers',
        );
        const pathStart = Date.now();
        let pathData: unknown;
        try {
            pathData = await fetchMediamtxJson(pathEndpoint);
        } catch {
            pathData = { error: `Path "${mediaPath}" not found` };
        }
        const pathIssues: string[] = [];
        const pathObj = pathData as any;
        if (pathObj?.error) {
            pathIssues.push(`MediaMTX path error: ${pathObj.error}`);
        } else {
            if (!pathObj?.source) {
                pathIssues.push('No active publisher source is connected to this path.');
            }
            if (pathObj?.ready === false) {
                pathIssues.push('Stream path is registered but not ready.');
            }
        }
        sendResult(
            res,
            0,
            'MediaMTX Path Status',
            'Checks if the stream path is registered, lists publisher and readers',
            {
                stdout: JSON.stringify(pathData, null, 2),
                stderr: '',
                exitCode: 0,
                durationMs: Date.now() - pathStart,
                command: `fetch ${apiBase}${pathEndpoint}`,
            },
            { issues: pathIssues },
        );
        if (aborted) return res.end();

        const connProtocol = publisherProtocol || probeProtocol;
        const connEndpoint = connProtocol === 'srt' ? '/v3/srtconns/list' : '/v3/rtmpconns/list';
        const pollDurLabel = `${POLL_DURATION_MS / 1000}s`;

        // Detect which reader protocols are enabled in MediaMTX
        const enabled = await getEnabledReaderProtocols();
        const readerProtos: string[] = [];
        if (enabled.rtmp) readerProtos.push('RTMP');
        if (enabled.srt) readerProtos.push('SRT');
        if (enabled.hls) readerProtos.push('HLS');
        if (enabled.webrtc) readerProtos.push('WebRTC');
        const readerLabel = readerProtos.join('/') || 'none';

        // ── Steps 1–2: Poll publisher + readers first (before probe adds a reader) ──
        sendRunning(
            res,
            1,
            `Publisher Connection (${connProtocol.toUpperCase()})`,
            `Samples ${connProtocol.toUpperCase()} publisher stats every 1s for ${pollDurLabel}`,
        );
        sendRunning(
            res,
            2,
            'Reader Connections',
            `Samples ${readerLabel} readers every 1s for ${pollDurLabel}`,
        );

        const [pubPoll, readerPoll] = await Promise.all([
            runPoll(
                POLL_DURATION_MS,
                POLL_INTERVAL_MS,
                async () => {
                    const data = (await fetchMediamtxJson(connEndpoint)) as {
                        items?: { path: string; state: string }[];
                    };
                    return (data.items || []).filter(
                        (c) => c.path === mediaPath && c.state === 'publish',
                    );
                },
                () => aborted,
            ),
            runPoll(
                POLL_DURATION_MS,
                POLL_INTERVAL_MS,
                async () => {
                    const fetches: Promise<[string, unknown[]]>[] = [];
                    if (enabled.rtmp)
                        fetches.push(
                            fetchMediamtxJson('/v3/rtmpconns/list')
                                .then((d) => {
                                    const items =
                                        (d as { items?: { path: string; state: string }[] })
                                            .items || [];
                                    return [
                                        'rtmp',
                                        items.filter(
                                            (c) => c.path === mediaPath && c.state === 'read',
                                        ),
                                    ] as [string, unknown[]];
                                })
                                .catch(() => ['rtmp', []] as [string, unknown[]]),
                        );
                    if (enabled.srt)
                        fetches.push(
                            fetchMediamtxJson('/v3/srtconns/list')
                                .then((d) => {
                                    const items =
                                        (d as { items?: { path: string; state: string }[] })
                                            .items || [];
                                    return [
                                        'srt',
                                        items.filter(
                                            (c) => c.path === mediaPath && c.state === 'read',
                                        ),
                                    ] as [string, unknown[]];
                                })
                                .catch(() => ['srt', []] as [string, unknown[]]),
                        );
                    if (enabled.hls)
                        fetches.push(
                            fetchMediamtxJson('/v3/hlsmuxers/list')
                                .then((d) => {
                                    const items = (d as { items?: { path: string }[] }).items || [];
                                    return ['hls', items.filter((c) => c.path === mediaPath)] as [
                                        string,
                                        unknown[],
                                    ];
                                })
                                .catch(() => ['hls', []] as [string, unknown[]]),
                        );
                    if (enabled.webrtc)
                        fetches.push(
                            fetchMediamtxJson('/v3/webrtcsessions/list')
                                .then((d) => {
                                    const items =
                                        (d as { items?: { path: string; state: string }[] })
                                            .items || [];
                                    return [
                                        'webrtc',
                                        items.filter(
                                            (c) => c.path === mediaPath && c.state === 'read',
                                        ),
                                    ] as [string, unknown[]];
                                })
                                .catch(() => ['webrtc', []] as [string, unknown[]]),
                        );
                    const results = await Promise.all(fetches);
                    const out: Record<string, unknown[]> = {};
                    for (const [proto, items] of results) out[proto] = items;
                    return out;
                },
                () => aborted,
            ),
        ]);

        // Dispatch poll results (indices 1–2)
        const pubAnalysis = analyzePublisherPoll(pubPoll.samples, connProtocol);
        sendResult(
            res,
            1,
            `Publisher Connection (${connProtocol.toUpperCase()})`,
            `Samples ${connProtocol.toUpperCase()} publisher stats every 1s for ${pollDurLabel}`,
            {
                stdout: JSON.stringify(pubPoll.samples, null, 2),
                stderr: '',
                exitCode: 0,
                durationMs: pubPoll.durationMs,
                command: `poll ${apiBase}${connEndpoint} every ${POLL_INTERVAL_MS}ms for ${pollDurLabel}`,
            },
            { issues: pubAnalysis.issues, status: pubAnalysis.status },
        );

        const readerAnalysis = analyzeReaderPoll(readerPoll.samples);
        sendResult(
            res,
            2,
            'Reader Connections',
            `Samples ${readerLabel} readers every 1s for ${pollDurLabel}`,
            {
                stdout: JSON.stringify(readerPoll.samples, null, 2),
                stderr: '',
                exitCode: 0,
                durationMs: readerPoll.durationMs,
                command: `poll ${readerProtos.map((p) => p.toLowerCase()).join(' + ')} every ${POLL_INTERVAL_MS}ms for ${pollDurLabel}`,
            },
            { issues: readerAnalysis.issues },
        );

        if (aborted) return res.end();

        // ── Steps 3–7: ffprobe (runs after polls so probe reader doesn't pollute stats) ──
        const probeSteps = [
            {
                name: `Stream Codec Info (${protoLabel})`,
                desc: 'Codec, resolution, FPS, sample rate, and channel layout',
            },
            {
                name: `Container/Format Info (${protoLabel})`,
                desc: 'Overall bitrate, container format, and stream duration',
            },
            {
                name: 'Packet Timing & Interleaving',
                desc: `DTS/PTS gaps, muxing quality over ${PROBE_DURATION_S}s`,
            },
            {
                name: 'GOP & Frame Analysis',
                desc: `Keyframe intervals, B-frame density, and A/V clock drift over ${PROBE_DURATION_S}s`,
            },
            {
                name: 'Error/Warning Log',
                desc: 'Codec warnings, missing refs, and discontinuities',
            },
        ];

        for (let i = 0; i < probeSteps.length; i++) {
            sendRunning(res, 3 + i, probeSteps[i].name, probeSteps[i].desc);
        }

        const probeArgs = [
            '-v',
            'warning',
            '-print_format',
            'json',
            '-show_streams',
            '-show_format',
            '-show_packets',
            '-show_frames',
            '-show_entries',
            'stream=index,codec_name,codec_type,profile,level,width,height,pix_fmt,r_frame_rate,avg_frame_rate,time_base,start_time,sample_rate,sample_fmt,channels,channel_layout:format=filename,nb_streams,format_name,start_time,probe_score:packet=stream_index,pts_time,dts_time,size,flags:frame=media_type,stream_index,key_frame,pts_time,pict_type,pkt_size',
            '-read_intervals',
            `%+${PROBE_DURATION_S}`,
            '-probesize',
            '500M',
            '-analyzeduration',
            `${PROBE_DURATION_S * 1_000_000}`,
            '-fpsprobesize',
            '600',
        ];

        // Protocol-Specific Timestamp Override:
        // For RTMP (TCP), we inject '-use_wallclock_as_timestamps 1' to rewrite packet
        // timestamps to their real socket arrival time. This exposes network-level TCP
        // Head-of-Line blocking (HOL) congestion signatures (stalls and burst flushes).
        // For SRT (UDP), we do NOT use wallclock mode. Instead, we preserve original
        // container PTS/DTS timestamps to analyze media-layer muxing quality (like clock drift,
        // out-of-order DTS, and PTS sequence gaps), while using SRT socket stats to monitor transport.
        if (probeProtocol === 'rtmp') {
            probeArgs.push('-use_wallclock_as_timestamps', '1');
        }

        probeArgs.push(probeUrl);

        const probeResult = await runExec(ffprobeCmd, probeArgs, PROBE_TIMEOUT_MS);
        if (aborted) return res.end();

        // ── Dispatch probe results (indices 3–7) ────
        const parsed = parseConsolidatedOutput(probeResult.stdout);

        const codecAndFormat = analyzeCodecAndFormat(parsed.streams, parsed.format);
        const codecIssues = codecAndFormat.issues.filter(
            (issue) =>
                issue.includes('frame rate') ||
                issue.includes('Pixel format') ||
                issue.includes('Baseline profile') ||
                issue.includes('sample rate') ||
                issue.includes('channel'),
        );
        const formatIssues = codecAndFormat.issues.filter((issue) => !codecIssues.includes(issue));

        // 3: Stream Codec Info
        sendResult(
            res,
            3,
            probeSteps[0].name,
            probeSteps[0].desc,
            {
                stdout: JSON.stringify({ streams: parsed.streams }, null, 2),
                stderr: '',
                exitCode: probeResult.exitCode,
                durationMs: probeResult.durationMs,
                command: probeResult.command,
            },
            { issues: codecIssues },
        );

        // 4: Container/Format Info
        sendResult(
            res,
            4,
            probeSteps[1].name,
            probeSteps[1].desc,
            {
                stdout: JSON.stringify({ format: parsed.format }, null, 2),
                stderr: '',
                exitCode: probeResult.exitCode,
                durationMs: probeResult.durationMs,
                command: probeResult.command,
            },
            { issues: formatIssues },
        );

        // 5: Packet Timing & Interleaving — analysis in stdout, raw packets for download
        const interleaving = analyzeInterleaving(parsed.packets);
        const startupGap = analyzeStartupGap(parsed.streams);
        const packetCounts = {
            totalPackets: parsed.packets.length,
            videoPackets: parsed.packets.filter((p) => p.stream_index === 0).length,
            audioPackets: parsed.packets.filter((p) => p.stream_index === 1).length,
        };
        sendResult(
            res,
            5,
            probeSteps[2].name,
            probeSteps[2].desc,
            {
                stdout: JSON.stringify({ interleaving, startupGap, ...packetCounts }, null, 2),
                stderr: '',
                exitCode: probeResult.exitCode,
                durationMs: probeResult.durationMs,
                command: probeResult.command,
            },
            { issues: [...interleaving.issues, ...startupGap.issues] },
        );

        // 6: GOP & Frame Analysis
        const gop = analyzeGOP(parsed.frames);
        const clockDrift = analyzeClockDrift(parsed.frames);
        const frameCounts = {
            totalFrames: parsed.frames.length,
            videoFrames: parsed.frames.filter((f) => f.media_type === 'video').length,
            audioFrames: parsed.frames.filter((f) => f.media_type === 'audio').length,
        };
        sendResult(
            res,
            6,
            probeSteps[3].name,
            probeSteps[3].desc,
            {
                stdout: JSON.stringify({ gop, clockDrift, ...frameCounts }, null, 2),
                stderr: '',
                exitCode: probeResult.exitCode,
                durationMs: probeResult.durationMs,
                command: probeResult.command,
            },
            { issues: [...gop.issues, ...clockDrift.issues] },
        );

        // 7: Error/Warning Log — analysis in stdout, raw stderr for download
        const warnings = analyzeWarnings(probeResult.stderr);
        sendResult(
            res,
            7,
            probeSteps[4].name,
            probeSteps[4].desc,
            {
                stdout: JSON.stringify(warnings, null, 2),
                stderr: '',
                exitCode: probeResult.exitCode,
                durationMs: probeResult.durationMs,
                command: probeResult.command,
            },
            { issues: warnings.issues },
        );

        // Send raw probe data as a separate event for post-mortem downloads.
        // The full ffprobe JSON (packets + frames) can be large — this avoids
        // bloating individual step results while keeping everything available.
        sendSSE(res, 'probe-raw', {
            stdout: probeResult.stdout,
            stderr: probeResult.stderr,
        });

        if (aborted) return res.end();

        // ── Step 8: MediaMTX logs ───────────────────
        const logDesc = publishStartedAt
            ? 'Filtered logs for this pipeline since publish started'
            : 'Last 10000 log lines filtered for this pipeline';
        sendRunning(res, 8, 'MediaMTX Logs', logDesc);
        const journal = await runMediamtxLogs(publishStartedAt, mediaPath);
        if (!aborted) {
            const logIssues = analyzeMediamtxLogs(journal.stdout);
            sendResult(res, 8, 'MediaMTX Logs', logDesc, journal, { issues: logIssues });
        }

        if (!aborted) {
            sendSSE(res, 'done', { totalDurationMs: Date.now() - totalStart });
        }

        res.end();
    });
}
