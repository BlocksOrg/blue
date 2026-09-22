import { expect, test, type Page } from "@playwright/test";
import { readFile, rm } from "node:fs/promises";
import path from "node:path";
import YAML from "yaml";
import {
  collect,
  prepareClient,
  readClientFile,
  runCli,
  spawnCli,
  waitForOutput,
} from "../support/cli.js";
import { loginAsAdmin } from "../support/dashboard.js";

const CONTROL = process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080";
const DEVICE_URL = /http:\/\/127\.0\.0\.1:3000\/device\/[A-Za-z0-9_-]+/;
const CAPABILITIES = [
  "tenant_client_version_pin",
  "adapter_intervals",
  "compiled_harness_registry",
  "transactional_reconcile",
  "versioned_state",
  "unverified_harness_versions",
  "gateway_inference_jwt",
];

async function completeDeviceLogin(page: Page, home: string): Promise<void> {
  const login = spawnCli(home, ["login"]);
  const completion = collect(login);
  const deviceUrl = await waitForOutput(login, DEVICE_URL);
  await page.goto(deviceUrl);
  await page.getByRole("button", { name: "Authorize" }).click();
  await expect(page.getByText("CLI authorized")).toBeVisible();
  const result = await completion;
  expect(result.code, `${result.stdout}\n${result.stderr}`).toBe(0);
}

function withClientVersionPin(managedYaml: string, version: string): string {
  const document = YAML.parse(managedYaml);
  document.required_client_version = version;
  document.required_capabilities = Array.from(
    new Set([
      ...(document.required_capabilities ?? []),
      "tenant_client_version_pin",
    ]),
  );
  return YAML.stringify(document);
}

test("@smoke tenant client-version pins reject incompatible CLI commands without changing managed state", async ({ page }) => {
  const home = await prepareClient("client-version-pin");
  await loginAsAdmin(page, { fresh: true });

  const versionResult = await runCli(home, ["version"]);
  expect(versionResult.code, versionResult.stderr).toBe(0);
  const executingVersion = versionResult.stdout.match(/Blue metaharness (\S+)/)?.[1];
  expect(executingVersion).toMatch(/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/);
  const executingMajor = Number(executingVersion!.split(".")[0]);
  const [major, minor, patchVersion] = executingVersion!.split(/[.+-]/).slice(0, 3).map(Number);
  const recommendedVersion = `${major}.${minor}.${patchVersion + 1}`;
  const incompatibleVersion = `${executingMajor + 1}.0.0`;

  const originalResponse = await page.request.get(`${CONTROL}/admin/governance-config`);
  expect(originalResponse.status(), await originalResponse.text()).toBe(200);
  const original = await originalResponse.json();

  try {
    await completeDeviceLogin(page, home);

    const gateway = await runCli(home, ["gateway"]);
    expect(gateway.code, gateway.stderr).toBe(0);
    expect(gateway.stdout).toContain("status          : ready");

    const matchingResponse = await page.request.put(`${CONTROL}/admin/governance-config`, {
      data: {
        base_revision: original.revision,
        managed_yaml: withClientVersionPin(original.managed_yaml, executingVersion!),
      },
    });
    expect(matchingResponse.status(), await matchingResponse.text()).toBe(200);
    const matching = await matchingResponse.json();

    const selectCodex = await runCli(home, ["agent", "codex"]);
    expect(selectCodex.code, selectCodex.stderr).toBe(0);
    const initialApply = await runCli(home, ["apply", "--yes"]);
    expect(initialApply.code, initialApply.stderr).toBe(0);

    const managedPath = path.join(home, ".codex", "blue.config.toml");
    const cachePath = path.join(home, ".cache", "blue", "governance-config.json");
    const managedBeforeMismatch = await readFile(managedPath);
    expect((await readFile(cachePath)).length).toBeGreaterThan(0);

    const session = JSON.parse(await readClientFile(home, ".config/blue/session.json"));
    const compatible = await page.request.get(`${CONTROL}/governance-config`, {
      headers: {
        authorization: `Bearer ${session.token}`,
        "x-blue-contract-version": "3",
        "x-blue-capabilities": CAPABILITIES.join(","),
      },
    });
    expect(compatible.status(), await compatible.text()).toBe(200);
    expect(compatible.headers()["x-blue-required-client-version"]).toBe(executingVersion);
    const compatibleDocument = await compatible.json();
    expect(compatibleDocument.required_client_version).toBe(executingVersion);
    expect(compatibleDocument.required_capabilities).toContain("tenant_client_version_pin");

    const missingCapability = await page.request.get(`${CONTROL}/governance-config`, {
      headers: {
        authorization: `Bearer ${session.token}`,
        "x-blue-contract-version": "3",
        "x-blue-capabilities": CAPABILITIES.filter(
          (capability) => capability !== "tenant_client_version_pin",
        ).join(","),
      },
    });
    expect(missingCapability.status(), await missingCapability.text()).toBe(426);
    expect(missingCapability.headers()["x-blue-required-client-version"]).toBe(executingVersion);

    const recommendedResponse = await page.request.put(`${CONTROL}/admin/governance-config`, {
      data: {
        base_revision: matching.revision,
        managed_yaml: withClientVersionPin(matching.managed_yaml, recommendedVersion),
      },
    });
    expect(recommendedResponse.status(), await recommendedResponse.text()).toBe(200);
    const recommended = await recommendedResponse.json();

    for (const args of [["apply", "--yes"], ["config"], ["doctor"]]) {
      const allowed = await runCli(home, args);
      const output = `${allowed.stdout}\n${allowed.stderr}`;
      expect(allowed.code, output).toBe(0);
      expect(output.match(/is compatible, but this tenant recommends/g)).toHaveLength(1);
      expect(output).toContain(`Blue ${recommendedVersion}`);
      expect(output).not.toContain(`Install Blue ${recommendedVersion} now`);
    }
    expect((await readFile(cachePath)).length).toBeGreaterThan(0);
    expect(await readFile(managedPath)).toEqual(managedBeforeMismatch);

    const rangedPin = await page.request.put(`${CONTROL}/admin/governance-config`, {
      data: {
        base_revision: recommended.revision,
        managed_yaml: withClientVersionPin(recommended.managed_yaml, `^${executingVersion}`),
      },
    });
    expect(rangedPin.status(), await rangedPin.text()).toBe(400);
    const afterRejectedPin = await (
      await page.request.get(`${CONTROL}/admin/governance-config`)
    ).json();
    expect(afterRejectedPin.revision).toBe(recommended.revision);
    expect(afterRejectedPin.managed_yaml).toBe(recommended.managed_yaml);

    const incompatibleResponse = await page.request.put(`${CONTROL}/admin/governance-config`, {
      data: {
        base_revision: recommended.revision,
        managed_yaml: withClientVersionPin(recommended.managed_yaml, incompatibleVersion),
      },
    });
    expect(incompatibleResponse.status(), await incompatibleResponse.text()).toBe(200);

    const personalized = await page.request.get(`${CONTROL}/governance-config`, {
      headers: {
        authorization: `Bearer ${session.token}`,
        "x-blue-contract-version": "3",
        "x-blue-capabilities": CAPABILITIES.join(","),
      },
    });
    expect(personalized.status(), await personalized.text()).toBe(200);
    expect(personalized.headers()["x-blue-required-client-version"]).toBe(incompatibleVersion);
    expect((await personalized.json()).required_client_version).toBe(incompatibleVersion);

    await rm(path.join(home, "agent-log", "codex.env"), { force: true });
    for (const args of [["apply", "--yes"], ["run", "codex"]]) {
      const blocked = await runCli(home, args);
      const output = `${blocked.stdout}\n${blocked.stderr}`;
      expect(blocked.code, output).not.toBe(0);
      expect(output).toContain(
        `Blue ${executingVersion} has an incompatible major version; this tenant requires Blue ${incompatibleVersion}.`,
      );
      expect(output).toContain(`Release: https://github.com/BlocksOrg/blue/releases/tag/v${incompatibleVersion}`);
      expect(output).toContain("Or run `blue reset` to detach from this tenant.");
      expect(output).not.toContain(`Install Blue ${incompatibleVersion} now`);
      expect(await readFile(managedPath)).toEqual(managedBeforeMismatch);
      await expect(readFile(path.join(home, "agent-log", "codex.env"))).rejects.toThrow();
    }
    await expect(readFile(cachePath)).rejects.toThrow();

    const reset = await runCli(home, ["reset", "--yes"]);
    expect(reset.code, `${reset.stdout}\n${reset.stderr}`).toBe(0);
    expect(reset.stdout).toContain("Blue reset complete");
  } finally {
    const currentResponse = await page.request.get(`${CONTROL}/admin/governance-config`);
    expect(currentResponse.status(), await currentResponse.text()).toBe(200);
    const current = await currentResponse.json();
    const restored = await page.request.put(`${CONTROL}/admin/governance-config`, {
      data: { base_revision: current.revision, managed_yaml: original.managed_yaml },
    });
    expect(restored.status(), await restored.text()).toBe(200);
  }
});
