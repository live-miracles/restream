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

async function startServer({ app, healthMonitor, db, log, appPort, appHost }) {
    const { runJobCleanup, runJobLogCleanup } = createCleanupRunners({ db, log });
    await healthMonitor.start();

    app.listen(appPort, appHost, () => {
        console.log(`Controller running on ${appHost}:${appPort}`);

        runJobCleanup('Job cleanup');

        const dailyCleanupTimer = setInterval(
            () => runJobCleanup('Periodic job cleanup'),
            24 * 60 * 60 * 1000,
        );
        dailyCleanupTimer.unref?.();

        const hourlyJobLogCleanupTimer = setInterval(() => runJobLogCleanup(7), 60 * 60 * 1000);
        hourlyJobLogCleanupTimer.unref?.();
    });
}

module.exports = { startServer };
