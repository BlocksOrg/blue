import assert from "node:assert/strict";
import { test } from "node:test";
import { validateDashboardRuntimeEnv } from "./runtime-config.mjs";

const valid = {
  BETTER_AUTH_SECRET: "a".repeat(32),
  HARNESS_BOOTSTRAP_ADMIN_PASSWORD: "b".repeat(32),
};

test("production validation accepts complete strong secrets", () => {
  assert.doesNotThrow(() => validateDashboardRuntimeEnv(valid));
});

test("an absent environment is production and aggregates redacted failures", () => {
  assert.throws(
    () => validateDashboardRuntimeEnv({}),
    (error: Error) => {
      assert.match(error.message, /BETTER_AUTH_SECRET is required/);
      assert.match(error.message, /HARNESS_BOOTSTRAP_ADMIN_PASSWORD is required/);
      return true;
    },
  );
});

test("production rejects short, blank, and repository sample values", () => {
  for (const value of [
    "",
    "short",
    "development-only-better-auth-secret-change-me",
    "local-development-secret-change-before-production",
    "local-inference-proxy-oauth-secret",
    "e2e-better-auth-secret-at-least-32-bytes",
    "change-me-in-production",
  ]) {
    assert.throws(() =>
      validateDashboardRuntimeEnv({
        ...valid,
        BETTER_AUTH_SECRET: value,
        HARNESS_BOOTSTRAP_ADMIN_PASSWORD: value,
      }),
    );
  }
});

test("development is the only mode that permits local defaults", () => {
  assert.doesNotThrow(() =>
    validateDashboardRuntimeEnv({ BLUE_ENVIRONMENT: "development" }),
  );
  assert.throws(() =>
    validateDashboardRuntimeEnv({ BLUE_ENVIRONMENT: "staging" }),
  );
});

test("gateway mode requires a strong proxy OAuth client secret", () => {
  assert.throws(
    () => validateDashboardRuntimeEnv({ ...valid, BLUE_GATEWAY_ENABLED: "true" }),
    /HARNESS_PROXY_OAUTH_CLIENT_SECRET is required/,
  );
  assert.doesNotThrow(() =>
    validateDashboardRuntimeEnv({
      ...valid,
      BLUE_GATEWAY_ENABLED: "true",
      HARNESS_PROXY_OAUTH_CLIENT_SECRET: "c".repeat(32),
    }),
  );
});

test("gateway mode flag is strict in production", () => {
  assert.throws(
    () => validateDashboardRuntimeEnv({ ...valid, BLUE_GATEWAY_ENABLED: "yes" }),
    /BLUE_GATEWAY_ENABLED must be `true` or `false`/,
  );
});
