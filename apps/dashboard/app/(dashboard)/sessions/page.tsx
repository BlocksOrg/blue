import Link from "next/link";
import { redirect } from "next/navigation";
import {
  ArrowDown,
  ArrowUp,
  ChevronLeft,
  ChevronRight,
  ChevronsLeft,
  ChevronsRight,
  Search,
  SlidersHorizontal,
  X,
} from "lucide-react";
import { api, requireIdentity } from "../../../lib/api";
import { Badge } from "@/components/ui/badge";
import { Button, buttonVariants } from "@/components/ui/button";
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
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { cn } from "@/lib/utils";
import { PageSizeSelect } from "@/components/page-size-select";
import { AsyncUserSelect } from "@/components/async-user-select";
import { SessionActions, SessionSharingSummary } from "./session-sharing";

type Session = {
  id: string;
  user_id: string;
  harness: string;
  compatibility_profile: string;
  native_session_id: string;
  user_email: string;
  cwd?: string;
  updated_at: string;
  status?: "pending" | "complete" | "superseded" | "failed";
  size_bytes?: number;
  retention_expires_at?: string;
  artifact_format: string;
  resumable: boolean;
  title?: string;
  summary?: string;
  sharing_mode: "private" | "workspace" | "selected";
  share_recipient_count: number;
};

type SessionPage = {
  items: Session[];
  page: number;
  per_page: number;
  total: number;
  total_pages: number;
};

type Facets = {
  users: { id: string; email: string }[];
  harnesses: string[];
};

type SearchParams = Record<string, string | string[] | undefined>;

const allowedPageSizes = new Set([25, 50, 75, 100]);

function value(params: SearchParams, name: string) {
  const current = params[name];
  return Array.isArray(current) ? current[0] ?? "" : current ?? "";
}

function selection(params: SearchParams, name: string) {
  const selected = value(params, name);
  return selected === "all" ? "" : selected;
}

function positiveInteger(raw: string, fallback: number) {
  const parsed = Number.parseInt(raw, 10);
  return Number.isSafeInteger(parsed) && parsed > 0 ? parsed : fallback;
}

function hrefWith(
  current: Record<string, string>,
  changes: Record<string, string | number | undefined>,
) {
  const params = new URLSearchParams(current);
  for (const [key, next] of Object.entries(changes)) {
    if (next === undefined || next === "" || next === "all") params.delete(key);
    else params.set(key, String(next));
  }
  const query = params.toString();
  return query ? `/sessions?${query}` : "/sessions";
}

function pageItems(current: number, total: number): (number | "ellipsis")[] {
  if (total <= 7) return Array.from({ length: total }, (_, index) => index + 1);
  if (current <= 4) return [1, 2, 3, 4, 5, "ellipsis", total];
  if (current >= total - 3)
    return [1, "ellipsis", total - 4, total - 3, total - 2, total - 1, total];
  return [1, "ellipsis", current - 1, current, current + 1, "ellipsis", total];
}

function formatBytes(bytes?: number) {
  if (bytes === undefined) return "—";
  if (bytes < 1024) return `${bytes.toLocaleString()} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let size = bytes / 1024;
  let unit = 0;
  while (size >= 1024 && unit < units.length - 1) {
    size /= 1024;
    unit += 1;
  }
  return `${size.toLocaleString(undefined, { maximumFractionDigits: 1 })} ${units[unit]}`;
}

function retention(expiresAt?: string) {
  if (!expiresAt) return { label: "Not set", variant: "outline" as const };
  const days = Math.ceil((new Date(expiresAt).getTime() - Date.now()) / 86_400_000);
  if (days < 0) return { label: "Expired", variant: "destructive" as const };
  if (days === 0) return { label: "Expires today", variant: "destructive" as const };
  if (days <= 7)
    return { label: `Expires in ${days}d`, variant: "destructive" as const };
  if (days <= 30)
    return { label: `Expires in ${days}d`, variant: "secondary" as const };
  return { label: new Date(expiresAt).toLocaleDateString(), variant: "outline" as const };
}

export default async function Sessions({ searchParams }: { searchParams: Promise<SearchParams> }) {
  const raw = await searchParams;
  const requestedPage = positiveInteger(value(raw, "page"), 1);
  const requestedSize = positiveInteger(value(raw, "per_page"), 25);
  const perPage = allowedPageSizes.has(requestedSize) ? requestedSize : 25;
  const filters = {
    q: value(raw, "q").trim(),
    user_id: selection(raw, "user_id"),
    harness: selection(raw, "harness"),
    updated_from: value(raw, "updated_from"),
    updated_to: value(raw, "updated_to"),
    sort: value(raw, "sort") === "updated_asc" ? "updated_asc" : "updated_desc",
  };
  const currentParams: Record<string, string> = {};
  for (const [key, current] of Object.entries({
    ...filters,
    page: String(requestedPage),
    per_page: String(perPage),
  })) {
    if (current && !(key === "page" && current === "1") && !(key === "per_page" && current === "25"))
      currentParams[key] = current;
  }

  const query = new URLSearchParams({
    page: String(requestedPage),
    per_page: String(perPage),
    sort: filters.sort,
  });
  for (const name of ["q", "user_id", "harness", "updated_from", "updated_to"] as const) {
    if (filters[name]) query.set(name, filters[name]);
  }

  const [data, facets, me] = await Promise.all([
    api<SessionPage>(`/session-uploads?${query}`),
    api<Facets>(`/session-uploads/facets${filters.user_id ? `?selected_user_id=${encodeURIComponent(filters.user_id)}` : ""}`),
    requireIdentity(),
  ]);
  if (data.total > 0 && requestedPage > data.total_pages) {
    redirect(hrefWith(currentParams, { page: data.total_pages }));
  }

  const userEmail = facets.users.find((user) => user.id === filters.user_id)?.email;
  const activeFilters = [
    filters.q && { key: "q", label: `Search: ${filters.q}` },
    me.role === "admin" && filters.user_id && { key: "user_id", label: `User: ${userEmail ?? "Unknown"}` },
    filters.harness && { key: "harness", label: `Harness: ${filters.harness}` },
    filters.updated_from && { key: "updated_from", label: `From: ${filters.updated_from}` },
    filters.updated_to && { key: "updated_to", label: `To: ${filters.updated_to}` },
  ].filter(Boolean) as { key: string; label: string }[];
  const hasFilters = activeFilters.length > 0;
  const first = data.total === 0 ? 0 : (data.page - 1) * data.per_page + 1;
  const last = Math.min(data.page * data.per_page, data.total);
  const sortAscending = filters.sort === "updated_asc";

  return (
    <div className="flex flex-col gap-6">
      <div className="flex min-w-0 flex-col gap-3 sm:flex-row sm:items-center">
        <form action="/sessions" className="flex min-w-0 flex-1 gap-2">
          {Object.entries(filters).map(([key, current]) =>
            key !== "q" && current ? <input key={key} type="hidden" name={key} value={current} /> : null,
          )}
          <input type="hidden" name="per_page" value={perPage} />
          <div className="relative min-w-0 max-w-md flex-1">
            <Search className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" />
            <Input name="q" defaultValue={filters.q} maxLength={200} placeholder="Search title, preview, ID, or path" className="pl-8" aria-label="Search sessions" />
          </div>
          <Button type="submit" variant="secondary">Search</Button>
        </form>
        <Dialog>
          <DialogTrigger render={<Button variant="outline" />}>
            <SlidersHorizontal /> Filters{activeFilters.length ? ` (${activeFilters.length})` : ""}
          </DialogTrigger>
          <DialogContent className="max-h-[calc(100svh-2rem)] overflow-y-auto sm:max-w-xl">
            <DialogHeader>
              <DialogTitle>Filter sessions</DialogTitle>
              <DialogDescription>Narrow the session history.</DialogDescription>
            </DialogHeader>
            <form action="/sessions" className="grid min-w-0 gap-4 sm:grid-cols-2">
            <input type="hidden" name="per_page" value={perPage} />
            <input type="hidden" name="q" value={filters.q} />
            {me.role === "admin" && (
              <div className="grid min-w-0 gap-2">
                <Label htmlFor="session-user">User</Label>
                <AsyncUserSelect id="session-user" name="user_id" source="sessions" value={filters.user_id} initialUsers={facets.users} />
              </div>
            )}
            <div className="grid min-w-0 gap-2">
              <Label htmlFor="session-harness">Harness</Label>
              <Select name="harness" defaultValue={filters.harness || "all"}>
                <SelectTrigger id="session-harness" className="w-full capitalize">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="all">All harnesses</SelectItem>
                  {facets.harnesses.map((harness) => (
                    <SelectItem key={harness} value={harness} className="capitalize">{harness}</SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="grid min-w-0 gap-2">
              <Label htmlFor="updated-from">Updated from</Label>
              <Input id="updated-from" name="updated_from" type="date" defaultValue={filters.updated_from} />
            </div>
            <div className="grid min-w-0 gap-2">
              <Label htmlFor="updated-to">Updated to</Label>
              <Input id="updated-to" name="updated_to" type="date" defaultValue={filters.updated_to} />
            </div>
            <div className="grid min-w-0 gap-2">
              <Label htmlFor="session-sort">Order</Label>
              <Select name="sort" defaultValue={filters.sort}>
                <SelectTrigger id="session-sort" className="w-full"><SelectValue /></SelectTrigger>
                <SelectContent>
                  <SelectItem value="updated_desc">Newest updated</SelectItem>
                  <SelectItem value="updated_asc">Oldest updated</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <DialogFooter className="sm:col-span-2">
              <Button type="submit">Apply filters</Button>
              {(hasFilters || sortAscending) && (
                <Button variant="outline" render={<Link href="/sessions" />}>Clear all</Button>
              )}
            </DialogFooter>
          </form>
          </DialogContent>
        </Dialog>
      </div>
      {activeFilters.length > 0 && (
        <div className="flex min-w-0 flex-wrap items-center gap-2" aria-label="Applied filters">
          {activeFilters.map((filter) => (
            <Link key={filter.key} href={hrefWith(currentParams, { [filter.key]: undefined, page: 1 })} title={filter.label} className="inline-flex h-7 max-w-full min-w-0 items-center gap-1 rounded-md border bg-muted/40 px-2.5 text-xs text-muted-foreground hover:bg-muted hover:text-foreground" aria-label={`Remove ${filter.label}`}>
              <span className="truncate">{filter.label}</span><X className="size-3 shrink-0" />
            </Link>
          ))}
        </div>
      )}

      <section className="-mx-4 overflow-hidden border-y bg-transparent sm:-mx-6 lg:-mx-8">
        <div className="overflow-x-auto">
          {data.items.length === 0 ? (
            <div className="flex min-h-48 flex-col items-center justify-center gap-3 text-center">
              <div>
                <p className="font-medium">{hasFilters ? "No matching sessions" : "No captured sessions yet"}</p>
                <p className="mt-1 text-sm text-muted-foreground">
                  {hasFilters
                    ? "Try removing a filter or broadening the date range."
                    : "Sessions will appear here after a configured harness uploads raw session data."}
                </p>
              </div>
              {hasFilters && <Button variant="outline" render={<Link href="/sessions" />}>Clear filters</Button>}
            </div>
          ) : (
            <Table className="[&_td:first-child]:pl-4 [&_td:last-child]:pr-4 [&_th:first-child]:pl-4 [&_th:last-child]:pr-4 sm:[&_td:first-child]:pl-6 sm:[&_td:last-child]:pr-6 sm:[&_th:first-child]:pl-6 sm:[&_th:last-child]:pr-6 lg:[&_td:first-child]:pl-8 lg:[&_td:last-child]:pr-8 lg:[&_th:first-child]:pl-8 lg:[&_th:last-child]:pr-8">
              <TableHeader className="bg-background/35 text-muted-foreground">
                <TableRow>
                  <TableHead>Harness</TableHead>
                  <TableHead className="w-[20%]">Session</TableHead>
                  {me.role === "admin" && <TableHead>User</TableHead>}
                  <TableHead>Artifact</TableHead>
                  <TableHead>Size</TableHead>
                  <TableHead aria-sort={sortAscending ? "ascending" : "descending"}>
                    <Link
                      href={hrefWith(currentParams, { sort: sortAscending ? "updated_desc" : "updated_asc", page: 1 })}
                      className="inline-flex items-center gap-1 rounded-sm hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                    >
                      Updated {sortAscending ? <ArrowUp className="size-3.5" /> : <ArrowDown className="size-3.5" />}
                    </Link>
                  </TableHead>
                  <TableHead>Retention</TableHead>
                  <TableHead>Shared with</TableHead>
                  <TableHead className="text-right">Actions</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {data.items.map((item) => {
                  const retentionState = retention(item.retention_expires_at);
                  return (
                    <TableRow key={item.id} className="h-12 hover:bg-background/25">
                      <TableCell>
                        <div className="capitalize">{item.harness}</div>
                        <div className="font-mono text-xs text-muted-foreground">{item.compatibility_profile}</div>
                      </TableCell>
                      <TableCell className="w-[20%] max-w-[20vw] whitespace-normal">
                        <Link className="line-clamp-4 break-words font-medium underline-offset-4 hover:underline" href={`/sessions/${item.id}`}>
                          {item.title ?? `${item.harness[0]?.toUpperCase() ?? ""}${item.harness.slice(1)} session`}
                        </Link>
                      </TableCell>
                      {me.role === "admin" && <TableCell className="max-w-56 truncate" title={item.user_email}>{item.user_email}</TableCell>}
                      <TableCell><Badge variant="outline" className="capitalize">{item.resumable ? "resumable" : "raw only"}</Badge></TableCell>
                      <TableCell>{formatBytes(item.size_bytes)}</TableCell>
                      <TableCell>{new Date(item.updated_at).toLocaleString()}</TableCell>
                      <TableCell title={item.retention_expires_at ? new Date(item.retention_expires_at).toLocaleString() : undefined}>
                        <Badge variant={retentionState.variant}>{retentionState.label}</Badge>
                      </TableCell>
                      <TableCell>
                        <SessionSharingSummary
                          sessionId={item.id}
                          mode={item.sharing_mode}
                          recipientCount={item.share_recipient_count}
                        />
                      </TableCell>
                      <TableCell className="text-right">
                        <SessionActions
                          sessionId={item.id}
                          sessionLabel={item.title ?? `${item.harness} session`}
                          canShare={item.user_id === me.id}
                        />
                      </TableCell>
                    </TableRow>
                  );
                })}
              </TableBody>
            </Table>
          )}
        </div>
        <div className="flex flex-col items-center justify-between gap-3 border-t px-4 py-3 sm:flex-row sm:px-6 lg:px-8">
          <p className="text-sm text-muted-foreground">
            {first.toLocaleString()}–{last.toLocaleString()} of {data.total.toLocaleString()}
          </p>
          <div className="flex flex-wrap items-center justify-center gap-3 sm:justify-end">
            <PageSizeSelect pathname="/sessions" params={currentParams} value={perPage} />
            {data.total_pages > 1 && <nav className="flex items-center gap-1" aria-label="Session pages">
              {data.total_pages > 12 && (
                <Link
                  href={hrefWith(currentParams, { page: 1 })}
                  aria-label="First page"
                  className={cn(buttonVariants({ variant: "outline", size: "icon-sm" }), "hidden sm:inline-flex", data.page === 1 && "pointer-events-none opacity-50")}
                  aria-disabled={data.page === 1}
                ><ChevronsLeft /></Link>
              )}
              <Link
                href={hrefWith(currentParams, { page: Math.max(1, data.page - 1) })}
                aria-label="Previous page"
                className={cn(buttonVariants({ variant: "outline", size: "icon-sm" }), data.page === 1 && "pointer-events-none opacity-50")}
                aria-disabled={data.page === 1}
              ><ChevronLeft /></Link>
              {pageItems(data.page, data.total_pages).map((item, index) =>
                item === "ellipsis" ? (
                  <span key={`ellipsis-${index}`} className="hidden size-7 items-center justify-center text-sm text-muted-foreground sm:flex" aria-hidden="true">…</span>
                ) : (
                  <Link
                    key={item}
                    href={hrefWith(currentParams, { page: item })}
                    aria-label={`Page ${item}`}
                    aria-current={item === data.page ? "page" : undefined}
                    className={cn(
                      buttonVariants({ variant: item === data.page ? "default" : "outline", size: "icon-sm" }),
                      Math.abs(item - data.page) > 1 && "hidden sm:inline-flex",
                    )}
                  >{item}</Link>
                ),
              )}
              <Link
                href={hrefWith(currentParams, { page: Math.min(data.total_pages, data.page + 1) })}
                aria-label="Next page"
                className={cn(buttonVariants({ variant: "outline", size: "icon-sm" }), data.page === data.total_pages && "pointer-events-none opacity-50")}
                aria-disabled={data.page === data.total_pages}
              ><ChevronRight /></Link>
              {data.total_pages > 12 && (
                <Link
                  href={hrefWith(currentParams, { page: data.total_pages })}
                  aria-label="Last page"
                  className={cn(buttonVariants({ variant: "outline", size: "icon-sm" }), "hidden sm:inline-flex", data.page === data.total_pages && "pointer-events-none opacity-50")}
                  aria-disabled={data.page === data.total_pages}
                ><ChevronsRight /></Link>
              )}
            </nav>}
          </div>
        </div>
      </section>
    </div>
  );
}
