import { cp, copyFile, mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { checkReleaseSnapshot, releasedNavigation } from "./check-release-snapshot.mjs";

const version = process.argv[2];
if (!version || !/^\d+\.\d+\.\d+$/.test(version)) {
  throw new Error("usage: npm run release:docs -- <major.minor.patch>");
}

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");
const docsRoot = resolve(root, "apps/docs");
const cargo = await readFile(resolve(root, "Cargo.toml"), "utf8");
const workspaceVersion = cargo.match(/\[workspace\.package\][\s\S]*?version\s*=\s*"([^"]+)"/)?.[1];
const contract = await readFile(resolve(root, "deploy/contract/governance.openapi.yaml"), "utf8");
const contractVersion = contract.match(/^  version:\s*"([^"]+)"$/m)?.[1];

if (workspaceVersion !== version || contractVersion !== version) {
  throw new Error(`version mismatch: requested=${version}, workspace=${workspaceVersion}, OpenAPI=${contractVersion}`);
}

const releaseDir = resolve(docsRoot, version);
const configPath = resolve(docsRoot, "docs.json");
const config = JSON.parse(await readFile(configPath, "utf8"));
if (config.navigation.versions.some((entry) => entry.version === version)) {
  throw new Error(`documentation version ${version} already exists in docs.json`);
}
const next = config.navigation.versions.find((entry) => entry.version === "Next");
if (!next) throw new Error("docs.json has no Next version to snapshot");

await cp(resolve(docsRoot, "next"), releaseDir, {
  recursive: true,
  errorOnExist: true,
  force: false,
});

async function rewriteLinks(directory) {
  const entries = await (await import("node:fs/promises")).readdir(directory, { withFileTypes: true });
  for (const entry of entries) {
    const path = resolve(directory, entry.name);
    if (entry.isDirectory()) await rewriteLinks(path);
    if (entry.isFile() && entry.name.endsWith(".mdx")) {
      const content = await readFile(path, "utf8");
      await writeFile(path, content.replaceAll("/next/", `/${version}/`));
    }
  }
}
await rewriteLinks(releaseDir);

await mkdir(resolve(docsRoot, "openapi"), { recursive: true });
await copyFile(resolve(root, "deploy/contract/governance.openapi.yaml"), resolve(docsRoot, `openapi/${version}.yaml`));

const stable = releasedNavigation(next, version);
config.navigation.versions = [stable, next, ...config.navigation.versions.filter((entry) => entry.version !== "Next")];
await writeFile(configPath, `${JSON.stringify(config, null, 2)}\n`);
await checkReleaseSnapshot(version, root);
console.log(`Created immutable documentation snapshot ${version}.`);
