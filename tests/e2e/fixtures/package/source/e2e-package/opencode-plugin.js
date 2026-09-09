import { tool } from "@opencode-ai/plugin"
import { mkdir, writeFile } from "node:fs/promises"

// Deterministic managed-package marker used by native OpenCode certification.
export const BlueCertificationPlugin = async () => {
  await mkdir("/tmp/blue-e2e/component-markers", { recursive: true })
  await writeFile("/tmp/blue-e2e/component-markers/opencode-package-plugin", "loaded\n")
  return {
    tool: {
      blue_package_certify: tool({
        description: "Return the managed OpenCode plugin certification marker",
        args: {},
        async execute() { return "BLUE_PLUGIN_OK" },
      }),
    },
  }
}
