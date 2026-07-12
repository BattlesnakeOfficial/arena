import { defineConfig, devices } from '@playwright/test';

const PORT = process.env.PORT || '3000';
const MOCK_GITHUB_PORT = process.env.MOCK_GITHUB_PORT || '8081';
const BASE_URL = `http://localhost:${PORT}`;
const MOCK_GITHUB_URL = `http://localhost:${MOCK_GITHUB_PORT}`;
const DATABASE_URL = process.env.DATABASE_URL || 'postgresql://localhost:5432/arena_test';

export default defineConfig({
  testDir: './tests',
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 2 : 0,
  workers: process.env.CI ? 1 : undefined,
  reporter: 'html',
  use: {
    baseURL: BASE_URL,
    trace: 'on-first-retry',
  },

  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
  ],

  webServer: [
    // Mock GitHub OAuth server
    {
      command: 'cargo run -p mock-github-oauth',
      cwd: '..',
      port: parseInt(MOCK_GITHUB_PORT),
      reuseExistingServer: !process.env.CI,
      timeout: 120 * 1000,
      env: {
        MOCK_GITHUB_PORT: MOCK_GITHUB_PORT,
      },
    },
    // Main application server
    {
      command: 'cargo run -p arena',
      cwd: '..',
      url: BASE_URL,
      reuseExistingServer: !process.env.CI,
      timeout: 120 * 1000,
      env: {
        RUST_BACKTRACE: '1',
        RUST_LOG: 'arena=debug',
        DATABASE_URL: DATABASE_URL,
        // Configure app to use mock OAuth server
        GITHUB_OAUTH_URL: `${MOCK_GITHUB_URL}/login/oauth/authorize`,
        GITHUB_TOKEN_URL: `${MOCK_GITHUB_URL}/login/oauth/access_token`,
        GITHUB_API_URL: MOCK_GITHUB_URL,
        // Dummy credentials (mock server doesn't validate these)
        GITHUB_CLIENT_ID: 'mock_client_id',
        GITHUB_CLIENT_SECRET: 'mock_client_secret',
        GITHUB_REDIRECT_URI: `${BASE_URL}/auth/github/callback`,
        // Faster job polling for e2e tests (default is 60 seconds)
        JOB_POLL_INTERVAL_SECS: '2',
        // Disable the homepage feed cache so seeded data is visible immediately
        HOME_FEED_CACHE_SECS: '0',
      },
    },
  ],
});
