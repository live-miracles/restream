export interface RuntimeLifecycle {
    beginShutdown(): boolean;
    isShuttingDown(): boolean;
}

export function createRuntimeLifecycle(): RuntimeLifecycle {
    let shuttingDown = false;

    return {
        beginShutdown(): boolean {
            if (shuttingDown) return false;
            shuttingDown = true;
            return true;
        },
        isShuttingDown(): boolean {
            return shuttingDown;
        },
    };
}
