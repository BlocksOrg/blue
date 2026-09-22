"use client";

import { useEffect, useState, type ReactNode } from "react";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";

export type GatewayTab = "overview" | "models" | "keys" | "logs";

export function GatewayTabs({
  admin,
  activeTab,
  overview,
  models,
  keys,
  logs,
}: {
  admin: boolean;
  activeTab: GatewayTab;
  overview: ReactNode;
  models: ReactNode;
  keys: ReactNode;
  logs: ReactNode;
}) {
  const [tab, setTab] = useState<GatewayTab>(activeTab);

  useEffect(() => setTab(activeTab), [activeTab]);

  function selectTab(next: string | number) {
    if (!admin && next !== "keys") return;
    if (next !== "overview" && next !== "models" && next !== "keys" && next !== "logs") return;
    setTab(next);
    const url = new URL(window.location.href);
    if (next === "overview") url.searchParams.delete("tab");
    else url.searchParams.set("tab", next);
    window.history.replaceState(window.history.state, "", url);
  }

  return (
    <Tabs value={tab} onValueChange={selectTab} className="gap-5">
      <div className="overflow-x-auto border-b">
        <TabsList variant="line" className="h-10 min-w-max gap-5 px-1">
          {admin && <TabsTrigger value="overview" className="flex-none px-2">Overview</TabsTrigger>}
          {admin && <TabsTrigger value="models" className="flex-none px-2">Models</TabsTrigger>}
          <TabsTrigger value="keys" className="flex-none px-2">Key</TabsTrigger>
          {admin && <TabsTrigger value="logs" className="flex-none px-2">Logs</TabsTrigger>}
        </TabsList>
      </div>
      {admin && <TabsContent value="overview">{overview}</TabsContent>}
      {admin && <TabsContent value="models">{models}</TabsContent>}
      <TabsContent value="keys">{keys}</TabsContent>
      {admin && <TabsContent value="logs">{logs}</TabsContent>}
    </Tabs>
  );
}
