import { mkdir, readFile, writeFile, copyFile, cp } from "node:fs/promises";
import { resolve, join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { createHash, randomUUID } from "node:crypto";
export const repo = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
export const sha256 = (bytes) =>
  createHash("sha256").update(bytes).digest("hex");

export async function prepareFixtures(
  directory,
  { legacy = false, platform = process.platform } = {},
) {
  await mkdir(directory, { recursive: true });
  await copyFile(
    join(repo, "tests/e2e-slim/fixtures/mcp-server.mjs"),
    join(directory, "mcp-server.mjs"),
  );
  const artifactId = randomUUID();
  let healthSha256;
  let archive = Buffer.from(
    await readFile(
      join(repo, "tests/e2e/fixtures/package/e2e-package.tar.gz.b64"),
      "utf8",
    ),
    "base64",
  );
  if (!legacy) {
    const { default: YAML } = await import("yaml");
    // The slim policies currently select skills only. Still port executable
    // components so future adapter selection cannot silently run Unix syntax.
    const source = join(directory, "source");
    await cp(join(repo, "tests/e2e/fixtures/package/source"), source, {
      recursive: true,
    });
    const root = join(source, "e2e-package");
    if (platform === "win32") {
      const marker = `node -e "require('fs').mkdirSync(process.env.E2E_SLIM_MARKER_DIR,{recursive:true});require('fs').writeFileSync(require('path').join(process.env.E2E_SLIM_MARKER_DIR,'kimi-package-hook'),'started')"`;
      await writeFile(
        join(root, "hooks/kimi.toml"),
        `[[hooks]]\nevent = "SessionStart"\ncommand = ${JSON.stringify(marker)}\ntimeout = 5\n`,
      );
    }
    const plugin = join(root, "opencode-plugin.js");
    await writeFile(
      plugin,
      (await readFile(plugin, "utf8"))
        .replaceAll(
          '"/tmp/blue-e2e/component-markers"',
          "process.env.E2E_SLIM_MARKER_DIR",
        )
        .replaceAll(
          '"/tmp/blue-e2e/component-markers/opencode-package-plugin"',
          "`${process.env.E2E_SLIM_MARKER_DIR}/opencode-package-plugin`",
        ),
    );
    const tar = await import("tar");
    await tar.c(
      {
        gzip: true,
        portable: true,
        cwd: source,
        file: join(directory, "e2e-package.tar.gz"),
      },
      ["e2e-package"],
    );
    archive = await readFile(join(directory, "e2e-package.tar.gz"));
    for (const suite of ["governance-only", "gateway"]) {
      const policy = YAML.parse(
        await readFile(
          join(repo, `tests/e2e-slim/fixtures/${suite}-slim.yaml`),
          "utf8",
        ),
      );
      for (const pkg of policy.governance.packages) {
        pkg.source_ref =
          "http://127.0.0.1:9000/package-artifacts/e2e-package.tar.gz";
        pkg.artifact_id = artifactId;
        pkg.sha256 = sha256(archive);
      }
      for (const harness of Object.values(policy.governance.harnesses)) {
        for (const mcp of harness.mcp)
          mcp.args = [join(directory, "mcp-server.mjs")];
      }
      await writeFile(
        join(directory, `${suite}-slim.yaml`),
        YAML.stringify(policy),
      );
    }
    // A fresh, inert object proves this run's storage path without downloading
    // an executable archive anonymously. Blue fetches the archive via signed URLs.
    const health = `blue-native-health:${randomUUID()}\n`;
    await writeFile(join(directory, "health.txt"), health);
    healthSha256 = sha256(health);
    const grant = {
      Version: "2012-10-17",
      Statement: [
        {
          Effect: "Allow",
          Principal: { AWS: ["*"] },
          Action: ["s3:GetObject"],
          Resource: ["arn:aws:s3:::package-artifacts/health.txt"],
        },
      ],
    };
    await writeFile(
      join(directory, "package-policy.json"),
      JSON.stringify(grant),
    );
  }
  await writeFile(join(directory, "e2e-package.tar.gz"), archive);
  return {
    artifactId,
    healthSha256,
    packageSize: archive.length,
    packageSha256: sha256(archive),
    provisionerSha256: sha256(
      await readFile(
        join(repo, "tests/e2e-slim/fixtures/litellm-provisioner.mjs"),
      ),
    ),
  };
}
if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  const directory = resolve(process.argv[2] || "/tmp/blue-e2e");
  const result = await prepareFixtures(directory, {
    legacy: process.argv.includes("--legacy"),
  });
  console.log(JSON.stringify(result));
}
