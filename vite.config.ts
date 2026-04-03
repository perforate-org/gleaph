import { defineConfig } from "vite-plus";

export default defineConfig({
  staged: {
    "*.{js,ts,tsx,json,md}": "vp check --fix",
  },
  run: {
    tasks: {
      "sdk:check": {
        command: "vp run @gleaph/sdk#check",
      },
      "sdk:test": {
        command: "vp run @gleaph/sdk#test",
        dependsOn: ["sdk:check"],
      },
      "sdk:build": {
        command: "vp run @gleaph/sdk#build",
        dependsOn: ["sdk:check"],
      },
      "sdk:pack": {
        command: "vp run @gleaph/sdk#pack",
        dependsOn: ["sdk:test", "sdk:build"],
      },
      "product:check": {
        command: "vp run @gleaph/product#check",
      },
      "product:build": {
        command: "vp run @gleaph/product#build",
        dependsOn: ["product:check"],
      },
      "dashboard:check": {
        command: "vp run @gleaph/dashboard#check",
      },
      "dashboard:build": {
        command: "vp run @gleaph/dashboard#build",
        dependsOn: ["dashboard:check"],
      },
    },
  },
});
