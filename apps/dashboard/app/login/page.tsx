import { headers } from "next/headers";
import { redirect } from "next/navigation";
import { LoginForm } from "./login-form";
import {
  Card,
  CardContent,
  CardHeader,
} from "@/components/ui/card";
import { BrandLogo } from "@/components/brand-logo";
import { auth } from "@/lib/auth";
import { getBranding } from "@/lib/branding-server";
import {
  DEFAULT_CALLBACK_PATH,
  safeCallbackPath,
} from "@/lib/safe-callback-path";

export default async function Login({
  searchParams,
}: {
  searchParams: Promise<{ callbackURL?: string }>;
}) {
  const { callbackURL } = await searchParams;
  const callbackPath = safeCallbackPath(callbackURL);
  const session = await auth.api.getSession({ headers: await headers() });
  if (session) {
    redirect(
      callbackPath === "/login" ? DEFAULT_CALLBACK_PATH : callbackPath,
    );
  }

  const branding = await getBranding();
  return (
    <main className="grid min-h-svh place-items-center bg-background p-4">
      <Card className="w-full max-w-sm rounded-lg border-0 bg-card/50 shadow-none ring-0">
        <CardHeader>
          <BrandLogo url={branding.logo_url} placement="login" />
        </CardHeader>
        <CardContent>
          <LoginForm callbackURL={callbackPath} />
        </CardContent>
      </Card>
    </main>
  );
}
