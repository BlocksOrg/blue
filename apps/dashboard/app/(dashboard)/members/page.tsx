import { api, requireAdminIdentity } from "../../../lib/api";
import Link from "next/link";
import { redirect } from "next/navigation";
import { createUser } from "../../actions";
import { identityConfig } from "../../../lib/identity-config";
import { buildManagedIdentityOverview } from "../../../lib/identity-overview";
import {
  resolveMemberFilters,
  resolveMembersTab,
  type MemberFilters,
  type MembersTab,
} from "../../../lib/members-view";
import { InvitationActions, MemberActions } from "./member-actions";
import { MembersTabs } from "./members-tabs";
import { Info, Plus, Search, SlidersHorizontal, X } from "lucide-react";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
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
import { TablePaginationFooter } from "@/components/table-pagination-footer";

type SearchParams = Record<string, string | string[] | undefined>;
const allowedPageSizes = new Set([25, 50, 75, 100]);

function queryValue(params: SearchParams, name: string) {
  const current = params[name];
  return Array.isArray(current) ? current[0] ?? "" : current ?? "";
}

function positiveInteger(raw: string, fallback: number) {
  const parsed = Number.parseInt(raw, 10);
  return Number.isSafeInteger(parsed) && parsed > 0 ? parsed : fallback;
}

function selection(params: SearchParams, name: string) {
  const selected = queryValue(params, name);
  return selected === "all" ? "" : selected;
}

function membersHref(
  current: Record<string, string>,
  changes: Record<string, string | number | undefined>,
) {
  const params = new URLSearchParams(current);
  for (const [key, next] of Object.entries(changes)) {
    if (next === undefined || next === "" || next === "all" || (key === "page" && next === 1)) params.delete(key);
    else params.set(key, String(next));
  }
  const query = params.toString();
  return query ? `/members?${query}` : "/members";
}

type User = {
  id: string;
  subject: string;
  email: string;
  role: string;
  active: boolean;
  status: "active" | "suspended" | "removed";
  protected: boolean;
  provisioning_source: "local" | "scim";
  managed: boolean;
};
type Invitation = {
  id: string;
  email: string;
  role: string;
  status: "pending" | "accepted" | "canceled" | "expired";
  expires_at: string;
};
type Page<T> = {
  items: T[];
  page: number;
  per_page: number;
  total: number;
  total_pages: number;
};
type IdentityStatus = {
  auth_mode: "password" | "oidc";
  scim: {
    configured: boolean;
    group_role_mappings: Record<string, "admin" | "member">;
  };
};

function MemberListControls({
  invited,
  filters,
  activeFilters,
  perPage,
}: {
  invited: boolean;
  filters: MemberFilters;
  activeFilters: { key: string; label: string }[];
  perPage: number;
}) {
  const current: Record<string, string> = {};
  if (invited) current.tab = "invited";
  if (filters.q) current.q = filters.q;
  if (filters.role) current.role = filters.role;
  if (filters.status) current.status = filters.status;
  if (filters.provisioning_source) current.provisioning_source = filters.provisioning_source;
  if (perPage !== 25) current.per_page = String(perPage);
  const clearHref = invited ? "/members?tab=invited" : "/members";
  return (
    <div className="grid min-w-0 gap-3 px-4 pb-4 sm:px-6 lg:px-8">
      <div className="flex min-w-0 flex-col gap-3 sm:flex-row sm:items-center">
        <form action="/members" className="flex min-w-0 flex-1 gap-2">
          {invited && <input type="hidden" name="tab" value="invited" />}
          {filters.role && <input type="hidden" name="role" value={filters.role} />}
          {filters.status && <input type="hidden" name="status" value={filters.status} />}
          {filters.provisioning_source && <input type="hidden" name="provisioning_source" value={filters.provisioning_source} />}
          {perPage !== 25 && <input type="hidden" name="per_page" value={perPage} />}
          <div className="relative min-w-0 max-w-md flex-1">
            <Search className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" />
            <Input name="q" defaultValue={filters.q} maxLength={200} placeholder={invited ? "Search invited email" : "Search email or subject"} className="pl-8" aria-label={invited ? "Search invited members" : "Search members"} />
          </div>
          <Button type="submit" variant="secondary">Search</Button>
        </form>
        <Dialog>
          <DialogTrigger render={<Button variant="outline" />}><SlidersHorizontal /> Filters{activeFilters.length ? ` (${activeFilters.length})` : ""}</DialogTrigger>
          <DialogContent className="max-h-[calc(100svh-2rem)] overflow-y-auto sm:max-w-md">
            <DialogHeader><DialogTitle>{invited ? "Filter invitations" : "Filter members"}</DialogTitle><DialogDescription>{invited ? "Narrow outstanding invitations." : "Narrow organization members."}</DialogDescription></DialogHeader>
            <form action="/members" className="grid min-w-0 gap-4 sm:grid-cols-2">
              {invited && <input type="hidden" name="tab" value="invited" />}
              <input type="hidden" name="q" value={filters.q} />
              {invited && filters.status && <input type="hidden" name="status" value={filters.status} />}
              {invited && filters.provisioning_source && <input type="hidden" name="provisioning_source" value={filters.provisioning_source} />}
              {perPage !== 25 && <input type="hidden" name="per_page" value={perPage} />}
              <div className="grid min-w-0 gap-2">
                <Label htmlFor={`${invited ? "invited" : "member"}-role`}>Role</Label>
                <Select name="role" defaultValue={filters.role || "all"}><SelectTrigger id={`${invited ? "invited" : "member"}-role`} className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectItem value="all">All roles</SelectItem><SelectItem value="admin">Admin</SelectItem><SelectItem value="member">Member</SelectItem></SelectContent></Select>
              </div>
              {!invited && <>
                <div className="grid min-w-0 gap-2">
                  <Label htmlFor="member-status">Status</Label>
                  <Select name="status" defaultValue={filters.status || "all"}><SelectTrigger id="member-status" className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectItem value="all">All statuses</SelectItem><SelectItem value="active">Active</SelectItem><SelectItem value="suspended">Suspended</SelectItem><SelectItem value="removed">Removed</SelectItem></SelectContent></Select>
                </div>
                <div className="grid min-w-0 gap-2">
                  <Label htmlFor="member-source">Provisioning source</Label>
                  <Select name="provisioning_source" defaultValue={filters.provisioning_source || "all"}><SelectTrigger id="member-source" className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectItem value="all">All sources</SelectItem><SelectItem value="local">Local</SelectItem><SelectItem value="scim">SCIM</SelectItem></SelectContent></Select>
                </div>
              </>}
              <DialogFooter className="sm:col-span-2"><Button type="submit">Apply filters</Button>{activeFilters.length > 0 && <Button variant="outline" render={<Link href={clearHref} />}>Clear all</Button>}</DialogFooter>
            </form>
          </DialogContent>
        </Dialog>
      </div>
      {activeFilters.length > 0 && <div className="flex min-w-0 flex-wrap gap-2" aria-label="Applied filters">{activeFilters.map((filter) => <Link key={filter.key} href={membersHref(current, { [filter.key]: undefined, page: 1 })} aria-label={`Remove ${filter.label}`} title={filter.label} className="inline-flex h-7 max-w-full min-w-0 items-center gap-1 rounded-md border bg-muted/40 px-2.5 text-xs text-muted-foreground hover:bg-muted hover:text-foreground"><span className="truncate">{filter.label}</span><X className="size-3 shrink-0" /></Link>)}</div>}
    </div>
  );
}

export default async function Members({
  searchParams,
}: {
  searchParams: Promise<SearchParams>;
}) {
  const me = await requireAdminIdentity();
  const raw = await searchParams;
  const configuredIdentity = identityConfig();
  const identityStatus =
    configuredIdentity.mode === "oidc"
      ? await api<IdentityStatus>("/admin/identity/status")
      : null;
  const identityOverview = buildManagedIdentityOverview(
    configuredIdentity,
    process.env.BETTER_AUTH_URL ?? "http://127.0.0.1:3000",
    process.env.CONTROL_API_PUBLIC_URL ?? "http://127.0.0.1:8080",
  );
  const managedWorkspace = Boolean(
    identityOverview &&
      identityStatus?.auth_mode === "oidc" &&
      identityStatus.scim.configured,
  );
  const requestedSize = positiveInteger(queryValue(raw, "per_page"), 25);
  const perPage = allowedPageSizes.has(requestedSize) ? requestedSize : 25;
  const requestedTab = queryValue(raw, "tab");
  const activeTab: MembersTab = resolveMembersTab(
    requestedTab,
    managedWorkspace,
  );
  const requestedPage = positiveInteger(queryValue(raw, "page"), 1);
  const filters = resolveMemberFilters({
    q: queryValue(raw, "q"),
    role: selection(raw, "role"),
    status: selection(raw, "status"),
    provisioning_source: selection(raw, "provisioning_source"),
  });
  const memberPage = activeTab === "members" ? requestedPage : 1;
  const invitedPage = activeTab === "invited" ? requestedPage : 1;
  const memberQuery = new URLSearchParams({ page: String(memberPage), per_page: String(perPage) });
  const invitationQuery = new URLSearchParams({ status: "outstanding", page: String(invitedPage), per_page: String(perPage) });
  if (filters.q) { memberQuery.set("q", filters.q); invitationQuery.set("q", filters.q); }
  if (filters.role) { memberQuery.set("role", filters.role); invitationQuery.set("role", filters.role); }
  if (filters.status) memberQuery.set("status", filters.status);
  if (filters.provisioning_source) memberQuery.set("provisioning_source", filters.provisioning_source);
  const [users, invited] = await Promise.all([
    api<Page<User>>(`/admin/users?${memberQuery}`),
    api<Page<Invitation>>(`/admin/invitations?${invitationQuery}`),
  ]);
  const activeData =
    activeTab === "identity" ? null : activeTab === "invited" ? invited : users;
  if (
    activeData &&
    activeData.total > 0 &&
    requestedPage > activeData.total_pages
  ) {
    const params = new URLSearchParams();
    if (activeTab === "invited") params.set("tab", "invited");
    if (filters.q) params.set("q", filters.q);
    if (filters.role) params.set("role", filters.role);
    if (filters.status) params.set("status", filters.status);
    if (filters.provisioning_source) params.set("provisioning_source", filters.provisioning_source);
    if (perPage !== 25) params.set("per_page", String(perPage));
    if (activeData.total_pages > 1)
      params.set("page", String(activeData.total_pages));
    const query = params.toString();
    redirect(query ? `/members?${query}` : "/members");
  }
  const paginationParams: Record<string, string> = {};
  if (perPage !== 25) paginationParams.per_page = String(perPage);
  if (filters.q) paginationParams.q = filters.q;
  if (filters.role) paginationParams.role = filters.role;
  if (filters.status) paginationParams.status = filters.status;
  if (filters.provisioning_source) paginationParams.provisioning_source = filters.provisioning_source;
  const memberActiveFilters = [
    filters.q && { key: "q", label: `Search: ${filters.q}` },
    filters.role && { key: "role", label: `Role: ${filters.role}` },
    filters.status && { key: "status", label: `Status: ${filters.status}` },
    filters.provisioning_source && { key: "provisioning_source", label: `Source: ${filters.provisioning_source === "scim" ? "SCIM" : "Local"}` },
  ].filter(Boolean) as { key: string; label: string }[];
  const invitationActiveFilters = memberActiveFilters.filter((filter) => filter.key === "q" || filter.key === "role");
  return (
    <div className="flex flex-col gap-6">
        {!managedWorkspace && (
          <div className="dashboard-page-header flex flex-wrap items-start justify-between gap-4">
            <Dialog>
              <DialogTrigger render={<Button />}><Plus /> Invite member</DialogTrigger>
              <DialogContent className="sm:max-w-md">
                <DialogHeader>
                  <DialogTitle>Invite a member</DialogTitle>
                  <DialogDescription>Send an invitation to join this organization.</DialogDescription>
                </DialogHeader>
                <form action={createUser} className="grid gap-5">
                  <div className="grid gap-2">
                    <Label htmlFor="invite-email">Email</Label>
                    <Input id="invite-email" name="email" type="email" required autoFocus />
                  </div>
                  <div className="grid gap-2">
                    <Label>Role</Label>
                    <Select name="role" defaultValue="member">
                      <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
                      <SelectContent><SelectItem value="member">Member</SelectItem><SelectItem value="admin">Admin</SelectItem></SelectContent>
                    </Select>
                  </div>
                  <DialogFooter><Button type="submit">Send invitation</Button></DialogFooter>
                </form>
              </DialogContent>
            </Dialog>
          </div>
        )}
      {managedWorkspace && identityOverview && (
        <Alert aria-label="Managed identity information">
          <Info />
          <AlertTitle>{identityOverview.providerName} manages this organization</AlertTitle>
          <AlertDescription>
            Sign-in uses OIDC, and membership and roles are provisioned through SCIM.{" "}
            <Link href="/members?tab=identity">View identity configuration</Link>
          </AlertDescription>
        </Alert>
      )}
      <MembersTabs
        managed={managedWorkspace}
        memberCount={users.total}
        invitationCount={invited.total}
        members={(
          <section className="-mx-4 overflow-hidden border-y bg-transparent pt-4 sm:-mx-6 lg:-mx-8">
          <MemberListControls invited={false} filters={filters} activeFilters={memberActiveFilters} perPage={perPage} />
          <div className="overflow-x-auto">
          <Table className="[&_td:first-child]:pl-4 [&_td:last-child]:pr-4 [&_th:first-child]:pl-4 [&_th:last-child]:pr-4 sm:[&_td:first-child]:pl-6 sm:[&_td:last-child]:pr-6 sm:[&_th:first-child]:pl-6 sm:[&_th:last-child]:pr-6 lg:[&_td:first-child]:pl-8 lg:[&_td:last-child]:pr-8 lg:[&_th:first-child]:pl-8 lg:[&_th:last-child]:pr-8">
            <TableHeader className="bg-background/35 text-muted-foreground">
              <TableRow>
                <TableHead>User</TableHead>
                <TableHead>Subject</TableHead>
                <TableHead>Role</TableHead>
                <TableHead>Status</TableHead>
                <TableHead className="text-right">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {users.items.length === 0 && <TableRow><TableCell colSpan={5} className="h-24 text-center text-muted-foreground">{memberActiveFilters.length ? "No members match these filters." : "No members found."}</TableCell></TableRow>}
              {users.items.map((user) => (
                <TableRow key={user.id} className="h-12 hover:bg-background/25">
                  <TableCell className="max-w-64 truncate font-medium" title={user.email}>{user.email}</TableCell>
                  <TableCell className="max-w-64 truncate font-mono text-xs text-muted-foreground" title={user.subject}>
                    {user.subject}
                  </TableCell>
                  <TableCell>
                    <div className="flex flex-wrap gap-2">
                      <Badge variant="outline" className="capitalize">{user.role}</Badge>
                      {user.managed && <Badge variant="secondary">SCIM</Badge>}
                    </div>
                  </TableCell>
                  <TableCell>
                    <Badge variant={user.active ? "default" : "secondary"} className="capitalize">
                      {user.status}
                    </Badge>
                  </TableCell>
                  <TableCell className="text-right"><MemberActions user={user} currentUserId={me.id} /></TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
          </div>
          <TablePaginationFooter
            pathname="/members"
            params={paginationParams}
            page={users.page}
            perPage={perPage}
            total={users.total}
            label="Member"
          />
          </section>
        )}
        invitations={(
          <section className="-mx-4 overflow-hidden border-y bg-transparent pt-4 sm:-mx-6 lg:-mx-8">
          <MemberListControls invited filters={filters} activeFilters={invitationActiveFilters} perPage={perPage} />
          <div className="overflow-x-auto">
          <Table className="[&_td:first-child]:pl-4 [&_td:last-child]:pr-4 [&_th:first-child]:pl-4 [&_th:last-child]:pr-4 sm:[&_td:first-child]:pl-6 sm:[&_td:last-child]:pr-6 sm:[&_th:first-child]:pl-6 sm:[&_th:last-child]:pr-6 lg:[&_td:first-child]:pl-8 lg:[&_td:last-child]:pr-8 lg:[&_th:first-child]:pl-8 lg:[&_th:last-child]:pr-8">
            <TableHeader className="bg-background/35 text-muted-foreground">
              <TableRow>
                <TableHead>Email</TableHead>
                <TableHead>Role</TableHead>
                <TableHead>Status</TableHead>
                <TableHead>Expires</TableHead>
                <TableHead className="text-right">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {invited.items.length === 0 && (
                <TableRow>
                  <TableCell colSpan={5} className="h-24 text-center text-muted-foreground">
                    {invitationActiveFilters.length ? "No outstanding invitations match these filters." : "No outstanding invitations."}
                  </TableCell>
                </TableRow>
              )}
              {invited.items.map((invitation) => (
                <TableRow key={invitation.id} className="h-12 hover:bg-background/25">
                  <TableCell className="max-w-64 truncate font-medium" title={invitation.email}>{invitation.email}</TableCell>
                  <TableCell className="capitalize">{invitation.role}</TableCell>
                  <TableCell>
                    <Badge variant="outline" className="capitalize">
                      {invitation.status}
                    </Badge>
                  </TableCell>
                  <TableCell>{new Date(invitation.expires_at).toLocaleString()}</TableCell>
                  <TableCell className="text-right"><InvitationActions invitation={invitation} /></TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
          </div>
          <TablePaginationFooter
            pathname="/members"
            params={{ ...paginationParams, tab: "invited" }}
            page={invited.page}
            perPage={perPage}
            total={invited.total}
            label="Invited member"
          />
          </section>
        )}
        identity={(
          identityOverview && identityStatus ? (
            <section aria-labelledby="identity-overview" className="overflow-hidden rounded-xl border">
              <h2 id="identity-overview" className="sr-only">Identity and provisioning overview</h2>
              <Table>
                <TableBody>
                  <TableRow>
                    <TableCell className="w-52 text-muted-foreground">Provider</TableCell>
                    <TableCell className="font-medium">{identityOverview.providerName}</TableCell>
                  </TableRow>
                  <TableRow>
                    <TableCell className="text-muted-foreground">Provider ID</TableCell>
                    <TableCell className="font-mono text-xs">{identityOverview.providerId}</TableCell>
                  </TableRow>
                  <TableRow>
                    <TableCell className="text-muted-foreground">Authentication</TableCell>
                    <TableCell><Badge variant="secondary">OIDC configured</Badge></TableCell>
                  </TableRow>
                  <TableRow>
                    <TableCell className="text-muted-foreground">OIDC issuer</TableCell>
                    <TableCell className="break-all font-mono text-xs">{identityOverview.issuer}</TableCell>
                  </TableRow>
                  <TableRow>
                    <TableCell className="text-muted-foreground">OIDC client ID</TableCell>
                    <TableCell className="break-all font-mono text-xs">{identityOverview.clientId}</TableCell>
                  </TableRow>
                  <TableRow>
                    <TableCell className="text-muted-foreground">Sign-in callback</TableCell>
                    <TableCell className="break-all font-mono text-xs">{identityOverview.callbackUrl}</TableCell>
                  </TableRow>
                  <TableRow>
                    <TableCell className="text-muted-foreground">Provisioning</TableCell>
                    <TableCell><Badge variant="secondary">SCIM configured</Badge></TableCell>
                  </TableRow>
                  <TableRow>
                    <TableCell className="text-muted-foreground">SCIM base URL</TableCell>
                    <TableCell className="break-all font-mono text-xs">{identityOverview.scimBaseUrl}</TableCell>
                  </TableRow>
                  <TableRow>
                    <TableCell className="text-muted-foreground">SCIM authentication</TableCell>
                    <TableCell>Bearer credential configured</TableCell>
                  </TableRow>
                  <TableRow>
                    <TableCell className="text-muted-foreground">Default role</TableCell>
                    <TableCell><Badge variant="outline">Member</Badge></TableCell>
                  </TableRow>
                  <TableRow>
                    <TableCell className="text-muted-foreground">Group role mappings</TableCell>
                    <TableCell>
                      {Object.entries(identityStatus.scim.group_role_mappings).length === 0 ? (
                        <span className="text-muted-foreground">No mappings configured. Provisioned users receive the Member role.</span>
                      ) : (
                        <div className="flex flex-wrap gap-2">
                          {Object.entries(identityStatus.scim.group_role_mappings).map(([group, role]) => (
                            <Badge key={group} variant="outline">
                              {group} → <span className="capitalize">{role}</span>
                            </Badge>
                          ))}
                        </div>
                      )}
                    </TableCell>
                  </TableRow>
                </TableBody>
              </Table>
            </section>
          ) : null
        )}
      />
    </div>
  );
}
