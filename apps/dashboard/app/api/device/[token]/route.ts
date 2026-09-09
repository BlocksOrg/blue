import { auth, authPool, invalidateDeviceBrowserLink } from "../../../../lib/auth";

async function pendingUserCode(token: string): Promise<string | undefined> {
  if (!/^[A-Za-z0-9_-]{43}$/.test(token)) return undefined;
  const result = await authPool.query<{ userCode: string }>(
    `select "userCode" from auth."deviceCode"
     where "browserToken"=$1 and status='pending' and "expiresAt">now()`,
    [token],
  );
  return result.rows[0]?.userCode;
}

async function authenticated(request: Request): Promise<boolean> {
  return Boolean(await auth.api.getSession({ headers: request.headers }));
}

export async function GET(
  request: Request,
  { params }: { params: Promise<{ token: string }> },
) {
  if (!(await authenticated(request)))
    return Response.json({ error: "authentication required" }, { status: 401 });
  const userCode = await pendingUserCode((await params).token);
  if (!userCode)
    return Response.json({ error: "invalid or expired link" }, { status: 400 });
  try {
    const verification = await auth.api.deviceVerify({
      query: { user_code: userCode },
      headers: request.headers,
    });
    return Response.json(verification, {
      headers: { "cache-control": "no-store", pragma: "no-cache" },
    });
  } catch {
    return Response.json(
      { error: "authorization request is unavailable" },
      { status: 400 },
    );
  }
}

export async function POST(
  request: Request,
  { params }: { params: Promise<{ token: string }> },
) {
  if (!(await authenticated(request)))
    return Response.json({ error: "authentication required" }, { status: 401 });
  const action = String((await request.json().catch(() => ({}))).action ?? "");
  if (action !== "approve" && action !== "deny")
    return Response.json({ error: "invalid action" }, { status: 400 });

  const userCode = await pendingUserCode((await params).token);
  if (!userCode)
    return Response.json({ error: "invalid or expired link" }, { status: 400 });
  try {
    await auth.api.deviceVerify({
      query: { user_code: userCode },
      headers: request.headers,
    });
    const result =
      action === "approve"
        ? await auth.api.deviceApprove({
            body: { userCode },
            headers: request.headers,
          })
        : await auth.api.deviceDeny({
            body: { userCode },
            headers: request.headers,
          });
    await invalidateDeviceBrowserLink(userCode);
    return Response.json(result, {
      headers: { "cache-control": "no-store", pragma: "no-cache" },
    });
  } catch {
    return Response.json(
      { error: "authorization request is unavailable" },
      { status: 400 },
    );
  }
}
