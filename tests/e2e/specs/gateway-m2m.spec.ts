import {
  expect,
  test,
  type APIRequestContext,
  type Browser,
  type BrowserContext,
  type Page,
} from "@playwright/test";
import { readFile } from "node:fs/promises";
import http from "node:http";
import https from "node:https";
import YAML from "yaml";
import { collect, prepareClient, readClientFile, runCli, spawnCli, waitForOutput } from "../support/cli.js";
import { loginAsAdmin } from "../support/dashboard.js";
import { enableGateway } from "../support/governance.js";

// The inference proxy authenticates to the Control API's internal resolver with
// a short-lived OAuth2 client-credentials (M2M) token that it caches and
// refreshes in-process. The e2e stack sets a ~3s access-token TTL and a 1s proxy
// cache TTL, so sustained load crosses several token rollovers. These tests
// prove the proxy keeps serving 200s across refreshes and that a burst collapses
// into a single token fetch (cache + single-flight), rather than one fetch per
// request or an auth failure during rollover.

const PROXY = "http://127.0.0.1:8081";
const UPSTREAM = "http://fake-upstream:4010";
const CONTROL = process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080";
const DASHBOARD = process.env.E2E_DASHBOARD_URL ?? "http://127.0.0.1:3000";

function parseMetric(body: string, name: string): number {
  const match = body.match(new RegExp(`^${name}\\s+(\\d+)`, "m"));
  expect(match, `metric ${name} present in:\n${body}`).toBeTruthy();
  return Number(match![1]);
}

function jwtClaims(token: string): Record<string, unknown> {
  return JSON.parse(
    Buffer.from(token.split(".")[1], "base64url").toString("utf8"),
  );
}

async function signInMember(
  browser: Browser,
  email: string,
): Promise<{ context: BrowserContext; page: Page }> {
  const context = await browser.newContext();
  const page = await context.newPage();
  await page.goto(`${DASHBOARD}/login`, { waitUntil: "commit" });
  await page.getByLabel("Email").fill(email);
  await page.getByRole("button", { name: "Continue" }).click();
  await page.getByLabel("Password").fill("member-password-e2e");
  await Promise.all([
    page.waitForURL(/\/sessions/, { waitUntil: "commit", timeout: 30_000 }),
    page.getByRole("button", { name: "Sign in" }).click({ noWaitAfter: true }),
  ]);
  return { context, page };
}

async function createMember(
  admin: Page,
  browser: Browser,
): Promise<{ email: string; userId: string }> {
  const email = `gateway-revocation-${Date.now()}@example.com`;
  const invited = await admin.request.post(`${CONTROL}/admin/invitations`, {
    data: { email, role: "member" },
  });
  expect(invited.status(), await invited.text()).toBe(201);
  const invitation = await invited.json();
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    await page.goto(`${DASHBOARD}/accept-invitation?id=${invitation.id}`);
    await page.getByLabel("Password").fill("member-password-e2e");
    await page.getByRole("button", { name: "Create account" }).click();
    await expect(page).toHaveURL(/\/sessions/);
    const me = await context.request.get(`${CONTROL}/auth/me`);
    expect(me.status(), await me.text()).toBe(200);
    return { email, userId: (await me.json()).id };
  } finally {
    await context.close();
  }
}

async function mintInferenceToken(
  home: string,
  page: Page,
  args: string[] = ["login"],
): Promise<string> {
  const login = spawnCli(home, args);
  const deviceUrl = await waitForOutput(
    login,
    /http:\/\/127\.0\.0\.1:3000\/device\/[A-Za-z0-9_-]+/,
  );
  await page.goto(deviceUrl);
  await page.getByRole("button", { name: "Authorize" }).click();
  await expect(page.getByText("CLI authorized")).toBeVisible();
  const loginResult = await collect(login);
  expect(loginResult.code, loginResult.stderr).toBe(0);

  const gateway = await runCli(home, ["gateway"]);
  expect(gateway.code, gateway.stderr).toBe(0);
  const session = JSON.parse(
    await readClientFile(home, ".config/blue/session.json"),
  );
  expect(jwtClaims(session.token).sid).toEqual(expect.any(String));
  const config = await page.request.get(`${CONTROL}/governance-config`, {
    headers: {
      authorization: `Bearer ${session.token}`,
      "x-blue-contract-version": "4",
      "x-blue-capabilities":
        "adapter_intervals,compiled_harness_registry,transactional_reconcile,versioned_state,gateway_inference_jwt,gateway_model_catalog,tenant_client_version_pin",
    },
  });
  expect(config.status(), await config.text()).toBe(200);
  const token = (await config.json()).gateway?.token ?? "";
  expect(token).toBeTruthy();
  return token;
}

async function inferenceStatus(
  request: APIRequestContext,
  token: string,
): Promise<number> {
  return (
    await request.post(`${PROXY}/v1/chat/completions`, {
      headers: { authorization: `Bearer ${token}` },
      data: {
        model: "gpt-e2e",
        messages: [{ role: "user", content: "revocation-check" }],
      },
    })
  ).status();
}

async function expectRevoked(
  request: APIRequestContext,
  token: string,
  invalidationsBefore: number,
): Promise<void> {
  await expect
    .poll(async () =>
      parseMetric(
        await (await request.get(`${PROXY}/metrics`)).text(),
        "gateway_proxy_invalidation_events_total",
      ),
    )
    .toBeGreaterThan(invalidationsBefore);
  await expect
    .poll(() => inferenceStatus(request, token), {
      timeout: 10_000,
      intervals: [50, 100, 250],
    })
    .toBe(401);
}

type InternalResult = { status?: number; error?: string };

async function callInternal(options: {
  cert?: Buffer;
  key?: Buffer;
  authorization?: string;
  plaintext?: boolean;
  path?: string;
  method?: "GET" | "POST";
}): Promise<InternalResult> {
  const transport = options.plaintext ? http : https;
  const ca = options.plaintext ? undefined : await readFile("/certs/ca.crt");
  return new Promise((resolve) => {
    const request = transport.request(
      {
        hostname: "127.0.0.1",
        port: 8082,
        path: options.path ?? "/internal/gateway/resolve",
        method: options.method ?? "POST",
        ca,
        cert: options.cert,
        key: options.key,
        rejectUnauthorized: true,
        headers: {
          "content-type": "application/json",
          ...(options.authorization ? { authorization: options.authorization } : {}),
        },
      },
      (response) => {
        response.resume();
        response.on("end", () => resolve({ status: response.statusCode }));
      },
    );
    request.once("error", (error) => resolve({ error: `${(error as NodeJS.ErrnoException).code ?? ""} ${error.message}` }));
    request.end(options.method === "GET" ? undefined : JSON.stringify({ user_id: "00000000-0000-0000-0000-000000000000", blue_oauth_session_id: "not-a-real-session" }));
  });
}

async function serviceToken(request: APIRequestContext): Promise<string> {
  const credentials = Buffer.from(
    "blue-inference-proxy:e2e-proxy-oauth-secret",
  ).toString("base64");
  const response = await request.post(`${DASHBOARD}/api/auth/oauth2/token`, {
    headers: { authorization: `Basic ${credentials}`, origin: DASHBOARD },
    form: {
      grant_type: "client_credentials",
      scope: "gateway:resolve",
      resource: CONTROL,
    },
  });
  expect(response.status(), await response.text()).toBe(200);
  return (await response.json()).access_token as string;
}

test.describe.serial("Gateway M2M auth", () => {
  let home: string;
  let inferenceToken: string;

  test.beforeAll(async () => {
    home = await prepareClient("gateway-m2m");
  });

  test("mints an inference JWT and uses the M2M-authenticated resolver", async ({ page }) => {
    await loginAsAdmin(page);
    const currentResponse = await page.request.get(`${CONTROL}/admin/governance-config`);
    expect(currentResponse.status(), await currentResponse.text()).toBe(200);
    const current = await currentResponse.json();
    const managedConfig = YAML.parse(String(current.managed_yaml));
    enableGateway(managedConfig);
    const managedYaml = YAML.stringify(managedConfig);
    const updateResponse = await page.request.put(`${CONTROL}/admin/governance-config`, {
      data: { base_revision: current.revision, managed_yaml: managedYaml },
    });
    expect(updateResponse.status(), await updateResponse.text()).toBe(200);

    const login = spawnCli(home, ["login"]);
    const deviceUrl = await waitForOutput(login, /http:\/\/127\.0\.0\.1:3000\/device\/[A-Za-z0-9_-]+/);
    await page.goto(deviceUrl);
    await page.getByRole("button", { name: "Authorize" }).click();
    await expect(page.getByText("CLI authorized")).toBeVisible();
    expect((await collect(login)).code).toBe(0);

    const gateway = await runCli(home, ["gateway"]);
    expect(gateway.code, gateway.stderr).toBe(0);
    expect(gateway.stdout).toContain("status          : ready");

    // Provisioning changes the server-authored runtime policy. Reconcile once
    // more so this isolated client receives the newly minted inference JWT.
    const preferred = await runCli(home, ["agent", "codex"]);
    expect(preferred.code, preferred.stderr).toBe(0);
    const applied = await runCli(home, ["apply", "--yes"]);
    expect(applied.code, applied.stderr).toBe(0);

    // Fetch the same personalized response the CLI consumes. Runtime gateway
    // credentials are intentionally not persisted in Blue's config cache.
    const session = JSON.parse(await readClientFile(home, ".config/blue/session.json"));
    const accessClaims = JSON.parse(
      Buffer.from(session.token.split(".")[1], "base64url").toString("utf8"),
    );
    expect(accessClaims.sid).toEqual(expect.any(String));
    const configResponse = await page.request.get(
      `${CONTROL}/governance-config`,
      {
        headers: {
          authorization: `Bearer ${session.token}`,
          "x-blue-contract-version": "4",
          "x-blue-capabilities":
            "adapter_intervals,compiled_harness_registry,transactional_reconcile,versioned_state,gateway_inference_jwt,gateway_model_catalog,tenant_client_version_pin",
        },
      },
    );
    expect(configResponse.status(), await configResponse.text()).toBe(200);
    inferenceToken = (await configResponse.json()).gateway?.token ?? "";
    expect(inferenceToken).toBeTruthy();

    // Sanity: an authorized request resolves through the M2M-secured hop.
    const ok = await page.request.post(`${PROXY}/v1/chat/completions`, {
      headers: { authorization: `Bearer ${inferenceToken}` },
      data: { model: "gpt-e2e", messages: [{ role: "user", content: "hello" }] },
    });
    expect(ok.status(), await ok.text()).toBe(200);
  });

  test("preserves deterministic upstream failures without leaking transport metadata", async ({ page }) => {
    await loginAsAdmin(page);
    expect(inferenceToken).toBeTruthy();
    await page.request.post(`${UPSTREAM}/_e2e/reset`);
    const marker = `private-prompt-${Date.now()}`;
    const querySecret = `query-secret-${Date.now()}`;

    for (const [status, retryAfter] of [[400, undefined], [429, "11"], [500, undefined]] as const) {
      const response = await page.request.post(
        `${PROXY}/v1/e2e/status/${status}?api_key=${querySecret}`,
        {
          headers: {
            authorization: `Bearer ${inferenceToken}`,
            connection: "x-e2e-request-hop",
            "x-e2e-request-hop": "must-not-reach-upstream",
            "x-e2e-end-to-end": "preserved",
            "x-harness-agent": "codex",
          },
          data: { model: "gpt-e2e", messages: [{ role: "user", content: marker }] },
        },
      );
      expect(response.status()).toBe(status);
      expect(await response.json()).toEqual({ error: { message: `e2e upstream ${status}` } });
      expect(response.headers()["x-e2e-end-to-end"]).toBe("preserved");
      expect(response.headers()["x-e2e-upstream-hop"]).toBeUndefined();
      expect(response.headers()["connection"]).toBeUndefined();
      expect(response.headers()["retry-after"]).toBe(retryAfter);
    }

    const malformed = await page.request.post(`${PROXY}/v1/e2e/malformed-json`, {
      headers: { authorization: `Bearer ${inferenceToken}` },
      data: { model: "gpt-e2e" },
    });
    expect(malformed.status()).toBe(200);
    expect(await malformed.text()).toBe('{"incomplete":');

    const disconnected = await page.request.post(`${PROXY}/v1/e2e/disconnect`, {
      headers: { authorization: `Bearer ${inferenceToken}` },
      data: { model: "gpt-e2e" },
    });
    expect(disconnected.status()).toBe(502);
    expect(await disconnected.text()).toBe("upstream request failed");

    const interrupted = await page.request.post(`${PROXY}/v1/e2e/interrupted-sse`, {
      headers: { authorization: `Bearer ${inferenceToken}` },
      data: { model: "gpt-e2e", stream: true },
    });
    expect(interrupted.status()).toBe(502);
    expect(await interrupted.text()).toBe("upstream request failed");

    const upstreamRequests = await (await page.request.get(`${UPSTREAM}/_e2e/requests`)).json();
    const forwarded = upstreamRequests.find((item: { path: string }) => item.path === "/v1/e2e/status/400");
    expect(forwarded.headers["x-e2e-request-hop"]).toBeUndefined();
    expect(forwarded.headers["connection"]).toBeUndefined();
    expect(forwarded.headers["x-e2e-end-to-end"]).toBe("preserved");
    expect(forwarded.authorization).not.toBe(inferenceToken);

    await expect.poll(async () => {
      const response = await page.request.get(`${CONTROL}/gateway/request-logs?per_page=50`);
      const items = (await response.json()).items as Array<{ path: string; http_status: number | null }>;
      return items.filter((item) => item.path.startsWith("/v1/e2e/"));
    }).toEqual(expect.arrayContaining([
      expect.objectContaining({ path: "/v1/e2e/status/400", http_status: 400 }),
      expect.objectContaining({ path: "/v1/e2e/status/429", http_status: 429 }),
      expect.objectContaining({ path: "/v1/e2e/status/500", http_status: 500 }),
      expect.objectContaining({ path: "/v1/e2e/disconnect", http_status: null }),
      expect.objectContaining({ path: "/v1/e2e/interrupted-sse", http_status: null }),
    ]));
    const logs = await (await page.request.get(`${CONTROL}/gateway/request-logs?per_page=50`)).text();
    expect(logs).not.toContain(inferenceToken);
    expect(logs).not.toContain(marker);
    expect(logs).not.toContain(querySecret);
    expect(logs).not.toContain("api_key");
  });

  test("enforces TLS, the trusted proxy certificate, and OAuth independently", async ({ page }) => {
    const [clientCert, clientKey, rogueCert, rogueKey] = await Promise.all([
      readFile("/certs/client.crt"),
      readFile("/certs/client.key"),
      readFile("/certs/rogue-client.crt"),
      readFile("/certs/rogue-client.key"),
    ]);

    // The internal listener is TLS-only; plaintext cannot reach HTTP routing.
    const plaintext = await callInternal({ plaintext: true, authorization: "Bearer arbitrary" });
    expect(plaintext.status).toBeUndefined();
    expect(plaintext.error).toBeTruthy();

    // A bearer token alone is rejected during the TLS handshake.
    const noCertificate = await callInternal({ authorization: "Bearer arbitrary" });
    expect(noCertificate.status).toBeUndefined();
    expect(noCertificate.error).toBeTruthy();

    // A client certificate from another CA is also rejected before routing.
    const untrustedCertificate = await callInternal({
      cert: rogueCert,
      key: rogueKey,
      authorization: "Bearer arbitrary",
    });
    expect(untrustedCertificate.status).toBeUndefined();
    expect(untrustedCertificate.error).toBeTruthy();

    // The trusted workload certificate gets through TLS, but it cannot replace
    // OAuth M2M authorization at the application boundary.
    const noOauth = await callInternal({ cert: clientCert, key: clientKey });
    expect(noOauth.error).toBeUndefined();
    expect(noOauth.status).toBe(401);

    const noOauthJwks = await callInternal({
      cert: clientCert,
      key: clientKey,
      path: "/internal/gateway/jwks",
      method: "GET",
    });
    expect(noOauthJwks.error).toBeUndefined();
    expect(noOauthJwks.status).toBe(401);

    const token = await serviceToken(page.request);
    const trustedJwks = await callInternal({
      cert: clientCert,
      key: clientKey,
      authorization: `Bearer ${token}`,
      path: "/internal/gateway/jwks",
      method: "GET",
    });
    expect(trustedJwks.error).toBeUndefined();
    expect(trustedJwks.status).toBe(200);
  });

  test("serves sustained load across token refreshes without auth failures", async ({ page }) => {
    expect(inferenceToken).toBeTruthy();
    await page.request.post(`${UPSTREAM}/_e2e/reset`);

    const beforeMetrics = await (await page.request.get(`${PROXY}/metrics`)).text();
    const before = parseMetric(beforeMetrics, "gateway_proxy_oauth_token_fetches_total");
    const fetchErrorsBefore = parseMetric(beforeMetrics, "gateway_proxy_oauth_token_fetch_errors_total");

    const send = () =>
      page.request
        .post(`${PROXY}/v1/chat/completions`, {
          headers: { authorization: `Bearer ${inferenceToken}` },
          data: { model: "gpt-e2e", messages: [{ role: "user", content: "load" }] },
        })
        .then((response) => response.status());

    // ~12s of sustained waves ≈ 4 access-token TTL cycles (3s each).
    const statuses: number[] = [];
    const deadline = Date.now() + 12_000;
    while (Date.now() < deadline) {
      statuses.push(...(await Promise.all(Array.from({ length: 25 }, send))));
    }

    const metricsBody = await (await page.request.get(`${PROXY}/metrics`)).text();
    const after = parseMetric(metricsBody, "gateway_proxy_oauth_token_fetches_total");
    const fetchErrors = parseMetric(metricsBody, "gateway_proxy_oauth_token_fetch_errors_total");

    // No request failed while tokens were rolling over.
    expect(statuses.length).toBeGreaterThan(100);
    expect(statuses.filter((status) => status !== 200)).toEqual([]);

    // Token fetches grew ~once per TTL, not once per request: proves the proxy
    // caches and refreshes rather than minting a token per resolve.
    const fetched = after - before;
    expect(fetched).toBeGreaterThanOrEqual(1);
    expect(fetched).toBeLessThanOrEqual(12);
    expect(fetched).toBeLessThan(statuses.length / 10);
    expect(fetchErrors).toBe(fetchErrorsBefore);
  });

  test("collapses a concurrent burst into a single-flight token fetch", async ({ page }) => {
    expect(inferenceToken).toBeTruthy();

    const before = parseMetric(await (await page.request.get(`${PROXY}/metrics`)).text(), "gateway_proxy_oauth_token_fetches_total");

    // Fire a large concurrent burst. Even if it straddles a token rollover, the
    // single-flight guard funnels it into at most one fetch (not one per call).
    const statuses = await Promise.all(
      Array.from({ length: 200 }, () =>
        page.request
          .post(`${PROXY}/v1/chat/completions`, {
            headers: { authorization: `Bearer ${inferenceToken}` },
            data: { model: "gpt-e2e", messages: [{ role: "user", content: "burst" }] },
          })
          .then((response) => response.status()),
      ),
    );

    const after = parseMetric(await (await page.request.get(`${PROXY}/metrics`)).text(), "gateway_proxy_oauth_token_fetches_total");

    expect(statuses.filter((status) => status !== 200)).toEqual([]);
    // At most one rollover can fall inside a burst this short.
    expect(after - before).toBeLessThanOrEqual(2);
  });

  test("restores gateway access when the CLI logs back in from a live browser session", async ({ page, browser }) => {
    test.setTimeout(120_000);
    await loginAsAdmin(page);
    const member = await createMember(page, browser);

    // One browser context for the whole test. The revocation test above calls
    // browser.newContext() per iteration, so it gets a fresh sid every time —
    // which is exactly why this bug went unnoticed.
    const login = await signInMember(browser, member.email);
    try {
      const home = await prepareClient(`gateway-reactivation-${Date.now()}`);
      const first = await mintInferenceToken(home, login.page);
      expect(await inferenceStatus(login.context.request, first)).toBe(200);
      const invalidations = parseMetric(
        await (await login.context.request.get(`${PROXY}/metrics`)).text(),
        "gateway_proxy_invalidation_events_total",
      );

      const logout = await runCli(home, ["logout"]);
      expect(logout.code, logout.stderr).toBe(0);
      await expectRevoked(login.context.request, first, invalidations);

      // Log back in from the same signed-in browser: no new browser session.
      const second = await mintInferenceToken(home, login.page);
      // Without this, a future Better Auth change that mints a new sid would
      // make the rest of the test pass vacuously.
      expect(jwtClaims(second).blue_oauth_session_id).toBe(
        jwtClaims(first).blue_oauth_session_id,
      );

      await expect
        .poll(() => inferenceStatus(login.context.request, second), {
          timeout: 10_000,
          intervals: [50, 100, 250],
        })
        .toBe(200);
      // The pre-logout token stays dead: session_not_before.
      expect(await inferenceStatus(login.context.request, first)).toBe(401);
    } finally {
      await login.context.close();
    }
  });

  test("retires inference access on a forced re-login, not just the refresh grant", async ({ page, browser }) => {
    test.setTimeout(120_000);
    await loginAsAdmin(page);
    const member = await createMember(page, browser);

    // One browser context, as above: `--force` from a still signed-in browser
    // reuses the sid, so nothing *else* can invalidate the old JWT. That is the
    // case `--force` is documented for — replacing a session you no longer
    // trust — and the one where revoking only the refresh grant left the
    // already-minted token buying inference for its full 12h TTL.
    const login = await signInMember(browser, member.email);
    try {
      const home = await prepareClient(`gateway-forced-relogin-${Date.now()}`);
      const first = await mintInferenceToken(home, login.page);
      expect(await inferenceStatus(login.context.request, first)).toBe(200);
      const invalidations = parseMetric(
        await (await login.context.request.get(`${PROXY}/metrics`)).text(),
        "gateway_proxy_invalidation_events_total",
      );

      const second = await mintInferenceToken(home, login.page, [
        "login",
        "--force",
      ]);
      expect(jwtClaims(second).blue_oauth_session_id).toBe(
        jwtClaims(first).blue_oauth_session_id,
      );

      // The forced re-login must not have cost the user working access.
      await expect
        .poll(() => inferenceStatus(login.context.request, second), {
          timeout: 10_000,
          intervals: [50, 100, 250],
        })
        .toBe(200);
      await expectRevoked(login.context.request, first, invalidations);
    } finally {
      await login.context.close();
    }
  });

  test("fails closed and never reinstalls a stale inference token after the browser session ends", async ({ page, browser }) => {
    test.setTimeout(120_000);
    await loginAsAdmin(page);
    const member = await createMember(page, browser);
    const login = await signInMember(browser, member.email);
    try {
      const home = await prepareClient(`gateway-fail-closed-${Date.now()}`);
      const token = await mintInferenceToken(home, login.page);
      expect(await inferenceStatus(login.context.request, token)).toBe(200);
      const preferred = await runCli(home, ["agent", "codex"]);
      expect(preferred.code, preferred.stderr).toBe(0);
      const applied = await runCli(home, ["apply", "--yes"]);
      expect(applied.code, applied.stderr).toBe(0);

      // The runtime credential must never reach the on-disk cache: it is what
      // used to get reinstalled into agent config after the proxy 401'd.
      const cached = JSON.parse(
        await readClientFile(home, ".cache/blue/governance-config.json"),
      );
      expect(cached.config.gateway?.type).toBe("litellm");
      expect(cached.config.gateway?.token).toBeUndefined();
      expect(cached.config.gateway?.proxy_url).toBeUndefined();

      const signOut = await login.context.request.post(
        `${DASHBOARD}/api/auth/sign-out`,
        { headers: { origin: DASHBOARD }, data: {} },
      );
      expect(signOut.status(), await signOut.text()).toBe(200);

      // The session file is still on disk and its access token still refreshes,
      // so only a live check can tell the truth here.
      await expect
        .poll(async () => (await runCli(home, ["doctor"])).stdout, {
          timeout: 30_000,
          intervals: [250, 500, 1_000],
        })
        .toContain("session       : EXPIRED (run `blue login`)");

      // A gateway launch must refuse rather than fall back to a cached config
      // holding no usable token — or, worse, to the user's own provider keys.
      const launched = await runCli(home, ["run", "codex", "--", "after-signout"]);
      expect(launched.code).not.toBe(0);
      expect(`${launched.stdout}${launched.stderr}`).toContain("blue login");
    } finally {
      await login.context.close();
    }
  });

  test("revokes cached inference access for every session and user lifecycle path", async ({ page, browser }) => {
    test.setTimeout(240_000);
    await loginAsAdmin(page);
    const member = await createMember(page, browser);
    let sequence = 0;

    const memberLogin = async () => {
      const login = await signInMember(browser, member.email);
      const home = await prepareClient(`gateway-revocation-${sequence++}`);
      const token = await mintInferenceToken(home, login.page);
      expect(await inferenceStatus(login.context.request, token)).toBe(200);
      const metrics = await (await login.context.request.get(`${PROXY}/metrics`)).text();
      const invalidations = parseMetric(
        metrics,
        "gateway_proxy_invalidation_events_total",
      );
      return { ...login, home, token, invalidations };
    };

    const cliLogout = await memberLogin();
    const logout = await runCli(cliLogout.home, ["logout"]);
    expect(logout.code, logout.stderr).toBe(0);
    await expectRevoked(
      cliLogout.context.request,
      cliLogout.token,
      cliLogout.invalidations,
    );
    await cliLogout.context.close();

    const browserDeletion = await memberLogin();
    const signOut = await browserDeletion.context.request.post(
      `${DASHBOARD}/api/auth/sign-out`,
      { headers: { origin: DASHBOARD }, data: {} },
    );
    expect(signOut.status(), await signOut.text()).toBe(200);
    await expectRevoked(
      browserDeletion.context.request,
      browserDeletion.token,
      browserDeletion.invalidations,
    );
    await browserDeletion.context.close();

    const adminRevocation = await memberLogin();
    const revoked = await page.request.post(
      `${CONTROL}/admin/users/${member.userId}/sessions/revoke`,
    );
    expect(revoked.status(), await revoked.text()).toBe(204);
    await expectRevoked(
      adminRevocation.context.request,
      adminRevocation.token,
      adminRevocation.invalidations,
    );
    await adminRevocation.context.close();

    const suspension = await memberLogin();
    const concurrent = Array.from({ length: 40 }, async (_, index) => {
      await new Promise((resolve) => setTimeout(resolve, (index % 8) * 15));
      return inferenceStatus(suspension.context.request, suspension.token);
    });
    const suspended = await page.request.patch(
      `${CONTROL}/admin/users/${member.userId}`,
      { data: { status: "suspended" } },
    );
    expect(suspended.status(), await suspended.text()).toBe(200);
    await Promise.all(concurrent);
    await expectRevoked(
      suspension.context.request,
      suspension.token,
      suspension.invalidations,
    );
    expect(
      await Promise.all(
        Array.from({ length: 20 }, () =>
          inferenceStatus(suspension.context.request, suspension.token),
        ),
      ),
    ).toEqual(Array(20).fill(401));
    await suspension.context.close();

    const reactivated = await page.request.patch(
      `${CONTROL}/admin/users/${member.userId}`,
      { data: { status: "active" } },
    );
    expect(reactivated.status(), await reactivated.text()).toBe(200);
    const removal = await memberLogin();
    const removed = await page.request.delete(
      `${CONTROL}/admin/users/${member.userId}`,
    );
    expect(removed.status(), await removed.text()).toBe(204);
    await expectRevoked(
      removal.context.request,
      removal.token,
      removal.invalidations,
    );
    await removal.context.close();
  });
});
