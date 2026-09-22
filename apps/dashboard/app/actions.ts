"use server";

import { headers } from "next/headers";
import { redirect, unstable_rethrow } from "next/navigation";
import { revalidatePath } from "next/cache";
import { api } from "../lib/api";
import { auth, authPool } from "../lib/auth";
import { randomUUID } from "crypto";
import { identityConfig } from "../lib/identity-config";
import type { Branding } from "../lib/branding";
import type { GatewayModelsResource } from "../lib/gateway-models";

export async function logout() {
  await auth.api.signOut({ headers: await headers() });
  redirect("/login");
}

export type ConfigState = {
  saved?: boolean;
  error?: string;
  revision?: string;
  signature?: string;
};

export type AudienceUserOption = {
  id: string;
  email: string;
  role: "admin" | "member";
  status: "active" | "suspended" | "removed";
};

export type AudienceUserSearchResult = {
  users: AudienceUserOption[];
  error?: string;
};

export type FilterUserSource = "sessions" | "gateway" | "clients";

export type FilterUserOption = {
  id: string;
  email: string;
};

export type FilterUserSearchResult = {
  users: FilterUserOption[];
  error?: string;
};

export type HarnessConfigState = {
  saved?: boolean;
  error?: string;
  revision?: string;
  harness?: string;
  managedConfigYaml?: string;
  versionRequirement?: string;
  allowUnverifiedVersions?: boolean;
};

export type BrandingState = {
  saved?: boolean;
  error?: string;
  branding?: Branding;
};

export async function saveBranding(
  _: BrandingState,
  form: FormData,
): Promise<BrandingState> {
  try {
    const branding = await api<Branding>("/admin/branding", {
      method: "PUT",
      body: JSON.stringify({
        logo_url: String(form.get("logo_url") ?? ""),
        favicon_url: String(form.get("favicon_url") ?? ""),
      }),
    });
    revalidatePath("/", "layout");
    return { saved: true, branding };
  } catch (error) {
    return {
      error:
        error instanceof Error
          ? error.message
              .replace(/^\d+:\s*/, "")
              .replace(/^\{"error":"(.*)"\}$/, "$1")
          : "Branding settings could not be saved.",
    };
  }
}

export async function saveHarnessManagedConfig(
  _: HarnessConfigState,
  form: FormData,
): Promise<HarnessConfigState> {
  const harness = String(form.get("harness") ?? "");
  try {
    const saved = await api<{
      revision: string;
      managed_config_yaml: string;
      version_requirement?: string;
      allow_unverified_versions?: boolean;
    }>(
      `/admin/harnesses/${encodeURIComponent(harness)}/managed-config`,
      {
        method: "PUT",
        body: JSON.stringify({
          base_revision: form.get("revision"),
          managed_config_yaml: String(form.get("managed_config_yaml") ?? ""),
          version_requirement: String(form.get("version_requirement") ?? "") || null,
          allow_unverified_versions: form.get("allow_unverified_versions") === "on",
        }),
      },
    );
    return {
      saved: true,
      revision: saved.revision,
      harness,
      managedConfigYaml: saved.managed_config_yaml,
      versionRequirement: saved.version_requirement ?? "",
      allowUnverifiedVersions: saved.allow_unverified_versions ?? false,
    };
  } catch (error) {
    return {
      error:
        error instanceof Error
          ? error.message
              .replace(/^\d+:\s*/, "")
              .replace(/^\{"error":"(.*)"\}$/, "$1")
          : "The managed configuration could not be saved.",
      harness,
    };
  }
}

export type GatewayKeyState = { error?: string };

export async function ensureGatewayKey(_: GatewayKeyState): Promise<GatewayKeyState> {
  try {
    await api("/gateway/key/ensure", { method: "POST", body: '{"manual":true}' });
    revalidatePath("/gateway");
    return {};
  } catch (error) {
    unstable_rethrow(error);
    if (!(error instanceof Error)) return { error: "Gateway key could not be provisioned." };
    const detail = error.message.replace(/^\d{3}:\s*/, "");
    try {
      const parsed = JSON.parse(detail) as { error?: string };
      return { error: parsed.error || "Gateway key could not be provisioned." };
    } catch {
      return { error: detail || "Gateway key could not be provisioned." };
    }
  }
}

export type GatewayProxyHealthState = {
  status: "idle" | "healthy" | "unhealthy";
  checkedAt?: string;
  latencyMs?: number;
  httpStatus?: number;
  error?: string;
};

export async function checkGatewayProxyHealth(
  _: GatewayProxyHealthState,
): Promise<GatewayProxyHealthState> {
  try {
    const result = await api<{
      status: "healthy" | "unhealthy";
      checked_at: string;
      latency_ms: number;
      http_status?: number;
      error?: string;
    }>("/gateway/proxy/health", { method: "POST" });
    return {
      status: result.status,
      checkedAt: result.checked_at,
      latencyMs: result.latency_ms,
      httpStatus: result.http_status,
      error: result.error,
    };
  } catch (error) {
    return {
      status: "unhealthy",
      error:
        error instanceof Error
          ? error.message
              .replace(/^\d+:\s*/, "")
              .replace(/^\{"error":"(.*)"\}$/, "$1")
          : "Proxy health check failed.",
    };
  }
}

export type GatewayModelsActionState = {
  saved?: boolean;
  error?: string;
  revision?: string;
  resource?: GatewayModelsResource;
};

function actionError(error: unknown, fallback: string) {
  if (!(error instanceof Error)) return fallback;
  const detail = error.message.replace(/^\d{3}:\s*/, "");
  try {
    const parsed = JSON.parse(detail) as { error?: string };
    return parsed.error || fallback;
  } catch {
    return detail || fallback;
  }
}

export async function refreshGatewayModels(
  _: GatewayModelsActionState,
): Promise<GatewayModelsActionState> {
  try {
    const resource = await api<GatewayModelsResource>("/admin/gateway/models/refresh", {
      method: "POST",
    });
    revalidatePath("/gateway");
    return { saved: true, resource, revision: resource.revision };
  } catch (error) {
    return { error: actionError(error, "Gateway models could not be refreshed.") };
  }
}

export async function saveHarnessGatewayModels(
  _: GatewayModelsActionState,
  form: FormData,
): Promise<GatewayModelsActionState> {
  const harness = String(form.get("harness") ?? "");
  try {
    const saved = await api<{ revision: string }>(
      `/admin/gateway/models/harnesses/${encodeURIComponent(harness)}`,
      {
        method: "PUT",
        body: JSON.stringify({
          base_revision: String(form.get("revision") ?? ""),
          gateway_models: JSON.parse(String(form.get("gateway_models") ?? "[]")),
          default_model: String(form.get("default_model") ?? "") || null,
        }),
      },
    );
    revalidatePath("/gateway");
    revalidatePath("/harnesses");
    revalidatePath("/", "layout");
    return { saved: true, revision: saved.revision };
  } catch (error) {
    return { error: actionError(error, "Gateway model assignments could not be saved.") };
  }
}

export async function acknowledgeGatewayModel(
  _: GatewayModelsActionState,
  form: FormData,
): Promise<GatewayModelsActionState> {
  try {
    await api("/admin/gateway/models/acknowledge", {
      method: "POST",
      body: JSON.stringify({
        model_id: String(form.get("model_id") ?? ""),
        fingerprint: String(form.get("fingerprint") ?? ""),
      }),
    });
    revalidatePath("/gateway");
    return { saved: true };
  } catch (error) {
    return { error: actionError(error, "Model change could not be acknowledged.") };
  }
}

export async function saveExtensions(
  _: ConfigState,
  form: FormData,
): Promise<ConfigState> {
  try {
    const packages = JSON.parse(String(form.get("packages") ?? "[]"));
    const packageOverrides = JSON.parse(
      String(form.get("package_overrides") ?? "{}"),
    );
    const packageAudiences = JSON.parse(
      String(form.get("package_audiences") ?? "{}"),
    );
    const mcp = JSON.parse(String(form.get("mcp") ?? "{}"));
    const saved = await api<{ revision: string }>("/admin/governance-extensions", {
      method: "PUT",
      body: JSON.stringify({
        base_revision: form.get("revision"),
        packages,
        package_audiences: packageAudiences,
        package_overrides: packageOverrides,
        mcp,
      }),
    });
    revalidatePath("/extensions");
    revalidatePath("/", "layout");
    return {
      saved: true,
      revision: saved.revision,
      signature: JSON.stringify({
        packages,
        audiences: packageAudiences,
        overrides: packageOverrides,
        mcp,
      }),
    };
  } catch (error) {
    return {
      error:
        error instanceof Error ? error.message : "Extension configuration failed",
    };
  }
}

export async function searchAudienceUsers(
  query: string,
  selectedUserIds: string[],
): Promise<AudienceUserSearchResult> {
  try {
    const users = await api<AudienceUserOption[]>("/admin/users/options", {
      method: "POST",
      body: JSON.stringify({ q: query, selected_user_ids: selectedUserIds }),
    });
    return { users: users.slice(0, 10) };
  } catch (error) {
    return {
      users: [],
      error:
        error instanceof Error
          ? error.message
              .replace(/^\d+:\s*/, "")
              .replace(/^\{"error":"(.*)"\}$/, "$1")
          : "Members could not be loaded.",
    };
  }
}

export async function searchFilterUsers(
  source: FilterUserSource,
  query: string,
  selectedUserId?: string,
): Promise<FilterUserSearchResult> {
  const paths: Record<FilterUserSource, string> = {
    sessions: "/session-uploads/facets",
    gateway: "/gateway/request-logs/facets",
    clients: "/admin/client-status/facets",
  };
  if (!Object.hasOwn(paths, source)) return { users: [] };
  const params = new URLSearchParams();
  const trimmed = query.trim().slice(0, 200);
  if (trimmed) params.set("user_q", trimmed);
  if (selectedUserId) params.set("selected_user_id", selectedUserId);
  try {
    const result = await api<{ users: FilterUserOption[] }>(
      `${paths[source]}?${params}`,
    );
    return { users: result.users.slice(0, 10) };
  } catch (error) {
    return {
      users: [],
      error:
        error instanceof Error
          ? error.message
              .replace(/^\d+:\s*/, "")
              .replace(/^\{"error":"(.*)"\}$/, "$1")
          : "Users could not be loaded.",
    };
  }
}

export type InspectPackageState = {
  source_ref?: string;
  artifact_id?: string;
  resolved_commit?: string;
  sha256?: string;
  size_bytes?: number;
  error?: string;
};

export async function inspectPackageSource(
  _: InspectPackageState,
  form: FormData,
): Promise<InspectPackageState> {
  try {
    const connectionId = String(form.get("connection_id") ?? "");
    const body = connectionId
      ? {
          connection_id: connectionId,
          repository: form.get("repository"),
          // A blank field must reach the API as an absent ref so it answers
          // "ref is required" instead of rejecting "" as an unsafe character.
          ref: String(form.get("ref") ?? "").trim() || undefined,
        }
      : { source_ref: form.get("source_ref") };
    return await api<InspectPackageState>("/admin/package-source/inspect", {
      method: "POST",
      body: JSON.stringify(body),
    });
  } catch (error) {
    return {
      error:
        error instanceof Error
          ? error.message
          : "Package source inspection failed",
    };
  }
}

export type InvitationLinkState = {
  invitationId?: string;
  email?: string;
  invitationUrl?: string;
  error?: string;
};

type AdminInvitationIssued = {
  id: string;
  email: string;
  invitation_url: string;
};

function invitationError(error: unknown) {
  if (!(error instanceof Error)) return "Unable to issue the invitation link.";
  const detail = error.message.replace(/^\d{3}:\s*/, "");
  try {
    const parsed = JSON.parse(detail) as { error?: string; message?: string };
    return parsed.message ?? parsed.error ?? "Unable to issue the invitation link.";
  } catch {
    return detail || "Unable to issue the invitation link.";
  }
}

export async function createUser(
  _: InvitationLinkState,
  form: FormData,
): Promise<InvitationLinkState> {
  try {
    const invitation = await api<AdminInvitationIssued>("/admin/invitations", {
      method: "POST",
      body: JSON.stringify({
        email: String(form.get("email")),
        role: String(form.get("role")) as "admin" | "member",
      }),
    });
    revalidatePath("/members");
    return { invitationId: invitation.id, email: invitation.email, invitationUrl: invitation.invitation_url };
  } catch (error) {
    return { error: invitationError(error) };
  }
}

export async function updateUser(form: FormData) {
  const userId = String(form.get("user_id"));
  const field = String(form.get("field"));
  const value = String(form.get("value"));
  await api(`/admin/users/${userId}`, {
    method: "PATCH",
    body: JSON.stringify({ [field]: value }),
  });
  revalidatePath("/members");
}

export async function revokeUserSessions(form: FormData) {
  await api(`/admin/users/${form.get("user_id")}/sessions/revoke`, {
    method: "POST",
  });
  revalidatePath("/members");
}

export async function deleteUser(form: FormData) {
  await api(`/admin/users/${form.get("user_id")}`, { method: "DELETE" });
  revalidatePath("/members");
}

export async function removeClientStatus(form: FormData) {
  await api(`/admin/client-status/${form.get("client_id")}`, { method: "DELETE" });
  revalidatePath("/clients");
}

export async function regenerateInvitation(
  _: InvitationLinkState,
  form: FormData,
): Promise<InvitationLinkState> {
  try {
    const invitation = await api<AdminInvitationIssued>(
      `/admin/invitations/${form.get("invitation_id")}/regenerate`,
      { method: "POST" },
    );
    return { invitationId: invitation.id, email: invitation.email, invitationUrl: invitation.invitation_url };
  } catch (error) {
    return { error: invitationError(error) };
  }
}

export async function cancelInvitation(form: FormData) {
  await api(`/admin/invitations/${form.get("invitation_id")}`, {
    method: "DELETE",
  });
  revalidatePath("/members");
}

export async function acceptInvitation(form: FormData) {
  if (identityConfig().mode === "oidc")
    redirect("/login?error=Invitations+are+disabled+for+managed+workspaces");
  const invitationId = String(form.get("invitation_id") ?? "");
  const password = String(form.get("password") ?? "");
  const invitation = await authPool.query<{
    email: string;
    role: string | null;
    organizationId: string;
    slug: string;
    expiresAt: Date;
  }>(
    `select i.email,i.role,i."organizationId",o.slug,i."expiresAt"
     from auth."invitation" i join auth."organization" o on o.id=i."organizationId"
     where i.id=$1 and i.status='pending'`,
    [invitationId],
  );
  const pending = invitation.rows[0];
  if (!pending || pending.expiresAt.getTime() <= Date.now())
    redirect("/login?error=Invitation+expired");
  const publicOrg = await authPool.query<{ id: string }>(
    "select id::text from public.organizations where slug=$1",
    [pending.slug],
  );
  if (!publicOrg.rowCount)
    throw new Error("Invitation organization is not provisioned");
  const created = await auth.api.signUpEmail({
    body: { name: pending.email, email: pending.email, password },
  });
  const role = pending.role === "admin" ? "admin" : "member";
  const connection = await authPool.connect();
  try {
    await connection.query("begin");
    await connection.query(
      'update auth."user" set "organizationId"=$1,"governanceRole"=$2,"emailVerified"=true,"updatedAt"=now() where id=$3',
      [publicOrg.rows[0].id, role, created.user.id],
    );
    await connection.query(
      'insert into auth."member" (id,"organizationId","userId",role,"createdAt") values ($1,$2,$3,$4,now())',
      [randomUUID(), pending.organizationId, created.user.id, role],
    );
    await connection.query(
      `insert into public.users (id,organization_id,subject,email,role,status,active)
       values ($1,$2,$3,$4,$5,'active',true)
       on conflict (organization_id,email) do update set
         subject=excluded.subject,role=excluded.role,status='active',active=true,
         tokens_valid_after=null,updated_at=now()`,
      [randomUUID(), publicOrg.rows[0].id, created.user.id, pending.email, role],
    );
    await connection.query(
      "update auth.\"invitation\" set status='accepted' where id=$1",
      [invitationId],
    );
    await connection.query("commit");
  } catch (error) {
    await connection.query("rollback");
    await authPool.query('delete from auth."user" where id=$1', [
      created.user.id,
    ]);
    throw error;
  } finally {
    connection.release();
  }
  await auth.api.signInEmail({
    headers: await headers(),
    body: { email: pending.email, password },
  });
  redirect("/sessions");
}

export async function downloadSession(form: FormData) {
  const result = await api<{ download_url: string }>(
    `/session-uploads/${form.get("session_id")}/download`,
    { method: "POST" },
  );
  redirect(result.download_url);
}

export async function updateSessionSharing(form: FormData) {
  const sessionId = String(form.get("session_id"));
  const mode = String(form.get("mode"));
  await api(`/session-uploads/${sessionId}/sharing`, {
    method: "PUT",
    body: JSON.stringify({
      mode,
      user_ids: mode === "selected" ? form.getAll("user_ids").map(String) : [],
    }),
  });
  revalidatePath("/sessions");
  revalidatePath(`/sessions/${sessionId}`);
}

export type SessionShareMember = { id: string; email: string };
export type SessionSharing = {
  mode: "private" | "workspace" | "selected";
  can_edit: boolean;
  recipient_count: number;
  recipients: SessionShareMember[];
};

export async function getSessionSharing(sessionId: string): Promise<SessionSharing> {
  return api<SessionSharing>(`/session-uploads/${encodeURIComponent(sessionId)}/sharing`);
}

export async function searchSessionShareMembers(
  query: string,
): Promise<{ items: SessionShareMember[]; error?: string }> {
  try {
    const params = new URLSearchParams();
    const trimmed = query.trim().slice(0, 200);
    if (trimmed) params.set("q", trimmed);
    return await api<{ items: SessionShareMember[] }>(
      `/session-uploads/members${params.size ? `?${params}` : ""}`,
    );
  } catch (error) {
    return {
      items: [],
      error: error instanceof Error ? error.message : "Members could not be loaded.",
    };
  }
}
