export type MembersTab = "members" | "invited" | "identity";

export function resolveMembersTab(
  requestedTab: string,
  managed: boolean,
): MembersTab {
  if (managed && requestedTab === "identity") return "identity";
  if (!managed && requestedTab === "invited") return "invited";
  return "members";
}

export const memberRoles = ["admin", "member"] as const;
export const memberStatuses = ["active", "suspended", "removed"] as const;
export const provisioningSources = ["local", "scim"] as const;

export type MemberFilters = {
  q: string;
  role: string;
  status: string;
  provisioning_source: string;
};

function allowed(value: string, options: readonly string[]) {
  return options.includes(value) ? value : "";
}

/**
 * Filters arrive from the URL, so anything the Control API would reject with a
 * 400 is dropped rather than forwarded.
 */
export function resolveMemberFilters(raw: MemberFilters): MemberFilters {
  return {
    q: raw.q.trim().slice(0, 200),
    role: allowed(raw.role, memberRoles),
    status: allowed(raw.status, memberStatuses),
    provisioning_source: allowed(raw.provisioning_source, provisioningSources),
  };
}
