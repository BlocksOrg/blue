import { validateDashboardRuntimeEnv } from "./lib/runtime-config.mjs";

/** Next invokes this hook when the server runtime starts, but not while
 * producing the standalone build. Throwing here prevents the listener from
 * serving with development credentials. */
export function register() {
  validateDashboardRuntimeEnv();
}
