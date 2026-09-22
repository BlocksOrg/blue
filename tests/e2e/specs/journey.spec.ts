import { expect, test } from "@playwright/test";
import type { Page } from "@playwright/test";
import type { ChildProcessWithoutNullStreams } from "node:child_process";
import { chmod, copyFile, mkdir, readFile, readdir, rm, stat, symlink, writeFile } from "node:fs/promises";
import { createHash, randomUUID } from "node:crypto";
import { gzipSync } from "node:zlib";
import path from "node:path";
import { collect, prepareClient, prepareEmptyClient, prepareRelocatedClient, readClientFile, relocatedEnv, runBareCliInPty, runCli, runCliWithInput, runCliWithoutHome, spawnCli, spawnCliInPty, stateRoot, waitForOutput } from "../support/cli.js";
import { loginAsAdmin } from "../support/dashboard.js";
import { enableGateway } from "../support/governance.js";
import YAML from "yaml";

const canonicalProfiles: Record<string, string> = {
  codex: "codex-v0_145_0",
  claude: "claude-v2_1_242",
  kimi: "kimi-v0_0_0",
  opencode: "opencode-v0_0_0",
};

const deviceUrlPattern = /http:\/\/127\.0\.0\.1:3000\/device\/[A-Za-z0-9_-]+/;

type PersistedOauthSession = {
  token: string;
  refresh_token: string;
  client_id: string;
  expires_at: number;
  resource?: string;
  scope?: string;
};

async function approveDeviceFlow(
  page: Page,
  child: ChildProcessWithoutNullStreams,
) {
  const completion = collect(child);
  const deviceUrl = await waitForOutput(child, deviceUrlPattern);
  await page.goto(deviceUrl);
  await expect(page.getByText("Confirmation code")).toBeVisible();
  await page.getByRole("button", { name: "Authorize" }).click();
  await expect(page.getByText("CLI authorized")).toBeVisible();
  return completion;
}

async function expectRefreshGrantRejected(page: Page, session: PersistedOauthSession) {
  const dashboard = process.env.E2E_DASHBOARD_URL ?? "http://127.0.0.1:3000";
  const form: Record<string, string> = {
    grant_type: "refresh_token",
    refresh_token: session.refresh_token,
    client_id: session.client_id,
  };
  if (session.resource) form.resource = session.resource;
  if (session.scope) form.scope = session.scope;
  const response = await page.request.post(`${dashboard}/api/auth/oauth2/token`, {
    headers: { origin: dashboard },
    form,
  });
  const body = await response.json();
  expect(response.status(), JSON.stringify(body)).toBe(400);
  expect(body).toMatchObject({ error: "invalid_grant" });
}

function oversizedGnuLongNameArchive(): Buffer {
  const body = Buffer.alloc(64 * 1024 + 1, "a");
  const header = Buffer.alloc(512);
  header.write("././@LongLink", 0, "ascii");
  header.write("0000600\0", 100, "ascii");
  header.write("0000000\0", 108, "ascii");
  header.write("0000000\0", 116, "ascii");
  header.write(`${body.length.toString(8).padStart(11, "0")}\0`, 124, "ascii");
  header.write("00000000000\0", 136, "ascii");
  header.fill(" ", 148, 156);
  header.write("L", 156, "ascii");
  header.write("ustar ", 257, "ascii");
  header.write(" \0", 263, "ascii");
  const checksum = header.reduce((sum, byte) => sum + byte, 0);
  header.write(`${checksum.toString(8).padStart(6, "0")}\0 `, 148, "ascii");
  const padding = Buffer.alloc((512 - (body.length % 512)) % 512);
  return gzipSync(Buffer.concat([header, body, padding, Buffer.alloc(1024)]), { level: 9 });
}

function extensionPayload(document: any, packages: any[], baseRevision: string, packageAudiences: any = {}) {
  const packageOverrides: Record<string, any> = {};
  const mcp: Record<string, any> = {};
  for (const [harness, policy] of Object.entries<any>(document.harnesses ?? {})) {
    packageOverrides[harness] = policy.package_overrides ?? {};
    mcp[harness] = policy.mcp ?? [];
  }
  return {
    base_revision: baseRevision,
    packages,
    package_audiences: packageAudiences,
    package_overrides: packageOverrides,
    mcp,
  };
}

test.describe.serial("Blue deployment journey", () => {
  let home: string;
  let adminSessionId: string;

  test.beforeAll(async () => {
    home = await prepareClient("journey");
  });

  test("@smoke dashboard login approves a real CLI device flow", async ({ page }) => {
    // An attempt that fails *after* the CLI login succeeds leaves a valid
    // session behind, and `blue login` then short-circuits with "Already logged
    // in" — so every retry fails for a different reason than the first one did,
    // and the test can never recover. Start each attempt logged out.
    await rm(path.join(home, ".config", "blue", "session.json"), { force: true });
    await loginAsAdmin(page);
    const child = spawnCli(home, ["login"]);
    const deviceUrl = await waitForOutput(child, /http:\/\/127\.0\.0\.1:3000\/device\/[A-Za-z0-9_-]+/);
    await page.goto(deviceUrl);
    await expect(page.getByText("Confirmation code")).toBeVisible();
    await page.getByRole("button", { name: "Authorize" }).click();
    await expect(page.getByText("CLI authorized")).toBeVisible();
    const result = await collect(child);
    expect(result.code, `${result.stdout}\n${result.stderr}`).toBe(0);
    const session = path.join(home, ".config", "blue", "session.json");
    expect((await stat(session)).mode & 0o777).toBe(0o600);
    const persistedSession = JSON.parse(await readFile(session, "utf8"));
    expect(persistedSession.refresh_token).toBeTruthy();
    const accessClaims = JSON.parse(
      Buffer.from(persistedSession.token.split(".")[1], "base64url").toString(
        "utf8",
      ),
    );
    expect(accessClaims.sid).toEqual(expect.any(String));
    expect(accessClaims.sid).not.toBe("");
    // The resource indicator has to survive into session.json, or the CLI's own
    // refresh asks for a token the Control API will not accept.
    expect(persistedSession.resource).toBeTruthy();

    // Age the access token and let the CLI refresh it. This used to be a
    // hand-rolled POST to the token endpoint — a second implementation of the
    // grant that happened to send `resource` when `refresh_if_needed` did not,
    // which is exactly why no test caught the omission. Only `expires_at` is
    // rewritten; every other field stays as the CLI wrote it.
    persistedSession.expires_at = Math.floor(Date.now() / 1000) - 60;
    await writeFile(session, JSON.stringify(persistedSession), { mode: 0o600 });

    await expect(stat(path.join(home, ".codex", "blue.config.toml"))).rejects.toThrow();
    await expect(stat(path.join(home, ".config", "blue", "runtime", "kimi", "config.toml"))).rejects.toThrow();
    // Any command that talks to the service drives the refresh.
    const preferred = await runCli(home, ["agent", "claude"]);
    expect(preferred.code, preferred.stderr).toBe(0);

    const refreshedSession = JSON.parse(await readFile(session, "utf8"));
    expect(refreshedSession.token).not.toBe(persistedSession.token);
    expect(refreshedSession.expires_at).toBeGreaterThan(
      Math.floor(Date.now() / 1000),
    );
    const refreshedClaims = JSON.parse(
      Buffer.from(refreshedSession.token.split(".")[1], "base64url").toString(
        "utf8",
      ),
    );
    // Same browser session behind it, and an audience the Control API accepts —
    // the two things a refresh without `resource` silently gets wrong.
    expect(refreshedClaims.sid).toBe(accessClaims.sid);
    expect(refreshedClaims.aud).toBeTruthy();
  });

  test("@smoke authenticated users are redirected away from login", async ({ page }) => {
    await loginAsAdmin(page);

    await page.goto("/login", { waitUntil: "commit" });
    await expect(page).toHaveURL(/\/sessions$/);
    await expect(page.getByLabel("Email")).toHaveCount(0);

    await page.goto("/login?callbackURL=%2Fgateway", { waitUntil: "commit" });
    await expect(page).toHaveURL(/\/gateway$/);
    await expect(page.getByLabel("Email")).toHaveCount(0);
  });

  test("@smoke executable provisioner creates managed gateway access", async () => {
    const result = await runCli(home, ["gateway"]);
    expect(result.code, result.stderr).toBe(0);
    expect(result.stdout).toContain("status          : ready");
    expect(result.stdout).toContain("alias           : e2e");
    expect(result.stdout).toContain("gateway id      : e2e-executable");

    const invocationPath = "/work/tests/e2e/artifacts/provisioner/invocations.jsonl";
    const invocationsBefore = (await readFile(invocationPath, "utf8")).trim().split("\n").length;
    const reused = await runCli(home, ["gateway"]);
    expect(reused.code, reused.stderr).toBe(0);
    expect(reused.stdout).toContain("status          : ready");
    const invocationsAfter = (await readFile(invocationPath, "utf8")).trim().split("\n").length;
    expect(invocationsAfter).toBe(invocationsBefore);
  });

  test("@smoke /direct tears down gateway wiring and offers an agent reload", async ({ page }) => {
    await loginAsAdmin(page);
    const control = process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080";
    const originalResponse = await page.request.get(`${control}/admin/governance-config`);
    expect(originalResponse.status(), await originalResponse.text()).toBe(200);
    const original = await originalResponse.json();
    const gatewayConfig = YAML.parse(original.managed_yaml);
    gatewayConfig.harnesses.codex.managed_config.model = "gpt-e2e";
    enableGateway(gatewayConfig);
    const gatewayResponse = await page.request.put(`${control}/admin/governance-config`, {
      data: {
        base_revision: original.revision,
        managed_yaml: YAML.stringify(gatewayConfig),
      },
    });
    expect(gatewayResponse.status(), await gatewayResponse.text()).toBe(200);

    const directHome = await prepareClient("direct-mode");
    await copyFile(
      path.join(home, ".config", "blue", "session.json"),
      path.join(directHome, ".config", "blue", "session.json"),
    );
    const direct = spawnCliInPty(directHome, "blue run codex -- direct-mode", {
      E2E_AGENT_READ_STDIN: "1",
    });
    const directCompletion = collect(direct);
    try {
      await waitForOutput(direct, /Ctrl-\] Control/);
      await expect
        .poll(async () =>
          readClientFile(directHome, "agent-log/codex.env").catch(() => ""),
        )
        .toMatch(/^env_HARNESS_CODEX_KEY=eyJ/m);
      expect(await readClientFile(directHome, ".codex/blue.config.toml")).toContain(
        'model_provider = "governed"',
      );
      direct.stdin.write("\u001d");
      await waitForOutput(direct, /Command/);
      const menuEntry = waitForOutput(direct, /Toggle personal provider credentials/);
      direct.stdin.write("/dir");
      await menuEntry;
      direct.stdin.write("\r");
      await waitForOutput(direct, /Keep current session/);
      direct.stdin.write("\r");
      await new Promise((resolve) => setTimeout(resolve, 250));
      direct.stdin.write("\u001d");
      await new Promise((resolve) => setTimeout(resolve, 250));
      direct.stdin.write("continue\r");
      const result = await directCompletion;
      expect(result.code, `${result.stdout}\n${result.stderr}`).toBe(0);
    } finally {
      if (direct.exitCode === null) direct.kill("SIGTERM");
    }

    expect(await readClientFile(directHome, ".config/blue/blue.toml")).toContain(
      "force_governance_only = true",
    );
    const directProfile = await readClientFile(directHome, ".codex/blue.config.toml");
    expect(directProfile).not.toContain('model_provider = "governed"');
    expect(directProfile).toContain('model = "gpt-e2e"');
    expect(await readClientFile(directHome, "agent-log/codex.env")).toContain(
      "env_HARNESS_CODEX_KEY=eyJ",
    );

    const directRestart = await runCli(directHome, ["run", "codex", "--", "verify-direct"]);
    expect(directRestart.code, directRestart.stderr).toBe(0);
    expect(await readClientFile(directHome, "agent-log/codex.env")).toContain(
      "env_HARNESS_CODEX_KEY=\n",
    );

    const gateway = spawnCliInPty(directHome, "blue run codex -- gateway-mode", {
      E2E_AGENT_READ_STDIN: "1",
    });
    const gatewayCompletion = collect(gateway);
    try {
      await waitForOutput(gateway, /Ctrl-\] Control/);
      gateway.stdin.write("\u001d");
      await waitForOutput(gateway, /Command/);
      gateway.stdin.write("/direct\r");
      await waitForOutput(gateway, /Keep current session/);
      gateway.stdin.write("\r");
      await new Promise((resolve) => setTimeout(resolve, 250));
      gateway.stdin.write("\u001d");
      await new Promise((resolve) => setTimeout(resolve, 250));
      gateway.stdin.write("continue\r");
      const result = await gatewayCompletion;
      expect(result.code, `${result.stdout}\n${result.stderr}`).toBe(0);
    } finally {
      if (gateway.exitCode === null) gateway.kill("SIGTERM");
    }

    expect(await readClientFile(directHome, ".config/blue/blue.toml")).toContain(
      "force_governance_only = false",
    );
    expect(await readClientFile(directHome, ".codex/blue.config.toml")).toContain(
      'model_provider = "governed"',
    );
    const gatewayRestart = await runCli(directHome, ["run", "codex", "--", "verify-gateway"]);
    expect(gatewayRestart.code, gatewayRestart.stderr).toBe(0);
    expect(await readClientFile(directHome, "agent-log/codex.env")).toMatch(
      /^env_HARNESS_CODEX_KEY=eyJ/m,
    );

    const currentResponse = await page.request.get(`${control}/admin/governance-config`);
    expect(currentResponse.status(), await currentResponse.text()).toBe(200);
    const current = await currentResponse.json();
    const restoreResponse = await page.request.put(`${control}/admin/governance-config`, {
      data: { base_revision: current.revision, managed_yaml: original.managed_yaml },
    });
    expect(restoreResponse.status(), await restoreResponse.text()).toBe(200);
  });

  test("guided setup discovers the deployment and completes device login", async ({ page }) => {
    await loginAsAdmin(page);
    const setupHome = await prepareEmptyClient("guided-setup");
    const child = spawnCliInPty(setupHome, "blue setup");
    const completion = collect(child);
    const deviceUrl = waitForOutput(child, /http:\/\/127\.0\.0\.1:3000\/device\/[A-Za-z0-9_-]+/, 30_000);
    setTimeout(() => child.stdin.write("http://127.0.0.1:8080\r"), 500);
    await page.goto(await deviceUrl);
    await page.getByRole("button", { name: "Authorize" }).click();
    await expect(page.getByText("CLI authorized")).toBeVisible();
    const result = await completion;
    expect(result.code, result.stderr).toBe(0);
    expect(await readClientFile(setupHome, ".config/blue/blue.toml")).toContain("http://127.0.0.1:8080");
    await expect(stat(path.join(setupHome, ".codex", "blue.config.toml"))).rejects.toThrow();
    const bare = spawnCliInPty(setupHome, "blue");
    const launched = collect(bare);
    await waitForOutput(bare, /Choose your coding agent/);
    bare.stdin.write("\u001b[B\r");
    const bareResult = await launched;
    expect(bareResult.code, bareResult.stderr).toBe(0);
    expect(bareResult.stdout).toContain("fake-claude-ok");
    expect(await readClientFile(setupHome, ".config/blue/blue.toml")).toContain('preferred_harness = "claude"');
    expect((await stat(path.join(setupHome, ".config", "blue", "runtime", "claude", "settings.json"))).isFile()).toBeTruthy();
    await expect(stat(path.join(setupHome, ".codex", "blue.config.toml"))).rejects.toThrow();
    await expect(stat(path.join(setupHome, ".config", "blue", "runtime", "kimi", "config.toml"))).rejects.toThrow();
    await expect(stat(path.join(setupHome, ".config", "blue", "runtime", "opencode", "opencode.json"))).rejects.toThrow();
  });

  test("@smoke first-run agent choice persists only after version repair succeeds", async () => {
    const repairHome = await prepareClient("declined-first-run-repair");
    await copyFile(
      path.join(home, ".config", "blue", "session.json"),
      path.join(repairHome, ".config", "blue", "session.json"),
    );
    const repairPrefix = path.join(repairHome, "repair npm prefix");
    const repairBin = path.join(repairPrefix, "bin");
    const packageRoot = path.join(repairPrefix, "lib", "node_modules", "@openai", "codex");
    const codexExecutable = path.join(packageRoot, "bin", "codex");
    const versionFile = path.join(repairHome, "codex-version");
    const installLog = path.join(repairHome, "npm-install.log");
    await mkdir(repairBin, { recursive: true });
    await mkdir(path.dirname(codexExecutable), { recursive: true });
    await writeFile(
      path.join(packageRoot, "package.json"),
      JSON.stringify({ name: "@openai/codex", bin: { codex: "bin/codex" } }),
    );
    await symlink(codexExecutable, path.join(repairBin, "codex"));
    await writeFile(versionFile, "999.0.0\n");
    await writeFile(
      codexExecutable,
      `#!/usr/bin/env bash
set -euo pipefail
if [[ "\${1:-}" == "--version" ]] || [[ "\${1:-}" == "version" ]]; then
  printf 'codex %s\\n' "$(cat "$E2E_CODEX_VERSION_FILE")"
  exit 0
fi
printf 'fake-codex-ok\\n'
`,
    );
    await writeFile(
      path.join(repairBin, "npm"),
      `#!/usr/bin/env bash
set -euo pipefail
case "\${1:-}" in
  view)
    [[ "$#" == 4 && "$2" == '@openai/codex@>=0.145.0 <0.151.1-0' && "$3" == version && "$4" == --json ]]
    printf '"0.145.0"\\n'
    ;;
  install)
    [[ "$#" == 5 && "$2" == -g && "$3" == --prefix && "$4" == "$E2E_NPM_PREFIX" && "$5" == @openai/codex@0.145.0 ]]
    printf '%s\\n' "$@" > "$E2E_INSTALL_LOG"
    printf '0.145.0\\n' > "$E2E_CODEX_VERSION_FILE"
    ;;
  *) exit 64 ;;
esac
`,
    );
    await Promise.all([
      chmod(codexExecutable, 0o755),
      chmod(path.join(repairBin, "npm"), 0o755),
    ]);
    const incompatibleVersions = {
      E2E_CODEX_VERSION: "999.0.0",
      E2E_CLAUDE_VERSION: "999.0.0",
      E2E_KIMI_VERSION: "999.0.0",
      E2E_OPENCODE_VERSION: "999.0.0",
      E2E_CODEX_VERSION_FILE: versionFile,
      E2E_INSTALL_LOG: installLog,
      E2E_NPM_PREFIX: repairPrefix,
      PATH: `${repairBin}:/usr/local/bin:${process.env.PATH ?? ""}`,
    };

    const declined = spawnCliInPty(repairHome, "blue", incompatibleVersions);
    const declinedCompletion = collect(declined);
    try {
      await waitForOutput(declined, /Choose your coding agent/);
      declined.stdin.write("\r");
      await waitForOutput(declined, /Install codex 0\.145\.0 with/);
      declined.stdin.write("\r");
      const result = await declinedCompletion;
      expect(result.code, `${result.stdout}\n${result.stderr}`).not.toBe(0);
      expect(`${result.stdout}\n${result.stderr}`).toContain("codex installation declined");
    } finally {
      if (declined.exitCode === null) declined.kill("SIGTERM");
    }

    expect(await readClientFile(repairHome, ".config/blue/blue.toml")).not.toContain(
      "preferred_harness",
    );

    expect(await readFile(versionFile, "utf8")).toBe("999.0.0\n");
    await expect(stat(installLog)).rejects.toThrow();

    const retried = spawnCliInPty(repairHome, "blue", incompatibleVersions);
    const retriedCompletion = collect(retried);
    try {
      await waitForOutput(retried, /Choose your coding agent/);
      retried.stdin.write("\r");
      await waitForOutput(retried, /Install codex 0\.145\.0 with/);
      retried.stdin.write("y\r");
      const result = await retriedCompletion;
      expect(result.code, `${result.stdout}\n${result.stderr}`).toBe(0);
      expect(result.stdout).toContain("fake-codex-ok");
    } finally {
      if (retried.exitCode === null) retried.kill("SIGTERM");
    }

    expect(await readFile(versionFile, "utf8")).toBe("0.145.0\n");
    expect(await readFile(installLog, "utf8")).toBe(
      ["install", "-g", "--prefix", repairPrefix, "@openai/codex@0.145.0", ""].join("\n"),
    );
    expect(await readClientFile(repairHome, ".config/blue/blue.toml")).toContain(
      'preferred_harness = "codex"',
    );

    await writeFile(versionFile, "999.0.0\n");
    await writeFile(codexExecutable, (await readFile(codexExecutable, "utf8")) + "\n");
    await rm(installLog);
    for (const command of ["blue apply --yes", "blue daemon"]) {
      const child = spawnCliInPty(repairHome, command, incompatibleVersions);
      const result = await collect(child);
      expect(result.code, result.stdout + result.stderr).not.toBe(0);
      await expect(stat(installLog)).rejects.toThrow();
    }
    const pipedApply = await runCli(repairHome, ["apply"], incompatibleVersions);
    expect(pipedApply.code).not.toBe(0);
    await expect(stat(installLog)).rejects.toThrow();
    const apply = spawnCliInPty(repairHome, "blue apply", incompatibleVersions);
    const applied = collect(apply);
    try {
      await waitForOutput(apply, /Install codex 0\.145\.0 with/);
      apply.stdin.write("y\r");
      const result = await applied;
      expect(result.code, result.stdout + result.stderr).toBe(0);
      expect(await readFile(versionFile, "utf8")).toBe("0.145.0\n");
    } finally {
      if (apply.exitCode === null) apply.kill("SIGTERM");
    }

  });

  test("@smoke absent agents install only after terminal confirmation", async () => {
    const installHome = await prepareClient("absent-agent-install");
    await copyFile(path.join(home, ".config/blue/session.json"), path.join(installHome, ".config/blue/session.json"));
    const configPath = path.join(installHome, ".config/blue/blue.toml");
    const originalConfig = await readFile(configPath, "utf8");
    const bin = path.join(installHome, "isolated-bin");
    const log = path.join(installHome, "installer.log");
    await mkdir(bin);
    // An explicit allowlist excludes all preinstalled harnesses and the real npm.
    for (const [name, target] of Object.entries({ blue: "/usr/local/bin/blue", script: "/usr/bin/script", sh: "/bin/sh", bash: "/bin/bash", cat: "/bin/cat", chmod: "/bin/chmod" })) {
      await symlink(target, path.join(bin, name));
    }
    const agent = path.join(bin, "codex");
    await writeFile(path.join(bin, "npm"), `#!/bin/bash
set -euo pipefail
case "\${1:-}" in
  view)
    [[ "$#" == 4 && "$2" == '@openai/codex@>=0.145.0 <0.151.1-0' && "$3" == version && "$4" == --json ]]
    printf '"0.145.0"\\n'
    ;;
  install)
    [[ "$#" == 3 && "$2" == -g && "$3" == @openai/codex@0.145.0 ]]
    printf '%s\\n' "$@" > "$E2E_INSTALL_LOG"
    cat > "$E2E_INSTALLED_AGENT" <<'AGENT'
#!/bin/sh
case "$1" in
  --version|version) echo 'codex 0.145.0';;
  *) echo 'fresh-codex-ok';;
esac
AGENT
    chmod +x "$E2E_INSTALLED_AGENT"
    ;;
  *) exit 64 ;;
esac
`);
    await chmod(path.join(bin, "npm"), 0o755);
    const env = { PATH: bin, SHELL: "/bin/sh", E2E_INSTALL_LOG: log, E2E_INSTALLED_AGENT: agent };
    for (const args of [["codex"], ["agent", "codex"]]) {
      const result = await runCli(installHome, args, env);
      expect(result.code, result.stdout + result.stderr).not.toBe(0);
    }
    await expect(stat(log)).rejects.toThrow();
    await expect(stat(agent)).rejects.toThrow();

    const choose = async (command: string, accept: boolean, picker: boolean) => {
      const child = spawnCliInPty(installHome, command, env);
      const completion = collect(child);
      try {
        if (picker) {
          await waitForOutput(child, /Choose your (?:default )?coding agent/);
          child.stdin.write("\r");
        }
        await waitForOutput(child, /Install codex 0\.145\.0 with/);
        expect(await readFile(configPath, "utf8")).toBe(originalConfig);
        await expect(stat(path.join(installHome, ".codex/blue.config.toml"))).rejects.toThrow();
        child.stdin.write(accept ? "y\r" : "\r");
        return await completion;
      } finally {
        if (child.exitCode === null) child.kill("SIGTERM");
      }
    };
    const declined = await choose("blue", false, true);
    expect(declined.code).not.toBe(0);
    expect(declined.stdout + declined.stderr).toContain("installation declined");
    expect(await readFile(configPath, "utf8")).toBe(originalConfig);
    await expect(stat(log)).rejects.toThrow();

    const direct = await choose("blue codex", true, false);
    expect(direct.code, direct.stdout + direct.stderr).toBe(0);
    expect(direct.stdout).toContain("fresh-codex-ok");
    expect(await readFile(log, "utf8")).toBe("install\n-g\n@openai/codex@0.145.0\n");
    expect(await readFile(configPath, "utf8")).toBe(originalConfig);

    // Reset only this fixture's artifacts for both all-absent selection surfaces.
    for (const command of ["blue agent", "blue"]) {
      await rm(agent);
      await rm(path.join(installHome, ".codex"), { recursive: true, force: true });
      await writeFile(configPath, originalConfig);
      const result = await choose(command, true, true);
      expect(result.code, result.stdout + result.stderr).toBe(0);
      expect(await readFile(configPath, "utf8")).toContain('preferred_harness = "codex"');
      if (command === "blue") expect(result.stdout).toContain("fresh-codex-ok");
    }
  });

  test("@smoke CLI applies policy, reports health, and launches Codex transparently", async () => {
    const control = process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080";
    for (const args of [["version"], ["help"], ["doctor"], ["config"], ["apply", "--yes"], ["status"], ["verify"]]) {
      const result = await runCli(home, args);
      expect(result.code, `${args.join(" ")}\n${result.stderr}`).toBe(0);
      if (args[0] === "status") {
        expect(result.stdout).toContain(`Tenant URL     : ${control}`);
      }
      if (args[0] === "doctor") {
        expect(result.stdout).toContain(`config source : ${control}/governance-config`);
        expect(result.stdout).not.toContain(`http(${control}/governance-config)`);
      }
    }
    const launched = await runCli(home, ["run", "codex", "--", "hello world"]);
    expect(launched.code, launched.stderr).toBe(0);
    expect(launched.stdout).toContain("fake-codex-ok");
    const log = await readClientFile(home, "agent-log/codex.env");
    expect(log).toContain("hello world");
    expect(log).toContain("env_OPENAI_API_KEY=");
    expect(await readClientFile(home, ".codex/blue.config.toml")).toContain("gpt-e2e");
    await expect(stat(path.join(home, ".config", "blue", "runtime", "kimi", "config.toml"))).rejects.toThrow();
    await expect(stat(path.join(home, ".config", "blue", "runtime", "opencode", "opencode.json"))).rejects.toThrow();
  });

  test("reapplying an unchanged revision preserves managed bytes and digests", async () => {
    const managedPath = path.join(home, ".codex", "blue.config.toml");
    const statePath = path.join(home, ".config", "blue", "applied-state.json");
    const beforeManaged = await readFile(managedPath);
    const beforeState = JSON.parse(await readFile(statePath, "utf8"));

    const reapplied = await runCli(home, ["apply", "--yes"]);
    expect(reapplied.code, reapplied.stderr).toBe(0);

    const afterManaged = await readFile(managedPath);
    const afterState = JSON.parse(await readFile(statePath, "utf8"));
    expect(afterManaged.equals(beforeManaged)).toBe(true);
    expect(afterState.revision).toBe(beforeState.revision);
    expect(afterState.files).toEqual(beforeState.files);
    expect(afterState.harnesses).toEqual(beforeState.harnesses);
  });

  test("@smoke hostile package sources fail closed and a clean package revision still applies", async ({ page }) => {
    await loginAsAdmin(page);
    const control = process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080";
    const originalResponse = await page.request.get(`${control}/admin/governance-config`);
    expect(originalResponse.status(), await originalResponse.text()).toBe(200);
    const original = await originalResponse.json();
    const hostileHome = await prepareClient("hostile-package");
    await copyFile(
      path.join(home, ".config", "blue", "session.json"),
      path.join(hostileHome, ".config", "blue", "session.json"),
    );
    const preferred = await runCli(hostileHome, ["agent", "codex"]);
    expect(preferred.code, preferred.stderr).toBe(0);
    const archivePath = path.join(hostileHome, "oversized-metadata.tar.gz");
    const archive = oversizedGnuLongNameArchive();
    await writeFile(archivePath, archive);
    const adapter = { codex: { skills_dir: "payload" } };

    try {
      const privateSource = {
        id: "private-source",
        version: "1.0.0",
        source_ref: "https://127.0.0.1/private.tar.gz",
        sha256: "a".repeat(64),
        adapters: adapter,
      };
      const privateUpdate = await page.request.put(`${control}/admin/governance-extensions`, {
        data: extensionPayload(original.document, [privateSource], original.revision),
      });
      expect(privateUpdate.status(), await privateUpdate.text()).toBe(200);
      const privateApply = await runCli(hostileHome, ["apply", "--yes"]);
      expect(privateApply.code).not.toBe(0);
      expect(`${privateApply.stdout}\n${privateApply.stderr}`).toMatch(/public (host|addresses)/i);

      const afterPrivate = await privateUpdate.json();
      const metadataSource = {
        id: "oversized-metadata",
        version: "1.0.0",
        source_ref: `file://${archivePath}`,
        sha256: createHash("sha256").update(archive).digest("hex"),
        adapters: adapter,
      };
      const metadataUpdate = await page.request.put(`${control}/admin/governance-extensions`, {
        data: extensionPayload(afterPrivate.document, [metadataSource], afterPrivate.revision),
      });
      expect(metadataUpdate.status(), await metadataUpdate.text()).toBe(200);
      const metadataApply = await runCli(hostileHome, ["apply", "--yes"]);
      expect(metadataApply.code).not.toBe(0);
      expect(`${metadataApply.stdout}\n${metadataApply.stderr}`).toContain("archive metadata limit");
      await expect(stat(path.join(hostileHome, ".codex", "blue.config.toml"))).rejects.toThrow();
      const transactionRoot = path.join(hostileHome, ".blue-transactions");
      expect(await readdir(transactionRoot).catch(() => [])).toHaveLength(0);
    } finally {
      const currentResponse = await page.request.get(`${control}/admin/governance-config`);
      expect(currentResponse.status(), await currentResponse.text()).toBe(200);
      const current = await currentResponse.json();
      const restore = await page.request.put(`${control}/admin/governance-extensions`, {
        data: extensionPayload(
          original.document,
          original.document.packages,
          current.revision,
          original.package_audiences,
        ),
      });
      expect(restore.status(), await restore.text()).toBe(200);
    }

    const recovered = await runCli(hostileHome, ["apply", "--yes"]);
    expect(recovered.code, `${recovered.stdout}\n${recovered.stderr}`).toBe(0);
    expect(await readClientFile(hostileHome, ".codex/blue.config.toml")).toContain("gpt-e2e");
    expect((await runCli(hostileHome, ["verify"])).code).toBe(0);
  });

  test("@smoke governance-only deployment does not attempt gateway provisioning", async ({ request }) => {
    const control = process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080";
    const governanceControl = "http://blue-governance-only:8080";
    const adminSession = JSON.parse(await readFile(path.join(home, ".config", "blue", "session.json"), "utf8"));
    const adminHeaders = { authorization: `Bearer ${adminSession.token}` };
    const activeResponse = await request.get(`${governanceControl}/admin/governance-config`, {
      headers: adminHeaders,
    });
    expect(activeResponse.status(), await activeResponse.text()).toBe(200);
    const active = await activeResponse.json();
    const governanceOnly = YAML.parse(active.managed_yaml);
    delete governanceOnly.gateway;
    const disableResponse = await request.put(`${governanceControl}/admin/governance-config`, {
      headers: adminHeaders,
      data: { base_revision: active.revision, managed_yaml: YAML.stringify(governanceOnly) },
    });
    expect(disableResponse.status(), await disableResponse.text()).toBe(200);

    try {
      const governanceHome = await prepareClient("governance-only");
      const configPath = path.join(governanceHome, ".config", "blue", "blue.toml");
      await writeFile(
        configPath,
        (await readFile(configPath, "utf8")).replace("http://127.0.0.1:8080", "http://blue-governance-only:8080"),
      );
      await copyFile(
        path.join(home, ".config", "blue", "session.json"),
        path.join(governanceHome, ".config", "blue", "session.json"),
      );

      const session = JSON.parse(await readFile(path.join(governanceHome, ".config", "blue", "session.json"), "utf8"));
      const ensured = await request.post("http://blue-governance-only:8080/gateway/key/ensure", {
        headers: { authorization: `Bearer ${session.token}` },
        data: {},
      });
      expect(ensured.status(), await ensured.text()).toBe(200);
      expect(await ensured.json()).toMatchObject({ enabled: false, status: "disabled" });

      for (const args of [["config"], ["agent", "codex"], ["apply", "--yes"], ["status"]]) {
        const result = await runCli(governanceHome, args);
        expect(result.code, `${args.join(" ")}\n${result.stderr}`).toBe(0);
        expect(result.stderr).not.toContain("gateway mode is not enabled");
      }
      const launched = await runCli(governanceHome, ["run", "codex", "--", "governance-only"]);
      expect(launched.code, launched.stderr).toBe(0);
      expect(launched.stdout).toContain("fake-codex-ok");
      const log = await readClientFile(governanceHome, "agent-log/codex.env");
      expect(log).toContain("env_HARNESS_CODEX_KEY=\n");
      expect(log).toContain("env_OPENAI_BASE_URL=\n");
    } finally {
      const currentResponse = await request.get(`${control}/admin/governance-config`, {
        headers: adminHeaders,
      });
      expect(currentResponse.status(), await currentResponse.text()).toBe(200);
      const current = await currentResponse.json();
      const restoreResponse = await request.put(`${control}/admin/governance-config`, {
        headers: adminHeaders,
        data: { base_revision: current.revision, managed_yaml: active.managed_yaml },
      });
      expect(restoreResponse.status(), await restoreResponse.text()).toBe(200);
    }
  });

  test("@smoke dashboard mutation produces a stale client revision that apply repairs", async ({ page }) => {
    await loginAsAdmin(page);
    const control = process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080";
    const currentResponse = await page.request.get(`${control}/admin/governance-config`);
    expect(currentResponse.status()).toBe(200);
    const current = await currentResponse.json();
    const changed = String(current.managed_yaml).replace(/revision:\s*[^\n]+/, `revision: "e2e-${Date.now()}"`);
    const update = await page.request.put(`${control}/admin/governance-config`, {
      data: { base_revision: current.revision, managed_yaml: changed },
    });
    expect(update.status(), await update.text()).toBe(200);
    const stale = await runCli(home, ["verify"]);
    expect(stale.code).not.toBe(0);
    const apply = await runCli(home, ["apply", "--yes"]);
    expect(apply.code, apply.stderr).toBe(0);
    expect((await runCli(home, ["verify"])).code).toBe(0);
  });

  test("full harness matrix preserves argv and generates isolated overlays", async () => {
    const expectedFiles: Record<string, string> = {
      codex: ".codex/blue.config.toml",
      claude: ".config/blue/runtime/claude/settings.json",
      kimi: ".config/blue/runtime/kimi/config.toml",
      opencode: ".config/blue/runtime/opencode/opencode.json",
    };
    for (const [harness, file] of Object.entries(expectedFiles)) {
      const result = await runCli(home, [harness, "--e2e-flag", "value with spaces"]);
      expect(result.code, `${harness}: ${result.stderr}`).toBe(0);
      expect(result.stdout).toContain(`fake-${harness}-ok`);
      expect(await readClientFile(home, `agent-log/${harness}.env`)).toContain("value with spaces");
      expect((await stat(path.join(home, file))).isFile()).toBeTruthy();
      const compatibilityState = JSON.parse(
        await readClientFile(
          home,
          `.config/blue/runtime/${harness}/compatibility-state.json`,
        ),
      );
      expect(compatibilityState.schema_version).toBe(4);
      expect(compatibilityState.profile_id).toBe(canonicalProfiles[harness]);
    }
    const status = await runCli(home, ["status"]);
    expect(status.stdout).toContain("e2e-skill");
    expect(status.stdout).toContain("applied");
    expect(await readClientFile(home, ".config/blue/runtime/kimi/skills/example/SKILL.md")).toContain("E2E managed skill fixture");
  });

  test("Codex compatibility boundaries select exact profiles and retire legacy state", async () => {
    const boundaryHome = await prepareClient("codex-boundary");
    const boundaryBin = path.join(boundaryHome, "bin");
    const boundaryCodex = path.join(boundaryBin, "codex");
    const boundaryPath = `${boundaryBin}:${process.env.PATH ?? ""}`;
    await mkdir(boundaryBin, { recursive: true });
    await writeFile(
      boundaryCodex,
      "#!/bin/sh\nif [ \"${1:-}\" = \"--version\" ] || [ \"${1:-}\" = \"version\" ]; then printf 'codex 0.144.99\\n'; exit 0; fi\nexec /usr/local/bin/codex \"$@\"\n",
    );
    await chmod(boundaryCodex, 0o755);
    await copyFile(
      path.join(home, ".config", "blue", "session.json"),
      path.join(boundaryHome, ".config", "blue", "session.json"),
    );

    const legacy = await runCli(
      boundaryHome,
      ["run", "codex", "--", "legacy-boundary"],
      { PATH: boundaryPath },
    );
    expect(legacy.code, legacy.stderr).toBe(0);
    const statePath = ".config/blue/runtime/codex/compatibility-state.json";
    const legacyState = JSON.parse(await readClientFile(boundaryHome, statePath));
    expect(legacyState).toMatchObject({
      schema_version: 4,
      profile_id: "codex-v0_114_0",
    });
    expect(await readClientFile(boundaryHome, ".codex/blue.config.toml")).not.toContain(
      "session-upload",
    );

    legacyState.profile_id = "codex-v1";
    await writeFile(path.join(boundaryHome, statePath), JSON.stringify(legacyState));
    await writeFile(
      path.join(boundaryHome, ".codex", "blue.config.toml"),
      "# force reconciliation from legacy compatibility state\n",
    );
    await writeFile(
      boundaryCodex,
      "#!/bin/sh\n# upgraded binary fingerprint\nif [ \"${1:-}\" = \"--version\" ] || [ \"${1:-}\" = \"version\" ]; then printf 'codex 0.145.0\\n'; exit 0; fi\nexec /usr/local/bin/codex \"$@\"\n",
    );
    const current = await runCli(
      boundaryHome,
      ["run", "codex", "--", "current-boundary"],
      { PATH: boundaryPath },
    );
    expect(current.code, current.stderr).toBe(0);
    expect(JSON.parse(await readClientFile(boundaryHome, statePath))).toMatchObject({
      schema_version: 4,
      profile_id: "codex-v0_145_0",
    });
    expect(await readClientFile(boundaryHome, ".codex/blue.config.toml")).toContain(
      "session-upload",
    );
  });

  test("PTY launch preserves stdin and native exit status", async () => {
    const child = spawnCli(home, ["run", "codex", "--", "stdin-check"], {
      E2E_AGENT_READ_STDIN: "1",
      E2E_AGENT_EXIT_CODE: "17",
    });
    const result = collect(child);
    child.stdin.end("hello from stdin\n");
    const completed = await result;
    expect(completed.code).toBe(17);
    expect(completed.stdout).toContain("fake-codex-stdin:hello from stdin");
  });

  test("@smoke preferred agent and bare blue preserve fragmented TUI output", async () => {
    expect((await runCli(home, ["agent", "claude"])).code).toBe(0);
    const launched = await runBareCliInPty(home, {
      E2E_AGENT_FRAGMENTED_ANSI: "1",
      E2E_AGENT_FRAGMENTED_CLEAR: "1",
    });
    expect(launched.code, launched.stderr).toBe(0);
    expect(launched.stdout).toContain("fake-claude-ok");
    const startupAt = launched.stdout.indexOf("Starting claude");
    const handoffAt = launched.stdout.indexOf(
      "\u001b[?2026l\u001b[?1049l\u001b[r\u001b[2J\u001b[H\u001b[?25h",
      startupAt,
    );
    const agentAt = launched.stdout.indexOf("fake-claude-ok");
    expect(startupAt).toBeGreaterThanOrEqual(0);
    expect(handoffAt).toBeGreaterThan(startupAt);
    expect(agentAt).toBeGreaterThan(handoffAt);
    expect(launched.stdout).toContain("\u001b[38;2;1;2;3mBLUE_ANSI_OK\u001b[0m");
    const clearAt = launched.stdout.indexOf("\u001b[2J", launched.stdout.indexOf("BLUE_ANSI_OK"));
    expect(clearAt).toBeGreaterThanOrEqual(0);
    expect(launched.stdout.indexOf("Ctrl-] Control", clearAt)).toBeGreaterThan(clearAt);
    expect(await readClientFile(home, ".config/blue/blue.toml")).toContain('preferred_harness = "claude"');
    const noninteractive = await runCli(home, ["agent"]);
    expect(noninteractive.code).toBe(1);
    expect(noninteractive.stderr).toContain("requires a name");
  });

  test("PTY teardown restores terminal modes leaked by an agent", async () => {
    const launched = await runBareCliInPty(home, { E2E_AGENT_LEAK_TERMINAL_MODES: "1" });
    expect(launched.code, launched.stderr).toBe(0);
    const enabledAt = launched.stdout.indexOf("\u001b[?1003h");
    const exitedAt = launched.stdout.indexOf("fake-claude-ok");
    expect(enabledAt).toBeGreaterThanOrEqual(0);
    expect(exitedAt).toBeGreaterThan(enabledAt);
    for (const reset of ["\u001b[r", "\u001b[?1003l", "\u001b[?1006l", "\u001b[?1004l", "\u001b[?2004l", "\u001b[<u", "\u001b[=0u", "\u001b[?25h"]) {
      expect(launched.stdout.lastIndexOf(reset), `missing terminal reset ${JSON.stringify(reset)}`).toBeGreaterThan(exitedAt);
    }
  });

  test("supervisor commands report live state without terminating the agent", async () => {
    const child = spawnCliInPty(home, "blue run claude -- supervisor-commands", {
      E2E_AGENT_READ_STDIN: "1",
      E2E_AGENT_EXIT_CODE: "23",
    });
    const completion = collect(child);
    try {
      await waitForOutput(child, /Ctrl-\] Control/);
      child.stdin.write("\u001d");
      await waitForOutput(child, /Command/);

      const commands: Array<[string, RegExp]> = [
        ["/status", /Overall/],
        ["/health", /healthy/],
        ["/version", /metaharness/],
        ["/gateway", /alias/],
        ["/doctor", /Harnesses/],
        ["/help", /Commands:/],
      ];
      for (const [command, output] of commands) {
        child.stdin.write(`${command}\r`);
        await waitForOutput(child, output);
        expect(child.exitCode, `${command} terminated the running agent`).toBeNull();
      }

      child.stdin.write("/agent\r");
      await waitForOutput(child, /default/);
      expect(child.exitCode).toBeNull();
      child.stdin.write("\r");
      await waitForOutput(child, /already/);

      child.stdin.write("/apply\r");
      await waitForOutput(child, /configuration/);
      child.stdin.write("\u001b[B\r");
      await waitForOutput(child, /applied/);
      expect(child.exitCode).toBeNull();

      child.stdin.write("/quit\r");
      await waitForOutput(child, /exit/);
      child.stdin.write("\u001b[B\r");
      const result = await completion;
      expect(result.code, `${result.stdout}\n${result.stderr}`).toBe(23);
    } finally {
      if (child.exitCode === null) child.kill("SIGTERM");
    }
  });

  test("portable sessions round-trip through storage and native restore for every harness", async ({ page }) => {
    await loginAsAdmin(page);
    const transcripts = new Map<string, { path: string; content: string }>();
    for (const harness of ["codex", "claude", "kimi", "opencode"]) {
      const sessionId = `e2e-${harness}`;
      const transcript = harness === "codex"
        ? path.join(home, ".codex", "sessions", "2026", "09", "05", `rollout-${sessionId}.jsonl`)
        : harness === "claude"
          ? path.join(home, ".claude", "projects", "e2e", `${sessionId}.jsonl`)
          : harness === "kimi"
            ? path.join(home, ".config", "blue", "runtime", "kimi", "sessions", "e2e", sessionId, "agents", "main", "wire.jsonl")
            : path.join(home, `${sessionId}-export.json`);
      const content = harness === "opencode"
        ? JSON.stringify({
            info: { id: sessionId, title: "e2e-opencode" },
            messages: [{ info: { role: "user" }, parts: [{ type: "text", text: "e2e-opencode" }] }],
          })
        : `${JSON.stringify({ role: "user", content: `e2e-${harness}` })}\n`;
      transcripts.set(harness, { path: transcript, content });
      await mkdir(path.dirname(transcript), { recursive: true });
      await writeFile(transcript, content);
      if (harness === "claude") {
        const companion = path.join(path.dirname(transcript), sessionId, "subagents", "agent-1.jsonl");
        await mkdir(path.dirname(companion), { recursive: true });
        await writeFile(companion, `${JSON.stringify({ role: "assistant", content: "claude companion" })}\n`);
      }
      if (harness === "kimi") {
        const sessionRoot = path.resolve(path.dirname(transcript), "..", "..");
        await writeFile(path.join(sessionRoot, "state.json"), JSON.stringify({
          conversation: { ready: true },
          access_token: "must-not-leave-source",
          approvals: ["must-not-restore"],
        }));
        await mkdir(path.join(sessionRoot, "plans"), { recursive: true });
        await writeFile(path.join(sessionRoot, "plans", "plan.md"), "safe plan");
      }
      const result = await runCliWithInput(
        home,
        ["session-upload", harness],
        JSON.stringify({
          session_id: sessionId,
          transcript_path: transcript,
          cwd: "/workspace/e2e",
          profile: canonicalProfiles[harness],
        }),
      );
      expect(result.code, `${harness}: ${result.stderr}`).toBe(0);
    }
    await expect.poll(async () => {
      const response = await page.request.get(`${process.env.E2E_CONTROL_API_URL}/session-uploads?per_page=25`);
      return (await response.json()).items.length;
    }, { timeout: 30_000 }).toBe(4);
    const response = await page.request.get(`${process.env.E2E_CONTROL_API_URL}/session-uploads?per_page=25`);
    expect(response.status()).toBe(200);
    const sessions = await response.json();
    expect(sessions.items).toHaveLength(4);
    expect(sessions.items.map((item: { harness: string }) => item.harness).sort()).toEqual([
      "claude",
      "codex",
      "kimi",
      "opencode",
    ]);
    const codex = sessions.items.find((item: { harness: string }) => item.harness === "codex");
    adminSessionId = codex.id;
    const detail = await page.request.get(`${process.env.E2E_CONTROL_API_URL}/session-uploads/${codex.id}`);
    expect(detail.status()).toBe(200);
    expect((await detail.json()).artifacts[0].status).toBe("complete");
    for (const session of sessions.items as Array<{ id: string; harness: string; artifact_format: string; resumable: boolean }>) {
      expect(session.artifact_format).toBe("blue-session-bundle-v1");
      expect(session.resumable).toBe(true);
      const download = await page.request.post(`${process.env.E2E_CONTROL_API_URL}/session-uploads/${session.id}/download`);
      const downloadBody = await download.json();
      expect(download.status()).toBe(200);
      const stored = await page.request.get(downloadBody.download_url);
      expect(stored.status(), await stored.text()).toBe(200);
      const bundlePath = path.join(home, "downloads", `${session.harness}.bundle.tgz`);
      await mkdir(path.dirname(bundlePath), { recursive: true });
      await writeFile(bundlePath, await stored.body());

      const restoreHome = await prepareEmptyClient(`restore-${session.harness}`);
      const preflight = await runCli(restoreHome, ["session-restore", "--bundle", bundlePath, "--preflight"]);
      expect(preflight.code, `${session.harness} preflight: ${preflight.stderr}`).toBe(0);
      const restored = await runCli(restoreHome, ["session-restore", "--bundle", bundlePath]);
      expect(restored.code, `${session.harness} restore: ${restored.stderr}`).toBe(0);
      const result = JSON.parse(restored.stdout);
      expect(result.harness).toBe(session.harness);
      const expectedId = session.harness === "opencode" ? "e2e-opencode-imported" : `e2e-${session.harness}`;
      expect(result.launch_args).toContain(expectedId);

      const source = transcripts.get(session.harness)!;
      if (session.harness !== "opencode") {
        const nativeRelative = path.relative(home, source.path);
        expect(await readClientFile(restoreHome, nativeRelative)).toBe(source.content);
      }
      if (session.harness === "claude") {
        expect(await readClientFile(restoreHome, ".claude/projects/e2e/e2e-claude/subagents/agent-1.jsonl")).toContain("claude companion");
        expect(await readClientFile(restoreHome, ".claude/projects/e2e/sessions-index.json")).toContain("e2e-claude");
      }
      if (session.harness === "kimi") {
        const state = await readClientFile(restoreHome, ".config/blue/runtime/kimi/sessions/e2e/e2e-kimi/state.json");
        expect(state).toContain("conversation");
        expect(state).not.toContain("must-not-leave-source");
        expect(state).not.toContain("approvals");
        expect(await readClientFile(restoreHome, ".config/blue/runtime/kimi/sessions/e2e/e2e-kimi/plans/plan.md")).toBe("safe plan");
        expect(await readClientFile(restoreHome, ".config/blue/runtime/kimi/session_index.jsonl")).toContain("e2e-kimi");
      }
    }
    await page.goto(`/sessions/${codex.id}`);
    await expect(page.getByRole("heading", { name: "e2e-codex" })).toBeVisible();
  });

  // Kimi keeps its transcripts in Blue's managed runtime rather than a
  // harness-native directory, so it is the one harness whose session artifacts
  // move when the runtime does. Every other test here runs with
  // XDG_CONFIG_HOME = $HOME/.config, where the runtime's portable wire root and
  // its physical location coincide and the distinction cannot fail.
  test("Kimi sessions stay portable when the managed runtime is outside $HOME", async ({ page }) => {
    await loginAsAdmin(page);
    const uploader = await prepareRelocatedClient("kimi-xdg-uploader");
    for (const file of ["blue.toml", "session.json"]) {
      await copyFile(path.join(home, ".config", "blue", file), path.join(uploader.configHome, "blue", file));
    }

    // The session lives under the *relocated* runtime, which is not under $HOME.
    const sessionId = "e2e-kimi-xdg";
    const sessionRoot = path.join(uploader.configHome, "blue", "runtime", "kimi", "sessions", "e2e", sessionId);
    const transcript = path.join(sessionRoot, "agents", "main", "wire.jsonl");
    const content = `${JSON.stringify({ role: "user", content: sessionId })}\n`;
    await mkdir(path.dirname(transcript), { recursive: true });
    await writeFile(transcript, content);
    await writeFile(path.join(sessionRoot, "state.json"), JSON.stringify({ conversation: { ready: true } }));

    // Capture alone is the regression: deriving the wire path by stripping $HOME
    // yields nothing here, and the manifest contract rejects an artifact with no
    // native destination, so this exits non-zero if the root is not portable.
    const uploaded = await runCliWithInput(
      uploader.home,
      ["session-upload", "kimi"],
      JSON.stringify({
        session_id: sessionId,
        transcript_path: transcript,
        cwd: "/workspace/e2e",
        profile: canonicalProfiles.kimi,
      }),
      relocatedEnv(uploader),
    );
    expect(uploaded.code, uploaded.stderr).toBe(0);

    const listUrl = `${process.env.E2E_CONTROL_API_URL}/session-uploads?per_page=50`;
    await expect.poll(async () => {
      const items = (await (await page.request.get(listUrl)).json()).items as Array<{ native_session_id: string }>;
      return items.some((item) => item.native_session_id === sessionId);
    }, { timeout: 30_000 }).toBe(true);
    const items = (await (await page.request.get(listUrl)).json()).items as Array<{ id: string; native_session_id: string }>;
    const id = items.find((item) => item.native_session_id === sessionId)!.id;
    const download = await page.request.post(`${process.env.E2E_CONTROL_API_URL}/session-uploads/${id}/download`);
    expect(download.status()).toBe(200);
    const stored = await page.request.get((await download.json()).download_url);
    expect(stored.status(), await stored.text()).toBe(200);
    const bundlePath = path.join(home, "downloads", "kimi-xdg.bundle.tgz");
    await mkdir(path.dirname(bundlePath), { recursive: true });
    await writeFile(bundlePath, await stored.body());

    // A bundle captured from a relocated runtime restores into the default
    // layout: the recorded root is portable, not this machine's.
    const defaultHome = await prepareEmptyClient("kimi-xdg-restore-default");
    const intoDefault = await runCli(defaultHome, ["session-restore", "--bundle", bundlePath]);
    expect(intoDefault.code, intoDefault.stderr).toBe(0);
    expect(await readClientFile(defaultHome, `.config/blue/runtime/kimi/sessions/e2e/${sessionId}/agents/main/wire.jsonl`)).toBe(content);

    // And into another relocated layout, where it must follow XDG rather than
    // the home directory the wire path nominally names.
    const restorer = await prepareRelocatedClient("kimi-xdg-restore-relocated");
    const intoRelocated = await runCli(restorer.home, ["session-restore", "--bundle", bundlePath], relocatedEnv(restorer));
    expect(intoRelocated.code, intoRelocated.stderr).toBe(0);
    expect(await readFile(path.join(restorer.configHome, "blue", "runtime", "kimi", "sessions", "e2e", sessionId, "agents", "main", "wire.jsonl"), "utf8")).toBe(content);
    expect(await readFile(path.join(restorer.configHome, "blue", "runtime", "kimi", "session_index.jsonl"), "utf8")).toContain(sessionId);
    await expect(stat(path.join(restorer.home, ".config", "blue", "runtime"))).rejects.toThrow();
  });

  // Restore walks a destination component by component and refuses to traverse a
  // symlink. The walk starts at the root that authorises the path, so a
  // dotfile-managed ~/.config no longer blocks Blue's own managed runtime — but
  // a symlinked *native* harness root, which is how a bundle escapes the home
  // directory, must still be refused.
  test("session restore tolerates a symlinked managed root but not a symlinked native one", async () => {
    const managed = await prepareEmptyClient("restore-symlinked-managed");
    const elsewhere = path.join(stateRoot(), "restore-symlinked-managed-target");
    await mkdir(path.join(elsewhere, "blue"), { recursive: true });
    await rm(path.join(managed, ".config"), { recursive: true, force: true });
    await symlink(elsewhere, path.join(managed, ".config"));
    const kimi = await runCli(managed, ["session-restore", "--bundle", path.join(home, "downloads", "kimi.bundle.tgz"), "--preflight"]);
    expect(kimi.code, kimi.stderr).toBe(0);

    const native = await prepareEmptyClient("restore-symlinked-native");
    await mkdir(path.join(stateRoot(), "restore-symlinked-native-target"), { recursive: true });
    await symlink(path.join(stateRoot(), "restore-symlinked-native-target"), path.join(native, ".codex"));
    const codex = await runCli(native, ["session-restore", "--bundle", path.join(home, "downloads", "codex.bundle.tgz"), "--preflight"]);
    expect(codex.code).not.toBe(0);
    expect(codex.stderr).toContain("symlink");
  });

  // `blue login` in a container that sets both XDG roots and no $HOME: the
  // profile answers for nothing those roots do not already cover, so demanding
  // it up front only broke the paths that were fully specified.
  test("client paths resolve from XDG when $HOME is unset", async () => {
    const client = await prepareRelocatedClient("homeless-client");
    for (const file of ["blue.toml", "session.json"]) {
      await copyFile(path.join(home, ".config", "blue", file), path.join(client.configHome, "blue", file));
    }
    const config = await runCliWithoutHome(client.home, ["config"], relocatedEnv(client));
    expect(config.code, config.stderr).toBe(0);
    expect(config.stdout).toContain("revision");

    // The shim directory has no XDG override, so it still needs a profile —
    // and says so rather than failing somewhere less obvious.
    const doctor = await runCliWithoutHome(client.home, ["doctor"], relocatedEnv(client));
    expect(doctor.code).not.toBe(0);
    expect(doctor.stderr).toContain("HOME");
  });

  test("gateway swaps the inference JWT and records request metadata", async ({ page }) => {
    await loginAsAdmin(page);
    const log = await readClientFile(home, "agent-log/codex.env");
    const token = log.match(/^env_HARNESS_CODEX_KEY=(.+)$/m)?.[1];
    expect(token).toBeTruthy();
    expect((await page.request.post("http://blue:8081/v1/chat/completions", { data: {} })).status()).toBe(401);
    expect((await page.request.post("http://blue:8081/v1/chat/completions", {
      headers: { authorization: "Bearer invalid-inference-jwt" },
      data: {},
    })).status()).toBe(401);
    const response = await page.request.post("http://blue:8081/v1/chat/completions?api-version=e2e", {
      headers: {
        authorization: `Bearer ${token}`,
        "x-harness-agent": "codex",
        "x-harness-repo": "blocks/e2e",
        "x-harness-git-branch": "main",
        "x-e2e-forwarded": "yes",
      },
      data: { model: "gpt-e2e", messages: [{ role: "user", content: "hello" }] },
    });
    expect(response.status(), await response.text()).toBe(200);
    expect((await response.json()).choices[0].message.content).toBe("hello");
    const streamed = await page.request.post("http://blue:8081/v1/chat/completions", {
      headers: { authorization: `Bearer ${token}` },
      data: { model: "gpt-e2e", stream: true, messages: [{ role: "user", content: "stream" }] },
    });
    expect(streamed.status()).toBe(200);
    expect(streamed.headers()["content-type"]).toContain("text/event-stream");
    expect(await streamed.text()).toContain("data: [DONE]");
    const upstreamFailure = await page.request.post("http://blue:8081/v1/e2e/error", {
      headers: { authorization: `Bearer ${token}` },
      data: { model: "gpt-e2e" },
    });
    expect(upstreamFailure.status()).toBe(429);
    expect(upstreamFailure.headers()["retry-after"]).toBe("7");
    expect(await upstreamFailure.text()).toContain("e2e upstream rate limit");
    const upstreamRequests = await page.request.get("http://fake-upstream:4010/_e2e/requests");
    expect(upstreamRequests.status()).toBe(200);
    const forwarded = (await upstreamRequests.json()).find((item: { query: string }) => item.query === "?api-version=e2e");
    expect(forwarded.headers["x-e2e-forwarded"]).toBe("yes");
    expect(forwarded.authorization).not.toBe(token);
    await expect.poll(async () => {
      const logs = await page.request.get(`${process.env.E2E_CONTROL_API_URL}/gateway/request-logs`);
      const items = (await logs.json()).items;
      return items.find((item: { path: string; harness?: string }) =>
        item.path === "/v1/chat/completions" && item.harness === "codex"
      );
    }).toMatchObject({
      harness: "codex",
      repository: "blocks/e2e",
      branch: "main",
      model: "gpt-e2e",
      http_status: 200,
      result: "success",
    });
  });

  test("managed-file drift fails verification and is repaired", async () => {
    const overlay = path.join(home, ".config", "blue", "runtime", "claude", "settings.json");
    await writeFile(overlay, "tampered = true\n");
    expect((await runCli(home, ["verify"])).code).not.toBe(0);
    expect((await runCli(home, ["apply", "--yes"])).code).toBe(0);
    expect((await runCli(home, ["verify"])).code).toBe(0);
  });

  test("daemon observes a new revision and reconciles it without apply", async ({ page }) => {
    await loginAsAdmin(page);
    const child = spawnCli(home, ["daemon", "--interval", "1"]);
    await waitForOutput(child, /Starting reconcile daemon/);
    const completion = collect(child);
    try {
      const control = process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080";
      const currentResponse = await page.request.get(`${control}/admin/gateway/models`);
      expect(currentResponse.status()).toBe(200);
      const current = await currentResponse.json();
      const model = `gpt-daemon-${Date.now()}`;
      const claude = current.harnesses.find((item: { key: string }) => item.key === "claude");
      expect(claude.exposure).toBe("catalog");
      const update = await page.request.put(`${control}/admin/gateway/models/harnesses/claude`, {
        data: {
          base_revision: current.revision,
          gateway_models: [...claude.gateway_models, model],
          default_model: model,
        },
      });
      expect(update.status(), await update.text()).toBe(200);
      await expect.poll(async () => {
        const settings = JSON.parse(
          await readClientFile(home, ".config/blue/runtime/claude/settings.json"),
        );
        return {
          availableModels: settings.availableModels,
          enforceAvailableModels: settings.enforceAvailableModels,
          model: settings.model,
          modelPicker: settings.modelPicker,
        };
      }, { timeout: 20_000 }).toEqual({
        availableModels: [...claude.gateway_models, model],
        enforceAvailableModels: true,
        model,
        modelPicker: {
          options: [...claude.gateway_models, model].map((id: string) => ({ model: id, label: id })),
          replaceBuiltInOptions: true,
        },
      });
    } finally {
      child.kill("SIGTERM");
    }
    const result = await completion;
    expect([0, 1, 143]).toContain(result.code);
    expect((await runCli(home, ["verify"])).code).toBe(0);
  });

  test("running supervisor displays a revision received over SSE", async ({ page }) => {
    await loginAsAdmin(page);
    const child = spawnCliInPty(home, "blue run codex -- wait-for-revision", {
      E2E_AGENT_READ_STDIN: "1",
    });
    const completion = collect(child);
    try {
      await waitForOutput(child, /Ctrl-\] Control/);
      const control = process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080";
      const currentResponse = await page.request.get(`${control}/admin/gateway/models`);
      expect(currentResponse.status()).toBe(200);
      const current = await currentResponse.json();
      const model = `gpt-sse-${Date.now()}`;
      const codex = current.harnesses.find((item: { key: string }) => item.key === "codex");
      // Register before publishing the revision: the SSE event may arrive
      // before the mutation response on a fast local stack.
      const revisionNotice = waitForOutput(child, /New Blue policy/);
      const update = await page.request.put(`${control}/admin/gateway/models/harnesses/codex`, {
        data: {
          base_revision: current.revision,
          gateway_models: [...codex.gateway_models, model],
          default_model: model,
        },
      });
      expect(update.status(), await update.text()).toBe(200);
      await revisionNotice;
      child.stdin.write("continue\r");
      const result = await completion;
      expect(result.code, `${result.stdout}\n${result.stderr}`).toBe(0);
    } finally {
      if (child.exitCode === null) child.kill("SIGTERM");
    }
  });

  test("shim lifecycle is isolated and reversible", async () => {
    const dir = path.join(home, "shims");
    await mkdir(dir, { recursive: true });
    const install = await runCli(home, ["shim", "install", "--dir", dir]);
    expect(install.code, install.stderr).toBe(0);
    for (const harness of ["codex", "claude", "kimi", "opencode"]) {
      expect((await stat(path.join(dir, harness))).isFile()).toBeTruthy();
    }
    const uninstall = await runCli(home, ["shim", "uninstall", "--dir", dir]);
    expect(uninstall.code, uninstall.stderr).toBe(0);
  });

  test("dashboard theme follows the system and persists an explicit choice", async ({ page }) => {
    await page.emulateMedia({ colorScheme: "light" });
    await loginAsAdmin(page);

    const root = page.locator("html");
    await expect(root).not.toHaveClass(/\bdark\b/);

    const selectTheme = async (name: "Dark" | "System" | "Light") => {
      const themeTrigger = page.getByRole("menuitem", { name: "Theme" });
      if (!(await themeTrigger.isVisible())) {
        await page.getByRole("button", { name: /Open user menu/ }).click();
      }
      await expect(themeTrigger).toBeVisible();
      await themeTrigger.dispatchEvent("click");
      const option = page.getByRole("menuitemradio", { name });
      await expect(option).toBeVisible();
      await option.dispatchEvent("click");
    };

    await selectTheme("Dark");
    await expect(root).toHaveClass(/\bdark\b/);
    await expect.poll(() => page.evaluate(() => localStorage.getItem("blue-theme"))).toBe("dark");

    const persistedPage = await page.context().newPage();
    await persistedPage.goto("/sessions");
    await expect(persistedPage.locator("html")).toHaveClass(/\bdark\b/);
    await persistedPage.close();

    await selectTheme("System");
    await expect(root).not.toHaveClass(/\bdark\b/);

    await page.emulateMedia({ colorScheme: "dark" });
    await expect(root).toHaveClass(/\bdark\b/);

    await selectTheme("Light");
    await expect(root).not.toHaveClass(/\bdark\b/);
    await expect.poll(() => page.evaluate(() => localStorage.getItem("blue-theme"))).toBe("light");
  });

  test("governance updates are atomic under conflicts and validation failures", async ({ page }) => {
    await loginAsAdmin(page, { fresh: true });
    const control = process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080";
    const originalResponse = await page.request.get(`${control}/admin/governance-config`);
    expect(originalResponse.status(), await originalResponse.text()).toBe(200);
    const original = await originalResponse.json();
    const historyBefore = await (await page.request.get(`${control}/admin/governance-config/revisions`)).json();
    const documents = ["conflict-a", "conflict-b"].map((suffix) => {
      const document = YAML.parse(original.managed_yaml);
      const model = `gpt-${suffix}-${Date.now()}`;
      document.harnesses.codex.managed_config.model = model;
      document.harnesses.codex.gateway_models = [model];
      return YAML.stringify(document);
    });

    try {
      const raced = await Promise.all(documents.map((managed_yaml) =>
        page.request.put(`${control}/admin/governance-config`, {
          data: { base_revision: original.revision, managed_yaml },
        })
      ));
      expect(raced.map((response) => response.status()).sort()).toEqual([200, 409]);

      const winnerResponse = await page.request.get(`${control}/admin/governance-config`);
      const winner = await winnerResponse.json();
      const winningDocument = YAML.parse(documents[raced.findIndex((response) => response.status() === 200)]);
      expect(winner.document.harnesses.codex.managed_config.model).toBe(
        winningDocument.harnesses.codex.managed_config.model,
      );
      const historyAfterRace = await (await page.request.get(`${control}/admin/governance-config/revisions`)).json();
      expect(historyAfterRace).toHaveLength(historyBefore.length + 1);
      expect(historyAfterRace.filter((entry: { revision: string }) => entry.revision === winner.revision)).toHaveLength(1);

      const invalidDocuments = [
        "harnesses: [",
        YAML.stringify({ ...winner.document, gateway: { type: "litellm", token: "must-not-persist" } }),
        YAML.stringify({
          ...winner.document,
          allowed_harnesses: [...winner.document.allowed_harnesses, "unsupported-e2e"],
          harnesses: { ...winner.document.harnesses, "unsupported-e2e": { managed_config: {} } },
        }),
      ];
      for (const managed_yaml of invalidDocuments) {
        const rejected = await page.request.put(`${control}/admin/governance-config`, {
          data: { base_revision: winner.revision, managed_yaml },
        });
        expect(rejected.status(), await rejected.text()).toBe(400);
        const stillCurrent = await (await page.request.get(`${control}/admin/governance-config`)).json();
        expect(stillCurrent.revision).toBe(winner.revision);
        expect(stillCurrent.managed_yaml).toBe(winner.managed_yaml);
      }
    } finally {
      const current = await (await page.request.get(`${control}/admin/governance-config`)).json();
      const restored = await page.request.put(`${control}/admin/governance-config`, {
        data: { base_revision: current.revision, managed_yaml: original.managed_yaml },
      });
      expect(restored.status(), await restored.text()).toBe(200);
    }
  });

  test("dashboard harness editor publishes configuration consumed by the CLI", async ({ page }) => {
    await loginAsAdmin(page, { fresh: true });
    const control = process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080";
    const snapshotResponse = await page.request.get(`${control}/admin/governance-config`);
    expect(snapshotResponse.status(), await snapshotResponse.text()).toBe(200);
    const snapshot = await snapshotResponse.json();

    await page.getByRole("link", { name: "Harnesses" }).click();
    await expect(page).toHaveURL(/\/harnesses$/);
    await page.getByRole("button", { name: "Actions for Codex" }).click();
    await page.getByRole("menuitem", { name: "Edit", exact: true }).click();
    const model = "gpt-e2e";
    await page.getByLabel("Managed config YAML").fill(`model: ${model}\nreasoning_effort: high\napproval_policy: never\nsandbox_mode: workspace-write\n`);
    await page.getByRole("button", { name: "Save changes" }).click();
    await expect(page.getByRole("dialog")).toBeHidden();
    const launch = await runCli(home, ["codex", "--dashboard-check"]);
    expect(launch.code, launch.stderr).toBe(0);
    expect(await readClientFile(home, ".codex/blue.config.toml")).toContain(model);
    expect(await readClientFile(home, ".codex/blue.config.toml")).toContain('model_reasoning_effort = "high"');
    expect(await readClientFile(home, "agent-log/codex.env")).toContain("check_for_update_on_startup=false");

    await page.getByRole("button", { name: "Actions for Codex" }).click();
    await page.getByRole("menuitem", { name: "Edit", exact: true }).click();
    await page.getByLabel("Allowed harness versions").fill(">=0.0.0");
    await page.getByRole("checkbox", { name: "Allow unverified versions" }).check();
    await page.getByRole("button", { name: "Save changes" }).click();
    await expect(page.getByRole("dialog")).toBeHidden();
    const uncappedLaunch = await runCli(home, ["codex", "--dashboard-uncapped-check"]);
    expect(uncappedLaunch.code, uncappedLaunch.stderr).toBe(0);
    expect(await readClientFile(home, "agent-log/codex.env")).not.toContain("check_for_update_on_startup=false");

    await page.getByRole("button", { name: "Actions for Codex" }).click();
    await page.getByRole("menuitem", { name: "Edit", exact: true }).click();
    await page.getByLabel("Allowed harness versions").fill(">=0.0.0, <999.0.0");
    await page.getByRole("button", { name: "Save changes" }).click();
    await expect(page.getByRole("dialog")).toBeHidden();
    const cappedLaunch = await runCli(home, ["codex", "--dashboard-capped-check"]);
    expect(cappedLaunch.code, cappedLaunch.stderr).toBe(0);
    expect(await readClientFile(home, "agent-log/codex.env")).toContain("check_for_update_on_startup=false");

    const currentResponse = await page.request.get(`${control}/admin/governance-config`);
    expect(currentResponse.status(), await currentResponse.text()).toBe(200);
    const restoreResponse = await page.request.put(`${control}/admin/governance-config`, {
      data: {
        base_revision: (await currentResponse.json()).revision,
        managed_yaml: snapshot.managed_yaml,
      },
    });
    expect(restoreResponse.status(), await restoreResponse.text()).toBe(200);
  });

  test("gateway model catalog assignments enforce editable catalogs and keep Codex read-only", async ({ page }) => {
    await loginAsAdmin(page, { fresh: true });
    const control = process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080";
    const snapshotResponse = await page.request.get(`${control}/admin/governance-config`);
    expect(snapshotResponse.status(), await snapshotResponse.text()).toBe(200);
    const snapshot = await snapshotResponse.json();

    try {
      const refreshed = await page.request.post(`${control}/admin/gateway/models/refresh`);
      expect(refreshed.status(), await refreshed.text()).toBe(200);
      const catalog = await refreshed.json();
      expect(catalog.models.map((model: { id: string }) => model.id)).toEqual(expect.arrayContaining([
        "e2e/model",
        "e2e/alternate",
        "gpt-e2e",
      ]));
      expect(catalog.sync.discovery_supported).toBe(true);

      const kimiSaved = await page.request.put(`${control}/admin/gateway/models/harnesses/kimi`, {
        data: {
          base_revision: catalog.revision,
          gateway_models: ["kimi-e2e", "kimi-e2e-secondary"],
          default_model: null,
        },
      });
      const kimiSavedBody = await kimiSaved.text();
      expect(kimiSaved.status(), kimiSavedBody).toBe(200);
      const kimiRevision = JSON.parse(kimiSavedBody).revision;

      const saved = await page.request.put(`${control}/admin/gateway/models/harnesses/opencode`, {
        data: {
          base_revision: kimiRevision,
          gateway_models: ["e2e/model", "e2e/alternate"],
          default_model: "e2e/model",
        },
      });
      expect(saved.status(), await saved.text()).toBe(200);

      await page.goto("/gateway?tab=models");
      await expect(page.getByRole("tab", { name: "Models" })).toBeVisible();
      await expect(page.getByRole("tab", { name: "Discovery" })).toHaveAttribute("data-active", "");
      await expect(page.getByText("Discovery status", { exact: true })).toBeVisible();

      await page.getByRole("tab", { name: "Catalog" }).click();
      await expect(page).toHaveURL(/tab=models.*models_tab=catalog/);
      await expect(page.getByText("e2e/alternate", { exact: true }).first()).toBeVisible();

      await page.getByRole("tab", { name: "Assignments" }).click();
      await expect(page).toHaveURL(/tab=models.*models_tab=assignments/);
      await page.setViewportSize({ width: 1280, height: 600 });
      const codex = catalog.harnesses.find((item: { key: string }) => item.key === "codex");
      const codexRow = page.getByRole("row").filter({ hasText: "Codex" });
      await expect(codexRow).toContainText(String(codex.gateway_models.length));
      await expect(codexRow).toContainText(codex.default_model ?? "Native fallback");
      await expect(codexRow).toContainText("Selected only");
      await expect(codexRow).toContainText("Full catalog assignment is unavailable for Codex.");
      await expect(codexRow.getByRole("button", { name: "Edit Codex models" })).toBeDisabled();
      for (const harness of ["Claude", "Kimi", "OpenCode"]) {
        await expect(page.getByRole("button", { name: `Edit ${harness} models` })).toBeEnabled();
      }
      await page.getByRole("button", { name: "Edit OpenCode models" }).click();
      await expect(page.getByRole("dialog")).toContainText("This agent exposes every assigned model");
      await expect(page.getByRole("button", { name: "Cancel" })).toBeVisible();
      await expect(page.getByRole("button", { name: "Save assignments" })).toBeVisible();
      const modelList = page.getByLabel("Available gateway models");
      await expect(modelList).toHaveCSS("overflow-y", "auto");
      expect(await modelList.evaluate((element) => element.scrollHeight > element.clientHeight)).toBe(true);
      await page.getByRole("button", { name: "Cancel" }).click();

      const launch = await runCli(home, ["opencode", "--catalog-check"]);
      expect(launch.code, launch.stderr).toBe(0);
      const opencode = JSON.parse(await readClientFile(home, ".config/blue/runtime/opencode/opencode.json"));
      expect(Object.keys(opencode.provider.governed.models)).toEqual(expect.arrayContaining([
        "e2e/model",
        "e2e/alternate",
      ]));

      await mkdir(path.join(home, ".kimi-code"), { recursive: true });
      await writeFile(
        path.join(home, ".kimi-code", "config.toml"),
        'default_model = "native-model"\n[models.native-model]\nprovider = "native-provider"\nmodel = "native-model"\nmax_context_size = 4096\n[providers.native-provider]\ntype = "openai"\nbase_url = "https://native.example"\napi_key = "native"\n',
      );
      const kimiLaunch = await runCli(home, ["kimi", "--catalog-check"]);
      expect(kimiLaunch.code, kimiLaunch.stderr).toBe(0);
      const kimi = await readClientFile(home, ".config/blue/runtime/kimi/config.toml");
      expect(kimi).toContain('default_model = "kimi-e2e"');
      expect(kimi).not.toContain("native-model");
      expect(kimi).not.toContain("native-provider");
      expect(kimi.match(/^\[models\./gm)).toHaveLength(2);
      expect(kimi.match(/^\[providers\./gm)).toHaveLength(1);
      expect(kimi.indexOf("[models.kimi-e2e]")).toBeLessThan(
        kimi.indexOf("[models.kimi-e2e-secondary]"),
      );
    } finally {
      const current = await (await page.request.get(`${control}/admin/governance-config`)).json();
      const restored = await page.request.put(`${control}/admin/governance-config`, {
        data: { base_revision: current.revision, managed_yaml: snapshot.managed_yaml },
      });
      expect(restored.status(), await restored.text()).toBe(200);
    }
  });

  test("administrator API supports branding and invitation lifecycle", async ({ page }) => {
    await loginAsAdmin(page);
    const control = process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080";
    const me = await page.request.get(`${control}/auth/me`);
    expect((await me.json()).role).toBe("admin");
    for (const route of [
      "/admin/governance-config/revisions",
      "/admin/blue-config/export",
      "/admin/harnesses/managed-configs",
      "/admin/package-catalog",
      "/admin/users",
      "/admin/client-status",
      "/admin/client-status/facets",
      "/session-uploads/facets",
      "/gateway/request-logs/facets",
    ]) {
      expect((await page.request.get(`${control}${route}`)).status(), route).toBe(200);
    }
    const instanceId = randomUUID();
    const reported = await page.request.post(`${control}/client-status`, {
      data: {
        instance_id: instanceId,
        hostname: "removable-e2e-client",
        client_version: "0.1.0",
        platform: "linux",
        applied: false,
        files_ok: true,
      },
    });
    expect(reported.status(), await reported.text()).toBe(204);
    const clientPage = await (await page.request.get(`${control}/admin/client-status?q=${instanceId}`)).json();
    expect(clientPage.items).toHaveLength(1);
    expect(clientPage.items[0]).toEqual(expect.objectContaining({
      instance_id: instanceId,
      activity_status: "recent",
      first_seen_at: expect.any(String),
      id: expect.any(String),
    }));
    const removed = await page.request.delete(`${control}/admin/client-status/${clientPage.items[0].id}`);
    expect(removed.status(), await removed.text()).toBe(204);
    expect((await page.request.delete(`${control}/admin/client-status/${clientPage.items[0].id}`)).status()).toBe(404);
    const gatewayStatus = await (await page.request.get(`${control}/gateway/status`)).json();
    expect(gatewayStatus.harnesses.length).toBeGreaterThan(0);
    expect(gatewayStatus.inference_proxy_url).toBeTruthy();
    expect(gatewayStatus.upstream_gateway_url).toBeTruthy();
    expect(gatewayStatus.provisioner_type).toBeTruthy();
    expect(gatewayStatus.runtime_checks).toBeTruthy();
    const branding = await page.request.put(`${control}/admin/branding`, {
      data: { logo_url: "https://example.com/blue.png", favicon_url: null },
    });
    expect(branding.status()).toBe(200);
    expect((await (await page.request.get(`${control}/branding`)).json()).logo_url).toContain("example.com");

    const identity = await (await page.request.get(`${control}/auth/me`)).json();
    const adminEmail: string = identity.email;
    const upperCased = adminEmail.toUpperCase();

    const bySubstring = await (await page.request.get(
      `${control}/admin/users?q=${encodeURIComponent(upperCased.slice(1, 6))}`,
    )).json();
    expect(bySubstring.items.map((item: { email: string }) => item.email)).toContain(adminEmail);
    const bySubject = await (await page.request.get(
      `${control}/admin/users?q=${encodeURIComponent(identity.subject ?? adminEmail)}`,
    )).json();
    expect(bySubject.total).toBeGreaterThan(0);
    expect((await (await page.request.get(
      `${control}/admin/users?provisioning_source=local`,
    )).json()).items.every((item: { provisioning_source: string }) => item.provisioning_source === "local")).toBe(true);
    expect((await page.request.get(`${control}/admin/users?provisioning_source=sso`)).status()).toBe(400);
    expect((await page.request.get(`${control}/admin/users?q=${"a".repeat(201)}`)).status()).toBe(400);
    expect((await (await page.request.get(
      `${control}/admin/users?q=${encodeURIComponent("no-such-member@example.invalid")}`,
    )).json()).total).toBe(0);

    for (const facet of [
      "/admin/client-status/facets",
      "/session-uploads/facets",
      "/gateway/request-logs/facets",
    ]) {
      const unfiltered = await (await page.request.get(`${control}${facet}`)).json();
      expect(unfiltered.users.length).toBeLessThanOrEqual(10);
      const missed = await (await page.request.get(
        `${control}${facet}?user_q=${encodeURIComponent("no-such-member@example.invalid")}`,
      )).json();
      expect(missed.users, facet).toEqual([]);
      expect((await page.request.get(`${control}${facet}?user_q=${"a".repeat(201)}`)).status(), facet).toBe(400);
      const hydrated = await (await page.request.get(
        `${control}${facet}?user_q=${encodeURIComponent("no-such-member@example.invalid")}&selected_user_id=${identity.id}`,
      )).json();
      expect(hydrated.users, facet).toEqual([]);
      if (unfiltered.users.length > 0) {
        const last = unfiltered.users[unfiltered.users.length - 1];
        const matched = await (await page.request.get(
          `${control}${facet}?user_q=${encodeURIComponent(last.email.toUpperCase())}`,
        )).json();
        expect(matched.users.map((user: { id: string }) => user.id), facet).toContain(last.id);
        const prioritized = await (await page.request.get(
          `${control}${facet}?selected_user_id=${last.id}`,
        )).json();
        expect(prioritized.users[0].id, facet).toBe(last.id);
      }
    }

    const created = await page.request.post(`${control}/admin/invitations`, {
      data: { email: `invite-${Date.now()}@example.com`, role: "member" },
    });
    expect(created.status(), await created.text()).toBe(201);
    const invitation = await created.json();
    expect((await page.request.get(`${control}/admin/invitations/${invitation.id}`)).status()).toBe(200);
    const invitationMatches = await (await page.request.get(
      `${control}/admin/invitations?status=outstanding&role=member&q=${encodeURIComponent(invitation.email.toUpperCase())}`,
    )).json();
    expect(invitationMatches.items.map((item: { id: string }) => item.id)).toContain(invitation.id);
    expect((await (await page.request.get(
      `${control}/admin/invitations?status=outstanding&role=admin&q=${encodeURIComponent(invitation.email)}`,
    )).json()).total).toBe(0);
    expect((await page.request.get(`${control}/admin/invitations?role=owner`)).status()).toBe(400);
    expect((await page.request.post(`${control}/admin/invitations/${invitation.id}/resend`)).status()).toBe(200);
    expect((await page.request.delete(`${control}/admin/invitations/${invitation.id}`)).status()).toBe(204);
  });

  test("member sees a provisioning error when the gateway account is missing", async ({ page, browser }) => {
    await loginAsAdmin(page);
    const control = process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080";
    const dashboard = process.env.E2E_DASHBOARD_URL ?? "http://127.0.0.1:3000";
    const modePath = "/work/tests/e2e/artifacts/provisioner/mode";
    const email = `missing-gateway-${Date.now()}@example.com`;
    const created = await page.request.post(`${control}/admin/invitations`, { data: { email, role: "member" } });
    expect(created.status(), await created.text()).toBe(201);
    const invitation = await created.json();
    const memberContext = await browser.newContext();
    let memberId: string | undefined;
    try {
      const memberPage = await memberContext.newPage();
      await memberPage.goto(`${dashboard}/accept-invitation?id=${invitation.id}`);
      await memberPage.getByLabel("Password").fill("member-password-e2e");
      await memberPage.getByRole("button", { name: "Create account" }).click();
      await expect(memberPage).toHaveURL(/\/sessions/);
      const identity = await memberContext.request.get(`${control}/auth/me`);
      expect(identity.status(), await identity.text()).toBe(200);
      memberId = (await identity.json()).id;

      await memberPage.goto(`${dashboard}/gateway`);
      await expect(memberPage.getByRole("button", { name: "Provision key" })).toBeVisible();
      await writeFile(modePath, "account-missing\n");
      const posted = memberPage.waitForResponse((response) =>
        response.request().method() === "POST" && new URL(response.url()).pathname === "/gateway",
      );
      await memberPage.getByRole("button", { name: "Provision key" }).click();
      expect((await posted).status()).toBe(200);
      await expect(memberPage.getByText("gateway account is not provisioned: member has no upstream account", { exact: true })).toBeVisible();
      await expect(memberPage.getByRole("tab", { name: "Key" })).toBeVisible();

      await memberPage.reload();
      await expect(memberPage.getByText("gateway account is not provisioned: member has no upstream account", { exact: true })).toBeVisible();
      const access = await memberContext.request.get(`${control}/gateway/key`);
      expect(access.status(), await access.text()).toBe(200);
      expect(await access.json()).toMatchObject({ status: "error", error: "gateway account is not provisioned: member has no upstream account" });
    } finally {
      await rm(modePath, { force: true });
      await memberContext.close();
      if (memberId) await page.request.delete(`${control}/admin/users/${memberId}`);
    }
  });

  test("invited member can read policy but cannot call administrator or cross-user APIs", async ({ page, browser }) => {
    await loginAsAdmin(page);
    const control = process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080";
    const dashboard = process.env.E2E_DASHBOARD_URL ?? "http://127.0.0.1:3000";
    const email = `member-${Date.now()}@example.com`;
    const created = await page.request.post(`${control}/admin/invitations`, { data: { email, role: "member" } });
    expect(created.status(), await created.text()).toBe(201);
    const invitation = await created.json();
    const memberContext = await browser.newContext();
    try {
      const memberPage = await memberContext.newPage();
      await memberPage.goto(`${dashboard}/accept-invitation?id=${invitation.id}`);
      await memberPage.getByLabel("Password").fill("member-password-e2e");
      await memberPage.getByRole("button", { name: "Create account" }).click();
      await expect(memberPage).toHaveURL(/\/sessions/);
      const me = await memberContext.request.get(`${control}/auth/me`);
      expect(me.status(), await me.text()).toBe(200);
      const identity = await me.json();
      expect(identity.role).toBe("member");
      expect((await memberContext.request.get(`${control}/admin/users`)).status()).toBe(403);
      expect((await memberContext.request.get(`${control}/health/dependencies`)).status()).toBe(200);
      expect((await memberContext.request.post(`${control}/gateway/proxy/health`)).status()).toBe(403);
      expect((await memberContext.request.get(`${control}/gateway/request-logs`)).status()).toBe(403);
      expect((await memberContext.request.get(`${control}/gateway/request-logs/facets`)).status()).toBe(403);
      const gatewayStatus = await memberContext.request.get(`${control}/gateway/status`);
      expect(gatewayStatus.status(), await gatewayStatus.text()).toBe(200);
      expect(await gatewayStatus.json()).toEqual({ enabled: true, runtime_configured: true });

      // Gateway personalization deliberately rejects browser-cookie sessions:
      // inference JWTs must be bound to an OAuth access token carrying `sid`.
      const memberHome = await prepareClient(`member-policy-${Date.now()}`);
      const memberLogin = spawnCli(memberHome, ["login"]);
      const memberDeviceUrl = await waitForOutput(
        memberLogin,
        /http:\/\/127\.0\.0\.1:3000\/device\/[A-Za-z0-9_-]+/,
      );
      await memberPage.goto(memberDeviceUrl);
      await memberPage.getByRole("button", { name: "Authorize" }).click();
      await expect(memberPage.getByText("CLI authorized")).toBeVisible();
      expect((await collect(memberLogin)).code).toBe(0);
      const memberGateway = await runCli(memberHome, ["gateway"]);
      expect(memberGateway.code, memberGateway.stderr).toBe(0);
      const memberOauth = JSON.parse(
        await readClientFile(memberHome, ".config/blue/session.json"),
      );
      const memberConfig = await memberContext.request.get(`${control}/governance-config`, {
        headers: {
          authorization: `Bearer ${memberOauth.token}`,
          "x-blue-contract-version": "4",
          "x-blue-capabilities": "adapter_intervals,compiled_harness_registry,transactional_reconcile,versioned_state,gateway_inference_jwt,gateway_model_catalog,unverified_harness_versions,tenant_client_version_pin",
        },
      });
      expect(memberConfig.status(), await memberConfig.text()).toBe(200);
      expect((await memberConfig.json()).revision).toBeTruthy();
      const memberSessions = await memberContext.request.get(`${control}/session-uploads?per_page=25`);
      expect(memberSessions.status(), await memberSessions.text()).toBe(200);
      expect((await memberSessions.json()).items).not.toContainEqual(expect.objectContaining({ id: adminSessionId }));
      expect((await memberContext.request.get(`${control}/session-uploads/${adminSessionId}`)).status()).toBe(403);
      expect((await memberContext.request.post(`${control}/session-uploads/${adminSessionId}/download`)).status()).toBe(403);

      const shared = await page.request.put(`${control}/session-uploads/${adminSessionId}/sharing`, {
        data: { mode: "selected", user_ids: [identity.id] },
      });
      expect(shared.status(), await shared.text()).toBe(200);
      const resumable = await memberContext.request.get(`${control}/session-uploads?resumable=true&limit=100`);
      expect(resumable.status(), await resumable.text()).toBe(200);
      expect((await resumable.json()).items).toContainEqual(expect.objectContaining({
        id: adminSessionId,
        shared: true,
      }));
      expect((await memberContext.request.get(`${control}/session-uploads/${adminSessionId}`)).status()).toBe(200);
      expect((await memberContext.request.post(`${control}/session-uploads/${adminSessionId}/download`)).status()).toBe(200);
      expect((await memberContext.request.put(`${control}/session-uploads/${adminSessionId}/sharing`, {
        data: { mode: "workspace" },
      })).status()).toBe(403);

      const revoked = await page.request.put(`${control}/session-uploads/${adminSessionId}/sharing`, {
        data: { mode: "private" },
      });
      expect(revoked.status(), await revoked.text()).toBe(200);
      expect((await memberContext.request.get(`${control}/session-uploads/${adminSessionId}`)).status()).toBe(403);
      expect((await memberContext.request.post(`${control}/session-uploads/${adminSessionId}/download`)).status()).toBe(403);

      await memberPage.goto(`${dashboard}/sessions`);
      await expect(memberPage.getByRole("link", { name: "Sessions" })).toBeVisible();
      await expect(memberPage.getByRole("link", { name: "Gateway" })).toBeVisible();
      for (const name of ["Harnesses", "Extensions", "Members", "Clients"]) {
        await expect(memberPage.getByRole("link", { name })).toHaveCount(0);
      }
      await memberPage.goto(`${dashboard}/gateway`);
      await expect(memberPage.getByRole("tab", { name: "Key" })).toBeVisible();
      await expect(memberPage.getByRole("tab", { name: "Overview" })).toHaveCount(0);
      await expect(memberPage.getByRole("tab", { name: "Logs" })).toHaveCount(0);
      const reconciled = memberPage.waitForResponse((response) =>
        response.request().method() === "POST" && new URL(response.url()).pathname === "/gateway",
      );
      await memberPage.getByRole("button", { name: "Reconcile key" }).click();
      expect((await reconciled).status()).toBe(200);
      await expect(memberPage.getByText("ready", { exact: true })).toBeVisible();
      const deniedPaths = ["/harnesses", "/extensions", "/members", "/clients", "/gateway?tab=logs"];
      for (const path of deniedPaths) {
        const response = await memberPage.goto(`${dashboard}${path}`, { waitUntil: "commit" });
        expect(response?.status(), path).toBe(404);
      }
      const suspended = await page.request.patch(`${control}/admin/users/${identity.id}`, { data: { status: "suspended" } });
      expect(suspended.status(), await suspended.text()).toBe(200);
      await expect.poll(async () => (await memberContext.request.get(`${control}/auth/me`)).status()).toBe(401);
      const reactivated = await page.request.patch(`${control}/admin/users/${identity.id}`, { data: { status: "active" } });
      expect(reactivated.status(), await reactivated.text()).toBe(200);
      expect((await page.request.delete(`${control}/admin/users/${identity.id}`)).status()).toBe(204);
    } finally {
      await memberContext.close();
    }
  });

  test("SCIM supports discovery plus user and group lifecycle", async ({ request }) => {
    const base = `${process.env.E2E_CONTROL_API_URL}/scim/v2`;
    const headers = { authorization: "Bearer e2e-scim-token", "content-type": "application/scim+json" };
    for (const route of ["/ServiceProviderConfig", "/ResourceTypes", "/Schemas"]) {
      expect((await request.get(`${base}${route}`, { headers })).status()).toBe(200);
    }
    const createdUser = await request.post(`${base}/Users`, {
      headers,
      data: {
        schemas: ["urn:ietf:params:scim:schemas:core:2.0:User"],
        externalId: `external-${Date.now()}`,
        userName: `scim-${Date.now()}@example.com`,
        active: true,
        name: { givenName: "E2E", familyName: "Member" },
      },
    });
    expect(createdUser.status(), await createdUser.text()).toBe(201);
    const user = await createdUser.json();
    const group = await request.post(`${base}/Groups`, {
      headers,
      data: {
        schemas: ["urn:ietf:params:scim:schemas:core:2.0:Group"],
        displayName: "Blue Administrators",
        members: [{ value: user.id }],
      },
    });
    expect(group.status(), await group.text()).toBe(201);
    const groupBody = await group.json();
    expect(groupBody.members).toEqual([
      expect.objectContaining({ value: user.id }),
    ]);
    const invalidReplacement = await request.put(`${base}/Groups/${groupBody.id}`, {
      headers,
      data: {
        schemas: ["urn:ietf:params:scim:schemas:core:2.0:Group"],
        displayName: "Replacement Must Roll Back",
        members: [{ value: randomUUID() }],
      },
    });
    expect(invalidReplacement.status(), await invalidReplacement.text()).toBe(400);
    const unchangedGroup = await request.get(`${base}/Groups/${groupBody.id}`, { headers });
    expect(unchangedGroup.status(), await unchangedGroup.text()).toBe(200);
    expect(await unchangedGroup.json()).toEqual(expect.objectContaining({
      displayName: "Blue Administrators",
      members: [expect.objectContaining({ value: user.id })],
    }));
    expect((await request.get(`${base}/Users?filter=${encodeURIComponent(`userName eq "${user.userName}"`)}`, { headers })).status()).toBe(200);
    expect((await request.patch(`${base}/Users/${user.id}`, {
      headers,
      data: {
        schemas: ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
        Operations: [{ op: "replace", path: "active", value: false }],
      },
    })).status()).toBe(200);
    expect((await request.delete(`${base}/Groups/${groupBody.id}`, { headers })).status()).toBe(204);
    expect((await request.delete(`${base}/Users/${user.id}`, { headers })).status()).toBe(204);
  });

  test("logout removes the local rotating session", async () => {
    const logout = await runCli(home, ["logout"]);
    expect(logout.code, logout.stderr).toBe(0);
    await expect(readFile(path.join(home, ".config", "blue", "session.json"), "utf8")).rejects.toThrow();
  });

  test("interactive reset cancels without changes, then archives state and revokes gateway access", async ({ page }) => {
    await loginAsAdmin(page);
    const control = process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080";
    const originalResponse = await page.request.get(`${control}/admin/governance-config`);
    expect(originalResponse.status(), await originalResponse.text()).toBe(200);
    const original = await originalResponse.json();
    const originalDocument = YAML.parse(original.managed_yaml);
    const enabledGatewayForTest = !originalDocument.gateway;
    if (enabledGatewayForTest) {
      enableGateway(originalDocument);
      const enabled = await page.request.put(`${control}/admin/governance-config`, {
        data: { base_revision: original.revision, managed_yaml: YAML.stringify(originalDocument) },
      });
      expect(enabled.status(), await enabled.text()).toBe(200);
    }
    const resetHome = await prepareClient(`interactive-reset-${Date.now()}`);
    const loggedIn = await approveDeviceFlow(page, spawnCli(resetHome, ["login"]));
    expect(loggedIn.code, `${loggedIn.stdout}\n${loggedIn.stderr}`).toBe(0);
    expect((await runCli(resetHome, ["agent", "codex"])).code).toBe(0);
    expect((await runCli(resetHome, ["gateway"])).code).toBe(0);
    expect((await runCli(resetHome, ["apply", "--yes"])).code).toBe(0);

    const watched = [
      ".config/blue/blue.toml",
      ".config/blue/session.json",
      ".config/blue/applied-state.json",
      ".codex/blue.config.toml",
    ];
    const optionalPackageState = path.join(resetHome, ".config/blue/package-state.json");
    await expect(stat(optionalPackageState)).rejects.toThrow();
    const before = new Map<string, Buffer>();
    for (const relative of watched) before.set(relative, await readFile(path.join(resetHome, relative)));

    const cancelled = spawnCliInPty(resetHome, "blue reset");
    const cancelledCompletion = collect(cancelled);
    await waitForOutput(cancelled, /Reset Blue and disconnect/);
    cancelled.stdin.write("n\r");
    const cancelledResult = await cancelledCompletion;
    expect(cancelledResult.code, cancelledResult.stderr).toBe(0);
    expect(`${cancelledResult.stdout}\n${cancelledResult.stderr}`).toContain("Reset cancelled");
    for (const [relative, bytes] of before) {
      expect((await readFile(path.join(resetHome, relative))).equals(bytes), relative).toBe(true);
    }
    await expect(stat(optionalPackageState)).rejects.toThrow();

    const launched = await runCli(resetHome, ["run", "codex", "--", "before-reset"]);
    expect(launched.code, launched.stderr).toBe(0);
    const gatewayEnvironment = await readClientFile(resetHome, "agent-log/codex.env");
    const inferenceToken = gatewayEnvironment.match(/^env_HARNESS_CODEX_KEY=(eyJ\S+)$/m)?.[1] ?? "";
    expect(inferenceToken).toBeTruthy();

    const confirmed = spawnCliInPty(resetHome, "blue reset");
    const confirmedCompletion = collect(confirmed);
    await waitForOutput(confirmed, /Reset Blue and disconnect/);
    confirmed.stdin.write("y\r");
    const confirmedResult = await confirmedCompletion;
    expect(confirmedResult.code, confirmedResult.stderr).toBe(0);
    expect(`${confirmedResult.stdout}\n${confirmedResult.stderr}`).toContain("Blue reset complete");
    for (const relative of [".config/blue/blue.toml", ".config/blue/session.json", ".codex/blue.config.toml"]) {
      await expect(readFile(path.join(resetHome, relative))).rejects.toThrow();
    }
    const archives = await readdir(path.join(resetHome, ".config", "blue", "tenants"));
    expect(archives).toHaveLength(1);
    const archive = path.join(resetHome, ".config", "blue", "tenants", archives[0]);
    expect(JSON.parse(await readFile(path.join(archive, "manifest.json"), "utf8"))).toMatchObject({
      schema_version: 1,
      canonical_url: process.env.E2E_CONTROL_API_URL,
    });
    expect(await readFile(path.join(archive, "state", "applied-state.json"))).toEqual(
      before.get(".config/blue/applied-state.json"),
    );
    await expect.poll(async () => (
      await page.request.post("http://blue:8081/v1/chat/completions", {
        headers: { authorization: `Bearer ${inferenceToken}` },
        data: { model: "gpt-e2e", messages: [{ role: "user", content: "after-reset" }] },
      })
    ).status()).toBe(401);

    const repeated = await runCli(resetHome, ["reset", "--yes"]);
    expect(repeated.code, repeated.stderr).toBe(0);
    expect(repeated.stdout).toContain("already reset");
    expect(await readdir(path.join(resetHome, ".config", "blue", "tenants"))).toEqual(archives);

    if (enabledGatewayForTest) {
      const current = await (await page.request.get(`${control}/admin/governance-config`)).json();
      const restored = await page.request.put(`${control}/admin/governance-config`, {
        data: { base_revision: current.revision, managed_yaml: original.managed_yaml },
      });
      expect(restored.status(), await restored.text()).toBe(200);
    }
  });

  test("declining replacement authorization leaves an ordinarily retired session signed out", async ({ page }) => {
    const cancelledHome = await prepareClient("cancelled-session-replacement");
    await loginAsAdmin(page);
    const initial = await approveDeviceFlow(page, spawnCli(cancelledHome, ["login"]));
    expect(initial.code, `${initial.stdout}\n${initial.stderr}`).toBe(0);

    const replacement = spawnCli(cancelledHome, ["login", "--force"]);
    const completion = collect(replacement);
    const deviceUrl = await waitForOutput(replacement, deviceUrlPattern);
    await page.goto(deviceUrl);
    await page.getByRole("button", { name: "Deny" }).click();
    await expect(page.getByText("Authorization denied")).toBeVisible();
    const declined = await completion;
    expect(declined.code).not.toBe(0);
    await expect(
      readFile(path.join(cancelledHome, ".config", "blue", "session.json"), "utf8"),
    ).rejects.toThrow();
  });

  test("reset never recreates an expired local session while cleaning up", async ({ page }) => {
    const resetHome = await prepareClient("reset-expired-session");
    await loginAsAdmin(page);
    const initial = await approveDeviceFlow(page, spawnCli(resetHome, ["login"]));
    expect(initial.code, `${initial.stdout}\n${initial.stderr}`).toBe(0);

    const sessionPath = path.join(resetHome, ".config", "blue", "session.json");
    const expired = JSON.parse(await readFile(sessionPath, "utf8")) as PersistedOauthSession;
    expired.expires_at = Math.floor(Date.now() / 1000) - 60;
    await writeFile(sessionPath, JSON.stringify(expired), { mode: 0o600 });

    const reset = await runCli(resetHome, ["reset", "--yes"]);
    expect(reset.code, `${reset.stdout}\n${reset.stderr}`).toBe(0);
    await expect(readFile(sessionPath, "utf8")).rejects.toThrow();
  });

  // These tests deliberately replay a revoked token to prove the old grant is
  // dead. Better Auth treats that as theft and invalidates every refresh grant
  // for the same user/client, so keep them after all shared-session journeys.
  test("blue login retires the refresh grant of a rejected session", async ({ page }) => {
    const recoveryHome = await prepareClient("login-session-recovery");
    await loginAsAdmin(page);

    const initial = await approveDeviceFlow(page, spawnCli(recoveryHome, ["login"]));
    expect(initial.code, `${initial.stdout}\n${initial.stderr}`).toBe(0);

    const sessionPath = path.join(recoveryHome, ".config", "blue", "session.json");
    const previous = JSON.parse(await readFile(sessionPath, "utf8")) as PersistedOauthSession;
    expect(previous.refresh_token).toBeTruthy();
    previous.token = "locally-fresh-but-invalid-access-token";
    previous.expires_at = Math.floor(Date.now() / 1000) + 900;
    await writeFile(sessionPath, JSON.stringify(previous), { mode: 0o600 });

    const recovered = await approveDeviceFlow(page, spawnCli(recoveryHome, ["login"]));
    expect(recovered.code, `${recovered.stdout}\n${recovered.stderr}`).toBe(0);
    expect(recovered.stdout).toContain("Logged in");

    const replacement = JSON.parse(await readFile(sessionPath, "utf8")) as PersistedOauthSession;
    expect(replacement.refresh_token).toBeTruthy();
    expect(replacement.refresh_token).not.toBe(previous.refresh_token);
    await expectRefreshGrantRejected(page, previous);
  });

  test("@smoke bare blue replaces a permanently invalid refresh grant after a delayed approval", async ({ page }) => {
    const recoveryHome = await prepareClient("gateway-preflight-recovery");
    await loginAsAdmin(page);

    const initial = await approveDeviceFlow(page, spawnCli(recoveryHome, ["login"]));
    expect(initial.code, `${initial.stdout}\n${initial.stderr}`).toBe(0);
    const preferred = await runCli(recoveryHome, ["agent", "codex"]);
    expect(preferred.code, preferred.stderr).toBe(0);

    const sessionPath = path.join(recoveryHome, ".config", "blue", "session.json");
    const previous = JSON.parse(await readFile(sessionPath, "utf8")) as PersistedOauthSession;
    expect(previous.refresh_token).toBeTruthy();
    const dashboard = process.env.E2E_DASHBOARD_URL ?? "http://127.0.0.1:3000";
    const revoked = await page.request.post(`${dashboard}/api/auth/oauth2/revoke`, {
      headers: { origin: dashboard },
      form: {
        token: previous.refresh_token,
        token_type_hint: "refresh_token",
        client_id: previous.client_id,
      },
    });
    expect(revoked.status(), await revoked.text()).toBeLessThan(300);
    previous.token = "expired-access-token-cannot-clean-up-gateway-binding";
    previous.expires_at = Math.floor(Date.now() / 1000) - 60;
    await writeFile(sessionPath, JSON.stringify(previous), { mode: 0o600 });

    const recovery = spawnCliInPty(recoveryHome, "blue");
    const completion = collect(recovery);
    const deviceUrl = await waitForOutput(recovery, deviceUrlPattern);
    await page.goto(deviceUrl);
    await expect(page.getByText("Confirmation code")).toBeVisible();
    // Wait through a full five-second polling interval. The pending poll must
    // not require a browser-session binding that only approval can create.
    await page.waitForTimeout(6_000);
    expect(recovery.exitCode).toBeNull();
    await page.getByRole("button", { name: "Authorize" }).click();
    await expect(page.getByText("CLI authorized")).toBeVisible();
    const recovered = await completion;
    expect(recovered.code, `${recovered.stdout}\n${recovered.stderr}`).toBe(0);
    expect(recovered.stdout).toContain("fake-codex-ok");
    const recoveryOutput = `${recovered.stdout}\n${recovered.stderr}`;
    expect(recoveryOutput).not.toContain(
      "Device authorization is not bound to a session",
    );
    expect(recoveryOutput).not.toContain(
      "previous remote gateway session could not be confirmed revoked",
    );
    const authorizationUrls = new Set(
      recoveryOutput.match(new RegExp(deviceUrlPattern, "g")) ?? [],
    );
    expect(authorizationUrls.size).toBe(1);

    const replacement = JSON.parse(await readFile(sessionPath, "utf8")) as PersistedOauthSession;
    expect(replacement.refresh_token).toBeTruthy();
    expect(replacement.refresh_token).not.toBe(previous.refresh_token);
  });
});
