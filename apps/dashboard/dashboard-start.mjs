import { validateDashboardRuntimeEnv } from "./lib/runtime-config.mjs";

// Validate before importing Next's standalone server, so no listener can be
// opened with missing or repository-known production credentials.
validateDashboardRuntimeEnv();
await import("./server.js");

