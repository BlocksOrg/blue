import Link from "next/link";
import { redirect } from "next/navigation";
import {
  ChevronLeft,
  ChevronRight,
  ChevronsLeft,
  ChevronsRight,
  Search,
  SlidersHorizontal,
  X,
} from "lucide-react";
import { api, requireAdminIdentity } from "../../../lib/api";
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
import { cn } from "@/lib/utils";
import { PageSizeSelect } from "@/components/page-size-select";
import { AsyncUserSelect } from "@/components/async-user-select";
import {
  ClientInventoryTable,
  type ClientStatus,
} from "./client-inventory-table";

type ClientPage = {
  items: ClientStatus[];
  current_revision?: string;
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
  return query ? `/clients?${query}` : "/clients";
}

function pageItems(current: number, total: number): (number | "ellipsis")[] {
  if (total <= 7) return Array.from({ length: total }, (_, index) => index + 1);
  if (current <= 4) return [1, 2, 3, 4, 5, "ellipsis", total];
  if (current >= total - 3)
    return [1, "ellipsis", total - 4, total - 3, total - 2, total - 1, total];
  return [1, "ellipsis", current - 1, current, current + 1, "ellipsis", total];
}

export default async function ClientsPage({
  searchParams,
}: {
  searchParams: Promise<SearchParams>;
}) {
  await requireAdminIdentity();
  const raw = await searchParams;
  const requestedPage = positiveInteger(value(raw, "page"), 1);
  const requestedSize = positiveInteger(value(raw, "per_page"), 25);
  const perPage = allowedPageSizes.has(requestedSize) ? requestedSize : 25;
  const filters = {
    q: value(raw, "q").trim(),
    user_id: selection(raw, "user_id"),
    harness: selection(raw, "harness"),
    health: selection(raw, "health"),
    last_seen_from: value(raw, "last_seen_from"),
    last_seen_to: value(raw, "last_seen_to"),
    sort:
      value(raw, "sort") === "last_seen_asc"
        ? "last_seen_asc"
        : "last_seen_desc",
  };
  const currentParams: Record<string, string> = {};
  for (const [key, current] of Object.entries({
    ...filters,
    page: String(requestedPage),
    per_page: String(perPage),
  })) {
    if (
      current &&
      !(key === "page" && current === "1") &&
      !(key === "per_page" && current === "25")
    )
      currentParams[key] = current;
  }

  const query = new URLSearchParams({
    page: String(requestedPage),
    per_page: String(perPage),
    sort: filters.sort,
  });
  for (const name of [
    "q",
    "user_id",
    "harness",
    "health",
    "last_seen_from",
    "last_seen_to",
  ] as const) {
    if (filters[name]) query.set(name, filters[name]);
  }

  const [data, facets] = await Promise.all([
    api<ClientPage>(`/admin/client-status?${query}`),
    api<Facets>(`/admin/client-status/facets${filters.user_id ? `?selected_user_id=${encodeURIComponent(filters.user_id)}` : ""}`),
  ]);
  if (data.total > 0 && requestedPage > data.total_pages) {
    redirect(hrefWith(currentParams, { page: data.total_pages }));
  }

  const userEmail = facets.users.find((user) => user.id === filters.user_id)?.email;
  const healthLabels: Record<string, string> = {
    current: "Current",
    outdated: "Outdated",
    attention: "Needs attention",
  };
  const activeFilters = [
    filters.q && { key: "q", label: `Search: ${filters.q}` },
    filters.user_id && { key: "user_id", label: `User: ${userEmail ?? "Unknown"}` },
    filters.harness && { key: "harness", label: `Harness: ${filters.harness}` },
    filters.health && {
      key: "health",
      label: `Health: ${healthLabels[filters.health] ?? filters.health}`,
    },
    filters.last_seen_from && { key: "last_seen_from", label: `From: ${filters.last_seen_from}` },
    filters.last_seen_to && { key: "last_seen_to", label: `To: ${filters.last_seen_to}` },
  ].filter(Boolean) as { key: string; label: string }[];
  const hasFilters = activeFilters.length > 0;
  const first = data.total === 0 ? 0 : (data.page - 1) * data.per_page + 1;
  const last = Math.min(data.page * data.per_page, data.total);
  const sortAscending = filters.sort === "last_seen_asc";

  return (
    <div className="flex flex-col gap-6">
      <div className="flex min-w-0 flex-col gap-3 sm:flex-row sm:items-center">
        <form action="/clients" className="flex min-w-0 flex-1 gap-2">
          {Object.entries(filters).map(([key, current]) => key !== "q" && current ? <input key={key} type="hidden" name={key} value={current} /> : null)}
          <input type="hidden" name="per_page" value={perPage} />
          <div className="relative min-w-0 max-w-md flex-1">
            <Search className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" />
            <Input name="q" defaultValue={filters.q} maxLength={200} placeholder="Search hostname, instance ID, or email" className="pl-8" aria-label="Search clients" />
          </div>
          <Button type="submit" variant="secondary">Search</Button>
        </form>
        <Dialog>
          <DialogTrigger render={<Button variant="outline" />}>
            <SlidersHorizontal /> Filters{activeFilters.length ? ` (${activeFilters.length})` : ""}
          </DialogTrigger>
          <DialogContent className="max-h-[calc(100svh-2rem)] overflow-y-auto sm:max-w-2xl">
            <DialogHeader><DialogTitle>Filter clients</DialogTitle><DialogDescription>Narrow the operational inventory.</DialogDescription></DialogHeader>
            <form action="/clients" className="grid min-w-0 gap-4 sm:grid-cols-2">
            <input type="hidden" name="per_page" value={perPage} />
            <input type="hidden" name="q" value={filters.q} />
            <div className="grid min-w-0 gap-2">
              <Label htmlFor="client-user">User</Label>
              <AsyncUserSelect id="client-user" name="user_id" source="clients" value={filters.user_id} initialUsers={facets.users} />
            </div>
            <div className="grid min-w-0 gap-2">
              <Label htmlFor="client-harness">Harness</Label>
              <Select name="harness" defaultValue={filters.harness || "all"}>
                <SelectTrigger id="client-harness" className="w-full capitalize"><SelectValue /></SelectTrigger>
                <SelectContent>
                  <SelectItem value="all">All harnesses</SelectItem>
                  {facets.harnesses.map((harness) => (
                    <SelectItem key={harness} value={harness} className="capitalize">{harness}</SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="grid min-w-0 gap-2">
              <Label htmlFor="client-health">Health</Label>
              <Select name="health" defaultValue={filters.health || "all"}>
                <SelectTrigger id="client-health" className="w-full"><SelectValue /></SelectTrigger>
                <SelectContent>
                  <SelectItem value="all">All states</SelectItem>
                  <SelectItem value="current">Current</SelectItem>
                  <SelectItem value="outdated">Outdated</SelectItem>
                  <SelectItem value="attention">Needs attention</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div className="grid min-w-0 gap-2">
              <Label htmlFor="client-sort">Order</Label>
              <Select name="sort" defaultValue={filters.sort}>
                <SelectTrigger id="client-sort" className="w-full"><SelectValue /></SelectTrigger>
                <SelectContent>
                  <SelectItem value="last_seen_desc">Most recently seen</SelectItem>
                  <SelectItem value="last_seen_asc">Least recently seen</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div className="grid min-w-0 gap-2">
              <Label htmlFor="last-seen-from">Last seen from</Label>
              <Input id="last-seen-from" name="last_seen_from" type="date" defaultValue={filters.last_seen_from} />
            </div>
            <div className="grid min-w-0 gap-2">
              <Label htmlFor="last-seen-to">Last seen to</Label>
              <Input id="last-seen-to" name="last_seen_to" type="date" defaultValue={filters.last_seen_to} />
            </div>
            <DialogFooter className="sm:col-span-2">
              <Button type="submit">Apply filters</Button>
              {(hasFilters || sortAscending) && (
                <Button variant="outline" render={<Link href="/clients" />}>Clear all</Button>
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
                <p className="font-medium">
                  {hasFilters ? "No matching clients" : "No reporting clients yet"}
                </p>
                <p className="mt-1 text-sm text-muted-foreground">
                  {hasFilters
                    ? "Try removing a filter or broadening the last-seen range."
                    : <>Run <code className="rounded bg-muted px-1">blue status</code> or <code className="rounded bg-muted px-1">blue apply</code> on a managed machine.</>}
                </p>
              </div>
              {hasFilters && <Button variant="outline" render={<Link href="/clients" />}>Clear filters</Button>}
            </div>
          ) : (
            <ClientInventoryTable
              clients={data.items.map((client) => ({
                ...client,
                first_seen_label: new Date(client.first_seen_at).toLocaleString(),
                last_seen_label: new Date(client.last_seen_at).toLocaleString(),
              }))}
              currentRevision={data.current_revision}
              sortAscending={sortAscending}
              lastSeenSortHref={hrefWith(currentParams, {
                sort: sortAscending ? "last_seen_desc" : "last_seen_asc",
                page: 1,
              })}
            />
          )}
        </div>
        <div className="flex flex-col items-center justify-between gap-3 border-t px-4 py-3 sm:flex-row sm:px-6 lg:px-8">
          <p className="text-sm text-muted-foreground">
            {first.toLocaleString()}–{last.toLocaleString()} of {data.total.toLocaleString()}
          </p>
          <div className="flex flex-wrap items-center justify-center gap-3 sm:justify-end">
            <PageSizeSelect pathname="/clients" params={currentParams} value={perPage} />
            {data.total_pages > 1 && <nav className="flex items-center gap-1" aria-label="Client pages">
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
