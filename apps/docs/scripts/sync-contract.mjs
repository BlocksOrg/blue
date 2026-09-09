import { copyFile, mkdir } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");
const source = resolve(root, "deploy/contract/governance.openapi.yaml");
const nextTarget = resolve(root, "apps/docs/openapi/next.yaml");

await mkdir(dirname(nextTarget), { recursive: true });
await copyFile(source, nextTarget);
console.log("Synchronized the canonical contract to the Next API snapshot");
