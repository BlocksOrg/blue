import type { Metadata, Viewport } from "next";
import "./globals.css";
import { Geist } from "next/font/google";
import { cn } from "@/lib/utils";
import { TooltipProvider } from "@/components/ui/tooltip";
import { getBranding } from "@/lib/branding-server";
import { resolveBranding } from "@/lib/branding";
import { StartupConsole } from "@/components/startup-console";
import { ThemeProvider } from "@/components/theme-provider";

const geist = Geist({ subsets: ["latin"], variable: "--font-sans" });

export async function generateMetadata(): Promise<Metadata> {
  const branding = await getBranding();
  const resolved = resolveBranding(branding);
  return {
    title: "Blue",
    description: "Governance control plane",
    icons: branding.favicon_url
      ? { icon: branding.favicon_url }
      : {
          icon: [
            {
              url: resolved.favicon_url,
              type: "image/png",
              sizes: "92x92",
            },
          ],
        },
  };
}

export const viewport: Viewport = {
  colorScheme: "light dark",
};

export default function RootLayout({
  children,
}: Readonly<{ children: React.ReactNode }>) {
  const deploymentVersion = process.env.BLUE_DEPLOYMENT_VERSION ?? "development";
  return (
    <html
      lang="en"
      suppressHydrationWarning
      className={cn("font-sans", geist.variable)}
    >
      <body className="min-h-svh antialiased">
        <ThemeProvider>
          <StartupConsole version={deploymentVersion} />
          <TooltipProvider>{children}</TooltipProvider>
        </ThemeProvider>
      </body>
    </html>
  );
}
