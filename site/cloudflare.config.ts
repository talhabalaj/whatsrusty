import { bindings, defineConfig } from "cf/config";
import * as entrypoint from "./src/index.ts" with { type: "cf-worker" };
export default defineConfig({
  accountId: "dd0a87415ee63e34b183118e8bdd13c8",
  worker: {
    name: "whatsrusty-site", compatibilityDate: "2026-10-15", entrypoint,
    domains: ["whatsrusty.apps.balaj.me"], workersDev: false, previewUrls: false,
    assets: { runWorkerFirst: true },
    env: { ASSETS: bindings.assets() }
  }
});
