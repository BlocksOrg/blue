import { headers } from "next/headers";
import { redirect } from "next/navigation";
import type { Metadata } from "next";
import { auth, authPool } from "../../../lib/auth";
import { DeviceApproval } from "../device-approval";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";

export const dynamic = "force-dynamic";
export const metadata: Metadata = { referrer: "no-referrer" };

function InvalidLink() {
  return (
    <Card className="w-full max-w-sm rounded-lg border-0 bg-card/50 shadow-none ring-0">
      <CardHeader>
        <CardTitle className="text-xl">Authorization link unavailable</CardTitle>
        <CardDescription>
          This temporary link is invalid, expired, or has already been used.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <p className="text-sm text-muted-foreground">
          Return to the terminal and run{" "}
          <code className="font-mono">blue login</code> again.
        </p>
      </CardContent>
    </Card>
  );
}

export default async function DeviceLinkPage({
  params,
}: {
  params: Promise<{ token: string }>;
}) {
  const { token } = await params;
  const result = /^[A-Za-z0-9_-]{43}$/.test(token)
    ? await authPool.query<{ userCode: string }>(
        `select "userCode" from auth."deviceCode"
         where "browserToken"=$1 and status='pending' and "expiresAt">now()`,
        [token],
      )
    : { rows: [] };
  const userCode = result.rows[0]?.userCode;

  if (!userCode)
    return (
      <main className="grid min-h-svh place-items-center bg-background p-4">
        <InvalidLink />
      </main>
    );

  const session = await auth.api.getSession({ headers: await headers() });
  const returnPath = `/device/${token}`;
  if (!session)
    redirect(`/login?callbackURL=${encodeURIComponent(returnPath)}`);

  return (
    <main className="grid min-h-svh place-items-center bg-background p-4">
      <Card className="w-full max-w-sm rounded-lg border-0 bg-card/50 shadow-none ring-0">
        <CardHeader>
          <CardTitle className="text-xl">Authorize the CLI</CardTitle>
          <CardDescription>
            Confirm that this code matches the one shown in your terminal.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <DeviceApproval
            code={userCode}
            browserToken={token}
            returnPath={returnPath}
          />
        </CardContent>
      </Card>
    </main>
  );
}
