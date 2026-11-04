import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests/browser",
  testMatch: "*.spec.mjs",
  fullyParallel: true,
  use: {
    // Separate from the local installer on port 4173.
    baseURL: "http://127.0.0.1:4174/dedibridge/",
    browserName: "chromium",
    headless: true,
    trace: "retain-on-failure",
  },
  webServer: {
    command: "node tests/browser/server.mjs",
    url: "http://127.0.0.1:4174/dedibridge/",
    timeout: 10000,
    reuseExistingServer: false,
  },
});
