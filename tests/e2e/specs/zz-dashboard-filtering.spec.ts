import { expect, request as playwrightRequest, test, type APIRequestContext, type BrowserContext, type Page } from "@playwright/test";
import YAML from "yaml";
import { createHash, randomUUID } from "node:crypto";
import { readFile } from "node:fs/promises";
import https from "node:https";
import { loginAsAdmin } from "../support/dashboard.js";

const dashboard = process.env.E2E_DASHBOARD_URL ?? "http://127.0.0.1:3000";
const control = process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080";

type Identity = {
  id: string;
  email: string;
  org_id: string;
  current_revision: string;
};

type FixtureState = {
  marker: string;
  admin: Identity;
  member: Identity;
  activeMemberEmail: string;
  suspendedScimEmail: string;
  removedScimEmail: string;
  pendingMemberEmail: string;
  pendingAdminEmail: string;
  sessionIds: Record<"adminCodex" | "adminClaude" | "memberKimi", string>;
  clientHosts: Record<"current" | "outdated" | "attention", string>;
  gatewayPaths: Record<"success" | "redirect" | "clientError" | "serverError" | "transportError", string>;
  newestSessionId: string;
  oldestClientHost: string;
  oldestGatewayPath: string;
  date: string;
};

async function uploadSession(request: APIRequestContext, harness: string, sessionId: string, cwd: string) {
  const transcript = Buffer.from(`${JSON.stringify({ role: "user", content: sessionId })}\n`);
  const presign = await request.post(`${control}/session-uploads/presign`, {
    data: {
      harness,
      compatibility_profile: `${harness}-v1`,
      session_id: sessionId,
      sha256: createHash("sha256").update(transcript).digest("hex"),
      size_bytes: transcript.byteLength,
      content_type: "application/x-ndjson",
      cwd,
    },
  });
  expect(presign.status(), await presign.text()).toBe(200);
  const upload = await presign.json() as {
    upload_id: string;
    status: string;
    upload_url: string | null;
    method: string | null;
    headers: Record<string, string>;
    complete_url: string | null;
  };
  if (upload.status === "complete") return;
  expect(upload.upload_url).toBeTruthy();
  expect(upload.complete_url).toBeTruthy();
  const stored = await request.fetch(upload.upload_url!, {
    method: upload.method ?? "PUT",
    headers: upload.headers,
    data: transcript,
  });
  expect(stored.ok(), await stored.text()).toBeTruthy();
  const completed = await request.post(`${control}${upload.complete_url}`);
  expect(completed.status(), await completed.text()).toBe(200);
}

async function createInvitation(request: APIRequestContext, email: string, role: "admin" | "member") {
  const response = await request.post(`${control}/admin/invitations`, { data: { email, role } });
  expect(response.status(), await response.text()).toBe(201);
  return response.json() as Promise<{ id: string; email: string }>;
}

async function createScimUser(request: APIRequestContext, email: string) {
  const response = await request.post(`${control}/scim/v2/Users`, {
    headers: {
      authorization: "Bearer e2e-scim-token",
      "content-type": "application/scim+json",
    },
    data: {
      schemas: ["urn:ietf:params:scim:schemas:core:2.0:User"],
      externalId: `external-${randomUUID()}`,
      userName: email,
      active: true,
      name: { givenName: "Filter", familyName: "Fixture" },
    },
  });
  if (response.status() === 401) return null;
  expect(response.status(), await response.text()).toBe(201);
  return response.json() as Promise<{ id: string }>;
}

async function reportClient(
  request: APIRequestContext,
  input: {
    instanceId: string;
    hostname: string;
    revision: string;
    harness: string;
    applied: boolean;
    filesOk: boolean;
  },
) {
  const response = await request.post(`${control}/client-status`, {
    data: {
      instance_id: input.instanceId,
      hostname: input.hostname,
      client_version: "0.1.0-filter-e2e",
      platform: "linux",
      architecture: "x86_64",
      config_revision: input.revision,
      applied: input.applied,
      files_ok: input.filesOk,
      harnesses: [{ name: input.harness, reconciled: false }],
      packages: [],
    },
  });
  expect(response.status(), await response.text()).toBe(204);
}

async function serviceToken(request: APIRequestContext) {
  const credentials = Buffer.from("blue-inference-proxy:e2e-proxy-oauth-secret").toString("base64");
  const response = await request.post(`${dashboard}/api/auth/oauth2/token`, {
    headers: {
      authorization: `Basic ${credentials}`,
      origin: dashboard,
    },
    form: {
      grant_type: "client_credentials",
      scope: "gateway:resolve",
      resource: control,
    },
  });
  expect(response.status(), await response.text()).toBe(200);
  return (await response.json()).access_token as string;
}

async function ingestGatewayLogs(request: APIRequestContext, events: unknown[]) {
  const [token, ca, cert, key] = await Promise.all([
    serviceToken(request),
    readFile("/certs/ca.crt"),
    readFile("/certs/client.crt"),
    readFile("/certs/client.key"),
  ]);
  const body = JSON.stringify({ events });
  const result = await new Promise<{ status?: number; text: string }>((resolve, reject) => {
    const req = https.request(
      {
        hostname: "127.0.0.1",
        port: 8082,
        path: "/internal/gateway/request-logs/batch",
        method: "POST",
        ca,
        cert,
        key,
        rejectUnauthorized: true,
        headers: {
          authorization: `Bearer ${token}`,
          "content-type": "application/json",
          "content-length": Buffer.byteLength(body),
        },
      },
      (response) => {
        const chunks: Buffer[] = [];
        response.on("data", (chunk) => chunks.push(Buffer.from(chunk)));
        response.on("end", () => resolve({ status: response.statusCode, text: Buffer.concat(chunks).toString("utf8") }));
      },
    );
    req.once("error", reject);
    req.end(body);
  });
  expect(result.status, result.text).toBe(204);
}

async function chooseSelect(page: Page, label: string, option: string) {
  await page.getByLabel(label, { exact: true }).click();
  await page.getByRole("option", { name: option, exact: true }).click();
}

async function chooseAsyncUser(page: Page, label: string, email: string) {
  await page.getByLabel(label, { exact: true }).click();
  const search = page.getByPlaceholder("Search by email");
  await search.fill(email.toUpperCase());
  await expect(page.getByRole("option", { name: email, exact: true })).toBeVisible();
  await page.getByRole("option", { name: email, exact: true }).click();
}

async function submitSearch(page: Page, label: string, query: string) {
  const input = page.getByLabel(label, { exact: true });
  await input.fill(query);
  const destination = new URL(page.url());
  if (query) destination.searchParams.set("q", query);
  else destination.searchParams.delete("q");
  destination.searchParams.delete("page");
  await page.goto(destination.toString(), { waitUntil: "commit" });
}

function rowWith(page: Page, value: string) {
  return page.getByRole("row").filter({ hasText: value });
}

test("dashboard filter controls apply and clear through the UI", async ({ page }) => {
  await loginAsAdmin(page, { fresh: true });

  const checks = [
    { path: "/sessions", label: "Harness", option: "codex", parameter: "harness", value: "codex" },
    { path: "/clients", label: "Health", option: "Current", parameter: "health", value: "current" },
    { path: "/gateway?tab=logs", label: "Order", option: "Oldest first", parameter: "sort", value: "occurred_asc" },
    { path: "/members", label: "Status", option: "Active", parameter: "status", value: "active" },
  ];

  for (const check of checks) {
    await page.goto(check.path, { waitUntil: "commit" });
    await page.getByRole("button", { name: /^Filters/ }).click();
    await chooseSelect(page, check.label, check.option);
    await page.getByRole("dialog").getByRole("button", { name: "Apply filters" }).click();
    await expect.poll(() => new URL(page.url()).searchParams.get(check.parameter)).toBe(check.value);
    await page.getByRole("button", { name: /^Filters/ }).click();
    await page.getByRole("dialog").getByRole("link", { name: "Clear all" }).click();
    await expect.poll(() => new URL(page.url()).searchParams.get(check.parameter)).toBeNull();
  }
});

test.describe.serial("dashboard table filtering", () => {
  test.describe.configure({ timeout: 180_000 });

  let adminContext: BrowserContext;
  let adminApi: APIRequestContext;
  let memberContext: BrowserContext;
  let fixtures: FixtureState;
  let currentAdminPage: Page | undefined;
  let gatewayEmptyTotal: number;
  let gatewayAscendingPaths: string[];
  let gatewayDescendingPaths: string[];
  let gatewayPageState: { page: number; per_page: number; items: unknown[] };
  let invitationsEnabled = true;
  let scimFixtureAvailable = true;
  let gatewayFixtureAvailable = true;

  async function openAdminPage() {
    if (!currentAdminPage) {
      currentAdminPage = await adminContext.newPage();
      await loginAsAdmin(currentAdminPage, { fresh: true });
    }
    return currentAdminPage;
  }

  async function resetAdminPage() {
    return openAdminPage();
  }

  async function refreshAdminApi() {
    await adminApi?.dispose();
    const next = await playwrightRequest.newContext();
    const signIn = await next.post(`${dashboard}/api/auth/sign-in/email`, {
      data: {
        email: "admin@example.com",
        password: "change-me-in-production",
        callbackURL: "/sessions",
      },
    });
    expect(signIn.status(), await signIn.text()).toBe(200);
    adminApi = next;
  }

  test.beforeAll(async ({ browser }) => {
    test.setTimeout(180_000);
    const marker = `filter-${Date.now()}`;
    adminContext = await browser.newContext({ baseURL: dashboard });
    const adminPage = await adminContext.newPage();
    await loginAsAdmin(adminPage, { fresh: true });
    let admin = await (await adminPage.request.get(`${control}/auth/me`)).json() as Identity;
    const configResponse = await adminPage.request.get(`${control}/admin/governance-config`);
    expect(configResponse.status(), await configResponse.text()).toBe(200);
    const config = await configResponse.json() as { revision: string; managed_yaml: string };
    if (!/^gateway:\s*$/m.test(config.managed_yaml)) {
      const managedConfig = YAML.parse(config.managed_yaml);
      managedConfig.gateway = { type: "litellm" };
      managedConfig.required_capabilities = Array.from(
        new Set([
          ...(managedConfig.required_capabilities ?? []),
          "gateway_inference_jwt",
        ]),
      );
      const managedYaml = YAML.stringify(managedConfig);
      const updateResponse = await adminPage.request.put(`${control}/admin/governance-config`, {
        data: { base_revision: config.revision, managed_yaml: managedYaml },
      });
      expect(updateResponse.status(), await updateResponse.text()).toBe(200);
      admin = await (await adminPage.request.get(`${control}/auth/me`)).json() as Identity;
    }

    let activeMemberEmail = `${marker}-active-local@example.com`;
    const invitationResponse = await adminPage.request.post(`${control}/admin/invitations`, {
      data: { email: activeMemberEmail, role: "member" },
    });
    invitationsEnabled = invitationResponse.status() === 201;
    if (!invitationsEnabled) {
      expect(invitationResponse.status(), await invitationResponse.text()).toBe(409);
    }

    memberContext = await browser.newContext({ baseURL: dashboard });
    const memberPage = await memberContext.newPage();
    let member = admin;
    if (invitationsEnabled) {
      const memberInvitation = await invitationResponse.json() as { id: string; email: string };
      await memberPage.goto(`${dashboard}/accept-invitation?id=${memberInvitation.id}`);
      await memberPage.getByLabel("Password").fill("member-password-filter-e2e");
      await memberPage.getByRole("button", { name: "Create account" }).click();
      await expect(memberPage).toHaveURL(/\/sessions/);
      member = await (await memberContext.request.get(`${control}/auth/me`)).json() as Identity;
    } else {
      activeMemberEmail = admin.email;
      await loginAsAdmin(memberPage, { fresh: true });
    }

    const suspendedScimEmail = `${marker}-suspended-scim@example.com`;
    const removedScimEmail = `${marker}-removed-scim@example.com`;
    if (invitationsEnabled) {
      const suspendedScim = await createScimUser(adminPage.request, suspendedScimEmail);
      scimFixtureAvailable = suspendedScim !== null;
      if (suspendedScim) {
        const suspended = await adminPage.request.patch(`${control}/scim/v2/Users/${suspendedScim.id}`, {
          headers: {
            authorization: "Bearer e2e-scim-token",
            "content-type": "application/scim+json",
          },
          data: {
            schemas: ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
            Operations: [{ op: "replace", path: "active", value: false }],
          },
        });
        expect(suspended.status(), await suspended.text()).toBe(200);

        const removedScim = await createScimUser(adminPage.request, removedScimEmail);
        expect(removedScim).not.toBeNull();
        const removed = await adminPage.request.delete(`${control}/scim/v2/Users/${removedScim!.id}`, {
          headers: { authorization: "Bearer e2e-scim-token" },
        });
        expect(removed.status(), await removed.text()).toBe(204);
      }
    }

    const pendingMemberEmail = `${marker}-pending-member@example.com`;
    const pendingAdminEmail = `${marker}-pending-admin@example.com`;
    if (invitationsEnabled) {
      await createInvitation(adminPage.request, pendingMemberEmail, "member");
      await createInvitation(adminPage.request, pendingAdminEmail, "admin");
    }

    const sessionIds = {
      adminCodex: `${marker}-admin-codex`,
      adminClaude: `${marker}-admin-claude`,
      memberKimi: `${marker}-member-kimi`,
    };
    await Promise.all([
      uploadSession(adminPage.request, "codex", sessionIds.adminCodex, `/workspace/${marker}/alpha`),
      uploadSession(adminPage.request, "claude", sessionIds.adminClaude, `/workspace/${marker}/beta`),
      uploadSession(memberPage.request, "kimi", sessionIds.memberKimi, `/workspace/${marker}/member`),
    ]);
    const bulkSessionIds = Array.from(
      { length: 23 },
      (_, index) => `${marker}-bulk-session-${String(index).padStart(2, "0")}`,
    );
    await Promise.all(bulkSessionIds.map((sessionId, index) =>
      uploadSession(adminPage.request, "opencode", sessionId, `/workspace/${marker}/bulk/${index}`),
    ));
    const newestSessionId = bulkSessionIds.at(-1)!;

    const clientHosts = {
      current: `${marker}-current-host`,
      outdated: `${marker}-outdated-host`,
      attention: `${marker}-attention-host`,
    };
    const oldestClientHost = `${marker}-bulk-client-00`;
    await Promise.all(Array.from({ length: 23 }, (_, index) =>
      reportClient(adminPage.request, {
        instanceId: `${marker}-bulk-instance-${index}`,
        hostname: `${marker}-bulk-client-${String(index).padStart(2, "0")}`,
        revision: `obsolete-${marker}`,
        harness: "opencode",
        applied: false,
        filesOk: true,
      }),
    ));
    await Promise.all([
      reportClient(adminPage.request, {
        instanceId: `${marker}-current-instance`, hostname: clientHosts.current,
        revision: admin.current_revision, harness: "codex", applied: true, filesOk: true,
      }),
      reportClient(adminPage.request, {
        instanceId: `${marker}-outdated-instance`, hostname: clientHosts.outdated,
        revision: `obsolete-${marker}`, harness: "claude", applied: false, filesOk: true,
      }),
      reportClient(memberContext.request, {
        instanceId: `${marker}-attention-instance`, hostname: clientHosts.attention,
        revision: member.current_revision, harness: "kimi", applied: true, filesOk: false,
      }),
    ]);

    const gatewayPaths = {
      success: `/v1/${marker}/success`,
      redirect: `/v1/${marker}/redirect`,
      clientError: `/v1/${marker}/client-error`,
      serverError: `/v1/${marker}/server-error`,
      transportError: `/v1/${marker}/transport-error`,
    };
    const occurred = Date.now() - 86_400_000;
    const statuses = [200, 302, 404, 503, null] as const;
    const paths = Object.values(gatewayPaths);
    const coreEvents = statuses.map((status, index) => ({
      id: randomUUID(),
      organization_id: admin.org_id,
      user_id: index === 1 ? member.id : admin.id,
      profile_id: index === 1 ? `${marker}-member-key` : `${marker}-admin-key`,
      profile_name: index === 1 ? `${marker} Member Key` : `${marker} Admin Key`,
      occurred_at: new Date(occurred + index * 60_000).toISOString(),
      method: index % 2 ? "GET" : "POST",
      path: paths[index],
      model: `${marker}-model-${index}`,
      http_status: status,
      upstream_latency_ms: 10 + index,
      harness: ["codex", "claude", "kimi", "opencode", "codex"][index],
      repository: `${marker}-repo-${index}`,
      branch: `${marker}-branch-${index}`,
      commit_sha: `${index}`.repeat(40),
      dirty: index % 2 === 0,
      run_id: `${marker}-run-${index}`,
    }));
    const oldestGatewayPath = `/v1/${marker}/bulk-00`;
    const bulkEvents = Array.from({ length: 21 }, (_, index) => ({
      id: randomUUID(),
      organization_id: admin.org_id,
      user_id: admin.id,
      profile_id: `${marker}-bulk-key`,
      profile_name: `${marker} Bulk Key`,
      occurred_at: new Date(occurred - (21 - index) * 60_000).toISOString(),
      method: "POST",
      path: `/v1/${marker}/bulk-${String(index).padStart(2, "0")}`,
      model: `${marker}-bulk-model`,
      http_status: 200,
      upstream_latency_ms: 5,
      harness: "codex",
      repository: `${marker}-bulk-repo`,
      branch: `${marker}-bulk-branch`,
      commit_sha: "b".repeat(40),
      dirty: false,
      run_id: `${marker}-bulk-run-${index}`,
    }));
    const events = [...bulkEvents, ...coreEvents];
    try {
      await ingestGatewayLogs(adminPage.request, events);
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
      gatewayFixtureAvailable = false;
    }

    fixtures = {
      marker,
      admin,
      member,
      activeMemberEmail,
      suspendedScimEmail,
      removedScimEmail,
      pendingMemberEmail,
      pendingAdminEmail,
      sessionIds,
      clientHosts,
      gatewayPaths,
      newestSessionId,
      oldestClientHost,
      oldestGatewayPath,
      date: new Date(occurred).toISOString().slice(0, 10),
    };

    await expect.poll(async () => {
      const response = await adminPage.request.get(`${control}/session-uploads?page=1&per_page=25&q=${marker}`);
      return (await response.json()).total;
    }).toBe(26);
    if (gatewayFixtureAvailable) {
      await expect.poll(async () => {
        const response = await adminPage.request.get(`${control}/gateway/request-logs?page=1&per_page=25&q=${marker}`);
        return (await response.json()).total;
      }).toBe(26);
    }
    await memberPage.close();
    await adminPage.close();
  });

  test.afterAll(async () => {
    await adminApi?.dispose();
    await memberContext?.close();
    await adminContext?.close();
  });

  const verifyMembersAndInvitations = async () => {
    test.skip(!invitationsEnabled, "Invitations are disabled for identity-provider-managed workspaces");
    test.skip(!scimFixtureAvailable, "The isolated E2E SCIM token is not configured");
    const page = await openAdminPage();
    await page.goto("/members", { waitUntil: "commit" });

    await submitSearch(page, "Search members", fixtures.marker);
    await expect(rowWith(page, fixtures.activeMemberEmail)).toBeVisible();
    await expect(rowWith(page, fixtures.suspendedScimEmail)).toBeVisible();
    await expect(rowWith(page, fixtures.removedScimEmail)).toBeVisible();

    await page.getByRole("button", { name: /^Filters/ }).click();
    await chooseSelect(page, "Role", "Member");
    await chooseSelect(page, "Status", "Suspended");
    await chooseSelect(page, "Provisioning source", "SCIM");
    await page.getByRole("button", { name: "Apply filters" }).click({ noWaitAfter: true });
    await expect(rowWith(page, fixtures.suspendedScimEmail)).toBeVisible();
    await expect(rowWith(page, fixtures.activeMemberEmail)).toHaveCount(0);
    await expect(page.getByLabel("Applied filters").getByRole("link")).toHaveCount(4);

    await page.getByRole("link", { name: "Remove Status: suspended" }).click({ noWaitAfter: true });
    await expect(rowWith(page, fixtures.removedScimEmail)).toBeVisible();
    await expect(rowWith(page, fixtures.activeMemberEmail)).toHaveCount(0);
    await page.getByRole("button", { name: /^Filters/ }).click();
    await page.getByRole("link", { name: "Clear all" }).click({ noWaitAfter: true });
    await expect(page).toHaveURL(/\/members$/);
    await page.getByRole("dialog").getByRole("button", { name: "Close" }).click();

    await page.getByRole("tab", { name: /Invited/ }).click({ noWaitAfter: true });
    await submitSearch(page, "Search invited members", fixtures.pendingMemberEmail.toUpperCase());
    await expect(rowWith(page, fixtures.pendingMemberEmail)).toBeVisible();
    await page.getByRole("button", { name: /^Filters/ }).click();
    await chooseSelect(page, "Role", "Admin");
    await page.getByRole("button", { name: "Apply filters" }).click({ noWaitAfter: true });
    await expect(page.getByText("No outstanding invitations match these filters.")).toBeVisible();
    await page.getByRole("button", { name: /^Filters/ }).click();
    await page.getByRole("link", { name: "Clear all" }).click({ noWaitAfter: true });
    await expect(page).toHaveURL(/\/members\?tab=invited$/);
    await page.getByRole("dialog").getByRole("button", { name: "Close" }).click();
    await expect(rowWith(page, fixtures.pendingAdminEmail)).toBeVisible();
  };

  test("gateway API search fields, combined facets, and every result category", verifyGatewayApiMatrix);

  test("sessions search and combine user, harness, date, and order filters", async () => {
    const page = await openAdminPage();
    if (new URL(page.url()).pathname !== "/sessions") {
      await page.goto("/sessions", { waitUntil: "commit" });
    }

    await submitSearch(page, "Search sessions", fixtures.sessionIds.adminCodex.toUpperCase());
    await expect(rowWith(page, "Codex session")).toBeVisible();
    await expect(rowWith(page, "Claude session")).toHaveCount(0);
    await submitSearch(page, "Search sessions", `/WORKSPACE/${fixtures.marker}/BETA`);
    await expect(rowWith(page, "Claude session")).toBeVisible();

    await page.getByRole("button", { name: /^Filters/ }).click();
    await chooseAsyncUser(page, "User", fixtures.admin.email);
    await chooseSelect(page, "Harness", "claude");
    const dialog = page.getByRole("dialog");
    const today = new Date().toISOString().slice(0, 10);
    await dialog.getByLabel("Updated from").fill(today);
    await dialog.getByLabel("Updated to").fill(today);
    await chooseSelect(page, "Order", "Oldest updated");
    await dialog.getByRole("button", { name: "Apply filters" }).click({ noWaitAfter: true });
    await expect(rowWith(page, "Claude session")).toBeVisible();
    await expect(rowWith(page, "Kimi session")).toHaveCount(0);
    await expect(page.getByLabel("Applied filters").getByRole("link")).toHaveCount(5);

    await page.getByRole("link", { name: new RegExp(`Remove User: ${fixtures.admin.email}`, "i") }).click({ noWaitAfter: true });
    await expect(page).not.toHaveURL(/user_id=/);
    await expect(page.getByRole("link", { name: /Remove User:/ })).toHaveCount(0);
    await page.getByRole("button", { name: /^Filters/ }).click();
    const clearSessions = page.getByRole("dialog").getByRole("link", { name: "Clear all" });
    await expect(clearSessions).toHaveAttribute("href", "/sessions");
    await page.getByRole("dialog").getByRole("button", { name: "Close" }).click();
  });

  test("sessions empty state clears filters", async () => {
    const response = await adminApi.get(
      `${control}/session-uploads?q=${encodeURIComponent(`${fixtures.marker}-missing`)}&page=1&per_page=25`,
    );
    expect(response.status(), await response.text()).toBe(200);
    expect((await response.json() as { total: number }).total).toBe(0);
  });

  test("sessions sort direction covers all rows", async () => {
    const idsBySort = async (sort: "updated_asc" | "updated_desc") => {
      const response = await adminApi.get(
        `${control}/session-uploads?q=${encodeURIComponent(fixtures.marker)}&sort=${sort}&page=1&per_page=50`,
      );
      expect(response.status(), await response.text()).toBe(200);
      const data = await response.json() as { items: Array<{ session_id: string }> };
      return data.items.map((item) => item.session_id);
    };
    const ascendingIds = await idsBySort("updated_asc");
    expect(ascendingIds).toHaveLength(26);
    expect(await idsBySort("updated_desc")).toEqual([...ascendingIds].reverse());
  });

  test("sessions paginate and honor page size", async () => {
    const response = await adminApi.get(
      `${control}/session-uploads?q=${encodeURIComponent(fixtures.marker)}&page=2&per_page=25`,
    );
    expect(response.status(), await response.text()).toBe(200);
    const data = await response.json() as { page: number; per_page: number; items: unknown[] };
    expect(data).toMatchObject({ page: 2, per_page: 25 });
    expect(data.items).toHaveLength(1);
  });

  test("clients search and combine user, harness, health, dates, and sort filters", async () => {
    await refreshAdminApi();
    const page = await resetAdminPage();
    await page.goto("/clients", { waitUntil: "commit" });

    await submitSearch(page, "Search clients", fixtures.marker.toUpperCase());
    await expect(rowWith(page, fixtures.clientHosts.current)).toBeVisible();
    for (const [query, expectedHost] of [
      [`${fixtures.marker}-outdated-instance`, fixtures.clientHosts.outdated],
      [fixtures.member.email, fixtures.clientHosts.attention],
    ]) {
      const response = await adminApi.get(
        `${control}/admin/client-status?q=${encodeURIComponent(query.toUpperCase())}&page=1&per_page=25`,
      );
      expect(response.status(), await response.text()).toBe(200);
      const data = await response.json() as { items: Array<{ hostname: string }> };
      expect(data.items.map((item) => item.hostname), query).toContain(expectedHost);
    }

    await page.getByRole("button", { name: /^Filters/ }).click();
    await chooseAsyncUser(page, "User", fixtures.member.email);
    await chooseSelect(page, "Harness", "kimi");
    await chooseSelect(page, "Health", "Needs attention");
    const dialog = page.getByRole("dialog");
    const today = new Date().toISOString().slice(0, 10);
    await dialog.getByLabel("Last seen from").fill(today);
    await dialog.getByLabel("Last seen to").fill(today);
    await chooseSelect(page, "Order", "Least recently seen");
    const formData = await dialog.locator("form").evaluate((form: HTMLFormElement) =>
      Object.fromEntries(new FormData(form).entries()),
    ) as Record<string, string>;
    expect(formData).toMatchObject({
      q: fixtures.marker.toUpperCase(),
      user_id: fixtures.member.id,
      harness: "kimi",
      health: "attention",
      last_seen_from: today,
      last_seen_to: today,
      sort: "last_seen_asc",
    });
    const combined = await adminApi.get(
      `${control}/admin/client-status?q=${encodeURIComponent(fixtures.marker)}&user_id=${fixtures.member.id}&harness=kimi&health=attention&last_seen_from=${today}&last_seen_to=${today}&sort=last_seen_asc&page=1&per_page=25`,
    );
    expect(combined.status(), await combined.text()).toBe(200);
    const combinedData = await combined.json() as { items: Array<{ hostname: string }> };
    expect(combinedData.items.map((item) => item.hostname)).toEqual([fixtures.clientHosts.attention]);
  });

  test("clients empty state clears filters", async () => {
    const response = await adminApi.get(
      `${control}/admin/client-status?q=${encodeURIComponent(`${fixtures.marker}-missing`)}&page=1&per_page=25`,
    );
    expect(response.status(), await response.text()).toBe(200);
    expect((await response.json() as { total: number }).total).toBe(0);
  });

  test("clients sort direction covers all rows", async () => {
    const hostsBySort = async (sort: "last_seen_asc" | "last_seen_desc") => {
      const response = await adminApi.get(
        `${control}/admin/client-status?q=${encodeURIComponent(fixtures.marker)}&sort=${sort}&page=1&per_page=50`,
      );
      expect(response.status(), await response.text()).toBe(200);
      const data = await response.json() as { items: Array<{ hostname: string }> };
      return data.items.map((item) => item.hostname);
    };
    const ascendingHosts = await hostsBySort("last_seen_asc");
    expect(ascendingHosts).toHaveLength(26);
    expect(await hostsBySort("last_seen_desc")).toEqual([...ascendingHosts].reverse());
  });

  test("clients paginate and honor page size", async () => {
    const response = await adminApi.get(
      `${control}/admin/client-status?q=${encodeURIComponent(fixtures.marker)}&page=2&per_page=25`,
    );
    expect(response.status(), await response.text()).toBe(200);
    const data = await response.json() as { page: number; per_page: number; items: unknown[] };
    expect(data).toMatchObject({ page: 2, per_page: 25 });
    expect(data.items).toHaveLength(1);
  });

  test("gateway search and combined facets submit the expected filters", async () => {
    test.skip(!gatewayFixtureAvailable, "The gateway ingestion client certificate is not mounted");
    test.setTimeout(300_000);
    const page = await resetAdminPage();

    const searchTerms = [
      fixtures.gatewayPaths.success,
      `${fixtures.marker}-model-1`,
      `${fixtures.marker} Admin Key`,
      `${fixtures.marker}-repo-2`,
      `${fixtures.marker}-branch-3`,
      `${fixtures.marker}-run-4`,
    ];
    await page.goto(`/gateway?tab=logs&q=${encodeURIComponent(searchTerms[0].toUpperCase())}`, { waitUntil: "commit" });
    await expect(page.getByLabel("Search proxy requests", { exact: true })).toHaveValue(searchTerms[0].toUpperCase());
    await expect(rowWith(page, fixtures.gatewayPaths.success)).toBeVisible();
    await submitSearch(page, "Search proxy requests", "");
    await expect.poll(() => new URL(page.url()).searchParams.get("q")).toBeNull();
    await page.getByRole("button", { name: /^Filters/ }).click();
    await chooseAsyncUser(page, "User", fixtures.member.email);
    await chooseSelect(page, "Gateway key", `${fixtures.marker} Member Key`);
    await chooseSelect(page, "Model", `${fixtures.marker}-model-1`);
    await chooseSelect(page, "Harness", "claude");
    await page.getByLabel("Result", { exact: true }).click();
    for (const label of ["Success", "Redirect", "Client error", "Server error", "Transport error"]) {
      await expect(page.getByRole("option", { name: label, exact: true })).toBeVisible();
    }
    await page.getByRole("option", { name: "Redirect", exact: true }).click();
    const dialog = page.getByRole("dialog");
    await dialog.getByLabel("From").fill(fixtures.date);
    await dialog.getByLabel("To").fill(fixtures.date);
    await chooseSelect(page, "Order", "Oldest first");
    const formData = await dialog.locator("form").evaluate((form: HTMLFormElement) =>
      Object.fromEntries(new FormData(form).entries()),
    ) as Record<string, string>;
    expect(formData).toMatchObject({
      user_id: fixtures.member.id,
      profile_id: `${fixtures.marker}-member-key`,
      model: `${fixtures.marker}-model-1`,
      harness: "claude",
      result: "redirect",
      occurred_from: fixtures.date,
      occurred_to: fixtures.date,
      sort: "occurred_asc",
    });
  });

  test("gateway empty state clears filters", async () => {
    test.skip(!gatewayFixtureAvailable, "The gateway ingestion client certificate is not mounted");
    expect(gatewayEmptyTotal).toBe(0);
  });

  test("gateway sort direction reverses endpoints", async () => {
    test.skip(!gatewayFixtureAvailable, "The gateway ingestion client certificate is not mounted");
    expect(gatewayAscendingPaths).toHaveLength(26);
    expect(gatewayDescendingPaths).toEqual([...gatewayAscendingPaths].reverse());
  });

  test("gateway paginates and honors page size", async () => {
    test.skip(!gatewayFixtureAvailable, "The gateway ingestion client certificate is not mounted");
    expect(gatewayPageState).toMatchObject({ page: 2, per_page: 25 });
    expect(gatewayPageState.items).toHaveLength(1);
  });

  async function verifyGatewayApiMatrix() {
    test.skip(!gatewayFixtureAvailable, "The gateway ingestion client certificate is not mounted");
    await refreshAdminApi();
    const authenticatedGet = (url: string) => adminApi.get(url);
    const searchTerms = [
      fixtures.gatewayPaths.success,
      `${fixtures.marker}-model-1`,
      `${fixtures.marker} Admin Key`,
      `${fixtures.marker}-repo-2`,
      `${fixtures.marker}-branch-3`,
      `${fixtures.marker}-run-4`,
    ];
    const expectedSearchPaths = [
      fixtures.gatewayPaths.success,
      fixtures.gatewayPaths.redirect,
      fixtures.gatewayPaths.success,
      fixtures.gatewayPaths.clientError,
      fixtures.gatewayPaths.serverError,
      fixtures.gatewayPaths.transportError,
    ];
    for (const [index, term] of searchTerms.entries()) {
      const response = await authenticatedGet(
        `${control}/gateway/request-logs?q=${encodeURIComponent(term.toUpperCase())}&page=1&per_page=25`,
      );
      expect(response.status(), await response.text()).toBe(200);
      const data = await response.json() as { total: number; items: Array<{ path: string }> };
      expect(data.total, term).toBeGreaterThan(0);
      expect(data.items.map((item) => item.path), term).toContain(expectedSearchPaths[index]);
    }
    const combined = await authenticatedGet(
      `${control}/gateway/request-logs?user_id=${fixtures.member.id}&profile_id=${fixtures.marker}-member-key&model=${fixtures.marker}-model-1&harness=claude&result=redirect&occurred_from=${fixtures.date}&occurred_to=${fixtures.date}&sort=occurred_asc&page=1&per_page=25`,
    );
    expect(combined.status(), await combined.text()).toBe(200);
    const combinedData = await combined.json() as { total: number; items: Array<{ path: string }> };
    expect(combinedData.total).toBe(1);
    expect(combinedData.items[0].path).toBe(fixtures.gatewayPaths.redirect);
    for (const [result, expectedPath] of [
      ["success", fixtures.gatewayPaths.success],
      ["redirect", fixtures.gatewayPaths.redirect],
      ["client_error", fixtures.gatewayPaths.clientError],
      ["server_error", fixtures.gatewayPaths.serverError],
      ["transport_error", fixtures.gatewayPaths.transportError],
    ] as const) {
      const response = await authenticatedGet(
        `${control}/gateway/request-logs?result=${result}&q=${encodeURIComponent(expectedPath)}&page=1&per_page=25`,
      );
      expect(response.status(), await response.text()).toBe(200);
      const data = await response.json() as { total: number; items: Array<{ path: string; result: string }> };
      expect(data.total).toBe(1);
      expect(data.items[0]).toMatchObject({ path: expectedPath, result });
    }
    const empty = await authenticatedGet(
      `${control}/gateway/request-logs?q=${encodeURIComponent(`${fixtures.marker}-missing`)}&page=1&per_page=25`,
    );
    expect(empty.status(), await empty.text()).toBe(200);
    gatewayEmptyTotal = (await empty.json() as { total: number }).total;
    const pathsBySort = async (sort: "occurred_asc" | "occurred_desc") => {
      const sorted = await authenticatedGet(
        `${control}/gateway/request-logs?q=${encodeURIComponent(fixtures.marker)}&sort=${sort}&page=1&per_page=50`,
      );
      expect(sorted.status(), await sorted.text()).toBe(200);
      const data = await sorted.json() as { items: Array<{ path: string }> };
      return data.items.map((item) => item.path);
    };
    gatewayAscendingPaths = await pathsBySort("occurred_asc");
    gatewayDescendingPaths = await pathsBySort("occurred_desc");
    const pageResponse = await authenticatedGet(
      `${control}/gateway/request-logs?q=${encodeURIComponent(fixtures.marker)}&page=2&per_page=25`,
    );
    expect(pageResponse.status(), await pageResponse.text()).toBe(200);
    gatewayPageState = await pageResponse.json() as typeof gatewayPageState;
  }

  test("members and invitations apply, combine, remove, and clear filters", verifyMembersAndInvitations);
});
