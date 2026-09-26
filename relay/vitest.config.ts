import { cloudflareTest } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [
    cloudflareTest({
      wrangler: { configPath: "./wrangler.jsonc" },
      miniflare: {
        bindings: {
          RELAY_SECRET: "test-relay-secret-0123456789abcdef0123456789abcdef",
          KOFI_VERIFICATION_TOKEN: "kofi-test-token-3f6c",
          KOFI_BUFFER_MAX: "3",
          QUEUE_TITLE: "Drums & <Requests>",
        },
      },
    }),
  ],
  test: {
    include: ["test/**/*.test.ts"],
  },
});
