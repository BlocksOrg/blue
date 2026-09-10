import { toNextJsHandler } from "better-auth/next-js";
import {
  auth,
  bindDeviceAuthorizationSession,
  createDeviceBrowserLink,
  ensureAuthBootstrap,
  invalidateDeviceBrowserLink,
} from "../../../../lib/auth";
import { bootstrapEmail, identityConfig } from "../../../../lib/identity-config";

const handlers = toNextJsHandler(auth);

function ready(handler: (request: Request) => Promise<Response>) {
  return async (request: Request) => {
    await ensureAuthBootstrap();
    const pathname = new URL(request.url).pathname;
    const isDeviceDecision =
      pathname.endsWith("/device/approve") ||
      pathname.endsWith("/device/deny");
    const deviceDecision = isDeviceDecision
      ? await request.clone().json().catch(() => ({}))
      : undefined;
    if (
      identityConfig().mode === "oidc" &&
      pathname.endsWith("/sign-in/email")
    ) {
      const body = await request.clone().json().catch(() => ({}));
      if (String(body.email ?? "").trim().toLowerCase() !== bootstrapEmail())
        return Response.json(
          { error: "identity provider sign-in required" },
          { status: 403 },
        );
    }
    // Accounts are provisioned by bootstrap or organization invitations. Do
    // not expose Better Auth's otherwise-public email signup endpoint.
    if (
      pathname.endsWith("/sign-up/email") ||
      pathname.includes("/organization/invite-member") ||
      pathname.includes("/organization/cancel-invitation") ||
      pathname.includes("/organization/remove-member") ||
      pathname.includes("/organization/update-member-role") ||
      pathname.includes("/api/auth/admin/")
    ) {
      return Response.json({ error: "invitation required" }, { status: 403 });
    }
    if (
      pathname.endsWith("/device/approve") &&
      deviceDecision?.userCode &&
      !(await bindDeviceAuthorizationSession(
        String(deviceDecision.userCode),
        request.headers,
      ))
    )
      return Response.json(
        { error: "device authorization session binding failed" },
        { status: 400 },
      );
    const response = await handler(request);
    if (pathname.endsWith("/device/code") && response.ok) {
      const body = (await response.clone().json()) as {
        user_code?: string;
        verification_uri_complete?: string;
      };
      if (!body.user_code)
        return Response.json(
          { error: "invalid device authorization response" },
          { status: 500 },
        );
      body.verification_uri_complete = await createDeviceBrowserLink(
        body.user_code,
      );
      const responseHeaders = new Headers(response.headers);
      responseHeaders.delete("content-length");
      responseHeaders.set("cache-control", "no-store");
      responseHeaders.set("pragma", "no-cache");
      return Response.json(body, {
        status: response.status,
        headers: responseHeaders,
      });
    }
    if (isDeviceDecision && response.ok) {
      const userCode = String(deviceDecision?.userCode ?? "");
      if (userCode) await invalidateDeviceBrowserLink(userCode);
    }
    return response;
  };
}

export const GET = ready(handlers.GET);
export const POST = ready(handlers.POST);
export const PATCH = ready(handlers.PATCH);
export const PUT = ready(handlers.PUT);
export const DELETE = ready(handlers.DELETE);
