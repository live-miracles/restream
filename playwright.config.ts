import { defineConfig, devices } from '@playwright/test';

export default defineConfig({
    testDir: './test',
    fullyParallel: false,
    forbidOnly: !!process.env.CI,
    retries: process.env.CI ? 1 : 0,
    workers: 1,
    reporter: 'list',
    timeout: 30000,
    use: {
        baseURL: process.env.BASE_URL || 'http://localhost:3030',
        trace: 'on-first-retry',
        screenshot: 'only-on-failure',
    },
    projects: [
        {
            name: 'chromium',
            use: { ...devices['Desktop Chrome'] },
        },
    ],
});
