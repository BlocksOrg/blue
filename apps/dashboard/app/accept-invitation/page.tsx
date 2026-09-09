import { acceptInvitation } from "../actions";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { authPool } from "../../lib/auth";
import { identityConfig } from "../../lib/identity-config";
import { redirect } from "next/navigation";

export default async function AcceptInvitation({
  searchParams,
}: {
  searchParams: Promise<{ id?: string }>;
}) {
  if (identityConfig().mode === "oidc")
    redirect("/login?error=Invitations+are+disabled+for+managed+workspaces");
  const { id = "" } = await searchParams;
  const invitation = await authPool.query<{ email: string }>(
    `select email from auth."invitation"
     where id=$1 and status='pending' and "expiresAt">now()`,
    [id],
  );
  const email = invitation.rows[0]?.email;
  return (
    <main className="grid min-h-svh place-items-center bg-background p-4">
      <Card className="w-full max-w-sm rounded-lg bg-card/50 shadow-2xl shadow-black/20">
        <CardHeader>
          <CardTitle className="text-xl">Accept invitation</CardTitle>
        </CardHeader>
        <CardContent>
          <form action={acceptInvitation} className="grid gap-4">
            <input type="hidden" name="invitation_id" value={id} />
            <div className="grid gap-2">
              <Label>Email</Label>
              <Input
                value={email ?? "Invitation unavailable"}
                title={email ?? "Invitation unavailable"}
                disabled
              />
            </div>
            <div className="grid gap-2">
              <Label htmlFor="new-password">Password</Label>
              <Input
                id="new-password"
                name="password"
                type="password"
                minLength={12}
                autoComplete="new-password"
                required
                autoFocus
              />
            </div>
            <Button type="submit" size="lg" disabled={!email}>
              Create account
            </Button>
          </form>
        </CardContent>
      </Card>
    </main>
  );
}
