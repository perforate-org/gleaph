import { defineConfig } from "vite-plus";

export default defineConfig({
  staged: {
    "*.{js,ts,tsx,json,md}": "vp check --fix",
  },
  run: {
    tasks: {},
  },
});
