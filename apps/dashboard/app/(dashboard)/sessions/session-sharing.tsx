"use client";

import * as React from "react";
import { useRouter } from "next/navigation";
import { Ellipsis } from "lucide-react";
import {
  getSessionSharing,
  searchSessionShareMembers,
  updateSessionSharing,
  type SessionShareMember,
} from "../../actions";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { downloadSession } from "../../actions";

type SharingMode = "private" | "workspace" | "selected";

type ShareDialogProps = {
  sessionId: string;
  sessionLabel: string;
  trigger?: React.ReactElement;
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
};

export function SessionShareDialog({
  sessionId,
  sessionLabel,
  trigger,
  open,
  onOpenChange,
}: ShareDialogProps) {
  const router = useRouter();
  const [internalOpen, setInternalOpen] = React.useState(false);
  const shown = open ?? internalOpen;
  const setShown = onOpenChange ?? setInternalOpen;
  const [mode, setMode] = React.useState<SharingMode>("private");
  const [query, setQuery] = React.useState("");
  const [members, setMembers] = React.useState<SessionShareMember[]>([]);
  const [selected, setSelected] = React.useState<Map<string, SessionShareMember>>(new Map());
  const [loading, setLoading] = React.useState(false);
  const [searching, setSearching] = React.useState(false);
  const [saving, startSaving] = React.useTransition();
  const [error, setError] = React.useState<string>();

  React.useEffect(() => {
    if (!shown) return;
    let cancelled = false;
    setLoading(true);
    setError(undefined);
    setQuery("");
    Promise.all([getSessionSharing(sessionId), searchSessionShareMembers("")])
      .then(([sharing, result]) => {
        if (cancelled) return;
        setMode(sharing.mode);
        setSelected(new Map(sharing.recipients.map((member) => [member.id, member])));
        setMembers(result.items);
        setError(result.error);
      })
      .catch((cause) => {
        if (!cancelled) setError(cause instanceof Error ? cause.message : "Sharing could not be loaded.");
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [sessionId, shown]);

  React.useEffect(() => {
    if (!shown || loading) return;
    let cancelled = false;
    const timeout = window.setTimeout(() => {
      setSearching(true);
      searchSessionShareMembers(query)
        .then((result) => {
          if (cancelled) return;
          setMembers(result.items);
          setError(result.error);
        })
        .finally(() => {
          if (!cancelled) setSearching(false);
        });
    }, 250);
    return () => {
      cancelled = true;
      window.clearTimeout(timeout);
    };
  }, [loading, query, shown]);

  function toggle(member: SessionShareMember, checked: boolean) {
    setSelected((current) => {
      const next = new Map(current);
      if (checked) next.set(member.id, member);
      else next.delete(member.id);
      return next;
    });
  }

  function save() {
    const form = new FormData();
    form.set("session_id", sessionId);
    form.set("mode", mode);
    if (mode === "selected") {
      for (const id of selected.keys()) form.append("user_ids", id);
    }
    setError(undefined);
    startSaving(async () => {
      try {
        await updateSessionSharing(form);
        setShown(false);
        router.refresh();
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : "Sharing could not be saved.");
      }
    });
  }

  return (
    <Dialog open={shown} onOpenChange={setShown}>
      {trigger && <DialogTrigger render={trigger} />}
      <DialogContent className="max-h-[calc(100svh-2rem)] overflow-y-auto sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Share session</DialogTitle>
          <DialogDescription className="truncate">
            Choose who can view, download, and resume {sessionLabel}.
          </DialogDescription>
        </DialogHeader>
        {loading ? (
          <p className="py-8 text-center text-sm text-muted-foreground">Loading sharing settings…</p>
        ) : (
          <div className="grid gap-4">
            <div className="grid gap-2">
              <Label htmlFor={`sharing-mode-${sessionId}`}>Access</Label>
              <select
                id={`sharing-mode-${sessionId}`}
                value={mode}
                onChange={(event) => setMode(event.target.value as SharingMode)}
                className="h-9 rounded-md border bg-background px-3 text-sm"
              >
                <option value="private">Private</option>
                <option value="workspace">All active workspace members</option>
                <option value="selected">Selected active members</option>
              </select>
            </div>
            {mode === "selected" && (
              <div className="grid gap-3">
                <div className="grid gap-2">
                  <Label htmlFor={`member-search-${sessionId}`}>Members</Label>
                  <Input
                    id={`member-search-${sessionId}`}
                    value={query}
                    maxLength={200}
                    placeholder="Search by email"
                    onChange={(event) => setQuery(event.target.value)}
                  />
                </div>
                <div className="max-h-48 overflow-y-auto rounded-md border">
                  {members.map((member) => (
                    <label key={member.id} className="flex cursor-pointer items-center gap-2 border-b px-3 py-2 text-sm last:border-b-0">
                      <input
                        type="checkbox"
                        checked={selected.has(member.id)}
                        onChange={(event) => toggle(member, event.target.checked)}
                      />
                      <span className="min-w-0 truncate" title={member.email}>{member.email}</span>
                    </label>
                  ))}
                  {!members.length && (
                    <p className="px-3 py-6 text-center text-sm text-muted-foreground">
                      {searching ? "Searching…" : "No active members found."}
                    </p>
                  )}
                </div>
                {selected.size > 0 && (
                  <div className="grid max-h-28 gap-1 overflow-y-auto rounded-md bg-muted/40 p-2">
                    <span className="px-1 text-xs font-medium text-muted-foreground">Selected ({selected.size})</span>
                    {[...selected.values()].map((member) => (
                      <button
                        key={member.id}
                        type="button"
                        className="truncate rounded px-1 py-0.5 text-left text-xs hover:bg-muted"
                        title={`Remove ${member.email}`}
                        onClick={() => toggle(member, false)}
                      >
                        {member.email}
                      </button>
                    ))}
                  </div>
                )}
              </div>
            )}
            <p className="rounded-md border border-amber-500/30 bg-amber-500/5 p-3 text-xs text-muted-foreground">
              Session data may contain raw prompts, tool results, file paths, and other sensitive conversation content. Revoking access cannot remove copies already downloaded or restored.
            </p>
            {error && <p role="alert" className="text-sm text-destructive">{error.replace(/^\d+:\s*/, "")}</p>}
          </div>
        )}
        <DialogFooter>
          <Button type="button" variant="outline" onClick={() => setShown(false)}>Cancel</Button>
          <Button
            type="button"
            onClick={save}
            disabled={loading || saving || (mode === "selected" && selected.size === 0)}
          >
            {saving ? "Saving…" : "Save sharing"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

export function SessionSharingSummary({
  sessionId,
  mode,
  recipientCount,
}: {
  sessionId: string;
  mode: SharingMode;
  recipientCount: number;
}) {
  const [recipients, setRecipients] = React.useState<SessionShareMember[]>();
  const [error, setError] = React.useState<string>();
  const shared = mode !== "private" && recipientCount > 0;
  const label = mode === "workspace"
    ? `Workspace (${recipientCount})`
    : mode === "selected"
      ? `${recipientCount} ${recipientCount === 1 ? "person" : "people"}`
      : "Private";

  if (!shared) return <Badge variant="outline">Private</Badge>;

  function loadRecipients() {
    if (recipients || error) return;
    getSessionSharing(sessionId)
      .then((sharing) => setRecipients(sharing.recipients))
      .catch((cause) => setError(cause instanceof Error ? cause.message : "Recipients could not be loaded."));
  }

  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        onClick={loadRecipients}
        render={<button type="button" className="rounded-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring" aria-label={`Shared with ${label}. Show recipients.`} />}
      >
        <Badge variant="secondary">{label}</Badge>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="w-72">
        <DropdownMenuLabel>Shared with</DropdownMenuLabel>
        <div className="max-h-52 overflow-y-auto px-1 py-1">
          {!recipients && !error && <p className="px-1 py-2 text-sm text-muted-foreground">Loading recipients…</p>}
          {error && <p className="px-1 py-2 text-sm text-destructive">{error}</p>}
          {recipients?.map((recipient) => (
            <p key={recipient.id} className="truncate rounded px-1 py-1 text-sm" title={recipient.email}>{recipient.email}</p>
          ))}
        </div>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

export function SessionActions({
  sessionId,
  sessionLabel,
  canShare,
}: {
  sessionId: string;
  sessionLabel: string;
  canShare: boolean;
}) {
  const [shareOpen, setShareOpen] = React.useState(false);
  return (
    <>
      <DropdownMenu>
        <DropdownMenuTrigger
          render={
            <Button
              type="button"
              variant="ghost"
              size="icon-sm"
              aria-label={`Actions for ${sessionLabel}`}
            />
          }
        >
          <Ellipsis />
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="w-36">
          {canShare && <DropdownMenuItem onClick={() => setShareOpen(true)}>Share</DropdownMenuItem>}
          <form action={downloadSession}>
            <input type="hidden" name="session_id" value={sessionId} />
            <DropdownMenuItem render={<button type="submit" className="w-full" />}>Download</DropdownMenuItem>
          </form>
        </DropdownMenuContent>
      </DropdownMenu>
      {canShare && (
        <SessionShareDialog
          sessionId={sessionId}
          sessionLabel={sessionLabel}
          open={shareOpen}
          onOpenChange={setShareOpen}
        />
      )}
    </>
  );
}
