#!/usr/bin/env node
// Test-only executable gateway provisioner for the e2e-slim gateway path.
//
// Speaks control-api's executable-provisioner protocol (JSON request envelope
// on stdin, JSON response envelope on stdout; non-zero exit for error
// envelopes) and provisions REAL credentials against the real in-network
// LiteLLM: it creates the user's LiteLLM account on demand (the one admin
// onboarding step the builtin-litellm provisioner deliberately refuses to do)
// and mints a real virtual key for it. Runs inside the control-api container
// (node:22 base), which supplies:
//   E2E_LITELLM_URL             in-network LiteLLM base URL
//   HARNESS_LITELLM_ADMIN_KEY   LiteLLM master key
//
// The keys grant access to nothing real: the backing LiteLLM is ephemeral and
// only reaches OpenRouter through the CI secret.

const PROTOCOL_VERSION = 1;

const litellmUrl = (process.env.E2E_LITELLM_URL ?? "").replace(/\/+$/, "");
const adminKey = process.env.HARNESS_LITELLM_ADMIN_KEY ?? "";

/// A protocol-level failure with one of the wire error codes control-api
/// understands: invalid_config, account_missing, conflict, credential_invalid,
/// unavailable, rejected.
class WireFailure extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

function fail(code, message) {
  throw new WireFailure(code, message);
}

// Flush the envelope fully before exiting: process.exit() right after a
// write() can truncate piped stdout.
function finish(envelope, exitCode) {
  process.stdout.write(JSON.stringify(envelope), () => process.exit(exitCode));
}

async function litellm(method, path, body) {
  const response = await fetch(`${litellmUrl}${path}`, {
    method,
    headers: {
      authorization: `Bearer ${adminKey}`,
      "content-type": "application/json",
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {
    // Some error responses are not JSON; callers see json === null.
  }
  return { ok: response.ok, status: response.status, json, text };
}

async function findUserId(email) {
  const query = new URLSearchParams({ user_email: email, page_size: "100" });
  const response = await litellm("GET", `/user/list?${query}`);
  if (!response.ok) {
    fail("unavailable", `GET /user/list: ${response.status}: ${response.text}`);
  }
  const users = response.json?.users ?? response.json ?? [];
  for (const user of users) {
    if ((user.user_email ?? "").toLowerCase() === email.toLowerCase()) {
      return user.user_id ?? null;
    }
  }
  return null;
}

async function ensureUserId(email) {
  const existing = await findUserId(email);
  if (existing) {
    return existing;
  }
  const created = await litellm("POST", "/user/new", {
    user_email: email,
    user_role: "internal_user",
  });
  if (created.ok && created.json?.user_id) {
    return created.json.user_id;
  }
  // Tolerate a lost creation race: another ensure may have won.
  const raced = await findUserId(email);
  if (raced) {
    return raced;
  }
  fail("unavailable", `POST /user/new: ${created.status}: ${created.text}`);
}

async function ensure(request) {
  const email = request?.identity?.email;
  if (!email) {
    fail("invalid_config", "ensure request has no identity.email");
  }
  const userId = await ensureUserId(email);
  const generated = await litellm("POST", "/key/generate", {
    user_id: userId,
    metadata: { provisioner: "e2e-slim-executable" },
  });
  const key = generated.json?.key;
  if (!generated.ok || !key) {
    fail(
      "rejected",
      `POST /key/generate: ${generated.status}: ${generated.text}`,
    );
  }
  return {
    credential: key,
    external_id: userId,
    alias: "e2e-slim",
    metadata: { provisioner: "e2e-slim-executable", litellm_user_id: userId },
    expires_at: null,
  };
}

async function revoke(request) {
  // external_id is the LiteLLM user id; delete every key minted for it.
  // Best-effort: a user with no keys (or already-deleted keys) still revokes.
  const userId = request?.external_id;
  if (userId) {
    const query = new URLSearchParams({
      user_id: userId,
      return_full_object: "true",
      size: "100",
    });
    const listed = await litellm("GET", `/key/list?${query}`);
    const keys = (listed.json?.keys ?? [])
      .map((key) => (typeof key === "string" ? key : key?.token))
      .filter(Boolean);
    if (keys.length > 0) {
      await litellm("POST", "/key/delete", { keys });
    }
  }
  return { revoked: true };
}

async function main() {
  if (!litellmUrl) {
    fail("invalid_config", "E2E_LITELLM_URL is not set");
  }
  if (!adminKey) {
    fail("invalid_config", "HARNESS_LITELLM_ADMIN_KEY is not set");
  }
  const stdin = await new Promise((resolve, reject) => {
    let data = "";
    process.stdin.setEncoding("utf8");
    process.stdin.on("data", (chunk) => (data += chunk));
    process.stdin.on("end", () => resolve(data));
    process.stdin.on("error", reject);
  });
  let envelope;
  try {
    envelope = JSON.parse(stdin);
  } catch {
    fail("invalid_config", "request envelope is not valid JSON");
  }
  if (envelope.protocol_version !== PROTOCOL_VERSION) {
    fail(
      "invalid_config",
      `unsupported protocol_version ${envelope.protocol_version}`,
    );
  }
  if (envelope.operation === "ensure") {
    return ensure(envelope.request);
  }
  if (envelope.operation === "revoke") {
    return revoke(envelope.request);
  }
  fail("invalid_config", `unknown operation ${envelope.operation}`);
}

main().then(
  (result) =>
    finish(
      { protocol_version: PROTOCOL_VERSION, status: "success", result },
      0,
    ),
  (error) =>
    finish(
      {
        protocol_version: PROTOCOL_VERSION,
        status: "error",
        error: {
          code: error instanceof WireFailure ? error.code : "unavailable",
          message: String(error?.message ?? error),
        },
      },
      1,
    ),
);
