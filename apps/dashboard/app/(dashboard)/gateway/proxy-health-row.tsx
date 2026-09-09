"use client";

import { Activity, RefreshCw } from "lucide-react";
import { startTransition, useActionState, useEffect, useRef } from "react";
import {
  checkGatewayProxyHealth,
  type GatewayProxyHealthState,
} from "@/app/actions";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { TableCell, TableRow } from "@/components/ui/table";

const initialState: GatewayProxyHealthState = { status: "idle" };

export function ProxyHealthRow({ configured }: { configured: boolean }) {
  const [state, action, pending] = useActionState(
    checkGatewayProxyHealth,
    initialState,
  );
  const checkedOnMount = useRef(false);

  useEffect(() => {
    if (!configured || checkedOnMount.current) return;
    checkedOnMount.current = true;
    startTransition(action);
  }, [action, configured]);

  const label = pending
    ? "Checking"
    : state.status === "healthy"
      ? "Healthy"
      : state.status === "unhealthy"
        ? "Unhealthy"
        : configured
          ? "Not checked"
          : "Not configured";
  const detail = state.error
    ? state.error
    : state.checkedAt
      ? `${state.latencyMs ?? 0} ms · checked ${new Date(state.checkedAt).toLocaleString()}`
      : "Ping the inference proxy from the Control API.";

  return (
    <TableRow>
      <TableCell className="text-muted-foreground">Proxy health</TableCell>
      <TableCell>
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="flex min-w-0 items-center gap-2" aria-live="polite">
            <Badge
              variant={
                state.status === "healthy"
                  ? "default"
                  : state.status === "unhealthy"
                    ? "destructive"
                    : "outline"
              }
            >
              <Activity className="size-3" /> {label}
            </Badge>
            <span className="min-w-0 text-xs text-muted-foreground">{detail}</span>
          </div>
          <form action={action}>
            <Button type="submit" variant="outline" size="sm" disabled={!configured || pending}>
              <RefreshCw className={pending ? "animate-spin" : undefined} />
              {pending ? "Pinging" : "Ping proxy"}
            </Button>
          </form>
        </div>
      </TableCell>
    </TableRow>
  );
}
