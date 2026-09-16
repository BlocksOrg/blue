import { createReadStream } from "node:fs";
import { createHash } from "node:crypto";
import { createServer } from "node:net";
import { mkdir, readFile, writeFile, rm } from "node:fs/promises";
import { join, resolve } from "node:path";
import spawn from "cross-spawn";
import * as tar from "tar";
import pg from "pg";
import { run, waitFor, stopTree } from "./process.mjs";
import { repo, sha256 } from "./prepare-fixtures.mjs";

const aws = async (args) =>
  JSON.parse(
    (await run("aws", [...args, "--output", "json"], { capture: true })) ||
      "{}",
  );
const required = (key) => {
  if (!process.env[key])
    throw new Error(`${key} is required for the dedicated E2E backend`);
  return process.env[key];
};
const shellQuote = (s) => `'${s.replaceAll("'", "'\\''")}'`;
export const portsFor = (suite) =>
  suite === "gateway" ? [8080, 5432, 9000, 8081, 4000] : [8080, 5432, 9000];
export async function assertFreePorts(ports) {
  const servers = [];
  try {
    for (const port of ports) {
      const server = createServer();
      await new Promise((resolve, reject) => {
        server.once("error", reject);
        server.listen(port, "127.0.0.1", resolve);
      });
      servers.push(server);
    }
  } finally {
    await Promise.all(
      servers.map((server) => new Promise((resolve) => server.close(resolve))),
    );
  }
}
export function sanitize(text, secrets = []) {
  for (const secret of secrets.filter(Boolean))
    text = text.replaceAll(secret, "[REDACTED]");
  return text
    .replace(/eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+/g, "[JWT]")
    .replace(/(Bearer\s+|sk-)[A-Za-z0-9._-]+/gi, "[TOKEN]")
    .replace(/X-Amz-[^\s"<>]+/gi, "[SIGNED-URL]");
}

async function hashFile(path) {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest("hex");
}
async function imageDigest(path) {
  let manifest = "";
  await tar.t({
    file: path,
    onReadEntry(entry) {
      if (entry.path === "manifest.json")
        entry.on("data", (chunk) => {
          manifest += chunk;
        });
    },
  });
  const image = JSON.parse(manifest).find((image) =>
    image.RepoTags?.includes("blue-e2e-base:local"),
  );
  if (!image) throw new Error("artifact has no blue-e2e-base:local image");
  const config = image.Config.split("/")
    .at(-1)
    .replace(/\.json$/, "");
  if (!/^[a-f0-9]{64}$/.test(config))
    throw new Error("unrecognized Docker config digest");
  return `sha256:${config}`;
}

export class Backend {
  constructor({
    mode,
    suite,
    directory,
    fixtures,
    image,
    execute = { run, aws },
  }) {
    Object.assign(this, {
      mode,
      suite,
      directory,
      fixtures,
      image,
      execute,
      tunnels: [],
      closing: false,
    });
    this.databaseUrl =
      mode === "local"
        ? process.env.E2E_SLIM_DATABASE_URL ||
          "postgres://harness:harness@127.0.0.1:5432/governance"
        : "postgres://harness:harness@127.0.0.1:5432/governance";
    const database = new URL(this.databaseUrl);
    if (database.hostname !== "127.0.0.1")
      throw new Error("native test database must be on client loopback");
    this.ports = portsFor(suite).map((port) =>
      port === 5432 ? Number(database.port || 5432) : port,
    );
    this.controller = new AbortController();
    this.manifestPath = join(directory, "lease.json");
  }
  async save() {
    await writeFile(this.manifestPath, JSON.stringify(this.lease, null, 2));
  }
  async ssm(commands, timeout = 1200) {
    const file = join(this.directory, "ssm-request.json");
    await writeFile(
      file,
      JSON.stringify({
        InstanceIds: [this.lease.instanceId],
        DocumentName: "AWS-RunShellScript",
        Parameters: { commands, executionTimeout: [String(timeout)] },
        TimeoutSeconds: timeout,
      }),
      { mode: 0o600 },
    );
    let sent;
    try {
      sent = await this.execute.aws([
        "ssm",
        "send-command",
        "--cli-input-json",
        `file://${file}`,
      ]);
    } finally {
      await rm(file, { force: true });
    }
    let invocation;
    await waitFor(
      "SSM command",
      async () => {
        if (!this.closing) this.controller.signal.throwIfAborted();
        try {
          invocation = await this.execute.aws([
            "ssm",
            "get-command-invocation",
            "--command-id",
            sent.Command.CommandId,
            "--instance-id",
            this.lease.instanceId,
          ]);
        } catch (error) {
          if (error.message.includes("InvocationDoesNotExist")) return false;
          throw error;
        }
        if (invocation.Status === "Success") return true;
        if (["Pending", "InProgress", "Delayed"].includes(invocation.Status))
          return false;
        throw new Error(
          `SSM command ${invocation.Status}: ${sanitize(invocation.StandardErrorContent, [process.env.OPENROUTER_API_KEY, process.env.LITELLM_MASTER_KEY])}`,
        );
      },
      timeout * 1000,
    );
    return invocation.StandardOutputContent;
  }
  composeArgs() {
    const args = [
      "compose",
      "-p",
      "blue-e2e-native",
      "-f",
      join(repo, "tests/e2e-slim/docker-compose.yml"),
    ];
    if (this.suite === "gateway")
      args.push("-f", join(repo, "tests/e2e-slim/docker-compose.gateway.yml"));
    args.push("-f", join(repo, "tests/e2e-slim/docker-compose.native.yml"));
    if (this.suite === "gateway")
      args.push(
        "-f",
        join(repo, "tests/e2e-slim/docker-compose.native-gateway.yml"),
      );
    return args;
  }
  localEnv() {
    return {
      ...process.env,
      E2E_NATIVE_POSTGRES_PORT: new URL(this.databaseUrl).port || "5432",
      E2E_NATIVE_FIXTURES: this.fixtures,
      HARNESS_PROVISIONER_EXECUTABLE_SHA256: this.hashes.provisionerSha256,
    };
  }
  async start(hashes) {
    this.controller.signal.throwIfAborted();
    await assertFreePorts(this.ports);
    this.hashes = hashes;
    if (this.mode === "local") {
      this.localStarted = true;
      if (process.platform !== "linux")
        throw new Error(
          "local backend requires Linux Docker; Windows uses --backend aws",
        );
      await this.execute.run(
        "docker",
        [
          ...this.composeArgs(),
          "up",
          "-d",
          "--wait",
          "--wait-timeout",
          "240",
          "--no-build",
        ],
        { env: this.localEnv() },
      );
    } else {
      const bucket = required("E2E_NATIVE_BUCKET"),
        subnet = required("E2E_NATIVE_SUBNET_ID"),
        securityGroup = required("E2E_NATIVE_SECURITY_GROUP_ID");
      const profile = required("E2E_NATIVE_INSTANCE_PROFILE"),
        ami = required("E2E_NATIVE_AMI_ID");
      if (!this.image)
        throw new Error(
          "E2E_NATIVE_IMAGE_TAR must point at images.yml blue-images.tar",
        );
      const runId = process.env.GITHUB_RUN_ID || `local-${Date.now()}`,
        attempt = process.env.GITHUB_RUN_ATTEMPT || "1";
      const prefix = `runs/${runId}/${attempt}/${process.platform}/${this.suite}`;
      this.lease = {
        schemaVersion: 1,
        runId,
        attempt,
        os: process.platform,
        suite: this.suite,
        sha: await this.execute.run("git", ["rev-parse", "HEAD"], {
          cwd: repo,
          capture: true,
        }),
        imageDigest: await imageDigest(this.image),
        imageArtifactSha256: await hashFile(this.image),
        bucket,
        s3Prefix: prefix,
        expiry: Math.floor(Date.now() / 1000) + 10800,
        portMappings: portsFor(this.suite).map((port) => ({
          local: port,
          remote: port,
        })),
      };
      await this.save();
      const bundle = join(this.directory, "source.tar.gz");
      const tracked = (
        await this.execute.run("git", ["ls-files", "-z"], {
          cwd: repo,
          capture: true,
        })
      )
        .split("\0")
        .filter(Boolean);
      await tar.c({ cwd: repo, file: bundle, gzip: true }, tracked);
      this.lease.sourceArchiveSha256 = await hashFile(bundle);
      await this.save();
      const fixtureBundle = join(this.directory, "fixtures.tar.gz");
      await tar.c({ cwd: this.fixtures, file: fixtureBundle, gzip: true }, [
        "governance-only-slim.yaml",
        "gateway-slim.yaml",
        "package-policy.json",
        "e2e-package.tar.gz",
      ]);
      const secretFile = join(this.directory, "backend.env");
      await writeFile(
        secretFile,
        `OPENROUTER_API_KEY=${shellQuote(process.env.OPENROUTER_API_KEY || "")}\nLITELLM_MASTER_KEY=${shellQuote(process.env.LITELLM_MASTER_KEY || "sk-e2e-slim-master")}\n`,
        { mode: 0o600 },
      );
      try {
        for (const [local, remote] of [
          [this.image, "images.tar"],
          [bundle, "source.tar.gz"],
          [fixtureBundle, "fixtures.tar.gz"],
          [secretFile, "backend.env"],
        ]) {
          await this.execute.run("aws", [
            "s3",
            "cp",
            local,
            `s3://${bucket}/${prefix}/${remote}`,
            "--only-show-errors",
          ]);
        }
      } finally {
        await rm(secretFile, { force: true });
      }
      const tags = [
        { Key: "BlueE2E", Value: "native" },
        { Key: "ExpiresAt", Value: String(this.lease.expiry) },
        { Key: "Run", Value: runId },
      ];
      const launched = await this.execute.aws([
        "ec2",
        "run-instances",
        "--image-id",
        ami,
        "--instance-type",
        process.env.E2E_NATIVE_INSTANCE_TYPE || "t3.xlarge",
        "--subnet-id",
        subnet,
        "--security-group-ids",
        securityGroup,
        "--iam-instance-profile",
        JSON.stringify({ Name: profile }),
        "--metadata-options",
        "HttpTokens=required,HttpEndpoint=enabled",
        "--tag-specifications",
        JSON.stringify([
          { ResourceType: "instance", Tags: tags },
          { ResourceType: "volume", Tags: tags },
        ]),
        "--client-token",
        `${runId}-${attempt}-${process.platform}-${this.suite}`,
        "--count",
        "1",
      ]);
      this.lease.instanceId = launched.Instances[0].InstanceId;
      await this.save();
      await waitFor(
        "backend SSM online",
        async () => {
          this.controller.signal.throwIfAborted();
          const info = await this.execute.aws([
            "ssm",
            "describe-instance-information",
            "--filters",
            JSON.stringify([
              { Key: "InstanceIds", Values: [this.lease.instanceId] },
            ]),
          ]);
          if (
            info.InstanceInformationList.some((i) => i.PingStatus === "Online")
          )
            return true;
          const state = await this.execute.aws([
            "ec2",
            "describe-instances",
            "--instance-ids",
            this.lease.instanceId,
          ]);
          const name = state.Reservations[0].Instances[0].State.Name;
          if (
            ["terminated", "shutting-down", "stopped", "stopping"].includes(
              name,
            )
          )
            throw new Error(`backend entered ${name}`);
          return false;
        },
        600_000,
      );
      const uri = `s3://${bucket}/${prefix}`;
      await this.ssm([
        "set -eu",
        "mkdir -p /opt/blue-e2e/fixtures /opt/blue-e2e/source",
        `aws s3 cp ${shellQuote(uri)} /opt/blue-e2e --recursive --only-show-errors`,
        "chmod 600 /opt/blue-e2e/backend.env",
        `echo '${this.lease.imageArtifactSha256}  /opt/blue-e2e/images.tar' | sha256sum -c -`,
        "docker load -i /opt/blue-e2e/images.tar",
        `test "$(docker image inspect --format '{{.Id}}' blue-e2e-base:local)" = '${this.lease.imageDigest}'`,
        `echo '${this.lease.sourceArchiveSha256}  /opt/blue-e2e/source.tar.gz' | sha256sum -c -`,
        "tar xzf /opt/blue-e2e/source.tar.gz -C /opt/blue-e2e/source",
        "tar xzf /opt/blue-e2e/fixtures.tar.gz -C /opt/blue-e2e/fixtures",
        `bash /opt/blue-e2e/source/tests/e2e-native/backend.sh up ${this.suite}`,
      ]);
      for (const port of portsFor(this.suite)) {
        const child = spawn(
          "aws",
          [
            "ssm",
            "start-session",
            "--target",
            this.lease.instanceId,
            "--document-name",
            "AWS-StartPortForwardingSession",
            "--parameters",
            JSON.stringify({
              portNumber: [String(port)],
              localPortNumber: [String(port)],
            }),
          ],
          {
            detached: process.platform !== "win32",
            stdio: ["ignore", "pipe", "pipe"],
          },
        );
        this.tunnels.push(child);
        child.stdout.on("data", () => {});
        child.stderr.on("data", () => {});
        child.on("error", (error) => {
          if (!this.closing) this.controller.abort(error);
        });
        child.on("exit", (code) => {
          if (!this.closing)
            this.controller.abort(
              new Error(`SSM tunnel ${port} exited (${code})`),
            );
        });
      }
    }
    await waitFor(
      "client tunnels and backend health",
      async () => {
        this.controller.signal.throwIfAborted();
        try {
          const response = await fetch("http://127.0.0.1:8080/health", {
            signal: AbortSignal.timeout(3000),
          });
          if (!response.ok) return false;
          const object = await fetch(
            "http://127.0.0.1:9000/package-artifacts/health.txt",
            { signal: AbortSignal.timeout(3000), redirect: "error" },
          );
          if (!object.ok) return false;
          if (
            sha256(Buffer.from(await object.arrayBuffer())) !==
            hashes.healthSha256
          )
            throw new Error(
              "health fixture digest mismatch through client tunnel",
            );
          const client = new pg.Client({
            connectionString: this.databaseUrl,
            connectionTimeoutMillis: 3000,
          });
          try {
            await client.connect();
            await client.query("SELECT 1");
          } finally {
            await client.end();
          }
          if (this.suite === "gateway") {
            for (const url of [
              "http://127.0.0.1:8081/health",
              "http://127.0.0.1:4000/health/liveliness",
            ])
              if (!(await fetch(url, { signal: AbortSignal.timeout(3000) })).ok)
                return false;
          }
          return true;
        } catch (error) {
          if (error.message.includes("digest mismatch")) throw error;
          return false;
        }
      },
      90_000,
    );
    // Public package fetches intentionally reject HTTP/private addresses.
    // Seed a managed artifact, then exercise the existing authenticated download
    // API and its original, loopback-advertised presigned MinIO URL.
    const database = new pg.Client({
      connectionString: this.databaseUrl,
      connectionTimeoutMillis: 5000,
    });
    try {
      await database.connect();
      const seeded = await database.query(
        `INSERT INTO package_artifacts
        (id, organization_id, connection_id, provider, repository, requested_ref, resolved_commit, source_ref, object_key, sha256, size_bytes)
        SELECT $1::uuid, id, 'e2e-native', 'fixture', 'e2e-package', 'fixture', $2, $3, 'e2e-package.tar.gz', $2, $4
        FROM organizations WHERE slug = 'e2e'`,
        [
          hashes.artifactId,
          hashes.packageSha256,
          "http://127.0.0.1:9000/package-artifacts/e2e-package.tar.gz",
          hashes.packageSize,
        ],
      );
      if (seeded.rowCount !== 1)
        throw new Error("expected exactly one fixture organization");
    } finally {
      await database.end();
    }
  }
  async cleanup() {
    this.closing = true;
    const errors = [];
    const attempt = async (fn) => {
      try {
        await fn();
      } catch (error) {
        errors.push(error.message);
      }
    };
    if (this.mode === "local" && this.localStarted) {
      await attempt(async () => {
        const logs = await this.execute.run(
          "docker",
          [...this.composeArgs(), "logs", "--no-color"],
          { env: this.localEnv(), capture: true },
        );
        await writeFile(
          join(this.directory, "backend.log"),
          sanitize(logs, [
            process.env.OPENROUTER_API_KEY,
            process.env.LITELLM_MASTER_KEY,
          ]),
        );
      });
      await attempt(() =>
        this.execute.run(
          "docker",
          [...this.composeArgs(), "down", "-v", "--remove-orphans"],
          { env: this.localEnv() },
        ),
      );
    }
    if (this.lease?.instanceId) {
      await attempt(async () => {
        const uri = `s3://${this.lease.bucket}/${this.lease.s3Prefix}/backend.log`;
        await this.ssm(
          [
            `bash /opt/blue-e2e/source/tests/e2e-native/backend.sh logs ${this.suite} > /opt/blue-e2e/backend.log`,
            `aws s3 cp /opt/blue-e2e/backend.log ${shellQuote(uri)} --only-show-errors`,
          ],
          60,
        );
        await this.execute.run("aws", [
          "s3",
          "cp",
          uri,
          join(this.directory, "backend.log"),
          "--only-show-errors",
        ]);
      });
      await attempt(() =>
        this.ssm(
          [
            `bash /opt/blue-e2e/source/tests/e2e-native/backend.sh down ${this.suite}`,
          ],
          120,
        ),
      );
      for (const child of this.tunnels) stopTree(child);
      await attempt(() =>
        this.execute.aws([
          "ec2",
          "terminate-instances",
          "--instance-ids",
          this.lease.instanceId,
        ]),
      );
    }
    if (this.lease)
      await attempt(() =>
        this.execute.run("aws", [
          "s3",
          "rm",
          `s3://${this.lease.bucket}/${this.lease.s3Prefix}/`,
          "--recursive",
          "--only-show-errors",
        ]),
      );
    if (errors.length) throw new Error(`cleanup errors: ${errors.join("; ")}`);
  }
}
