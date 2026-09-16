"use client";

import { useEffect, useRef, useState } from "react";
import Link from "next/link";
import { usePathname } from "next/navigation";
import {
  Check,
  ChevronsUpDown,
  Copy,
  ExternalLink,
  Monitor,
  Moon,
  Sun,
} from "lucide-react";
import { useTheme } from "next-themes";
import { logout } from "@/app/actions";
import {
  CurrentConfigurationDialog,
  type BlueConfigExport,
} from "@/components/current-configuration-dialog";
import { BrandLogo } from "@/components/brand-logo";
import { SettingsDialog } from "@/components/settings-dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import type { Branding } from "@/lib/branding";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
} from "@/components/ui/sidebar";

const workspaceItems = [{ href: "/sessions", label: "Sessions" }];
const adminItems = [
  { href: "/harnesses", label: "Harnesses" },
  { href: "/extensions", label: "Extensions" },
  { href: "/members", label: "Members" },
  { href: "/clients", label: "Clients" },
];

const DOCS_URL = "https://docs.bluee.sh";

export function DashboardSidebar({
  email,
  role,
  gatewayEnabled,
  currentRevision,
  blueConfig,
  initialBranding,
  deploymentVersion,
}: {
  email: string;
  role: "admin" | "member";
  gatewayEnabled: boolean;
  currentRevision: string;
  blueConfig: BlueConfigExport | null;
  initialBranding: Branding;
  deploymentVersion: string;
}) {
  const pathname = usePathname();
  const [configurationOpen, setConfigurationOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [branding, setBranding] = useState(initialBranding);
  const [revisionCopied, setRevisionCopied] = useState(false);
  const { theme, setTheme } = useTheme();
  const copyResetTimer = useRef<number | null>(null);

  async function copyRevision() {
    await navigator.clipboard.writeText(currentRevision);
    setRevisionCopied(true);
    if (copyResetTimer.current) window.clearTimeout(copyResetTimer.current);
    copyResetTimer.current = window.setTimeout(
      () => setRevisionCopied(false),
      2000,
    );
  }

  useEffect(
    () => () => {
      if (copyResetTimer.current) window.clearTimeout(copyResetTimer.current);
    },
    [],
  );
  const workspace = gatewayEnabled
    ? [...workspaceItems, { href: "/gateway", label: "Gateway" }]
    : workspaceItems;
  const itemList = (items: typeof adminItems) => items.map((item) => (
    <SidebarMenuItem key={item.href}>
      <SidebarMenuButton
        render={<Link href={item.href} />}
        isActive={pathname === item.href || pathname.startsWith(`${item.href}/`)}
        tooltip={item.label}
      >
        <span>{item.label}</span>
      </SidebarMenuButton>
    </SidebarMenuItem>
  ));
  return (
    <Sidebar collapsible="icon">
      <SidebarHeader>
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton
              size="lg"
              render={<Link href="/sessions" />}
              tooltip="Blue"
            >
              <BrandLogo
                url={branding.logo_url}
                placement="sidebar"
              />
              <span
                className="ml-auto font-mono text-[10px] text-muted-foreground group-data-[collapsible=icon]:hidden"
                title="Blue deployment image version"
              >
                v{deploymentVersion}
              </span>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarHeader>
      <SidebarContent>
        <SidebarGroup>
          <SidebarGroupLabel className="text-sidebar-foreground/50">
            Workspace
          </SidebarGroupLabel>
          <SidebarGroupContent>
            <SidebarMenu>{itemList(workspace)}</SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
        {role === "admin" && (
          <SidebarGroup>
            <SidebarGroupLabel className="text-sidebar-foreground/50">
              Administration
            </SidebarGroupLabel>
            <SidebarGroupContent>
              <SidebarMenu>{itemList(adminItems)}</SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        )}
      </SidebarContent>
      <SidebarFooter className="px-2 pt-2 pb-1">
        <SidebarMenu>
          <SidebarMenuItem>
            <DropdownMenu>
              <DropdownMenuTrigger
                render={
                  <SidebarMenuButton
                    size="lg"
                    aria-label={`Open user menu for ${email}`}
                  />
                }
              >
                <span className="grid min-w-0 flex-1 text-left text-sm leading-tight group-data-[collapsible=icon]:hidden">
                  <span className="truncate font-medium">{email}</span>
                  <span className="truncate text-xs capitalize text-muted-foreground">
                    {role}
                  </span>
                </span>
                <ChevronsUpDown className="ml-auto size-4 group-data-[collapsible=icon]:mx-auto" />
              </DropdownMenuTrigger>
              <DropdownMenuContent
                side="top"
                align="start"
                className="w-(--anchor-width) min-w-56"
              >
                <DropdownMenuGroup>
                  <DropdownMenuLabel className="px-2 py-1.5">
                    <span className="block truncate text-sm text-foreground">
                      {email}
                    </span>
                    <span className="block capitalize">{role}</span>
                  </DropdownMenuLabel>
                </DropdownMenuGroup>
                <DropdownMenuSeparator />
                <div className="grid gap-1.5 p-2">
                  <label
                    htmlFor="current-revision"
                    className="text-xs font-medium text-muted-foreground"
                  >
                    Current revision
                  </label>
                  <div className="flex gap-1.5">
                    <Input
                      id="current-revision"
                      value={currentRevision}
                      readOnly
                      className="font-mono text-xs"
                      onFocus={(event) => event.currentTarget.select()}
                    />
                    <Button
                      type="button"
                      variant="outline"
                      size="icon"
                      onClick={copyRevision}
                      aria-label={
                        revisionCopied
                          ? "Current revision copied"
                          : "Copy current revision"
                      }
                      title={revisionCopied ? "Copied" : "Copy revision"}
                    >
                      {revisionCopied ? <Check /> : <Copy />}
                    </Button>
                  </div>
                </div>
                <DropdownMenuSeparator />
                <DropdownMenuItem
                  render={
                    <a
                      href={DOCS_URL}
                      target="_blank"
                      rel="noreferrer"
                      className="w-full"
                    />
                  }
                >
                  Documentation
                  <ExternalLink className="ml-auto" />
                </DropdownMenuItem>
                <DropdownMenuSub>
                  <DropdownMenuSubTrigger>
                    <Monitor />
                    Theme
                  </DropdownMenuSubTrigger>
                  <DropdownMenuSubContent>
                    <DropdownMenuRadioGroup
                      value={theme ?? "system"}
                      onValueChange={setTheme}
                    >
                      <DropdownMenuRadioItem value="system">
                        <Monitor />
                        System
                      </DropdownMenuRadioItem>
                      <DropdownMenuRadioItem value="light">
                        <Sun />
                        Light
                      </DropdownMenuRadioItem>
                      <DropdownMenuRadioItem value="dark">
                        <Moon />
                        Dark
                      </DropdownMenuRadioItem>
                    </DropdownMenuRadioGroup>
                  </DropdownMenuSubContent>
                </DropdownMenuSub>
                {role === "admin" && (
                  <>
                    <DropdownMenuSeparator />
                    <DropdownMenuItem
                      onClick={() => setSettingsOpen(true)}
                    >
                      Settings
                    </DropdownMenuItem>
                    {blueConfig && (
                      <DropdownMenuItem
                        onClick={() => setConfigurationOpen(true)}
                      >
                        Current configuration
                      </DropdownMenuItem>
                    )}
                  </>
                )}
                <DropdownMenuSeparator />
                <DropdownMenuItem
                  variant="destructive"
                  render={
                    <button
                      type="submit"
                      form="sidebar-logout"
                      className="w-full"
                    />
                  }
                >
                  Sign out
                </DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
          </SidebarMenuItem>
        </SidebarMenu>
        <form id="sidebar-logout" action={logout} />
      </SidebarFooter>
      {blueConfig && (
        <CurrentConfigurationDialog
          open={configurationOpen}
          onOpenChange={setConfigurationOpen}
          value={blueConfig}
        />
      )}
      {settingsOpen && (
        <SettingsDialog
          open={settingsOpen}
          onOpenChange={setSettingsOpen}
          value={branding}
          onSaved={setBranding}
        />
      )}
    </Sidebar>
  );
}
