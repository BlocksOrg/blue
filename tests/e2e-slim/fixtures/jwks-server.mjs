// Minimal auth sidecar for the e2e-slim stack. Serves two things, both backed by
// the committed, test-only RSA keypair in tests/fixtures/jwt (mounted at ./jwt):
//
//   GET  /jwks         — the public JWK set, so control-api can verify the RS256
//                        user tokens the Rust test minter signs (no dashboard /
//                        Better Auth required).
//
//   POST /oauth2/token — a client-credentials token issuer standing in for the
//                        dashboard's Better Auth OAuth endpoint. Used ONLY by the
//                        gateway path (docker-compose.gateway.yml): the
//                        inference-proxy fetches an M2M service token here to call
//                        control-api's internal gateway resolver. The returned JWT
//                        is signed with the SAME private key as the user tokens
//                        (control-api verifies both against the one served JWKS),
//                        with claims control-api's service-token check requires:
//                        sub = client_id = azp = <client_id>, scope contains
//                        `gateway:resolve`, and iss/aud byte-matching control-api.
import { createServer } from "node:http";
import { readFileSync } from "node:fs";
import { createPrivateKey, createSign } from "node:crypto";

const jwks = readFileSync(new URL("./jwt/jwks.json", import.meta.url));
const signingKey = createPrivateKey(
  readFileSync(new URL("./jwt/signing-key.pem", import.meta.url)),
);
const port = Number(process.env.PORT || 8080);

// These MUST byte-match the constants in src/lib.rs (KID / ISSUER / AUDIENCE) and
// control-api's HARNESS_AUTH_* env; the gateway overlay passes them through.
const kid = process.env.E2E_TOKEN_KID || "e2e-slim-rsa-1";
const issuer = process.env.E2E_TOKEN_ISSUER || "https://e2e-slim.blue.test/";
const defaultAudience =
  process.env.E2E_TOKEN_AUDIENCE || "https://control-api.e2e-slim.blue.test";
const defaultClientId = process.env.E2E_TOKEN_CLIENT_ID || "blue-inference-proxy";

function base64url(input) {
  return Buffer.from(input).toString("base64url");
}

// Sign an RS256 JWT with the committed test key. control-api verifies it against
// the same JWKS this sidecar serves (kid match), pinning issuer + audience.
function mintServiceToken({ clientId, scope, audience }) {
  const now = Math.floor(Date.now() / 1000);
  const header = { alg: "RS256", typ: "JWT", kid };
  const payload = {
    sub: clientId,
    client_id: clientId,
    azp: clientId,
    scope,
    iss: issuer,
    aud: audience,
    iat: now,
    exp: now + 3600,
  };
  const signingInput = `${base64url(JSON.stringify(header))}.${base64url(
    JSON.stringify(payload),
  )}`;
  const signature = createSign("RSA-SHA256")
    .update(signingInput)
    .sign(signingKey)
    .toString("base64url");
  return `${signingInput}.${signature}`;
}

function readBody(request) {
  return new Promise((resolve) => {
    let data = "";
    request.on("data", (chunk) => {
      data += chunk;
    });
    request.on("end", () => resolve(data));
  });
}

createServer(async (request, response) => {
  const url = (request.url || "").split("?")[0];

  if (request.method === "POST" && url === "/oauth2/token") {
    // The inference-proxy authenticates with client_secret_basic and posts a
    // urlencoded client-credentials grant (grant_type / scope / resource). We
    // don't gate on the secret — this issuer only ever mints throwaway tokens
    // for the throwaway stack — but we mirror the real claims exactly.
    const body = await readBody(request);
    const form = new URLSearchParams(body);
    const auth = request.headers.authorization || "";
    let clientId = defaultClientId;
    if (auth.startsWith("Basic ")) {
      const decoded = Buffer.from(auth.slice("Basic ".length), "base64").toString();
      clientId = decoded.split(":")[0] || defaultClientId;
    }
    const scope = form.get("scope") || "gateway:resolve";
    const audience = form.get("resource") || defaultAudience;
    const token = mintServiceToken({ clientId, scope, audience });
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify({ access_token: token, token_type: "Bearer", expires_in: 3600 }));
    return;
  }

  if (url === "/jwks" || url === "/") {
    response.writeHead(200, { "content-type": "application/json" });
    response.end(jwks);
    return;
  }
  if (url === "/health") {
    response.writeHead(200, { "content-type": "text/plain" });
    response.end("ok\n");
    return;
  }
  response.writeHead(404);
  response.end();
}).listen(port, "0.0.0.0", () => {
  console.log(`jwks-server listening on :${port}`);
});
