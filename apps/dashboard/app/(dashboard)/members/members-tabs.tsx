"use client";

import type { ReactNode } from "react";
import { usePathname, useRouter, useSearchParams } from "next/navigation";
import { Badge } from "@/components/ui/badge";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { resolveMembersTab } from "@/lib/members-view";

export function MembersTabs({
  managed,
  memberCount,
  invitationCount,
  members,
  invitations,
  identity,
}: {
  managed: boolean;
  memberCount: number;
  invitationCount: number;
  members: ReactNode;
  invitations: ReactNode;
  identity: ReactNode;
}) {
  const pathname = usePathname();
  const router = useRouter();
  const searchParams = useSearchParams();
  const tab = resolveMembersTab(searchParams.get("tab") ?? "", managed);

  function selectTab(next: string | number) {
    if (
      (next !== "members" && next !== "invited" && next !== "identity") ||
      (managed && next === "invited") ||
      (!managed && next === "identity")
    ) {
      return;
    }
    const params = new URLSearchParams(searchParams.toString());
    params.delete("page");
    if (next === "members") params.delete("tab");
    else params.set("tab", next);
    const query = params.toString();
    router.replace(query ? `${pathname}?${query}` : pathname, { scroll: false });
  }

  return (
    <Tabs value={tab} onValueChange={selectTab} className="gap-5">
      <div className="overflow-x-auto border-b">
        <TabsList variant="line" className="h-10 min-w-max gap-5 px-1">
          <TabsTrigger value="members" className="flex-none px-2">
            Members <Badge variant="secondary">{memberCount}</Badge>
          </TabsTrigger>
          <TabsTrigger value="invited" disabled={managed} className="flex-none px-2">
            Invited <Badge variant="secondary">{invitationCount}</Badge>
          </TabsTrigger>
          {managed && (
            <TabsTrigger value="identity" className="flex-none px-2">
              Identity &amp; provisioning
            </TabsTrigger>
          )}
        </TabsList>
      </div>
      <TabsContent value="members">{members}</TabsContent>
      {!managed && <TabsContent value="invited">{invitations}</TabsContent>}
      {managed && <TabsContent value="identity">{identity}</TabsContent>}
    </Tabs>
  );
}
