import { defineConfig } from "vitest/config";

// A dedicated config so Vitest runs the pure-logic modules in `src/lib`
// without pulling in the SvelteKit plugin (which needs `.svelte-kit` synced
// and would transform `.svelte` files we don't unit-test here).
export default defineConfig({
  test: {
    include: ["src/**/*.test.ts"],
    environment: "node",
  },
});
