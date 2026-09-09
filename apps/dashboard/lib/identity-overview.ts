export type IdentityOverviewInput =
  | { mode: "password" }
  | {
      mode: "oidc";
      providerId: string;
      providerName: string;
      issuer: string;
      clientId: string;
    };

export type ManagedIdentityOverview = {
  providerId: string;
  providerName: string;
  issuer: string;
  clientId: string;
  callbackUrl: string;
  scimBaseUrl: string;
};

export function buildManagedIdentityOverview(
  identity: IdentityOverviewInput,
  dashboardUrl: string,
  controlApiUrl: string,
): ManagedIdentityOverview | null {
  if (identity.mode !== "oidc") return null;

  return {
    providerId: identity.providerId,
    providerName: identity.providerName,
    issuer: identity.issuer,
    clientId: identity.clientId,
    callbackUrl: new URL(
      `/api/auth/callback/${encodeURIComponent(identity.providerId)}`,
      dashboardUrl,
    ).toString(),
    scimBaseUrl: new URL("/scim/v2", controlApiUrl).toString(),
  };
}
