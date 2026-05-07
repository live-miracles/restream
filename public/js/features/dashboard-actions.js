// Dashboard action coordinator.
// Keeps dashboard refresh/baseline callbacks and visibility-sync dependents out of the render
// modules so cross-feature coordination does not require direct imports.

const dashboardActionHandlers = {
    refreshDashboard: null,
    syncUserConfigBaseline: null,
};

const dashboardVisibilitySyncHandlers = new Set();

function setDashboardActionHandlers(handlers) {
    Object.assign(dashboardActionHandlers, handlers || {});
}

async function refreshDashboard() {
    return dashboardActionHandlers.refreshDashboard?.();
}

async function syncUserConfigBaseline() {
    return dashboardActionHandlers.syncUserConfigBaseline?.();
}

function registerDashboardVisibilitySync(handler) {
    if (typeof handler !== 'function') {
        return () => {};
    }

    dashboardVisibilitySyncHandlers.add(handler);
    return () => {
        dashboardVisibilitySyncHandlers.delete(handler);
    };
}

async function syncDashboardVisibilityDependents() {
    for (const handler of dashboardVisibilitySyncHandlers) {
        await handler();
    }
}

export {
    refreshDashboard,
    registerDashboardVisibilitySync,
    setDashboardActionHandlers,
    syncDashboardVisibilityDependents,
    syncUserConfigBaseline,
};