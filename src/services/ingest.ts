import { spawn } from 'child_process';
import type { ChildProcess } from 'child_process';
import path from 'path';
import { errMsg, log } from '../utils/app';
import type { Db, Ingest } from '../types';

const MEDIAMTX_RTMP_BASE = 'rtmp://localhost:1935';
const LIVE_PATH_PREFIX = 'live/';

export interface IngestService {
    start(id: string): { ok: boolean; error?: string };
    stop(id: string): void;
    isRunning(id: string): boolean;
}

export function createIngestService({ db, mediaDir }: { db: Db; mediaDir: string }): IngestService {
    const ffmpegCmd = process.env.FFMPEG_PATH || 'ffmpeg';
    const processes = new Map<string, ChildProcess>();

    function buildArgs(ingest: Ingest): string[] {
        const filePath = path.join(mediaDir, ingest.filename);
        const rtmpUrl = `${MEDIAMTX_RTMP_BASE}/${LIVE_PATH_PREFIX}${ingest.streamKey}`;

        const args = ['-nostdin', '-hide_banner', '-loglevel', 'info', '-nostats'];

        if (ingest.loop) {
            args.push('-stream_loop', '-1');
        }

        args.push('-re');

        if (ingest.startTime && ingest.startTime.trim()) {
            args.push('-ss', ingest.startTime.trim());
        }

        args.push('-i', filePath);
        args.push('-c:v', 'copy', '-c:a', 'copy');
        args.push('-flvflags', 'no_duration_filesize', '-f', 'flv', rtmpUrl);

        return args;
    }

    return {
        start(id: string): { ok: boolean; error?: string } {
            if (processes.has(id)) return { ok: true };

            const ingest = db.getIngest(id);
            if (!ingest) return { ok: false, error: 'Ingest not found' };

            const args = buildArgs(ingest);
            let child: ChildProcess;
            try {
                child = spawn(ffmpegCmd, args, {
                    stdio: ['ignore', 'ignore', 'pipe'],
                    env: process.env,
                });
            } catch (err) {
                log('error', 'Failed to spawn ingest ffmpeg', { id, error: errMsg(err) });
                return { ok: false, error: errMsg(err) };
            }

            processes.set(id, child);
            log('info', 'Ingest started', {
                id,
                filename: ingest.filename,
                streamKey: ingest.streamKey,
                pid: child.pid ?? null,
            });

            child.stderr?.on('data', (chunk: Buffer) => {
                const lines = chunk
                    .toString()
                    .split('\n')
                    .filter((l) => l.trim());
                for (const line of lines) {
                    log('debug', `[ingest:${id}] ${line}`);
                }
            });

            child.on('error', (err) => {
                log('error', 'Ingest ffmpeg error', { id, error: errMsg(err) });
                processes.delete(id);
            });

            child.on('exit', (code, signal) => {
                processes.delete(id);
                log('info', 'Ingest ended', { id, code, signal });
            });

            return { ok: true };
        },

        stop(id: string): void {
            const proc = processes.get(id);
            if (!proc) return;
            try {
                proc.kill('SIGTERM');
            } catch {
                // process may already be gone
            }
            processes.delete(id);
        },

        isRunning(id: string): boolean {
            return processes.has(id);
        },
    };
}
