import test from "node:test";
import { validateAwsConfiguration } from "./preflight.mjs";
import { verifyWindowsIsolation } from "./windows-isolation.mjs";
import assert from "node:assert/strict";
import {
  access,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve, win32 } from "node:path";
import YAML from "yaml";
import {
  cells,
  binDirectory,
  prepareWindowsAgentStateCleanup,
  validatePackage,
} from "./install-agents.mjs";
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
  assert.equal(binDirectory("/install", "linux"), "/install/bin");
});
test("Windows agent verification removes only runner-owned state", async () => {
  const root = await mkdtemp(join(tmpdir(), "blue agent state "));
  const env = {
    USERPROFILE: join(root, "profile"),
    APPDATA: join(root, "roaming"),
    LOCALAPPDATA: join(root, "local"),
  };
  const owned = [
    join(env.USERPROFILE, ".codex", "tmp", "marker"),
    join(env.USERPROFILE, ".config", "opencode", "marker"),
    join(env.LOCALAPPDATA, "Blue", "marker"),
  ];
  const unrelated = join(env.USERPROFILE, "Documents", "keep.txt");
  try {
    const cleanup = await prepareWindowsAgentStateCleanup({
      platform: "win32",
      env,
    });
    for (const path of [...owned, unrelated]) {
      await mkdir(resolve(path, ".."), { recursive: true });
      await writeFile(path, "test");
    }
    await cleanup();
    for (const path of owned)
      await assert.rejects(access(path), { code: "ENOENT" });
    assert.equal(await readFile(unrelated, "utf8"), "test");
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
test("Windows agent verification refuses pre-existing state", async () => {
  const root = await mkdtemp(join(tmpdir(), "blue existing state "));
  const env = {
    USERPROFILE: join(root, "profile"),
    APPDATA: join(root, "roaming"),
    LOCALAPPDATA: join(root, "local"),
  };
  try {
    await mkdir(join(env.USERPROFILE, ".codex"), { recursive: true });
    await assert.rejects(
      prepareWindowsAgentStateCleanup({ platform: "win32", env }),
      /fresh Windows account required/,
    );
    await access(join(env.USERPROFILE, ".codex"));
  } finally {
    await rm(root, { recursive: true, force: true });
  }
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
    const health = await readFile(join(dir, "health.txt"), "utf8");
    assert.match(health, /^blue-native-health:[0-9a-f-]+\n$/);
    assert.equal(sha256(health), native.healthSha256);
    const grant = JSON.parse(await readFile(join(dir, "package-policy.json")));
    assert.deepEqual(grant.Statement, [
      {
        Effect: "Allow",
        Principal: { AWS: ["*"] },
        Action: ["s3:GetObject"],
        Resource: ["arn:aws:s3:::package-artifacts/health.txt"],
      },
    ]);
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

const runtimeConfiguration = {
  E2E_NATIVE_BUCKET: "private-bucket-value",
  E2E_NATIVE_SUBNET_ID: "subnet-value",
  E2E_NATIVE_SECURITY_GROUP_ID: "security-group-value",
  E2E_NATIVE_INSTANCE_PROFILE: "instance-profile-value",
  E2E_NATIVE_AMI_ID: "ami-value",
};
const ciConfiguration = {
  ...runtimeConfiguration,
  E2E_NATIVE_REGION: "region-value",
  E2E_NATIVE_ROLE_ARN: "role-value",
};

test("AWS preflight accepts complete runtime and CI configurations", () => {
  validateAwsConfiguration(runtimeConfiguration);
  validateAwsConfiguration(ciConfiguration, { ci: true });
});

test("AWS preflight lists every missing or blank setting together", () => {
  assert.throws(
    () =>
      validateAwsConfiguration(
        { E2E_NATIVE_BUCKET: " \t", E2E_NATIVE_AMI_ID: "\n" },
        { ci: true },
      ),
    (error) => {
      for (const name of Object.keys(ciConfiguration))
        assert.ok(error.message.includes(name));
      assert.ok(error.message.includes("tests/e2e-native/infra/README.md"));
      return true;
    },
  );
});

test("AWS preflight permits manual credential/region chain but requires CI inputs", () => {
  validateAwsConfiguration(runtimeConfiguration);
  assert.throws(
    () =>
      validateAwsConfiguration(
        { ...runtimeConfiguration, E2E_NATIVE_REGION: " " },
        { ci: true },
      ),
    /E2E_NATIVE_REGION, E2E_NATIVE_ROLE_ARN/,
  );
});

test("AWS preflight errors name missing settings without supplied values", () => {
  assert.throws(
    () =>
      validateAwsConfiguration(
        { ...ciConfiguration, E2E_NATIVE_SUBNET_ID: "" },
        { ci: true },
      ),
    (error) => {
      assert.ok(error.message.includes("E2E_NATIVE_SUBNET_ID"));
      for (const value of Object.values(ciConfiguration))
        assert.ok(!error.message.includes(value));
      return true;
    },
  );
});

test("Windows isolation requires a real passing test and preserves failure evidence", async () => {
  const dir = await mkdtemp(join(tmpdir(), "blue isolation "));
  const name =
    "platform::windows_tests::sequential_native_profiles_remove_owned_state";
  const success = `test ${name} ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;`;
  const signal = new AbortController().signal;
  try {
    await verifyWindowsIsolation({
      directory: dir,
      signal,
      execute: async (command, args, options) => {
        assert.equal(command, "cargo");
        assert.ok(args.includes(name));
        assert.ok(args.includes("--exact"));
        assert.equal(options.signal, signal);
        assert.equal(options.env.E2E_SLIM_REQUIRED, "1");
        assert.ok(options.timeout > 0);
        return success;
      },
    });
    assert.equal(
      await readFile(join(dir, "windows-isolation.log"), "utf8"),
      success,
    );
    for (const output of [
      "test result: ok. 0 passed; 0 failed; 0 ignored;",
      `test ${name} ... ignored\ntest result: ok. 0 passed; 0 failed; 1 ignored;`,
    ]) {
      await assert.rejects(
        verifyWindowsIsolation({ directory: dir, execute: async () => output }),
        /exactly one passing test/,
      );
      assert.equal(
        await readFile(join(dir, "windows-isolation.log"), "utf8"),
        output,
      );
    }
    await assert.rejects(
      verifyWindowsIsolation({
        directory: dir,
        execute: async () => {
          throw Object.assign(new Error("cargo failed"), {
            output: "linker failed",
          });
        },
      }),
      /cargo failed/,
    );
    assert.equal(
      await readFile(join(dir, "windows-isolation.log"), "utf8"),
      "linker failed",
    );
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
