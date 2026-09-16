import Link from "next/link";
import { notFound, redirect } from "next/navigation";
import { AlertCircle, ArrowDown, ArrowUp, Search, SlidersHorizontal, X } from "lucide-react";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle, DialogTrigger } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { TablePaginationFooter } from "@/components/table-pagination-footer";
import { AsyncUserSelect } from "@/components/async-user-select";
import { api, requireIdentity } from "@/lib/api";
import { RefreshButton } from "./refresh-button";
import { GatewayTabs, type GatewayTab } from "./gateway-tabs";
import { ProxyHealthRow } from "./proxy-health-row";
import { GatewayKeyForm } from "./gateway-key-form";

type GatewayStatus = {
  enabled: boolean;
  runtime_configured: boolean;
  inference_proxy_url?: string;
  upstream_gateway_url?: string;
  provisioner_type?: string;
  harnesses?: Array<{ name: string; gateway_type: string }>;
};

type GatewayAccess = {
  enabled: boolean;
  email: string;
  status: "ready" | "missing" | "invalid" | "recovering" | "error" | "disabled";
  alias?: string | null;
  external_id?: string | null;
  expires_at?: string | null;
  last_reconciled_at?: string | null;
  error?: string | null;
  invalidated_at?: string | null;
  invalidation_reason?: string | null;
  next_retry_at?: string | null;
};

type RequestLog = {
  id: string;
  user_id: string;
  user_email: string;
  profile_id?: string | null;
  profile_name?: string | null;
  occurred_at: string;
  method: string;
  path: string;
  model?: string | null;
  http_status?: number | null;
  result: "success" | "redirect" | "client_error" | "server_error" | "transport_error";
  upstream_latency_ms: number;
  harness?: string | null;
  repository?: string | null;
  branch?: string | null;
  commit_sha?: string | null;
  dirty?: boolean | null;
  run_id?: string | null;
};

type RequestLogPage = { items: RequestLog[]; page: number; per_page: number; total: number; total_pages: number };
type RequestLogFacets = {
  users: Array<{ id: string; email: string }>;
  profiles: Array<{ id: string; name?: string | null }>;
  models: string[];
  harnesses: string[];
  results: string[];
};
type SearchParams = Record<string, string | string[] | undefined>;

const pageSizes = new Set([25, 50, 75, 100]);
const resultLabels: Record<string, string> = {
  success: "Success",
  redirect: "Redirect",
  client_error: "Client error",
  server_error: "Server error",
  transport_error: "Transport error",
};

function value(params: SearchParams, key: string) {
  const current = params[key];
  return Array.isArray(current) ? current[0] ?? "" : current ?? "";
}

function selected(params: SearchParams, key: string) {
  const current = value(params, key);
  return current === "all" ? "" : current;
}

function positive(value: string, fallback: number) {
  const parsed = Number(value);
  return Number.isInteger(parsed) && parsed > 0 ? parsed : fallback;
}

function date(value: string) {
  const parsed = new Date(value);
  return Number.isNaN(parsed.getTime()) ? value : new Intl.DateTimeFormat("en-US", { dateStyle: "medium" }).format(parsed);
}

function errorMessage(error: unknown, fallback: string) {
  return error instanceof Error
    ? error.message.replace(/^\d+:\s*/, "").replace(/^\{"error":"(.*)"\}$/, "$1")
    : fallback;
}

function hrefWith(current: Record<string, string>, changes: Record<string, string | number | undefined>) {
  const params = new URLSearchParams(current);
  for (const [key, next] of Object.entries(changes)) {
    if (next === undefined || next === "" || (key === "page" && next === 1)) params.delete(key);
    else params.set(key, String(next));
  }
  const query = params.toString();
  return query ? `/gateway?${query}` : "/gateway";
}

function resultBadge(result: RequestLog["result"]) {
  if (result === "success") return "default" as const;
  if (result === "redirect") return "secondary" as const;
  if (result === "client_error") return "outline" as const;
  return "destructive" as const;
}

export default async function GatewayPage({ searchParams }: { searchParams: Promise<SearchParams> }) {
  const [raw, me] = await Promise.all([searchParams, requireIdentity()]);
  const requestedTab = value(raw, "tab");
  const hasLogQuery = ["q", "user_id", "profile_id", "model", "harness", "result", "occurred_from", "occurred_to", "sort", "page", "per_page"].some((key) => Boolean(value(raw, key)));
  if (me.role !== "admin" && ((requestedTab && requestedTab !== "keys") || hasLogQuery)) notFound();
  const activeTab: GatewayTab = me.role !== "admin"
    ? "keys"
    : requestedTab === "keys" || requestedTab === "logs"
      ? requestedTab
      : hasLogQuery
        ? "logs"
        : "overview";
  const requestedPage = positive(value(raw, "page"), 1);
  const requestedSize = positive(value(raw, "per_page"), 25);
  const perPage = pageSizes.has(requestedSize) ? requestedSize : 25;
  const filters = {
    q: value(raw, "q").trim(),
    user_id: selected(raw, "user_id"),
    profile_id: selected(raw, "profile_id"),
    model: selected(raw, "model"),
    harness: selected(raw, "harness"),
    result: selected(raw, "result"),
    occurred_from: value(raw, "occurred_from"),
    occurred_to: value(raw, "occurred_to"),
    sort: value(raw, "sort") === "occurred_asc" ? "occurred_asc" : "occurred_desc",
  };
  const currentParams: Record<string, string> = {};
  for (const [key, current] of Object.entries({ ...filters, page: String(requestedPage), per_page: String(perPage) })) {
    if (current && !(key === "page" && current === "1") && !(key === "per_page" && current === "25") && !(key === "sort" && current === "occurred_desc")) currentParams[key] = current;
  }
  currentParams.tab = "logs";

  const status = await api<GatewayStatus>("/gateway/status");
  if (!status.enabled) notFound();

  let access: GatewayAccess | undefined;
  let accessError: string | undefined;
  let validationError: string | undefined;
  if (status.runtime_configured) {
    try { access = await api<GatewayAccess>("/gateway/key"); }
    catch (error) { accessError = errorMessage(error, "Gateway access could not be loaded."); }
    if (activeTab === "keys" && access?.status === "ready") {
      try { access = await api<GatewayAccess>("/gateway/key/validate", { method: "POST", body: "{}" }); }
      catch (error) { validationError = errorMessage(error, "The upstream key could not be verified."); }
    }
  }

  const query = new URLSearchParams({ page: String(requestedPage), per_page: String(perPage), sort: filters.sort });
  for (const key of ["q", "user_id", "profile_id", "model", "harness", "result", "occurred_from", "occurred_to"] as const) {
    if (filters[key]) query.set(key, filters[key]);
  }
  let logs: RequestLogPage = { items: [], page: 1, per_page: perPage, total: 0, total_pages: 0 };
  let facets: RequestLogFacets = { users: [], profiles: [], models: [], harnesses: [], results: [] };
  let logError: string | undefined;
  if (me.role === "admin") {
    try {
      [logs, facets] = await Promise.all([
        api<RequestLogPage>(`/gateway/request-logs?${query}`),
        api<RequestLogFacets>(`/gateway/request-logs/facets${filters.user_id ? `?selected_user_id=${encodeURIComponent(filters.user_id)}` : ""}`),
      ]);
    } catch (error) {
      logError = errorMessage(error, "Request logs could not be loaded.");
    }
  }
  if (!logError && logs.total > 0 && requestedPage > logs.total_pages) redirect(hrefWith(currentParams, { page: logs.total_pages }));

  const profileLabel = facets.profiles.find((profile) => profile.id === filters.profile_id)?.name;
  const userLabel = facets.users.find((user) => user.id === filters.user_id)?.email;
  const activeFilters = [
    filters.q && { key: "q", label: `Search: ${filters.q}` },
    filters.user_id && { key: "user_id", label: `User: ${userLabel ?? "Unknown"}` },
    filters.profile_id && { key: "profile_id", label: `Key: ${profileLabel ?? filters.profile_id}` },
    filters.model && { key: "model", label: `Model: ${filters.model}` },
    filters.harness && { key: "harness", label: `Harness: ${filters.harness}` },
    filters.result && { key: "result", label: `Result: ${resultLabels[filters.result] ?? filters.result}` },
    filters.occurred_from && { key: "occurred_from", label: `From: ${filters.occurred_from}` },
    filters.occurred_to && { key: "occurred_to", label: `To: ${filters.occurred_to}` },
  ].filter(Boolean) as Array<{ key: string; label: string }>;
  const hasFilters = activeFilters.length > 0;
  const ascending = filters.sort === "occurred_asc";

  return (
    <div className="flex flex-col gap-8">
      {!status.runtime_configured && (
        <Alert variant="destructive">
          <AlertCircle /><AlertTitle>Gateway setup is incomplete</AlertTitle>
          <AlertDescription>{me.role === "admin" ? "Complete the missing gateway runtime settings." : "An administrator must complete the gateway runtime settings."}</AlertDescription>
        </Alert>
      )}
      {accessError && <Alert variant="destructive"><AlertCircle /><AlertTitle>Gateway access unavailable</AlertTitle><AlertDescription>{accessError}</AlertDescription></Alert>}

      <GatewayTabs
        admin={me.role === "admin"}
        activeTab={activeTab}
        overview={(
      <section aria-labelledby="gateway-overview" className="overflow-hidden rounded-xl border">
        <h2 id="gateway-overview" className="sr-only">Overview</h2>
        <Table>
          <TableBody>
            <TableRow><TableCell className="w-48 text-muted-foreground">Connection</TableCell><TableCell className="font-medium">{status.runtime_configured ? "Connected" : "Not configured"}</TableCell></TableRow>
            <TableRow><TableCell className="text-muted-foreground">Gateway type</TableCell><TableCell className="capitalize">{status.harnesses?.[0]?.gateway_type ?? "—"}</TableCell></TableRow>
            <TableRow><TableCell className="text-muted-foreground">Gateway URL</TableCell><TableCell className="break-all whitespace-normal font-mono text-xs">{status.upstream_gateway_url ?? "—"}</TableCell></TableRow>
            <TableRow><TableCell className="text-muted-foreground">Provisioner</TableCell><TableCell className="break-all whitespace-normal font-mono text-xs">{status.provisioner_type ?? "—"}</TableCell></TableRow>
            <TableRow><TableCell className="text-muted-foreground">Proxy URL</TableCell><TableCell className="break-all whitespace-normal font-mono text-xs">{status.inference_proxy_url ?? "—"}</TableCell></TableRow>
            <ProxyHealthRow configured={Boolean(status.inference_proxy_url)} />
            <TableRow><TableCell className="text-muted-foreground">Routed harnesses</TableCell><TableCell className="whitespace-normal capitalize">{status.harnesses?.map((item) => item.name).join(", ") || "—"}</TableCell></TableRow>
          </TableBody>
        </Table>
      </section>
        )}
        keys={(
      <section aria-labelledby="gateway-keys" className="overflow-hidden rounded-xl border">
        <h2 id="gateway-keys" className="sr-only">Managed gateway key</h2>
        <Table>
          <TableBody>
            <TableRow><TableCell className="w-48 text-muted-foreground">Status</TableCell><TableCell><Badge variant={access?.status === "ready" ? "default" : access?.status === "invalid" || access?.status === "error" ? "destructive" : "outline"}>{access?.status ?? "Unavailable"}</Badge></TableCell></TableRow>
            <TableRow><TableCell className="text-muted-foreground">Alias</TableCell><TableCell className="break-all whitespace-normal font-mono text-xs">{access?.alias ?? "—"}</TableCell></TableRow>
            <TableRow><TableCell className="text-muted-foreground">Last reconciled</TableCell><TableCell>{access?.last_reconciled_at ? new Date(access.last_reconciled_at).toLocaleString() : "—"}</TableCell></TableRow>
            <TableRow><TableCell className="text-muted-foreground">Expires</TableCell><TableCell>{access?.expires_at ? date(access.expires_at) : "—"}</TableCell></TableRow>
            {access?.next_retry_at && <TableRow><TableCell className="text-muted-foreground">Retry available</TableCell><TableCell>{new Date(access.next_retry_at).toLocaleString()}</TableCell></TableRow>}
          </TableBody>
        </Table>
        {access?.error && <div className="border-t p-4"><Alert variant="destructive"><AlertCircle /><AlertTitle>Provisioning failed</AlertTitle><AlertDescription>{access.error}</AlertDescription></Alert></div>}
        {validationError && <div className="border-t p-4"><Alert><AlertCircle /><AlertTitle>Key verification unavailable</AlertTitle><AlertDescription>{validationError} The stored key was left unchanged.</AlertDescription></Alert></div>}
        {access?.status === "invalid" && <div className="border-t p-4"><Alert variant="destructive"><AlertCircle /><AlertTitle>Upstream credential is invalid</AlertTitle><AlertDescription>{access.invalidation_reason ?? "The credential was rejected or revoked by the upstream gateway."} Provision a new credential to restore gateway access.</AlertDescription></Alert></div>}
        <GatewayKeyForm label={access?.status === "invalid" ? "Provision new key" : access?.status === "missing" ? "Provision key" : access?.status === "recovering" ? "Recovering…" : "Reconcile key"} disabled={access?.status === "recovering" || Boolean(access?.next_retry_at && new Date(access.next_retry_at).getTime() > Date.now())} existingError={access?.error} />
      </section>
        )}
        logs={(
      <section aria-labelledby="gateway-requests" className="overflow-hidden rounded-xl border">
        <h2 id="gateway-requests" className="sr-only">Proxy requests</h2>
        <div className="grid gap-4 border-b px-4 py-4 sm:px-6">
          <div className="flex justify-end"><RefreshButton /></div>
          <div className="flex min-w-0 flex-col gap-3 sm:flex-row">
            <form action="/gateway" className="flex min-w-0 flex-1 gap-2">
              <input type="hidden" name="tab" value="logs" />
              {Object.entries(filters).map(([key, current]) => key !== "q" && current ? <input key={key} type="hidden" name={key} value={current} /> : null)}
              <input type="hidden" name="per_page" value={perPage} />
              <div className="relative min-w-0 max-w-md flex-1"><Search className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" /><Input name="q" defaultValue={filters.q} maxLength={200} placeholder="Search requests" className="pl-8" aria-label="Search proxy requests" /></div>
              <Button type="submit" variant="secondary">Search</Button>
            </form>
            <Dialog>
              <DialogTrigger render={<Button variant="outline" />}><SlidersHorizontal /> Filters{activeFilters.length ? ` (${activeFilters.length})` : ""}</DialogTrigger>
              <DialogContent className="max-h-[calc(100svh-2rem)] overflow-y-auto sm:max-w-xl">
                <DialogHeader><DialogTitle>Filter proxy requests</DialogTitle></DialogHeader>
                <form action="/gateway" className="grid gap-4 sm:grid-cols-2">
                  <input type="hidden" name="tab" value="logs" />
                  <input type="hidden" name="q" value={filters.q} /><input type="hidden" name="per_page" value={perPage} />
                  {me.role === "admin" && <div className="grid min-w-0 gap-2"><Label htmlFor="log-user">User</Label><AsyncUserSelect id="log-user" name="user_id" source="gateway" value={filters.user_id} initialUsers={facets.users} /></div>}
                  <FilterSelect id="log-profile" label="Gateway key" name="profile_id" value={filters.profile_id} options={facets.profiles.map((item) => ({ value: item.id, label: item.name ?? item.id }))} />
                  <FilterSelect id="log-model" label="Model" name="model" value={filters.model} options={facets.models.map((item) => ({ value: item, label: item }))} />
                  <FilterSelect id="log-harness" label="Harness" name="harness" value={filters.harness} options={facets.harnesses.map((item) => ({ value: item, label: item }))} />
                  <FilterSelect id="log-result" label="Result" name="result" value={filters.result} options={facets.results.map((item) => ({ value: item, label: resultLabels[item] ?? item }))} />
                  <div className="grid gap-2"><Label htmlFor="log-from">From</Label><Input id="log-from" name="occurred_from" type="date" defaultValue={filters.occurred_from} /></div>
                  <div className="grid gap-2"><Label htmlFor="log-to">To</Label><Input id="log-to" name="occurred_to" type="date" defaultValue={filters.occurred_to} /></div>
                  <div className="grid gap-2"><Label htmlFor="log-sort">Order</Label><Select name="sort" defaultValue={filters.sort}><SelectTrigger id="log-sort" className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectItem value="occurred_desc">Newest first</SelectItem><SelectItem value="occurred_asc">Oldest first</SelectItem></SelectContent></Select></div>
                  <DialogFooter className="sm:col-span-2"><Button type="submit">Apply filters</Button>{(hasFilters || ascending) && <Button variant="outline" render={<Link href="/gateway?tab=logs" />}>Clear all</Button>}</DialogFooter>
                </form>
              </DialogContent>
            </Dialog>
          </div>
          {activeFilters.length > 0 && <div className="flex min-w-0 flex-wrap gap-2" aria-label="Applied filters">{activeFilters.map((filter) => <Link key={filter.key} href={hrefWith(currentParams, { [filter.key]: undefined, page: 1 })} title={filter.label} className="inline-flex h-7 max-w-full min-w-0 items-center gap-1 rounded-md border bg-muted/40 px-2.5 text-xs text-muted-foreground hover:bg-muted hover:text-foreground" aria-label={`Remove ${filter.label}`}><span className="truncate">{filter.label}</span><X className="size-3 shrink-0" /></Link>)}</div>}
        </div>
        {logError ? (
          <div className="p-4 sm:p-6"><Alert variant="destructive"><AlertCircle /><AlertTitle>Request logs unavailable</AlertTitle><AlertDescription>{logError}</AlertDescription></Alert></div>
        ) : (
          <>
            <div className="overflow-x-auto">
              {logs.items.length === 0 ? <div className="grid min-h-48 place-items-center px-4 text-center"><div><p className="font-medium">{hasFilters ? "No matching requests" : "No proxy requests yet"}</p>{hasFilters && <Button className="mt-3" variant="outline" render={<Link href="/gateway?tab=logs" />}>Clear filters</Button>}</div></div> : (
                <Table className="min-w-5xl">
                  <TableHeader><TableRow>{me.role === "admin" && <TableHead>User</TableHead>}<TableHead>Key</TableHead><TableHead>Request</TableHead><TableHead>Model</TableHead><TableHead>Context</TableHead><TableHead>Result</TableHead><TableHead>Latency</TableHead><TableHead aria-sort={ascending ? "ascending" : "descending"}><Link href={hrefWith(currentParams, { sort: ascending ? "occurred_desc" : "occurred_asc", page: 1 })} className="inline-flex items-center gap-1 hover:underline">Time {ascending ? <ArrowUp className="size-3.5" /> : <ArrowDown className="size-3.5" />}</Link></TableHead></TableRow></TableHeader>
                  <TableBody>{logs.items.map((item) => <TableRow key={item.id}>{me.role === "admin" && <TableCell className="max-w-52 truncate" title={item.user_email}>{item.user_email}</TableCell>}<TableCell>{item.profile_name ?? item.profile_id ?? "—"}</TableCell><TableCell><span className="mr-2 font-mono text-xs font-semibold">{item.method}</span><span className="font-mono text-xs">{item.path}</span></TableCell><TableCell>{item.model ?? "—"}</TableCell><TableCell><div>{item.harness ?? "—"}</div>{item.repository && <div className="max-w-56 truncate text-xs text-muted-foreground" title={item.repository}>{item.repository}{item.branch ? ` · ${item.branch}` : ""}</div>}</TableCell><TableCell><Badge variant={resultBadge(item.result)}>{item.http_status ?? resultLabels[item.result]}</Badge></TableCell><TableCell>{item.upstream_latency_ms.toLocaleString()} ms</TableCell><TableCell><time dateTime={item.occurred_at}>{new Date(item.occurred_at).toLocaleString()}</time></TableCell></TableRow>)}</TableBody>
                </Table>
              )}
            </div>
            <TablePaginationFooter pathname="/gateway" params={currentParams} page={logs.page} perPage={logs.per_page} total={logs.total} label="Proxy request" />
          </>
        )}
      </section>
        )}
      />
    </div>
  );
}

function FilterSelect({ id, label, name, value: current, options }: { id: string; label: string; name: string; value: string; options: Array<{ value: string; label: string }> }) {
  return <div className="grid min-w-0 gap-2"><Label htmlFor={id}>{label}</Label><Select name={name} defaultValue={current || "all"}><SelectTrigger id={id} className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectItem value="all">All</SelectItem>{options.map((option) => <SelectItem key={option.value} value={option.value} title={option.label}>{option.label}</SelectItem>)}</SelectContent></Select></div>;
}
