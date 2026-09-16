import semver from "semver";
import { readFile, mkdir, writeFile, rm, lstat } from "node:fs/promises";
import { join, delimiter, posix, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { repo } from "./prepare-fixtures.mjs";
import { run } from "./process.mjs";
export const binDirectory = (prefix, platform = process.platform) =>
  platform === "win32" ? prefix : posix.join(prefix, "bin");
export async function prepareWindowsAgentStateCleanup({
  platform = process.platform,
  env = process.env,
  remove = rm,
  inspect = lstat,
} = {}) {
  if (platform !== "win32") return async () => {};
  for (const key of ["USERPROFILE", "APPDATA", "LOCALAPPDATA"])
    if (!env[key]) throw new Error(`${key} is required on Windows`);
  const owned = [
    ...[
      ".config/blue",
      ".cache/blue",
      ".codex",
      ".claude",
      ".claude.json",
      ".claude.json.backup",
      ".kimi",
      ".kimi-code",
      ".config/opencode",
      ".local/share/opencode",
      ".local/state/opencode",
      ".cache/opencode",
    ].map((relative) => join(env.USERPROFILE, relative)),
    ...[env.APPDATA, env.LOCALAPPDATA].flatMap((root) =>
      ["Blue", "opencode", "kimi"].map((name) => join(root, name)),
    ),
  ];
  for (const path of owned)
    try {
      await inspect(path);
      throw new Error(`fresh Windows account required; refusing ${path}`);
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
  return async () =>
    Promise.all(
      owned.map((path) => remove(path, { recursive: true, force: true })),
    );
}
export async function cells(matrix = true) {
  const lock = JSON.parse(
    await readFile(join(repo, "tests/e2e/agents.lock.json")),
  );
  const historical = JSON.parse(
    await readFile(join(repo, "tests/e2e-slim/agents.matrix.json")),
  );
  return Object.entries(lock.agents).flatMap(([agent, pin]) =>
    [
      pin.version,
      ...(matrix
        ? historical.agents[agent].versions.map((v) => v.version)
        : []),
    ].map((version) => ({
      agent,
      version,
      package: pin.package,
      pin: version === pin.version,
    })),
  );
}
export function validatePackage(metadata, node, platform, architecture) {
  const engines = metadata.engines || metadata;
  if (engines.node && !semver.satisfies(node, engines.node))
    throw new Error(`requires Node ${engines.node}; actual ${node}`);
  for (const [key, actual] of [
    ["os", platform],
    ["cpu", architecture],
  ]) {
    const allowed = metadata[key];
    if (
      allowed &&
      (allowed.includes(`!${actual}`) ||
        (allowed.some((v) => !v.startsWith("!")) &&
          !allowed.includes(actual) &&
          !allowed.includes("any")))
    )
      throw new Error(
        `unsupported ${key} ${actual}; package declares ${allowed}`,
      );
  }
}
export async function installAgents(directory, matrix = true) {
  if (Number(process.versions.node.split(".")[0]) < 22)
    throw new Error("Node >=22 required");
  await mkdir(directory, { recursive: true });
  const expected = await cells(matrix),
    manifest = {},
    report = {
      os: process.platform,
      arch: process.arch,
      node: process.version,
      cells: [],
    };
  const pins = [];
  // Check every pinned package before installing any cell. Report unsupported
  // OS/architecture and Node engines with the precise package/version.
  for (const cell of expected) {
    try {
      const metadata = JSON.parse(
        await run(
          "npm",
          [
            "view",
            `${cell.package}@${cell.version}`,
            "engines",
            "os",
            "cpu",
            "--json",
          ],
          { capture: true },
        ),
      );
      validatePackage(
        metadata,
        process.versions.node,
        process.platform,
        process.arch,
      );
    } catch (error) {
      report.cells = expected.map((c) => ({
        ...c,
        status: c === cell ? "failed-or-unsupported" : "not-run",
        ...(c === cell ? { reason: error.message } : {}),
      }));
      await writeFile(
        join(directory, "install-report.json"),
        JSON.stringify(report, null, 2),
      );
      throw new Error(`${cell.agent} ${cell.version}: ${error.message}`);
    }
  }
  const cleanupAgentState = await prepareWindowsAgentStateCleanup();
  try {
    for (const cell of expected) {
      const prefix = join(directory, cell.agent, cell.version),
        bin = binDirectory(prefix);
      const entry = { ...cell, status: "installing" };
      report.cells.push(entry);
      try {
        console.log(
          `Installing/verifying ${cell.agent} ${cell.version} (${process.platform}/${process.arch})`,
        );
        // Every prefix is owned by this runner; a fresh prefix avoids npm's
        // optional-native-dependency pruning on repeat global installs.
        await rm(prefix, { recursive: true, force: true });
        // npm enforces engines and package OS/CPU constraints, including native optional packages.
        await run(
          "npm",
          [
            "install",
            "--global",
            "--engine-strict",
            "--prefix",
            prefix,
            `${cell.package}@${cell.version}`,
          ],
          { capture: true },
        );
        const actual = await run(
          join(bin, cell.agent + (process.platform === "win32" ? ".cmd" : "")),
          ["--version"],
          { capture: true, timeout: 60_000 },
        );
        if (
          !actual.match(/\d+\.\d+\.\d+(?:-[\w.-]+)?/g)?.includes(cell.version)
        )
          throw new Error(`expected ${cell.version}, got ${actual}`);
        entry.actual = actual;
        entry.status = "installed";
        (manifest[cell.agent] ||= {})[cell.version] = bin;
        if (cell.pin) pins.push(bin);
      } catch (error) {
        entry.status = "failed-or-unsupported";
        entry.reason = error.message;
        throw error;
      }
    }
  } finally {
    for (const cell of expected)
      if (
        !report.cells.some(
          (e) => e.agent === cell.agent && e.version === cell.version,
        )
      )
        report.cells.push({ ...cell, status: "not-run" });
    try {
      await writeFile(
        join(directory, "install-report.json"),
        JSON.stringify(report, null, 2),
      );
    } finally {
      await cleanupAgentState();
    }
  }
  const path = join(directory, "matrix.json");
  await writeFile(path, JSON.stringify(manifest, null, 2));
  return {
    manifest: path,
    path: [...pins, process.env.PATH].join(delimiter),
    expected,
  };
}
if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
)
  await installAgents(
    resolve(process.argv[2]),
    !process.argv.includes("--pins-only"),
  );
