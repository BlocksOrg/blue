import { bootstrapEmail, identityConfig } from "../../../lib/identity-config";

export async function POST(request: Request) {
  const body = await request.json().catch(() => ({}));
  const email = String(body.email ?? "").trim().toLowerCase();
  const identity = identityConfig();
  if (identity.mode === "password" || email === bootstrapEmail())
    return Response.json({ method: "password" });
  return Response.json({
    method: "oidc",
    providerId: identity.providerId,
    providerName: identity.providerName,
  });
}
