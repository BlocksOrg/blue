import { readFile, readdir } from "node:fs/promises";
import { dirname, extname, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");

export function releasedNavigation(next, version) {
  return JSON.parse(
    JSON.stringify(next)
      .replaceAll('"Next"', `"${version}"`)
      .replaceAll("next/", `${version}/`)
      .replaceAll("openapi/next.yaml", `openapi/${version}.yaml`),
  );
}

async function filesBelow(directory) {
  const files = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = resolve(directory, entry.name);
    if (entry.isDirectory()) files.push(...await filesBelow(path));
    else files.push(path);
  }
  return files;
}

export async function checkReleaseSnapshot(version, root = repositoryRoot) {
  if (!version || !/^\d+\.\d+\.\d+$/.test(version)) {
    throw new Error("usage: npm run check:release -- <major.minor.patch>");
  }

  const docsRoot = resolve(root, "apps/docs");
  const nextDirectory = resolve(docsRoot, "next");
  const releaseDirectory = resolve(docsRoot, version);
  const failures = [];

  const config = JSON.parse(await readFile(resolve(docsRoot, "docs.json"), "utf8"));
  const versions = config.navigation?.versions ?? [];
  const nextEntries = versions.filter((entry) => entry.version === "Next");
  const releaseEntries = versions.filter((entry) => entry.version === version);

  if (nextEntries.length !== 1) {
    failures.push(`docs.json: expected exactly one Next version, found ${nextEntries.length}`);
  }
  if (releaseEntries.length !== 1) {
    failures.push(`docs.json: expected exactly one ${version} version, found ${releaseEntries.length}`);
  }
  if (versions[0]?.version !== version) {
    failures.push(`docs.json: ${version} must be the first and default documentation version`);
  }
  if (
    nextEntries.length === 1
    && releaseEntries.length === 1
    && JSON.stringify(releaseEntries[0]) !== JSON.stringify(releasedNavigation(nextEntries[0], version))
  ) {
    failures.push(`docs.json: ${version} navigation is not an exact snapshot of Next navigation`);
  }

  let releaseFiles = [];
  try {
    releaseFiles = await filesBelow(releaseDirectory);
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
    failures.push(`missing documentation snapshot directory apps/docs/${version}`);
  }

  if (releaseFiles.length) {
    const nextFiles = await filesBelow(nextDirectory);
    const expectedPaths = new Set(nextFiles.map((path) => relative(nextDirectory, path)));
    const actualPaths = new Set(releaseFiles.map((path) => relative(releaseDirectory, path)));

    for (const path of expectedPaths) {
      if (!actualPaths.has(path)) {
        failures.push(`${version}/${path}: missing from release snapshot`);
        continue;
      }

      const nextPath = resolve(nextDirectory, path);
      const releasePath = resolve(releaseDirectory, path);
      const nextContent = await readFile(nextPath);
      const expectedContent = extname(path) === ".mdx"
        ? Buffer.from(nextContent.toString("utf8").replaceAll("/next/", `/${version}/`))
        : nextContent;
      const actualContent = await readFile(releasePath);
      if (!actualContent.equals(expectedContent)) {
        failures.push(`${version}/${path}: differs from the release-time Next snapshot`);
      }
    }

    for (const path of actualPaths) {
      if (!expectedPaths.has(path)) failures.push(`${version}/${path}: unexpected release snapshot file`);
    }
  }

  const canonicalContract = await readFile(
    resolve(root, "deploy/contract/governance.openapi.yaml"),
    "utf8",
  );
  const contractVersion = canonicalContract.match(/^  version:\s*"([^"]+)"$/m)?.[1];
  if (contractVersion !== version) {
    failures.push(`canonical OpenAPI version is ${contractVersion ?? "missing"}, expected ${version}`);
  }

  try {
    const releasedContract = await readFile(resolve(docsRoot, `openapi/${version}.yaml`), "utf8");
    if (releasedContract !== canonicalContract) {
      failures.push(`openapi/${version}.yaml: differs from the canonical release contract`);
    }
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
    failures.push(`missing OpenAPI snapshot apps/docs/openapi/${version}.yaml`);
  }

  if (failures.length) {
    throw new Error(`Invalid documentation snapshot ${version}:\n${failures.join("\n")}`);
  }

  console.log(`Validated immutable documentation snapshot ${version}.`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await checkReleaseSnapshot(process.argv[2]);
}
