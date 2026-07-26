import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  outputDir: "./test-results",
  fullyParallel: true,
  retries: 0,
  reporter: [["list"]],
  use: {
    baseURL: "http://127.0.0.1:4178",
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    colorScheme: "dark",
  },
  projects: [
    {
      name: "desktop",
      use: { ...devices["Desktop Chrome"], viewport: { width: 1440, height: 980 } },
    },
    {
      name: "tablet",
      use: {
        ...devices["Desktop Chrome"],
        viewport: { width: 1024, height: 900 },
        hasTouch: true,
      },
    },
    {
      name: "mobile",
      use: {
        ...devices["Desktop Chrome"],
        viewport: { width: 393, height: 852 },
        hasTouch: true,
        isMobile: true,
      },
    },
  ],
  webServer: {
    command: "npm run dev -- --port 4178",
    url: "http://127.0.0.1:4178",
    reuseExistingServer: true,
    timeout: 120_000,
  },
});
