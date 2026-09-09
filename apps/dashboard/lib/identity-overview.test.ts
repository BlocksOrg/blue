import assert from "node:assert/strict";
import test from "node:test";
import { buildManagedIdentityOverview } from "./identity-overview.ts";

test("builds public OIDC and SCIM endpoints without secret fields", () => {
  assert.deepEqual(
    buildManagedIdentityOverview(
      {
        mode: "oidc",
        providerId: "okta",
        providerName: "Okta",
        issuer: "https://example.okta.com",
        clientId: "blue-dashboard",
      },
      "https://governance.example.com",
      "https://control.example.com",
    ),
    {
      providerId: "okta",
      providerName: "Okta",
      issuer: "https://example.okta.com",
      clientId: "blue-dashboard",
      callbackUrl: "https://governance.example.com/api/auth/callback/okta",
      scimBaseUrl: "https://control.example.com/scim/v2",
    },
  );
});

test("returns no overview for password authentication", () => {
  assert.equal(
    buildManagedIdentityOverview(
      { mode: "password" },
      "https://governance.example.com",
      "https://control.example.com",
    ),
    null,
  );
});
