import "server-only";
import { headers as requestHeaders } from "next/headers";
import { notFound, redirect } from "next/navigation";

export type Identity = {
  id: string;
  sub: string;
  email: string;
  org_id: string;
  role: "admin" | "member";
  expires_at: number;
  current_revision: string;
};

const baseUrl = process.env.CONTROL_API_URL ?? "http://localhost:8080";

export async function api<T>(path: string, init: RequestInit = {}): Promise<T> {
  const headers = new Headers(init.headers);
  const incoming = await requestHeaders();
  const cookie = incoming.get("cookie");
  if (cookie) headers.set("cookie", cookie);
  if (init.body && !headers.has("content-type"))
    headers.set("content-type", "application/json");
  const request = () =>
    fetch(new URL(path, baseUrl), {
      ...init,
      headers,
      cache: "no-store",
    });
  let response = await request();
  if (response.status === 401) {
    const { auth } = await import("./auth");
    const session = await auth.api.getSession({ headers: incoming });
    if (!session) redirect("/logout");
    await response.body?.cancel();
    response = await request();
  }
  if (!response.ok) {
    const message = await response.text();
    throw new Error(`${response.status}: ${message}`);
  }
  if (response.status === 204) return undefined as T;
  return response.json() as Promise<T>;
}

export async function requireIdentity(): Promise<Identity> {
  return api<Identity>("/auth/me");
}

export async function requireAdminIdentity(): Promise<Identity> {
  const incoming = await requestHeaders();
  const { auth } = await import("./auth");
  const session = await auth.api.getSession({ headers: incoming });
  if (!session) redirect("/logout");
  if (session.user.governanceRole !== "admin") notFound();
  const identity = await requireIdentity();
  if (identity.role !== "admin") notFound();
  return identity;
}
