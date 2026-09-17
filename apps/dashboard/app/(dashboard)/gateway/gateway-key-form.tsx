"use client";

import { useActionState } from "react";
import { AlertCircle } from "lucide-react";
import { ensureGatewayKey, type GatewayKeyState } from "@/app/actions";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";

const initialState: GatewayKeyState = {};

export function GatewayKeyForm({ label, disabled, existingError }: { label: string; disabled: boolean; existingError?: string | null }) {
  const [state, action, pending] = useActionState(ensureGatewayKey, initialState);

  return (
    <div className="border-t p-4">
      {state.error && state.error !== existingError && (
        <Alert variant="destructive" className="mb-4">
          <AlertCircle />
          <AlertTitle>Provisioning failed</AlertTitle>
          <AlertDescription>{state.error}</AlertDescription>
        </Alert>
      )}
      <form action={action} className="flex justify-end">
        <Button type="submit" disabled={disabled || pending}>
          {pending ? "Provisioning…" : label}
        </Button>
      </form>
    </div>
  );
}
