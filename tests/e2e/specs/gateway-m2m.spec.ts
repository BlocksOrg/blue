import { expect, test } from "@playwright/test";
import { readFile } from "node:fs/promises";
import http from "node:http";
import https from "node:https";
import { collect, prepareClient, readClientFile, runCli, spawnCli, waitForOutput } from "../support/cli.js";
import { loginAsAdmin } from "../support/dashboard.js";

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

function parseMetric(body: string, name: string): number {
  const match = body.match(new RegExp(`^${name}\\s+(\\d+)`, "m"));
  expect(match, `metric ${name} present in:\n${body}`).toBeTruthy();
  return Number(match![1]);
}

type InternalResult = { status?: number; error?: string };

async function callInternal(options: {
  cert?: Buffer;
  key?: Buffer;
  authorization?: string;
  plaintext?: boolean;
}): Promise<InternalResult> {
  const transport = options.plaintext ? http : https;
  const ca = options.plaintext ? undefined : await readFile("/certs/ca.crt");
  return new Promise((resolve) => {
    const request = transport.request(
      {
        hostname: "127.0.0.1",
        port: 8082,
        path: "/internal/gateway/resolve",
        method: "POST",
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
    request.end(JSON.stringify({ pseudotoken: "not-a-real-token" }));
  });
}

test.describe.serial("Gateway M2M auth", () => {
  let home: string;
  let pseudotoken: string;

  test.beforeAll(async () => {
    home = await prepareClient("gateway-m2m");
  });

  test("mints a pseudotoken over the M2M-authenticated resolver", async ({ page }) => {
    await loginAsAdmin(page);
    const currentResponse = await page.request.get(`${CONTROL}/admin/governance-config`);
    expect(currentResponse.status(), await currentResponse.text()).toBe(200);
    const current = await currentResponse.json();
    const managedYaml = String(current.managed_yaml).replace(
      /^harnesses:\s*$/m,
      "gateway:\n  type: litellm\nharnesses:",
    );
    expect(managedYaml).not.toBe(current.managed_yaml);
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
    // more so this isolated client receives the newly minted pseudotoken.
    const preferred = await runCli(home, ["agent", "codex"]);
    expect(preferred.code, preferred.stderr).toBe(0);
    const applied = await runCli(home, ["apply", "--yes"]);
    expect(applied.code, applied.stderr).toBe(0);

    // Fetch the same personalized response the CLI consumes. Runtime gateway
    // credentials are intentionally not persisted in Blue's config cache.
    const session = JSON.parse(await readClientFile(home, ".config/blue/session.json"));
    const configResponse = await page.request.get(
      `${CONTROL}/governance-config`,
      {
        headers: {
          authorization: `Bearer ${session.token}`,
          "x-blue-contract-version": "2",
          "x-blue-capabilities":
            "adapter_intervals,compiled_harness_registry,transactional_reconcile,versioned_state",
        },
      },
    );
    expect(configResponse.status(), await configResponse.text()).toBe(200);
    pseudotoken = (await configResponse.json()).gateway?.pseudotoken ?? "";
    expect(pseudotoken).toBeTruthy();

    // Sanity: an authorized request resolves through the M2M-secured hop.
    const ok = await page.request.post(`${PROXY}/v1/chat/completions`, {
      headers: { authorization: `Bearer ${pseudotoken}` },
      data: { model: "gpt-e2e", messages: [{ role: "user", content: "hello" }] },
    });
    expect(ok.status(), await ok.text()).toBe(200);
  });

  test("enforces TLS, the trusted proxy certificate, and OAuth independently", async () => {
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
  });

  test("serves sustained load across token refreshes without auth failures", async ({ page }) => {
    expect(pseudotoken).toBeTruthy();
    await page.request.post(`${UPSTREAM}/_e2e/reset`);

    const beforeMetrics = await (await page.request.get(`${PROXY}/metrics`)).text();
    const before = parseMetric(beforeMetrics, "gateway_proxy_oauth_token_fetches_total");
    const fetchErrorsBefore = parseMetric(beforeMetrics, "gateway_proxy_oauth_token_fetch_errors_total");

    const send = () =>
      page.request
        .post(`${PROXY}/v1/chat/completions`, {
          headers: { authorization: `Bearer ${pseudotoken}` },
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
    expect(pseudotoken).toBeTruthy();

    const before = parseMetric(await (await page.request.get(`${PROXY}/metrics`)).text(), "gateway_proxy_oauth_token_fetches_total");

    // Fire a large concurrent burst. Even if it straddles a token rollover, the
    // single-flight guard funnels it into at most one fetch (not one per call).
    const statuses = await Promise.all(
      Array.from({ length: 200 }, () =>
        page.request
          .post(`${PROXY}/v1/chat/completions`, {
            headers: { authorization: `Bearer ${pseudotoken}` },
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
});
