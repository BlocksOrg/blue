import { parseArgs } from "node:util";
import { resolve, join } from "node:path";
import { mkdir, writeFile, readFile, rm } from "node:fs/promises";
import { prepareFixtures, repo } from "./prepare-fixtures.mjs";
import { installAgents, cells } from "./install-agents.mjs";
import { Backend, sanitize } from "./backend.mjs";
import { run } from "./process.mjs";

const { values } = parseArgs({
  options: {
    backend: { type: "string", default: "local" },
    suite: { type: "string", default: "governance" },
    matrix: { type: "boolean", default: true },
    "cleanup-only": { type: "boolean", default: false },
    directory: {
      type: "string",
      default: process.env.E2E_NATIVE_RUN_DIR || "tests/e2e-native/artifacts",
    },
  },
});
if (
  !["local", "aws"].includes(values.backend) ||
  !["governance", "gateway"].includes(values.suite)
)
  throw new Error("--backend local|aws --suite governance|gateway required");
const directory = resolve(values.directory);
await mkdir(directory, { recursive: true });
const backend = new Backend({
  mode: values.backend,
  suite: values.suite,
  directory,
  fixtures: join(directory, "fixtures"),
  image: process.env.E2E_NATIVE_IMAGE_TAR,
});
if (values["cleanup-only"]) {
  try {
    backend.lease = JSON.parse(await readFile(backend.manifestPath));
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }
  await backend.cleanup();
} else {
  if (values.suite === "gateway" && !process.env.OPENROUTER_API_KEY)
    throw new Error("gateway NOT RUN: OPENROUTER_API_KEY is required");
  if (!values.matrix)
    throw new Error("native certification requires the full --matrix");
  const reportDir = join(directory, "cells");
  await rm(reportDir, { recursive: true, force: true });
  const expected = await cells(true);
  const report = {
    os: process.platform,
    architecture: process.arch,
    suite: values.suite,
    expected,
    executed: [],
    status: "failed",
  };
  let failure;
  const cancel = () =>
    backend.controller.abort(new Error("native run cancelled"));
  process.once("SIGINT", cancel);
  process.once("SIGTERM", cancel);
  try {
    const rust = await run("rustc", ["-vV"], { capture: true });
    if (process.platform === "win32") {
      if (!rust.includes("pc-windows-msvc"))
        throw new Error("native Windows requires Rust MSVC + Windows SDK");
      if (process.env.E2E_SLIM_DISPOSABLE_ACCOUNT !== "1")
        throw new Error(
          "fresh disposable Windows account must be acknowledged with E2E_SLIM_DISPOSABLE_ACCOUNT=1",
        );
      await run("bash", ["--version"], { capture: true });
    }
    await run("cargo", ["nextest", "--version"], { capture: true });
    // This also verifies the MSVC linker and SDK before leasing cloud resources.
    await run("cargo", ["build", "--locked", "-p", "gh-cli", "--bin", "blue"], {
      cwd: repo,
      timeout: 1_800_000,
      signal: backend.controller.signal,
    });
    const agents = await installAgents(join(directory, "agents"));
    const hashes = await prepareFixtures(backend.fixtures);
    await backend.start(hashes);
    const env = {
      ...process.env,
      PATH: agents.path,
      E2E_SLIM_REQUIRED: "1",
      E2E_SLIM_AGENT_MATRIX: agents.manifest,
      E2E_SLIM_REPORT_DIR: reportDir,
      E2E_SLIM_BLUE_BIN: join(
        repo,
        "target/debug/blue" + (process.platform === "win32" ? ".exe" : ""),
      ),
      E2E_SLIM_CONTROL_API_URL: "http://127.0.0.1:8080",
      E2E_SLIM_DATABASE_URL: backend.databaseUrl,
      E2E_SLIM_MINIO_URL: "http://127.0.0.1:9000",
    };
    if (values.suite === "gateway")
      Object.assign(env, {
        E2E_SLIM_LITELLM_URL: "http://127.0.0.1:4000",
        E2E_SLIM_LITELLM_MASTER_KEY:
          process.env.LITELLM_MASTER_KEY || "sk-e2e-slim-master",
        E2E_SLIM_INFERENCE_PROXY_URL: "http://127.0.0.1:8081",
        E2E_SLIM_OPENROUTER: "1",
      });
    // Same bodies, same generated cells and retry policy on every OS.
    let testLog = "";
    try {
      testLog = await run(
        "cargo",
        [
          "nextest",
          "run",
          "--locked",
          "--manifest-path",
          join(repo, "tests/e2e-slim/Cargo.toml"),
          "--config-file",
          join(repo, "tests/e2e-slim/.config/nextest.toml"),
          "--profile",
          values.suite,
          "--test-threads",
          "1",
          "--no-fail-fast",
        ],
        {
          cwd: repo,
          env,
          timeout: 4_800_000,
          signal: backend.controller.signal,
          capture: true,
          combine: true,
        },
      );
    } catch (error) {
      testLog = error.output || error.message;
      throw error;
    } finally {
      await writeFile(
        join(directory, "nextest.log"),
        sanitize(testLog, [
          process.env.OPENROUTER_API_KEY,
          process.env.LITELLM_MASTER_KEY,
        ]),
      );
    }
    for (const cell of expected)
      report.executed.push(
        JSON.parse(
          await readFile(
            join(
              reportDir,
              `${values.suite}-${cell.agent}-${cell.version}.json`,
            ),
          ),
        ),
      );
    if (report.executed.length === 0)
      throw new Error("zero real agent cells executed");
    report.status = "passed";
  } catch (error) {
    failure = error;
    report.reason = sanitize(error.message, [
      process.env.OPENROUTER_API_KEY,
      process.env.LITELLM_MASTER_KEY,
    ]);
  } finally {
    // Include partial evidence when nextest/install/startup failed.
    report.cells = await Promise.all(
      expected.map(async (cell) => {
        try {
          return JSON.parse(
            await readFile(
              join(
                reportDir,
                `${values.suite}-${cell.agent}-${cell.version}.json`,
              ),
            ),
          );
        } catch {
          return { ...cell, status: "not-certified" };
        }
      }),
    );
    try {
      await backend.cleanup();
    } catch (error) {
      failure ||= error;
      report.cleanupError = error.message;
      report.status = "failed";
    }
    await writeFile(
      join(directory, "coverage.json"),
      JSON.stringify(report, null, 2),
    );
    console.log(
      `${report.suite} ${report.os}: ${report.cells.filter((c) => c.status === "passed").length}/${expected.length} cells passed; ${report.status}`,
    );
    process.removeListener("SIGINT", cancel);
    process.removeListener("SIGTERM", cancel);
  }
  if (failure)
    throw new Error(
      sanitize(failure.message, [
        process.env.OPENROUTER_API_KEY,
        process.env.LITELLM_MASTER_KEY,
      ]),
    );
}
