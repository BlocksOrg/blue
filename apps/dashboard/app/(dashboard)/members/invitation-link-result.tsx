"use client";

import { useEffect, useRef, useState } from "react";
import { Check, Copy } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";

export function InvitationLinkResult({ email, url }: { email: string; url: string }) {
  const [copyStatus, setCopyStatus] = useState<"idle" | "copied" | "failed">("idle");
  const resetTimer = useRef<number | null>(null);

  useEffect(() => () => {
    if (resetTimer.current) window.clearTimeout(resetTimer.current);
  }, []);

  async function copy() {
    try {
      await navigator.clipboard.writeText(url);
      setCopyStatus("copied");
    } catch {
      setCopyStatus("failed");
    }
    if (resetTimer.current) window.clearTimeout(resetTimer.current);
    resetTimer.current = window.setTimeout(() => setCopyStatus("idle"), 2000);
  }

  return (
    <div className="grid gap-4">
      <p className="text-sm text-muted-foreground">Share this link with <strong className="text-foreground">{email}</strong>. The email address is pinned to this invitation.</p>
      <div className="grid gap-2">
        <Label htmlFor="issued-invitation-url">Invitation link</Label>
        <div className="flex gap-2">
          <Input id="issued-invitation-url" value={url} readOnly onFocus={(event) => event.currentTarget.select()} />
          <Button
            type="button"
            variant="outline"
            size="icon"
            onClick={copy}
            aria-label={copyStatus === "copied" ? "Invitation link copied" : "Copy invitation link"}
            title={copyStatus === "copied" ? "Copied" : "Copy invitation link"}
          >
            {copyStatus === "copied" ? <Check /> : <Copy />}
          </Button>
        </div>
        {copyStatus === "failed" && (
          <p className="text-sm" role="alert">Copy failed. Select and copy the link manually.</p>
        )}
      </div>
    </div>
  );
}
