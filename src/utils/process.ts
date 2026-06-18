import type { ChildProcess } from 'child_process';
import { log, errMsg } from './app';

/**
 * Checks if a process is alive by sending signal 0.
 */
export function isProcessAlive(proc: ChildProcess | number): boolean {
    const pid = typeof proc === 'number' ? proc : proc.pid;
    if (!pid || !Number.isFinite(pid)) return false;
    try {
        process.kill(pid, 0);
        return true;
    } catch {
        return false;
    }
}

interface GracefulKillOptions {
    logTag?: string;
    stdinTimeoutMs?: number;
    sigtermTimeoutMs?: number;
    forceSignal?: string | null;
}

/**
 * Gracefully kills a process by writing 'q' to stdin, escalating to SIGTERM, and then SIGKILL.
 */
export function gracefullyKillProcess(
    proc: ChildProcess,
    {
        logTag = 'process',
        stdinTimeoutMs = 3000,
        sigtermTimeoutMs = 2000,
        forceSignal = null,
    }: GracefulKillOptions = {},
): void {
    if (!proc || !proc.pid) return;

    const pid = proc.pid;

    // If a non-graceful/force signal is requested directly, bypass standard input 'q'
    if (forceSignal && forceSignal !== 'SIGTERM' && forceSignal !== 'SIGINT') {
        try {
            if (isProcessAlive(proc)) {
                log('info', `Force signal ${forceSignal} requested for process ${pid}`, {
                    tag: logTag,
                });
                proc.kill(forceSignal as NodeJS.Signals);
            }
        } catch (err) {
            log('error', `Failed to send force signal ${forceSignal} to process ${pid}`, {
                tag: logTag,
                error: errMsg(err),
            });
        }
        if (forceSignal !== 'SIGKILL') {
            const killTimer = setTimeout(() => {
                try {
                    if (isProcessAlive(proc)) {
                        log(
                            'warn',
                            `Process ${pid} still alive after ${forceSignal}, sending SIGKILL`,
                            { tag: logTag },
                        );
                        proc.kill('SIGKILL');
                    }
                } catch (err) {
                    // ignore
                }
            }, stdinTimeoutMs + sigtermTimeoutMs);
            proc.once('exit', () => clearTimeout(killTimer));
        }
        return;
    }

    let step = 0; // 0 = writing 'q' on stdin, 1 = SIGTERM, 2 = SIGKILL

    const scheduleNext = (delayMs: number) => {
        const timer = setTimeout(() => {
            try {
                if (isProcessAlive(proc)) {
                    if (step === 0) {
                        step = 1;
                        log('info', `Process ${pid} still alive after stdin 'q', sending SIGTERM`, {
                            tag: logTag,
                        });
                        proc.kill('SIGTERM');
                        scheduleNext(sigtermTimeoutMs);
                    } else if (step === 1) {
                        step = 2;
                        log('warn', `Process ${pid} still alive after SIGTERM, sending SIGKILL`, {
                            tag: logTag,
                        });
                        proc.kill('SIGKILL');
                    }
                }
            } catch (err) {
                log('error', `Error during escalation for process ${pid}`, {
                    tag: logTag,
                    error: errMsg(err),
                });
            }
        }, delayMs);
        proc.once('exit', () => clearTimeout(timer));
    };

    let stdinUsed = false;
    if (proc.stdin && proc.stdin.writable) {
        try {
            proc.stdin.write('q');
            proc.stdin.end();
            stdinUsed = true;
            log('debug', `Wrote 'q' to stdin of process ${pid}`, { tag: logTag });
        } catch (err) {
            log('warn', `Failed to write 'q' to stdin of process ${pid}, falling back to SIGTERM`, {
                tag: logTag,
                error: errMsg(err),
            });
        }
    }

    if (stdinUsed) {
        step = 0;
        scheduleNext(stdinTimeoutMs);
    } else {
        try {
            if (isProcessAlive(proc)) {
                log('info', `No writable stdin for process ${pid}, sending SIGTERM`, {
                    tag: logTag,
                });
                proc.kill('SIGTERM');
            }
        } catch (err) {
            // ignore
        }
        step = 1;
        scheduleNext(sigtermTimeoutMs);
    }
}
