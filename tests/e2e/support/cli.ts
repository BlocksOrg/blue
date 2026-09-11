import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";

export type CliResult = { code: number; stdout: string; stderr: string };

export function stateRoot(): string {
  return process.env.E2E_STATE_ROOT ?? "/tmp/blue-e2e";
}

/**
 * Wipe and recreate a client home so a spec never inherits state from an
 * earlier run. Playwright retries re-run `beforeAll` against the same fixed
 * path, and a surviving `session.json` makes `blue login` short-circuit with
 * "Already logged in" — failing the retry for a reason unrelated to the test.
 */
async function freshHome(name: string): Promise<string> {
  const root = stateRoot();
  const home = path.join(root, name);
  const relative = path.relative(root, home);
  if (!relative || relative.startsWith("..") || path.isAbsolute(relative)) {
    throw new Error(`refusing to reset a client home outside ${root}: ${home}`);
  }
  await rm(home, { recursive: true, force: true });
  await mkdir(path.join(home, ".config", "blue"), { recursive: true });
  await mkdir(path.join(home, ".cache"), { recursive: true });
  return home;
}

export async function prepareClient(name = "default"): Promise<string> {
  const home = await freshHome(name);
  const configDir = path.join(home, ".config", "blue");
  await writeFile(
    path.join(configDir, "blue.toml"),
    `[service]\nurl = "${process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080"}"\n\n[identity]\nmode = "oidc"\nissuer = "${process.env.E2E_DASHBOARD_URL ?? "http://127.0.0.1:3000"}/api/auth"\nclient_id = "blue-cli"\nscopes = ["openid", "profile", "email", "offline_access", "governance:read", "session:write", "client-status:write"]\n`,
  );
  return home;
}

export async function prepareEmptyClient(name: string): Promise<string> {
  return freshHome(name);
}

export function cliEnv(home: string, extra: NodeJS.ProcessEnv = {}): NodeJS.ProcessEnv {
  return {
    ...process.env,
    HOME: home,
    XDG_CONFIG_HOME: path.join(home, ".config"),
    XDG_CACHE_HOME: path.join(home, ".cache"),
    PATH: `/usr/local/bin:${process.env.PATH ?? ""}`,
    BROWSER: "/e2e/browser-is-deliberately-unavailable",
    NO_COLOR: "1",
    E2E_AGENT_LOG_DIR: path.join(home, "agent-log"),
    E2E_CODEX_VERSION: "0.145.0",
    E2E_CLAUDE_VERSION: "2.0.12",
    E2E_KIMI_VERSION: "0.0.0",
    E2E_OPENCODE_VERSION: "0.0.0",
    ...extra,
  };
}

export function spawnCli(
  home: string,
  args: string[],
  extra: NodeJS.ProcessEnv = {},
  cwd?: string,
): ChildProcessWithoutNullStreams {
  return spawn("blue", args, { cwd, env: cliEnv(home, extra), stdio: ["pipe", "pipe", "pipe"] });
}

export async function collect(child: ChildProcessWithoutNullStreams): Promise<CliResult> {
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk) => (stdout += chunk.toString()));
  child.stderr.on("data", (chunk) => (stderr += chunk.toString()));
  const code = await new Promise<number>((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", (value) => resolve(value ?? 1));
  });
  return { code, stdout, stderr };
}

export async function runCli(
  home: string,
  args: string[],
  extra: NodeJS.ProcessEnv = {},
  cwd?: string,
): Promise<CliResult> {
  return collect(spawnCli(home, args, extra, cwd));
}

export async function runCliWithInput(
  home: string,
  args: string[],
  input: string,
  extra: NodeJS.ProcessEnv = {},
): Promise<CliResult> {
  const child = spawnCli(home, args, extra);
  const result = collect(child);
  child.stdin.end(input);
  return result;
}

export async function runBareCliInPty(home: string, extra: NodeJS.ProcessEnv = {}): Promise<CliResult> {
  return collect(
    spawn("script", ["-qec", "blue", "/dev/null"], {
      env: cliEnv(home, { TERM: "xterm-256color", ...extra }),
      stdio: ["pipe", "pipe", "pipe"],
    }),
  );
}

export function spawnCliInPty(
  home: string,
  command: string,
  extra: NodeJS.ProcessEnv = {},
): ChildProcessWithoutNullStreams {
  return spawn("script", ["-qec", command, "/dev/null"], {
    env: cliEnv(home, { TERM: "xterm-256color", ...extra }),
    stdio: ["pipe", "pipe", "pipe"],
  });
}

export async function waitForOutput(
  child: ChildProcessWithoutNullStreams,
  pattern: RegExp,
  timeoutMs = 20_000,
): Promise<string> {
  let output = "";
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`timed out waiting for ${pattern}; output: ${output}`)), timeoutMs);
    const inspect = (chunk: Buffer) => {
      output += chunk.toString();
      const match = output.match(pattern);
      if (match) {
        clearTimeout(timer);
        resolve(match[0]);
      }
    };
    child.stdout.on("data", inspect);
    child.stderr.on("data", inspect);
    child.once("exit", (code) => {
      clearTimeout(timer);
      reject(new Error(`blue exited with ${code} before producing ${pattern}; output: ${output}`));
    });
  });
}

export async function readClientFile(home: string, relative: string): Promise<string> {
  return readFile(path.join(home, relative), "utf8");
}
