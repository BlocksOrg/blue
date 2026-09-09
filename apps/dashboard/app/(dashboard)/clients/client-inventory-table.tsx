"use client";

import { Fragment, useState } from "react";
import Link from "next/link";
import { ArrowDown, ArrowUp, ChevronRight, CircleAlert, Ellipsis } from "lucide-react";
import { removeClientStatus } from "@/app/actions";
import {
  ConfirmationDialog,
  type ConfirmationAction,
} from "@/components/confirmation-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";

export type HarnessInventory = {
  name: string;
  api_allowed: boolean;
  client_supported: boolean;
  installed: boolean;
  path?: string;
  version?: string;
  reconciled: boolean;
  compatibility_profile?: string;
  compatibility_error?: string;
};

export type PackageInventory = {
  id: string;
  version: string;
  sha256?: string;
  harness: string;
  state: string;
  error?: string;
};

export type ClientStatus = {
  id: string;
  instance_id: string;
  hostname?: string;
  user_email: string;
  client_version: string;
  platform: string;
  architecture?: string;
  config_revision?: string;
  applied: boolean;
  files_ok: boolean;
  harnesses?: Array<HarnessInventory | string>;
  packages?: PackageInventory[];
  error?: string;
  status: "current" | "outdated" | "attention";
  status_reasons: string[];
  first_seen_at: string;
  last_seen_at: string;
  activity_status: "recent" | "stale";
};

type ClientStatusView = ClientStatus & { first_seen_label: string; last_seen_label: string };

function harnessState(harness: HarnessInventory) {
  if (harness.compatibility_error) return "Version mismatch";
  if (!harness.client_supported) return "Unsupported client";
  if (!harness.api_allowed)
    return harness.installed ? "Installed · not allowed" : "Not allowed";
  if (!harness.installed) return "Allowed · not installed";
  return harness.reconciled ? "Reconciled" : "Ready";
}

function revisionNotice(client: ClientStatusView, currentRevision?: string) {
  if (!currentRevision || client.config_revision === currentRevision) return undefined;
  return client.config_revision
    ? `Running revision ${client.config_revision}; current revision is ${currentRevision}.`
    : `No applied revision reported; current revision is ${currentRevision}.`;
}

const statusLabels = {
  current: "Current",
  outdated: "Outdated",
  attention: "Needs attention",
} as const;

function inventorySummary(client: ClientStatusView) {
  const harnesses = client.harnesses ?? [];
  const packages = client.packages ?? [];
  const reconciled = harnesses.filter(
    (item) => typeof item === "string" || item.reconciled,
  ).length;
  const applied = packages.filter((item) => item.state === "applied").length;
  return { harnesses, packages, reconciled, applied };
}

function IssueIndicator({ label }: { label: string }) {
  return (
    <span className="inline-flex text-destructive" title={label}>
      <CircleAlert aria-hidden="true" />
      <span className="sr-only">{label}</span>
    </span>
  );
}

function StatusBadge({ label, tone }: { label: string; tone: "healthy" | "warning" | "destructive" | "neutral" }) {
  return (
    <Badge
      variant={tone === "destructive" ? "destructive" : "outline"}
      className={cn(
        tone === "healthy" && "border-emerald-300 bg-emerald-100 text-emerald-800 dark:border-emerald-800 dark:bg-emerald-950 dark:text-emerald-300",
        tone === "warning" && "border-amber-300 bg-amber-100 text-amber-900 dark:border-amber-800 dark:bg-amber-950 dark:text-amber-300",
      )}
    >
      {label}
    </Badge>
  );
}

function ConfigDetails({ client, currentRevision }: { client: ClientStatusView; currentRevision?: string }) {
  const details = Array.from(new Set([
    revisionNotice(client, currentRevision),
    !client.applied ? "Configuration has not been applied." : undefined,
    !client.files_ok ? "Managed configuration is missing or has drifted." : undefined,
    client.error,
    ...client.status_reasons,
  ].filter((detail): detail is string => Boolean(detail))));
  const tone = client.status === "attention" ? "destructive" : client.status === "outdated" ? "warning" : "healthy";

  return (
    <TableRow className="align-top hover:bg-background/25">
      <TableCell className="whitespace-normal">
        <div className="font-medium">Managed configuration</div>
        <div className="select-all break-all font-mono text-xs text-muted-foreground">{client.instance_id}</div>
      </TableCell>
      <TableCell className="whitespace-normal font-mono text-xs">{client.config_revision ?? "Not reported"}</TableCell>
      <TableCell className="whitespace-normal">
        <div>{currentRevision ? `Current revision ${currentRevision}` : "Current revision unavailable"}</div>
        <div className="text-xs text-muted-foreground">{client.files_ok ? "Managed files verified" : "Managed files not verified"}</div>
        <div className="text-xs text-muted-foreground">First seen {client.first_seen_label}</div>
      </TableCell>
      <TableCell className="whitespace-normal">
        <StatusBadge label={statusLabels[client.status]} tone={tone} />
        {details.length > 0 && (
          <ul className="mt-2 list-disc space-y-1 pl-4 text-xs text-destructive">
            {details.map((detail) => <li key={detail} className="break-words">{detail}</li>)}
          </ul>
        )}
      </TableCell>
    </TableRow>
  );
}

function HarnessDetails({ client, currentRevision, value }: { client: ClientStatusView; currentRevision?: string; value: Array<HarnessInventory | string> }) {
  return (
    <div className="overflow-x-auto">
      <Table className="min-w-3xl">
        <TableHeader className="bg-background/35 text-muted-foreground"><TableRow><TableHead>Item</TableHead><TableHead>Installed version</TableHead><TableHead>Configuration</TableHead><TableHead>Status</TableHead></TableRow></TableHeader>
        <TableBody>
          <ConfigDetails client={client} currentRevision={currentRevision} />
          {value.map((item) => {
            if (typeof item === "string") return (
              <TableRow key={item} className="align-top hover:bg-background/25">
                <TableCell className="font-medium capitalize">{item}</TableCell><TableCell>—</TableCell>
                <TableCell className="whitespace-normal text-xs text-muted-foreground">Legacy applied-harness report</TableCell>
                <TableCell><StatusBadge label="Reconciled" tone="healthy" /></TableCell>
              </TableRow>
            );
            const tone = item.compatibility_error ? "destructive" : item.reconciled ? "healthy" : item.api_allowed ? "warning" : "neutral";
            return (
              <TableRow key={item.name} className="align-top hover:bg-background/25">
                <TableCell className="max-w-72 whitespace-normal"><div className="font-medium capitalize">{item.name}</div><div className="break-all text-xs text-muted-foreground">{item.path ?? "Not found on PATH"}</div></TableCell>
                <TableCell>{item.version ?? "—"}</TableCell><TableCell className="whitespace-normal">{item.compatibility_profile ?? "—"}</TableCell>
                <TableCell className="max-w-96 whitespace-normal"><StatusBadge label={harnessState(item)} tone={tone} />{item.compatibility_error && <p className="mt-2 break-words text-xs text-destructive">{item.compatibility_error}</p>}</TableCell>
              </TableRow>
            );
          })}
        </TableBody>
      </Table>
    </div>
  );
}

function PackageDetails({ value }: { value: PackageInventory[] }) {
  if (value.length === 0) return <div className="flex min-h-32 items-center justify-center text-sm text-muted-foreground">No extensions reported.</div>;
  return (
    <div className="overflow-x-auto">
      <Table className="min-w-3xl">
        <TableHeader className="bg-background/35 text-muted-foreground"><TableRow><TableHead>Extension</TableHead><TableHead>Harness</TableHead><TableHead>Version</TableHead><TableHead>Digest</TableHead><TableHead>Status</TableHead></TableRow></TableHeader>
        <TableBody>{value.map((item, index) => (
          <TableRow key={`${item.harness}:${item.id}:${item.state}:${index}`} className="align-top hover:bg-background/25">
            <TableCell className="font-medium">{item.id}</TableCell><TableCell className="capitalize">{item.harness}</TableCell><TableCell>{item.version}</TableCell>
            <TableCell className="max-w-64 truncate font-mono text-xs" title={item.sha256}>{item.sha256 ?? "—"}</TableCell>
            <TableCell className="max-w-96 whitespace-normal"><StatusBadge label={item.state} tone={item.state === "applied" && !item.error ? "healthy" : "destructive"} />{item.error && <p className="mt-2 break-words text-xs text-destructive">{item.error}</p>}</TableCell>
          </TableRow>
        ))}</TableBody>
      </Table>
    </div>
  );
}

export function ClientInventoryTable({
  clients,
  currentRevision,
  lastSeenSortHref,
  sortAscending,
}: {
  clients: ClientStatusView[];
  currentRevision?: string;
  lastSeenSortHref: string;
  sortAscending: boolean;
}) {
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set());
  const [confirmation, setConfirmation] = useState<ConfirmationAction>();

  function toggle(key: string) {
    setExpanded((current) => {
      const next = new Set(current);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }

  return (
    <>
    <Table className="min-w-5xl [&_td:first-child]:pl-4 [&_td:last-child]:pr-4 [&_th:first-child]:pl-4 [&_th:last-child]:pr-4 sm:[&_td:first-child]:pl-6 sm:[&_td:last-child]:pr-6 sm:[&_th:first-child]:pl-6 sm:[&_th:last-child]:pr-6 lg:[&_td:first-child]:pl-8 lg:[&_td:last-child]:pr-8 lg:[&_th:first-child]:pl-8 lg:[&_th:last-child]:pr-8">
      <TableHeader className="bg-background/35 text-muted-foreground">
        <TableRow>
          <TableHead className="w-10"><span className="sr-only">Details</span></TableHead>
          <TableHead>Client</TableHead>
          <TableHead>User</TableHead>
          <TableHead>Harnesses</TableHead>
          <TableHead>Packages</TableHead>
          <TableHead>Revision</TableHead>
          <TableHead>State</TableHead>
          <TableHead aria-sort={sortAscending ? "ascending" : "descending"}>
            <Link
              href={lastSeenSortHref}
              className="inline-flex items-center gap-1 rounded-sm hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
            >
              Last seen {sortAscending ? <ArrowUp className="size-3.5" /> : <ArrowDown className="size-3.5" />}
            </Link>
          </TableHead>
          <TableHead className="text-center">Actions</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {clients.map((client, index) => {
          const key = `${client.user_email}:${client.instance_id}`;
          const detailId = `client-inventory-${index}-${key.replace(/[^a-zA-Z0-9_-]/g, "-")}`;
          const isExpanded = expanded.has(key);
          const summary = inventorySummary(client);
          const harnessIssue =
            client.status !== "current" ||
            summary.harnesses.some(
              (item) =>
                typeof item !== "string" &&
                Boolean(item.compatibility_error),
            );
          const extensionIssue = summary.packages.some(
            (item) => item.state !== "applied" || Boolean(item.error),
          );
          return (
            <Fragment key={key}>
              <TableRow aria-expanded={isExpanded} className="h-12 hover:bg-background/25">
                <TableCell>
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon-sm"
                    onClick={() => toggle(key)}
                    aria-expanded={isExpanded}
                    aria-controls={detailId}
                    aria-label={`${isExpanded ? "Hide" : "Show"} inventory for ${client.hostname ?? client.instance_id}`}
                  >
                    <ChevronRight className={cn("transition-transform", isExpanded && "rotate-90")} />
                  </Button>
                </TableCell>
                <TableCell className="max-w-56 whitespace-normal">
                  <div className="truncate font-medium" title={client.hostname ?? client.instance_id}>
                    {client.hostname ?? client.instance_id.slice(0, 8)}
                  </div>
                  <div className="truncate text-xs text-muted-foreground" title={`${client.platform}${client.architecture ? `/${client.architecture}` : ""} · v${client.client_version}`}>
                    {client.platform}{client.architecture ? `/${client.architecture}` : ""} · v{client.client_version}
                  </div>
                </TableCell>
                <TableCell className="max-w-56 truncate" title={client.user_email}>
                  {client.user_email}
                </TableCell>
                <TableCell>
                  <div className="font-medium">{summary.harnesses.length.toLocaleString()} reported</div>
                  <div className="text-xs text-muted-foreground">
                    {summary.reconciled.toLocaleString()} reconciled
                  </div>
                </TableCell>
                <TableCell>
                  <div className="font-medium">{summary.packages.length.toLocaleString()} reported</div>
                  <div className="text-xs text-muted-foreground">
                    {summary.applied.toLocaleString()} applied
                  </div>
                </TableCell>
                <TableCell className="max-w-40 truncate font-mono text-xs" title={client.config_revision}>
                  {client.config_revision ?? "—"}
                </TableCell>
                <TableCell>
                  <Badge
                    variant={
                      client.status === "attention" ? "destructive" : "outline"
                    }
                    className={cn(
                      client.status === "current" &&
                        "border-emerald-300 bg-emerald-100 text-emerald-800 dark:border-emerald-800 dark:bg-emerald-950 dark:text-emerald-300",
                      client.status === "outdated" &&
                        "border-amber-300 bg-amber-100 text-amber-900 dark:border-amber-800 dark:bg-amber-950 dark:text-amber-300",
                    )}
                  >
                    {statusLabels[client.status]}
                  </Badge>
                </TableCell>
                <TableCell>
                  <time dateTime={client.last_seen_at}>{client.last_seen_label}</time>
                  {client.activity_status === "stale" && <div><Badge variant="outline" className="mt-1">Stale</Badge></div>}
                </TableCell>
                <TableCell className="text-center">
                  <DropdownMenu>
                    <DropdownMenuTrigger
                      render={
                        <Button
                          type="button"
                          variant="ghost"
                          size="icon-sm"
                          aria-label={`Actions for ${client.hostname ?? client.instance_id}`}
                        />
                      }
                    >
                      <Ellipsis />
                    </DropdownMenuTrigger>
                    <DropdownMenuContent align="end" className="w-40">
                      <DropdownMenuItem
                        variant="destructive"
                        onClick={() => setConfirmation({
                          action: removeClientStatus,
                          fields: { client_id: client.id },
                          title: "Remove client record?",
                          description: `This removes the inventory record for ${client.hostname ?? client.instance_id}. It does not revoke access, and an active client will reappear when it next reports. First seen ${client.first_seen_label}.`,
                          confirmLabel: "Remove client",
                          destructive: true,
                        })}
                      >
                        Delete
                      </DropdownMenuItem>
                    </DropdownMenuContent>
                  </DropdownMenu>
                </TableCell>
              </TableRow>
              {isExpanded && (
                <TableRow id={detailId}>
                  <TableCell colSpan={9} className="!p-0 whitespace-normal bg-muted/30">
                    <div className="py-4">
                    <Tabs defaultValue="harnesses" className="gap-0">
                      <div className="overflow-x-auto border-b">
                        <TabsList variant="line" className="min-w-max px-1">
                          <TabsTrigger value="harnesses" className="gap-2 px-3 py-2">
                            Harnesses
                            <Badge variant="secondary" className="min-w-5 justify-center px-1.5">{summary.harnesses.length}</Badge>
                            {harnessIssue && <IssueIndicator label="Harnesses contain issues" />}
                          </TabsTrigger>
                          <TabsTrigger value="extensions" className="gap-2 px-3 py-2">
                            Extensions
                            <Badge variant="secondary" className="min-w-5 justify-center px-1.5">{summary.packages.length}</Badge>
                            {extensionIssue && <IssueIndicator label="Extensions contain issues" />}
                          </TabsTrigger>
                        </TabsList>
                      </div>
                      <TabsContent value="harnesses">
                        <HarnessDetails client={client} currentRevision={currentRevision} value={summary.harnesses} />
                      </TabsContent>
                      <TabsContent value="extensions">
                        <PackageDetails value={summary.packages} />
                      </TabsContent>
                    </Tabs>
                    </div>
                  </TableCell>
                </TableRow>
              )}
            </Fragment>
          );
        })}
      </TableBody>
    </Table>
    <ConfirmationDialog confirmation={confirmation} open={Boolean(confirmation)} onOpenChange={(open) => { if (!open) setConfirmation(undefined); }} />
    </>
  );
}
