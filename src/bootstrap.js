'use strict';

// Runtime registries and app bootstrap.
// createRuntimeRegistries() owns the in-memory Maps (processes, ffmpeg progress, media snapshots)
// that are passed into services at startup. State that must survive a restart lives in SQLite instead.
// bootstrapApp() wires services and routes onto an Express instance and returns a teardown handle.
function createRuntimeRegistries() {
    return {
        ffmpegProgressByJobId: new Map(),
        ffmpegOutputMediaByJobId: new Map(),
        processes: new Map(),
    };
}

// App bootstrap

// Startup helpers keep app.listen, health bootstrapping, and periodic cleanup timers together so
// the composition root does not need to manage operational concerns directly.
function createCleanupRunners({ db, log }) {
    function runJobCleanup(label) {
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

function waitForServerClose(server) {
    if (!server || typeof server.close !== 'function') {
        return Promise.resolve();
    }

    return new Promise((resolve, reject) => {
        try {
            server.close((err) => {
                if (err) {
                    reject(err);
                    return;
                }
                resolve();
            });
        } catch (err) {
            reject(err);
        }
    });
}

function createShutdownController({
    server,
    healthMonitor,
    log,
    onShutdown,
    processRef,
    timerHandles,
}) {
    let shutdownPromise = null;

    async function shutdown(signal = 'manual') {
        if (shutdownPromise) return shutdownPromise;

        shutdownPromise = (async () => {
            log('info', 'Controller shutdown started', { signal });

            for (const timerHandle of timerHandles) {
                clearInterval(timerHandle);
            }

            await waitForServerClose(server);
            const runtimeResult = onShutdown ? await onShutdown({ signal }) : null;
            await healthMonitor.stop?.();

            log('info', 'Controller shutdown finished', { signal });
            return runtimeResult;
        })();

        return shutdownPromise;
    }

    function registerSignalHandlers() {
        ['SIGINT', 'SIGTERM'].forEach((signal) => {
            processRef.once(signal, () => {
                void shutdown(signal)
                    .then(() => {
                        processRef.exit(0);
                    })
                    .catch((err) => {
                        console.error(`Graceful shutdown failed on ${signal}:`, err);
                        processRef.exit(1);
                    });
            });
        });
    }

    return {
        registerSignalHandlers,
        shutdown,
    };
}

async function startServer({
    app,
    healthMonitor,
    db,
    log,
    appPort,
    appHost,
    onShutdown = null,
    processRef = process,
}) {
    // Health bootstrapping runs before opening the port so the first /health calls have seeded
    // transition history and a collector schedule in place.
    const { runJobCleanup, runJobLogCleanup } = createCleanupRunners({ db, log });
    const timerHandles = [];
    await healthMonitor.start();

    const server = await new Promise((resolve, reject) => {
        let resolved = false;
        let instance = null;
        let listening = false;

        const resolveIfReady = () => {
            if (!listening || !instance || resolved) return;
            resolved = true;
            resolve(instance);
        };

        const handleListening = () => {
            console.log(`Controller running on ${appHost}:${appPort}`);

            runJobCleanup('Job cleanup');

            const dailyCleanupTimer = setInterval(
                () => runJobCleanup('Periodic job cleanup'),
                24 * 60 * 60 * 1000,
            );
            dailyCleanupTimer.unref?.();
            timerHandles.push(dailyCleanupTimer);

            const hourlyJobLogCleanupTimer = setInterval(
                () => runJobLogCleanup(7),
                60 * 60 * 1000,
            );
            hourlyJobLogCleanupTimer.unref?.();
            timerHandles.push(hourlyJobLogCleanupTimer);

            listening = true;
            resolveIfReady();
        };

        instance = app.listen(appPort, appHost, handleListening);
        resolveIfReady();

        instance.once?.('error', (err) => {
            if (!resolved) {
                reject(err);
            }
        });
    });

    const shutdownController = createShutdownController({
        server,
        healthMonitor,
        log,
        onShutdown,
        processRef,
        timerHandles,
    });
    shutdownController.registerSignalHandlers();

    return shutdownController;
}

module.exports = {
    createRuntimeRegistries,
    createShutdownController,
    startServer,
    waitForServerClose,
};
