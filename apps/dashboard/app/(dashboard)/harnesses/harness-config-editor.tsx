"use client";

import { useActionState, useEffect, useState } from "react";
import { Ellipsis, Search, X } from "lucide-react";
import {
  saveHarnessManagedConfig,
  type HarnessConfigState,
} from "@/app/actions";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Textarea } from "@/components/ui/textarea";
import { useUnsavedChangesWarning } from "@/hooks/use-unsaved-changes-warning";
import type { HarnessMetadata } from "@/lib/harness-metadata";
import { TablePaginationFooter } from "@/components/table-pagination-footer";

export type HarnessName = string;

function hasManagedConfig(value: string | undefined) {
  const normalized = value?.trim();
  return Boolean(normalized && normalized !== "{}");
}

function HarnessActions({
  harness,
  onEdit,
}: {
  harness: HarnessMetadata;
  onEdit: (harness: HarnessMetadata) => void;
}) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        render={
          <Button
            variant="ghost"
            size="icon-sm"
            aria-label={`Actions for ${harness.label}`}
          />
        }
      >
        <Ellipsis />
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-40">
        <DropdownMenuItem onClick={() => onEdit(harness)}>
          Edit
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function HarnessEditDialog({
  harness,
  revision,
  managedConfigYaml,
  versionRequirement,
  allowUnverifiedVersions,
  onClose,
  onSaved,
}: {
  harness: HarnessMetadata;
  revision: string;
  managedConfigYaml: string;
  versionRequirement: string;
  allowUnverifiedVersions: boolean;
  onClose: () => void;
  onSaved: (value: {
    revision: string;
    managedConfigYaml: string;
    versionRequirement: string;
    allowUnverifiedVersions: boolean;
  }) => void;
}) {
  const [state, action, saving] = useActionState<HarnessConfigState, FormData>(
    saveHarnessManagedConfig,
    {},
  );
  const [configDraft, setConfigDraft] = useState(managedConfigYaml);
  const [versionDraft, setVersionDraft] = useState(versionRequirement);
  const [unverifiedDraft, setUnverifiedDraft] = useState(allowUnverifiedVersions);
  const dirty =
    configDraft !== managedConfigYaml || versionDraft !== versionRequirement || unverifiedDraft !== allowUnverifiedVersions;

  useUnsavedChangesWarning(dirty && !saving);

  useEffect(() => {
    if (
      !state.saved ||
      !state.revision ||
      state.managedConfigYaml === undefined
    ) {
      return;
    }
    onSaved({
      revision: state.revision,
      managedConfigYaml: state.managedConfigYaml,
      versionRequirement: state.versionRequirement ?? "",
      allowUnverifiedVersions: state.allowUnverifiedVersions ?? false,
    });
  }, [state, onSaved]);

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open && !saving) onClose();
      }}
    >
      <DialogContent className="max-h-[80vh] overflow-hidden p-0 sm:max-w-2xl">
        <form
          action={action}
          className="grid max-h-[80vh] min-h-0 grid-rows-[auto_minmax(0,1fr)_auto]"
        >
          <input type="hidden" name="revision" value={revision} />
          <input type="hidden" name="harness" value={harness.key} />
          <DialogHeader className="p-4 pr-12">
            <DialogTitle>Edit {harness.label}</DialogTitle>
            <DialogDescription>
              Manage the version policy and settings that Blue applies for this
              coding agent.
            </DialogDescription>
          </DialogHeader>

          <div className="grid min-h-0 gap-5 overflow-y-auto border-t p-4">
            {state.error && (
              <Alert variant="destructive">
                <AlertTitle>Configuration not saved</AlertTitle>
                <AlertDescription>{state.error}</AlertDescription>
              </Alert>
            )}

            <div className="grid gap-2">
              <Label htmlFor={`${harness.key}-version-requirement`}>
                Allowed harness versions
              </Label>
              <Input
                id={`${harness.key}-version-requirement`}
                name="version_requirement"
                className="font-mono"
                value={versionDraft}
                onChange={(event) => setVersionDraft(event.target.value)}
                placeholder=">=1.2.0, <2.0.0"
                spellCheck={false}
                autoFocus
              />
              <p className="text-xs text-muted-foreground">
                Optional semver range. Incompatible or unparseable installed
                versions are blocked before Blue changes managed files. Native
                updates remain enabled only when unverified versions are
                allowed with a range that has no maximum.
              </p>
            </div>

            <div className="flex items-start gap-3 rounded-md border p-3">
              <Checkbox
                id={`${harness.key}-allow-unverified`}
                name="allow_unverified_versions"
                checked={unverifiedDraft}
                onCheckedChange={(checked) => setUnverifiedDraft(checked === true)}
              />
              <div className="grid gap-1">
                <Label htmlFor={`${harness.key}-allow-unverified`}>
                  Allow unverified versions
                </Label>
                <p className="text-xs text-muted-foreground">
                  Requires an explicit range. Blue will reuse the latest
                  matching profile, but vendor breaking changes may produce
                  invalid configuration.
                </p>
              </div>
            </div>

            <div className="grid gap-2">
              <Label htmlFor={`${harness.key}-managed-config`}>
                Managed config YAML
              </Label>
              <Textarea
                id={`${harness.key}-managed-config`}
                name="managed_config_yaml"
                className="min-h-72 font-mono text-xs"
                value={configDraft}
                onChange={(event) => setConfigDraft(event.target.value)}
                aria-invalid={Boolean(state.error)}
                aria-describedby={`${harness.key}-managed-config-help`}
                placeholder={"model: model-name\nsetting_name: value"}
                spellCheck={false}
              />
              <p
                id={`${harness.key}-managed-config-help`}
                className={
                  state.error
                    ? "text-xs text-destructive"
                    : "text-xs text-muted-foreground"
                }
              >
                Enter a YAML mapping of setting names to values. An empty
                mapping clears all managed settings for this agent.
              </p>
            </div>
          </div>

          <DialogFooter className="mx-0 mb-0 rounded-b-xl">
            <Button
              type="button"
              variant="outline"
              onClick={onClose}
              disabled={saving}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={saving}>
              {saving ? "Validating and saving…" : "Save changes"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

export function HarnessConfigEditor({
  revision: initialRevision,
  configurations,
  versionRequirements,
  unverifiedOverrides,
  harnesses,
}: {
  revision: string;
  configurations: Record<string, string>;
  versionRequirements: Record<string, string | null>;
  unverifiedOverrides: Record<string, boolean>;
  harnesses: HarnessMetadata[];
}) {
  const [revision, setRevision] = useState(initialRevision);
  const [savedConfigs, setSavedConfigs] = useState(configurations);
  const [savedVersions, setSavedVersions] = useState<Record<string, string>>(
    () =>
      Object.fromEntries(
        Object.entries(versionRequirements).map(([key, value]) => [
          key,
          value ?? "",
        ]),
      ),
  );
  const [savedUnverified, setSavedUnverified] = useState(unverifiedOverrides);
  const [editingHarness, setEditingHarness] = useState<HarnessMetadata>();
  const [searchDraft, setSearchDraft] = useState("");
  const [query, setQuery] = useState("");
  const [page, setPage] = useState(1);
  const [perPage, setPerPage] = useState(25);
  const normalizedQuery = query.trim().toLocaleLowerCase();
  const filteredHarnesses = normalizedQuery
    ? harnesses.filter((harness) => {
        const configured = hasManagedConfig(savedConfigs[harness.key]);
        const searchable = [
          harness.key,
          harness.label,
          harness.description,
          ...harness.aliases,
          ...harness.capabilities,
          ...(harness.generations.flatMap((generation) => [
            generation.profile,
            generation.introduced,
            generation.before,
            generation.lifecycle,
          ])),
          savedVersions[harness.key] || "any version",
          configured ? "configured" : "not configured",
        ]
          .join(" ")
          .toLocaleLowerCase();
        return searchable.includes(normalizedQuery);
      })
    : harnesses;
  const totalPages = Math.max(1, Math.ceil(filteredHarnesses.length / perPage));
  const currentPage = Math.min(page, totalPages);
  const visibleHarnesses = filteredHarnesses.slice(
    (currentPage - 1) * perPage,
    currentPage * perPage,
  );

  return (
    <>
      <div className="flex flex-col gap-6">
        <form
          className="flex min-w-0 flex-1 gap-2"
          role="search"
          onSubmit={(event) => {
            event.preventDefault();
            setQuery(searchDraft.trim());
            setPage(1);
          }}
        >
          <div className="relative min-w-0 max-w-md flex-1">
            <Search className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" />
            <Input
              value={searchDraft}
              onChange={(event) => setSearchDraft(event.target.value)}
              maxLength={200}
              placeholder="Search harnesses"
              className="px-8"
              aria-label="Search harnesses"
            />
            {searchDraft && (
              <Button
                type="button"
                variant="ghost"
                size="icon-sm"
                className="absolute top-1/2 right-0.5 -translate-y-1/2"
                aria-label="Clear harness search"
                onClick={() => {
                  setSearchDraft("");
                  setQuery("");
                  setPage(1);
                }}
              >
                <X />
              </Button>
            )}
          </div>
          <Button type="submit" variant="secondary">
            Search
          </Button>
        </form>

        <section className="-mx-4 overflow-hidden border-y bg-transparent sm:-mx-6 lg:-mx-8">
        <div className="overflow-x-auto">
        <Table className="min-w-4xl [&_td:first-child]:pl-4 [&_td:last-child]:pr-4 [&_th:first-child]:pl-4 [&_th:last-child]:pr-4 sm:[&_td:first-child]:pl-6 sm:[&_td:last-child]:pr-6 sm:[&_th:first-child]:pl-6 sm:[&_th:last-child]:pr-6 lg:[&_td:first-child]:pl-8 lg:[&_td:last-child]:pr-8 lg:[&_th:first-child]:pl-8 lg:[&_th:last-child]:pr-8">
          <TableHeader className="bg-background/35 text-muted-foreground">
            <TableRow>
              <TableHead>Harness</TableHead>
              <TableHead>Supported versions</TableHead>
              <TableHead>Allowed versions</TableHead>
              <TableHead>Managed config</TableHead>
              <TableHead className="text-right">Actions</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {filteredHarnesses.length === 0 && (
              <TableRow>
                <TableCell
                  colSpan={5}
                  className="h-24 text-center text-muted-foreground"
                >
                  {query
                    ? `No harnesses match “${query}”.`
                    : "No harnesses are available."}
                </TableCell>
              </TableRow>
            )}
            {visibleHarnesses.map((harness) => {
              const versionRequirement = savedVersions[harness.key] ?? "";
              const configured = hasManagedConfig(savedConfigs[harness.key]);
              return (
                <TableRow
                  key={harness.key}
                  className="h-12 hover:bg-background/25"
                >
                  <TableCell className="max-w-80 whitespace-normal">
                    <div className="font-medium">{harness.label}</div>
                    <div className="mt-0.5 text-xs text-muted-foreground">
                      {harness.description}
                    </div>
                  </TableCell>
                  <TableCell className="whitespace-normal">
                    <div className="flex min-w-56 flex-wrap gap-1.5">
                      {harness.generations.map((generation) => (
                        <Badge
                          key={generation.profile}
                          variant={
                            generation.lifecycle === "supported"
                              ? "secondary"
                              : "outline"
                          }
                          className="font-mono font-normal"
                        >
                          {generation.profile}: [{generation.introduced},{" "}
                          {generation.before})
                          {generation.lifecycle === "deprecated"
                            ? " · deprecated"
                            : ""}
                        </Badge>
                      ))}
                    </div>
                  </TableCell>
                  <TableCell className="font-mono text-xs">
                    {versionRequirement || (
                      <span className="font-sans text-muted-foreground">
                        Any version
                      </span>
                    )}
                  </TableCell>
                  <TableCell>
                    <Badge variant={configured ? "secondary" : "outline"}>
                      {configured ? "Configured" : "Not configured"}
                    </Badge>
                  </TableCell>
                  <TableCell className="text-right">
                    <HarnessActions
                      harness={harness}
                      onEdit={setEditingHarness}
                    />
                  </TableCell>
                </TableRow>
              );
            })}
          </TableBody>
        </Table>
        </div>
        <TablePaginationFooter
          pathname="/harnesses"
          params={{}}
          page={currentPage}
          perPage={perPage}
          total={filteredHarnesses.length}
          label="Harness"
          onPageChange={setPage}
          onPageSizeChange={(size) => {
            setPerPage(size);
            setPage(1);
          }}
        />
        </section>
      </div>

      {editingHarness && (
        <HarnessEditDialog
          key={editingHarness.key}
          harness={editingHarness}
          revision={revision}
          managedConfigYaml={savedConfigs[editingHarness.key] ?? ""}
          versionRequirement={savedVersions[editingHarness.key] ?? ""}
          allowUnverifiedVersions={savedUnverified[editingHarness.key] ?? false}
          onClose={() => setEditingHarness(undefined)}
          onSaved={(value) => {
            setRevision(value.revision);
            setSavedConfigs((current) => ({
              ...current,
              [editingHarness.key]: value.managedConfigYaml,
            }));
            setSavedVersions((current) => ({
              ...current,
              [editingHarness.key]: value.versionRequirement,
            }));
            setSavedUnverified((current) => ({
              ...current,
              [editingHarness.key]: value.allowUnverifiedVersions,
            }));
            setEditingHarness(undefined);
          }}
        />
      )}
    </>
  );
}
