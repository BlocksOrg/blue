"use client";

import { useActionState, useEffect, useMemo, useState } from "react";
import { AlertCircle, Check, Plus, RefreshCw, Search } from "lucide-react";
import { useRouter } from "next/navigation";
import {
  acknowledgeGatewayModel,
  refreshGatewayModels,
  saveHarnessGatewayModels,
  type GatewayModelsActionState,
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
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  canEditGatewayModelAssignments,
  gatewayModelAssignmentReadOnlyReason,
  gatewayModelFallbackLabel,
  modelStateLabel,
  modelStateWarning,
  type GatewayModel,
  type GatewayModelHarness,
  type GatewayModelsResource,
  type GatewayModelsTab,
} from "@/lib/gateway-models";

function displayDate(value?: string | null) {
  return value ? new Date(value).toLocaleString() : "Never";
}

function errorAlert(error?: string) {
  return error ? (
    <Alert variant="destructive">
      <AlertCircle />
      <AlertTitle>Action failed</AlertTitle>
      <AlertDescription>{error}</AlertDescription>
    </Alert>
  ) : null;
}

function RefreshCatalogButton() {
  const router = useRouter();
  const [state, action, pending] = useActionState<GatewayModelsActionState>(
    refreshGatewayModels,
    {},
  );
  useEffect(() => {
    if (state.saved || state.error) router.refresh();
  }, [state.saved, state.error, router]);
  return (
    <div className="grid justify-items-end gap-2">
      <form action={action}>
        <Button type="submit" variant="outline" size="sm" disabled={pending}>
          <RefreshCw className={pending ? "animate-spin" : undefined} />
          {pending ? "Refreshing…" : "Refresh now"}
        </Button>
      </form>
      {state.error && <p className="text-xs text-destructive">{state.error}</p>}
    </div>
  );
}

function AcknowledgeButton({ model }: { model: GatewayModel }) {
  const router = useRouter();
  const [state, action, pending] = useActionState<GatewayModelsActionState, FormData>(
    acknowledgeGatewayModel,
    {},
  );
  useEffect(() => {
    if (state.saved) router.refresh();
  }, [state.saved, router]);
  if (!model.fingerprint) return null;
  return (
    <form action={action} className="grid justify-items-end gap-1">
      <input type="hidden" name="model_id" value={model.id} />
      <input type="hidden" name="fingerprint" value={model.fingerprint} />
      <Button type="submit" variant="outline" size="sm" disabled={pending}>
        <Check /> {pending ? "Acknowledging…" : "Acknowledge"}
      </Button>
      {state.error && <span className="max-w-56 text-right text-xs text-destructive">{state.error}</span>}
    </form>
  );
}

function HarnessEditor({
  harness,
  models,
  revision,
  onClose,
}: {
  harness: GatewayModelHarness;
  models: GatewayModel[];
  revision: string;
  onClose: () => void;
}) {
  const router = useRouter();
  const [assigned, setAssigned] = useState(harness.gateway_models);
  const [defaultModel, setDefaultModel] = useState(harness.default_model ?? "none");
  const [manual, setManual] = useState("");
  const [query, setQuery] = useState("");
  const [state, action, pending] = useActionState<GatewayModelsActionState, FormData>(
    saveHarnessGatewayModels,
    {},
  );
  const known = useMemo(() => {
    const byId = new Map(models.map((model) => [model.id, model]));
    for (const id of assigned) {
      if (!byId.has(id)) {
        byId.set(id, { id, metadata: {}, state: "unknown", assigned_to: [harness.key] });
      }
    }
    return [...byId.values()].sort((left, right) => left.id.localeCompare(right.id));
  }, [models, assigned, harness.key]);
  const filtered = known.filter((model) =>
    `${model.id} ${model.display_name ?? ""}`.toLocaleLowerCase().includes(query.trim().toLocaleLowerCase()),
  );

  useEffect(() => {
    if (!state.saved) return;
    router.refresh();
    onClose();
  }, [state.saved, router, onClose]);

  function toggle(id: string, checked: boolean) {
    setAssigned((current) => checked
      ? current.includes(id) ? current : [...current, id]
      : current.filter((value) => value !== id));
    if (!checked && defaultModel === id) setDefaultModel("none");
  }

  function addManual() {
    const value = manual.trim();
    if (!value || assigned.includes(value)) return;
    setAssigned((current) => [...current, value]);
    setManual("");
  }

  return (
    <Dialog open onOpenChange={(open) => !open && !pending && onClose()}>
      <DialogContent className="max-h-[calc(100svh-2rem)] overflow-hidden p-0 sm:max-w-2xl">
        <form action={action} className="grid max-h-[calc(100svh-2rem)] min-h-0 grid-rows-[auto_minmax(0,1fr)_auto]">
          <input type="hidden" name="revision" value={revision} />
          <input type="hidden" name="harness" value={harness.key} />
          <input type="hidden" name="gateway_models" value={JSON.stringify(assigned)} />
          <input type="hidden" name="default_model" value={defaultModel === "none" ? "" : defaultModel} />
          <DialogHeader className="border-b p-6">
            <DialogTitle>{harness.label} models</DialogTitle>
            <DialogDescription>
              {harness.exposure === "catalog"
                ? "This agent exposes every assigned model in its model picker."
                : "This agent exposes only its selected model; additional assignments remain available for changing the default."}
            </DialogDescription>
          </DialogHeader>
          <div className="flex min-h-0 flex-col gap-5 overflow-hidden p-6">
            {errorAlert(state.error)}
            <div className="grid shrink-0 gap-2">
              <Label>Default model</Label>
              <Select value={defaultModel} onValueChange={(value) => value && setDefaultModel(value)}>
                <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
                <SelectContent>
                  <SelectItem value="none">Use first assigned model</SelectItem>
                  {assigned.map((id) => <SelectItem key={id} value={id}>{id}</SelectItem>)}
                </SelectContent>
              </Select>
              <p className="text-xs text-muted-foreground">
                Without an explicit default, this harness resolves to the first assigned model.
              </p>
            </div>
            <div className="grid shrink-0 gap-2">
              <Label htmlFor={`${harness.key}-manual-model`}>Add model ID manually</Label>
              <div className="flex gap-2">
                <Input
                  id={`${harness.key}-manual-model`}
                  value={manual}
                  onChange={(event) => setManual(event.target.value)}
                  placeholder="provider/model-id"
                  onKeyDown={(event) => {
                    if (event.key === "Enter") { event.preventDefault(); addManual(); }
                  }}
                />
                <Button type="button" variant="outline" onClick={addManual}><Plus /> Add</Button>
              </div>
              <p className="text-xs text-muted-foreground">Undiscovered IDs are allowed and will be marked Unknown.</p>
            </div>
            <div className="relative shrink-0">
              <Search className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" />
              <Input value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search models" className="pl-8" />
            </div>
            <div aria-label="Available gateway models" className="min-h-32 flex-1 overflow-y-auto rounded-xl border">
              {filtered.length === 0 ? (
                <p className="p-5 text-sm text-muted-foreground">No models match this search.</p>
              ) : filtered.map((model) => (
                <label key={model.id} className="flex cursor-pointer items-start gap-3 border-b p-4 last:border-b-0">
                  <Checkbox checked={assigned.includes(model.id)} onCheckedChange={(checked) => toggle(model.id, checked === true)} />
                  <span className="grid min-w-0 flex-1 gap-1">
                    <span className="break-all font-mono text-sm">{model.id}</span>
                    {modelStateWarning(model.state) && <span className="text-xs text-amber-700 dark:text-amber-400">{modelStateWarning(model.state)}</span>}
                  </span>
                  <Badge variant={model.state === "available" ? "secondary" : "outline"}>{modelStateLabel(model.state)}</Badge>
                </label>
              ))}
            </div>
          </div>
          <DialogFooter className="mx-0 mb-0 min-h-16 items-center rounded-b-xl px-6 py-4">
            <Button type="button" variant="outline" onClick={onClose} disabled={pending}>Cancel</Button>
            <Button type="submit" disabled={pending || assigned.length === 0}>
              {pending ? "Saving…" : "Save assignments"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

function DiscoveryTab({ resource }: { resource: GatewayModelsResource }) {
  return (
    <div className="grid gap-6">
      {resource.sync.discovery_supported === false && (
        <Alert><AlertCircle /><AlertTitle>Discovery unsupported</AlertTitle><AlertDescription>This provisioner does not support model discovery. Manual assignments remain available.</AlertDescription></Alert>
      )}
      {resource.sync.latest_error && (
        <Alert variant="destructive"><AlertCircle /><AlertTitle>Latest refresh failed</AlertTitle><AlertDescription>{resource.sync.latest_error} The last successful catalog was preserved.</AlertDescription></Alert>
      )}
      {resource.sync.stale && !resource.sync.latest_error && (
        <Alert><AlertCircle /><AlertTitle>Catalog is stale</AlertTitle><AlertDescription>The last successful refresh is older than two refresh intervals.</AlertDescription></Alert>
      )}
      <section className="overflow-hidden rounded-xl border">
        <div className="flex flex-col gap-4 border-b p-4 sm:flex-row sm:items-center sm:justify-between">
          <div>
            <h2 className="font-medium">Discovery status</h2>
            <p className="text-sm text-muted-foreground">Last successful refresh: {displayDate(resource.sync.last_successful_refresh_at)}</p>
          </div>
          <RefreshCatalogButton />
        </div>
        <Table>
          <TableBody>
            <TableRow><TableCell className="w-48 text-muted-foreground">Gateway type</TableCell><TableCell className="font-medium">{resource.gateway_type}</TableCell></TableRow>
            <TableRow><TableCell className="text-muted-foreground">Refresh interval</TableCell><TableCell>{resource.refresh_interval_seconds} seconds</TableCell></TableRow>
            <TableRow><TableCell className="text-muted-foreground">Last attempted refresh</TableCell><TableCell>{displayDate(resource.sync.last_refresh_at)}</TableCell></TableRow>
            <TableRow><TableCell className="text-muted-foreground">Source revision</TableCell><TableCell className="break-all font-mono text-xs">{resource.sync.source_revision ?? "—"}</TableCell></TableRow>
          </TableBody>
        </Table>
      </section>
    </div>
  );
}

function CatalogTab({ resource }: { resource: GatewayModelsResource }) {
  const [query, setQuery] = useState("");
  const models = resource.models.filter((model) =>
    `${model.id} ${model.display_name ?? ""} ${model.assigned_to.join(" ")}`
      .toLocaleLowerCase()
      .includes(query.trim().toLocaleLowerCase()),
  );
  return (
    <section className="overflow-hidden rounded-xl border">
        <div className="flex flex-col gap-3 border-b p-4 sm:flex-row sm:items-center sm:justify-between">
          <div><h2 className="font-medium">Model catalog</h2><p className="text-sm text-muted-foreground">{resource.models.length} known and manually assigned model IDs</p></div>
          <div className="relative w-full sm:max-w-xs"><Search className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" /><Input value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search models" className="pl-8" /></div>
        </div>
        <Table>
          <TableHeader><TableRow><TableHead>Model</TableHead><TableHead>Status</TableHead><TableHead>Assigned to</TableHead><TableHead>Last seen</TableHead><TableHead><span className="sr-only">Actions</span></TableHead></TableRow></TableHeader>
          <TableBody>{models.length === 0 ? <TableRow><TableCell colSpan={5} className="py-10 text-center text-muted-foreground">No models found.</TableCell></TableRow> : models.map((model) => (
            <TableRow key={model.id}>
              <TableCell><div className="grid gap-1"><span className="break-all font-mono text-xs">{model.id}</span>{model.display_name && model.display_name !== model.id && <span className="text-xs text-muted-foreground">{model.display_name}</span>}{modelStateWarning(model.state) && <span className="text-xs text-amber-700 dark:text-amber-400">{modelStateWarning(model.state)}</span>}</div></TableCell>
              <TableCell><Badge variant={model.state === "available" ? "secondary" : model.state === "removed" ? "destructive" : "outline"}>{modelStateLabel(model.state)}</Badge></TableCell>
              <TableCell className="capitalize">{model.assigned_to.join(", ") || "—"}</TableCell>
              <TableCell>{displayDate(model.last_seen_at)}</TableCell>
              <TableCell className="text-right">{model.state === "changed" && <AcknowledgeButton model={model} />}</TableCell>
            </TableRow>
          ))}</TableBody>
        </Table>
    </section>
  );
}

function AssignmentsTab({ resource }: { resource: GatewayModelsResource }) {
  const [editing, setEditing] = useState<GatewayModelHarness>();
  return (
    <>
      <section className="overflow-hidden rounded-xl border">
        <div className="border-b p-4"><h2 className="font-medium">Harness assignments</h2><p className="text-sm text-muted-foreground">Assignments are revisioned governance policy and are never changed by discovery.</p></div>
        <Table>
          <TableHeader><TableRow><TableHead>Harness</TableHead><TableHead>Assigned</TableHead><TableHead>Default</TableHead><TableHead>Exposure</TableHead><TableHead><span className="sr-only">Actions</span></TableHead></TableRow></TableHeader>
          <TableBody>{resource.harnesses.map((harness) => (
            <TableRow key={harness.key}>
              <TableCell className="font-medium">{harness.label}</TableCell>
              <TableCell>{harness.gateway_models.length}</TableCell>
              <TableCell className="max-w-64 truncate font-mono text-xs">
                {gatewayModelFallbackLabel(harness)}
              </TableCell>
              <TableCell><Badge variant="outline">{harness.exposure === "catalog" ? "Full catalog" : "Selected only"}</Badge></TableCell>
              <TableCell className="text-right">
                {canEditGatewayModelAssignments(harness) ? (
                  <Button type="button" variant="outline" size="sm" aria-label={`Edit ${harness.label} models`} onClick={() => setEditing(harness)}>Edit</Button>
                ) : (
                  <span className="inline-grid justify-items-end gap-1">
                    <Button type="button" variant="outline" size="sm" disabled aria-label={`Edit ${harness.label} models`}>Edit</Button>
                    <span className="max-w-64 text-xs text-muted-foreground">{gatewayModelAssignmentReadOnlyReason(harness)}</span>
                  </span>
                )}
              </TableCell>
            </TableRow>
          ))}</TableBody>
        </Table>
      </section>
      {editing && <HarnessEditor harness={editing} models={resource.models} revision={resource.revision} onClose={() => setEditing(undefined)} />}
    </>
  );
}

export function ModelCatalog({
  resource,
  activeTab,
}: {
  resource: GatewayModelsResource;
  activeTab: GatewayModelsTab;
}) {
  const [tab, setTab] = useState<GatewayModelsTab>(activeTab);

  useEffect(() => setTab(activeTab), [activeTab]);

  function selectTab(next: string | number) {
    if (next !== "discovery" && next !== "catalog" && next !== "assignments") return;
    setTab(next);
    const url = new URL(window.location.href);
    url.searchParams.set("tab", "models");
    if (next === "discovery") url.searchParams.delete("models_tab");
    else url.searchParams.set("models_tab", next);
    window.history.replaceState(window.history.state, "", url);
  }

  return (
    <Tabs value={tab} onValueChange={selectTab} className="gap-6">
      <div className="overflow-x-auto border-b">
        <TabsList variant="line" className="h-10 min-w-max gap-5 px-1">
          <TabsTrigger value="discovery" className="flex-none px-2">Discovery</TabsTrigger>
          <TabsTrigger value="catalog" className="flex-none px-2">Catalog</TabsTrigger>
          <TabsTrigger value="assignments" className="flex-none px-2">Assignments</TabsTrigger>
        </TabsList>
      </div>
      <TabsContent value="discovery"><DiscoveryTab resource={resource} /></TabsContent>
      <TabsContent value="catalog"><CatalogTab resource={resource} /></TabsContent>
      <TabsContent value="assignments"><AssignmentsTab resource={resource} /></TabsContent>
    </Tabs>
  );
}
