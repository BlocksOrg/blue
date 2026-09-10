"use client";

import { useActionState, useEffect, useMemo, useState } from "react";
import { Ellipsis, PackageOpen, Plus, Search, ShieldAlert } from "lucide-react";
import {
  inspectPackageSource,
  saveExtensions,
  searchAudienceUsers,
  type AudienceUserOption,
  type ConfigState,
  type InspectPackageState,
} from "../../actions";
import {
  expandMcpDefinitions,
  groupMcpServers,
  McpEditorDialog,
  type McpByHarness,
  type McpDefinition,
} from "./mcp-manager";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { FloatingSaveBar } from "@/components/floating-save-bar";
import {
  ConfirmationDialog,
  type ConfirmationAction,
} from "@/components/confirmation-dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useUnsavedChangesWarning } from "@/hooks/use-unsaved-changes-warning";
import type { HarnessMetadata } from "@/lib/harness-metadata";
import {
  adapterSupportsHarness,
  compareSemver,
  describeMappingIssue,
  harnessMappingIssue,
  semverTuple,
  type PackageAdapter,
} from "@/lib/mapping-compatibility";

export type { PackageAdapter };
import { TablePaginationFooter } from "@/components/table-pagination-footer";
import { Skeleton } from "@/components/ui/skeleton";


export type ManagedPackage = {
  id: string;
  name?: string;
  version: string;
  source_ref: string;
  artifact_id?: string;
  sha256: string;
  platform_sources?: Record<string, { source_ref: string; artifact_id?: string; sha256: string }>;
  settings?: Record<string, unknown>;
  adapters?: Record<string, PackageAdapter>;
};

export type PackageAudience = {
  scope: "organization" | "users";
  user_ids: string[];
};

export type PackageSourceConnection = {
  id: string;
  name: string;
  provider: "github" | "bitbucket_cloud" | "bitbucket_data_center";
  namespaces: string[];
};

type PackageOverride = { enabled?: boolean; settings?: Record<string, unknown> };
type Overrides = Record<string, Record<string, PackageOverride>>;
type Capability = "skills" | "hooks" | "subagents" | "plugins" | "helpers";
type ExtensionTab = "all" | Capability | "mcp";
type InventoryRow = {
  packageId: string;
  harness: string;
  kind: Capability;
  label: string;
  sourcePath: string;
  variantIndex?: number;
};
type Draft = Pick<ManagedPackage, "id" | "name" | "version" | "source_ref" | "artifact_id" | "sha256"> & {
  adapters: Record<string, PackageAdapter>;
};

const tabs: { value: ExtensionTab; label: string }[] = [
  { value: "all", label: "All" },
  { value: "skills", label: "Skills" },
  { value: "hooks", label: "Hooks" },
  { value: "subagents", label: "Subagents" },
  { value: "plugins", label: "Plugins" },
  { value: "helpers", label: "Helpers" },
  { value: "mcp", label: "MCP servers" },
];
const componentKinds = [
  ["skills_dir", "Skill"],
  ["agents_dir", "Subagent"],
  ["hooks_file", "Hook"],
  ["plugin_dir", "Plugin"],
  ["plugin", "Plugin module"],
  ["helper", "Helper"],
] as const;
type ComponentKind = (typeof componentKinds)[number][0];
const emptyDraft: Draft = { id: "", name: "", version: "", source_ref: "", artifact_id: undefined, sha256: "", adapters: {} };


function inventory(item: ManagedPackage): InventoryRow[] {
  const rows: InventoryRow[] = [];
  Object.entries(item.adapters ?? {}).forEach(([harness, adapter]) => {
    const add = (kind: Capability, label: string, sourcePath?: string, variantIndex?: number) => {
      if (sourcePath) rows.push({ packageId: item.id, harness, kind, label, sourcePath, variantIndex });
    };
    add("skills", "Skills", adapter.skills_dir);
    add("subagents", "Subagents", adapter.agents_dir);
    add("hooks", "Hooks", adapter.hooks_file);
    add("plugins", "Plugin", adapter.plugin_dir);
    (adapter.plugins ?? []).forEach((path) => add("plugins", "Plugin", path));
    Object.entries(adapter.helpers ?? {}).forEach(([name, asset]) =>
      Object.entries(asset.paths).forEach(([platform, path]) =>
        add("helpers", `Helper: ${name} (${platform})`, path),
      ),
    );
    (adapter.variants ?? []).forEach((variant, variantIndex) => {
      const prefix = `${variant.introduced} ≤ version${variant.before ? ` < ${variant.before}` : ""}: `;
      add("skills", `${prefix}Skills`, variant.skills_dir, variantIndex);
      add("subagents", `${prefix}Subagents`, variant.agents_dir, variantIndex);
      add("hooks", `${prefix}Hooks`, variant.hooks_file, variantIndex);
      add("plugins", `${prefix}Plugin`, variant.plugin_dir, variantIndex);
      (variant.plugins ?? []).forEach((path) =>
        add("plugins", `${prefix}Plugin`, path, variantIndex),
      );
      Object.entries(variant.helpers ?? {}).forEach(([name, asset]) =>
        Object.entries(asset.paths).forEach(([platform, path]) =>
          add("helpers", `${prefix}Helper: ${name} (${platform})`, path, variantIndex),
        ),
      );
    });
  });
  return rows;
}

function capabilities(item: ManagedPackage) {
  return [...new Set(inventory(item).map((row) => row.kind))];
}
function capabilityLabel(kind: Capability) {
  return tabs.find((tab) => tab.value === kind)?.label ?? kind;
}
function installLocation(row: InventoryRow) {
  return `$XDG_CONFIG_HOME/blue/packages/${row.packageId}/<sha256>/content/${row.sourcePath}`;
}


function DetailSheet({
  item,
  custom,
  overrideText,
  audience,
  harnesses,
  dirty,
  publishing,
  publishDisabled,
  onClose,
  onAudienceChange,
  onAdaptersChange,
  onOverrideChange,
  onSettingsChange,
}: {
  item?: ManagedPackage;
  custom: boolean;
  overrideText: string;
  audience: PackageAudience;
  harnesses: HarnessMetadata[];
  dirty: boolean;
  publishing: boolean;
  publishDisabled: boolean;
  onClose: () => void;
  onAudienceChange: (audience: PackageAudience) => void;
  onAdaptersChange: (adapters: Record<string, PackageAdapter>) => void;
  onOverrideChange: (harness: string, packageId: string, value: string) => void;
  onSettingsChange: (harness: string, packageId: string, settings: Record<string, unknown>) => void;
}) {
  const overrides = useMemo(() => JSON.parse(overrideText) as Overrides, [overrideText]);
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [memberQuery, setMemberQuery] = useState("");
  const [memberResults, setMemberResults] = useState<AudienceUserOption[]>([]);
  const [memberSearchError, setMemberSearchError] = useState("");
  const [memberSearchPending, setMemberSearchPending] = useState(false);
  const [memberSearchAttempt, setMemberSearchAttempt] = useState(0);

  useEffect(() => {
    if (!item) return;
    setDrafts(Object.fromEntries(Object.keys(item.adapters ?? {}).map((harness) => [
      harness,
      JSON.stringify(overrides[harness]?.[item.id]?.settings ?? {}, null, 2),
    ])));
    setErrors({});
    setMemberQuery("");
    setMemberResults([]);
    setMemberSearchError("");
  }, [item, overrideText]);

  const selectedUserSignature = audience.user_ids.join(",");
  useEffect(() => {
    if (!item || audience.scope !== "users") return;
    let current = true;
    setMemberSearchPending(true);
    setMemberSearchError("");
    const timeout = window.setTimeout(async () => {
      const result = await searchAudienceUsers(memberQuery, audience.user_ids);
      if (!current) return;
      setMemberResults(result.users.slice(0, 10));
      setMemberSearchError(result.error ?? "");
      setMemberSearchPending(false);
    }, memberQuery.trim() ? 250 : 0);
    return () => {
      current = false;
      window.clearTimeout(timeout);
    };
  }, [item, audience.scope, selectedUserSignature, memberQuery, memberSearchAttempt]);

  if (!item) return null;
  const rows = inventory(item);
  const kinds = capabilities(item);
  const executable = kinds.some((kind) => ["hooks", "plugins", "helpers"].includes(kind));
  const selectedUsers = new Set(audience.user_ids);
  const visibleUsers = memberResults.slice(0, 10);

  function toggleUser(userId: string, checked: boolean) {
    const next = new Set(audience.user_ids);
    checked ? next.add(userId) : next.delete(userId);
    onAudienceChange({ scope: "users", user_ids: [...next].sort() });
  }

  function toggleHarness(harness: HarnessMetadata, checked: boolean) {
    const adapters = structuredClone(item!.adapters ?? {});
    if (!checked) {
      delete adapters[harness.key];
      onAdaptersChange(adapters);
      return;
    }
    const template = Object.values(adapters).find((adapter) =>
      adapterSupportsHarness(adapter, harness),
    );
    if (template) {
      adapters[harness.key] = structuredClone(template);
      onAdaptersChange(adapters);
    }
  }

  function updateSettings(harness: string, value: string) {
    setDrafts((current) => ({ ...current, [harness]: value }));
    try {
      const parsed = JSON.parse(value);
      if (!parsed || Array.isArray(parsed) || typeof parsed !== "object") throw new Error("Settings must be a JSON object.");
      setErrors((current) => ({ ...current, [harness]: "" }));
      onSettingsChange(harness, item!.id, parsed);
    } catch (error) {
      setErrors((current) => ({
        ...current,
        [harness]: error instanceof Error ? error.message : "Invalid settings JSON",
      }));
    }
  }

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="max-h-[calc(100svh-2rem)] grid-rows-[auto_minmax(0,1fr)_auto] overflow-hidden sm:max-w-2xl">
        <DialogHeader>
          <div className="flex flex-wrap items-center gap-2 pr-8">
            <DialogTitle className="pr-0 text-lg">{item.name ?? item.id}</DialogTitle>
            <Badge variant="secondary" className="shrink-0">{custom ? "Custom" : "Curated"}</Badge>
          </div>
          <DialogDescription>Version {item.version} · Package {item.id}</DialogDescription>
        </DialogHeader>
        <div className="grid gap-6 overflow-y-auto pr-1">
          <section className="grid gap-3">
            <div className="min-w-0"><h3 className="font-medium">Included capabilities</h3><p className="text-sm break-words text-muted-foreground">Availability applies to this complete package bundle.</p></div>
            <div className="flex flex-wrap gap-2">
              {kinds.map((kind) => <Badge key={kind} variant="secondary">{capabilityLabel(kind)}</Badge>)}
              {Object.entries(item.adapters ?? {}).map(([harness, adapter]) => (
                <Badge key={harness} variant="outline" className="capitalize">
                  {harness}
                  {adapter.introduced ? ` ≥${adapter.introduced}` : ""}
                  {adapter.before ? ` <${adapter.before}` : ""}
                </Badge>
              ))}
            </div>
            {executable && <Alert><ShieldAlert /><AlertTitle>Includes executable content</AlertTitle><AlertDescription>Review hooks, plugins, and helper binaries before publishing this package.</AlertDescription></Alert>}
          </section>

          {custom && <section className="grid gap-3">
            <div>
              <h3 className="font-medium">Agents</h3>
              <p className="text-sm text-muted-foreground">Select every coding agent that should receive this extension. New agents reuse the verified capability paths above.</p>
            </div>
            <div className="grid gap-3 sm:grid-cols-2" role="group" aria-label="Agents">
              {harnesses.map((harness) => {
                const selected = Boolean(item.adapters?.[harness.key]);
                const compatible = selected || Object.values(item.adapters ?? {}).some((adapter) =>
                  adapterSupportsHarness(adapter, harness),
                );
                const lastSelected = selected && Object.keys(item.adapters ?? {}).length === 1;
                const description = !compatible
                  ? "Current mappings are not compatible with this agent."
                  : lastSelected
                    ? "At least one agent is required."
                    : harness.description;
                return (
                  <label className="flex min-h-14 min-w-0 items-start gap-3 rounded-lg border p-3" key={harness.key}>
                    <Checkbox
                      checked={selected}
                      disabled={!compatible || lastSelected}
                      onCheckedChange={(checked) => toggleHarness(harness, checked === true)}
                    />
                    <span className="min-w-0">
                      <span className="block truncate text-sm font-medium" title={harness.label}>{harness.label}</span>
                      <span className="block text-xs break-words text-muted-foreground">{description}</span>
                    </span>
                  </label>
                );
              })}
            </div>
          </section>}

          <section className="grid gap-3">
            <div>
              <h3 className="font-medium">Deployment audience</h3>
              <p className="text-sm text-muted-foreground">Choose who receives and loads this complete package bundle.</p>
            </div>
            <RadioGroup
              aria-label="Deployment audience"
              value={audience.scope}
              onValueChange={(scope) =>
                onAudienceChange(
                  scope === "organization"
                    ? { scope: "organization", user_ids: [] }
                    : { scope: "users", user_ids: audience.user_ids },
                )
              }
            >
              <label className="flex min-w-0 cursor-pointer items-start gap-3 rounded-lg border p-3">
                <RadioGroupItem value="organization" />
                <span className="min-w-0"><span className="block text-sm font-medium">Everyone</span><span className="block text-xs break-words text-muted-foreground">Every active user in this organization.</span></span>
              </label>
              <label className="flex min-w-0 cursor-pointer items-start gap-3 rounded-lg border p-3">
                <RadioGroupItem value="users" />
                <span className="min-w-0"><span className="block text-sm font-medium">Specific members</span><span className="block text-xs break-words text-muted-foreground">Only selected administrators and members.</span></span>
              </label>
            </RadioGroup>
            {audience.scope === "users" && <div className="grid gap-2 rounded-lg border p-3">
              <Label htmlFor={`${item.id}-member-search`}>Members</Label>
              <div className="relative">
                <Search className="pointer-events-none absolute left-2.5 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
                <Input id={`${item.id}-member-search`} value={memberQuery} onChange={(event) => setMemberQuery(event.target.value)} placeholder="Search by email" className="pl-8" />
              </div>
              <div className="max-h-64 overflow-y-auto rounded-md border" role="group" aria-label="Select members" aria-busy={memberSearchPending} aria-live="polite">
                {memberSearchPending ? Array.from({ length: 3 }, (_, index) => (
                  <div className="flex min-h-11 items-center gap-3 border-b px-3 py-2 last:border-b-0" key={index} aria-hidden="true">
                    <Skeleton className="size-4" />
                    <div className="grid flex-1 gap-1.5"><Skeleton className="h-4 w-2/3" /><Skeleton className="h-3 w-20" /></div>
                  </div>
                )) : memberSearchError ? <div className="grid justify-items-center gap-2 p-4 text-center">
                  <p className="text-sm text-destructive">Members could not be loaded. Try again.</p>
                  <Button type="button" variant="outline" size="sm" onClick={() => setMemberSearchAttempt((attempt) => attempt + 1)}>Retry</Button>
                </div> : visibleUsers.length ? visibleUsers.map((user) => {
                  const selected = selectedUsers.has(user.id);
                  const inactive = user.status !== "active";
                  return <div className="flex min-h-11 items-center gap-3 border-b px-3 py-2 last:border-b-0" key={user.id}>
                    <Checkbox id={`${item.id}-${user.id}`} checked={selected} disabled={inactive} onCheckedChange={(checked) => toggleUser(user.id, checked === true)} />
                    <Label htmlFor={`${item.id}-${user.id}`} className="min-w-0 flex-1 font-normal">
                      <span className="block break-all text-sm">{user.email}</span>
                      <span className="block text-xs capitalize text-muted-foreground">{user.role}{inactive ? ` · ${user.status}` : ""}</span>
                    </Label>
                    {inactive && selected && <Button type="button" variant="ghost" size="sm" className="shrink-0" onClick={() => toggleUser(user.id, false)}>Remove</Button>}
                  </div>;
                }) : <p className="p-4 text-center text-sm text-muted-foreground">{memberQuery.trim() ? "No members match this search." : "No eligible members found."}</p>}
              </div>
              {!audience.user_ids.length && <p className="text-xs text-destructive">Select at least one member before publishing.</p>}
            </div>}
          </section>

          <section className="grid gap-3">
            <div><h3 className="font-medium">Agent availability</h3><p className="text-sm text-muted-foreground">Extensions default to enabled for every selected agent.</p></div>
            {Object.keys(item.adapters ?? {}).map((harness) => {
              const enabled = overrides[harness]?.[item.id]?.enabled;
              const value = enabled === undefined ? "inherit" : enabled ? "enabled" : "disabled";
              return (
                <div className="grid gap-3 rounded-lg border p-3" key={harness}>
                  <div className="grid items-end gap-3 sm:grid-cols-[1fr_15rem]">
                    <div><Label htmlFor={`${item.id}-${harness}`} className="capitalize">{harness}</Label><p className="text-xs text-muted-foreground">{rows.filter((row) => row.harness === harness).map((row) => row.label).join(", ")}</p></div>
                    <Select value={value} onValueChange={(next) => next && onOverrideChange(harness, item.id, next)}>
                      <SelectTrigger id={`${item.id}-${harness}`} className="w-full"><SelectValue /></SelectTrigger>
                      <SelectContent><SelectItem value="inherit">Organization default</SelectItem><SelectItem value="enabled">Enabled</SelectItem><SelectItem value="disabled">Disabled</SelectItem></SelectContent>
                    </Select>
                  </div>
                  <details>
                    <summary className="cursor-pointer text-sm font-medium">Advanced settings</summary>
                    <Textarea className="mt-3 min-h-28 font-mono text-xs" aria-label={`${harness} settings JSON`} value={drafts[harness] ?? "{}"} onChange={(event) => updateSettings(harness, event.target.value)} spellCheck={false} />
                    {errors[harness] && <p className="mt-2 text-xs text-destructive">{errors[harness]}</p>}
                  </details>
                </div>
              );
            })}
          </section>

          <section className="grid gap-3">
            <div><h3 className="font-medium">Verified source</h3><p className="text-sm text-muted-foreground">Immutable metadata used by managed clients.</p></div>
            <dl className="grid gap-3 rounded-lg border p-3 text-sm">
              <div><dt className="text-muted-foreground">Source</dt><dd className="break-all font-mono text-xs">{item.source_ref}</dd></div>
              <div><dt className="text-muted-foreground">SHA-256</dt><dd className="break-all font-mono text-xs">{item.sha256}</dd></div>
            </dl>
            <details>
              <summary className="cursor-pointer text-sm font-medium">Archive paths and install locations</summary>
              <div className="mt-3 overflow-x-auto rounded-lg border">
                <Table><TableHeader><TableRow><TableHead>Harness</TableHead><TableHead>Capability</TableHead><TableHead>Archive path</TableHead><TableHead>Managed location</TableHead></TableRow></TableHeader>
                  <TableBody>{rows.map((row, index) => <TableRow key={`${row.harness}:${row.sourcePath}:${index}`}><TableCell className="capitalize">{row.harness}</TableCell><TableCell>{row.label}</TableCell><TableCell className="font-mono text-xs">{row.sourcePath}</TableCell><TableCell className="max-w-72 break-all font-mono text-xs text-muted-foreground">{installLocation(row)}</TableCell></TableRow>)}</TableBody>
                </Table>
              </div>
            </details>
          </section>
        </div>
        <DialogFooter>
          <Button className="sm:ml-auto" type="submit" form="extensions-form" disabled={!dirty || publishing || publishDisabled}>{publishing ? "Publishing extension changes…" : "Publish extension changes"}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function AddDialog({
  open,
  defaultKind,
  catalogIds,
  customPackages,
  inspection,
  inspecting,
  connections,
  harnesses,
  onOpenChange,
  onAdd,
}: {
  open: boolean;
  defaultKind: ExtensionTab;
  catalogIds: Set<string>;
  customPackages: ManagedPackage[];
  inspection: InspectPackageState;
  inspecting: boolean;
  connections: PackageSourceConnection[];
  harnesses: HarnessMetadata[];
  onOpenChange: (open: boolean) => void;
  onAdd: (item: ManagedPackage) => void;
}) {
  const [draft, setDraft] = useState<Draft>(emptyDraft);
  const [error, setError] = useState("");
  const [sourceMode, setSourceMode] = useState<"repository" | "archive">(connections.length ? "repository" : "archive");
  const [connectionId, setConnectionId] = useState(connections[0]?.id ?? "");
  const [repository, setRepository] = useState("");
  const [requestedRef, setRequestedRef] = useState("");
  const [componentHarness, setComponentHarness] = useState(harnesses[0]?.key ?? "");
  const [componentKind, setComponentKind] = useState<ComponentKind>("skills_dir");
  const [componentPath, setComponentPath] = useState("");
  const [helperName, setHelperName] = useState("");
  const [helperPlatform, setHelperPlatform] = useState("default");
  const [introduced, setIntroduced] = useState("");
  const [before, setBefore] = useState("");
  const rows = inventory({ ...draft, id: draft.id || "new-extension" });

  useEffect(() => {
    const defaults: Partial<Record<ExtensionTab, ComponentKind>> = { skills: "skills_dir", hooks: "hooks_file", subagents: "agents_dir", plugins: "plugin_dir", helpers: "helper" };
    setComponentKind(defaults[defaultKind] ?? "skills_dir");
  }, [defaultKind, open]);
  useEffect(() => {
    if (open && inspection.source_ref && inspection.sha256)
      setDraft((current) => ({
        ...current,
        source_ref: inspection.source_ref ?? "",
        artifact_id: inspection.artifact_id,
        sha256: inspection.sha256 ?? "",
      }));
  }, [inspection.source_ref, inspection.artifact_id, inspection.sha256, open]);

  function close() {
    setDraft(emptyDraft); setError(""); setRepository(""); setRequestedRef(""); setComponentPath(""); setHelperName(""); setHelperPlatform("default"); onOpenChange(false);
  }
  function addMapping() {
    const path = componentPath.trim();
    if (!path) return setError("Enter the path inside the package archive.");
    if (componentKind === "helper" && !helperName.trim()) return setError("Enter the helper command name.");
    const adapter: PackageAdapter = structuredClone(draft.adapters[componentHarness] ?? {});
    let target: PackageAdapter = adapter;
    if (introduced.trim()) {
      adapter.variants ??= [];
      let variant = adapter.variants.find((item) => item.introduced === introduced.trim() && (item.before ?? "") === before.trim());
      if (!variant) {
        variant = { introduced: introduced.trim(), before: before.trim() || undefined };
        adapter.variants.push(variant);
      }
      target = variant;
    }
    if (componentKind === "plugin") target.plugins = [...(target.plugins ?? []), path];
    else if (componentKind === "helper") {
      target.helpers = { ...(target.helpers ?? {}) };
      const name = helperName.trim();
      target.helpers[name] = { paths: { ...(target.helpers[name]?.paths ?? {}), [helperPlatform.trim() || "default"]: path } };
    } else target[componentKind] = path;
    setDraft((current) => ({ ...current, adapters: { ...current.adapters, [componentHarness]: adapter } }));
    setComponentPath(""); setError("");
  }
  function updateAvailability(field: "introduced" | "before", value: string) {
    const adapter = structuredClone(draft.adapters[componentHarness] ?? {});
    if (value.trim()) adapter[field] = value.trim();
    else delete adapter[field];
    setDraft((current) => ({
      ...current,
      adapters: { ...current.adapters, [componentHarness]: adapter },
    }));
  }
  function removeMapping(row: InventoryRow) {
    const adapters = structuredClone(draft.adapters);
    const adapter = adapters[row.harness];
    if (!adapter) return;
    const target = row.variantIndex === undefined ? adapter : adapter.variants?.[row.variantIndex];
    if (!target) return;
    if (row.kind === "skills") delete target.skills_dir;
    else if (row.kind === "subagents") delete target.agents_dir;
    else if (row.kind === "hooks") delete target.hooks_file;
    else if (row.kind === "plugins" && target.plugin_dir === row.sourcePath) delete target.plugin_dir;
    else if (row.kind === "plugins") target.plugins = (target.plugins ?? []).filter((path) => path !== row.sourcePath);
    else Object.entries(target.helpers ?? {}).forEach(([name, asset]) => {
      Object.entries(asset.paths).forEach(([platform, path]) => { if (path === row.sourcePath) delete target.helpers?.[name]?.paths[platform]; });
      if (!Object.keys(target.helpers?.[name]?.paths ?? {}).length) delete target.helpers?.[name];
    });
    if (!inventory({ ...draft, adapters: { [row.harness]: adapter } }).length) delete adapters[row.harness];
    setDraft((current) => ({ ...current, adapters }));
  }
  function addExtension() {
    const id = draft.id.trim().toLowerCase();
    if (!/^[a-z0-9][a-z0-9-]*$/.test(id)) return setError("Extension ID must use lowercase letters, numbers, and hyphens.");
    if (!draft.version.trim() || !draft.source_ref.trim()) return setError("Version and immutable source are required.");
    if (!/^[a-f0-9]{64}$/.test(draft.sha256.trim())) return setError("SHA-256 must be exactly 64 lowercase hexadecimal characters.");
    if (!rows.length) return setError("Add at least one capability mapping.");
    for (const [harness, adapter] of Object.entries(draft.adapters)) {
      const metadata = harnesses.find((item) => item.key === harness);
      if (!metadata) return setError(`${harness} is not supported by the current client registry.`);
      const availabilityStart = semverTuple(adapter.introduced ?? "0.0.0");
      const availabilityEnd = adapter.before ? semverTuple(adapter.before) : undefined;
      if (!availabilityStart || (adapter.before && !availabilityEnd) || (availabilityEnd && compareSemver(availabilityStart, availabilityEnd) >= 0))
        return setError(`${harness} has an invalid availability range.`);
      const intervals = (adapter.variants ?? []).map((variant) => ({ variant, start: semverTuple(variant.introduced), end: variant.before ? semverTuple(variant.before) : undefined }));
      if (intervals.some(({ variant, start, end }) => !start || (variant.before && !end) || (end && compareSemver(start!, end) >= 0)))
        return setError(`${harness} has an invalid adapter interval boundary.`);
      if (intervals.some((interval, index) => index > 0 && compareSemver(intervals[index - 1].start!, interval.start!) >= 0))
        return setError(`${harness} adapter intervals must be ordered by introduced version.`);
      intervals.sort((left, right) => compareSemver(left.start!, right.start!));
      for (let index = 1; index < intervals.length; index += 1) {
        const previous = intervals[index - 1];
        if (!previous.end || compareSemver(previous.end, intervals[index].start!) > 0)
          return setError(`${harness} adapter intervals overlap at ${intervals[index].variant.introduced}.`);
      }
      if (intervals.some(({ start, end }) => compareSemver(start!, availabilityStart) < 0 || (availabilityEnd && (!end || compareSemver(end, availabilityEnd) > 0))))
        return setError(`${harness} has a layout interval outside its availability range.`);
      const hasFallback = Boolean(adapter.plugin_dir || adapter.skills_dir || adapter.agents_dir || adapter.hooks_file || adapter.plugins?.length || Object.keys(adapter.helpers ?? {}).length);
      if (!hasFallback && intervals.length) {
        if (compareSemver(intervals[0].start!, availabilityStart) !== 0 || (availabilityEnd ? !intervals.at(-1)?.end || compareSemver(intervals.at(-1)!.end!, availabilityEnd) !== 0 : Boolean(intervals.at(-1)?.end)))
          return setError(`${harness} layout intervals must cover its complete availability range when no fallback is defined.`);
        for (let index = 1; index < intervals.length; index += 1)
          if (!intervals[index - 1].end || compareSemver(intervals[index - 1].end!, intervals[index].start!) !== 0)
            return setError(`${harness} adapter intervals contain a gap before ${intervals[index].variant.introduced}.`);
      }
      const issue = harnessMappingIssue(adapter, metadata);
      if (issue) return setError(describeMappingIssue(issue, adapter, metadata));
    }
    if (catalogIds.has(id) || customPackages.some((item) => item.id === id)) return setError(`Extension ${id} is already configured.`);
    onAdd({ ...draft, id, name: draft.name?.trim() || undefined }); close();
  }

  return (
    <Dialog open={open} onOpenChange={(next) => next ? onOpenChange(true) : close()}>
      <DialogContent className="max-h-[calc(100svh-2rem)] grid-rows-[auto_minmax(0,1fr)_auto] overflow-hidden sm:max-w-2xl">
        <DialogHeader><DialogTitle className="text-lg">Add extension</DialogTitle><DialogDescription>Inspect an immutable source, then map the capabilities it contributes.</DialogDescription></DialogHeader>
        <div className="grid gap-6 overflow-y-auto pr-1">
          <section className="grid gap-3">
            <div><h3 className="font-medium">1. Inspect source</h3><p className="text-sm text-muted-foreground">Blue pins every inspected source to a digest. Managed repository credentials stay in the control plane.</p></div>
            <Tabs value={sourceMode} onValueChange={(value) => setSourceMode(value as "repository" | "archive")}>
              <TabsList><TabsTrigger value="repository">Managed repository</TabsTrigger><TabsTrigger value="archive">Public repository or archive</TabsTrigger></TabsList>
              <TabsContent value="repository" className="grid gap-3 pt-3">
                {connections.length ? <>
                  <input type="hidden" form="package-inspector" name="connection_id" value={sourceMode === "repository" ? connectionId : ""} />
                  <input type="hidden" form="package-inspector" name="repository" value={repository} />
                  <input type="hidden" form="package-inspector" name="ref" value={requestedRef} />
                  <div className="grid gap-3 sm:grid-cols-2"><div className="grid gap-2"><Label>Connection</Label><Select value={connectionId} onValueChange={(value) => value && setConnectionId(value)}><SelectTrigger className="w-full"><SelectValue /></SelectTrigger><SelectContent>{connections.map((connection) => <SelectItem key={connection.id} value={connection.id}>{connection.name}</SelectItem>)}</SelectContent></Select></div><div className="grid gap-2"><Label htmlFor="repository-ref">Ref</Label><Input id="repository-ref" value={requestedRef} placeholder="main or v1.2.0" onChange={(event) => setRequestedRef(event.target.value)} /></div></div>
                  <div className="grid gap-2"><Label htmlFor="repository-name">Repository</Label><Input id="repository-name" value={repository} placeholder="owner/repository" onChange={(event) => setRepository(event.target.value)} /><p className="text-xs text-muted-foreground">Allowed namespaces: {connections.find((item) => item.id === connectionId)?.namespaces.join(", ") || "none"}</p></div>
                </> : <Alert><AlertTitle>No repository connections</AlertTitle><AlertDescription>Ask a deployment operator to configure a GitHub or Bitbucket connection for this organization, or use a public repository or archive.</AlertDescription></Alert>}
              </TabsContent>
              <TabsContent value="archive" className="pt-3"><div className="grid gap-2"><Label htmlFor="inspect-source">Public GitHub repository or HTTPS archive</Label><Input id="inspect-source" form="package-inspector" name="source_ref" placeholder="github:owner/repository@v1.2.0" disabled={sourceMode !== "archive"} /><p className="text-xs text-muted-foreground">Use <code>github:owner/repository@ref</code> or an immutable HTTPS <code>.tar.gz</code> URL. A GitHub repository page URL is not an archive.</p></div></TabsContent>
            </Tabs>
            <Button form="package-inspector" type="submit" variant="outline" disabled={inspecting || (sourceMode === "repository" && !connections.length)}>{inspecting ? "Inspecting…" : "Inspect source"}</Button>
            {inspection.error && <Alert variant="destructive"><AlertDescription>{inspection.error}</AlertDescription></Alert>}
            {inspection.sha256 && <Alert><AlertDescription>Verified {inspection.artifact_id ? "and mirrored managed artifact" : "source"} · {inspection.size_bytes?.toLocaleString()} bytes{inspection.resolved_commit ? ` · commit ${inspection.resolved_commit.slice(0, 12)}` : ""}</AlertDescription></Alert>}
            <details><summary className="cursor-pointer text-sm font-medium">Enter immutable public source manually</summary><div className="mt-3 grid gap-3"><div className="grid gap-2"><Label htmlFor="immutable-source">Immutable source</Label><Input id="immutable-source" value={draft.source_ref} onChange={(event) => setDraft({ ...draft, source_ref: event.target.value, artifact_id: undefined })} /></div><div className="grid gap-2"><Label htmlFor="package-sha">SHA-256</Label><Input id="package-sha" className="font-mono" value={draft.sha256} onChange={(event) => setDraft({ ...draft, sha256: event.target.value.trim().toLowerCase(), artifact_id: undefined })} /></div></div></details>
          </section>
          <section className="grid gap-3">
            <div><h3 className="font-medium">2. Describe extension</h3><p className="text-sm text-muted-foreground">These values identify the bundle in governance revisions.</p></div>
            <div className="grid gap-3 sm:grid-cols-2"><div className="grid gap-2"><Label htmlFor="package-id">Extension ID</Label><Input id="package-id" value={draft.id} placeholder="team-toolkit" onChange={(event) => setDraft({ ...draft, id: event.target.value })} /></div><div className="grid gap-2"><Label htmlFor="package-version">Version</Label><Input id="package-version" value={draft.version} placeholder="1.2.0" onChange={(event) => setDraft({ ...draft, version: event.target.value })} /></div><div className="grid gap-2 sm:col-span-2"><Label htmlFor="package-name">Display name</Label><Input id="package-name" value={draft.name ?? ""} placeholder="Team toolkit" onChange={(event) => setDraft({ ...draft, name: event.target.value })} /></div></div>
          </section>
          <section className="grid gap-3">
            <div><h3 className="font-medium">3. Map capabilities</h3><p className="text-sm text-muted-foreground">Choose what the extension contributes, then point to that item inside the repository.</p></div>
            <div className="grid items-end gap-3 rounded-lg border bg-muted/30 p-3 sm:grid-cols-2">
              <div className="grid gap-2"><Label>Harness</Label><Select value={componentHarness} onValueChange={(value) => value && setComponentHarness(value)}><SelectTrigger className="w-full"><SelectValue /></SelectTrigger><SelectContent>{harnesses.map((value) => <SelectItem value={value.key} key={value.key}>{value.label}</SelectItem>)}</SelectContent></Select></div>
              <div className="grid gap-2"><Label>Capability</Label><Select value={componentKind} onValueChange={(value) => setComponentKind(value as ComponentKind)}><SelectTrigger className="w-full"><SelectValue /></SelectTrigger><SelectContent>{componentKinds.map(([value, label]) => <SelectItem value={value} key={value}>{label}</SelectItem>)}</SelectContent></Select></div>
              <div className="grid gap-2"><Label htmlFor="availability-introduced">Harness available from</Label><Input id="availability-introduced" className="font-mono" value={draft.adapters[componentHarness]?.introduced ?? ""} placeholder="0.0.0 (default)" onChange={(event) => updateAvailability("introduced", event.target.value)} /></div>
              <div className="grid gap-2"><Label htmlFor="availability-before">Harness available before</Label><Input id="availability-before" className="font-mono" value={draft.adapters[componentHarness]?.before ?? ""} placeholder="Open ended" onChange={(event) => updateAvailability("before", event.target.value)} /></div>
              {componentKind === "helper" && <><div className="grid gap-2"><Label htmlFor="helper-name">Command name</Label><Input id="helper-name" value={helperName} placeholder="rtk" onChange={(event) => setHelperName(event.target.value)} /></div><div className="grid gap-2"><Label htmlFor="helper-platform">Platform</Label><Input id="helper-platform" value={helperPlatform} onChange={(event) => setHelperPlatform(event.target.value)} /></div></>}
              <div className="grid gap-2 sm:col-span-2"><Label htmlFor="component-path">Path inside extracted archive</Label><Input id="component-path" value={componentPath} placeholder={componentKind === "skills_dir" ? "repository-commit/skills/secure-code-review" : "repository-commit/path/to/item"} onChange={(event) => setComponentPath(event.target.value)} /><p className="text-xs text-muted-foreground">For a skill, enter the folder containing <code>SKILL.md</code>, or a folder containing multiple skill folders. GitHub archives include a generated top-level <code>repository-commit</code> folder.</p></div>
              <div className="grid gap-2"><Label htmlFor="adapter-introduced">Layout introduced (optional)</Label><Input id="adapter-introduced" className="font-mono" value={introduced} placeholder="2.0.0" onChange={(event) => setIntroduced(event.target.value)} /></div>
              <div className="grid gap-2"><Label htmlFor="adapter-before">Layout before (exclusive)</Label><Input id="adapter-before" className="font-mono" value={before} placeholder="3.0.0 or blank" onChange={(event) => setBefore(event.target.value)} disabled={!introduced.trim()} /></div>
              <Button type="button" onClick={addMapping}>Add mapping</Button>
            </div>
            {rows.length > 0 && <div className="overflow-x-auto rounded-lg border"><Table><TableHeader><TableRow><TableHead>Harness</TableHead><TableHead>Capability</TableHead><TableHead>Path</TableHead><TableHead /></TableRow></TableHeader><TableBody>{rows.map((row, index) => <TableRow key={`${row.harness}:${row.sourcePath}:${index}`}><TableCell className="capitalize">{row.harness}</TableCell><TableCell>{row.label}</TableCell><TableCell className="font-mono text-xs">{row.sourcePath}</TableCell><TableCell className="text-right"><Button type="button" size="sm" variant="ghost" onClick={() => removeMapping(row)}>Remove</Button></TableCell></TableRow>)}</TableBody></Table></div>}
          </section>
          {error && <Alert variant="destructive"><AlertDescription>{error}</AlertDescription></Alert>}
        </div>
        <DialogFooter><Button type="button" variant="outline" onClick={close}>Cancel</Button><Button type="button" onClick={addExtension}>Add to pending changes</Button></DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function RowActions({
  label,
  onEdit,
  onRemove,
  removeDisabled = false,
}: {
  label: string;
  onEdit: () => void;
  onRemove: () => void;
  removeDisabled?: boolean;
}) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger render={<Button type="button" variant="ghost" size="icon-sm" aria-label={`Actions for ${label}`} />}>
        <Ellipsis />
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-36">
        <DropdownMenuItem render={<button type="button" className="w-full" onClick={onEdit} />}>Edit</DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem
          render={<button type="button" className="w-full" disabled={removeDisabled} onClick={onRemove} />}
          variant="destructive"
          disabled={removeDisabled}
        >
          Remove
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function ExtensionInventoryTable({
  packages,
  mcpItems,
  catalogIds,
  packageAudiences,
  packageEnabled,
  emptyLabel,
  emptyDescription,
  addLabel,
  onAdd,
  onPackageToggle,
  onPackageDetails,
  onPackageRemove,
  onMcpToggle,
  onMcpEdit,
  onMcpRemove,
  page,
  perPage,
  total,
  paginationParams,
  onPageChange,
  onPageSizeChange,
}: {
  packages: ManagedPackage[];
  mcpItems: { definition: McpDefinition; index: number }[];
  catalogIds: Set<string>;
  packageAudiences: Record<string, PackageAudience>;
  packageEnabled: (item: ManagedPackage) => boolean;
  emptyLabel: string;
  emptyDescription: string;
  addLabel: string;
  onAdd: () => void;
  onPackageToggle: (item: ManagedPackage, enabled: boolean) => void;
  onPackageDetails: (id: string) => void;
  onPackageRemove: (item: ManagedPackage) => void;
  onMcpToggle: (index: number) => void;
  onMcpEdit: (index: number) => void;
  onMcpRemove: (index: number) => void;
  page: number;
  perPage: number;
  total: number;
  paginationParams: Record<string, string>;
  onPageChange: (page: number) => void;
  onPageSizeChange: (size: number) => void;
}) {
  if (!packages.length && !mcpItems.length) {
    return (
      <section className="-mx-4 overflow-hidden border-y bg-transparent sm:-mx-6 lg:-mx-8">
        <div className="flex min-h-48 flex-col items-center justify-center gap-3 px-4 py-8 text-center sm:px-6 lg:px-8">
          <span className="flex size-9 items-center justify-center rounded-md bg-muted">
            <PackageOpen className="size-4" />
          </span>
          <div>
            <h3 className="font-medium">{emptyLabel}</h3>
            <p className="mt-1 text-sm text-muted-foreground">{emptyDescription}</p>
          </div>
          <Button type="button" onClick={onAdd}><Plus /> {addLabel}</Button>
        </div>
        <TablePaginationFooter pathname="/extensions" params={paginationParams} page={page} perPage={perPage} total={total} label="Extension" onPageChange={onPageChange} onPageSizeChange={onPageSizeChange} />
      </section>
    );
  }

  return (
    <section className="-mx-4 overflow-hidden border-y bg-transparent sm:-mx-6 lg:-mx-8">
      <div className="overflow-x-auto">
      <Table className="min-w-4xl [&_td:first-child]:pl-4 [&_td:last-child]:pr-4 [&_th:first-child]:pl-4 [&_th:last-child]:pr-4 sm:[&_td:first-child]:pl-6 sm:[&_td:last-child]:pr-6 sm:[&_th:first-child]:pl-6 sm:[&_th:last-child]:pr-6 lg:[&_td:first-child]:pl-8 lg:[&_td:last-child]:pr-8 lg:[&_th:first-child]:pl-8 lg:[&_th:last-child]:pr-8">
        <TableHeader className="bg-background/35 text-muted-foreground">
          <TableRow>
            <TableHead className="w-12"><span className="sr-only">Enabled</span></TableHead>
            <TableHead>Extension</TableHead>
            <TableHead>Type</TableHead>
            <TableHead>Agents</TableHead>
            <TableHead>Audience</TableHead>
            <TableHead>Status</TableHead>
            <TableHead className="text-right">Actions</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {packages.map((item) => {
            const active = packageEnabled(item);
            const custom = !catalogIds.has(item.id);
            const kinds = capabilities(item);
            const executable = kinds.some((kind) => ["hooks", "plugins", "helpers"].includes(kind));
            const audience = packageAudiences[item.id] ?? { scope: "organization", user_ids: [] };
            return (
              <TableRow key={`package:${item.id}`} className={active ? "h-14 hover:bg-background/25" : "h-14 opacity-70 hover:bg-background/25"}>
                <TableCell>
                  <Checkbox
                    checked={active}
                    onCheckedChange={(checked) => onPackageToggle(item, checked === true)}
                    aria-label={`${active ? "Disable" : "Enable"} ${item.name ?? item.id}`}
                  />
                </TableCell>
                <TableCell className="max-w-64 whitespace-normal">
                  <div className="font-medium">{item.name ?? item.id}</div>
                  <div className="text-xs text-muted-foreground">v{item.version} · {custom ? "Custom" : "Curated"}</div>
                </TableCell>
                <TableCell className="max-w-72 whitespace-normal">
                  <div className="flex flex-wrap items-center gap-1.5">
                    {kinds.map((kind) => <Badge variant="secondary" key={kind}>{capabilityLabel(kind)}</Badge>)}
                    {executable && <span className="inline-flex items-center gap-1 text-xs text-muted-foreground"><ShieldAlert className="size-3.5" /> Executable</span>}
                  </div>
                </TableCell>
                <TableCell className="max-w-56 whitespace-normal">
                  <div className="flex flex-wrap gap-1.5">
                    {Object.keys(item.adapters ?? {}).map((harness) => <Badge variant="outline" className="capitalize" key={harness}>{harness}</Badge>)}
                  </div>
                </TableCell>
                <TableCell>
                  <Badge variant="outline">
                    {audience.scope === "organization" ? "Everyone" : `${audience.user_ids.length} members`}
                  </Badge>
                </TableCell>
                <TableCell><Badge variant={active ? "default" : "secondary"}>{active ? "Enabled" : "Disabled"}</Badge></TableCell>
                <TableCell className="text-right">
                  <RowActions
                    label={item.name ?? item.id}
                    onEdit={() => onPackageDetails(item.id)}
                    onRemove={() => onPackageRemove(item)}
                    removeDisabled={!custom && !active}
                  />
                </TableCell>
              </TableRow>
            );
          })}
          {mcpItems.map(({ definition, index }) => {
            const server = definition.server;
            const active = !server.disabled;
            const connection = server.command
              ? `${server.command} ${(server.args ?? []).join(" ")}`.trim()
              : server.url;
            return (
              <TableRow key={`mcp:${JSON.stringify(server)}:${definition.harnesses.join(",")}`} className={active ? "h-14 hover:bg-background/25" : "h-14 opacity-70 hover:bg-background/25"}>
                <TableCell>
                  <Checkbox checked={active} onCheckedChange={() => onMcpToggle(index)} aria-label={`${active ? "Disable" : "Enable"} ${server.name}`} />
                </TableCell>
                <TableCell className="max-w-72 whitespace-normal">
                  <div className="font-medium">{server.name}</div>
                  <div className="break-all text-xs text-muted-foreground">{connection}</div>
                </TableCell>
                <TableCell className="whitespace-normal">
                  <div className="flex flex-wrap gap-1.5">
                    <Badge variant="secondary">MCP</Badge>
                    <Badge variant="outline">{server.command ? "Local" : "Remote"}</Badge>
                  </div>
                </TableCell>
                <TableCell className="max-w-56 whitespace-normal">
                  <div className="flex flex-wrap gap-1.5">
                    {definition.harnesses.map((harness) => <Badge variant="outline" className="capitalize" key={harness}>{harness}</Badge>)}
                  </div>
                </TableCell>
                <TableCell><Badge variant="outline">Everyone</Badge></TableCell>
                <TableCell><Badge variant={active ? "default" : "secondary"}>{active ? "Enabled" : "Disabled"}</Badge></TableCell>
                <TableCell className="text-right">
                  <RowActions label={server.name} onEdit={() => onMcpEdit(index)} onRemove={() => onMcpRemove(index)} />
                </TableCell>
              </TableRow>
            );
          })}
        </TableBody>
      </Table>
      </div>
      <TablePaginationFooter pathname="/extensions" params={paginationParams} page={page} perPage={perPage} total={total} label="Extension" onPageChange={onPageChange} onPageSizeChange={onPageSizeChange} />
    </section>
  );
}

export function PackageManager({ revision, catalog, selected, audiences, overrides, connections, mcp, harnesses }: {
  revision: string;
  catalog: ManagedPackage[];
  selected: ManagedPackage[];
  audiences: Record<string, PackageAudience>;
  overrides: Overrides;
  connections: PackageSourceConnection[];
  mcp: McpByHarness;
  harnesses: HarnessMetadata[];
}) {
  const [state, action, publishing] = useActionState<ConfigState, FormData>(saveExtensions, {});
  const [inspection, inspectAction, inspecting] = useActionState<InspectPackageState, FormData>(inspectPackageSource, {});
  const catalogIds = useMemo(() => new Set(catalog.map((item) => item.id)), [catalog]);
  const initiallyEnabled = useMemo(() => new Set(selected.filter((item) => catalogIds.has(item.id)).map((item) => item.id)), [selected, catalogIds]);
  const initialCustom = useMemo(() => selected.filter((item) => !catalogIds.has(item.id)), [selected, catalogIds]);
  const harnessKeys = useMemo(() => harnesses.map((item) => item.key), [harnesses]);
  const initialMcpDefinitions = useMemo(() => groupMcpServers(mcp, harnessKeys), [mcp, harnessKeys]);
  const initialPackages = useMemo(() => [...catalog.filter((item) => initiallyEnabled.has(item.id)), ...initialCustom], [catalog, initiallyEnabled, initialCustom]);
  const initialAudiences = useMemo(() => Object.fromEntries(initialPackages.map((item) => [item.id, audiences[item.id] ?? { scope: "organization", user_ids: [] }])), [initialPackages, audiences]);
  const initialSignature = useMemo(() => JSON.stringify({ packages: initialPackages, audiences: initialAudiences, overrides, mcp: expandMcpDefinitions(initialMcpDefinitions, harnessKeys) }), [initialPackages, initialAudiences, overrides, initialMcpDefinitions, harnessKeys]);
  const [enabled, setEnabled] = useState(() => new Set(initiallyEnabled));
  const [customPackages, setCustomPackages] = useState(initialCustom);
  const [packageAudiences, setPackageAudiences] = useState<Record<string, PackageAudience>>(initialAudiences);
  const [overrideText, setOverrideText] = useState(() => JSON.stringify(overrides));
  const [mcpDefinitions, setMcpDefinitions] = useState<McpDefinition[]>(initialMcpDefinitions);
  const [publishedSignature, setPublishedSignature] = useState(initialSignature);
  const [publishedRevision, setPublishedRevision] = useState(revision);
  const [activeTab, setActiveTab] = useState<ExtensionTab>("all");
  const [page, setPage] = useState(1);
  const [perPage, setPerPage] = useState(25);
  const [addOpen, setAddOpen] = useState(false);
  const [detailId, setDetailId] = useState<string>();
  const [mcpEditorOpen, setMcpEditorOpen] = useState(false);
  const [editingMcpIndex, setEditingMcpIndex] = useState<number>();
  const [confirmation, setConfirmation] = useState<ConfirmationAction>();

  const parsed = useMemo(() => {
    try {
      const value = JSON.parse(overrideText);
      if (!value || Array.isArray(value) || typeof value !== "object") return { value: overrides, error: "Extension overrides must be a JSON object." };
      return { value: value as Overrides, error: "" };
    } catch (error) {
      return { value: overrides, error: error instanceof Error ? error.message : "Invalid override JSON" };
    }
  }, [overrideText, overrides]);
  const packages = [...catalog.filter((item) => enabled.has(item.id)), ...customPackages];
  const currentAudiences = Object.fromEntries(packages.map((item) => [item.id, packageAudiences[item.id] ?? { scope: "organization", user_ids: [] }]));
  const mcpByHarness = expandMcpDefinitions(mcpDefinitions, harnessKeys);
  const directory = [...catalog, ...customPackages];
  const currentSignature = JSON.stringify({ packages, audiences: currentAudiences, overrides: parsed.value, mcp: mcpByHarness });
  const dirty = publishedSignature !== currentSignature;
  const invalidAudience = Object.values(currentAudiences).some((audience) => audience.scope === "users" && !audience.user_ids.length);
  const detailItem = directory.find((item) => item.id === detailId);
  function selectTab(value: ExtensionTab) {
    setActiveTab(value);
    setPage(1);
  }

  useEffect(() => {
    setPublishedSignature(initialSignature);
    setPublishedRevision(revision);
  }, [initialSignature, revision]);
  useEffect(() => {
    if (!state.saved || !state.signature || !state.revision) return;
    setPublishedSignature(state.signature);
    setPublishedRevision(state.revision);
  }, [state.saved, state.signature, state.revision]);
  useUnsavedChangesWarning(dirty && !publishing);

  function updateOverride(harness: string, packageId: string, value: string) {
    const next = structuredClone(parsed.value); next[harness] ??= {}; next[harness][packageId] ??= {};
    if (value === "inherit") delete next[harness][packageId].enabled; else next[harness][packageId].enabled = value === "enabled";
    if (!Object.keys(next[harness][packageId]).length) delete next[harness][packageId]; if (!Object.keys(next[harness]).length) delete next[harness]; setOverrideText(JSON.stringify(next));
  }
  function updateSettings(harness: string, packageId: string, settings: Record<string, unknown>) {
    const next = structuredClone(parsed.value); next[harness] ??= {}; next[harness][packageId] ??= {};
    if (Object.keys(settings).length) next[harness][packageId].settings = settings; else delete next[harness][packageId].settings;
    if (!Object.keys(next[harness][packageId]).length) delete next[harness][packageId]; if (!Object.keys(next[harness]).length) delete next[harness]; setOverrideText(JSON.stringify(next));
  }
  function removeCustom(id: string) {
    setCustomPackages((current) => current.filter((item) => item.id !== id));
    setPackageAudiences((current) => { const next = { ...current }; delete next[id]; return next; });
    const next = structuredClone(parsed.value); Object.keys(next).forEach((harness) => { delete next[harness][id]; if (!Object.keys(next[harness]).length) delete next[harness]; }); setOverrideText(JSON.stringify(next));
  }
  function updateCustomAdapters(id: string, adapters: Record<string, PackageAdapter>) {
    setCustomPackages((current) => current.map((item) =>
      item.id === id ? { ...item, adapters } : item,
    ));
    const next = structuredClone(parsed.value);
    Object.keys(next).forEach((harness) => {
      if (!adapters[harness]) delete next[harness][id];
      if (!Object.keys(next[harness]).length) delete next[harness];
    });
    setOverrideText(JSON.stringify(next));
  }
  function customEnabled(item: ManagedPackage) {
    const adapters = Object.keys(item.adapters ?? {});
    return adapters.some((harness) => parsed.value[harness]?.[item.id]?.enabled !== false);
  }
  function setCustomEnabled(item: ManagedPackage, enabled: boolean) {
    const next = structuredClone(parsed.value);
    Object.keys(item.adapters ?? {}).forEach((harness) => {
      next[harness] ??= {};
      next[harness][item.id] ??= {};
      if (enabled) delete next[harness][item.id].enabled;
      else next[harness][item.id].enabled = false;
      if (!Object.keys(next[harness][item.id]).length) delete next[harness][item.id];
      if (!Object.keys(next[harness]).length) delete next[harness];
    });
    setOverrideText(JSON.stringify(next));
  }
  const counts = Object.fromEntries(tabs.map((tab) => [tab.value, tab.value === "all" ? directory.length + mcpDefinitions.length : tab.value === "mcp" ? mcpDefinitions.length : directory.filter((item) => capabilities(item).includes(tab.value as Capability)).length]));
  const activePackageCount = directory.filter((item) => catalogIds.has(item.id) ? enabled.has(item.id) : customEnabled(item)).length;
  const enabledMcpCount = mcpDefinitions.filter((definition) => !definition.server.disabled).length;

  function openMcpEditor(index?: number) {
    setEditingMcpIndex(index);
    setMcpEditorOpen(true);
  }
  function saveMcp(definition: McpDefinition) {
    setMcpDefinitions((current) => editingMcpIndex === undefined ? [...current, definition] : current.map((item, index) => index === editingMcpIndex ? definition : item));
  }
  function setPackageActive(item: ManagedPackage, active: boolean) {
    if (!catalogIds.has(item.id)) {
      setCustomEnabled(item, active);
      return;
    }
    setEnabled((current) => {
      const next = new Set(current);
      active ? next.add(item.id) : next.delete(item.id);
      return next;
    });
  }
  function toggleMcp(index: number) {
    setMcpDefinitions((current) => current.map((definition, itemIndex) =>
      itemIndex === index
        ? { ...definition, server: { ...definition.server, disabled: !definition.server.disabled } }
        : definition,
    ));
  }
  function removeMcp(index: number) {
    setMcpDefinitions((current) => current.filter((_, itemIndex) => itemIndex !== index));
  }
  function confirmPackageRemoval(item: ManagedPackage) {
    const label = item.name ?? item.id;
    const custom = !catalogIds.has(item.id);
    setConfirmation({
      action: () => custom ? removeCustom(item.id) : setPackageActive(item, false),
      fields: {},
      title: `Remove ${label}?`,
      description: custom
        ? `${label} will be removed from the extension configuration. Managed clients will stop receiving it after you publish these changes.`
        : `${label} will be removed from the active deployment. It will remain available in the curated catalog and can be enabled again later.`,
      confirmLabel: "Remove extension",
      destructive: true,
    });
  }
  function confirmMcpRemoval(index: number) {
    const name = mcpDefinitions[index]?.server.name ?? "this MCP server";
    setConfirmation({
      action: () => removeMcp(index),
      fields: {},
      title: `Remove ${name}?`,
      description: `${name} will be removed from every selected agent after you publish these changes.`,
      confirmLabel: "Remove MCP server",
      destructive: true,
    });
  }
  function discardChanges() {
    const snapshot = JSON.parse(publishedSignature) as {
      packages: ManagedPackage[];
      audiences: Record<string, PackageAudience>;
      overrides: Overrides;
      mcp: McpByHarness;
    };
    setEnabled(new Set(snapshot.packages.filter((item) => catalogIds.has(item.id)).map((item) => item.id)));
    setCustomPackages(snapshot.packages.filter((item) => !catalogIds.has(item.id)));
    setPackageAudiences(structuredClone(snapshot.audiences));
    setOverrideText(JSON.stringify(snapshot.overrides));
    setMcpDefinitions(groupMcpServers(snapshot.mcp, harnessKeys));
    setDetailId(undefined);
    setAddOpen(false);
    setMcpEditorOpen(false);
    setEditingMcpIndex(undefined);
    setConfirmation(undefined);
  }

  return <>
    <form id="package-inspector" action={inspectAction} />
    <form id="extensions-form" action={action} className={dirty ? "grid gap-5 pb-28" : "grid gap-5"}>
      <input type="hidden" name="revision" value={publishedRevision} /><input type="hidden" name="packages" value={JSON.stringify(packages)} /><input type="hidden" name="package_audiences" value={JSON.stringify(currentAudiences)} /><input type="hidden" name="package_overrides" value={overrideText} /><input type="hidden" name="mcp" value={JSON.stringify(mcpByHarness)} />
      <div className="flex justify-end"><Button type="button" onClick={() => activeTab === "mcp" ? openMcpEditor() : setAddOpen(true)}><Plus />{activeTab === "mcp" ? "Add MCP server" : "Add extension"}</Button></div>
      <Tabs value={activeTab} onValueChange={(value) => selectTab(value as ExtensionTab)}>
        <div className="overflow-x-auto border-b"><TabsList variant="line" className="min-w-max px-1">{tabs.map((tab) => <TabsTrigger value={tab.value} key={tab.value} className="gap-2 px-3 py-2">{tab.label}<Badge variant="secondary" className="min-w-5 justify-center px-1.5">{counts[tab.value]}</Badge></TabsTrigger>)}</TabsList></div>
        {tabs.map((tab) => {
          const items = tab.value === "all" ? directory : directory.filter((item) => capabilities(item).includes(tab.value as Capability));
          const mcpItems = tab.value === "all" || tab.value === "mcp"
            ? mcpDefinitions.map((definition, index) => ({ definition, index }))
            : [];
          const mcpTab = tab.value === "mcp";
          const total = (mcpTab ? 0 : items.length) + mcpItems.length;
          const totalPages = Math.max(1, Math.ceil(total / perPage));
          const tabPage = tab.value === activeTab
            ? Math.min(page, totalPages)
            : 1;
          const start = (tabPage - 1) * perPage;
          const end = tabPage * perPage;
          const packageItems = mcpTab ? [] : items.slice(start, end);
          const mcpStart = Math.max(0, start - (mcpTab ? 0 : items.length));
          const mcpEnd = Math.max(0, end - (mcpTab ? 0 : items.length));
          const pageMcpItems = mcpItems.slice(mcpStart, mcpEnd);
          return (
            <TabsContent value={tab.value} key={tab.value} className="pt-5">
              <ExtensionInventoryTable
                packages={packageItems}
                mcpItems={pageMcpItems}
                catalogIds={catalogIds}
                packageAudiences={packageAudiences}
                packageEnabled={(item) => catalogIds.has(item.id) ? enabled.has(item.id) : customEnabled(item)}
                emptyLabel={`No ${tab.value === "all" ? "extensions" : tab.label.toLowerCase()} yet`}
                emptyDescription={mcpTab ? "Add a local or remote tool connection." : "Add an extension to distribute it to managed clients."}
                addLabel={mcpTab ? "Add MCP server" : "Add extension"}
                onAdd={() => mcpTab ? openMcpEditor() : setAddOpen(true)}
                onPackageToggle={setPackageActive}
                onPackageDetails={setDetailId}
                onPackageRemove={confirmPackageRemoval}
                onMcpToggle={toggleMcp}
                onMcpEdit={openMcpEditor}
                onMcpRemove={confirmMcpRemoval}
                page={tabPage}
                perPage={perPage}
                total={total}
                paginationParams={{}}
                onPageChange={setPage}
                onPageSizeChange={(size) => {
                  setPerPage(size);
                  setPage(1);
                }}
              />
            </TabsContent>
          );
        })}
      </Tabs>
      {dirty && (
        <FloatingSaveBar
          title="Unpublished extension changes"
          description={`Revision ${publishedRevision} · ${activePackageCount} packages · ${enabledMcpCount} MCP servers`}
          saveLabel="Publish extension changes"
          pendingLabel="Publishing extension changes…"
          onDiscard={discardChanges}
          disabled={Boolean(parsed.error) || invalidAudience}
        />
      )}
    </form>
    <AddDialog open={addOpen} defaultKind={activeTab} catalogIds={catalogIds} customPackages={customPackages} inspection={inspection} inspecting={inspecting} connections={connections} harnesses={harnesses} onOpenChange={setAddOpen} onAdd={(item) => { setCustomPackages((current) => [...current, item]); setPackageAudiences((current) => ({ ...current, [item.id]: { scope: "organization", user_ids: [] } })); }} />
    <McpEditorDialog open={mcpEditorOpen} definition={editingMcpIndex === undefined ? undefined : mcpDefinitions[editingMcpIndex]} definitions={mcpDefinitions} editingIndex={editingMcpIndex} supportedHarnesses={harnessKeys} onOpenChange={(open) => { setMcpEditorOpen(open); if (!open) setEditingMcpIndex(undefined); }} onSave={saveMcp} />
    {detailItem && <DetailSheet item={detailItem} custom={!catalogIds.has(detailItem.id)} overrideText={overrideText} audience={packageAudiences[detailItem.id] ?? { scope: "organization", user_ids: [] }} harnesses={harnesses} dirty={dirty} publishing={publishing} publishDisabled={Boolean(parsed.error) || invalidAudience} onClose={() => setDetailId(undefined)} onAudienceChange={(audience) => setPackageAudiences((current) => ({ ...current, [detailItem.id]: audience }))} onAdaptersChange={(adapters) => updateCustomAdapters(detailItem.id, adapters)} onOverrideChange={updateOverride} onSettingsChange={updateSettings} />}
    <ConfirmationDialog confirmation={confirmation} open={Boolean(confirmation)} onOpenChange={(open) => { if (!open) setConfirmation(undefined); }} />
  </>;
}
