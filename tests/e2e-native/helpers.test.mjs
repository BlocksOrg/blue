import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, win32 } from "node:path";
import YAML from "yaml";
import { cells, binDirectory, validatePackage } from "./install-agents.mjs";
import { prepareFixtures, sha256, repo } from "./prepare-fixtures.mjs";
import { assertFreePorts, sanitize, portsFor } from "./backend.mjs";
import { waitFor } from "./process.mjs";
import { createServer } from "node:net";

test("canonical lock supplies every unique pin and historical cell without slim symlink", async () => {
  const grid = await cells();
  assert.equal(grid.filter((c) => c.pin).length, 4);
  assert.equal(
    new Set(grid.map((c) => `${c.agent}/${c.version}`)).size,
    grid.length,
  );
  assert.ok(grid.length > 4);
});
test("npm prefix selection uses the native layout", () => {
  assert.equal(binDirectory("C:\\install", "win32"), "C:\\install");
  assert.equal(binDirectory("/install", "darwin"), "/install/bin");
});
test("fixture generation preserves legacy bytes and renders native policies as data", async () => {
  const dir = await mkdtemp(join(tmpdir(), "blue fixtures # "));
  try {
    const legacy = await prepareFixtures(dir, { legacy: true });
    assert.equal(
      legacy.packageSha256,
      "f84937b7e4221f795d8603b954cff846c266276fe2bc38e8e98c9812f4c1593f",
    );
    const native = await prepareFixtures(dir, { platform: "win32" });
    const policy = YAML.parse(
      await readFile(join(dir, "gateway-slim.yaml"), "utf8"),
    );
    assert.equal(
      policy.governance.packages[0].sha256,
      sha256(await readFile(join(dir, "e2e-package.tar.gz"))),
    );
    assert.equal(policy.governance.packages[0].sha256, native.packageSha256);
    assert.equal(
      policy.governance.harnesses.codex.mcp[0].args[0],
      join(dir, "mcp-server.mjs"),
    );
    const windowsPath = win32.join("C:\\Users\\Test # User", "mcp-server.mjs");
    assert.equal(
      YAML.parse(YAML.stringify({ path: windowsPath })).path,
      windowsPath,
    );
    assert.equal(
      policy.control_api.auth.issuer,
      "os.environ/HARNESS_AUTH_ISSUER",
    );
    assert.ok(
      !(
        await readFile(join(dir, "source/e2e-package/hooks/kimi.toml"), "utf8")
      ).includes("sh -c"),
    );
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
test("occupied client port fails preflight", async () => {
  const server = createServer();
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  try {
    await assert.rejects(
      assertFreePorts([server.address().port]),
      /EADDRINUSE/,
    );
  } finally {
    await new Promise((resolve) => server.close(resolve));
  }
  assert.deepEqual(portsFor("governance"), [8080, 5432, 9000]);
  assert.ok(!portsFor("gateway").includes(8082));
});
test("waits terminate on success, failure and deadline", async () => {
  await waitFor("success", async () => true, 10, 1);
  await assert.rejects(
    waitFor(
      "terminal",
      async () => {
        throw new Error("terminated");
      },
      10,
      1,
    ),
    /terminated/,
  );
  await assert.rejects(
    waitFor("pending", async () => false, 5, 1),
    /TIMEOUT/,
  );
});
test("logs redact credentials and signed URLs", () => {
  const output = sanitize(
    "Bearer token sk-secret eyJxxx.abc.def X-Amz-Signature=123 provider-key",
    ["provider-key"],
  );
  for (const secret of [
    "token",
    "sk-secret",
    "eyJxxx",
    "Signature=123",
    "provider-key",
  ])
    assert.ok(!output.includes(secret));
});

test("preflight rejects unsupported runtimes and architectures before installing", () => {
  assert.throws(
    () =>
      validatePackage(
        { engines: { node: ">=22.19.0" } },
        "22.14.0",
        "win32",
        "x64",
      ),
    /requires Node/,
  );
  assert.throws(
    () => validatePackage({ os: ["!win32"] }, "22.23.2", "win32", "x64"),
    /unsupported os/,
  );
  assert.throws(
    () => validatePackage({ cpu: ["x64"] }, "22.23.2", "win32", "arm64"),
    /unsupported cpu/,
  );
  validatePackage(
    { engines: { node: ">=22.19" }, os: ["win32"], cpu: ["x64"] },
    "22.23.2",
    "win32",
    "x64",
  );
});

test("backend cleanup still terminates the lease and removes objects after log/Compose failure", async () => {
  const { Backend } = await import("./backend.mjs");
  const calls = [];
  const backend = new Backend({
    mode: "aws",
    suite: "governance",
    directory: "/unused",
    fixtures: "/unused",
    execute: {
      aws: async (args) => {
        calls.push(args);
        return {};
      },
      run: async (command, args) => {
        calls.push([command, ...args]);
        return "";
      },
    },
  });
  backend.lease = {
    instanceId: "i-test",
    bucket: "e2e-test",
    s3Prefix: "runs/test/1/linux/governance",
  };
  backend.ssm = async () => {
    throw new Error("backend unavailable");
  };
  await assert.rejects(backend.cleanup(), /backend unavailable/);
  assert.ok(
    calls.some(
      (args) => args.includes("terminate-instances") && args.includes("i-test"),
    ),
  );
  assert.ok(
    calls.some(
      (args) =>
        args.includes("rm") &&
        args.includes("s3://e2e-test/runs/test/1/linux/governance/"),
    ),
  );
});

test("cancelled native subprocess exits and fails instead of hanging", async () => {
  const { run } = await import("./process.mjs");
  const controller = new AbortController();
  const result = run(process.execPath, ["-e", "setInterval(()=>{},1000)"], {
    signal: controller.signal,
    capture: true,
    timeout: 5000,
  });
  controller.abort(new Error("tunnel lost"));
  await assert.rejects(result, /tunnel lost/);
});
