/// Pure compatibility rules for package capability mappings.
/// Extracted from the extensions dialog so the reasons a mapping cannot
/// apply to a harness are unit-testable without rendering the client tree.
import { compare, parse, type SemVer } from "semver";

import type { HarnessMetadata } from "@/lib/harness-metadata";

export type PackageAdapter = {
  introduced?: string;
  before?: string;
  plugin_dir?: string;
  skills_dir?: string;
  agents_dir?: string;
  hooks_file?: string;
  plugins?: string[];
  helpers?: Record<string, { paths: Record<string, string> }>;
  variants?: Array<{
    introduced: string;
    before?: string;
    plugin_dir?: string;
    skills_dir?: string;
    agents_dir?: string;
    hooks_file?: string;
    plugins?: string[];
    helpers?: Record<string, { paths: Record<string, string> }>;
  }>;
};

export function semverTuple(value: string): SemVer | undefined {
  return parse(value.trim(), { loose: false }) ?? undefined;
}
export function compareSemver(left: SemVer, right: SemVer) {
  return compare(left, right);
}

export type MappingIssue =
  | { kind: "no-certified-overlap" }
  | { kind: "capability"; field: string; capability: string; generation: GenerationMetadata }
  | { kind: "agents-require-plugin"; generation: GenerationMetadata }
  | { kind: "hooks-as-plugin-modules"; generation: GenerationMetadata }
  | { kind: "hooks-require-plugin"; generation: GenerationMetadata }
  | { kind: "variant-ambiguous"; version: string }
  | { kind: "variant-uncovered"; version: string };

export type GenerationMetadata = HarnessMetadata["generations"][number];

export function generationRange(generation: GenerationMetadata) {
  return `${generation.introduced}–${generation.before ?? generation.verified_before}`;
}

/// The earliest certified generation that declares `capability`, so the message
/// can name the exact version the operator should start the mapping at.
export function capabilitySupportedFrom(harness: HarnessMetadata, capability: string) {
  return harness.generations
    .filter((generation) => generation.capabilities.includes(capability))
    .map((generation) => generation.introduced)
    .sort((left, right) => {
      const [a, b] = [semverTuple(left), semverTuple(right)];
      return a && b ? compareSemver(a, b) : 0;
    })[0];
}

export function mappingIssue(
  mapping: PackageAdapter,
  generation: GenerationMetadata,
): MappingIssue | undefined {
  const supported = new Set(generation.capabilities);
  const rules = generation.component_rules;
  const missing = (field: string, capability: string): MappingIssue => ({
    kind: "capability",
    field,
    capability,
    generation,
  });
  if (mapping.skills_dir && !supported.has("skills")) return missing("skills_dir", "skills");
  if (mapping.plugin_dir && !supported.has("plugins")) return missing("plugin_dir", "plugins");
  if (mapping.plugins?.length && !supported.has("plugins")) return missing("plugins", "plugins");
  if (Object.keys(mapping.helpers ?? {}).length && !supported.has("helpers"))
    return missing("helpers", "helpers");
  if (mapping.agents_dir) {
    const supportedDirectly = supported.has("agents");
    const supportedByPlugin =
      rules.agents_require_plugin
      && Boolean(mapping.plugin_dir)
      && supported.has("plugins");
    if (!supportedDirectly && !supportedByPlugin) {
      if (rules.agents_require_plugin && supported.has("plugins") && !mapping.plugin_dir)
        return { kind: "agents-require-plugin", generation };
      return missing("agents_dir", "agents");
    }
  }
  if (mapping.hooks_file) {
    if (rules.hooks_as_plugin_modules) return { kind: "hooks-as-plugin-modules", generation };
    if (!supported.has("hooks")) return missing("hooks_file", "hooks");
    if (rules.hooks_require_plugin && !mapping.plugin_dir)
      return { kind: "hooks-require-plugin", generation };
  }
  return undefined;
}

export function inInterval(version: SemVer, introduced: string, before?: string | null) {
  const lower = semverTuple(introduced);
  const upper = before ? semverTuple(before) : undefined;
  return Boolean(
    lower
      && compareSemver(version, lower) >= 0
      && (!upper || compareSemver(version, upper) < 0),
  );
}

/// Why an adapter cannot apply to a harness, or `undefined` when it can. Each
/// branch names one concrete cause so the dialog can tell the operator which
/// field to change instead of reporting a single opaque incompatibility.
export function harnessMappingIssue(
  adapter: PackageAdapter,
  harness: HarnessMetadata,
): MappingIssue | undefined {
  const availableFrom = semverTuple(adapter.introduced ?? "0.0.0");
  const availableBefore = adapter.before ? semverTuple(adapter.before) : undefined;
  if (
    !availableFrom
    || (adapter.before && !availableBefore)
    || (availableBefore && compareSemver(availableFrom, availableBefore) >= 0)
  ) return { kind: "no-certified-overlap" };
  const variants = adapter.variants ?? [];
  const fallback = { ...adapter, variants: [] };
  const hasFallback = Boolean(
    adapter.plugin_dir
      || adapter.skills_dir
      || adapter.agents_dir
      || adapter.hooks_file
      || adapter.plugins?.length
      || Object.keys(adapter.helpers ?? {}).length,
  );

  let overlapsCertifiedGeneration = false;
  let issue: MappingIssue | undefined;
  harness.generations.forEach((generation) => {
    if (issue) return;
    const certifiedEnd = [generation.before, generation.verified_before]
      .filter((value): value is string => Boolean(value))
      .map(semverTuple)
      .filter((value): value is SemVer => Boolean(value))
      .sort(compareSemver)[0];
    const generationStart = semverTuple(generation.introduced);
    if (
      !generationStart
      || !certifiedEnd
      || compareSemver(availableFrom, certifiedEnd) >= 0
      || (availableBefore && compareSemver(generationStart, availableBefore) >= 0)
    ) return;
    overlapsCertifiedGeneration = true;
    const variantBoundaries = variants.flatMap((variant) =>
      [variant.introduced, variant.before].filter(
        (value): value is string => Boolean(value),
      ),
    );
    const candidates = [
      generation.introduced,
      adapter.introduced ?? "0.0.0",
      adapter.before,
      ...variantBoundaries,
    ].filter((value): value is string => Boolean(value));
    candidates
      .map(semverTuple)
      .filter((version): version is SemVer => Boolean(version))
      .filter((version) => inInterval(version, generation.introduced, certifiedEnd?.version))
      .filter((version) => inInterval(version, adapter.introduced ?? "0.0.0", adapter.before))
      .forEach((version) => {
        if (issue) return;
        const matching = variants.filter((variant) =>
          inInterval(version, variant.introduced, variant.before),
        );
        if (matching.length > 1) {
          issue = { kind: "variant-ambiguous", version: version.version };
          return;
        }
        if (matching.length === 0 && variants.length > 0 && !hasFallback) {
          issue = { kind: "variant-uncovered", version: version.version };
          return;
        }
        issue = mappingIssue(matching[0] ?? fallback, generation);
      });
  });
  if (issue) return issue;
  return overlapsCertifiedGeneration ? undefined : { kind: "no-certified-overlap" };
}

export function adapterSupportsHarness(
  adapter: PackageAdapter,
  harness: HarnessMetadata,
) {
  return !harnessMappingIssue(adapter, harness);
}

/// Validate the complete version shape of one package adapter. This is shared
/// by the add and edit flows so an existing custom extension cannot be left in
/// a state that the create dialog would reject.
export function packageAdapterValidationError(
  adapter: PackageAdapter,
  harness: HarnessMetadata,
  checkCompatibility = true,
): string | undefined {
  const label = harness.label;
  const availabilityStart = semverTuple(adapter.introduced ?? "0.0.0");
  const availabilityEnd = adapter.before ? semverTuple(adapter.before) : undefined;
  if (
    !availabilityStart
    || (adapter.before && !availabilityEnd)
    || (availabilityEnd && compareSemver(availabilityStart, availabilityEnd) >= 0)
  ) return `${label} has an invalid availability range.`;

  const intervals = (adapter.variants ?? []).map((variant) => ({
    variant,
    start: semverTuple(variant.introduced),
    end: variant.before ? semverTuple(variant.before) : undefined,
  }));
  if (
    intervals.some(({ variant, start, end }) =>
      !start || (variant.before && !end) || (end && compareSemver(start, end) >= 0),
    )
  ) return `${label} has an invalid adapter interval boundary.`;
  if (
    intervals.some((interval, index) =>
      index > 0 && compareSemver(intervals[index - 1].start!, interval.start!) >= 0,
    )
  ) return `${label} adapter intervals must be ordered by introduced version.`;

  for (let index = 1; index < intervals.length; index += 1) {
    const previous = intervals[index - 1];
    if (!previous.end || compareSemver(previous.end, intervals[index].start!) > 0)
      return `${label} adapter intervals overlap at ${intervals[index].variant.introduced}.`;
  }
  if (
    intervals.some(({ start, end }) =>
      compareSemver(start!, availabilityStart) < 0
      || (availabilityEnd && (!end || compareSemver(end, availabilityEnd) > 0)),
    )
  ) return `${label} has a layout interval outside its availability range.`;

  const hasFallback = Boolean(
    adapter.plugin_dir
      || adapter.skills_dir
      || adapter.agents_dir
      || adapter.hooks_file
      || adapter.plugins?.length
      || Object.keys(adapter.helpers ?? {}).length,
  );
  if (!hasFallback && intervals.length) {
    const last = intervals.at(-1)!;
    if (
      compareSemver(intervals[0].start!, availabilityStart) !== 0
      || (availabilityEnd
        ? !last.end || compareSemver(last.end, availabilityEnd) !== 0
        : Boolean(last.end))
    ) return `${label} layout intervals must cover its complete availability range when no fallback is defined.`;
    for (let index = 1; index < intervals.length; index += 1) {
      if (
        !intervals[index - 1].end
        || compareSemver(intervals[index - 1].end!, intervals[index].start!) !== 0
      ) return `${label} adapter intervals contain a gap before ${intervals[index].variant.introduced}.`;
    }
  }

  if (!checkCompatibility) return undefined;
  const issue = harnessMappingIssue(adapter, harness);
  return issue ? describeMappingIssue(issue, adapter, harness) : undefined;
}

export function describeMappingIssue(
  issue: MappingIssue,
  adapter: PackageAdapter,
  harness: HarnessMetadata,
) {
  const label = harness.label;
  switch (issue.kind) {
    case "capability": {
      const supportedFrom = capabilitySupportedFrom(harness, issue.capability);
      const covers = `certified generation ${issue.generation.profile} covers ${generationRange(issue.generation)}`;
      return supportedFrom
        ? `${label} does not support ${issue.field} before ${supportedFrom} (${covers}). Set "Harness available from" to ${supportedFrom}, or remove the ${label} mapping.`
        : `${label} does not support ${issue.field} in any certified generation. Remove the ${label} mapping.`;
    }
    case "agents-require-plugin":
      return `${label} exposes subagents through a plugin, so an agents_dir mapping also needs a plugin_dir mapping (${issue.generation.profile}).`;
    case "hooks-require-plugin":
      return `${label} exposes hooks through a plugin, so a hooks_file mapping also needs a plugin_dir mapping (${issue.generation.profile}).`;
    case "hooks-as-plugin-modules":
      return `${label} loads hooks as plugin modules, so map them with plugins instead of hooks_file (${issue.generation.profile}).`;
    case "variant-ambiguous":
      return `${label} has more than one layout interval covering ${issue.version}; make the layout ranges disjoint.`;
    case "variant-uncovered":
      return `${label} has no layout interval covering ${issue.version} and no default mapping; add a default mapping or widen a layout range.`;
    case "no-certified-overlap": {
      const range = `${adapter.introduced ?? "0.0.0"}–${adapter.before ?? "open ended"}`;
      return `${label} has no certified generation inside the availability range ${range}. Adjust "Harness available from"/"available before" to overlap a certified generation.`;
    }
  }
}
