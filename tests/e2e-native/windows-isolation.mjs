import { join } from "node:path";
import { writeFile } from "node:fs/promises";
import { repo } from "./prepare-fixtures.mjs";
import { run } from "./process.mjs";
import { sanitize } from "./backend.mjs";

// Keep the native account regression independent of backend allocation.
export async function verifyWindowsIsolation({
  directory,
  signal,
  execute = run,
}) {
  const name =
    "platform::windows_tests::sequential_native_profiles_remove_owned_state";
  let log = "";
  try {
    log = await execute(
      "cargo",
      [
        "test",
        "--locked",
        "--manifest-path",
        join(repo, "tests/e2e-slim/Cargo.toml"),
        "--lib",
        name,
        "--",
        "--exact",
        "--test-threads=1",
      ],
      {
        cwd: repo,
        env: { ...process.env, E2E_SLIM_REQUIRED: "1" },
        timeout: 1_800_000,
        signal,
        capture: true,
        combine: true,
      },
    );
    if (
      !log.includes(`test ${name} ... ok`) ||
      !/test result: ok\. 1 passed; 0 failed; 0 ignored;/.test(log)
    )
      throw new Error(
        "Windows isolation regression must execute exactly one passing test; see windows-isolation.log",
      );
  } catch (error) {
    log = error.output || log || error.message;
    throw error;
  } finally {
    await writeFile(
      join(directory, "windows-isolation.log"),
      sanitize(log, [
        process.env.OPENROUTER_API_KEY,
        process.env.LITELLM_MASTER_KEY,
      ]),
    );
  }
}
