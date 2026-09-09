import "server-only";

export type IdentityConfig =
  | { mode: "password" }
  | {
      mode: "oidc";
      providerId: string;
      providerName: string;
      issuer: string;
      clientId: string;
      clientSecret: string;
    };

function required(name: string): string {
  const value = process.env[name]?.trim();
  if (!value) throw new Error(`${name} is required when HARNESS_AUTH_MODE=oidc`);
  return value;
}

export function identityConfig(): IdentityConfig {
  const mode = (process.env.HARNESS_AUTH_MODE ?? "password").trim().toLowerCase();
  if (mode === "password") return { mode };
  if (mode !== "oidc")
    throw new Error("HARNESS_AUTH_MODE must be password or oidc");
  return {
    mode,
    providerId: process.env.HARNESS_OIDC_PROVIDER_ID?.trim() || "okta",
    providerName: process.env.HARNESS_OIDC_PROVIDER_NAME?.trim() || "Okta",
    issuer: required("HARNESS_OIDC_ISSUER").replace(/\/$/, ""),
    clientId: required("HARNESS_OIDC_CLIENT_ID"),
    clientSecret: required("HARNESS_OIDC_CLIENT_SECRET"),
  };
}

export function bootstrapEmail(): string {
  return (
    process.env.HARNESS_BOOTSTRAP_ADMIN_EMAIL ?? "admin@example.com"
  ).toLowerCase();
}
