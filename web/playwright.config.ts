import { defineConfig } from '@playwright/test';
export default defineConfig({ testDir: './e2e', outputDir: '../deploy/dev/reports/playwright', reporter: 'list', use: { baseURL: process.env.YHOTOS_BASE_URL || 'http://127.0.0.1:18011', launchOptions: { executablePath: process.env.CHROME_PATH } } });
