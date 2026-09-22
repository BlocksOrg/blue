import assert from "node:assert/strict";
import { test } from "node:test";

import type { HarnessMetadata } from "./harness-metadata.ts";
import {
  adapterSupportsHarness,
  describeMappingIssue,
  harnessMappingIssue,
  packageAdapterValidationError,
  type PackageAdapter,
} from "./mapping-compatibility.ts";

const openRules = {
  agents_require_plugin: false,
  hooks_require_plugin: false,
  hooks_as_plugin_modules: false,
};

/// Mirrors the certified generations Claude actually ships: `skills` only
/// exists from 2.0.12, which is why a mapping left at the 0.0.0 default fails.
const claude: HarnessMetadata = {
  key: "claude",
  aliases: [],
  label: "Claude",
  description: "",
  binary_names: ["claude"],
  install_command_template: "",
  gateway_model_exposure: "catalog",
  capabilities: [],
  component_rules: openRules,
  generations: [
    {
      profile: "claude-v0_0_0",
      introduced: "0.0.0",
      before: "1.0.38",
      verified_before: "1.0.38-0",
      lifecycle: "supported",
      capabilities: ["mcp", "packages", "helpers", "gateway"],
      component_rules: openRules,
    },
    {
      profile: "claude-v1_0_38",
      introduced: "1.0.38",
      before: "2.0.12",
      verified_before: "2.0.12-0",
      lifecycle: "supported",
      capabilities: ["mcp", "packages", "hooks", "helpers", "gateway"],
      component_rules: openRules,
    },
    {
      profile: "claude-v2_0_12",
      introduced: "2.0.12",
      before: "2.1.242",
      verified_before: "2.1.242-0",
      lifecycle: "supported",
      capabilities: ["mcp", "packages", "skills", "plugins", "hooks", "helpers", "gateway"],
      component_rules: {
        agents_require_plugin: true,
        hooks_require_plugin: true,
        hooks_as_plugin_modules: false,
      },
    },
    {
      profile: "claude-v2_1_242",
      introduced: "2.1.242",
      before: null,
      verified_before: "2.1.253-0",
      lifecycle: "supported",
      capabilities: ["mcp", "packages", "skills", "plugins", "hooks", "helpers", "gateway"],
      component_rules: {
        agents_require_plugin: true,
        hooks_require_plugin: true,
        hooks_as_plugin_modules: false,
      },
    },
  ],
};

/// Codex declares `skills` in every generation, so the same mapping is valid
/// at the default availability start.
const codex: HarnessMetadata = {
  ...claude,
  key: "codex",
  label: "Codex",
  generations: [
    {
      profile: "codex-v0_0_0",
      introduced: "0.0.0",
      before: null,
      verified_before: "0.146.0-0",
      lifecycle: "supported",
      capabilities: ["mcp", "packages", "skills", "plugins", "hooks", "helpers", "gateway"],
      component_rules: openRules,
    },
  ],
};

const skills: PackageAdapter = { skills_dir: "repo-commit/skills/brand-guidelines" };

test("names the version a capability starts at instead of a generic incompatibility", () => {
  const issue = harnessMappingIssue(skills, claude);
  assert.ok(issue, "a skills mapping from 0.0.0 must not be accepted for Claude");
  assert.equal(issue.kind, "capability");
  const message = describeMappingIssue(issue, skills, claude);
  assert.match(message, /does not support skills_dir before 2\.0\.12/);
  assert.match(message, /Set "Harness available from" to 2\.0\.12/);
  assert.match(message, /claude-v0_0_0 covers 0\.0\.0–1\.0\.38/);
});

test("accepts the mapping once availability starts at the supporting generation", () => {
  const scoped: PackageAdapter = { ...skills, introduced: "2.0.12" };
  assert.equal(harnessMappingIssue(scoped, claude), undefined);
  assert.equal(adapterSupportsHarness(scoped, claude), true);
});

test("keeps harnesses that support the capability everywhere unrestricted", () => {
  assert.equal(harnessMappingIssue(skills, codex), undefined);
  assert.equal(adapterSupportsHarness(skills, codex), true);
});

test("reports an availability range that overlaps no certified generation", () => {
  const stranded: PackageAdapter = { ...skills, introduced: "9.0.0" };
  const issue = harnessMappingIssue(stranded, claude);
  assert.ok(issue);
  assert.equal(issue.kind, "no-certified-overlap");
  assert.match(
    describeMappingIssue(issue, stranded, claude),
    /no certified generation inside the availability range 9\.0\.0–open ended/,
  );
});

test("explains a component rule that requires an accompanying plugin mapping", () => {
  const hooks: PackageAdapter = { hooks_file: "repo-commit/hooks.json", introduced: "2.0.12" };
  const issue = harnessMappingIssue(hooks, claude);
  assert.ok(issue);
  assert.equal(issue.kind, "hooks-require-plugin");
  assert.match(describeMappingIssue(issue, hooks, claude), /also needs a plugin_dir mapping/);
});

test("rejects an inverted availability range", () => {
  const inverted: PackageAdapter = { ...skills, introduced: "2.0.12", before: "1.0.0" };
  assert.equal(adapterSupportsHarness(inverted, claude), false);
});

test("validates an edited harness availability range", () => {
  const edited: PackageAdapter = {
    ...skills,
    introduced: "2.0.12",
    before: "2.1.0",
  };
  assert.equal(packageAdapterValidationError(edited, claude), undefined);
});

test("reports malformed and inverted edited availability ranges", () => {
  assert.match(
    packageAdapterValidationError({ ...skills, introduced: "not-semver" }, claude) ?? "",
    /invalid availability range/,
  );
  assert.match(
    packageAdapterValidationError(
      { ...skills, introduced: "2.0.12", before: "2.0.12" },
      claude,
    ) ?? "",
    /invalid availability range/,
  );
});

test("rejects a layout variant outside an edited availability range", () => {
  const edited: PackageAdapter = {
    ...skills,
    introduced: "2.0.12",
    variants: [{ introduced: "2.0.0", skills_dir: "legacy/skills" }],
  };
  assert.match(
    packageAdapterValidationError(edited, claude) ?? "",
    /layout interval outside its availability range/,
  );
});

test("skips capability compatibility for a disabled harness mapping", () => {
  assert.equal(packageAdapterValidationError(skills, claude, false), undefined);
});

test("still validates the range shape for a disabled harness mapping", () => {
  assert.match(
    packageAdapterValidationError(
      { ...skills, introduced: "2.0.12", before: "2.0.12" },
      claude,
      false,
    ) ?? "",
    /invalid availability range/,
  );
});
