const fs = require('fs');
const os = require('os');
const { errMsg } = require('../utils/app');

const SYSTEM_METRICS_SAMPLE_INTERVAL_MS = Number(
    process.env.SYSTEM_METRICS_SAMPLE_INTERVAL_MS || 1000,
);

function getCpuTotals(cpuInfo = os.cpus()) {
    const totals = cpuInfo.reduce(
        (acc, cpu) => {
            const times = cpu.times || {};
            const total =
                Number(times.user || 0) +
                Number(times.nice || 0) +
                Number(times.sys || 0) +
                Number(times.idle || 0) +
                Number(times.irq || 0);
            acc.total += total;
            acc.idle += Number(times.idle || 0);
            return acc;
        },
        { total: 0, idle: 0 },
    );
    return totals;
}

function getMemoryUsage() {
    const totalBytes = os.totalmem();
    const freeBytes = os.freemem();
    const usedBytes = Math.max(0, totalBytes - freeBytes);
    const usedPercent = totalBytes > 0 ? (usedBytes / totalBytes) * 100 : null;

    return {
        totalBytes,
        usedBytes,
        freeBytes,
        usedPercent,
    };
}

function getNetworkTotals() {
    try {
        const content = fs.readFileSync('/proc/net/dev', 'utf8');
        const lines = content.split('\n').slice(2).filter(Boolean);
        let rx = 0;
        let tx = 0;

        for (const line of lines) {
            const [ifaceRaw, rest] = line.split(':');
            if (!ifaceRaw || !rest) continue;
            const iface = ifaceRaw.trim();
            if (!iface || iface === 'lo') continue;

            const fields = rest.trim().split(/\s+/);
            if (fields.length < 16) continue;

            rx += Number(fields[0] || 0);
            tx += Number(fields[8] || 0);
        }

        return { rx, tx };
    } catch (err) {
        return { rx: 0, tx: 0 };
    }
}

function getDiskUsage(pathname = '/') {
    try {
        const stats = fs.statfsSync(pathname);
        const blockSize = Number(stats.bsize || 0);
        const totalBlocks = Number(stats.blocks || 0);
        const availBlocks = Number(stats.bavail || stats.bfree || 0);

        const totalBytes = blockSize * totalBlocks;
        const freeBytes = blockSize * availBlocks;
        const usedBytes = Math.max(0, totalBytes - freeBytes);
        const usedPercent = totalBytes > 0 ? (usedBytes / totalBytes) * 100 : null;

        return { totalBytes, usedBytes, freeBytes, usedPercent };
    } catch (err) {
        return {
            totalBytes: null,
            usedBytes: null,
            freeBytes: null,
            usedPercent: null,
        };
    }
}

function captureSystemMetricsSample(now = Date.now()) {
    const cpuInfo = os.cpus();
    return {
        ts: now,
        cpu: getCpuTotals(cpuInfo),
        net: getNetworkTotals(),
        cores: cpuInfo.length,
        load1: Number(os.loadavg()[0].toFixed(2)),
        memory: getMemoryUsage(),
        disk: getDiskUsage('/'),
    };
}

function buildSystemMetricsSnapshot(previousSample, currentSample) {
    const dtSec = Math.max((currentSample.ts - previousSample.ts) / 1000, 0.001);
    const cpuTotalDiff = currentSample.cpu.total - previousSample.cpu.total;
    const cpuIdleDiff = currentSample.cpu.idle - previousSample.cpu.idle;
    let cpuUsagePercent = 0;
    if (cpuTotalDiff > 0) {
        cpuUsagePercent = Math.max(
            0,
            Math.min(100, ((cpuTotalDiff - cpuIdleDiff) / cpuTotalDiff) * 100),
        );
    }

    const rxDiff = Math.max(0, currentSample.net.rx - previousSample.net.rx);
    const txDiff = Math.max(0, currentSample.net.tx - previousSample.net.tx);
    const downloadBytesPerSec = rxDiff / dtSec;
    const uploadBytesPerSec = txDiff / dtSec;

    return {
        generatedAt: new Date(currentSample.ts).toISOString(),
        cpu: {
            usagePercent: Number(cpuUsagePercent.toFixed(2)),
            cores: currentSample.cores,
            load1: currentSample.load1,
        },
        memory: {
            totalBytes: currentSample.memory.totalBytes,
            usedBytes: currentSample.memory.usedBytes,
            freeBytes: currentSample.memory.freeBytes,
            usedPercent:
                currentSample.memory.usedPercent !== null
                    ? Number(currentSample.memory.usedPercent.toFixed(2))
                    : null,
        },
        disk: currentSample.disk,
        network: {
            downloadBytesPerSec: Number(downloadBytesPerSec.toFixed(2)),
            uploadBytesPerSec: Number(uploadBytesPerSec.toFixed(2)),
            downloadKbps: Number(((downloadBytesPerSec * 8) / 1000).toFixed(2)),
            uploadKbps: Number(((uploadBytesPerSec * 8) / 1000).toFixed(2)),
        },
    };
}

function registerSystemMetricsApi({ app }) {
    let previousSystemMetricsSample = captureSystemMetricsSample();
    let latestSystemMetricsSnapshot = buildSystemMetricsSnapshot(
        previousSystemMetricsSample,
        previousSystemMetricsSample,
    );

    function refreshSystemMetricsSnapshot() {
        const currentSample = captureSystemMetricsSample();
        latestSystemMetricsSnapshot = buildSystemMetricsSnapshot(
            previousSystemMetricsSample,
            currentSample,
        );
        previousSystemMetricsSample = currentSample;
    }

    refreshSystemMetricsSnapshot();

    const systemMetricsTimer = setInterval(() => {
        try {
            refreshSystemMetricsSnapshot();
        } catch {
            /* ignore sampling failures and keep the last good snapshot */
        }
    }, SYSTEM_METRICS_SAMPLE_INTERVAL_MS);
    systemMetricsTimer.unref?.();

    app.get('/metrics/system', (req, res) => {
        try {
            return res.json(latestSystemMetricsSnapshot);
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });
}

module.exports = { registerSystemMetricsApi };
