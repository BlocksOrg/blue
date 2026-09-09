import { readdir, readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");
const openapiRoot = resolve(root, "apps/docs/openapi");
const source = await readFile(resolve(root, "deploy/contract/governance.openapi.yaml"));
const nextSnapshot = await readFile(resolve(openapiRoot, "next.yaml"));
const failures = [];

if (!source.equals(nextSnapshot)) {
  failures.push("The Next API snapshot is stale. Run `npm run sync:contract` in apps/docs.");
}

for (const filename of await readdir(openapiRoot)) {
  const match = filename.match(/^(\d+\.\d+\.\d+)\.yaml$/);
  if (!match) continue;
  const content = await readFile(resolve(openapiRoot, filename), "utf8");
  const declared = content.match(/^  version:\s*"([^"]+)"$/m)?.[1];
  if (declared !== match[1]) {
    failures.push(
      `${filename}: OpenAPI info.version is ${declared ?? "missing"}, expected ${match[1]}`,
    );
  }
}

if (failures.length) {
  console.error(failures.join("\n"));
  process.exit(1);
}
console.log("The Next API snapshot matches the canonical contract; released snapshot versions are valid.");
