import { mkdir, writeFile } from "node:fs/promises"

// OpenCode 2.x variant of opencode-plugin.js: the same deterministic marker
// and tool, written against the v2 plugin API.
export default {
  id: "blue.e2e-certification",
  async setup(ctx) {
    await mkdir("/tmp/blue-e2e/component-markers", { recursive: true })
    await writeFile("/tmp/blue-e2e/component-markers/opencode-package-plugin", "loaded\n")
    await ctx.tool.transform((editor) => {
      editor.add({
        name: "blue_package_certify",
        description: "Return the managed OpenCode plugin certification marker",
        input: { type: "object", properties: {}, additionalProperties: false },
        async execute() { return { content: "BLUE_PLUGIN_OK" } },
      })
    })
  },
}
