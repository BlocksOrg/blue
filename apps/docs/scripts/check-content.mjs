import { readdir, readFile } from "node:fs/promises";
import { dirname, extname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const docsRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const config = JSON.parse(await readFile(resolve(docsRoot, "docs.json"), "utf8"));
const failures = [];

async function filesBelow(directory) {
  const files = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    if (["node_modules", "scripts"].includes(entry.name)) continue;
    const path = resolve(directory, entry.name);
    if (entry.isDirectory()) files.push(...await filesBelow(path));
    else files.push(path);
  }
  return files;
}

const files = await filesBelow(docsRoot);
const mdxFiles = files.filter((path) => extname(path) === ".mdx");
const existingPages = new Set(mdxFiles.map((path) => path.slice(docsRoot.length + 1, -4)));
const navigatedPages = new Set(
  (config.navigation?.versions ?? []).flatMap((version) =>
    (version.groups ?? []).flatMap((group) =>
      (group.pages ?? []).filter((page) => typeof page === "string"),
    ),
  ),
);
const versionPrefixes = new Set(
  (config.navigation?.versions ?? []).map((entry) =>
    entry.version === "Next" ? "next" : entry.version,
  ),
);
const isVersionPrefix = (segment) => segment === "next" || /^\d+\.\d+\.\d+$/.test(segment);

for (const path of mdxFiles) {
  const relative = path.slice(docsRoot.length + 1);
  const content = await readFile(path, "utf8");
  const frontmatter = content.match(/^---\n([\s\S]*?)\n---\n/)?.[1];
  if (!frontmatter) failures.push(`${relative}: missing frontmatter`);
  if (!frontmatter?.match(/^title:\s*".+"$/m)) failures.push(`${relative}: missing quoted title`);
  if (!frontmatter?.match(/^description:\s*".+"$/m)) failures.push(`${relative}: missing quoted description`);
  if (navigatedPages.has(relative.slice(0, -4)) && !frontmatter?.match(/^icon:\s*".+"$/m)) {
    failures.push(`${relative}: missing quoted icon`);
  }

  let insideFence = false;
  for (const line of content.split("\n")) {
    if (!line.startsWith("```")) continue;
    if (!insideFence && !line.slice(3).trim()) failures.push(`${relative}: code fence has no language`);
    insideFence = !insideFence;
  }
  for (const match of content.matchAll(/\]\((\/[^)#?]+)(?:[?#][^)]*)?\)/g)) {
    const target = match[1].replace(/^\//, "");
    const sourceVersion = relative.split("/")[0];
    const targetVersion = target.split("/")[0];
    if (versionPrefixes.has(sourceVersion) && isVersionPrefix(targetVersion) && sourceVersion !== targetVersion) {
      failures.push(`${relative}: cross-version internal link ${match[1]}`);
    }
    if (!existingPages.has(target) && !files.some((file) => file === resolve(docsRoot, target))) {
      failures.push(`${relative}: unresolved internal link ${match[1]}`);
    }
  }
}

for (const version of config.navigation?.versions ?? []) {
  for (const group of version.groups ?? []) {
    for (const page of group.pages ?? []) {
      if (typeof page === "string" && !existingPages.has(page)) {
        failures.push(`docs.json: missing page ${page}`);
      }
    }
    if (group.openapi) {
      const source = typeof group.openapi === "string" ? group.openapi : group.openapi.source;
      if (typeof source !== "string") {
        failures.push(`docs.json: OpenAPI entry for ${group.group} is missing a source`);
        continue;
      }
      const spec = source.replace(/^\//, "");
      if (!files.some((file) => file === resolve(docsRoot, spec))) {
        failures.push(`docs.json: missing OpenAPI file ${source}`);
      }
    }
  }
}

const nextVersions = (config.navigation?.versions ?? []).filter(
  (entry) => entry.version === "Next",
);
if (nextVersions.length !== 1) {
  failures.push(`docs.json: expected exactly one Next version, found ${nextVersions.length}`);
} else {
  const nextPages = new Set(
    nextVersions[0].groups.flatMap((group) =>
      (group.pages ?? []).filter((page) => typeof page === "string"),
    ),
  );
  for (const page of existingPages) {
    if (page.startsWith("next/") && !nextPages.has(page)) {
      failures.push(`docs.json: Next navigation is missing page ${page}`);
    }
  }
  for (const page of nextPages) {
    if (!page.startsWith("next/")) {
      failures.push(`docs.json: Next navigation contains non-Next page ${page}`);
    }
  }
}

if (failures.length) {
  console.error(failures.join("\n"));
  process.exit(1);
}
console.log(`Validated ${mdxFiles.length} MDX pages, navigation, code fences, and internal links.`);
