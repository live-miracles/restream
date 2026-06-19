import type { Express } from 'express';
import type { Db } from '../types';

interface HealthMonitor {
    start(): Promise<void>;
}

interface BootstrapOptions {
    app: Express;
    healthMonitor: HealthMonitor;
    db: Db;
    log: (level: string, message: string, fields?: Record<string, unknown>) => void;
    appPort: number;
    afterHealthStart?: () => void;
}

function createCleanupRunners({ db, log }: Pick<BootstrapOptions, 'db' | 'log'>) {
    function runJobCleanup(label: string) {
        try {
            const cleaned = db.cleanupOldJobs();
            if (cleaned.deletedJobs || cleaned.deletedLogs) {
                log('info', label, cleaned);
            }
        } catch (err) {
            console.error(`Error during ${label}:`, err);
        }
    }

    function runJobLogCleanup(days = 7) {
        try {
            db.deleteJobLogsOlderThan(days);
        } catch (err) {
            console.error('Error cleaning up old job logs:', err);
        }
    }

    return { runJobCleanup, runJobLogCleanup };
}

export async function startServer({
    app,
    healthMonitor,
    db,
    log,
    appPort,
    afterHealthStart,
}: BootstrapOptions): Promise<void> {
    const { runJobCleanup, runJobLogCleanup } = createCleanupRunners({ db, log });

    await healthMonitor.start();
    afterHealthStart?.();

    app.listen(appPort, () => {
        console.log(`Controller running on port ${appPort}`);

        runJobCleanup('Job cleanup');

        const dailyCleanupTimer = setInterval(
            () => runJobCleanup('Periodic job cleanup'),
            24 * 60 * 60 * 1000,
        );
        (dailyCleanupTimer as NodeJS.Timeout).unref?.();

        const hourlyJobLogCleanupTimer = setInterval(() => runJobLogCleanup(7), 60 * 60 * 1000);
        (hourlyJobLogCleanupTimer as NodeJS.Timeout).unref?.();
    });
}
