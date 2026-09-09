"use client";

import { useEffect, useState } from "react";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";

export function DeviceApproval({
  code,
  browserToken,
  returnPath,
}: {
  code: string;
  browserToken: string;
  returnPath: string;
}) {
  const [message, setMessage] = useState("");
  const [pendingAction, setPendingAction] = useState<"approve" | "deny" | null>(
    null,
  );
  const [ready, setReady] = useState(false);
  const [outcome, setOutcome] = useState<"approved" | "denied" | null>(null);

  useEffect(() => {
    let active = true;
    async function claim() {
      try {
        const verification = await fetch(`/api/device/${browserToken}`);
        if (verification.status === 401) {
          window.location.assign(
            `/login?callbackURL=${encodeURIComponent(returnPath)}`,
          );
          return;
        }
        const body = (await verification.json().catch(() => ({}))) as {
          status?: string;
        };
        if (!verification.ok || body.status !== "pending") {
          if (active)
            setMessage("This authorization request is no longer available.");
          return;
        }
        if (active) setReady(true);
      } catch {
        if (active)
          setMessage(
            "Authorization could not reach the server. Check the dashboard connection and try again.",
          );
      }
    }
    void claim();
    return () => {
      active = false;
    };
  }, [browserToken, returnPath]);

  async function decide(action: "approve" | "deny") {
    setPendingAction(action);
    setMessage("");
    try {
      const response = await fetch(`/api/device/${browserToken}`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ action }),
      });
      if (response.status === 401) {
        window.location.assign(
          `/login?callbackURL=${encodeURIComponent(returnPath)}`,
        );
        return;
      }
      if (response.ok) {
        setReady(false);
        setOutcome(action === "approve" ? "approved" : "denied");
      } else {
        setMessage("The code is invalid, expired, or belongs to another account.");
      }
    } catch {
      setMessage(
        "Authorization could not reach the server. Check the dashboard connection and try again.",
      );
    } finally {
      setPendingAction(null);
    }
  }

  if (outcome === "approved")
    return (
      <Alert className="p-4">
        <AlertTitle>CLI authorized</AlertTitle>
        <AlertDescription>
          Authorization succeeded. You can close this window and return to the
          terminal.
        </AlertDescription>
      </Alert>
    );

  if (outcome === "denied")
    return (
      <Alert className="p-4">
        <AlertTitle>Authorization denied</AlertTitle>
        <AlertDescription>
          The CLI was not authorized. You can close this window and return to
          the terminal.
        </AlertDescription>
      </Alert>
    );

  return (
    <div className="grid gap-4">
      <div className="grid gap-2 text-center">
        <p className="text-sm text-muted-foreground">Confirmation code</p>
        <p
          className="break-all font-mono text-2xl font-semibold tracking-widest"
          aria-label={`Confirmation code ${code}`}
        >
          {code}
        </p>
      </div>
      {message && (
        <Alert>
          <AlertDescription>{message}</AlertDescription>
        </Alert>
      )}
      <Button
        type="button"
        size="lg"
        disabled={pendingAction !== null || !ready}
        onClick={() => void decide("approve")}
      >
        {pendingAction === "approve" ? "Authorizing…" : "Authorize"}
      </Button>
      <Button
        type="button"
        size="lg"
        variant="outline"
        disabled={pendingAction !== null || !ready}
        onClick={() => void decide("deny")}
      >
        {pendingAction === "deny" ? "Denying…" : "Deny"}
      </Button>
    </div>
  );
}
