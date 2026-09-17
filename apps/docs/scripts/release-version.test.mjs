import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { copyFile, mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import test from "node:test";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import { releaseVersion } from "./release-version.mjs";

const version = "2.3.4";
const execFileAsync = promisify(execFile);
const scripts = dirname(fileURLToPath(import.meta.url));

async function fixture() {
  const root = await mkdtemp(resolve(tmpdir(), "blue-release-docs-"));
  const docs = resolve(root, "apps/docs");
  await mkdir(resolve(docs, "next"), { recursive: true });
  await mkdir(resolve(docs, "scripts"), { recursive: true });
  await mkdir(resolve(root, "deploy/contract"), { recursive: true });
  await copyFile(resolve(scripts, "release-version.mjs"), resolve(docs, "scripts/release-version.mjs"));
  await copyFile(
    resolve(scripts, "check-release-snapshot.mjs"),
    resolve(docs, "scripts/check-release-snapshot.mjs"),
  );
  await writeFile(resolve(root, "Cargo.toml"), `[workspace.package]\nversion = "${version}"\n`);
  await writeFile(resolve(root, "deploy/contract/governance.openapi.yaml"), `info:\n  version: "${version}"\n`);
  await writeFile(resolve(docs, "next/index.mdx"), "See /next/guide.\n");
  await writeFile(resolve(docs, "docs.json"), `${JSON.stringify({
    navigation: { versions: [{ version: "Next", pages: ["next/index"] }] },
  }, null, 2)}\n`);
  return root;
}

async function runCli(root, env = {}) {
  return execFileAsync(
    process.execPath,
    [resolve(root, "apps/docs/scripts/release-version.mjs"), version, "--replace-current"],
    {
      env: {
        ...process.env,
        GITHUB_ACTIONS: "",
        GITHUB_WORKFLOW: "",
        ...env,
      },
    },
  );
}

async function fixtureWithSnapshot() {
  const root = await fixture();
  await releaseVersion(version, { root });
  await writeFile(resolve(root, "apps/docs/next/index.mdx"), "Updated /next/guide.\n");
  return root;
}

async function generatedState(root) {
  return {
    docs: await readFile(resolve(root, `apps/docs/${version}/index.mdx`), "utf8"),
    openapi: await readFile(resolve(root, `apps/docs/openapi/${version}.yaml`), "utf8"),
    config: await readFile(resolve(root, "apps/docs/docs.json"), "utf8"),
  };
}

test("creates, replaces, and idempotently refreshes the current release", async (t) => {
  const root = await fixture();
  t.after(() => rm(root, { recursive: true, force: true }));

  await releaseVersion(version, { root });
  assert.equal((await generatedState(root)).docs, `See /${version}/guide.\n`);
  await assert.rejects(() => releaseVersion(version, { root }), /already exists/);

  await writeFile(resolve(root, "apps/docs/next/index.mdx"), "Updated /next/guide.\n");
  await releaseVersion(version, { root, replaceCurrent: true });
  const refreshed = await generatedState(root);
  assert.equal(refreshed.docs, `Updated /${version}/guide.\n`);

  await releaseVersion(version, { root, replaceCurrent: true });
  assert.deepEqual(await generatedState(root), refreshed);
});

test("refuses to replace a non-current historical release", async (t) => {
  const root = await fixture();
  t.after(() => rm(root, { recursive: true, force: true }));
  await releaseVersion(version, { root });

  const path = resolve(root, "apps/docs/docs.json");
  const config = JSON.parse(await readFile(path, "utf8"));
  config.navigation.versions.unshift({ version: "3.0.0", pages: ["3.0.0/index"] });
  await writeFile(path, `${JSON.stringify(config, null, 2)}\n`);

  await assert.rejects(
    () => releaseVersion(version, { root, replaceCurrent: true }),
    /refusing to replace historical documentation version/,
  );
});

test("CLI refuses replacement outside GitHub Actions without mutation", async (t) => {
  const root = await fixtureWithSnapshot();
  t.after(() => rm(root, { recursive: true, force: true }));
  const before = await generatedState(root);

  await assert.rejects(() => runCli(root), /reserved for the Release Please finalizer/);
  assert.deepEqual(await generatedState(root), before);
});

test("CLI refuses replacement from another GitHub Actions workflow without mutation", async (t) => {
  const root = await fixtureWithSnapshot();
  t.after(() => rm(root, { recursive: true, force: true }));
  const before = await generatedState(root);

  await assert.rejects(
    () => runCli(root, { GITHUB_ACTIONS: "true", GITHUB_WORKFLOW: "Release" }),
    /reserved for the Release Please finalizer/,
  );
  assert.deepEqual(await generatedState(root), before);
});

test("CLI permits replacement from the Release Please finalizer", async (t) => {
  const root = await fixtureWithSnapshot();
  t.after(() => rm(root, { recursive: true, force: true }));

  await runCli(root, { GITHUB_ACTIONS: "true", GITHUB_WORKFLOW: "Release Please" });
  assert.equal((await generatedState(root)).docs, `Updated /${version}/guide.\n`);
});
