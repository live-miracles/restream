import { execFile } from 'child_process';
import { readFile } from 'fs/promises';
import path from 'path';
import type { Express } from 'express';

const ffmpegCmd = process.env.FFMPEG_PATH || 'ffmpeg';
const ffprobeCmd = process.env.FFPROBE_PATH || 'ffprobe';
const EXEC_TIMEOUT_MS = 5_000;

function execVersion(cmd: string, args: string[]): Promise<string> {
    return new Promise((resolve) => {
        execFile(cmd, args, { timeout: EXEC_TIMEOUT_MS }, (err, stdout, stderr) => {
            if (err) return resolve(`error: ${err.message}`);
            const output = (stdout || stderr || '').trim();
            resolve(output.split('\n')[0] || 'unknown');
        });
    });
}

function execFull(cmd: string, args: string[]): Promise<string> {
    return new Promise((resolve) => {
        execFile(cmd, args, { timeout: EXEC_TIMEOUT_MS }, (err, stdout) => {
            if (err) return resolve(`error: ${err.message}`);
            resolve((stdout || '').trim());
        });
    });
}

async function getRestreamVersion(): Promise<{ commitHash: string; commitDate: string }> {
    const [hash, date] = await Promise.all([
        execFull('git', ['rev-parse', '--short', 'HEAD']),
        execFull('git', ['log', '-1', '--format=%ci']),
    ]);
    return { commitHash: hash, commitDate: date };
}

async function findMediamtxBinary(): Promise<string> {
    // Try finding the running mediamtx process and extract its binary path
    const cmdline = await execFull('ps', ['-eo', 'command=']);
    for (const line of cmdline.split('\n')) {
        const match = line.match(/^(\S*mediamtx)\b/);
        if (match) return match[1];
    }
    return 'mediamtx';
}

async function getMediamtxVersion(): Promise<string> {
    const binary = await findMediamtxBinary();
    return execVersion(binary, ['--version']);
}

async function getOsInfo(): Promise<{ kernel: string; distribution: string }> {
    const kernel = await execFull('uname', ['-srm']);

    let distribution = '';
    try {
        distribution = await readFile('/etc/os-release', 'utf-8').then((content) => {
            const pretty = content.match(/^PRETTY_NAME="?(.+?)"?\s*$/m);
            return pretty ? pretty[1] : content.split('\n')[0] || '';
        });
    } catch {
        distribution = await execFull('sw_vers', ['-productName']).then(
            async (name) => {
                const version = await execFull('sw_vers', ['-productVersion']);
                return `${name} ${version}`;
            },
            () => 'unknown',
        );
    }

    return { kernel, distribution };
}

async function getMediamtxConfig(): Promise<string> {
    const candidates = [
        path.join(process.cwd(), 'mediamtx.yml'),
        '/etc/mediamtx/mediamtx.yml',
        '/usr/local/etc/mediamtx/mediamtx.yml',
    ];

    for (const candidate of candidates) {
        try {
            return await readFile(candidate, 'utf-8');
        } catch {
            continue;
        }
    }
    return '# mediamtx.yml not found';
}

export function registerStatusApi({ app }: { app: Express }): void {
    app.get('/api/status', async (_req, res) => {
        const [restream, mediamtxVersion, ffmpegVersion, ffprobeVersion, osInfo] =
            await Promise.all([
                getRestreamVersion(),
                getMediamtxVersion(),
                execVersion(ffmpegCmd, ['-version']),
                execVersion(ffprobeCmd, ['-version']),
                getOsInfo(),
            ]);

        res.json({
            restream,
            mediamtx: mediamtxVersion,
            ffmpeg: ffmpegVersion,
            ffprobe: ffprobeVersion,
            os: osInfo,
        });
    });

    app.get('/api/status/mediamtx-config', async (_req, res) => {
        const config = await getMediamtxConfig();
        res.type('text/plain').send(config);
    });
}
