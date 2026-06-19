#!/usr/bin/env node
const http = require('http');
const { execSync } = require('child_process');
const fs = require('fs');
const path = require('path');

const saveLogs = process.argv.includes('--save-logs') || process.env.SAVE_LOGS === 'true';
const runParallel =
    process.argv.includes('--parallel') ||
    process.argv.includes('-p') ||
    process.env.RUN_PARALLEL === 'true';
let LOGS_DIR = '';

if (saveLogs) {
    const now = new Date();
    const pad = (num) => String(now.getUTCFullYear() ? num : num).padStart(2, '0');
    // Format: YYYYMMDD_HHmmss (e.g. 20260523_061530)
    const timestamp = `${now.getFullYear()}${pad(now.getMonth() + 1)}${pad(now.getDate())}_${pad(now.getHours())}${pad(now.getMinutes())}${pad(now.getSeconds())}`;
    LOGS_DIR = path.join(__dirname, 'artifacts', 'logs', `network_diag_${timestamp}`);
    fs.mkdirSync(LOGS_DIR, { recursive: true });
}

const CASES = [
    {
        id: 'A',
        name: 'Case A (Baseline / No Impairment)',
        delay: 0,
        loss: 0,
        desc: 'Normal loopback network. RTT is sub-millisecond, loss 0%. Both protocols perform perfectly.',
    },
    {
        id: 'B',
        name: 'Case B (Moderate LAN Delay)',
        delay: 20,
        loss: 0,
        desc: '20ms delay (40ms RTT), 0% packet loss. Both protocols perform with 100% health.',
    },
    {
        id: 'C',
        name: 'Case C (WAN Ingest - Slight Loss)',
        delay: 50,
        loss: 0.5,
        desc: '50ms delay (100ms RTT), 0.5% packet loss. RTMP is slightly strained due to TCP congestion control; SRT remains completely healthy.',
    },
    {
        id: 'D',
        name: 'Case D (WAN Ingest - Standard Loss)',
        delay: 50,
        loss: 1,
        desc: '50ms delay (100ms RTT), 1% packet loss. Mathis limit (~1.43 Mbps) triggers TCP congestion collapse for RTMP; SRT remains flawless.',
    },
    {
        id: 'E',
        name: 'Case E (Impaired Link - High Loss)',
        delay: 25,
        loss: 5,
        desc: '25ms delay (50ms RTT), 5% packet loss. Mathis limit (~1.27 Mbps) causes severe HOL blocking stalls for RTMP; SRT recovers 99.999% of loss.',
    },
    {
        id: 'F',
        name: 'Case F (Severe WAN Congestion)',
        delay: 75,
        loss: 3,
        desc: '75ms delay (150ms RTT), 3% packet loss. RTMP collapses completely, while SRT leverages its 500ms buffer to recover packets smoothly.',
    },
];

function applyTc(delay, loss, srtIp, srtPort, rtmpIp, rtmpPort) {
    try {
        execSync('sudo tc qdisc del dev lo root >/dev/null 2>&1 || true');
        if (delay > 0 || loss > 0) {
            // Set up a classful prio qdisc as root with 3 bands
            execSync('sudo tc qdisc add dev lo root handle 1: prio bands 3');
            // Attach netem to band 3 (class 1:3)
            execSync(
                `sudo tc qdisc add dev lo parent 1:3 handle 30: netem delay ${delay}ms loss ${loss}%`,
            );

            // Map remote address format (IPv6 bracket stripping and localhost mapping)
            const cleanSrtIp = srtIp === 'localhost' || srtIp === '::1' ? '127.0.0.1' : srtIp;
            const cleanRtmpIp = rtmpIp === 'localhost' || rtmpIp === '::1' ? '127.0.0.1' : rtmpIp;

            // Direct SRT publisher traffic (based on publisher IP:port tuple) to class 1:3
            execSync(
                `sudo tc filter add dev lo protocol ip parent 1: prio 1 u32 match ip dst ${cleanSrtIp}/32 match ip dport ${srtPort} 0xffff flowid 1:3`,
            );
            execSync(
                `sudo tc filter add dev lo protocol ip parent 1: prio 1 u32 match ip src ${cleanSrtIp}/32 match ip sport ${srtPort} 0xffff flowid 1:3`,
            );

            // Direct RTMP publisher traffic (based on publisher IP:port tuple) to class 1:3
            execSync(
                `sudo tc filter add dev lo protocol ip parent 1: prio 1 u32 match ip dst ${cleanRtmpIp}/32 match ip dport ${rtmpPort} 0xffff flowid 1:3`,
            );
            execSync(
                `sudo tc filter add dev lo protocol ip parent 1: prio 1 u32 match ip src ${cleanRtmpIp}/32 match ip sport ${rtmpPort} 0xffff flowid 1:3`,
            );

            console.log(
                `[tc] Applied precise publisher IP:port tuple rules (SRT: ${cleanSrtIp}:${srtPort}, RTMP: ${cleanRtmpIp}:${rtmpPort}) -> delay ${delay}ms, loss ${loss}%`,
            );
        } else {
            console.log('[tc] Cleared rules (baseline).');
        }
    } catch (err) {
        console.error('[tc] Error applying rules:', err.message);
    }
}

function cleanupTc() {
    try {
        execSync('sudo tc qdisc del dev lo root >/dev/null 2>&1 || true');
        console.log('[tc] Cleaned up dev lo root qdisc.');
    } catch (err) {
        // ignore
    }
}

function sleep(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
}

function fetchJson(url) {
    return new Promise((resolve, reject) => {
        http.get(url, (res) => {
            let data = '';
            res.on('data', (chunk) => (data += chunk));
            res.on('end', () => {
                try {
                    resolve(JSON.parse(data));
                } catch (err) {
                    reject(err);
                }
            });
        }).on('error', reject);
    });
}

async function discoverPipelines() {
    const pipelines = await fetchJson('http://localhost:3030/pipelines');
    const rtmpData = await fetchJson('http://localhost:9997/v3/rtmpconns/list');
    const srtData = await fetchJson('http://localhost:9997/v3/srtconns/list');

    const isLocal = (addr) => {
        if (!addr) return false;
        return (
            addr.startsWith('127.0.0.1:') ||
            addr.startsWith('[::1]:') ||
            addr.startsWith('localhost:') ||
            addr.startsWith('::1:')
        );
    };

    let rtmpPipelineId = null;
    let srtPipelineId = null;
    let rtmpPort = null;
    let srtPort = null;
    let rtmpIp = null;
    let srtIp = null;

    // Find local RTMP publisher
    const rtmpPub = (rtmpData.items || []).find((conn) => {
        if (conn.state !== 'publish' || !isLocal(conn.remoteAddr)) return false;
        return pipelines.some((p) => conn.path === `live/${p.streamKey}`);
    });
    if (rtmpPub) {
        const matchingPipeline = pipelines.find((p) => rtmpPub.path === `live/${p.streamKey}`);
        rtmpPipelineId = matchingPipeline ? matchingPipeline.id : null;
        const parts = rtmpPub.remoteAddr.split(':');
        rtmpPort = parts.pop();
        rtmpIp = parts.join(':').replace(/[\[\]]/g, '');
    }

    // Find local SRT publisher
    const srtPub = (srtData.items || []).find((conn) => {
        if (conn.state !== 'publish' || !isLocal(conn.remoteAddr)) return false;
        return pipelines.some((p) => conn.path === `live/${p.streamKey}`);
    });
    if (srtPub) {
        const matchingPipeline = pipelines.find((p) => srtPub.path === `live/${p.streamKey}`);
        srtPipelineId = matchingPipeline ? matchingPipeline.id : null;
        const parts = srtPub.remoteAddr.split(':');
        srtPort = parts.pop();
        srtIp = parts.join(':').replace(/[\[\]]/g, '');
    }

    if (!rtmpPipelineId || !srtPipelineId || !rtmpPort || !srtPort || !rtmpIp || !srtIp) {
        console.warn('\n[Notice] Suitable localhost publishers not found.');
        console.warn(
            'This script is only for localhost testing. Please ensure both local RTMP and SRT publishers are connected to localhost.',
        );
        process.exit(0); // Exit cleanly as requested
    }

    return {
        srt: srtPipelineId,
        rtmp: rtmpPipelineId,
        srtPort,
        rtmpPort,
        srtIp,
        rtmpIp,
    };
}

function runDiagnostics(pipelineId, protocol, logBuffer = null) {
    return new Promise((resolve, reject) => {
        const url = `http://localhost:3030/pipelines/${pipelineId}/diagnostics?publisher=${protocol}&probe=${protocol}`;
        const req = http.get(url, (res) => {
            let buffer = '';
            let currentEvent = null;
            const results = {};

            res.on('data', (chunk) => {
                buffer += chunk.toString();
                const lines = buffer.split('\n');
                buffer = lines.pop();

                for (const line of lines) {
                    const trimmed = line.trim();
                    if (!trimmed) continue;

                    if (trimmed.startsWith('event:')) {
                        currentEvent = trimmed.slice(6).trim();
                    } else if (trimmed.startsWith('data:')) {
                        const dataStr = trimmed.slice(5).trim();
                        if (currentEvent === 'result') {
                            try {
                                const r = JSON.parse(dataStr);
                                results[r.index] = r;
                                const logLine = `  [${protocol.toUpperCase()}] Completed step ${r.index}: ${r.name}`;
                                if (logBuffer) {
                                    logBuffer.push(logLine);
                                } else {
                                    console.log(logLine);
                                }
                            } catch (e) {
                                // ignore partial parses
                            }
                        } else if (currentEvent === 'done') {
                            req.destroy();
                            resolve(results);
                            return;
                        }
                    }
                }
            });

            res.on('end', () => {
                resolve(results);
            });
        });
        req.on('error', reject);
    });
}

async function start() {
    console.log('='.repeat(80));
    console.log(
        `SRT & RTMP NETWORK IMPAIRMENT DIAGNOSTIC TESTER (${runParallel ? 'PARALLEL' : 'SEQUENTIAL'})`,
    );
    console.log('='.repeat(80));
    if (runParallel) {
        console.log('Parallel mode enabled. SRT and RTMP diagnostic tasks will run concurrently.');
    }
    if (saveLogs) {
        console.log(`Saving raw logs to: ${LOGS_DIR}`);
    }

    let pipelines;
    try {
        pipelines = await discoverPipelines();
        console.log(
            `Discovered local target pipelines: SRT=${pipelines.srt} (${pipelines.srtIp}:${pipelines.srtPort}), RTMP=${pipelines.rtmp} (${pipelines.rtmpIp}:${pipelines.rtmpPort})`,
        );
    } catch (err) {
        console.error(
            'Failed to discover active local pipelines. Ensure the restream backend is running on port 3030.',
        );
        console.error('Error details:', err.message);
        process.exit(1);
    }

    try {
        for (const c of CASES) {
            console.log('\n' + '-'.repeat(80));
            console.log(`RUNNING: ${c.name}`);
            console.log(`Description: ${c.desc}`);
            console.log('-'.repeat(80));

            applyTc(
                c.delay,
                c.loss,
                pipelines.srtIp,
                pipelines.srtPort,
                pipelines.rtmpIp,
                pipelines.rtmpPort,
            );
            console.log('Waiting 5s for publishers and flow to adapt...');
            await sleep(5000);

            let srtResults, rtmpResults;
            const srtLogs = [];
            const rtmpLogs = [];
            if (runParallel) {
                console.log(
                    'Running backend diagnostics SSE checks for SRT and RTMP in PARALLEL (takes 10s)...',
                );
                [srtResults, rtmpResults] = await Promise.all([
                    runDiagnostics(pipelines.srt, 'srt', srtLogs),
                    runDiagnostics(pipelines.rtmp, 'rtmp', rtmpLogs),
                ]);
            } else {
                console.log('Running backend diagnostics SSE check for SRT (takes 10s)...');
                srtResults = await runDiagnostics(pipelines.srt, 'srt', srtLogs);

                console.log('\nRunning backend diagnostics SSE check for RTMP (takes 10s)...');
                rtmpResults = await runDiagnostics(pipelines.rtmp, 'rtmp', rtmpLogs);
            }

            let srtSavedLogPath = '';
            let rtmpSavedLogPath = '';
            if (saveLogs) {
                const srtPath = path.join(LOGS_DIR, `Case${c.id}_srt.json`);
                fs.writeFileSync(srtPath, JSON.stringify(srtResults, null, 2));
                srtSavedLogPath = srtPath;

                const rtmpPath = path.join(LOGS_DIR, `Case${c.id}_rtmp.json`);
                fs.writeFileSync(rtmpPath, JSON.stringify(rtmpResults, null, 2));
                rtmpSavedLogPath = rtmpPath;
            }

            // --- SRT BLOCK (Everything SRT-related) ---
            console.log('\n--- SRT BACKEND DIAGNOSTICS & ANALYSIS ---');
            console.log('Step Progress:');
            srtLogs.forEach((line) => console.log(line));
            if (srtSavedLogPath) {
                console.log(`Saved Raw Logs: ${srtSavedLogPath}`);
            }

            const srtStep1 = srtResults[1];
            if (srtStep1) {
                console.log(`Verdict: ${srtStep1.status || 'unknown'}`);

                try {
                    const samples = JSON.parse(srtStep1.stdout);
                    const active = [];
                    for (const s of samples) {
                        const conns = s.data || [];
                        const pub = conns.find((co) => co.state === 'publish');
                        if (pub) active.push(pub);
                    }
                    if (active.length > 0) {
                        const avgRtt =
                            active.reduce((acc, x) => acc + (x.msRTT || 0), 0) / active.length;
                        const avgBuf =
                            active.reduce((acc, x) => acc + (x.msReceiveBuf || 0), 0) /
                            active.length;
                        const first = active[0];
                        const last = active[active.length - 1];
                        const lost =
                            (last.packetsReceivedLoss || 0) - (first.packetsReceivedLoss || 0);
                        const retrans =
                            (last.packetsReceivedRetrans || 0) -
                            (first.packetsReceivedRetrans || 0);
                        const drop =
                            (last.packetsReceivedDrop || 0) - (first.packetsReceivedDrop || 0);

                        console.log(`Avg RTT: ${avgRtt.toFixed(2)} ms`);
                        console.log(`Avg Buffer Level: ${avgBuf.toFixed(1)} ms`);
                        console.log(
                            `Over 10s: Lost=${lost}, Retransmitted=${retrans}, Dropped=${drop}`,
                        );
                    }
                } catch (e) {
                    // ignore stats parse errors
                }

                if (srtStep1.issues && srtStep1.issues.length > 0) {
                    console.log('Detected Issues:');
                    srtStep1.issues.forEach((issue) => console.log(`  - ${issue}`));
                } else {
                    console.log('Detected Issues: None! Connection is perfectly healthy.');
                }
            } else {
                console.log('Error: Backend did not return diagnostics results for SRT Step 1.');
            }

            // --- RTMP BLOCK (Everything RTMP-related) ---
            console.log('\n--- RTMP BACKEND DIAGNOSTICS & ANALYSIS ---');
            console.log('Step Progress:');
            rtmpLogs.forEach((line) => console.log(line));
            if (rtmpSavedLogPath) {
                console.log(`Saved Raw Logs: ${rtmpSavedLogPath}`);
            }

            const rtmpStep1 = rtmpResults[1];
            if (rtmpStep1) {
                console.log(`Verdict: ${rtmpStep1.status || 'unknown'}`);

                try {
                    const samples = JSON.parse(rtmpStep1.stdout);
                    const active = [];
                    for (const s of samples) {
                        const conns = s.data || [];
                        const pub = conns.find((co) => co.state === 'publish');
                        if (pub) active.push(pub);
                    }
                    if (active.length > 0) {
                        const first = active[0];
                        const last = active[active.length - 1];
                        const firstBytes = Number(first.bytesReceived || first.inboundBytes || 0);
                        const lastBytes = Number(last.bytesReceived || last.inboundBytes || 0);
                        const durationSec = 10;
                        const avgBitrateMbps =
                            ((lastBytes - firstBytes) * 8) / (durationSec * 1000000);
                        const firstDiscard = Number(first.outboundFramesDiscarded || 0);
                        const lastDiscard = Number(last.outboundFramesDiscarded || 0);
                        const discarded = lastDiscard - firstDiscard;

                        console.log(`Avg Bitrate: ${avgBitrateMbps.toFixed(2)} Mbps`);
                        console.log(`Over 10s: Discarded Frames=${discarded}`);
                    }
                } catch (e) {
                    // ignore stats parse errors
                }

                if (rtmpStep1.issues && rtmpStep1.issues.length > 0) {
                    console.log('Detected Issues:');
                    rtmpStep1.issues.forEach((issue) => console.log(`  - ${issue}`));
                } else {
                    console.log('Detected Issues: None! Connection is perfectly healthy.');
                }
            } else {
                console.log('Error: Backend did not return diagnostics results for RTMP Step 1.');
            }
        }
    } catch (err) {
        console.error('An error occurred during execution:', err);
    } finally {
        cleanupTc();
    }

    console.log('\n' + '='.repeat(80));
    console.log('ALL EXPERIMENTS COMPLETED');
    console.log('='.repeat(80));
}

start();
