import { DashboardSidebar } from "@/components/dashboard-sidebar";
import {
  SidebarInset,
  SidebarProvider,
  SidebarTrigger,
} from "@/components/ui/sidebar";
import type { CSSProperties } from "react";
import type { BlueConfigExport } from "@/components/current-configuration-dialog";
import { api, requireIdentity } from "../../lib/api";
import { getBranding } from "@/lib/branding-server";
import { BrandLogo } from "@/components/brand-logo";

export default async function DashboardLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  const me = await requireIdentity();
  const [gateway, blueConfig, branding] = await Promise.all([
    api<{ enabled: boolean }>("/gateway/status"),
    me.role === "admin"
      ? api<BlueConfigExport>("/admin/blue-config/export")
      : Promise.resolve(null),
    getBranding(),
  ]);
  const deploymentVersion = process.env.BLUE_DEPLOYMENT_VERSION ?? "development";
  return (
    <SidebarProvider
      className="bg-card"
      style={{ "--sidebar": "var(--background)" } as CSSProperties}
    >
      <DashboardSidebar
        email={me.email}
        role={me.role}
        gatewayEnabled={gateway.enabled}
        currentRevision={me.current_revision}
        blueConfig={blueConfig}
        initialBranding={branding}
        deploymentVersion={deploymentVersion}
      />
      <SidebarInset className="bg-card">
        <header className="sticky top-0 z-20 flex h-12 shrink-0 items-center border-b bg-card/90 px-3 backdrop-blur md:hidden">
          <SidebarTrigger />
          <BrandLogo url={branding.logo_url} placement="mobile" />
        </header>
        <main className="flex w-full min-w-0 flex-1 flex-col px-4 py-6 sm:px-6 lg:px-8 lg:py-7">
          {children}
        </main>
      </SidebarInset>
    </SidebarProvider>
  );
}
