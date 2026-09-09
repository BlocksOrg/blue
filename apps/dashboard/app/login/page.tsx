import { LoginForm } from "./login-form";
import {
  Card,
  CardContent,
  CardHeader,
} from "@/components/ui/card";
import { BrandLogo } from "@/components/brand-logo";
import { getBranding } from "@/lib/branding-server";
import { safeCallbackPath } from "@/lib/safe-callback-path";

export default async function Login({
  searchParams,
}: {
  searchParams: Promise<{ callbackURL?: string }>;
}) {
  const { callbackURL } = await searchParams;
  const branding = await getBranding();
  return (
    <main className="grid min-h-svh place-items-center bg-background p-4">
      <Card className="w-full max-w-sm rounded-lg border-0 bg-card/50 shadow-none ring-0">
        <CardHeader>
          <BrandLogo url={branding.logo_url} placement="login" />
        </CardHeader>
        <CardContent>
          <LoginForm callbackURL={safeCallbackPath(callbackURL)} />
        </CardContent>
      </Card>
    </main>
  );
}
