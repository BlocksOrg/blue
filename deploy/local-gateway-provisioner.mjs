#!/usr/bin/env node
import process from "node:process";
import { createHash } from "node:crypto";
import { writeFileSync } from "node:fs";

const PROTOCOL_VERSION = 1;

class ProvisionerError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

function requiredEnv(name) {
  const value = process.env[name]?.trim();
  if (!value) throw new ProvisionerError("invalid_config", `${name} is required`);
  return value;
}

function respond(payload, exitCode = 0) {
  writeFileSync(1, `${JSON.stringify(payload)}\n`);
  process.exit(exitCode);
}

async function readEnvelope() {
  let input = "";
  process.stdin.setEncoding("utf8");
  for await (const chunk of process.stdin) input += chunk;
  try {
    return JSON.parse(input);
  } catch {
    throw new ProvisionerError("invalid_config", "request must be valid JSON");
  }
}

async function litellm(method, path, body, query) {
  const baseUrl = requiredEnv("HARNESS_GATEWAY_URL").replace(/\/$/, "");
  const adminKey = requiredEnv("HARNESS_LITELLM_ADMIN_KEY");
  const url = new URL(`${baseUrl}/${path.replace(/^\//, "")}`);
  for (const [name, value] of Object.entries(query ?? {})) url.searchParams.set(name, value);

  let response;
  try {
    response = await fetch(url, {
      method,
      headers: {
        authorization: `Bearer ${adminKey}`,
        accept: "application/json",
        ...(body === undefined ? {} : { "content-type": "application/json" }),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.timeout(20_000),
    });
  } catch {
    throw new ProvisionerError("unavailable", "LiteLLM request failed");
  }
  if (!response.ok) {
    let errorType;
    try {
      errorType = (await response.clone().json())?.error?.type;
    } catch {}
    if (path === "/key/info" && (response.status === 404 || errorType === "token_not_found_in_db")) {
      throw new ProvisionerError("credential_invalid", "LiteLLM key was deleted");
    }
    throw new ProvisionerError("rejected", `LiteLLM returned HTTP ${response.status}`);
  }
  try {
    return await response.json();
  } catch {
    throw new ProvisionerError("rejected", "LiteLLM returned invalid JSON");
  }
}

async function findUser(email) {
  const payload = await litellm("GET", "/user/list", undefined, {
    user_email: email,
    page_size: "100",
  });
  const users = (Array.isArray(payload.users) ? payload.users : []).filter(
    (user) => typeof user.user_email === "string" && user.user_email.toLowerCase() === email.toLowerCase(),
  );
  if (users.length === 0) throw new ProvisionerError("account_missing", email);
  if (users.length !== 1) {
    throw new ProvisionerError("conflict", `multiple LiteLLM users match ${email}`);
  }

  const user = users[0];
  if (typeof user.user_id !== "string" || !user.user_id.trim()) {
    throw new ProvisionerError("rejected", "LiteLLM user has no user_id");
  }
  const teamId = (Array.isArray(user.teams) ? user.teams : [])
    .map((team) => (typeof team === "string" ? team : team?.team_id ?? team?.id))
    .find((id) => typeof id === "string" && id.trim());
  if (!teamId) {
    throw new ProvisionerError("conflict", `LiteLLM user ${email} has no available teams`);
  }
  return { userId: user.user_id, teamId };
}

async function availableAlias(baseAlias, userId) {
  const payload = await litellm("GET", "/key/list", undefined, {
    user_id: userId,
    return_full_object: "true",
    size: "100",
  });
  const keys = Array.isArray(payload.keys) ? payload.keys : [];
  for (let suffix = 1; suffix <= 100; suffix += 1) {
    const candidate = suffix === 1 ? baseAlias : `${baseAlias}-${suffix}`;
    if (!keys.some((key) => key?.key_alias === candidate)) return candidate;
  }
  throw new ProvisionerError("conflict", `no available LiteLLM key alias for ${baseAlias}`);
}

async function ensure(request) {
  const identity = request?.identity;
  if (!identity || typeof identity.email !== "string" || !identity.email.trim()) {
    throw new ProvisionerError("invalid_config", "identity.email is required");
  }
  const normalizedEmail = identity.email.trim().toLowerCase();
  const { userId, teamId } = await findUser(normalizedEmail);
  const baseAlias = `blue:${normalizedEmail}`;
  const previous = request.previous;
  const alias = previous && !previous.external_id?.startsWith("local:")
    ? previous.alias
    : await availableAlias(baseAlias, userId);
  if (typeof alias !== "string" || !alias.trim()) {
    throw new ProvisionerError("rejected", "previous LiteLLM key has no alias");
  }
  const metadata = {
    managed_by: "blue",
    owner_email: normalizedEmail,
    provisioner: "local-litellm",
  };
  const keyPolicy = {
    user_id: userId,
    team_id: teamId,
    key_alias: alias,
    models: [],
    metadata,
  };
  if (previous && !previous.external_id?.startsWith("local:")) {
    const keyInfo = await litellm("GET", "/key/info", undefined, {
      key: previous.external_id,
    });
    if (keyInfo?.info?.blocked === true || keyInfo?.blocked === true) {
      throw new ProvisionerError("credential_invalid", "LiteLLM key was blocked");
    }
    await litellm("POST", "/key/update", {
      ...keyPolicy,
      key: previous.external_id,
    });
    return {
      credential: null,
      external_id: previous.external_id,
      alias,
      metadata: { team_id: teamId, models: [] },
      expires_at: null,
    };
  }

  const generated = await litellm("POST", "/key/generate", {
    ...keyPolicy,
    key_type: "llm_api",
  });
  if (typeof generated.key !== "string" || !generated.key) {
    throw new ProvisionerError("rejected", "LiteLLM generated no key");
  }
  const externalId =
    typeof generated.token === "string" && generated.token
      ? generated.token
      : createHash("sha256").update(generated.key).digest("hex");
  return {
    credential: generated.key,
    external_id: externalId,
    alias,
    metadata: { team_id: teamId, models: [] },
    expires_at: typeof generated.expires === "string" ? generated.expires : null,
  };
}

async function revoke(request) {
  if (typeof request?.external_id !== "string" || !request.external_id) {
    throw new ProvisionerError("invalid_config", "external_id is required");
  }
  if (!request.external_id.startsWith("local:")) {
    await litellm("POST", "/key/delete", { keys: [request.external_id] });
  }
  return { revoked: true };
}

async function listModels() {
  const payload = await litellm("GET", "/v1/models");
  if (!Array.isArray(payload?.data)) {
    throw new ProvisionerError("discovery_response", "LiteLLM response has no data array");
  }
  const ids = [...new Set(
    payload.data
      .map((model) => typeof model?.id === "string" ? model.id.trim() : "")
      .filter(Boolean),
  )].sort();
  const sourceRevision = createHash("sha256")
    .update(ids.map((id) => `${id}\0`).join(""))
    .digest("hex");
  return {
    models: ids.map((id) => ({ id })),
    source_revision: sourceRevision,
  };
}

try {
  const envelope = await readEnvelope();
  if (envelope.protocol_version !== PROTOCOL_VERSION) {
    throw new ProvisionerError("invalid_config", "unsupported protocol version");
  }
  const result =
    envelope.operation === "list_models"
      ? await listModels()
      : envelope.operation === "ensure"
      ? await ensure(envelope.request)
      : envelope.operation === "revoke"
        ? await revoke(envelope.request)
        : (() => {
            throw new ProvisionerError("invalid_config", "unsupported operation");
          })();
  respond({ protocol_version: PROTOCOL_VERSION, status: "success", result });
} catch (error) {
  const safe =
    error instanceof ProvisionerError
      ? error
      : new ProvisionerError("invalid_config", "provisioner request is invalid");
  respond(
    {
      protocol_version: PROTOCOL_VERSION,
      status: "error",
      error: { code: safe.code, message: safe.message },
    },
    1,
  );
}
