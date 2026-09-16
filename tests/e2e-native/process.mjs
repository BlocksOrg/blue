import spawn from "cross-spawn";

export function stopTree(child) {
  if (!child.pid) return;
  if (process.platform === "win32") {
    spawn.sync("taskkill", ["/PID", String(child.pid), "/T", "/F"], {
      stdio: "ignore",
    });
  } else {
    try {
      process.kill(-child.pid, "SIGTERM");
    } catch {
      child.kill();
    }
  }
}
export function run(
  command,
  args,
  {
    env = process.env,
    cwd,
    timeout = 600_000,
    capture = false,
    combine = false,
    signal,
  } = {},
) {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) {
      reject(signal.reason);
      return;
    }
    const child = spawn(command, args, {
      env,
      cwd,
      detached: process.platform !== "win32",
      stdio: capture ? ["ignore", "pipe", "pipe"] : "inherit",
    });
    let stdout = "",
      stderr = "",
      all = "";
    child.stdout?.on("data", (chunk) => {
      stdout += chunk;
      all += chunk;
    });
    child.stderr?.on("data", (chunk) => {
      stderr += chunk;
      all += chunk;
    });
    const cancel = () => {
      stopTree(child);
      reject(signal.reason);
    };
    signal?.addEventListener("abort", cancel, { once: true });
    const timer = setTimeout(() => {
      stopTree(child);
      reject(new Error(`${command} timed out after ${timeout}ms`));
    }, timeout);
    const clear = () => {
      clearTimeout(timer);
      signal?.removeEventListener("abort", cancel);
    };
    child.on("error", (error) => {
      clear();
      reject(error);
    });
    child.on("close", (code, sig) => {
      clear();
      if (code === 0) resolve((combine ? all : stdout).trim());
      else {
        const error = new Error(
          `${command} failed (${code ?? sig})${capture ? `: ${stderr}` : ""}`,
        );
        error.output = all;
        reject(error);
      }
    });
  });
}
export async function waitFor(
  label,
  probe,
  timeout = 300_000,
  interval = 3000,
) {
  const deadline = Date.now() + timeout;
  for (;;) {
    if (await probe()) return;
    if (Date.now() >= deadline) throw new Error(`TIMEOUT: ${label}`);
    console.log(`${new Date().toISOString()} waiting: ${label}`);
    await new Promise((resolve) =>
      setTimeout(resolve, Math.min(interval, deadline - Date.now())),
    );
  }
}
