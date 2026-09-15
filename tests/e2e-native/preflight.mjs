import { pathToFileURL } from "node:url";

export function validateAwsConfiguration(env, { ci = false } = {}) {
  const required = [
    "E2E_NATIVE_BUCKET",
    "E2E_NATIVE_SUBNET_ID",
    "E2E_NATIVE_SECURITY_GROUP_ID",
    "E2E_NATIVE_INSTANCE_PROFILE",
    "E2E_NATIVE_AMI_ID",
  ];
  if (ci) required.push("E2E_NATIVE_REGION", "E2E_NATIVE_ROLE_ARN");
  const missing = required.filter((name) => !env[name]?.trim());
  if (missing.length)
    throw new Error(
      `Missing AWS backend configuration: ${missing.join(", ")}. See tests/e2e-native/infra/README.md for setup and GitHub environment variables.`,
    );
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  try {
    validateAwsConfiguration(process.env, {
      ci: process.argv.includes("--ci"),
    });
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
