import { spawn } from 'child_process';
import type { ChildProcess } from 'child_process';
import { unlink, mkdir } from 'fs/promises';
import path from 'path';
import { errMsg, log } from '../utils/app';
import { buildPullInputUrl } from '../utils/mediamtx';
import type { Db } from '../types';

export interface RecordingService {
    init(): void;
    enableRecording(pipelineId: string): Promise<void>;
    disableRecording(pipelineId: string): void;
    getState(pipelineId: string): { enabled: boolean; active: boolean };
    onInputRecovered(pipelineId: string): void;
    onInputLost(pipelineId: string): void;
}

const MIN_DURATION_MS = 5000;
const CRASH_RESTART_DELAY_MS = 2000;

function sanitizeName(name: string): string {
    return name.replace(/[/\\:*?"<>|]/g, '_');
}

function buildFilename(pipeName: string): string {
    const now = new Date();
    const pad = (n: number) => String(n).padStart(2, '0');
    const date = `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}`;
    const time = `${pad(now.getHours())}-${pad(now.getMinutes())}-${pad(now.getSeconds())}`;
    return `${date} ${time} ${sanitizeName(pipeName)}.mkv`;
}

export function createRecordingService({
    db,
    mediaDir,
    isInputOn,
}: {
    db: Db;
    mediaDir: string;
    isInputOn: (pipelineId: string) => boolean;
}): RecordingService {
    const ffmpegCmd = process.env.FFMPEG_PATH || 'ffmpeg';
    const processes = new Map<string, ChildProcess>();
    const startedAt = new Map<string, number>();
    const filePaths = new Map<string, string>();
    const stopRequested = new Set<string>();

    function isEnabled(pipelineId: string): boolean {
        return db.getMeta(`recording_enabled:${pipelineId}`) === '1';
    }

    function isActive(pipelineId: string): boolean {
        return processes.has(pipelineId);
    }

    function startRecording(pipelineId: string): void {
        if (isActive(pipelineId)) return;
        const pipeline = db.getPipeline(pipelineId);
        if (!pipeline) return;

        const filename = buildFilename(pipeline.name);
        const filePath = path.join(mediaDir, filename);
        const inputUrl = buildPullInputUrl(pipeline.streamKey, 'rtmp');

        const args = [
            '-nostdin',
            '-hide_banner',
            '-loglevel',
            'info',
            '-nostats',
            '-i',
            inputUrl,
            '-c:v',
            'copy',
            '-c:a',
            'copy',
            '-f',
            'matroska',
            filePath,
        ];

        let child: ChildProcess;
        try {
            child = spawn(ffmpegCmd, args, {
                stdio: ['ignore', 'ignore', 'pipe'],
                env: process.env,
            });
        } catch (err) {
            log('error', 'Failed to spawn recording ffmpeg', { pipelineId, error: errMsg(err) });
            return;
        }

        processes.set(pipelineId, child);
        startedAt.set(pipelineId, Date.now());
        filePaths.set(pipelineId, filePath);
        log('info', 'Recording started', { pipelineId, filename, pid: child.pid ?? null });

        child.on('error', (err) => {
            log('error', 'Recording ffmpeg error', { pipelineId, error: errMsg(err) });
            processes.delete(pipelineId);
            startedAt.delete(pipelineId);
            filePaths.delete(pipelineId);
            stopRequested.delete(pipelineId);
        });

        child.on('exit', (code, signal) => {
            const wasStopRequested = stopRequested.delete(pipelineId);
            const duration = Date.now() - (startedAt.get(pipelineId) ?? Date.now());
            const fp = filePaths.get(pipelineId);
            processes.delete(pipelineId);
            startedAt.delete(pipelineId);
            filePaths.delete(pipelineId);

            log('info', 'Recording ended', { pipelineId, filename, duration, code, signal });

            if (fp && duration < MIN_DURATION_MS) {
                unlink(fp).catch(() => {});
                log('info', 'Deleted short recording', { pipelineId, filename, duration });
            }

            if (!wasStopRequested && isEnabled(pipelineId) && isInputOn(pipelineId)) {
                setTimeout(() => {
                    if (!isActive(pipelineId) && isEnabled(pipelineId) && isInputOn(pipelineId)) {
                        startRecording(pipelineId);
                    }
                }, CRASH_RESTART_DELAY_MS);
            }
        });
    }

    function stopRecording(pipelineId: string): void {
        const proc = processes.get(pipelineId);
        if (!proc) return;
        stopRequested.add(pipelineId);
        try {
            proc.kill('SIGTERM');
        } catch {
            // process may already be gone
        }
    }

    return {
        init(): void {
            for (const pipeline of db.listPipelines()) {
                if (isEnabled(pipeline.id) && isInputOn(pipeline.id)) {
                    startRecording(pipeline.id);
                }
            }
        },

        async enableRecording(pipelineId: string): Promise<void> {
            await mkdir(mediaDir, { recursive: true });
            db.setMeta(`recording_enabled:${pipelineId}`, '1');
            if (isInputOn(pipelineId)) startRecording(pipelineId);
        },

        disableRecording(pipelineId: string): void {
            db.setMeta(`recording_enabled:${pipelineId}`, '0');
            stopRecording(pipelineId);
        },

        getState(pipelineId: string): { enabled: boolean; active: boolean } {
            return { enabled: isEnabled(pipelineId), active: isActive(pipelineId) };
        },

        onInputRecovered(pipelineId: string): void {
            if (isEnabled(pipelineId)) startRecording(pipelineId);
        },

        onInputLost(pipelineId: string): void {
            stopRecording(pipelineId);
        },
    };
}
