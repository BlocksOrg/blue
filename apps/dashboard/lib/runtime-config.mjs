const SAMPLE_VALUES = new Set([
  "development-only-better-auth-secret-change-me",
  "local-development-secret-change-before-production",
  "local-inference-proxy-oauth-secret",
  "e2e-better-auth-secret-at-least-32-bytes",
  "change-me-in-production",
  "change-me-before-testing",
]);

export function isProductionBuild(env = process.env) {
  return env.NEXT_PHASE === "phase-production-build";
}

/** Validate runtime-only dashboard secrets without touching the database.
 * Missing and invalid names are aggregated; secret values are never returned
 * or included in the error. */
export function validateDashboardRuntimeEnv(env = process.env) {
  if (env.BLUE_ENVIRONMENT === "development") return;

  const errors = [];
  const requiredSecrets = [
    "BETTER_AUTH_SECRET",
    "HARNESS_BOOTSTRAP_ADMIN_PASSWORD",
  ];
  if (env.BLUE_GATEWAY_ENABLED === "true") {
    requiredSecrets.push("HARNESS_PROXY_OAUTH_CLIENT_SECRET");
  }
  for (const name of requiredSecrets) {
    const value = env[name];
    if (!value?.trim()) {
      errors.push(`${name} is required`);
    } else if (Buffer.byteLength(value) < 32) {
      errors.push(`${name} must be at least 32 bytes`);
    } else if (SAMPLE_VALUES.has(value)) {
      errors.push(`${name} must not use a repository sample value`);
    }
  }

  if (
    env.BLUE_GATEWAY_ENABLED !== undefined &&
    !["true", "false"].includes(env.BLUE_GATEWAY_ENABLED)
  ) {
    errors.push("BLUE_GATEWAY_ENABLED must be `true` or `false`");
  }

  if (errors.length) {
    throw new Error(
      `Invalid production dashboard environment:\n${errors
        .map((error) => `- ${error}`)
        .join("\n")}\nSet BLUE_ENVIRONMENT=development only for local development.`,
    );
  }
}
